//! The superblock: the geometry every other structure is located by.

use crate::bytes::{be16, be32, be64};
use crate::crc;
use crate::error::{Error, Result};

/// `XFSB`.
pub const MAGIC: u32 = 0x5846_5342;

// Version 4 feature bits, in `versionnum`.
const VERSION_DIRV2: u16 = 0x2000;
const VERSION_MOREBITS: u16 = 0x8000;
// Version 4 `features2` bits.
const FEATURES2_FTYPE: u32 = 0x200;
const FEATURES2_CRC: u32 = 0x100;

/// Incompatible features (v5) a reader must understand.
pub mod incompat {
    /// Directory entries record the file type.
    pub const FTYPE: u32 = 0x1;
    /// Inode chunks may be sparse.
    pub const SPINODES: u32 = 0x2;
    /// The metadata UUID differs from the user-visible one.
    pub const META_UUID: u32 = 0x4;
    /// Timestamps are 64-bit nanosecond counts.
    pub const BIGTIME: u32 = 0x8;
    /// `xfs_repair` must run before the filesystem is used.
    pub const NEEDSREPAIR: u32 = 0x10;
    /// Extent counters are 64 bits wide.
    pub const NREXT64: u32 = 0x20;
    /// File content exchange.
    pub const EXCHRANGE: u32 = 0x40;
    /// Directory parent pointers, kept as extended attributes.
    pub const PARENT: u32 = 0x80;
    /// Metadata directory tree.
    pub const METADIR: u32 = 0x100;
    /// Zoned realtime device.
    pub const ZONED: u32 = 0x200;

    /// Everything this reader understands. None of these change where a
    /// regular file's data, a directory's entries or an attribute is found
    /// on the data device.
    pub const KNOWN: u32 =
        FTYPE | SPINODES | META_UUID | BIGTIME | NREXT64 | EXCHRANGE | PARENT | METADIR | ZONED;
}

/// The parts of the primary superblock a reader needs.
#[derive(Debug, Clone)]
pub struct Superblock {
    /// Filesystem block size in bytes.
    pub block_size: u32,
    /// Blocks on the data device.
    pub data_blocks: u64,
    /// Filesystem UUID.
    pub uuid: [u8; 16],
    /// The root directory's inode.
    pub root_ino: u64,
    /// Blocks in each allocation group.
    pub ag_blocks: u32,
    /// Allocation groups.
    pub ag_count: u32,
    /// Format version: 4 or 5.
    pub version: u8,
    /// Sector size in bytes.
    pub sector_size: u16,
    /// Inode size in bytes.
    pub inode_size: u16,
    /// Inodes per block.
    pub inodes_per_block: u16,
    /// Volume label.
    pub label: String,
    /// log2 of the block size.
    pub block_log: u8,
    /// log2 of inodes per block.
    pub inopb_log: u8,
    /// log2 of blocks per AG, rounded up.
    pub agblk_log: u8,
    /// Inodes allocated.
    pub icount: u64,
    /// Inodes free.
    pub ifree: u64,
    /// Data blocks free.
    pub free_blocks: u64,
    /// log2 of directory blocks per directory block.
    pub dirblk_log: u8,
    /// Version 4 `features2`.
    pub features2: u32,
    /// Compatible features (v5).
    pub features_compat: u32,
    /// Read-only-compatible features (v5).
    pub features_ro_compat: u32,
    /// Incompatible features (v5).
    pub features_incompat: u32,
}


impl Superblock {
    /// Parse and check the primary superblock from the start of a device.
    ///
    /// `buf` must hold at least one sector (512 bytes); the checksum of a v5
    /// superblock covers its whole sector, so a v5 check needs that many.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 512 {
            return Err(Error::NotXfs("fewer than 512 bytes".into()));
        }
        if be32(buf, 0) != MAGIC {
            return Err(Error::NotXfs("no XFSB magic at byte 0".into()));
        }
        let versionnum = be16(buf, 100);
        let version = (versionnum & 0xf) as u8;
        let mut label = buf[108..120].to_vec();
        while label.last() == Some(&0) {
            label.pop();
        }
        let sb = Superblock {
            block_size: be32(buf, 4),
            data_blocks: be64(buf, 8),
            uuid: buf[32..48].try_into().unwrap(),
            root_ino: be64(buf, 56),
            ag_blocks: be32(buf, 84),
            ag_count: be32(buf, 88),
            version,
            sector_size: be16(buf, 102),
            inode_size: be16(buf, 104),
            inodes_per_block: be16(buf, 106),
            label: String::from_utf8_lossy(&label).into_owned(),
            block_log: buf[120],
            inopb_log: buf[123],
            agblk_log: buf[124],
            icount: be64(buf, 128),
            ifree: be64(buf, 136),
            free_blocks: be64(buf, 144),
            dirblk_log: buf[192],
            features2: if versionnum & VERSION_MOREBITS != 0 { be32(buf, 200) } else { 0 },
            features_compat: if version == 5 { be32(buf, 208) } else { 0 },
            features_ro_compat: if version == 5 { be32(buf, 212) } else { 0 },
            features_incompat: if version == 5 { be32(buf, 216) } else { 0 },
        };

        match version {
            4 => {
                if versionnum & VERSION_DIRV2 == 0 {
                    return Err(Error::Unsupported("version 1 directories".into()));
                }
                if sb.features2 & FEATURES2_CRC != 0 {
                    return Err(Error::Corrupt("v4 superblock with the CRC feature".into()));
                }
            }
            5 => {
                let sect = sb.sector_size as usize;
                if buf.len() < sect {
                    return Err(Error::Corrupt("superblock buffer shorter than a sector".into()));
                }
                if !crc::verify(&buf[..sect], 224) {
                    return Err(Error::Corrupt("superblock checksum".into()));
                }
                let unknown = sb.features_incompat & !incompat::KNOWN;
                if unknown != 0 {
                    return Err(Error::Unsupported(format!(
                        "incompatible features {unknown:#x}"
                    )));
                }
                if sb.features_incompat & incompat::NEEDSREPAIR != 0 {
                    return Err(Error::Unsupported("the filesystem needs xfs_repair".into()));
                }
            }
            v => return Err(Error::Unsupported(format!("superblock version {v}"))),
        }

        let bs = sb.block_size;
        if !(512..=65536).contains(&bs)
            || !bs.is_power_of_two()
            || 1u32 << sb.block_log != bs
            || !sb.inode_size.is_power_of_two()
            || !(256..=2048).contains(&sb.inode_size)
            || sb.inodes_per_block as u32 * sb.inode_size as u32 != bs
            || 1u32 << sb.inopb_log != sb.inodes_per_block as u32
            || sb.ag_count == 0
            || sb.ag_blocks == 0
            || (sb.ag_blocks as u64) > 1u64 << sb.agblk_log
            || sb.agblk_log > 31
            || (bs as u64) << sb.dirblk_log > 65536
        {
            return Err(Error::Corrupt("superblock geometry".into()));
        }
        Ok(sb)
    }

    /// Whether this is a v5 (CRC) filesystem.
    pub fn is_v5(&self) -> bool {
        self.version == 5
    }

    /// Whether directory entries carry the file type.
    pub fn has_ftype(&self) -> bool {
        if self.is_v5() {
            self.features_incompat & incompat::FTYPE != 0
        } else {
            self.features2 & FEATURES2_FTYPE != 0
        }
    }

    /// Whether directories carry parent pointers as attributes.
    pub fn has_parent(&self) -> bool {
        self.features_incompat & incompat::PARENT != 0
    }

    /// Whether extent counters may be 64 bits wide.
    pub fn has_nrext64(&self) -> bool {
        self.features_incompat & incompat::NREXT64 != 0
    }

    /// Directory block size in bytes.
    pub fn dir_block_size(&self) -> u32 {
        self.block_size << self.dirblk_log
    }

    /// Byte offset on the device of filesystem block `fsbno`.
    ///
    /// XFS block numbers are not linear: the AG number sits above bit
    /// `agblk_log`, and an AG is `ag_blocks` long, not `1 << agblk_log`.
    pub fn fsb_to_byte(&self, fsbno: u64) -> Result<u64> {
        let agno = fsbno >> self.agblk_log;
        let agbno = fsbno & ((1u64 << self.agblk_log) - 1);
        if agno >= self.ag_count as u64 || agbno >= self.ag_blocks as u64 {
            return Err(Error::Corrupt(format!("block {fsbno} outside the filesystem")));
        }
        Ok((agno * self.ag_blocks as u64 + agbno) << self.block_log)
    }

    /// Byte offset on the device of inode `ino`.
    pub fn ino_to_byte(&self, ino: u64) -> Result<u64> {
        let agino_log = self.agblk_log as u32 + self.inopb_log as u32;
        let agno = ino >> agino_log;
        let agino = ino & ((1u64 << agino_log) - 1);
        let agbno = agino >> self.inopb_log;
        let index = agino & ((1u64 << self.inopb_log) - 1);
        if ino == 0 || agno >= self.ag_count as u64 || agbno >= self.ag_blocks as u64 {
            return Err(Error::Corrupt(format!("inode {ino} outside the filesystem")));
        }
        Ok(((agno * self.ag_blocks as u64 + agbno) << self.block_log)
            + index * self.inode_size as u64)
    }
}
