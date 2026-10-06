//! The superblock: the geometry every other structure is located by.

use crate::bytes::{be16, be32, be64};
use crate::crc;
use crate::error::{Error, Result};

/// `XFSB`.
pub const MAGIC: u32 = 0x5846_5342;

// Version 4 feature bits, in `versionnum`.
const VERSION_LOGV2: u16 = 0x0400;
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

/// Read-only-compatible features (v5): structures a writer must keep up to date.
pub mod ro_compat {
    /// A B+tree of inode chunks with free inodes.
    pub const FINOBT: u32 = 0x1;
    /// The reverse-mapping B+tree: who owns every block.
    pub const RMAPBT: u32 = 0x2;
    /// Shared extents, counted in the refcount B+tree.
    pub const REFLINK: u32 = 0x4;
    /// The AGI counts the inode B+trees' blocks.
    pub const INOBTCOUNT: u32 = 0x8;

    /// Everything the writer keeps up to date.
    pub const KNOWN: u32 = FINOBT | RMAPBT | REFLINK | INOBTCOUNT;
}

/// `versionnum` bit: case-insensitive directory names (`-n version=ci`).
pub const VERSION_BORGBIT: u16 = 0x4000;

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
    /// The version number with its feature bits, as stored.
    pub versionnum: u16,
    /// First block of the internal log; 0 when the log is on its own device.
    pub log_start: u64,
    /// Blocks in the log.
    pub log_blocks: u32,
    /// The UUID stamped into metadata: `uuid` unless the `META_UUID`
    /// feature says it was changed after the filesystem was made.
    pub meta_uuid: [u8; 16],
    /// Inode chunk alignment, in blocks.
    pub inode_align: u32,
    /// Sparse inode chunk alignment, in blocks.
    pub spino_align: u32,
    /// Most of the space inodes may take, in percent; 0 for no limit.
    pub imax_pct: u8,
    /// Log-incompatible features.
    pub features_log_incompat: u32,
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
            versionnum,
            log_start: be64(buf, 48),
            log_blocks: be32(buf, 96),
            meta_uuid: buf[32..48].try_into().unwrap(),
            inode_align: be32(buf, 180),
            spino_align: if version == 5 { be32(buf, 228) } else { 0 },
            imax_pct: buf[127],
            features_log_incompat: if version == 5 { be32(buf, 220) } else { 0 },
        };
        let mut sb = sb;
        if sb.features_incompat & incompat::META_UUID != 0 {
            sb.meta_uuid = buf[248..264].try_into().unwrap();
        }

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

    /// Whether the log is version 2 (log stripe units, records over 32 KiB).
    pub fn has_logv2(&self) -> bool {
        self.is_v5() || self.versionnum & VERSION_LOGV2 != 0
    }

    /// Whether the log is on a device of its own, not inside this one.
    pub fn has_external_log(&self) -> bool {
        self.log_start == 0
    }

    /// Whether directories carry parent pointers as attributes.
    pub fn has_parent(&self) -> bool {
        self.features_incompat & incompat::PARENT != 0
    }

    /// Whether extent counters may be 64 bits wide.
    pub fn has_nrext64(&self) -> bool {
        self.features_incompat & incompat::NREXT64 != 0
    }

    /// Whether timestamps are 64-bit nanosecond counts.
    pub fn has_bigtime(&self) -> bool {
        self.features_incompat & incompat::BIGTIME != 0
    }

    /// Whether inode chunks may be sparse (and inode B+tree records say so).
    pub fn has_sparse_inodes(&self) -> bool {
        self.features_incompat & incompat::SPINODES != 0
    }

    /// Whether there is a free inode B+tree.
    pub fn has_finobt(&self) -> bool {
        self.features_ro_compat & ro_compat::FINOBT != 0
    }

    /// Whether there is a reverse-mapping B+tree.
    pub fn has_rmapbt(&self) -> bool {
        self.features_ro_compat & ro_compat::RMAPBT != 0
    }

    /// Whether the AGI counts the inode B+trees' blocks.
    pub fn has_inobtcount(&self) -> bool {
        self.features_ro_compat & ro_compat::INOBTCOUNT != 0
    }

    /// Whether the writer can change this filesystem, and if not, why not.
    pub fn check_writable(&self) -> Result<()> {
        let no = |why: &str| Err(Error::Unsupported(format!("writing: {why}")));
        if !self.is_v5() {
            return no("version 4 filesystems are read-only here (mkfs-xfs makes only v5)");
        }
        if self.versionnum & VERSION_BORGBIT != 0 {
            return no("case-insensitive directories");
        }
        let unknown_ro = self.features_ro_compat & !ro_compat::KNOWN;
        if unknown_ro != 0 {
            return no(&format!("read-only-compatible features {unknown_ro:#x}"));
        }
        let handled = incompat::FTYPE | incompat::SPINODES | incompat::META_UUID | incompat::BIGTIME | incompat::NREXT64 | incompat::EXCHRANGE;
        let unhandled = self.features_incompat & !handled;
        if unhandled != 0 {
            return no(&format!("incompatible features {unhandled:#x} (parent pointers, metadir or zoned)"));
        }
        if self.features_incompat & incompat::FTYPE == 0 {
            return no("directories without file types");
        }
        if self.features_log_incompat != 0 {
            return no("log-incompatible features are set: mount and unmount it once");
        }
        if self.inodes_per_block > 64 {
            return no("more than 64 inodes in a block");
        }
        Ok(())
    }

    /// Bytes in an AG header sector.
    pub fn sect(&self) -> usize {
        self.sector_size as usize
    }

    /// Filesystem block number of `agbno` in AG `agno`.
    pub fn agb_to_fsb(&self, agno: u32, agbno: u32) -> u64 {
        (agno as u64) << self.agblk_log | agbno as u64
    }

    /// The AG and AG block of filesystem block `fsbno`.
    pub fn fsb_to_agb(&self, fsbno: u64) -> (u32, u32) {
        ((fsbno >> self.agblk_log) as u32, (fsbno & ((1u64 << self.agblk_log) - 1)) as u32)
    }

    /// Byte offset on the device of block `agbno` in AG `agno`.
    pub fn agb_to_byte(&self, agno: u32, agbno: u32) -> u64 {
        (agno as u64 * self.ag_blocks as u64 + agbno as u64) << self.block_log
    }

    /// Blocks in AG `agno`: the last one may be short.
    pub fn ag_len(&self, agno: u32) -> u32 {
        let start = agno as u64 * self.ag_blocks as u64;
        (self.data_blocks - start).min(self.ag_blocks as u64) as u32
    }

    /// log2 of inodes in an AG's inode number space.
    pub fn agino_log(&self) -> u32 {
        self.agblk_log as u32 + self.inopb_log as u32
    }

    /// Inode number of `agino` in AG `agno`.
    pub fn agino_to_ino(&self, agno: u32, agino: u32) -> u64 {
        (agno as u64) << self.agino_log() | agino as u64
    }

    /// The AG and AG inode of inode `ino`.
    pub fn ino_to_agino(&self, ino: u64) -> (u32, u32) {
        ((ino >> self.agino_log()) as u32, (ino & ((1u64 << self.agino_log()) - 1)) as u32)
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
