//! The public API: an open filesystem, read by path.

use std::collections::{HashSet, VecDeque};

use crate::attr::{self, Xattr};
use crate::bmap::{self, Extent};
use crate::device::BlockDevice;
use crate::dir::{self, FileType, RawEntry};
use crate::error::{corrupt, Error, Result};
use crate::inode::{mode, Format, Inode, Timestamp};
use crate::sb::Superblock;

/// Symbolic links followed while resolving one path, as Linux allows.
const MAX_SYMLINKS: u32 = 40;

/// The longest symlink target XFS stores.
const MAX_LINK: u64 = 1024;

/// A v5 remote symlink block's header length, and its magic, `XSLM`.
const SYMLINK_HDR: usize = 56;
const SYMLINK_MAGIC: u32 = 0x5853_4c4d;

/// What a directory listing tells you about one name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The name. Non-UTF-8 bytes are replaced; `raw_name` has them as stored.
    pub name: String,
    /// The name exactly as stored.
    pub raw_name: Vec<u8>,
    /// The inode it refers to.
    pub inode: u64,
    /// What it is.
    pub kind: FileType,
    /// Whether it is a directory.
    pub is_dir: bool,
}

/// What `stat` tells you about a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    /// Inode number.
    pub inode: u64,
    /// Size in bytes.
    pub size: u64,
    /// Mode, including the file type bits.
    pub mode: u16,
    /// Hard links.
    pub links: u32,
    /// Owner.
    pub uid: u32,
    /// Group.
    pub gid: u32,
    /// Blocks of 512 bytes allocated, both forks.
    pub blocks: u64,
    /// Last access.
    pub atime: Timestamp,
    /// Last modification of the contents.
    pub mtime: Timestamp,
    /// Last change of the inode.
    pub ctime: Timestamp,
    /// Creation, on v5 filesystems.
    pub crtime: Option<Timestamp>,
    /// A device node's `(major, minor)`; `(0, 0)` for anything else.
    pub rdev: (u32, u32),
}

impl Stat {
    /// Whether this is a directory.
    pub fn is_dir(&self) -> bool {
        self.mode & mode::IFMT == mode::IFDIR
    }

    /// Whether this is a regular file.
    pub fn is_file(&self) -> bool {
        self.mode & mode::IFMT == mode::IFREG
    }

    /// Whether this is a symbolic link.
    pub fn is_symlink(&self) -> bool {
        self.mode & mode::IFMT == mode::IFLNK
    }

    /// What it is.
    pub fn kind(&self) -> FileType {
        FileType::from_mode(self.mode)
    }
}

/// One name found by [`Volume::walk`].
#[derive(Debug, Clone)]
pub struct WalkEntry {
    /// Path relative to the root walked, `/`-separated. Non-UTF-8 bytes are
    /// replaced; `raw_path` has them as stored.
    pub path: String,
    /// The path exactly as stored.
    pub raw_path: Vec<u8>,
    /// What `stat` says about it.
    pub stat: Stat,
}

/// An open XFS filesystem.
///
/// Reading only, for now: the filesystem is expected to be cleanly
/// unmounted, since nothing here replays the log.
pub struct Volume<D: BlockDevice> {
    dev: D,
    sb: Superblock,
}

impl<D: BlockDevice> Volume<D> {
    /// Open the filesystem on `device`, checking its superblock.
    pub async fn open(device: D) -> Result<Self> {
        let mut buf = vec![0u8; 512];
        if device.size() < 512 {
            return Err(Error::NotXfs("device smaller than a sector".into()));
        }
        device.read_at(0, &mut buf).await?;
        // A v5 superblock's checksum covers its whole sector.
        let sect = u16::from_be_bytes([buf[102], buf[103]]) as usize;
        if sect > 512 && sect <= 32768 && sect as u64 <= device.size() {
            buf.resize(sect, 0);
            device.read_at(0, &mut buf).await?;
        }
        let sb = Superblock::parse(&buf)?;
        let end = sb.data_blocks.checked_mul(sb.block_size as u64);
        if end.map_or(true, |e| e > device.size()) {
            return Err(Error::Corrupt(format!(
                "the filesystem is {} blocks of {} bytes, the device {} bytes",
                sb.data_blocks,
                sb.block_size,
                device.size()
            )));
        }
        Ok(Volume { dev: device, sb })
    }

    /// The superblock.
    pub fn superblock(&self) -> &Superblock {
        &self.sb
    }

    /// The device underneath.
    pub fn device(&self) -> &D {
        &self.dev
    }

    /// Give the device back.
    pub fn into_device(self) -> D {
        self.dev
    }

    // ---- blocks and inodes ------------------------------------------------

    async fn read_blocks(&self, fsb: u64, count: u64) -> Result<Vec<u8>> {
        let at = self.sb.fsb_to_byte(fsb)?;
        let len = count << self.sb.block_log;
        // The run must stay inside one AG, which is what makes it contiguous.
        self.sb.fsb_to_byte(fsb + count - 1)?;
        let mut buf = vec![0u8; len as usize];
        self.dev.read_at(at, &mut buf).await?;
        Ok(buf)
    }

    /// Read inode `ino`.
    pub async fn inode(&self, ino: u64) -> Result<Inode> {
        let at = self.sb.ino_to_byte(ino)?;
        let bs = self.sb.block_size as u64;
        let block_at = at & !(bs - 1);
        let mut buf = vec![0u8; bs as usize];
        self.dev.read_at(block_at, &mut buf).await?;
        let off = (at - block_at) as usize;
        Inode::parse(&self.sb, ino, &buf[off..])
    }

    /// The extents of an inode's data fork, or its attribute fork.
    pub async fn extents(&self, inode: &Inode, attr: bool) -> Result<Vec<Extent>> {
        let (format, fork, count) = if attr {
            match inode.aformat {
                None => return Ok(Vec::new()),
                Some(f) => (f, &inode.attr_fork, inode.anextents as u64),
            }
        } else {
            (inode.format, &inode.data_fork, inode.nextents)
        };
        match format {
            Format::Extents => bmap::finish(bmap::inline_extents(fork, count)?),
            Format::Btree => {
                let (level, ptrs) = bmap::btree_root(fork)?;
                let mut out = Vec::new();
                let mut seen = HashSet::new();
                let mut stack: Vec<(u64, u16)> = ptrs.into_iter().rev().map(|p| (p, level - 1)).collect();
                while let Some((fsb, want)) = stack.pop() {
                    if !seen.insert(fsb) || seen.len() as u64 > inode.nblocks + 1 {
                        return Err(corrupt(format!("inode {}: bmap B+tree loops", inode.ino)));
                    }
                    let buf = self.read_blocks(fsb, 1).await?;
                    let (lvl, n) = bmap::check_btree_block(&self.sb, &buf, fsb, inode.ino)?;
                    if lvl != want {
                        return Err(corrupt(format!(
                            "inode {}: bmap block {fsb} at level {lvl}, expected {want}",
                            inode.ino
                        )));
                    }
                    if lvl == 0 {
                        out.extend(bmap::leaf_extents(&self.sb, &buf, n));
                    } else {
                        stack.extend(bmap::node_ptrs(&self.sb, &buf, n).into_iter().rev().map(|p| (p, lvl - 1)));
                    }
                }
                if !attr && out.len() as u64 != inode.nextents {
                    return Err(corrupt(format!(
                        "inode {}: {} extents in the B+tree, {} recorded",
                        inode.ino,
                        out.len(),
                        inode.nextents
                    )));
                }
                bmap::finish(out)
            }
            Format::Local | Format::Dev => Ok(Vec::new()),
        }
    }

    /// `count` logical blocks of a fork starting at `lblk`, with holes and
    /// unwritten extents read as zeroes.
    async fn read_mapped(&self, extents: &[Extent], lblk: u64, count: u64) -> Result<Vec<u8>> {
        let bs = self.sb.block_size as usize;
        let mut out = vec![0u8; count as usize * bs];
        let end = lblk + count;
        let mut i = extents.partition_point(|e| e.end() <= lblk);
        while let Some(e) = extents.get(i) {
            if e.offset >= end {
                break;
            }
            i += 1;
            if e.unwritten {
                continue;
            }
            let from = e.offset.max(lblk);
            let to = e.end().min(end);
            let data = self.read_blocks(e.block + (from - e.offset), to - from).await?;
            let at = (from - lblk) as usize * bs;
            out[at..at + data.len()].copy_from_slice(&data);
        }
        Ok(out)
    }

    /// `len` bytes of a regular file's contents from `offset`, clipped to
    /// its size.
    pub async fn read_inode_range(
        &self,
        inode: &Inode,
        extents: &[Extent],
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>> {
        if offset >= inode.size || len == 0 {
            return Ok(Vec::new());
        }
        let len = len.min(inode.size - offset);
        if inode.format == Format::Local {
            // Not written by Linux for regular files, but valid on disk.
            let end = (offset + len) as usize;
            if end > inode.data_fork.len() {
                return Err(corrupt(format!("inode {}: inline data past the fork", inode.ino)));
            }
            return Ok(inode.data_fork[offset as usize..end].to_vec());
        }
        if inode.is_realtime() {
            return Err(Error::Unsupported(format!("inode {}: data on the realtime device", inode.ino)));
        }
        let log = self.sb.block_log;
        let first = offset >> log;
        let last = (offset + len - 1) >> log;
        let buf = self.read_mapped(extents, first, last - first + 1).await?;
        let skip = (offset - (first << log)) as usize;
        Ok(buf[skip..skip + len as usize].to_vec())
    }

    // ---- directories -------------------------------------------------------

    /// Every name in a directory inode as stored, `.` and `..` included.
    pub(crate) async fn dir_entries(&self, inode: &Inode) -> Result<Vec<RawEntry>> {
        if !inode.is_dir() {
            return Err(Error::NotADirectory(format!("inode {}", inode.ino)));
        }
        if inode.format == Format::Local {
            let (parent, mut names) = dir::parse_shortform(&self.sb, &inode.data_fork, inode.size)?;
            names.insert(0, RawEntry { name: b"..".to_vec(), ino: parent, ftype: FileType::Directory });
            names.insert(0, RawEntry { name: b".".to_vec(), ino: inode.ino, ftype: FileType::Directory });
            return Ok(names);
        }
        let extents = self.extents(inode, false).await?;
        let per = 1u64 << self.sb.dirblk_log;
        let limit = dir::LEAF_OFFSET >> self.sb.block_log;
        let mut out = Vec::new();
        let mut next = 0u64;
        for e in &extents {
            let mut lblk = (e.offset & !(per - 1)).max(next);
            while lblk < e.end().min(limit) {
                let buf = self.read_mapped(&extents, lblk, per).await?;
                dir::parse_data_block(&self.sb, &buf, inode.ino, &mut out)?;
                lblk += per;
                next = lblk;
            }
        }
        Ok(out)
    }

    async fn dir_lookup(&self, dir_inode: &Inode, name: &[u8]) -> Result<Option<u64>> {
        Ok(self
            .dir_entries(dir_inode)
            .await?
            .into_iter()
            .find(|e| e.name == name)
            .map(|e| e.ino))
    }

    /// Entries of a directory inode, without `.` and `..`, with each one's
    /// type filled in from its inode where the directory does not record it.
    pub async fn read_dir_inode(&self, inode: &Inode) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        for e in self.dir_entries(inode).await? {
            if e.name == b"." || e.name == b".." {
                continue;
            }
            let kind = match e.ftype {
                FileType::Unknown => FileType::from_mode(self.inode(e.ino).await?.mode),
                k => k,
            };
            out.push(Entry {
                name: String::from_utf8_lossy(&e.name).into_owned(),
                raw_name: e.name,
                inode: e.ino,
                kind,
                is_dir: kind == FileType::Directory,
            });
        }
        Ok(out)
    }

    // ---- symlinks ----------------------------------------------------------

    /// A symlink inode's target, as stored.
    pub async fn symlink_target(&self, inode: &Inode) -> Result<Vec<u8>> {
        if inode.file_type() != mode::IFLNK {
            return Err(Error::NotASymlink(format!("inode {}", inode.ino)));
        }
        let size = inode.size;
        if size == 0 || size > MAX_LINK {
            return Err(corrupt(format!("inode {}: symlink of {size} bytes", inode.ino)));
        }
        if inode.format == Format::Local {
            if size as usize > inode.data_fork.len() {
                return Err(corrupt(format!("inode {}: inline symlink past the fork", inode.ino)));
            }
            return Ok(inode.data_fork[..size as usize].to_vec());
        }
        let extents = self.extents(inode, false).await?;
        let bs = self.sb.block_size as u64;
        if !self.sb.is_v5() {
            let buf = self.read_mapped(&extents, 0, size.div_ceil(bs)).await?;
            return Ok(buf[..size as usize].to_vec());
        }
        // One header per mapped extent, not per block: Linux writes each
        // mapping as one buffer (`xfs_symlink_write_target`), and its
        // checksum covers the whole run.
        let mut out = Vec::with_capacity(size as usize);
        for e in &extents {
            if out.len() as u64 >= size {
                break;
            }
            if e.unwritten {
                return Err(corrupt(format!("inode {}: unwritten symlink block", inode.ino)));
            }
            let buf = self.read_blocks(e.block, e.count).await?;
            let bytes = crate::bytes::be32(&buf, 8) as usize;
            if crate::bytes::be32(&buf, 0) != SYMLINK_MAGIC
                || !crate::crc::verify(&buf, 12)
                || crate::bytes::be64(&buf, 32) != inode.ino
                || crate::bytes::be32(&buf, 4) as usize != out.len()
                || SYMLINK_HDR + bytes > buf.len()
            {
                return Err(corrupt(format!("inode {}: remote symlink block {}", inode.ino, e.offset)));
            }
            out.extend_from_slice(&buf[SYMLINK_HDR..SYMLINK_HDR + bytes]);
        }
        if out.len() as u64 != size {
            return Err(corrupt(format!("inode {}: symlink blocks hold {} of {size} bytes", inode.ino, out.len())));
        }
        Ok(out)
    }

    // ---- extended attributes ----------------------------------------------

    /// Every user-visible extended attribute of an inode.
    pub async fn xattrs_inode(&self, inode: &Inode) -> Result<Vec<Xattr>> {
        let found = match inode.aformat {
            None => return Ok(Vec::new()),
            // An attribute fork made ready and never used: Linux gives new
            // inodes one up front when ACLs or security labels may follow,
            // and counts it as no attributes (`xfs_inode_hasattr`).
            Some(Format::Extents) if inode.anextents == 0 => return Ok(Vec::new()),
            Some(Format::Local) => attr::parse_shortform(&inode.attr_fork)?,
            Some(Format::Extents | Format::Btree) => {
                let extents = self.extents(inode, true).await?;
                let mut found = Vec::new();
                let mut seen = HashSet::new();
                let mut blk = 0u32;
                loop {
                    if !seen.insert(blk) || seen.len() > 1 << 20 {
                        return Err(corrupt(format!("inode {}: attribute blocks loop", inode.ino)));
                    }
                    let buf = self.read_mapped(&extents, blk as u64, 1).await?;
                    match attr::parse_block(&self.sb, &buf, inode.ino, blk as u64)? {
                        attr::Block::Node { first_child } => blk = first_child,
                        attr::Block::Leaf { found: f, forw } => {
                            found.extend(f);
                            if forw == 0 {
                                break;
                            }
                            blk = forw;
                        }
                    }
                }
                found
            }
            Some(Format::Dev) => return Err(corrupt(format!("inode {}: device-format attributes", inode.ino))),
        };

        let mut out = Vec::with_capacity(found.len());
        let mut extents = None;
        for f in found {
            let value = match f.value {
                attr::Value::Inline(v) => v,
                attr::Value::Remote { blk, len } => {
                    if extents.is_none() {
                        extents = Some(self.extents(inode, true).await?);
                    }
                    let ext = extents.as_deref().unwrap();
                    let mut v = Vec::with_capacity(len as usize);
                    for i in 0..attr::remote_blocks(&self.sb, len) {
                        let buf = self.read_mapped(ext, blk as u64 + i, 1).await?;
                        let want = len as usize - v.len();
                        v.extend_from_slice(attr::remote_payload(&self.sb, &buf, inode.ino, want)?);
                    }
                    if v.len() != len as usize {
                        return Err(corrupt(format!("inode {}: remote attribute {}", inode.ino, f.name)));
                    }
                    v
                }
            };
            let value = if f.name.starts_with("system.posix_acl_") { attr::posix_acl(&value)? } else { value };
            out.push(Xattr { name: f.name, value });
        }
        Ok(out)
    }

    // ---- paths ---------------------------------------------------------------

    /// Resolve a path to an inode number. Symlinks in the middle of a path
    /// are followed, as Linux follows them; the last component is followed
    /// only when `follow` is set. Absolute symlink targets resolve from this
    /// filesystem's root, never the host's.
    pub async fn resolve(&self, path: &[u8], follow: bool) -> Result<u64> {
        let shown = || String::from_utf8_lossy(path).into_owned();
        let root = self.sb.root_ino;
        let mut comps: VecDeque<Vec<u8>> = split(path).collect();
        let mut cur = root;
        let mut links = 0;
        while let Some(comp) = comps.pop_front() {
            let dir = self.inode(cur).await?;
            if !dir.is_dir() {
                return Err(Error::NotADirectory(shown()));
            }
            let child = self.dir_lookup(&dir, &comp).await?.ok_or_else(|| Error::NotFound(shown()))?;
            if comps.is_empty() && !follow {
                return Ok(child);
            }
            let inode = self.inode(child).await?;
            if inode.file_type() == mode::IFLNK {
                links += 1;
                if links > MAX_SYMLINKS {
                    return Err(Error::SymlinkLoop(shown()));
                }
                let target = self.symlink_target(&inode).await?;
                if target.first() == Some(&b'/') {
                    cur = root;
                }
                for c in split(&target).collect::<Vec<_>>().into_iter().rev() {
                    comps.push_front(c);
                }
                continue;
            }
            cur = child;
        }
        Ok(cur)
    }

    /// The inode a path names, without following a final symlink.
    pub async fn lookup(&self, path: &str) -> Result<u64> {
        self.resolve(path.as_bytes(), false).await
    }

    /// Whether a path exists (a dangling symlink exists).
    pub async fn exists(&self, path: &str) -> Result<bool> {
        match self.lookup(path).await {
            Ok(_) => Ok(true),
            Err(Error::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Stat a path, without following a final symlink (`lstat`).
    pub async fn stat(&self, path: &str) -> Result<Stat> {
        let inode = self.inode(self.lookup(path).await?).await?;
        Ok(self.stat_inode(&inode))
    }

    /// What `stat` says about an inode already read.
    pub fn stat_inode(&self, inode: &Inode) -> Stat {
        Stat {
            inode: inode.ino,
            size: inode.size,
            mode: inode.mode,
            links: inode.nlink,
            uid: inode.uid,
            gid: inode.gid,
            blocks: inode.nblocks << (self.sb.block_log - 9),
            atime: inode.atime,
            mtime: inode.mtime,
            ctime: inode.ctime,
            crtime: inode.crtime,
            rdev: inode.device(),
        }
    }

    /// A regular file's whole contents, following symlinks.
    pub async fn read(&self, path: &str) -> Result<Vec<u8>> {
        let inode = self.file_inode(path).await?;
        let extents = self.extents(&inode, false).await?;
        self.read_inode_range(&inode, &extents, 0, inode.size).await
    }

    /// Up to `len` bytes of a regular file from `offset`, following symlinks.
    pub async fn read_range(&self, path: &str, offset: u64, len: u64) -> Result<Vec<u8>> {
        let inode = self.file_inode(path).await?;
        let extents = self.extents(&inode, false).await?;
        self.read_inode_range(&inode, &extents, offset, len).await
    }

    async fn file_inode(&self, path: &str) -> Result<Inode> {
        let inode = self.inode(self.resolve(path.as_bytes(), true).await?).await?;
        match inode.file_type() {
            mode::IFREG => Ok(inode),
            mode::IFDIR => Err(Error::IsADirectory(path.into())),
            _ => Err(Error::Unsupported(format!("{path}: not a regular file"))),
        }
    }

    /// A directory's entries, without `.` and `..`, following symlinks.
    pub async fn read_dir(&self, path: &str) -> Result<Vec<Entry>> {
        let inode = self.inode(self.resolve(path.as_bytes(), true).await?).await?;
        if !inode.is_dir() {
            return Err(Error::NotADirectory(path.into()));
        }
        self.read_dir_inode(&inode).await
    }

    /// A symlink's target. Non-UTF-8 bytes are replaced; see
    /// [`Volume::read_link_bytes`].
    pub async fn read_link(&self, path: &str) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.read_link_bytes(path).await?).into_owned())
    }

    /// A symlink's target exactly as stored.
    pub async fn read_link_bytes(&self, path: &str) -> Result<Vec<u8>> {
        let inode = self.inode(self.lookup(path).await?).await?;
        if inode.file_type() != mode::IFLNK {
            return Err(Error::NotASymlink(path.into()));
        }
        self.symlink_target(&inode).await
    }

    /// Every extended attribute of a path, without following a final
    /// symlink, in the names Linux reports (`user.`, `trusted.`,
    /// `security.`, `system.posix_acl_*`).
    pub async fn list_xattrs(&self, path: &str) -> Result<Vec<Xattr>> {
        let inode = self.inode(self.lookup(path).await?).await?;
        self.xattrs_inode(&inode).await
    }

    /// One extended attribute's value.
    pub async fn get_xattr(&self, path: &str, name: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.list_xattrs(path).await?.into_iter().find(|x| x.name == name).map(|x| x.value))
    }

    /// Every name under a directory, depth first in sorted order, the
    /// directory itself excluded. Symlinks are listed, not followed.
    pub async fn walk(&self, root: &str) -> Result<Vec<WalkEntry>> {
        let mut out = Vec::new();
        self.walk_each(root, |e, _| {
            out.push(e.clone());
            Ok(())
        })
        .await?;
        Ok(out)
    }

    /// Walk like [`Volume::walk`], handing each name and its inode to `f`
    /// as it is found rather than collecting them.
    pub async fn walk_each<F>(&self, root: &str, mut f: F) -> Result<()>
    where
        F: FnMut(&WalkEntry, &Inode) -> Result<()>,
    {
        let top = self.inode(self.resolve(root.as_bytes(), true).await?).await?;
        if !top.is_dir() {
            return Err(Error::NotADirectory(root.into()));
        }
        let mut visited = HashSet::new();
        visited.insert(top.ino);
        // An explicit stack rather than recursion; each level holds its
        // remaining names reversed, so popping walks them in order.
        let mut stack = vec![(Vec::new(), self.sorted_children(&top).await?)];
        while let Some((prefix, rest)) = stack.last_mut() {
            let Some(entry) = rest.pop() else {
                stack.pop();
                continue;
            };
            let mut raw_path = prefix.clone();
            if !raw_path.is_empty() {
                raw_path.push(b'/');
            }
            raw_path.extend_from_slice(&entry.raw_name);
            let inode = self.inode(entry.inode).await?;
            let item = WalkEntry {
                path: String::from_utf8_lossy(&raw_path).into_owned(),
                raw_path: raw_path.clone(),
                stat: self.stat_inode(&inode),
            };
            f(&item, &inode)?;
            if inode.is_dir() {
                if !visited.insert(inode.ino) {
                    return Err(corrupt(format!("directory {} reached twice", inode.ino)));
                }
                let children = self.sorted_children(&inode).await?;
                stack.push((raw_path, children));
            }
        }
        Ok(())
    }

    async fn sorted_children(&self, dir: &Inode) -> Result<Vec<Entry>> {
        let mut names = self.read_dir_inode(dir).await?;
        names.sort_by(|a, b| b.raw_name.cmp(&a.raw_name));
        Ok(names)
    }
}

/// The components of a path, without empty ones and `.`.
fn split(path: &[u8]) -> impl Iterator<Item = Vec<u8>> + '_ {
    path.split(|&b| b == b'/').filter(|c| !c.is_empty() && *c != b".").map(|c| c.to_vec())
}
