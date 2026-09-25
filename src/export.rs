//! Getting a tree out: into a tar stream, or onto the local filesystem.

use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use crate::device::BlockDevice;
use crate::error::{Error, Result};
use crate::inode::{mode, Timestamp};
use crate::tar::{self, EntryKind, Header, Sink};
use crate::volume::Volume;

/// File content is read and written in pieces this large.
const CHUNK: u64 = 1 << 20;

/// What [`Volume::pack_tar_to`] wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackReport {
    /// Regular files read out.
    pub files: u64,
    /// Directories.
    pub directories: u64,
    /// Symbolic links.
    pub symlinks: u64,
    /// Hard links — names beyond the first for one inode.
    pub hard_links: u64,
    /// Device nodes and FIFOs.
    pub devices: u64,
    /// Sockets, which tar cannot hold and are left out.
    pub sockets_skipped: u64,
    /// Extended attributes carried across.
    pub xattrs: u64,
    /// Bytes of file content read.
    pub bytes: u64,
}

/// How [`Volume::extract`] writes.
#[derive(Debug, Clone, Default)]
pub struct ExtractOptions {
    /// Set each name's owner and group, which takes root (or `CAP_CHOWN`).
    pub owner: bool,
}

/// What [`Volume::extract`] wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtractReport {
    /// Regular files.
    pub files: u64,
    /// Directories.
    pub directories: u64,
    /// Symbolic links.
    pub symlinks: u64,
    /// Hard links — names beyond the first for one inode.
    pub hard_links: u64,
    /// Bytes of file content.
    pub bytes: u64,
    /// Device nodes, FIFOs and sockets, which are not created: that takes
    /// `mknod`, and root. Their paths.
    pub specials_skipped: Vec<String>,
    /// Extended attributes found and not applied. A tar archive
    /// ([`Volume::pack_tar_to`]) carries them; a plain directory copy does not.
    pub xattrs_skipped: u64,
}

impl<D: BlockDevice> Volume<D> {
    /// Pack a tree into a tar archive held in memory.
    pub async fn pack_tar(&self, root: &str) -> Result<Vec<u8>> {
        let mut writer = tar::Writer::new(Vec::new());
        self.pack_into(&mut writer, root).await?;
        writer.finish().await?;
        Ok(writer.into_inner())
    }

    /// Pack a tree into a tar archive written to a stream.
    ///
    /// Names are relative to `root` and in sorted order, so the same tree
    /// always makes the same archive. Modes, ownership, modification times
    /// (to the nanosecond), symlinks, hard links, device nodes and extended
    /// attributes are all carried; sockets are left out, as tar has no type
    /// for them. The root directory itself is not an entry.
    pub async fn pack_tar_to<K: Sink>(&self, sink: K, root: &str) -> Result<PackReport> {
        let mut writer = tar::Writer::new(sink);
        let report = self.pack_into(&mut writer, root).await?;
        writer.finish().await?;
        Ok(report)
    }

    async fn pack_into<K: Sink>(&self, w: &mut tar::Writer<K>, root: &str) -> Result<PackReport> {
        let mut report = PackReport::default();
        let mut seen: HashMap<u64, Vec<u8>> = HashMap::new();
        for e in self.walk(root).await? {
            let st = e.stat;
            let inode = self.inode(st.inode).await?;
            let mut h = Header {
                path: e.raw_path.clone(),
                mode: (st.mode & 0o7777) as u32,
                uid: st.uid,
                gid: st.gid,
                mtime: st.mtime,
                ..Default::default()
            };
            if !st.is_dir() && st.links > 1 {
                if let Some(first) = seen.get(&st.inode) {
                    h.kind = EntryKind::HardLink;
                    h.link = first.clone();
                    w.append(&h, &[]).await?;
                    report.hard_links += 1;
                    continue;
                }
                seen.insert(st.inode, e.raw_path.clone());
            }
            let ftype = st.mode & mode::IFMT;
            if ftype == mode::IFSOCK {
                report.sockets_skipped += 1;
                continue;
            }
            if ftype != mode::IFLNK {
                h.xattrs = self.xattrs_inode(&inode).await?.into_iter().map(|x| (x.name, x.value)).collect();
                report.xattrs += h.xattrs.len() as u64;
            }
            match ftype {
                mode::IFDIR => {
                    h.kind = EntryKind::Directory;
                    h.path.push(b'/');
                    w.append(&h, &[]).await?;
                    report.directories += 1;
                }
                mode::IFLNK => {
                    h.kind = EntryKind::Symlink;
                    h.link = self.symlink_target(&inode).await?;
                    w.append(&h, &[]).await?;
                    report.symlinks += 1;
                }
                mode::IFCHR | mode::IFBLK | mode::IFIFO => {
                    h.kind = match ftype {
                        mode::IFCHR => EntryKind::CharDevice,
                        mode::IFBLK => EntryKind::BlockDevice,
                        _ => EntryKind::Fifo,
                    };
                    (h.major, h.minor) = st.rdev;
                    w.append(&h, &[]).await?;
                    report.devices += 1;
                }
                mode::IFREG => {
                    h.size = st.size;
                    w.begin(&h).await?;
                    let extents = self.extents(&inode, false).await?;
                    let mut off = 0;
                    while off < st.size {
                        let chunk = self.read_inode_range(&inode, &extents, off, CHUNK).await?;
                        w.data(&chunk).await?;
                        off += chunk.len() as u64;
                    }
                    report.files += 1;
                    report.bytes += st.size;
                }
                other => {
                    return Err(Error::Corrupt(format!("{}: file type {other:#o}", e.path)));
                }
            }
        }
        Ok(report)
    }

    /// Copy a tree onto the local filesystem under `dest`, which must be a
    /// new or empty directory.
    ///
    /// Regular files (zero-filled 4 KiB pieces become holes), directories, symlinks and hard
    /// links are created, with their permission bits and modification times;
    /// ownership too if asked. Device nodes, FIFOs and sockets are not, and
    /// neither are extended attributes — both are counted in the report.
    pub async fn extract(&self, root: &str, dest: &Path, opts: &ExtractOptions) -> Result<ExtractReport> {
        let mut report = ExtractReport::default();
        tokio::fs::create_dir_all(dest).await?;
        let mut seen: HashMap<u64, PathBuf> = HashMap::new();
        // Directories get their final mode and time once everything inside
        // them is written: a read-only directory would refuse its children,
        // and each child written would move its time.
        let mut dirs: Vec<(PathBuf, u16, Timestamp)> = Vec::new();
        for e in self.walk(root).await? {
            let st = e.stat;
            let path = dest.join(std::ffi::OsStr::from_bytes(&e.raw_path));
            let inode = self.inode(st.inode).await?;
            let ftype = st.mode & mode::IFMT;
            if !st.is_dir() && st.links > 1 {
                if let Some(first) = seen.get(&st.inode) {
                    tokio::fs::hard_link(first, &path).await?;
                    report.hard_links += 1;
                    continue;
                }
                seen.insert(st.inode, path.clone());
            }
            if ftype != mode::IFLNK {
                report.xattrs_skipped += self.xattrs_inode(&inode).await?.len() as u64;
            }
            match ftype {
                mode::IFDIR => {
                    tokio::fs::create_dir(&path).await?;
                    set_mode(&path, 0o700).await?;
                    dirs.push((path.clone(), st.mode, st.mtime));
                    report.directories += 1;
                }
                mode::IFLNK => {
                    let target = self.symlink_target(&inode).await?;
                    tokio::fs::symlink(std::ffi::OsStr::from_bytes(&target), &path).await?;
                    report.symlinks += 1;
                }
                mode::IFREG => {
                    let mut f = tokio::fs::File::create(&path).await?;
                    let extents = self.extents(&inode, false).await?;
                    let mut off = 0;
                    while off < st.size {
                        let chunk = self.read_inode_range(&inode, &extents, off, CHUNK).await?;
                        write_sparse(&mut f, &chunk).await?;
                        off += chunk.len() as u64;
                    }
                    f.set_len(st.size).await?;
                    f.flush().await?;
                    let f = f.into_std().await;
                    f.set_modified(system_time(st.mtime))?;
                    drop(f);
                    // Before the mode: a change of owner clears set-ID bits.
                    if opts.owner {
                        std::os::unix::fs::lchown(&path, Some(st.uid), Some(st.gid))?;
                    }
                    set_mode(&path, st.mode).await?;
                    report.files += 1;
                    report.bytes += st.size;
                    continue;
                }
                _ => {
                    report.specials_skipped.push(e.path.clone());
                    continue;
                }
            }
            if opts.owner {
                std::os::unix::fs::lchown(&path, Some(st.uid), Some(st.gid))?;
            }
        }
        for (path, m, mtime) in dirs.into_iter().rev() {
            let f = std::fs::File::open(&path)?;
            f.set_modified(system_time(mtime))?;
            set_mode(&path, m).await?;
        }
        Ok(report)
    }
}

/// Write `buf` at the file's position, seeking over every all-zero 4 KiB
/// piece rather than writing it, so holes stay holes (as `cp --sparse`).
async fn write_sparse(f: &mut tokio::fs::File, buf: &[u8]) -> Result<()> {
    const PIECE: usize = 4096;
    let mut at = 0;
    while at < buf.len() {
        let zero = |i: usize| buf[i..(i + PIECE).min(buf.len())].iter().all(|&b| b == 0);
        let run_zero = zero(at);
        let mut end = at;
        while end < buf.len() && zero(end) == run_zero {
            end = (end + PIECE).min(buf.len());
        }
        if run_zero {
            f.seek(std::io::SeekFrom::Current((end - at) as i64)).await?;
        } else {
            f.write_all(&buf[at..end]).await?;
        }
        at = end;
    }
    Ok(())
}

async fn set_mode(path: &Path, m: u16) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perm = std::fs::Permissions::from_mode((m & 0o7777) as u32);
    Ok(tokio::fs::set_permissions(path, perm).await?)
}

fn system_time(t: Timestamp) -> std::time::SystemTime {
    use std::time::{Duration, UNIX_EPOCH};
    if t.secs >= 0 {
        UNIX_EPOCH + Duration::new(t.secs as u64, t.nsecs)
    } else {
        UNIX_EPOCH - Duration::from_secs(t.secs.unsigned_abs()) + Duration::from_nanos(t.nsecs as u64)
    }
}
