//! Directories, in all four of their forms.
//!
//! Short-form directories live in the inode. Block, leaf and node directories
//! differ only in how names are *indexed*; the names themselves are always in
//! data blocks in the first 32 GiB of the directory's logical space, so
//! listing any of them is a walk of those data blocks. The leaf and node
//! blocks above that are hash indexes, needed to find a name quickly and not
//! needed to find every name.

use crate::bytes::{be16, be32, be64};
use crate::crc;
use crate::error::{corrupt, Result};
use crate::sb::Superblock;

/// Byte offset in a directory's logical space where the leaf (index)
/// blocks begin; data blocks lie below it.
pub const LEAF_OFFSET: u64 = 1 << 35;

const XD2B: u32 = 0x5844_3242;
const XD2D: u32 = 0x5844_3244;
const XDB3: u32 = 0x5844_4233;
const XDD3: u32 = 0x5844_4433;

/// The type of a name, as recorded in a directory entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    /// Not recorded (a filesystem without `ftype`).
    Unknown,
    /// Regular file.
    File,
    /// Directory.
    Directory,
    /// Character device.
    CharDevice,
    /// Block device.
    BlockDevice,
    /// FIFO.
    Fifo,
    /// Socket.
    Socket,
    /// Symbolic link.
    Symlink,
}

impl FileType {
    fn from_ftype(v: u8) -> Self {
        match v {
            1 => FileType::File,
            2 => FileType::Directory,
            3 => FileType::CharDevice,
            4 => FileType::BlockDevice,
            5 => FileType::Fifo,
            6 => FileType::Socket,
            7 => FileType::Symlink,
            _ => FileType::Unknown,
        }
    }

    /// The type named by an inode's mode.
    pub fn from_mode(mode: u16) -> Self {
        use crate::inode::mode::*;
        match mode & IFMT {
            IFREG => FileType::File,
            IFDIR => FileType::Directory,
            IFCHR => FileType::CharDevice,
            IFBLK => FileType::BlockDevice,
            IFIFO => FileType::Fifo,
            IFSOCK => FileType::Socket,
            IFLNK => FileType::Symlink,
            _ => FileType::Unknown,
        }
    }
}

/// One name in a directory, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEntry {
    /// The name's bytes.
    pub name: Vec<u8>,
    /// Its inode.
    pub ino: u64,
    /// Its type, when the filesystem records one.
    pub ftype: FileType,
}

/// Parse a short-form directory. Returns the parent and the entries; `.`
/// and `..` are not stored in this form.
pub fn parse_shortform(sb: &Superblock, fork: &[u8], size: u64) -> Result<(u64, Vec<RawEntry>)> {
    let size = size as usize;
    if size < 6 || size > fork.len() {
        return Err(corrupt(format!("short-form directory of {size} bytes")));
    }
    let buf = &fork[..size];
    let count = buf[0] as usize;
    let i8count = buf[1];
    let isz = if i8count > 0 { 8 } else { 4 };
    let ino_at = |o: usize| -> Result<u64> {
        if o + isz > buf.len() {
            return Err(corrupt("short-form directory entry runs off the end"));
        }
        Ok(if isz == 8 { be64(buf, o) } else { be32(buf, o) as u64 })
    };
    let parent = ino_at(2)?;
    let ftype = sb.has_ftype();
    let mut p = 2 + isz;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        if p + 3 > buf.len() {
            return Err(corrupt("short-form directory entry runs off the end"));
        }
        let namelen = buf[p] as usize;
        // namelen, then a 2-byte offset kept only to order readdir cookies.
        let name_at = p + 3;
        let after = name_at + namelen;
        if namelen == 0 || after + ftype as usize > buf.len() {
            return Err(corrupt("short-form directory name runs off the end"));
        }
        let t = if ftype { FileType::from_ftype(buf[after]) } else { FileType::Unknown };
        let ino = ino_at(after + ftype as usize)?;
        out.push(RawEntry { name: buf[name_at..after].to_vec(), ino, ftype: t });
        p = after + ftype as usize + isz;
    }
    Ok((parent, out))
}

/// Parse one directory data block (`dir_block_size` bytes) at directory
/// block number `dblk`, appending its names to `out`, `.` and `..` included.
pub fn parse_data_block(
    sb: &Superblock,
    buf: &[u8],
    dir_ino: u64,
    out: &mut Vec<RawEntry>,
) -> Result<()> {
    let magic = be32(buf, 0);
    let (hdr, block_form) = match magic {
        XD2B if !sb.is_v5() => (16, true),
        XD2D if !sb.is_v5() => (16, false),
        XDB3 if sb.is_v5() => (64, true),
        XDD3 if sb.is_v5() => (64, false),
        _ => return Err(corrupt(format!("directory {dir_ino}: data block magic {magic:#x}"))),
    };
    if sb.is_v5() {
        if !crc::verify(buf, 4) {
            return Err(corrupt(format!("directory {dir_ino}: data block checksum")));
        }
        if be64(buf, 40) != dir_ino {
            return Err(corrupt(format!("directory {dir_ino}: data block owned by {}", be64(buf, 40))));
        }
    }
    // A single-block directory keeps its hash index and a tail at the end of
    // the same block; names stop where the index starts.
    let end = if block_form {
        let count = be32(buf, buf.len() - 8) as usize;
        buf.len()
            .checked_sub(8 + count * 8)
            .filter(|&e| e >= hdr)
            .ok_or_else(|| corrupt(format!("directory {dir_ino}: block tail count {count}")))?
    } else {
        buf.len()
    };
    let ftype = sb.has_ftype() as usize;
    let mut p = hdr;
    while p < end {
        if p + 8 > end {
            return Err(corrupt(format!("directory {dir_ino}: entry runs off the block")));
        }
        if be16(buf, p) == 0xffff {
            // Free space: tag, length, ... length again at the end.
            let len = be16(buf, p + 2) as usize;
            if len < 8 || len % 8 != 0 {
                return Err(corrupt(format!("directory {dir_ino}: free space of {len} bytes")));
            }
            p += len;
            continue;
        }
        let ino = be64(buf, p);
        let namelen = buf[p + 8] as usize;
        let len = (8 + 1 + namelen + ftype + 2 + 7) & !7;
        if namelen == 0 || p + len > end {
            return Err(corrupt(format!("directory {dir_ino}: entry runs off the block")));
        }
        let name = buf[p + 9..p + 9 + namelen].to_vec();
        let t = if ftype == 1 { FileType::from_ftype(buf[p + 9 + namelen]) } else { FileType::Unknown };
        out.push(RawEntry { name, ino, ftype: t });
        p += len;
    }
    Ok(())
}
