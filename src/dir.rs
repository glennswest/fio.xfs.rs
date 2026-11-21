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
    /// The type a directory entry's `ftype` byte names.
    pub fn from_ftype(v: u8) -> Self {
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

    /// The `ftype` byte a directory entry stores for this type.
    pub fn code(&self) -> u8 {
        match self {
            FileType::Unknown => 0,
            FileType::File => 1,
            FileType::Directory => 2,
            FileType::CharDevice => 3,
            FileType::BlockDevice => 4,
            FileType::Fifo => 5,
            FileType::Socket => 6,
            FileType::Symlink => 7,
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
        // Every record is a multiple of 8 bytes at an 8-byte offset, so a
        // record that starts before `end` has at least its first 8 bytes.
        if p + 8 > end {
            return Err(corrupt(format!("directory {dir_ino}: entry runs off the block")));
        }
        if be16(buf, p) == 0xffff {
            // Free space: tag, length, ... length again at the end.
            let len = be16(buf, p + 2) as usize;
            if len < 8 || len % 8 != 0 || p + len > end {
                return Err(corrupt(format!("directory {dir_ino}: free space of {len} bytes at {p}")));
            }
            p += len;
            continue;
        }
        // A live entry: inode (8), name length (1), name, ftype, tag (2),
        // rounded up to 8 — at least 16 bytes. Its fixed header must fit
        // before the name length is read (finding 15cd3aece9ef).
        if p + 9 > end {
            return Err(corrupt(format!("directory {dir_ino}: entry runs off the block")));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytes::{put16, put32, put64};
    use crate::error::Error;

    /// A v4 superblock (4 KiB blocks, no ftype, no CRC) or a v5 one (ftype).
    fn sb(v5: bool) -> Superblock {
        let mut b = vec![0u8; 512];
        put32(&mut b, 0, 0x5846_5342);
        put32(&mut b, 4, 4096);
        put64(&mut b, 8, 4 << 16);
        put64(&mut b, 56, 128);
        put32(&mut b, 84, 1 << 16);
        put32(&mut b, 88, 4);
        put16(&mut b, 102, 512);
        put16(&mut b, 104, 512);
        put16(&mut b, 106, 8);
        b[120] = 12;
        b[121] = 9;
        b[122] = 9;
        b[123] = 3;
        b[124] = 16;
        if v5 {
            put16(&mut b, 100, 0xb4a5);
            put32(&mut b, 216, crate::sb::incompat::FTYPE);
            crc::stamp(&mut b, 224);
        } else {
            put16(&mut b, 100, 0x2004 | 0x0400); // DIRV2 | LOGV2
        }
        Superblock::parse(&b).unwrap()
    }

    const DIR: u64 = 128;

    /// A data-form block of `size` bytes: header, then a free record from the
    /// header to `tail`, then `fill` (8 bytes of non-free entry start)
    /// repeated to the end.
    fn data_block(v5: bool, size: usize, tail: usize) -> Vec<u8> {
        let hdr = if v5 { 64 } else { 16 };
        let mut b = vec![0u8; size];
        put32(&mut b, 0, if v5 { XDD3 } else { XD2D });
        if v5 {
            put64(&mut b, 40, DIR);
        }
        let free = size - tail - hdr;
        put16(&mut b, hdr, 0xffff);
        put16(&mut b, hdr + 2, free as u16);
        put16(&mut b, hdr + free - 2, hdr as u16);
        // A plausible inode number where the next entry would start.
        put64(&mut b, hdr + free, 1 << 40);
        b
    }

    fn parse(sb: &Superblock, mut b: Vec<u8>) -> std::thread::Result<Result<Vec<RawEntry>>> {
        if sb.is_v5() {
            crc::stamp(&mut b, 4);
        }
        std::panic::catch_unwind(move || {
            let mut out = Vec::new();
            parse_data_block(sb, &b, DIR, &mut out).map(|()| out)
        })
    }

    fn assert_corrupt(r: std::thread::Result<Result<Vec<RawEntry>>>, what: &str) {
        match r {
            Ok(Err(Error::Corrupt(_))) => {}
            Ok(other) => panic!("{what}: {other:?}"),
            Err(_) => panic!("{what}: panicked"),
        }
    }

    /// Finding 15cd3aece9ef: a live entry starting in the block's last 8
    /// bytes (the issue's 512-byte v4 example: free record at 16 of 488,
    /// leaving p = 504) is corrupt, not an index out of bounds.
    #[test]
    fn an_eight_byte_tail_is_corrupt_not_a_panic() {
        let v4 = sb(false);
        assert_corrupt(parse(&v4, data_block(false, 512, 8)), "v4 512-byte block");
        for v5 in [false, true] {
            let s = sb(v5);
            for size in [512usize, 4096] {
                assert_corrupt(parse(&s, data_block(v5, size, 8)), &format!("v5={v5} size={size}"));
            }
        }
    }

    /// Every tail length a live entry could start at: too short for its
    /// aligned length is corrupt, never a panic; a well-formed entry that
    /// fits is read.
    #[test]
    fn every_short_tail_is_handled() {
        for v5 in [false, true] {
            let s = sb(v5);
            for tail in (8..=64).step_by(8) {
                let mut b = data_block(v5, 4096, tail);
                let p = 4096 - tail;
                // namelen 1, name "a", ftype "regular file": as much of it
                // as fits in the tail.
                for (i, v) in [1u8, b'a', 1].into_iter().enumerate().take(if v5 { 3 } else { 2 }) {
                    if p + 8 + i < b.len() {
                        b[p + 8 + i] = v;
                    }
                }
                let len = (8 + 1 + 1 + s.has_ftype() as usize + 2 + 7) & !7;
                if len == tail {
                    put16(&mut b, p + len - 2, p as u16);
                    let out = parse(&s, b).unwrap().unwrap();
                    assert_eq!(out.len(), 1);
                    assert_eq!(out[0].name, b"a");
                } else if len > tail {
                    assert_corrupt(parse(&s, b), &format!("v5={v5} tail={tail}"));
                } else {
                    // The entry fits with room to spare; what follows it is
                    // zeroes (an inode-0 entry with an empty name): corrupt.
                    assert_corrupt(parse(&s, b), &format!("v5={v5} tail={tail} trailing"));
                }
            }
        }
    }

    /// A free record longer than what is left of the block is corrupt, not
    /// skipped past the end.
    #[test]
    fn a_free_record_past_the_end_is_corrupt() {
        for v5 in [false, true] {
            let s = sb(v5);
            let hdr = if v5 { 64 } else { 16 };
            let mut b = data_block(v5, 512, 8);
            put16(&mut b, hdr + 2, (512 - hdr + 8) as u16);
            assert_corrupt(parse(&s, b), &format!("v5={v5}"));
        }
    }
}
