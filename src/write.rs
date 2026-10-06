//! The write side: create, replace and remove files, directories, symlinks
//! and device nodes in a filesystem that is not mounted.
//!
//! Version 5 (CRC) filesystems only — what `mkfs.xfs` has made by default
//! since 2015 and all that `mkfs-xfs` makes — with every checksum, reverse
//! mapping, free-inode record and counter kept true, so `xfs_repair -n`
//! finds nothing and the kernel mounts the result.
//!
//! How it works:
//!
//! - File contents and inodes are written as they are given.
//! - A changed directory is kept in memory as its list of names and laid
//!   out once, by [`Volume::flush`], in whichever form fits
//!   ([`crate::dirwrite`]). Reads in the meantime see the list.
//! - Free space, inode chunks and reverse mappings of every AG touched are
//!   kept in memory ([`crate::alloc`]); [`Volume::flush`] writes their
//!   B+trees out afresh, then the AGF, AGI and superblock counters.
//!
//! So **nothing is consistent on disk until [`Volume::flush`]** — call it
//! before dropping the volume. Nothing here writes the log: the filesystem
//! must have been cleanly unmounted, and stays so.

use std::collections::BTreeMap;

use crate::alloc::{rmapf, Ag, CHUNK_INODES, MAX_EXTENT};
use crate::bmap::{self, Extent};
use crate::btree;
use crate::bytes::{be16, be32, be64, put16, put32, put64};
use crate::crc;
use crate::device::BlockDevice;
use crate::dir::{FileType, RawEntry};
use crate::dirwrite::{self, DirEnt, Layout};
use crate::error::{corrupt, Error, Result};
use crate::inode::{mode, Format, Inode, Timestamp};
use crate::log::LogState;
use crate::sb::Superblock;
use crate::volume::Volume;

/// The fixed part of a v3 inode, before its forks.
const CORE: usize = 176;
/// `IN`.
const INODE_MAGIC: u16 = 0x494e;
/// The inode's checksum.
const INODE_CRC: usize = 100;
/// `di_flags2` bits.
const DIFLAG2_REFLINK: u64 = 0x2;
const DIFLAG2_BIGTIME: u64 = 0x8;
const DIFLAG2_NREXT64: u64 = 0x10;
/// No next inode on an unlinked list.
const NULL_AGINO: u32 = u32::MAX;
/// `XSLM`, and a remote symlink block's header length.
const SYMLINK_MAGIC: u32 = 0x5853_4c4d;
const SYMLINK_HDR: usize = 56;
/// The longest symlink target: `XFS_SYMLINK_MAXLEN` (1024) less one — the
/// kernel refuses a target of that length and `xfs_repair` clears one.
const MAX_LINK: usize = 1023;
/// Largest write handed to the device at once.
const CHUNK: usize = 1 << 20;

/// Permissions and ownership for something being created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attrs {
    /// Permission bits, including setuid, setgid and sticky. The file-type
    /// bits are supplied by the operation, not by the caller.
    pub mode: u16,
    /// Owning user.
    pub uid: u32,
    /// Owning group.
    pub gid: u32,
}

impl Default for Attrs {
    fn default() -> Self {
        Attrs { mode: 0o644, uid: 0, gid: 0 }
    }
}

impl Attrs {
    /// Permissions, owned by root.
    pub fn mode(mode: u16) -> Self {
        Attrs { mode, ..Default::default() }
    }

    /// The defaults a directory wants rather than a file.
    pub fn dir() -> Self {
        Attrs { mode: 0o755, ..Default::default() }
    }

    /// Set the owner.
    pub fn owner(mut self, uid: u32, gid: u32) -> Self {
        self.uid = uid;
        self.gid = gid;
        self
    }

    fn perms(&self) -> u16 {
        self.mode & 0o7777
    }
}

/// What kind of special file to create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Special {
    /// A character device, such as `/dev/null`.
    CharDevice {
        /// Major number.
        major: u32,
        /// Minor number.
        minor: u32,
    },
    /// A block device, such as `/dev/sda`.
    BlockDevice {
        /// Major number.
        major: u32,
        /// Minor number.
        minor: u32,
    },
    /// A named pipe.
    Fifo,
    /// A unix socket.
    Socket,
}

impl Special {
    fn mode_bits(&self) -> u16 {
        match self {
            Special::CharDevice { .. } => mode::IFCHR,
            Special::BlockDevice { .. } => mode::IFBLK,
            Special::Fifo => mode::IFIFO,
            Special::Socket => mode::IFSOCK,
        }
    }
}

/// A directory changed since the last flush.
struct DirState {
    parent: u64,
    names: BTreeMap<Vec<u8>, (u64, u8)>,
}

/// What the write side holds between flushes.
pub(crate) struct Writer {
    ags: BTreeMap<u32, Ag>,
    dirs: BTreeMap<u64, DirState>,
    rng: u64,
    new_chunks: u64,
}

impl Writer {
    fn new() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9e37_79b9_7f4a_7c15);
        Writer { ags: BTreeMap::new(), dirs: BTreeMap::new(), rng: seed | 1, new_chunks: 0 }
    }

    /// A directory's names, `.` and `..` first, when it is held in memory.
    pub(crate) fn cached_entries(&self, ino: u64) -> Option<Vec<RawEntry>> {
        let d = self.dirs.get(&ino)?;
        let mut v = vec![
            RawEntry { name: b".".to_vec(), ino, ftype: FileType::Directory },
            RawEntry { name: b"..".to_vec(), ino: d.parent, ftype: FileType::Directory },
        ];
        v.extend(d.names.iter().map(|(n, &(i, t))| RawEntry { name: n.clone(), ino: i, ftype: FileType::from_ftype(t) }));
        Some(v)
    }

    fn next_gen(&mut self) -> u32 {
        // xorshift64*
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        (self.rng.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 32) as u32
    }
}

fn format_code(f: Format) -> u8 {
    match f {
        Format::Dev => 0,
        Format::Local => 1,
        Format::Extents => 2,
        Format::Btree => 3,
    }
}

fn ts_raw(t: Timestamp, bigtime: bool) -> u64 {
    if bigtime {
        ((t.secs + (1i64 << 31)).max(0) as u64) * 1_000_000_000 + t.nsecs as u64
    } else {
        let secs = t.secs.clamp(i32::MIN as i64, i32::MAX as i64) as i32 as u32 as u64;
        secs << 32 | t.nsecs as u64
    }
}

/// Split a path into its parent and last name.
fn split_path(path: &str) -> Result<(&str, Vec<u8>)> {
    let t = path.trim_end_matches('/');
    let (parent, name) = match t.rfind('/') {
        Some(i) => (&t[..i], &t[i + 1..]),
        None => ("", t),
    };
    if name.is_empty() || name == "." || name == ".." || name.len() > 255 {
        return Err(Error::InvalidPath(path.into()));
    }
    Ok((if parent.is_empty() { "/" } else { parent }, name.as_bytes().to_vec()))
}

/// The bytes of a raw inode, with helpers for the fields the writer sets.
struct Raw(Vec<u8>);

impl Raw {
    fn flags2(&self) -> u64 {
        be64(&self.0, 120)
    }

    fn bigtime(&self) -> bool {
        self.flags2() & DIFLAG2_BIGTIME != 0
    }

    fn data_fork_size(&self) -> usize {
        match self.0[82] as usize * 8 {
            0 => self.0.len() - CORE,
            n => n,
        }
    }

    fn set_data_fork(&mut self, format: Format, fork: &[u8]) {
        let n = self.data_fork_size();
        assert!(fork.len() <= n, "data fork too big for the inode");
        self.0[5] = format_code(format);
        self.0[CORE..CORE + n].fill(0);
        self.0[CORE..CORE + fork.len()].copy_from_slice(fork);
    }

    fn set_nextents(&mut self, n: u64) {
        if self.flags2() & DIFLAG2_NREXT64 != 0 {
            put64(&mut self.0, 24, n);
        } else {
            put32(&mut self.0, 76, n as u32);
        }
    }

    fn set_size(&mut self, n: u64) {
        put64(&mut self.0, 56, n);
    }

    fn nblocks(&self) -> u64 {
        be64(&self.0, 64)
    }

    fn set_nblocks(&mut self, n: u64) {
        put64(&mut self.0, 64, n);
    }

    fn nlink(&self) -> u32 {
        be32(&self.0, 16)
    }

    fn set_nlink(&mut self, n: u32) {
        put32(&mut self.0, 16, n);
    }

    fn mode(&self) -> u16 {
        be16(&self.0, 2)
    }

    fn set_mode(&mut self, m: u16) {
        put16(&mut self.0, 2, m);
    }

    fn set_owner(&mut self, uid: u32, gid: u32) {
        put32(&mut self.0, 8, uid);
        put32(&mut self.0, 12, gid);
    }

    /// Set mtime and ctime (`content`) or just ctime.
    fn touch(&mut self, now: Timestamp, content: bool) {
        let t = ts_raw(now, self.bigtime());
        if content {
            put64(&mut self.0, 40, t);
        }
        put64(&mut self.0, 48, t);
        let cc = be64(&self.0, 104);
        put64(&mut self.0, 104, cc.wrapping_add(1));
    }
}

impl<D: BlockDevice> Volume<D> {
    /// Fix the timestamp stamped onto new and changed inodes, in seconds
    /// since the Unix epoch, so that an image built twice comes out the same
    /// (the part `SOURCE_DATE_EPOCH` plays for `mkfs.xfs`). Unset, it is the
    /// time of each change.
    pub fn set_time(&mut self, secs: i64) {
        self.time = Some(Timestamp { secs, nsecs: 0 });
    }

    fn now(&self) -> Timestamp {
        self.time.unwrap_or_else(|| {
            let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
            Timestamp { secs: d.as_secs() as i64, nsecs: d.subsec_nanos() }
        })
    }

    fn writer(&mut self) -> Result<&mut Writer> {
        if self.w.is_none() {
            self.sb.check_writable()?;
            if !matches!(self.log, LogState::Clean | LogState::External) {
                return Err(Error::Unsupported(
                    "writing: the log is not clean; mount and unmount the filesystem once first".into(),
                ));
            }
            self.w = Some(Box::new(Writer::new()));
        }
        Ok(self.w.as_mut().unwrap())
    }

    fn w(&mut self) -> &mut Writer {
        self.w.as_mut().expect("writer")
    }

    /// Make sure AG `agno` is in memory.
    async fn load_ag(&mut self, agno: u32) -> Result<()> {
        self.writer()?;
        if !self.w().ags.contains_key(&agno) {
            let ag = Ag::load(&self.dev, &self.sb, agno).await?;
            self.w().ags.insert(agno, ag);
        }
        Ok(())
    }

    /// AG `agno`, and the superblock beside it.
    fn ag_mut(&mut self, agno: u32) -> (&Superblock, &mut Ag) {
        let w = self.w.as_mut().expect("writer");
        (&self.sb, w.ags.get_mut(&agno).expect("AG loaded"))
    }

    fn ag_order(&self, first: u32) -> Vec<u32> {
        let n = self.sb.ag_count;
        (0..n).map(|i| (first % n + i) % n).collect()
    }

    // ---- blocks --------------------------------------------------------------

    /// `n` blocks, from AG `pref` near `near` when there is room, as
    /// (first filesystem block, length) runs.
    async fn alloc_blocks(&mut self, pref: u32, n: u64, near: u32) -> Result<Vec<(u64, u32)>> {
        let mut out = Vec::new();
        let mut left = n;
        for agno in self.ag_order(pref) {
            if left == 0 {
                break;
            }
            self.load_ag(agno).await?;
            let (sb, ag) = self.ag_mut(agno);
            let near = if agno == pref { near } else { 0 };
            while left > 0 {
                match ag.alloc(left.min(MAX_EXTENT as u64) as u32, near) {
                    Some((s, l)) => {
                        out.push((sb.agb_to_fsb(agno, s), l));
                        left -= l as u64;
                    }
                    None => break,
                }
            }
        }
        if left > 0 {
            for (fsb, l) in out {
                let (agno, agbno) = self.sb.fsb_to_agb(fsb);
                self.ag_mut(agno).1.free_extent(agbno, l)?;
            }
            return Err(Error::NoSpace(format!("{n} blocks")));
        }
        Ok(out)
    }

    /// Record who owns blocks that were just allocated.
    fn rmap_extent(&mut self, fsb: u64, len: u32, owner: u64, offset: u64) {
        let (agno, agbno) = self.sb.fsb_to_agb(fsb);
        let (sb, ag) = self.ag_mut(agno);
        ag.rmap_add(sb, agbno, len, owner, offset);
    }

    /// Give blocks back, and forget who owned them.
    async fn release(&mut self, fsb: u64, len: u32, owner: u64) -> Result<()> {
        let (agno, agbno) = self.sb.fsb_to_agb(fsb);
        self.load_ag(agno).await?;
        let (sb, ag) = self.ag_mut(agno);
        ag.free_extent(agbno, len)?;
        ag.rmap_remove(sb, agbno, len, owner)
    }

    /// Free a fork's blocks, its B+tree's included; returns how many.
    async fn free_fork(&mut self, inode: &Inode, attr: bool) -> Result<u64> {
        let (extents, btree_blocks) = self.fork_map(inode, attr).await?;
        let mut n = 0;
        for e in extents {
            self.release(e.block, e.count as u32, inode.ino).await?;
            n += e.count;
        }
        for b in btree_blocks {
            self.release(b, 1, inode.ino).await?;
            n += 1;
        }
        Ok(n)
    }

    /// Write `data` into the blocks `extents` map, the last one zero-padded.
    async fn write_mapped(&self, extents: &[Extent], data: &[u8]) -> Result<()> {
        let bs = self.sb.block_size as usize;
        for e in extents {
            let mut at = e.offset as usize * bs;
            let end = (e.end() as usize * bs).min(data.len().div_ceil(bs) * bs);
            let mut dev_at = self.sb.fsb_to_byte(e.block)?;
            while at < end {
                let n = (end - at).min(CHUNK);
                let mut buf = vec![0u8; n];
                let have = data.len().saturating_sub(at).min(n);
                buf[..have].copy_from_slice(&data[at..at + have]);
                self.dev.write_at(dev_at, &buf).await?;
                at += n;
                dev_at += n as u64;
            }
        }
        Ok(())
    }

    /// Map `extents` in a fork of `fork_size` bytes: in the inode when they
    /// fit, else under a bmap B+tree. Returns the format, the fork's bytes
    /// and the B+tree's blocks.
    async fn map_fork(&mut self, ino: u64, extents: &[Extent], fork_size: usize) -> Result<(Format, Vec<u8>, u64)> {
        if extents.len() <= fork_size / 16 {
            let fork: Vec<u8> = extents.iter().flat_map(|e| e.encode()).collect();
            return Ok((Format::Extents, fork, 0));
        }
        let bs = self.sb.block_size as usize;
        let root_max = btree::bmap_root_max(fork_size);
        let nb = btree::bmap_blocks(extents.len(), bs, root_max);
        let (agno, near) = self.near(ino);
        let runs = self.alloc_blocks(agno, nb as u64, near).await?;
        let mut fsbs = Vec::with_capacity(nb);
        for &(fsb, len) in &runs {
            self.rmap_extent(fsb, len, ino, rmapf::BMBT);
            fsbs.extend(fsb..fsb + len as u64);
        }
        let recs: Vec<[u8; 16]> = extents.iter().map(Extent::encode).collect();
        let offsets: Vec<u64> = extents.iter().map(|e| e.offset).collect();
        let built = btree::build_bmap(&self.sb, ino, &recs, &offsets, &fsbs, root_max)?;
        for (fsb, buf) in &built.blocks {
            self.dev.write_at(self.sb.fsb_to_byte(*fsb)?, buf).await?;
        }
        let fork = btree::bmap_root_fork(fork_size, (built.levels - 1) as u16, &built.root_entries);
        Ok((Format::Btree, fork, nb as u64))
    }

    /// An inode's AG and the AG block it sits in: where its data should go.
    fn near(&self, ino: u64) -> (u32, u32) {
        let (agno, agino) = self.sb.ino_to_agino(ino);
        (agno, agino >> self.sb.inopb_log)
    }

    /// Store a regular file's contents in new blocks. Returns the fork's
    /// format and bytes, the blocks used and the extent count.
    async fn store_data(&mut self, ino: u64, fork_size: usize, data: &[u8]) -> Result<(Format, Vec<u8>, u64, u64)> {
        let bs = self.sb.block_size as u64;
        let n = (data.len() as u64).div_ceil(bs);
        if n == 0 {
            return Ok((Format::Extents, Vec::new(), 0, 0));
        }
        let (agno, near) = self.near(ino);
        let runs = self.alloc_blocks(agno, n, near).await?;
        let mut extents = Vec::with_capacity(runs.len());
        let mut lblk = 0;
        for (fsb, len) in runs {
            self.rmap_extent(fsb, len, ino, lblk);
            extents.push(Extent { offset: lblk, block: fsb, count: len as u64, unwritten: false });
            lblk += len as u64;
        }
        let mapped = match self.write_mapped(&extents, data).await {
            Ok(()) => self.map_fork(ino, &extents, fork_size).await,
            Err(e) => Err(e),
        };
        match mapped {
            Ok((format, fork, btree_blocks)) => Ok((format, fork, n + btree_blocks, extents.len() as u64)),
            Err(e) => {
                for x in &extents {
                    self.release(x.block, x.count as u32, ino).await?;
                }
                Err(e)
            }
        }
    }

    // ---- inodes --------------------------------------------------------------

    async fn read_raw(&self, ino: u64) -> Result<Raw> {
        let at = self.sb.ino_to_byte(ino)?;
        let mut buf = vec![0u8; self.sb.inode_size as usize];
        self.dev.read_at(at, &mut buf).await?;
        if be16(&buf, 0) != INODE_MAGIC || !crc::verify(&buf, INODE_CRC) || be64(&buf, 152) != ino {
            return Err(corrupt(format!("inode {ino}: magic, checksum or number")));
        }
        Ok(Raw(buf))
    }

    /// Write an inode back, checksummed: the whole block around it.
    async fn write_raw(&self, ino: u64, raw: &mut Raw) -> Result<()> {
        crc::stamp(&mut raw.0, INODE_CRC);
        let at = self.sb.ino_to_byte(ino)?;
        let bs = self.sb.block_size as u64;
        let block_at = at & !(bs - 1);
        let mut buf = vec![0u8; bs as usize];
        self.dev.read_at(block_at, &mut buf).await?;
        let off = (at - block_at) as usize;
        buf[off..off + raw.0.len()].copy_from_slice(&raw.0);
        self.dev.write_at(block_at, &buf).await
    }

    /// A new in-use inode's bytes, with an empty extent-format data fork.
    fn new_raw(&mut self, ino: u64, mode: u16, uid: u32, gid: u32, nlink: u32) -> Raw {
        let now = self.now();
        let gen = self.w().next_gen();
        let sb = &self.sb;
        let mut r = vec![0u8; sb.inode_size as usize];
        put16(&mut r, 0, INODE_MAGIC);
        put16(&mut r, 2, mode);
        r[4] = 3;
        r[5] = format_code(Format::Extents);
        put32(&mut r, 8, uid);
        put32(&mut r, 12, gid);
        put32(&mut r, 16, nlink);
        let flags2 = if sb.has_bigtime() { DIFLAG2_BIGTIME } else { 0 } | if sb.has_nrext64() { DIFLAG2_NREXT64 } else { 0 };
        put64(&mut r, 120, flags2);
        let t = ts_raw(now, sb.has_bigtime());
        for off in [32, 40, 48, 144] {
            put64(&mut r, off, t);
        }
        r[83] = format_code(Format::Extents);
        put32(&mut r, 92, gen);
        put32(&mut r, 96, NULL_AGINO);
        put64(&mut r, 104, 1);
        put64(&mut r, 152, ino);
        r[160..176].copy_from_slice(&sb.meta_uuid);
        Raw(r)
    }

    /// Write a new chunk's inodes, all free.
    async fn init_chunk(&mut self, agno: u32, agbno: u32) -> Result<()> {
        let isize = self.sb.inode_size as usize;
        let blocks = CHUNK_INODES / self.sb.inodes_per_block as u32;
        let mut buf = vec![0u8; blocks as usize * self.sb.block_size as usize];
        let first = self.sb.agino_to_ino(agno, agbno << self.sb.inopb_log);
        for i in 0..CHUNK_INODES as usize {
            let gen = self.w().next_gen();
            let r = &mut buf[i * isize..(i + 1) * isize];
            put16(r, 0, INODE_MAGIC);
            r[4] = 3;
            put32(r, 92, gen);
            put32(r, 96, NULL_AGINO);
            put64(r, 152, first + i as u64);
            r[160..176].copy_from_slice(&self.sb.meta_uuid);
            crc::stamp(r, INODE_CRC);
        }
        self.dev.write_at(self.sb.agb_to_byte(agno, agbno), &buf).await
    }

    /// A free inode, preferring AG `pref`: a free one in an existing chunk,
    /// else a new chunk, else the next AG.
    async fn alloc_ino(&mut self, pref: u32) -> Result<u64> {
        for agno in self.ag_order(pref) {
            self.load_ag(agno).await?;
            if let Some(agino) = self.ag_mut(agno).1.alloc_inode() {
                return Ok(self.sb.agino_to_ino(agno, agino));
            }
            if self.sb.imax_pct > 0 {
                let max = self.sb.data_blocks * self.sb.imax_pct as u64 / 100 * self.sb.inodes_per_block as u64;
                if self.sb.icount + (self.w().new_chunks + 1) * CHUNK_INODES as u64 > max {
                    return Err(Error::NoSpace(format!("inodes may take {}% of the space", self.sb.imax_pct)));
                }
            }
            let (sb, ag) = self.ag_mut(agno);
            if let Some(agbno) = ag.alloc_chunk(sb) {
                let agino = ag.alloc_inode().expect("a new chunk has free inodes");
                self.w().new_chunks += 1;
                self.init_chunk(agno, agbno).await?;
                return Ok(self.sb.agino_to_ino(agno, agino));
            }
        }
        Err(Error::NoSpace("no room for more inodes".into()))
    }

    /// Give back an inode allocated but never written.
    fn unalloc_ino(&mut self, ino: u64) -> Result<()> {
        let (agno, agino) = self.sb.ino_to_agino(ino);
        self.ag_mut(agno).1.free_inode(agino)
    }

    /// Free an inode and everything it holds.
    async fn destroy(&mut self, inode: &Inode) -> Result<()> {
        if inode.flags2 & DIFLAG2_REFLINK != 0 {
            return Err(Error::Unsupported(format!("inode {}: removing a file with shared extents", inode.ino)));
        }
        if inode.is_realtime() {
            return Err(Error::Unsupported(format!("inode {}: data on the realtime device", inode.ino)));
        }
        self.free_fork(inode, false).await?;
        if inode.aformat.is_some() {
            self.free_fork(inode, true).await?;
        }
        let mut raw = self.read_raw(inode.ino).await?;
        let gen = be32(&raw.0, 92).wrapping_add(1);
        let mut r = vec![0u8; raw.0.len()];
        put16(&mut r, 0, INODE_MAGIC);
        r[4] = 3;
        r[5] = format_code(Format::Extents);
        r[83] = format_code(Format::Extents);
        put64(&mut r, 120, raw.flags2() & (DIFLAG2_BIGTIME | DIFLAG2_NREXT64));
        put32(&mut r, 92, gen);
        put32(&mut r, 96, NULL_AGINO);
        put64(&mut r, 152, inode.ino);
        r[160..176].copy_from_slice(&self.sb.meta_uuid);
        raw.0 = r;
        self.write_raw(inode.ino, &mut raw).await?;
        let (agno, agino) = self.sb.ino_to_agino(inode.ino);
        self.load_ag(agno).await?;
        self.ag_mut(agno).1.free_inode(agino)
    }

    // ---- directories ---------------------------------------------------------

    /// The directory a new name goes in, and the name.
    async fn parent_dir(&mut self, path: &str) -> Result<(u64, Vec<u8>)> {
        let (parent, name) = split_path(path)?;
        let dino = self.resolve(parent.as_bytes(), true).await?;
        if !self.inode(dino).await?.is_dir() {
            return Err(Error::NotADirectory(parent.into()));
        }
        Ok((dino, name))
    }

    /// What `name` in directory `dino` refers to.
    async fn entry(&self, dino: u64, name: &[u8]) -> Result<Option<(u64, FileType)>> {
        let dir = self.inode(dino).await?;
        Ok(self.dir_entries(&dir).await?.into_iter().find(|e| e.name == name).map(|e| (e.ino, e.ftype)))
    }

    /// Directory `dino`'s names, held in memory from now until the flush.
    async fn dir_state(&mut self, dino: u64) -> Result<&mut DirState> {
        self.writer()?;
        if !self.w().dirs.contains_key(&dino) {
            let dir = self.inode(dino).await?;
            let mut parent = dino;
            let mut names = BTreeMap::new();
            for e in self.dir_entries(&dir).await? {
                if e.name == b".." {
                    parent = e.ino;
                } else if e.name != b"." {
                    names.insert(e.name, (e.ino, e.ftype.code()));
                }
            }
            self.w().dirs.insert(dino, DirState { parent, names });
        }
        Ok(self.w().dirs.get_mut(&dino).unwrap())
    }

    /// Lay a directory out from its names and write it.
    async fn write_dir(&mut self, dino: u64, st: &DirState) -> Result<()> {
        let inode = self.inode(dino).await?;
        if !inode.is_dir() {
            return Err(corrupt(format!("inode {dino}: not a directory")));
        }
        let freed = self.free_fork(&inode, false).await?;
        let mut raw = self.read_raw(dino).await?;
        let fork_size = raw.data_fork_size();
        let names: Vec<DirEnt> =
            st.names.iter().map(|(n, &(ino, ftype))| DirEnt { name: n.clone(), ino, ftype }).collect();
        let subdirs = names.iter().filter(|e| e.ftype == FileType::Directory.code()).count() as u32;
        let (format, fork, size, used, nextents) = match dirwrite::layout(&self.sb, dino, st.parent, &names, fork_size) {
            Layout::Short(bytes) => {
                let n = bytes.len() as u64;
                (Format::Local, bytes, n, 0, 0)
            }
            Layout::Blocks { size, blocks } => {
                let per = 1u64 << self.sb.dirblk_log;
                // Runs of consecutive logical blocks.
                let mut runs: Vec<(u64, u64)> = Vec::new();
                for b in &blocks {
                    match runs.last_mut() {
                        Some((s, n)) if *s + *n == b.lblk => *n += per,
                        _ => runs.push((b.lblk, per)),
                    }
                }
                let (agno, near) = self.near(dino);
                let mut extents = Vec::new();
                let mut used = 0;
                for (lstart, n) in runs {
                    let mut l = lstart;
                    for (fsb, len) in self.alloc_blocks(agno, n, near).await? {
                        self.rmap_extent(fsb, len, dino, l);
                        extents.push(Extent { offset: l, block: fsb, count: len as u64, unwritten: false });
                        l += len as u64;
                    }
                    used += n;
                }
                let bs = self.sb.block_size as usize;
                for mut b in blocks {
                    let fsb_of = |lblk: u64| -> Result<u64> {
                        let e = bmap::find(&extents, lblk).ok_or_else(|| corrupt("directory block unmapped"))?;
                        Ok(e.block + (lblk - e.offset))
                    };
                    let first = fsb_of(b.lblk)?;
                    put64(&mut b.data, b.blkno_off, self.sb.fsb_to_byte(first)? >> 9);
                    crc::stamp(&mut b.data, b.crc_off);
                    for k in 0..per {
                        let at = self.sb.fsb_to_byte(fsb_of(b.lblk + k)?)?;
                        self.dev.write_at(at, &b.data[k as usize * bs..(k as usize + 1) * bs]).await?;
                    }
                }
                let (format, fork, btree_blocks) = self.map_fork(dino, &extents, fork_size).await?;
                (format, fork, size, used + btree_blocks, extents.len() as u64)
            }
        };
        raw.set_data_fork(format, &fork);
        raw.set_size(size);
        raw.set_nextents(nextents);
        raw.set_nblocks(raw.nblocks().saturating_sub(freed) + used);
        raw.set_nlink(2 + subdirs);
        let now = self.now();
        raw.touch(now, true);
        self.write_raw(dino, &mut raw).await
    }

    // ---- making things -------------------------------------------------------

    /// Allocate an inode for a new name at `path`; returns the directory, the
    /// name, the inode number and its bytes, ready for a fork.
    async fn create_node(&mut self, path: &str, mode: u16, uid: u32, gid: u32, nlink: u32) -> Result<(u64, Vec<u8>, u64, Raw)> {
        let (dino, name) = self.parent_dir(path).await?;
        if self.entry(dino, &name).await?.is_some() {
            return Err(Error::Exists(path.into()));
        }
        self.writer()?;
        let (pref, _) = self.sb.ino_to_agino(dino);
        let ino = self.alloc_ino(pref).await?;
        let raw = self.new_raw(ino, mode, uid, gid, nlink);
        Ok((dino, name, ino, raw))
    }

    /// Put a name in a directory.
    async fn add_name(&mut self, dino: u64, name: Vec<u8>, ino: u64, kind: FileType) -> Result<()> {
        self.dir_state(dino).await?.names.insert(name, (ino, kind.code()));
        Ok(())
    }

    /// Create or replace a regular file, `0644` and owned by root when new.
    /// Parent directories must exist ([`Volume::mkdir_all`] makes them).
    /// Returns the inode number.
    pub async fn write(&mut self, path: &str, data: &[u8]) -> Result<u64> {
        self.write_file(path, data, None).await
    }

    /// Create or replace a regular file with these permissions and owner.
    pub async fn write_with(&mut self, path: &str, data: &[u8], attrs: &Attrs) -> Result<u64> {
        self.write_file(path, data, Some(attrs)).await
    }

    async fn write_file(&mut self, path: &str, data: &[u8], attrs: Option<&Attrs>) -> Result<u64> {
        let (dino, name) = self.parent_dir(path).await?;
        match self.entry(dino, &name).await? {
            Some((ino, FileType::File)) => {
                self.writer()?;
                let inode = self.inode(ino).await?;
                if inode.flags2 & DIFLAG2_REFLINK != 0 {
                    return Err(Error::Unsupported(format!("{path}: rewriting a file with shared extents")));
                }
                if inode.is_realtime() {
                    return Err(Error::Unsupported(format!("{path}: data on the realtime device")));
                }
                let mut raw = self.read_raw(ino).await?;
                // The new contents first: if there is no room, the old stay.
                let (format, fork, used, nextents) = self.store_data(ino, raw.data_fork_size(), data).await?;
                let freed = self.free_fork(&inode, false).await?;
                raw.set_data_fork(format, &fork);
                raw.set_size(data.len() as u64);
                raw.set_nextents(nextents);
                raw.set_nblocks(raw.nblocks().saturating_sub(freed) + used);
                if let Some(a) = attrs {
                    raw.set_mode(mode::IFREG | a.perms());
                    raw.set_owner(a.uid, a.gid);
                }
                let now = self.now();
                raw.touch(now, true);
                self.write_raw(ino, &mut raw).await?;
                Ok(ino)
            }
            Some((_, FileType::Directory)) => Err(Error::IsADirectory(path.into())),
            Some(_) => Err(Error::Exists(path.into())),
            None => {
                let a = attrs.copied().unwrap_or_default();
                let (dino, name, ino, mut raw) = self.create_node(path, mode::IFREG | a.perms(), a.uid, a.gid, 1).await?;
                let (format, fork, used, nextents) = match self.store_data(ino, raw.data_fork_size(), data).await {
                    Ok(stored) => stored,
                    Err(e) => {
                        self.unalloc_ino(ino)?;
                        return Err(e);
                    }
                };
                raw.set_data_fork(format, &fork);
                raw.set_size(data.len() as u64);
                raw.set_nextents(nextents);
                raw.set_nblocks(used);
                self.write_raw(ino, &mut raw).await?;
                self.add_name(dino, name, ino, FileType::File).await?;
                Ok(ino)
            }
        }
    }

    /// Make a directory, `0755` and owned by root. Its parent must exist.
    pub async fn mkdir(&mut self, path: &str) -> Result<u64> {
        self.mkdir_with(path, &Attrs::dir()).await
    }

    /// Make a directory with these permissions and owner.
    pub async fn mkdir_with(&mut self, path: &str, attrs: &Attrs) -> Result<u64> {
        let (dino, name, ino, mut raw) = self.create_node(path, mode::IFDIR | attrs.perms(), attrs.uid, attrs.gid, 2).await?;
        let fork = match dirwrite::layout(&self.sb, ino, dino, &[], raw.data_fork_size()) {
            Layout::Short(b) => b,
            Layout::Blocks { .. } => unreachable!("an empty directory fits in its inode"),
        };
        raw.set_data_fork(Format::Local, &fork);
        raw.set_size(fork.len() as u64);
        self.write_raw(ino, &mut raw).await?;
        self.w().dirs.insert(ino, DirState { parent: dino, names: BTreeMap::new() });
        self.add_name(dino, name, ino, FileType::Directory).await?;
        Ok(ino)
    }

    /// Make a directory and any parents it lacks, `0755` and owned by root.
    /// Directories already there are left as they are.
    pub async fn mkdir_all(&mut self, path: &str) -> Result<()> {
        self.mkdir_all_with(path, &Attrs::dir()).await
    }

    /// Make a directory and any parents it lacks, with these permissions and
    /// owner.
    pub async fn mkdir_all_with(&mut self, path: &str, attrs: &Attrs) -> Result<()> {
        let mut cur = String::new();
        for comp in path.split('/').filter(|c| !c.is_empty() && *c != ".") {
            cur.push('/');
            cur.push_str(comp);
            match self.resolve(cur.as_bytes(), true).await {
                Ok(ino) => {
                    if !self.inode(ino).await?.is_dir() {
                        return Err(Error::NotADirectory(cur));
                    }
                }
                Err(Error::NotFound(_)) => {
                    self.mkdir_with(&cur, attrs).await?;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Make a symbolic link at `path` pointing at `target` (1 to 1023 bytes).
    pub async fn symlink(&mut self, path: &str, target: &str) -> Result<u64> {
        let target = target.as_bytes();
        if target.is_empty() || target.len() > MAX_LINK {
            return Err(Error::InvalidPath(format!("symlink target of {} bytes", target.len())));
        }
        let (dino, name, ino, mut raw) = self.create_node(path, mode::IFLNK | 0o777, 0, 0, 1).await?;
        let fork_size = raw.data_fork_size();
        if target.len() <= fork_size {
            raw.set_data_fork(Format::Local, target);
            raw.set_nextents(0);
        } else {
            // One block per extent, never two blocks next to each other: the
            // kernel reads a remote symlink one header per mapping, merging
            // neighbours, while xfs_repair reads one header per block. Apart,
            // they agree.
            let bs = self.sb.block_size as usize;
            let payload = bs - SYMLINK_HDR;
            let nblk = target.len().div_ceil(payload);
            let (agno, _) = self.near(ino);
            let mut extents = Vec::new();
            let mut prev: Option<u64> = None;
            'blocks: for i in 0..nblk {
                for a in self.ag_order(agno) {
                    self.load_ag(a).await?;
                    let not = match prev.map(|f| self.sb.fsb_to_agb(f)) {
                        Some((pa, pb)) if pa == a => pb + 1,
                        _ => u32::MAX,
                    };
                    if let Some(b) = self.ag_mut(a).1.alloc_block_except(not) {
                        let fsb = self.sb.agb_to_fsb(a, b);
                        self.rmap_extent(fsb, 1, ino, i as u64);
                        extents.push(Extent { offset: i as u64, block: fsb, count: 1, unwritten: false });
                        prev = Some(fsb);
                        continue 'blocks;
                    }
                }
                for e in &extents {
                    self.release(e.block, 1, ino).await?;
                }
                self.unalloc_ino(ino)?;
                return Err(Error::NoSpace(path.into()));
            }
            for (i, e) in extents.iter().enumerate() {
                let off = i * payload;
                let part = &target[off..(off + payload).min(target.len())];
                let mut buf = vec![0u8; bs];
                put32(&mut buf, 0, SYMLINK_MAGIC);
                put32(&mut buf, 4, off as u32);
                put32(&mut buf, 8, part.len() as u32);
                buf[16..32].copy_from_slice(&self.sb.meta_uuid);
                put64(&mut buf, 32, ino);
                let at = self.sb.fsb_to_byte(e.block)?;
                put64(&mut buf, 40, at >> 9);
                buf[SYMLINK_HDR..SYMLINK_HDR + part.len()].copy_from_slice(part);
                crc::stamp(&mut buf, 12);
                self.dev.write_at(at, &buf).await?;
            }
            let fork: Vec<u8> = extents.iter().flat_map(|e| e.encode()).collect();
            raw.set_data_fork(Format::Extents, &fork);
            raw.set_nextents(extents.len() as u64);
            raw.set_nblocks(nblk as u64);
        }
        raw.set_size(target.len() as u64);
        self.write_raw(ino, &mut raw).await?;
        self.add_name(dino, name, ino, FileType::Symlink).await?;
        Ok(ino)
    }

    /// Make a device node, FIFO or socket.
    pub async fn mknod(&mut self, path: &str, kind: Special, attrs: &Attrs) -> Result<u64> {
        let dev = match kind {
            Special::CharDevice { major, minor } | Special::BlockDevice { major, minor } => {
                if major >= 1 << 14 || minor >= 1 << 18 {
                    return Err(Error::Unsupported(format!("device {major}:{minor} does not fit XFS's 32-bit number")));
                }
                major << 18 | minor
            }
            Special::Fifo | Special::Socket => 0,
        };
        let (dino, name, ino, mut raw) =
            self.create_node(path, kind.mode_bits() | attrs.perms(), attrs.uid, attrs.gid, 1).await?;
        raw.set_data_fork(Format::Dev, &dev.to_be_bytes());
        self.write_raw(ino, &mut raw).await?;
        self.add_name(dino, name, ino, FileType::from_mode(kind.mode_bits())).await?;
        Ok(ino)
    }

    /// Give an existing file (not a directory) another name.
    pub async fn link(&mut self, existing: &str, new_path: &str) -> Result<u64> {
        let ino = self.lookup(existing).await?;
        let inode = self.inode(ino).await?;
        if inode.is_dir() {
            return Err(Error::IsADirectory(existing.into()));
        }
        let (dino, name) = self.parent_dir(new_path).await?;
        if self.entry(dino, &name).await?.is_some() {
            return Err(Error::Exists(new_path.into()));
        }
        self.writer()?;
        let mut raw = self.read_raw(ino).await?;
        raw.set_nlink(raw.nlink() + 1);
        let now = self.now();
        raw.touch(now, false);
        self.write_raw(ino, &mut raw).await?;
        self.add_name(dino, name, ino, FileType::from_mode(inode.mode)).await?;
        Ok(ino)
    }

    // ---- removing things -----------------------------------------------------

    /// Remove a name that is not a directory; the file goes with its last
    /// name.
    pub async fn unlink(&mut self, path: &str) -> Result<()> {
        let (dino, name) = self.parent_dir(path).await?;
        let (ino, kind) = self.entry(dino, &name).await?.ok_or_else(|| Error::NotFound(path.into()))?;
        if kind == FileType::Directory {
            return Err(Error::IsADirectory(path.into()));
        }
        self.writer()?;
        let inode = self.inode(ino).await?;
        if inode.nlink <= 1 {
            self.destroy(&inode).await?;
        } else {
            let mut raw = self.read_raw(ino).await?;
            raw.set_nlink(raw.nlink() - 1);
            let now = self.now();
            raw.touch(now, false);
            self.write_raw(ino, &mut raw).await?;
        }
        self.dir_state(dino).await?.names.remove(&name);
        Ok(())
    }

    /// Remove an empty directory.
    pub async fn rmdir(&mut self, path: &str) -> Result<()> {
        let (dino, name) = self.parent_dir(path).await?;
        let (ino, kind) = self.entry(dino, &name).await?.ok_or_else(|| Error::NotFound(path.into()))?;
        if kind != FileType::Directory {
            return Err(Error::NotADirectory(path.into()));
        }
        let inode = self.inode(ino).await?;
        if self.dir_entries(&inode).await?.len() > 2 {
            return Err(Error::NotEmpty(path.into()));
        }
        self.writer()?;
        self.destroy(&inode).await?;
        self.w().dirs.remove(&ino);
        self.dir_state(dino).await?.names.remove(&name);
        Ok(())
    }

    // ---- changing things -----------------------------------------------------

    /// Change permission bits, following symlinks.
    pub async fn chmod(&mut self, path: &str, perms: u16) -> Result<()> {
        let ino = self.resolve(path.as_bytes(), true).await?;
        self.writer()?;
        let mut raw = self.read_raw(ino).await?;
        raw.set_mode(raw.mode() & mode::IFMT | perms & 0o7777);
        let now = self.now();
        raw.touch(now, false);
        self.write_raw(ino, &mut raw).await
    }

    /// Change the owner, following symlinks.
    pub async fn chown(&mut self, path: &str, uid: u32, gid: u32) -> Result<()> {
        let ino = self.resolve(path.as_bytes(), true).await?;
        self.writer()?;
        let mut raw = self.read_raw(ino).await?;
        raw.set_owner(uid, gid);
        let now = self.now();
        raw.touch(now, false);
        self.write_raw(ino, &mut raw).await
    }

    // ---- flush ---------------------------------------------------------------

    /// Bring everything on disk up to date: lay out every changed directory,
    /// rebuild the B+trees of every AG touched, write the AG headers and the
    /// superblock's counters, and flush the device.
    ///
    /// **Call this before dropping the volume.** Until then names, free
    /// space and inode maps are in memory only, and the filesystem on disk
    /// is not consistent. The volume can go on being written afterwards.
    pub async fn flush(&mut self) -> Result<()> {
        if self.w.is_none() {
            return self.dev.flush().await;
        }
        let dirs = std::mem::take(&mut self.w().dirs);
        for (dino, st) in &dirs {
            self.write_dir(*dino, st).await?;
        }

        let mut writes = Vec::new();
        {
            let sb = &self.sb;
            let w = self.w.as_mut().unwrap();
            for ag in w.ags.values_mut() {
                if ag.dirty {
                    writes.extend(ag.rebuild(sb)?);
                }
            }
        }
        for (at, buf) in writes {
            self.dev.write_at(at, &buf).await?;
        }

        let (mut icount, mut ifree, mut fdblocks) = (0u64, 0u64, 0u64);
        for agno in 0..self.sb.ag_count {
            let headers = match self.w.as_ref().unwrap().ags.get(&agno) {
                Some(ag) => (ag.agf().to_vec(), ag.agi().to_vec()),
                None => Ag::read_headers(&self.dev, &self.sb, agno).await?,
            };
            let (count, free) = Ag::agi_inodes(&headers.1);
            icount += count;
            ifree += free;
            fdblocks += Ag::agf_free_blocks(&headers.0);
        }
        let mut sbuf = vec![0u8; self.sb.sect()];
        self.dev.read_at(0, &mut sbuf).await?;
        put64(&mut sbuf, 128, icount);
        put64(&mut sbuf, 136, ifree);
        put64(&mut sbuf, 144, fdblocks);
        crc::stamp(&mut sbuf, 224);
        self.dev.write_at(0, &sbuf).await?;
        self.sb.icount = icount;
        self.sb.ifree = ifree;
        self.sb.free_blocks = fdblocks;
        self.w().new_chunks = 0;
        self.dev.flush().await
    }
}
