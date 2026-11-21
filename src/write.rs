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
//!
//! On failure: a change that cannot get or write its blocks gives them back
//! and changes nothing; a flush that fails part-way keeps what it did not
//! finish, so it can be run again; a failure nothing can undo (an inode
//! written outside a flush) poisons the writer, and every later change and
//! flush fails rather than report success.

use std::collections::BTreeMap;

use crate::alloc::{rmapf, Ag, CHUNK_INODES, MAX_EXTENT};
use crate::bmap::{self, Extent};
use crate::btree;
use crate::bytes::{be16, be32, be64, put16, put32, put64};
use crate::crc;
use crate::device::{read_aligned, BlockDevice};
use crate::dir::{FileType, RawEntry};
use crate::dirwrite::{self, DirEnt, Layout};
use crate::error::{corrupt, Error, Result};
use crate::inode::{mode, Format, Inode, Timestamp};
use crate::log::LogState;
use crate::sb::{quota, Superblock, QFLAGS_OFFSET};
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
    /// The inode and data fork as they were on disk, kept from the first
    /// attempt to write the directory out: if that attempt's inode write
    /// fails, what is on disk may be the new inode or neither, and a retry
    /// must still know which blocks are the old ones.
    disk: Option<DiskDir>,
}

/// A directory's inode bytes and fork map before it is rewritten.
struct DiskDir {
    raw: Vec<u8>,
    map: (Vec<Extent>, Vec<u64>),
}

/// What the write side holds between flushes.
pub(crate) struct Writer {
    ags: BTreeMap<u32, Ag>,
    dirs: BTreeMap<u64, DirState>,
    rng: u64,
    new_chunks: u64,
    /// Why the in-memory state can no longer be trusted, once a failure
    /// could not be undone: every later change and flush is refused, so
    /// none can report success over it.
    poisoned: Option<String>,
    /// The primary superblock's sector, as the writer last wrote it: the
    /// flush changes its counters here rather than reading back a sector a
    /// failed write may have left torn.
    sb_sector: Vec<u8>,
}

impl Writer {
    fn new(sb_sector: Vec<u8>) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9e37_79b9_7f4a_7c15);
        Writer { ags: BTreeMap::new(), dirs: BTreeMap::new(), rng: seed | 1, new_chunks: 0, poisoned: None, sb_sector }
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

/// Whether a directory may hold `name`: 1 to 255 bytes, neither `/` nor
/// NUL in it, and not `.` or `..` — what the read side accepts
/// ([`crate::dir`]) and what the kernel can name.
fn valid_name(name: &[u8]) -> bool {
    !name.is_empty() && name.len() <= 255 && name != b"." && name != b".." && !name.iter().any(|&b| b == b'/' || b == 0)
}

/// Split a path into its parent and last name. Every operation that makes a
/// name comes through here, so a path with a NUL anywhere in it, or a last
/// name a directory cannot hold, is refused before anything is allocated
/// (review finding 8b48ae8c1697).
fn split_path(path: &str) -> Result<(&str, Vec<u8>)> {
    if path.contains('\0') {
        return Err(Error::InvalidPath(path.escape_debug().to_string()));
    }
    let t = path.trim_end_matches('/');
    let (parent, name) = match t.rfind('/') {
        Some(i) => (&t[..i], &t[i + 1..]),
        None => ("", t),
    };
    if !valid_name(name.as_bytes()) {
        return Err(Error::InvalidPath(path.into()));
    }
    Ok((if parent.is_empty() { "/" } else { parent }, name.as_bytes().to_vec()))
}

/// A file's contents, stored in new blocks: what its data fork becomes.
struct Stored {
    format: Format,
    fork: Vec<u8>,
    /// Blocks taken, B+tree blocks included.
    blocks: u64,
    nextents: u64,
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

    /// The writer's state, made on the first change. Every change asks for
    /// it before it touches the device.
    async fn writer(&mut self) -> Result<&mut Writer> {
        self.check_poisoned()?;
        if self.w.is_none() {
            self.sb.check_writable()?;
            if !matches!(self.log, LogState::Clean | LogState::External) {
                return Err(Error::Unsupported(
                    "writing: the log is not clean; mount and unmount the filesystem once first".into(),
                ));
            }
            let mut sector = read_aligned(&self.dev, self.sb.block_size as u64, 0, self.sb.sect()).await?;
            if be32(&sector, 0) != crate::sb::MAGIC || !crc::verify(&sector, 224) {
                return Err(corrupt("primary superblock: magic or checksum"));
            }
            if self.sb.quota_counts_trusted() {
                self.invalidate_quota_counts(&mut sector).await?;
            }
            self.w = Some(Box::new(Writer::new(sector)));
        }
        Ok(self.w.as_mut().unwrap())
    }

    /// This writer keeps no quota counts, so before its first change it
    /// clears every quota "checked" flag in the primary superblock and
    /// flushes it: whatever happens after, the kernel recounts (quotacheck)
    /// the next time it mounts with quotas on, as it does after `xfs_repair`
    /// clears them. Accounting and enforcement flags stay as they were
    /// (review finding 01a341965933).
    async fn invalidate_quota_counts(&mut self, sector: &mut [u8]) -> Result<()> {
        let qflags = be16(sector, QFLAGS_OFFSET) & !quota::ALL_CHKD;
        put16(sector, QFLAGS_OFFSET, qflags);
        crc::stamp(sector, 224);
        self.dev.write_at(0, sector).await?;
        self.dev.flush().await?;
        self.sb.qflags = qflags;
        Ok(())
    }

    fn w(&mut self) -> &mut Writer {
        self.w.as_mut().expect("writer")
    }

    /// Refuse to go on after a failure that could not be undone.
    fn check_poisoned(&self) -> Result<()> {
        match self.w.as_ref().and_then(|w| w.poisoned.as_ref()) {
            Some(why) => Err(corrupt(format!(
                "an earlier failure left the writer's state inconsistent ({why}); reopen the volume"
            ))),
            None => Ok(()),
        }
    }

    /// Remember that the in-memory state no longer matches what was asked
    /// for, and pass the error on.
    fn poison(&mut self, e: Error) -> Error {
        if let Some(w) = self.w.as_mut() {
            w.poisoned.get_or_insert_with(|| e.to_string());
        }
        e
    }

    /// Give back runs of blocks allocated for something that then failed.
    /// If even that fails, the writer is poisoned.
    async fn unalloc_runs(&mut self, runs: &[(u64, u32)], owner: u64) {
        for &(fsb, len) in runs {
            if let Err(e) = self.release(fsb, len, owner).await {
                self.poison(e);
                return;
            }
        }
    }

    /// Make sure AG `agno` is in memory.
    async fn load_ag(&mut self, agno: u32) -> Result<()> {
        self.writer().await?;
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

    /// Give back the blocks of a fork (as [`Volume::fork_map`] found them)
    /// once the inode no longer points at them. The inode is already on
    /// disk without them, so a failure here poisons the writer.
    async fn free_map(&mut self, ino: u64, map: &(Vec<Extent>, Vec<u64>)) -> Result<()> {
        for e in &map.0 {
            if let Err(e) = self.release(e.block, e.count as u32, ino).await {
                return Err(self.poison(e));
            }
        }
        for &b in &map.1 {
            if let Err(e) = self.release(b, 1, ino).await {
                return Err(self.poison(e));
            }
        }
        Ok(())
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
    /// and the runs of blocks the B+tree took — none taken if it fails.
    async fn map_fork(&mut self, ino: u64, extents: &[Extent], fork_size: usize) -> Result<(Format, Vec<u8>, Vec<(u64, u32)>)> {
        if extents.len() <= fork_size / 16 {
            let fork: Vec<u8> = extents.iter().flat_map(|e| e.encode()).collect();
            return Ok((Format::Extents, fork, Vec::new()));
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
        let built = match btree::build_bmap(&self.sb, ino, &recs, &offsets, &fsbs, root_max) {
            Ok(built) => self.write_blocks(&built.blocks).await.map(|()| built),
            Err(e) => Err(e),
        };
        match built {
            Ok(built) => {
                let fork = btree::bmap_root_fork(fork_size, (built.levels - 1) as u16, &built.root_entries);
                Ok((Format::Btree, fork, runs))
            }
            Err(e) => {
                self.unalloc_runs(&runs, ino).await;
                Err(e)
            }
        }
    }

    /// Write whole blocks, each at its filesystem block.
    async fn write_blocks(&self, blocks: &[(u64, Vec<u8>)]) -> Result<()> {
        for (fsb, buf) in blocks {
            self.dev.write_at(self.sb.fsb_to_byte(*fsb)?, buf).await?;
        }
        Ok(())
    }

    /// An inode's AG and the AG block it sits in: where its data should go.
    fn near(&self, ino: u64) -> (u32, u32) {
        let (agno, agino) = self.sb.ino_to_agino(ino);
        (agno, agino >> self.sb.inopb_log)
    }

    /// Store a regular file's contents in new blocks. Nothing is kept if it
    /// fails.
    async fn store_data(&mut self, ino: u64, fork_size: usize, data: &[u8]) -> Result<Stored> {
        let bs = self.sb.block_size as u64;
        let n = (data.len() as u64).div_ceil(bs);
        if n == 0 {
            return Ok(Stored { format: Format::Extents, fork: Vec::new(), blocks: 0, nextents: 0 });
        }
        let (agno, near) = self.near(ino);
        let mut runs = self.alloc_blocks(agno, n, near).await?;
        let mut extents = Vec::with_capacity(runs.len());
        let mut lblk = 0;
        for &(fsb, len) in &runs {
            self.rmap_extent(fsb, len, ino, lblk);
            extents.push(Extent { offset: lblk, block: fsb, count: len as u64, unwritten: false });
            lblk += len as u64;
        }
        let mapped = match self.write_mapped(&extents, data).await {
            Ok(()) => self.map_fork(ino, &extents, fork_size).await,
            Err(e) => Err(e),
        };
        match mapped {
            Ok((format, fork, tree)) => {
                let blocks = n + tree.iter().map(|&(_, l)| l as u64).sum::<u64>();
                runs.extend(tree);
                Ok(Stored { format, fork, blocks, nextents: extents.len() as u64 })
            }
            Err(e) => {
                self.unalloc_runs(&runs, ino).await;
                Err(e)
            }
        }
    }

    /// Write an inode outside a flush. A failed write leaves the inode on
    /// disk neither old nor new, and nothing later rewrites it, so the
    /// writer is poisoned.
    async fn commit_raw(&mut self, ino: u64, raw: &mut Raw) -> Result<()> {
        match self.write_raw(ino, raw).await {
            Ok(()) => {
                // A pending directory (chmod, chown) keeps what is on disk.
                if let Some(d) = self.w.as_mut().and_then(|w| w.dirs.get_mut(&ino)).and_then(|d| d.disk.as_mut()) {
                    d.raw.clone_from(&raw.0);
                }
                Ok(())
            }
            Err(e) => Err(self.poison(e)),
        }
    }

    // ---- inodes --------------------------------------------------------------

    /// Read an inode's bytes, through the whole block around it (review
    /// finding 1fed3c80f651).
    async fn read_raw(&self, ino: u64) -> Result<Raw> {
        let at = self.sb.ino_to_byte(ino)?;
        let buf = read_aligned(&self.dev, self.sb.block_size as u64, at, self.sb.inode_size as usize).await?;
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
        let block_at = at - at % bs;
        let len = ((at + raw.0.len() as u64).div_ceil(bs) * bs - block_at) as usize;
        let mut buf = read_aligned(&self.dev, bs, block_at, len).await?;
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
    ///
    /// The inode-space limit (`sb_imax_pct`) only stops new chunks: as in
    /// the kernel's `xfs_dialloc`, an AG that may not grow is still searched
    /// for free inodes, and the search goes on to every other AG before
    /// giving up (review finding b8ffbc6331af).
    async fn alloc_ino(&mut self, pref: u32) -> Result<u64> {
        let may_grow = match self.inode_limit() {
            Some(max) => self.sb.icount + (self.w().new_chunks + 1) * CHUNK_INODES as u64 <= max,
            None => true,
        };
        for agno in self.ag_order(pref) {
            self.load_ag(agno).await?;
            if let Some(agino) = self.ag_mut(agno).1.alloc_inode() {
                return Ok(self.sb.agino_to_ino(agno, agino));
            }
            if !may_grow {
                continue;
            }
            let (sb, ag) = self.ag_mut(agno);
            if let Some(agbno) = ag.alloc_chunk(sb) {
                let agino = ag.alloc_inode().expect("a new chunk has free inodes");
                self.w().new_chunks += 1;
                // The chunk is allocated in memory: if its inodes did not
                // reach the disk, nothing can be trusted to undo that.
                if let Err(e) = self.init_chunk(agno, agbno).await {
                    return Err(self.poison(e));
                }
                return Ok(self.sb.agino_to_ino(agno, agino));
            }
        }
        if !may_grow {
            return Err(Error::NoSpace(format!("inodes may take {}% of the space", self.sb.imax_pct)));
        }
        Err(Error::NoSpace("no room for more inodes".into()))
    }

    /// The most inodes the filesystem may hold, if `sb_imax_pct` limits it:
    /// that share of the data blocks, in whole inode chunks, as the kernel's
    /// `xfs_set_maxicount` reckons it.
    fn inode_limit(&self) -> Option<u64> {
        if self.sb.imax_pct == 0 {
            return None;
        }
        let chunk_blocks = (CHUNK_INODES / self.sb.inodes_per_block as u32).max(1) as u64;
        let blocks = self.sb.data_blocks * self.sb.imax_pct as u64 / 100 / chunk_blocks * chunk_blocks;
        Some(blocks * self.sb.inodes_per_block as u64)
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
        // What it holds is read first and given back only once the inode on
        // disk is free.
        let data = self.fork_map(inode, false).await?;
        let attr = self.fork_map(inode, true).await?;
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
        self.commit_raw(inode.ino, &mut raw).await?;
        self.free_map(inode.ino, &data).await?;
        self.free_map(inode.ino, &attr).await?;
        let (agno, agino) = self.sb.ino_to_agino(inode.ino);
        let freed = match self.load_ag(agno).await {
            Ok(()) => self.ag_mut(agno).1.free_inode(agino),
            Err(e) => Err(e),
        };
        freed.map_err(|e| self.poison(e))
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
        self.writer().await?;
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
            self.w().dirs.insert(dino, DirState { parent, names, disk: None });
        }
        Ok(self.w().dirs.get_mut(&dino).unwrap())
    }

    /// Lay a directory out from its names and write it.
    ///
    /// The new layout goes into new blocks, then the inode is pointed at
    /// it, and only then are the old blocks given back (review finding
    /// 6dd91ec6c081). A failure before that gives back what it took and
    /// leaves the directory as it was, on disk and in memory, so the flush
    /// can be run again.
    async fn write_dir(&mut self, dino: u64, st: &mut DirState) -> Result<()> {
        if st.disk.is_none() {
            let inode = self.inode(dino).await?;
            if !inode.is_dir() {
                return Err(corrupt(format!("inode {dino}: not a directory")));
            }
            let map = self.fork_map(&inode, false).await?;
            let raw = self.read_raw(dino).await?;
            st.disk = Some(DiskDir { raw: raw.0, map });
        }
        let disk = st.disk.as_ref().expect("read above");
        let old = disk.map.clone();
        let mut raw = Raw(disk.raw.clone());
        let freed = old.0.iter().map(|e| e.count).sum::<u64>() + old.1.len() as u64;
        let fork_size = raw.data_fork_size();
        let names: Vec<DirEnt> =
            st.names.iter().map(|(n, &(ino, ftype))| DirEnt { name: n.clone(), ino, ftype }).collect();
        let subdirs = names.iter().filter(|e| e.ftype == FileType::Directory.code()).count() as u32;
        let mut taken = Vec::new();
        let laid = match dirwrite::layout(&self.sb, dino, st.parent, &names, fork_size) {
            Layout::Short(bytes) => {
                let n = bytes.len() as u64;
                Ok((Format::Local, bytes, n, 0, 0))
            }
            Layout::Blocks { size, blocks } => self.write_dir_blocks(dino, size, blocks, fork_size, &mut taken).await,
        };
        let written = match laid {
            Ok((format, fork, size, used, nextents)) => {
                raw.set_data_fork(format, &fork);
                raw.set_size(size);
                raw.set_nextents(nextents);
                raw.set_nblocks(raw.nblocks().saturating_sub(freed) + used);
                raw.set_nlink(2 + subdirs);
                let now = self.now();
                raw.touch(now, true);
                // The directory stays pending until this succeeds, and the
                // next flush writes the whole inode again.
                self.write_raw(dino, &mut raw).await
            }
            Err(e) => Err(e),
        };
        if let Err(e) = written {
            self.unalloc_runs(&taken, dino).await;
            return Err(e);
        }
        self.free_map(dino, &old).await
    }

    /// A block-form directory's blocks, written to new space; every run
    /// taken is pushed on `taken`. Returns the fork's format and bytes, the
    /// size, the blocks used and the extent count.
    async fn write_dir_blocks(
        &mut self,
        dino: u64,
        size: u64,
        blocks: Vec<dirwrite::DirBlock>,
        fork_size: usize,
        taken: &mut Vec<(u64, u32)>,
    ) -> Result<(Format, Vec<u8>, u64, u64, u64)> {
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
                taken.push((fsb, len));
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
        let (format, fork, tree) = self.map_fork(dino, &extents, fork_size).await?;
        let tree_blocks: u64 = tree.iter().map(|&(_, l)| l as u64).sum();
        taken.extend(tree);
        Ok((format, fork, size, used + tree_blocks, extents.len() as u64))
    }

    // ---- making things -------------------------------------------------------

    /// Allocate an inode for a new name at `path`; returns the directory, the
    /// name, the inode number and its bytes, ready for a fork.
    async fn create_node(&mut self, path: &str, mode: u16, uid: u32, gid: u32, nlink: u32) -> Result<(u64, Vec<u8>, u64, Raw)> {
        let (dino, name) = self.parent_dir(path).await?;
        if self.entry(dino, &name).await?.is_some() {
            return Err(Error::Exists(path.into()));
        }
        self.writer().await?;
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
                self.writer().await?;
                let inode = self.inode(ino).await?;
                if inode.flags2 & DIFLAG2_REFLINK != 0 {
                    return Err(Error::Unsupported(format!("{path}: rewriting a file with shared extents")));
                }
                if inode.is_realtime() {
                    return Err(Error::Unsupported(format!("{path}: data on the realtime device")));
                }
                if let Some(a) = attrs {
                    self.check_mode_change(&inode, a.perms(), path).await?;
                }
                let old = self.fork_map(&inode, false).await?;
                let freed = old.0.iter().map(|e| e.count).sum::<u64>() + old.1.len() as u64;
                let mut raw = self.read_raw(ino).await?;
                // The new contents first: if there is no room, the old stay.
                // The old blocks go back once the inode no longer maps them.
                let st = self.store_data(ino, raw.data_fork_size(), data).await?;
                raw.set_data_fork(st.format, &st.fork);
                raw.set_size(data.len() as u64);
                raw.set_nextents(st.nextents);
                raw.set_nblocks(raw.nblocks().saturating_sub(freed) + st.blocks);
                if let Some(a) = attrs {
                    raw.set_mode(mode::IFREG | a.perms());
                    raw.set_owner(a.uid, a.gid);
                }
                let now = self.now();
                raw.touch(now, true);
                self.commit_raw(ino, &mut raw).await?;
                self.free_map(ino, &old).await?;
                Ok(ino)
            }
            Some((_, FileType::Directory)) => Err(Error::IsADirectory(path.into())),
            Some(_) => Err(Error::Exists(path.into())),
            None => {
                let a = attrs.copied().unwrap_or_default();
                let (dino, name, ino, mut raw) = self.create_node(path, mode::IFREG | a.perms(), a.uid, a.gid, 1).await?;
                let st = match self.store_data(ino, raw.data_fork_size(), data).await {
                    Ok(stored) => stored,
                    Err(e) => {
                        self.unalloc_ino(ino)?;
                        return Err(e);
                    }
                };
                raw.set_data_fork(st.format, &st.fork);
                raw.set_size(data.len() as u64);
                raw.set_nextents(st.nextents);
                raw.set_nblocks(st.blocks);
                self.commit_raw(ino, &mut raw).await?;
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
        self.commit_raw(ino, &mut raw).await?;
        self.w().dirs.insert(ino, DirState { parent: dino, names: BTreeMap::new(), disk: None });
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

    /// Make a symbolic link at `path` pointing at `target` (1 to 1023
    /// bytes, no NUL).
    pub async fn symlink(&mut self, path: &str, target: &str) -> Result<u64> {
        let target = target.as_bytes();
        if target.is_empty() || target.len() > MAX_LINK {
            return Err(Error::InvalidPath(format!("symlink target of {} bytes", target.len())));
        }
        if target.contains(&0) {
            return Err(Error::InvalidPath(format!("symlink target with a NUL: {:?}", String::from_utf8_lossy(target))));
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
            let mut blocks = Vec::with_capacity(extents.len());
            for (i, e) in extents.iter().enumerate() {
                let off = i * payload;
                let part = &target[off..(off + payload).min(target.len())];
                let mut buf = vec![0u8; bs];
                put32(&mut buf, 0, SYMLINK_MAGIC);
                put32(&mut buf, 4, off as u32);
                put32(&mut buf, 8, part.len() as u32);
                buf[16..32].copy_from_slice(&self.sb.meta_uuid);
                put64(&mut buf, 32, ino);
                put64(&mut buf, 40, self.sb.fsb_to_byte(e.block)? >> 9);
                buf[SYMLINK_HDR..SYMLINK_HDR + part.len()].copy_from_slice(part);
                crc::stamp(&mut buf, 12);
                blocks.push((e.block, buf));
            }
            if let Err(err) = self.write_blocks(&blocks).await {
                let runs: Vec<(u64, u32)> = extents.iter().map(|e| (e.block, 1)).collect();
                self.unalloc_runs(&runs, ino).await;
                self.unalloc_ino(ino)?;
                return Err(err);
            }
            let fork: Vec<u8> = extents.iter().flat_map(|e| e.encode()).collect();
            raw.set_data_fork(Format::Extents, &fork);
            raw.set_nextents(extents.len() as u64);
            raw.set_nblocks(nblk as u64);
        }
        raw.set_size(target.len() as u64);
        self.commit_raw(ino, &mut raw).await?;
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
        self.commit_raw(ino, &mut raw).await?;
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
        self.writer().await?;
        let mut raw = self.read_raw(ino).await?;
        raw.set_nlink(raw.nlink() + 1);
        let now = self.now();
        raw.touch(now, false);
        self.commit_raw(ino, &mut raw).await?;
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
        self.writer().await?;
        let inode = self.inode(ino).await?;
        if inode.nlink <= 1 {
            self.destroy(&inode).await?;
        } else {
            let mut raw = self.read_raw(ino).await?;
            raw.set_nlink(raw.nlink() - 1);
            let now = self.now();
            raw.touch(now, false);
            self.commit_raw(ino, &mut raw).await?;
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
        self.writer().await?;
        self.destroy(&inode).await?;
        self.w().dirs.remove(&ino);
        self.dir_state(dino).await?.names.remove(&name);
        Ok(())
    }

    // ---- changing things -----------------------------------------------------

    /// Change permission bits, following symlinks.
    ///
    /// Refused ([`Error::Unsupported`]) when the read, write or execute bits
    /// change on an inode with an access ACL: the kernel rewrites the ACL's
    /// owner, mask and other entries to match the new mode, and this writer
    /// cannot write attributes, so the ACL would go on granting what the
    /// mode took away (review finding 0f02ac950500). Setuid, setgid and
    /// sticky are not in the ACL and may change.
    pub async fn chmod(&mut self, path: &str, perms: u16) -> Result<()> {
        let ino = self.resolve(path.as_bytes(), true).await?;
        self.writer().await?;
        let inode = self.inode(ino).await?;
        self.check_mode_change(&inode, perms & 0o7777, path).await?;
        let mut raw = self.read_raw(ino).await?;
        raw.set_mode(raw.mode() & mode::IFMT | perms & 0o7777);
        let now = self.now();
        raw.touch(now, false);
        self.commit_raw(ino, &mut raw).await
    }

    /// Refuse a change of `inode`'s permission bits to `perms` that its
    /// access ACL would have to follow.
    async fn check_mode_change(&self, inode: &Inode, perms: u16, path: &str) -> Result<()> {
        if (inode.mode ^ perms) & 0o777 == 0 || inode.aformat.is_none() {
            return Ok(());
        }
        if self.xattr_names(inode).await?.iter().any(|x| x.name == "system.posix_acl_access") {
            return Err(Error::Unsupported(format!(
                "{path}: changing the permission bits of a file with an access ACL (the ACL would have to be rewritten to match)"
            )));
        }
        Ok(())
    }

    /// Change the owner, following symlinks.
    pub async fn chown(&mut self, path: &str, uid: u32, gid: u32) -> Result<()> {
        let ino = self.resolve(path.as_bytes(), true).await?;
        self.writer().await?;
        let mut raw = self.read_raw(ino).await?;
        raw.set_owner(uid, gid);
        let now = self.now();
        raw.touch(now, false);
        self.commit_raw(ino, &mut raw).await
    }

    // ---- flush ---------------------------------------------------------------

    /// Bring everything on disk up to date: lay out every changed directory,
    /// rebuild the B+trees of every AG touched, write the AG headers and the
    /// superblock's counters, and flush the device.
    ///
    /// **Call this before dropping the volume.** Until then names, free
    /// space and inode maps are in memory only, and the filesystem on disk
    /// is not consistent. The volume can go on being written afterwards.
    ///
    /// A flush that fails part-way can be run again: a directory stays
    /// pending until it is written, and an AG until its trees, its headers,
    /// the superblock and the device flush are all done, so a retry writes
    /// whatever did not get there (review findings 793cd4f33612,
    /// a72ebcf6af32, 03ce0a0cf15e). A failure that could not be undone
    /// instead makes every later change and flush fail.
    pub async fn flush(&mut self) -> Result<()> {
        self.check_poisoned()?;
        if self.w.is_none() {
            return self.dev.flush().await;
        }
        let pending: Vec<u64> = self.w().dirs.keys().copied().collect();
        for dino in pending {
            let mut st = self.w().dirs.remove(&dino).expect("pending directory");
            if let Err(e) = self.write_dir(dino, &mut st).await {
                self.w().dirs.insert(dino, st);
                return Err(e);
            }
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
        let mut sbuf = self.w().sb_sector.clone();
        put64(&mut sbuf, 128, icount);
        put64(&mut sbuf, 136, ifree);
        put64(&mut sbuf, 144, fdblocks);
        crc::stamp(&mut sbuf, 224);
        self.dev.write_at(0, &sbuf).await?;
        self.w().sb_sector = sbuf;
        self.dev.flush().await?;
        self.sb.icount = icount;
        self.sb.ifree = ifree;
        self.sb.free_blocks = fdblocks;
        let w = self.w();
        w.new_chunks = 0;
        for ag in w.ags.values_mut() {
            ag.dirty = false;
        }
        Ok(())
    }
}
