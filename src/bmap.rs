//! Block maps: an inode fork's logical-to-physical mapping, whether it is a
//! list of extents in the inode or a B+tree rooted there.

use crate::bytes::{be16, be32, be64};
use crate::crc;
use crate::error::{corrupt, Result};
use crate::sb::Superblock;

/// One run of blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    /// First logical block in the fork.
    pub offset: u64,
    /// First filesystem block on the device.
    pub block: u64,
    /// Blocks in the run.
    pub count: u64,
    /// Allocated but never written: reads as zeroes.
    pub unwritten: bool,
}

impl Extent {
    /// Decode a packed 128-bit extent record.
    pub fn decode(rec: &[u8]) -> Extent {
        let l0 = be64(rec, 0);
        let l1 = be64(rec, 8);
        Extent {
            unwritten: l0 >> 63 != 0,
            offset: (l0 & !(1u64 << 63)) >> 9,
            block: ((l0 & 0x1ff) << 43) | (l1 >> 21),
            count: l1 & ((1 << 21) - 1),
        }
    }

    /// One past the last logical block.
    pub fn end(&self) -> u64 {
        self.offset + self.count
    }
}

/// `BMAP` and `BMA3`: B+tree block magic, v4 and v5.
const BMAP_MAGIC: u32 = 0x424d_4150;
const BMA3_MAGIC: u32 = 0x424d_4133;

/// Extents stored directly in a fork, `count` of them.
pub fn inline_extents(fork: &[u8], count: u64) -> Result<Vec<Extent>> {
    let count = usize::try_from(count).map_err(|_| corrupt("extent count"))?;
    if count.checked_mul(16).map_or(true, |n| n > fork.len()) {
        return Err(corrupt(format!("{count} extents do not fit in the fork")));
    }
    Ok((0..count).map(|i| Extent::decode(&fork[i * 16..])).collect())
}

/// The root of a B+tree held in a fork: its level and child block numbers.
pub fn btree_root(fork: &[u8]) -> Result<(u16, Vec<u64>)> {
    if fork.len() < 4 {
        return Err(corrupt("B+tree root in a tiny fork"));
    }
    let level = be16(fork, 0);
    let numrecs = be16(fork, 2) as usize;
    // Keys fill the first half of the space and pointers the second, each
    // sized for as many records as the fork could hold.
    let maxrecs = (fork.len() - 4) / 16;
    if level == 0 || numrecs == 0 || numrecs > maxrecs {
        return Err(corrupt(format!("B+tree root: level {level}, {numrecs} records")));
    }
    let ptrs = 4 + maxrecs * 8;
    Ok((level, (0..numrecs).map(|i| be64(fork, ptrs + i * 8)).collect()))
}

/// Header length of a long-format B+tree block.
pub fn long_block_header(sb: &Superblock) -> usize {
    if sb.is_v5() {
        72
    } else {
        24
    }
}

/// Check a B+tree block and return its level and record count.
pub fn check_btree_block(sb: &Superblock, buf: &[u8], fsb: u64, owner: u64) -> Result<(u16, usize)> {
    let magic = be32(buf, 0);
    if sb.is_v5() {
        if magic != BMA3_MAGIC {
            return Err(corrupt(format!("bmap block {fsb}: bad magic")));
        }
        if !crc::verify(buf, 64) {
            return Err(corrupt(format!("bmap block {fsb}: checksum")));
        }
        if be64(buf, 56) != owner {
            return Err(corrupt(format!("bmap block {fsb}: owned by {}", be64(buf, 56))));
        }
    } else if magic != BMAP_MAGIC {
        return Err(corrupt(format!("bmap block {fsb}: bad magic")));
    }
    let level = be16(buf, 4);
    let numrecs = be16(buf, 6) as usize;
    let hdr = long_block_header(sb);
    let fit = (buf.len() - hdr) / 16;
    if numrecs > fit {
        return Err(corrupt(format!("bmap block {fsb}: {numrecs} records")));
    }
    Ok((level, numrecs))
}

/// Child pointers of an interior B+tree block.
pub fn node_ptrs(sb: &Superblock, buf: &[u8], numrecs: usize) -> Vec<u64> {
    let hdr = long_block_header(sb);
    let maxrecs = (buf.len() - hdr) / 16;
    let ptrs = hdr + maxrecs * 8;
    (0..numrecs).map(|i| be64(buf, ptrs + i * 8)).collect()
}

/// Records of a leaf B+tree block.
pub fn leaf_extents(sb: &Superblock, buf: &[u8], numrecs: usize) -> Vec<Extent> {
    let hdr = long_block_header(sb);
    (0..numrecs).map(|i| Extent::decode(&buf[hdr + i * 16..])).collect()
}

/// Sort a fork's extents and check they do not overlap.
pub fn finish(mut extents: Vec<Extent>) -> Result<Vec<Extent>> {
    extents.retain(|e| e.count > 0);
    extents.sort_by_key(|e| e.offset);
    for w in extents.windows(2) {
        if w[0].end() > w[1].offset {
            return Err(corrupt(format!(
                "overlapping extents at logical blocks {} and {}",
                w[0].offset, w[1].offset
            )));
        }
    }
    Ok(extents)
}

/// The extent covering logical block `lblk`, if any.
pub fn find(extents: &[Extent], lblk: u64) -> Option<&Extent> {
    let i = extents.partition_point(|e| e.end() <= lblk);
    extents.get(i).filter(|e| e.offset <= lblk)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_packs_the_fields() {
        // offset 5, block 0x123456789, count 7, unwritten.
        let (off, blk, cnt) = (5u64, 0x1_2345_6789u64, 7u64);
        let l0 = 1u64 << 63 | off << 9 | blk >> 43;
        let l1 = (blk & ((1 << 43) - 1)) << 21 | cnt;
        let mut rec = [0u8; 16];
        rec[..8].copy_from_slice(&l0.to_be_bytes());
        rec[8..].copy_from_slice(&l1.to_be_bytes());
        let e = Extent::decode(&rec);
        assert_eq!(e, Extent { offset: off, block: blk, count: cnt, unwritten: true });
    }

    #[test]
    fn find_by_logical_block() {
        let x = |offset, count| Extent { offset, block: 100 + offset, count, unwritten: false };
        let v = vec![x(0, 2), x(4, 3), x(10, 1)];
        assert_eq!(find(&v, 1).unwrap().offset, 0);
        assert!(find(&v, 2).is_none());
        assert_eq!(find(&v, 6).unwrap().offset, 4);
        assert!(find(&v, 7).is_none());
        assert_eq!(find(&v, 10).unwrap().offset, 10);
        assert!(find(&v, 11).is_none());
    }
}
