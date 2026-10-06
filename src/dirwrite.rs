//! Writing directories: a whole directory from its list of names, in the
//! smallest form that holds it.
//!
//! The writer keeps a changed directory as a list of names and lays it out
//! once, at [`crate::Volume::flush`] — short form in the inode while it fits,
//! then a single block (names and hash index together), then data blocks
//! with one leaf block of index and best-free table, then data blocks under
//! a hash B+tree of leaf and node blocks with separate free-index blocks.
//! The same four forms the kernel moves a directory through as it grows;
//! building one from scratch needs none of the conversions.

use crate::bytes::{put16, put32, put64};
use crate::dir::LEAF_OFFSET;
use crate::sb::Superblock;

/// Byte offset in a directory's logical space of the free-index blocks.
pub const FREE_OFFSET: u64 = 2 * LEAF_OFFSET;

const XDB3: u32 = 0x5844_4233;
const XDD3: u32 = 0x5844_4433;
const XDF3: u32 = 0x5844_4633;
const LEAF1: u16 = 0x3df1;
const LEAFN: u16 = 0x3dff;
const NODE: u16 = 0x3ebe;

/// Header length of every v5 directory block.
const HDR: usize = 64;

/// One name to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEnt {
    /// The name.
    pub name: Vec<u8>,
    /// Its inode.
    pub ino: u64,
    /// Its `ftype` code (1 file, 2 directory, … 7 symlink).
    pub ftype: u8,
}

/// One directory block, ready but for its disk address and checksum.
#[derive(Debug, Clone)]
pub struct DirBlock {
    /// Logical file block it goes at.
    pub lblk: u64,
    /// Contents, one directory block long.
    pub data: Vec<u8>,
    /// Where its checksum goes.
    pub crc_off: usize,
    /// Where its own disk address (in 512-byte units) goes.
    pub blkno_off: usize,
}

/// A directory laid out.
#[derive(Debug, Clone)]
pub enum Layout {
    /// Short form: the bytes of the inode's data fork.
    Short(Vec<u8>),
    /// Blocks, and the directory's size (the end of its data blocks).
    Blocks {
        /// `di_size`.
        size: u64,
        /// Every block.
        blocks: Vec<DirBlock>,
    },
}

/// XFS's directory name hash (`xfs_da_hashname`).
pub fn hashname(name: &[u8]) -> u32 {
    let mut hash = 0u32;
    let mut n = name;
    while n.len() >= 4 {
        hash = (n[0] as u32) << 21 ^ (n[1] as u32) << 14 ^ (n[2] as u32) << 7 ^ n[3] as u32 ^ hash.rotate_left(7 * 4);
        n = &n[4..];
    }
    match n.len() {
        3 => (n[0] as u32) << 14 ^ (n[1] as u32) << 7 ^ n[2] as u32 ^ hash.rotate_left(7 * 3),
        2 => (n[0] as u32) << 7 ^ n[1] as u32 ^ hash.rotate_left(7 * 2),
        1 => n[0] as u32 ^ hash.rotate_left(7),
        _ => hash,
    }
}

/// Bytes a data entry takes: inode, length, name, type, tag, 8-aligned.
pub fn entsize(namelen: usize) -> usize {
    (8 + 1 + namelen + 1 + 2 + 7) & !7
}

/// Inode numbers above this need 8 bytes in a short-form directory.
const MAX_SHORT_INUM: u64 = u32::MAX as u64;

/// Lay out a directory: inode `ino`, parent `parent`, the names (without
/// `.` and `..`), and a data fork of `fork_size` bytes in the inode.
pub fn layout(sb: &Superblock, ino: u64, parent: u64, names: &[DirEnt], fork_size: usize) -> Layout {
    if let Some(sf) = shortform(parent, names, fork_size) {
        return Layout::Short(sf);
    }
    let dbs = sb.dir_block_size() as usize;
    let fsb_per = 1u64 << sb.dirblk_log;
    let leaf_first = LEAF_OFFSET >> sb.block_log;
    let free_first = FREE_OFFSET >> sb.block_log;

    let mut all = Vec::with_capacity(names.len() + 2);
    all.push(DirEnt { name: b".".to_vec(), ino, ftype: 2 });
    all.push(DirEnt { name: b"..".to_vec(), ino: parent, ftype: 2 });
    all.extend_from_slice(names);
    let used: usize = all.iter().map(|e| entsize(e.name.len())).sum();

    // A single block: names, then the hash index and a tail at the end.
    if HDR + used + all.len() * 8 + 8 <= dbs {
        let mut buf = vec![0u8; dbs];
        put32(&mut buf, 0, XDB3);
        let mut leaf = Vec::with_capacity(all.len());
        let mut p = HDR;
        for e in &all {
            leaf.push((hashname(&e.name), (p >> 3) as u32));
            p += put_entry(&mut buf, p, e);
        }
        let leaf_at = dbs - 8 - all.len() * 8;
        put_free(&mut buf, p, leaf_at - p);
        leaf.sort();
        for (i, (h, a)) in leaf.iter().enumerate() {
            put32(&mut buf, leaf_at + i * 8, *h);
            put32(&mut buf, leaf_at + i * 8 + 4, *a);
        }
        put32(&mut buf, dbs - 8, all.len() as u32);
        put32(&mut buf, dbs - 4, 0);
        stamp_header(sb, &mut buf, ino);
        return Layout::Blocks {
            size: dbs as u64,
            blocks: vec![DirBlock { lblk: 0, data: buf, crc_off: 4, blkno_off: 8 }],
        };
    }

    // Data blocks, packed in order.
    let mut blocks = Vec::new();
    let mut leaf: Vec<(u32, u32)> = Vec::with_capacity(all.len());
    let mut bests: Vec<u16> = Vec::new();
    let mut it = all.iter().peekable();
    while it.peek().is_some() {
        let db = bests.len() as u64;
        let mut buf = vec![0u8; dbs];
        put32(&mut buf, 0, XDD3);
        let mut p = HDR;
        while let Some(e) = it.peek() {
            if p + entsize(e.name.len()) > dbs {
                break;
            }
            leaf.push((hashname(&e.name), ((db * dbs as u64 + p as u64) >> 3) as u32));
            p += put_entry(&mut buf, p, e);
            it.next();
        }
        bests.push(put_free(&mut buf, p, dbs - p));
        stamp_header(sb, &mut buf, ino);
        blocks.push(DirBlock { lblk: db * fsb_per, data: buf, crc_off: 4, blkno_off: 8 });
    }
    let ndata = bests.len();
    leaf.sort();

    let da_info = |buf: &mut Vec<u8>, magic: u16, forw: u32, back: u32| {
        put32(buf, 0, forw);
        put32(buf, 4, back);
        put16(buf, 8, magic);
        buf[32..48].copy_from_slice(&sb.meta_uuid);
        put64(buf, 48, ino);
    };
    let put_leaf_ents = |buf: &mut Vec<u8>, ents: &[(u32, u32)]| {
        put16(buf, 56, ents.len() as u16);
        for (i, (h, a)) in ents.iter().enumerate() {
            put32(buf, HDR + i * 8, *h);
            put32(buf, HDR + i * 8 + 4, *a);
        }
    };

    // One leaf block: the whole index, and each data block's best free.
    if HDR + leaf.len() * 8 + ndata * 2 + 4 <= dbs {
        let mut buf = vec![0u8; dbs];
        da_info(&mut buf, LEAF1, 0, 0);
        put_leaf_ents(&mut buf, &leaf);
        let bests_at = dbs - 4 - ndata * 2;
        for (i, b) in bests.iter().enumerate() {
            put16(&mut buf, bests_at + i * 2, *b);
        }
        put32(&mut buf, dbs - 4, ndata as u32);
        blocks.push(DirBlock { lblk: leaf_first, data: buf, crc_off: 12, blkno_off: 16 });
        return Layout::Blocks { size: (ndata * dbs) as u64, blocks };
    }

    // Node form. Leaf blocks hold the index; above them, node blocks; the
    // root always sits at the first leaf position.
    let per_leaf = (dbs - HDR) / 8;
    let per_node = (dbs - HDR) / 8;
    let nleaf = leaf.len().div_ceil(per_leaf);
    let mut sizes = vec![nleaf];
    while *sizes.last().unwrap() > 1 {
        sizes.push(sizes.last().unwrap().div_ceil(per_node));
    }
    // Give out positions: the root first, then everything else in order.
    let total: usize = sizes.iter().sum();
    let mut pos = Vec::with_capacity(total);
    pos.push(leaf_first);
    pos.extend((1..total as u64).map(|i| leaf_first + i * fsb_per));
    // Level by level, leaves first; the root (the last block made) gets
    // position 0.
    let mut order: Vec<u64> = pos[1..].to_vec();
    order.push(pos[0]);
    let mut next = order.into_iter();
    let mut below: Vec<(u32, u64)> = Vec::new(); // (highest hash, position)
    for (level, &count) in sizes.iter().enumerate() {
        let addrs: Vec<u64> = (&mut next).take(count).collect();
        let n_items = if level == 0 { leaf.len() } else { below.len() };
        let mut at = 0;
        let mut built = Vec::with_capacity(count);
        for (i, take) in crate::btree::spread(n_items, count).into_iter().enumerate() {
            let mut buf = vec![0u8; dbs];
            let back = if i == 0 { 0 } else { addrs[i - 1] as u32 };
            let forw = addrs.get(i + 1).map_or(0, |&a| a as u32);
            let high;
            if level == 0 {
                da_info(&mut buf, LEAFN, forw, back);
                let part = &leaf[at..at + take];
                put_leaf_ents(&mut buf, part);
                high = part.last().map_or(0, |e| e.0);
            } else {
                da_info(&mut buf, NODE, forw, back);
                let part = &below[at..at + take];
                put16(&mut buf, 56, take as u16);
                put16(&mut buf, 58, level as u16);
                for (j, (h, before)) in part.iter().enumerate() {
                    put32(&mut buf, HDR + j * 8, *h);
                    put32(&mut buf, HDR + j * 8 + 4, *before as u32);
                }
                high = part.last().map_or(0, |e| e.0);
            }
            built.push((high, addrs[i]));
            blocks.push(DirBlock { lblk: addrs[i], data: buf, crc_off: 12, blkno_off: 16 });
            at += take;
        }
        below = built;
    }

    // Free-index blocks: each data block's best free, in fixed-size runs.
    let per_free = (dbs - HDR) / 2;
    for (i, chunk) in bests.chunks(per_free).enumerate() {
        let mut buf = vec![0u8; dbs];
        put32(&mut buf, 0, XDF3);
        buf[24..40].copy_from_slice(&sb.meta_uuid);
        put64(&mut buf, 40, ino);
        put32(&mut buf, 48, (i * per_free) as u32);
        put32(&mut buf, 52, chunk.len() as u32);
        put32(&mut buf, 56, chunk.len() as u32);
        for (j, b) in chunk.iter().enumerate() {
            put16(&mut buf, HDR + j * 2, *b);
        }
        blocks.push(DirBlock { lblk: free_first + i as u64 * fsb_per, data: buf, crc_off: 4, blkno_off: 8 });
    }
    blocks.sort_by_key(|b| b.lblk);
    Layout::Blocks { size: (ndata * dbs) as u64, blocks }
}

/// The short form, if it fits in `fork_size` bytes.
fn shortform(parent: u64, names: &[DirEnt], fork_size: usize) -> Option<Vec<u8>> {
    if names.len() > 255 {
        return None;
    }
    let i8count = (parent > MAX_SHORT_INUM) as usize + names.iter().filter(|e| e.ino > MAX_SHORT_INUM).count();
    let isz = if i8count > 0 { 8 } else { 4 };
    let size = 2 + isz + names.iter().map(|e| 1 + 2 + e.name.len() + 1 + isz).sum::<usize>();
    if size > fork_size {
        return None;
    }
    let mut buf = Vec::with_capacity(size);
    buf.push(names.len() as u8);
    buf.push(i8count as u8);
    let put_ino = |buf: &mut Vec<u8>, ino: u64| {
        if isz == 8 {
            buf.extend_from_slice(&ino.to_be_bytes());
        } else {
            buf.extend_from_slice(&(ino as u32).to_be_bytes());
        }
    };
    put_ino(&mut buf, parent);
    // Each name's offset is where it would sit in a single-block directory:
    // after the header, `.` and `..`.
    let mut offset = HDR + entsize(1) + entsize(2);
    for e in names {
        buf.push(e.name.len() as u8);
        buf.extend_from_slice(&(offset as u16).to_be_bytes());
        buf.extend_from_slice(&e.name);
        buf.push(e.ftype);
        put_ino(&mut buf, e.ino);
        offset += entsize(e.name.len());
    }
    Some(buf)
}

/// Write a data entry at `p`; returns its length.
fn put_entry(buf: &mut [u8], p: usize, e: &DirEnt) -> usize {
    let len = entsize(e.name.len());
    put64(buf, p, e.ino);
    buf[p + 8] = e.name.len() as u8;
    buf[p + 9..p + 9 + e.name.len()].copy_from_slice(&e.name);
    buf[p + 9 + e.name.len()] = e.ftype;
    put16(buf, p + len - 2, p as u16);
    len
}

/// Mark `len` bytes at `p` unused and make them the block's best free
/// region; returns `len` (0 for none).
fn put_free(buf: &mut [u8], p: usize, len: usize) -> u16 {
    if len > 0 {
        put16(buf, p, 0xffff);
        put16(buf, p + 2, len as u16);
        put16(buf, p + len - 2, p as u16);
        put16(buf, 48, p as u16);
        put16(buf, 50, len as u16);
    }
    len as u16
}

/// The UUID and owner of a data or block header.
fn stamp_header(sb: &Superblock, buf: &mut [u8], ino: u64) {
    buf[24..40].copy_from_slice(&sb.meta_uuid);
    put64(buf, 40, ino);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_matches_xfs() {
        // Worked by hand; tests/write.rs checks longer names against
        // xfs_db's `hash` command.
        assert_eq!(hashname(b"."), 0x2e);
        assert_eq!(hashname(b".."), 0x172e);
        assert_eq!(hashname(b"abcd"), (0x61 << 21) ^ (0x62 << 14) ^ (0x63 << 7) ^ 0x64);
    }

    #[test]
    fn entry_sizes() {
        assert_eq!(entsize(1), 16);
        assert_eq!(entsize(2), 16);
        assert_eq!(entsize(3), 16);
        assert_eq!(entsize(4), 16);
        assert_eq!(entsize(5), 24);
    }

    #[test]
    fn shortform_offsets_follow_block_layout() {
        let names = vec![
            DirEnt { name: b"a".to_vec(), ino: 131, ftype: 1 },
            DirEnt { name: b"bcdef".to_vec(), ino: 132, ftype: 2 },
        ];
        let sf = shortform(128, &names, 336).unwrap();
        assert_eq!(sf[0], 2);
        assert_eq!(sf[1], 0);
        // First name at offset 96, the second 16 bytes on.
        assert_eq!(u16::from_be_bytes([sf[7], sf[8]]), 96);
        let second = 6 + 1 + 2 + 1 + 1 + 4;
        assert_eq!(u16::from_be_bytes([sf[second + 1], sf[second + 2]]), 112);
        assert!(shortform(128, &names, 20).is_none());
    }
}
