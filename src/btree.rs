//! Building B+trees whole, from sorted records — as `xfs_repair` rebuilds
//! them in its phase 5, rather than one insert at a time as the kernel does.
//!
//! The writer keeps every record of a tree it changes in memory and writes
//! the tree out afresh at [`crate::Volume::flush`]: free space by block and
//! by size, inode chunks, chunks with free inodes, reverse mappings — and a
//! fork's extents when there are more than its inode can hold. A tree built
//! whole has no splits to get wrong, and its blocks are evenly filled, which
//! is what `xfs_repair` checks a non-root block against (at least half
//! full).

use std::cmp::Ordering;

use crate::bytes::{put16, put32, put64};
use crate::crc;
use crate::sb::Superblock;

/// A short-form (AG) B+tree block's v5 header length.
pub const SHORT_HDR: usize = 56;
/// A long-form (inode fork) B+tree block's v5 header length.
pub const LONG_HDR: usize = 72;
/// No sibling, in a short-form tree.
pub const NULL_AGBLOCK: u32 = u32::MAX;
/// No sibling, in a long-form tree.
pub const NULL_FSBLOCK: u64 = u64::MAX;

/// Record counts for `n` records spread as evenly as possible over `k`
/// blocks; every one gets at least `n / k`.
pub fn spread(n: usize, k: usize) -> Vec<usize> {
    (0..k).map(|i| n / k + usize::from(i < n % k)).collect()
}

/// Blocks at each level, leaves first, for `n` records: always at least one
/// (an empty tree is a root leaf with no records), and the last level is a
/// single root.
pub fn level_sizes(n: usize, leaf_max: usize, node_max: usize) -> Vec<usize> {
    let mut sizes = vec![n.div_ceil(leaf_max).max(1)];
    while *sizes.last().unwrap() > 1 {
        let below = *sizes.last().unwrap();
        sizes.push(below.div_ceil(node_max));
    }
    sizes
}

/// Blocks a short-form tree of `n` records takes.
pub fn blocks_for(n: usize, leaf_max: usize, node_max: usize) -> usize {
    level_sizes(n, leaf_max, node_max).iter().sum()
}

/// One record of an AG B+tree, with its keys.
#[derive(Debug, Clone)]
pub struct Rec {
    /// The record as stored.
    pub data: Vec<u8>,
    /// Its key.
    pub low: Vec<u8>,
    /// Its high key: the last thing it covers. Only overlapping trees (the
    /// reverse-mapping one) store it.
    pub high: Vec<u8>,
}

/// What kind of AG B+tree.
pub struct AgSpec {
    /// The block magic.
    pub magic: u32,
    /// Record length.
    pub rec_len: usize,
    /// Key length (one key; an overlapping tree stores two per entry).
    pub key_len: usize,
    /// Whether node entries carry a high key as well as a low one.
    pub overlapping: bool,
    /// Key order, for picking the highest high key.
    pub cmp: fn(&[u8], &[u8]) -> Ordering,
}

impl AgSpec {
    /// Records in a leaf.
    pub fn leaf_max(&self, block_size: usize) -> usize {
        (block_size - SHORT_HDR) / self.rec_len
    }

    /// Entries in a node.
    pub fn node_max(&self, block_size: usize) -> usize {
        (block_size - SHORT_HDR) / (self.key_len * self.slots() + 4)
    }

    fn slots(&self) -> usize {
        if self.overlapping {
            2
        } else {
            1
        }
    }

    /// Blocks a tree of `n` records takes.
    pub fn blocks(&self, n: usize, block_size: usize) -> usize {
        blocks_for(n, self.leaf_max(block_size), self.node_max(block_size))
    }
}

/// A tree laid out in blocks.
pub struct Built<B> {
    /// Each block's address and contents.
    pub blocks: Vec<(B, Vec<u8>)>,
    /// The root block (for a long-form tree, the root's entries are in
    /// [`Built::root_entries`] instead).
    pub root: B,
    /// Levels, leaves included.
    pub levels: u32,
    /// Long-form trees: the inode root's (key, pointer) entries.
    pub root_entries: Vec<(u64, u64)>,
}

/// Build an AG B+tree over `recs`, which must be sorted, in the blocks
/// `agbnos` (exactly [`AgSpec::blocks`] of them).
pub fn build_ag(sb: &Superblock, agno: u32, spec: &AgSpec, recs: &[Rec], agbnos: &[u32]) -> Built<u32> {
    let bs = sb.block_size as usize;
    let sizes = level_sizes(recs.len(), spec.leaf_max(bs), spec.node_max(bs));
    assert_eq!(sizes.iter().sum::<usize>(), agbnos.len(), "AG B+tree block count");
    let node_max = spec.node_max(bs);
    let mut next = agbnos.iter().copied();
    let mut blocks = Vec::new();

    // (low key, high key, block) of each block at the level below.
    let mut below: Vec<(Vec<u8>, Vec<u8>, u32)> = Vec::new();
    for (level, &count) in sizes.iter().enumerate() {
        let addrs: Vec<u32> = (&mut next).take(count).collect();
        let n_items = if level == 0 { recs.len() } else { below.len() };
        let mut at = 0;
        let mut built = Vec::with_capacity(count);
        for (i, take) in spread(n_items, count).into_iter().enumerate() {
            let mut buf = vec![0u8; bs];
            put32(&mut buf, 0, spec.magic);
            put16(&mut buf, 4, level as u16);
            put16(&mut buf, 6, take as u16);
            put32(&mut buf, 8, if i == 0 { NULL_AGBLOCK } else { addrs[i - 1] });
            put32(&mut buf, 12, addrs.get(i + 1).copied().unwrap_or(NULL_AGBLOCK));
            put64(&mut buf, 16, sb.agb_to_byte(agno, addrs[i]) >> 9);
            buf[32..48].copy_from_slice(&sb.meta_uuid);
            put32(&mut buf, 48, agno);
            let (low, high) = if level == 0 {
                let part = &recs[at..at + take];
                for (j, r) in part.iter().enumerate() {
                    buf[SHORT_HDR + j * spec.rec_len..][..spec.rec_len].copy_from_slice(&r.data);
                }
                keys_of(spec, part.iter().map(|r| (&r.low, &r.high)))
            } else {
                let part = &below[at..at + take];
                let slot = spec.key_len * spec.slots();
                let ptrs = SHORT_HDR + node_max * slot;
                for (j, (lo, hi, ptr)) in part.iter().enumerate() {
                    let k = SHORT_HDR + j * slot;
                    buf[k..k + spec.key_len].copy_from_slice(lo);
                    if spec.overlapping {
                        buf[k + spec.key_len..k + slot].copy_from_slice(hi);
                    }
                    put32(&mut buf, ptrs + j * 4, *ptr);
                }
                keys_of(spec, part.iter().map(|(l, h, _)| (l, h)))
            };
            crc::stamp(&mut buf, 52);
            built.push((low, high, addrs[i]));
            blocks.push((addrs[i], buf));
            at += take;
        }
        below = built;
    }
    Built { root: below[0].2, levels: sizes.len() as u32, blocks, root_entries: Vec::new() }
}

/// The low key of the first entry and the highest high key.
fn keys_of<'a>(spec: &AgSpec, mut it: impl Iterator<Item = (&'a Vec<u8>, &'a Vec<u8>)>) -> (Vec<u8>, Vec<u8>) {
    let Some((low, high)) = it.next() else {
        return (vec![0; spec.key_len], vec![0; spec.key_len]);
    };
    let mut best = high.clone();
    for (_, h) in it {
        if (spec.cmp)(h, &best) == Ordering::Greater {
            best = h.clone();
        }
    }
    (low.clone(), best)
}

/// `BMA3`.
pub const BMAP_MAGIC: u32 = 0x424d_4133;

/// Records in a bmap B+tree block.
pub fn bmap_block_max(block_size: usize) -> usize {
    (block_size - LONG_HDR) / 16
}

/// Entries an inode fork of `fork_size` bytes holds as a B+tree root.
pub fn bmap_root_max(fork_size: usize) -> usize {
    (fork_size - 4) / 16
}

/// Blocks a bmap B+tree of `n` extents takes under a root of `root_max`
/// entries held in the inode.
pub fn bmap_blocks(n: usize, block_size: usize, root_max: usize) -> usize {
    bmap_level_sizes(n, block_size, root_max).iter().sum()
}

fn bmap_level_sizes(n: usize, block_size: usize, root_max: usize) -> Vec<usize> {
    let max = bmap_block_max(block_size);
    let mut sizes = vec![n.div_ceil(max).max(1)];
    while *sizes.last().unwrap() > root_max {
        let below = *sizes.last().unwrap();
        sizes.push(below.div_ceil(max));
    }
    sizes
}

/// Build a bmap B+tree over packed extent records (`recs`, 16 bytes each,
/// sorted, with their start offsets in `offsets`) in the blocks `fsbs`
/// (exactly [`bmap_blocks`] of them), owned by inode `ino`. The root's
/// entries come back in [`Built::root_entries`], for the inode.
pub fn build_bmap(sb: &Superblock, ino: u64, recs: &[[u8; 16]], offsets: &[u64], fsbs: &[u64], root_max: usize) -> crate::Result<Built<u64>> {
    let bs = sb.block_size as usize;
    let sizes = bmap_level_sizes(recs.len(), bs, root_max);
    assert_eq!(sizes.iter().sum::<usize>(), fsbs.len(), "bmap B+tree block count");
    let max = bmap_block_max(bs);
    let mut next = fsbs.iter().copied();
    let mut blocks = Vec::new();
    let mut below: Vec<(u64, u64)> = Vec::new();
    for (level, &count) in sizes.iter().enumerate() {
        let addrs: Vec<u64> = (&mut next).take(count).collect();
        let n_items = if level == 0 { recs.len() } else { below.len() };
        let mut at = 0;
        let mut built = Vec::with_capacity(count);
        for (i, take) in spread(n_items, count).into_iter().enumerate() {
            let mut buf = vec![0u8; bs];
            put32(&mut buf, 0, BMAP_MAGIC);
            put16(&mut buf, 4, level as u16);
            put16(&mut buf, 6, take as u16);
            put64(&mut buf, 8, if i == 0 { NULL_FSBLOCK } else { addrs[i - 1] });
            put64(&mut buf, 16, addrs.get(i + 1).copied().unwrap_or(NULL_FSBLOCK));
            put64(&mut buf, 24, sb.fsb_to_byte(addrs[i])? >> 9);
            buf[40..56].copy_from_slice(&sb.meta_uuid);
            put64(&mut buf, 56, ino);
            let first_key;
            if level == 0 {
                for j in 0..take {
                    buf[LONG_HDR + j * 16..][..16].copy_from_slice(&recs[at + j]);
                }
                first_key = offsets[at];
            } else {
                let part = &below[at..at + take];
                for (j, (key, ptr)) in part.iter().enumerate() {
                    put64(&mut buf, LONG_HDR + j * 8, *key);
                    put64(&mut buf, LONG_HDR + max * 8 + j * 8, *ptr);
                }
                first_key = part[0].0;
            }
            crc::stamp(&mut buf, 64);
            built.push((first_key, addrs[i]));
            blocks.push((addrs[i], buf));
            at += take;
        }
        below = built;
    }
    Ok(Built { root: 0, levels: sizes.len() as u32 + 1, blocks, root_entries: below })
}

/// The bytes of an inode fork holding a bmap B+tree root.
pub fn bmap_root_fork(fork_size: usize, level: u16, entries: &[(u64, u64)]) -> Vec<u8> {
    let max = bmap_root_max(fork_size);
    let mut f = vec![0u8; fork_size];
    put16(&mut f, 0, level);
    put16(&mut f, 2, entries.len() as u16);
    for (j, (key, ptr)) in entries.iter().enumerate() {
        put64(&mut f, 4 + j * 8, *key);
        put64(&mut f, 4 + max * 8 + j * 8, *ptr);
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spread_is_even() {
        assert_eq!(spread(10, 3), vec![4, 3, 3]);
        assert_eq!(spread(0, 1), vec![0]);
        // Just over one block's worth: both at least half full.
        let s = spread(101, 2);
        assert!(s.iter().all(|&n| n >= 50));
    }

    #[test]
    fn levels() {
        assert_eq!(level_sizes(0, 10, 5), vec![1]);
        assert_eq!(level_sizes(10, 10, 5), vec![1]);
        assert_eq!(level_sizes(11, 10, 5), vec![2, 1]);
        assert_eq!(level_sizes(60, 10, 5), vec![6, 2, 1]);
        assert_eq!(blocks_for(60, 10, 5), 9);
    }

    #[test]
    fn bmap_levels_stop_at_the_inode_root() {
        // 4 KiB blocks hold 251 extents; a 336-byte fork's root holds 20.
        assert_eq!(bmap_level_sizes(30, 4096, 20), vec![1]);
        assert_eq!(bmap_level_sizes(251 * 21, 4096, 20), vec![21, 1]);
    }
}
