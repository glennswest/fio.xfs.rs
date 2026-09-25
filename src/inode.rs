//! On-disk inodes: the core, and where its two forks sit.

use crate::bytes::{be16, be32, be64};
use crate::crc;
use crate::error::{corrupt, Result};
use crate::sb::Superblock;

/// `IN`.
const MAGIC: u16 = 0x494e;

/// File type and permission bits of `mode`.
pub mod mode {
    /// The file type mask.
    pub const IFMT: u16 = 0o170000;
    /// Socket.
    pub const IFSOCK: u16 = 0o140000;
    /// Symbolic link.
    pub const IFLNK: u16 = 0o120000;
    /// Regular file.
    pub const IFREG: u16 = 0o100000;
    /// Block device.
    pub const IFBLK: u16 = 0o060000;
    /// Directory.
    pub const IFDIR: u16 = 0o040000;
    /// Character device.
    pub const IFCHR: u16 = 0o020000;
    /// FIFO.
    pub const IFIFO: u16 = 0o010000;
}

/// How a fork's contents are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// A device number, for device nodes.
    Dev,
    /// Inline in the inode (short-form directories, short symlinks, small
    /// attribute sets).
    Local,
    /// A list of extents in the inode.
    Extents,
    /// A B+tree of extents, rooted in the inode.
    Btree,
}

impl Format {
    fn from_u8(v: u8, what: &str) -> Result<Self> {
        Ok(match v {
            0 => Format::Dev,
            1 => Format::Local,
            2 => Format::Extents,
            3 => Format::Btree,
            v => return Err(corrupt(format!("{what} fork format {v}"))),
        })
    }
}

/// A point in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Timestamp {
    /// Seconds since the Unix epoch; negative before it.
    pub secs: i64,
    /// Nanoseconds within the second.
    pub nsecs: u32,
}

// `di_flags`.
const DIFLAG_REALTIME: u16 = 0x1;
// `di_flags2`.
const DIFLAG2_BIGTIME: u64 = 0x8;
const DIFLAG2_NREXT64: u64 = 0x10;

/// Seconds between the bigtime epoch (the lowest 32-bit time) and the Unix one.
const BIGTIME_EPOCH_OFFSET: i64 = 1 << 31;

/// A parsed inode, with both forks' raw bytes.
#[derive(Debug, Clone)]
pub struct Inode {
    /// Its number.
    pub ino: u64,
    /// Mode, including the file type bits.
    pub mode: u16,
    /// Inode version: 1, 2 or 3.
    pub version: u8,
    /// Data fork format.
    pub format: Format,
    /// Owner.
    pub uid: u32,
    /// Group.
    pub gid: u32,
    /// Hard links.
    pub nlink: u32,
    /// Project ID.
    pub projid: u32,
    /// Last access.
    pub atime: Timestamp,
    /// Last modification of the contents.
    pub mtime: Timestamp,
    /// Last change of the inode.
    pub ctime: Timestamp,
    /// Creation, on v3 inodes.
    pub crtime: Option<Timestamp>,
    /// Size in bytes.
    pub size: u64,
    /// Blocks allocated, in filesystem blocks, both forks.
    pub nblocks: u64,
    /// Data fork extents.
    pub nextents: u64,
    /// Attribute fork extents.
    pub anextents: u32,
    /// Attribute fork format, when there is an attribute fork.
    pub aformat: Option<Format>,
    /// `di_flags`.
    pub flags: u16,
    /// `di_flags2`, on v3 inodes.
    pub flags2: u64,
    /// Generation number.
    pub generation: u32,
    /// The data fork's bytes.
    pub data_fork: Vec<u8>,
    /// The attribute fork's bytes; empty when there is none.
    pub attr_fork: Vec<u8>,
}

impl Inode {
    /// Parse the inode `ino` from its `inode_size` bytes.
    pub fn parse(sb: &Superblock, ino: u64, buf: &[u8]) -> Result<Self> {
        let isize = sb.inode_size as usize;
        if buf.len() < isize {
            return Err(corrupt(format!("inode {ino}: short buffer")));
        }
        let buf = &buf[..isize];
        if be16(buf, 0) != MAGIC {
            return Err(corrupt(format!("inode {ino}: bad magic")));
        }
        let version = buf[4];
        let core = match version {
            1 | 2 if !sb.is_v5() => 100,
            3 if sb.is_v5() => 176,
            v => return Err(corrupt(format!("inode {ino}: version {v}"))),
        };
        if version == 3 {
            if !crc::verify(buf, 100) {
                return Err(corrupt(format!("inode {ino}: checksum")));
            }
            if be64(buf, 152) != ino {
                return Err(corrupt(format!("inode {ino}: records itself as {}", be64(buf, 152))));
            }
        }

        let flags2 = if version == 3 { be64(buf, 120) } else { 0 };
        let bigtime = flags2 & DIFLAG2_BIGTIME != 0;
        let ts = |o: usize| -> Timestamp {
            let raw = be64(buf, o);
            if bigtime {
                Timestamp {
                    secs: (raw / 1_000_000_000) as i64 - BIGTIME_EPOCH_OFFSET,
                    nsecs: (raw % 1_000_000_000) as u32,
                }
            } else {
                Timestamp { secs: (raw >> 32) as u32 as i32 as i64, nsecs: raw as u32 }
            }
        };

        let (nextents, anextents) = if flags2 & DIFLAG2_NREXT64 != 0 {
            (be64(buf, 24), be32(buf, 76))
        } else {
            (be32(buf, 76) as u64, be16(buf, 80) as u32)
        };

        let format = Format::from_u8(buf[5], "data")?;
        let forkoff = buf[82] as usize * 8;
        let literal = isize - core;
        let (data_fork, attr_fork, aformat) = if forkoff == 0 {
            (buf[core..].to_vec(), Vec::new(), None)
        } else {
            if forkoff >= literal {
                return Err(corrupt(format!("inode {ino}: fork offset {forkoff}")));
            }
            (
                buf[core..core + forkoff].to_vec(),
                buf[core + forkoff..].to_vec(),
                Some(Format::from_u8(buf[83], "attribute")?),
            )
        };

        Ok(Inode {
            ino,
            mode: be16(buf, 2),
            version,
            format,
            uid: be32(buf, 8),
            gid: be32(buf, 12),
            nlink: if version == 1 { be16(buf, 6) as u32 } else { be32(buf, 16) },
            projid: if version == 1 {
                0
            } else {
                (be16(buf, 22) as u32) << 16 | be16(buf, 20) as u32
            },
            atime: ts(32),
            mtime: ts(40),
            ctime: ts(48),
            crtime: (version == 3).then(|| ts(144)),
            size: be64(buf, 56),
            nblocks: be64(buf, 64),
            nextents,
            anextents,
            aformat,
            flags: be16(buf, 90),
            flags2,
            generation: be32(buf, 92),
            data_fork,
            attr_fork,
        })
    }

    /// The file type bits of `mode`.
    pub fn file_type(&self) -> u16 {
        self.mode & mode::IFMT
    }

    /// Whether this is a directory.
    pub fn is_dir(&self) -> bool {
        self.file_type() == mode::IFDIR
    }

    /// Whether the data lives on a realtime device rather than the data one.
    pub fn is_realtime(&self) -> bool {
        self.flags & DIFLAG_REALTIME != 0
    }

    /// A device node's number as `(major, minor)`.
    ///
    /// XFS keeps it as one 32-bit word, the minor in the low 18 bits.
    pub fn device(&self) -> (u32, u32) {
        if self.format != Format::Dev || self.data_fork.len() < 4 {
            return (0, 0);
        }
        let dev = be32(&self.data_fork, 0);
        (dev >> 18, dev & 0x3ffff)
    }
}
