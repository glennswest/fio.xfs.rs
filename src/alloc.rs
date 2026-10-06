//! Allocation groups, held in memory while they are written.
//!
//! The first write that touches an AG reads everything a writer has to keep
//! true about it: its free space (from the by-block B+tree), its inode
//! chunks, its reverse mappings (who owns every block), and the address of
//! every block of those trees. Extents and inode chunks are then taken from
//! and given back to the in-memory copy, and [`Ag::rebuild`] writes the trees
//! out afresh — with the AGF and AGI that point at them — when the volume is
//! flushed.
//!
//! The refcount B+tree is never changed: nothing here shares an extent, and
//! a file whose extents are shared is not rewritten (the caller checks).

use std::collections::{BTreeMap, BTreeSet};

use crate::btree::{self, AgSpec, Rec, NULL_AGBLOCK};
use crate::bytes::{be16, be32, be64, put32};
use crate::crc;
use crate::device::BlockDevice;
use crate::error::{corrupt, Error, Result};
use crate::sb::Superblock;

/// Owners of blocks that belong to no inode (`XFS_RMAP_OWN_*`).
pub mod owner {
    /// The AG headers.
    pub const FS: u64 = -3i64 as u64;
    /// The internal log.
    pub const LOG: u64 = -4i64 as u64;
    /// Free-space and reverse-mapping B+tree blocks, and the AGFL.
    pub const AG: u64 = -5i64 as u64;
    /// Inode B+tree blocks.
    pub const INOBT: u64 = -6i64 as u64;
    /// Inode chunks.
    pub const INODES: u64 = -7i64 as u64;
}

/// Reverse-mapping offset flags.
pub mod rmapf {
    /// The block is in the attribute fork.
    pub const ATTR: u64 = 1 << 63;
    /// The block is a bmap B+tree block.
    pub const BMBT: u64 = 1 << 62;
    /// The extent is unwritten.
    pub const UNWRITTEN: u64 = 1 << 61;
    /// The offset bits.
    pub const OFF_MASK: u64 = (1 << 54) - 1;
}

const AGF_MAGIC: u32 = 0x5841_4746;
const AGI_MAGIC: u32 = 0x5841_4749;
const AGFL_MAGIC: u32 = 0x5841_464c;
const AGF_CRC: usize = 216;
const AGI_CRC: usize = 312;
const AGFL_CRC: usize = 32;

const ABTB: u32 = 0x4142_3342; // AB3B
const ABTC: u32 = 0x4142_3343; // AB3C
const IBT3: u32 = 0x4941_4233; // IAB3
const FIB3: u32 = 0x4649_4233; // FIB3
const RMB3: u32 = 0x524d_4233; // RMB3

/// Inodes in a chunk.
pub const CHUNK_INODES: u32 = 64;

/// The longest extent a bmap record can describe.
pub const MAX_EXTENT: u32 = (1 << 21) - 1;

/// One reverse mapping: `len` blocks from `start` belong to `owner`, at
/// `offset` (with its flags) in the owner's fork when the owner is an inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rmap {
    /// First AG block.
    pub start: u32,
    /// Owner: an inode number or one of [`owner`].
    pub owner: u64,
    /// Offset in the owner's fork, with [`rmapf`] flags.
    pub offset: u64,
    /// Blocks.
    pub len: u32,
}

impl Rmap {
    fn end(&self) -> u32 {
        self.start + self.len
    }

    fn inode_owned(&self) -> bool {
        self.owner >> 63 == 0
    }

    /// Whether `next` continues this record, so the kernel (and
    /// `xfs_repair`) would keep them as one.
    fn merges_with(&self, next: &Rmap) -> bool {
        if self.owner != next.owner || self.end() != next.start || self.len as u64 + next.len as u64 > u32::MAX as u64 {
            return false;
        }
        if !self.inode_owned() {
            return true;
        }
        let flags = |r: &Rmap| r.offset & !rmapf::OFF_MASK;
        if flags(self) != flags(next) {
            return false;
        }
        if self.offset & rmapf::BMBT != 0 {
            return true;
        }
        (self.offset & rmapf::OFF_MASK) + self.len as u64 == next.offset & rmapf::OFF_MASK
    }

    fn encode(&self) -> Rec {
        let mut data = vec![0u8; 24];
        put32(&mut data, 0, self.start);
        put32(&mut data, 4, self.len);
        data[8..16].copy_from_slice(&self.owner.to_be_bytes());
        data[16..24].copy_from_slice(&self.offset.to_be_bytes());
        let key_off = self.offset & !rmapf::UNWRITTEN;
        let mut low = vec![0u8; 20];
        put32(&mut low, 0, self.start);
        low[4..12].copy_from_slice(&self.owner.to_be_bytes());
        low[12..20].copy_from_slice(&key_off.to_be_bytes());
        let adj = self.len - 1;
        let mut high = low.clone();
        put32(&mut high, 0, self.start + adj);
        if self.inode_owned() && self.offset & rmapf::BMBT == 0 {
            let off = ((key_off & rmapf::OFF_MASK) + adj as u64) | (key_off & !rmapf::OFF_MASK);
            high[12..20].copy_from_slice(&off.to_be_bytes());
        }
        Rec { data, low, high }
    }
}

fn rmap_key_cmp(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let k = |x: &[u8]| (be32(x, 0), be64(x, 4), be64(x, 12) & rmapf::OFF_MASK);
    k(a).cmp(&k(b))
}

fn bytes_cmp(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    a.cmp(b)
}

const BNO: AgSpec = AgSpec { magic: ABTB, rec_len: 8, key_len: 8, overlapping: false, cmp: bytes_cmp };
const CNT: AgSpec = AgSpec { magic: ABTC, rec_len: 8, key_len: 8, overlapping: false, cmp: bytes_cmp };
const INOBT: AgSpec = AgSpec { magic: IBT3, rec_len: 16, key_len: 4, overlapping: false, cmp: bytes_cmp };
const FINOBT: AgSpec = AgSpec { magic: FIB3, rec_len: 16, key_len: 4, overlapping: false, cmp: bytes_cmp };
const RMAP: AgSpec = AgSpec { magic: RMB3, rec_len: 24, key_len: 20, overlapping: true, cmp: rmap_key_cmp };

/// An inode chunk: 64 inodes from its start, some perhaps not there
/// (sparse), some free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    /// Bit `i` set: inodes `4i..4i+4` are not allocated on disk.
    pub holemask: u16,
    /// Inodes present.
    pub count: u8,
    /// Bit `i` set: inode `i` is free (holes are marked free too).
    pub free: u64,
}

impl Chunk {
    fn holes(&self) -> u64 {
        let mut m = 0u64;
        for i in 0..16 {
            if self.holemask & (1 << i) != 0 {
                m |= 0xf << (i * 4);
            }
        }
        m
    }

    /// Free inodes that are really there.
    pub fn free_count(&self) -> u32 {
        (self.free & !self.holes()).count_ones()
    }

    fn first_free(&self) -> Option<u32> {
        let usable = self.free & !self.holes();
        (usable != 0).then(|| usable.trailing_zeros())
    }

    fn encode(&self, sb: &Superblock, startino: u32) -> Rec {
        let mut data = vec![0u8; 16];
        put32(&mut data, 0, startino);
        if sb.has_sparse_inodes() {
            data[4..6].copy_from_slice(&self.holemask.to_be_bytes());
            data[6] = self.count;
            data[7] = self.free_count() as u8;
        } else {
            put32(&mut data, 4, self.free_count());
        }
        data[8..16].copy_from_slice(&self.free.to_be_bytes());
        let low = data[..4].to_vec();
        Rec { data, high: low.clone(), low }
    }
}

/// An allocation group being written.
pub struct Ag {
    /// Its number.
    pub agno: u32,
    /// Blocks in it.
    pub len: u32,
    agf: Vec<u8>,
    agi: Vec<u8>,
    /// Free extents: start → length.
    pub free: BTreeMap<u32, u32>,
    /// The AGFL's blocks, which are neither free here nor in a tree.
    agfl: Vec<u32>,
    /// Every block of the trees as they were read: free when rebuilt.
    old_tree_blocks: Vec<u32>,
    /// Inode chunks by first AG inode.
    pub chunks: BTreeMap<u32, Chunk>,
    /// Reverse mappings, when the filesystem keeps them.
    rmap: BTreeSet<Rmap>,
    /// Whether anything changed.
    pub dirty: bool,
}

/// Every record and block of an AG B+tree.
async fn walk_tree<D: BlockDevice + ?Sized>(
    dev: &D,
    sb: &Superblock,
    agno: u32,
    spec: &AgSpec,
    root: u32,
    levels: u32,
) -> Result<(Vec<Vec<u8>>, Vec<u32>)> {
    let bs = sb.block_size as usize;
    let name = format!("AG {agno} tree {:#x}", spec.magic);
    let mut recs = Vec::new();
    let mut blocks = Vec::new();
    if levels == 0 || levels > 9 {
        return Err(corrupt(format!("{name}: {levels} levels")));
    }
    let mut stack = vec![(root, levels - 1)];
    while let Some((agbno, want)) = stack.pop() {
        if agbno >= sb.ag_len(agno) || blocks.len() > 1 << 24 {
            return Err(corrupt(format!("{name}: block {agbno}")));
        }
        blocks.push(agbno);
        let mut buf = vec![0u8; bs];
        dev.read_at(sb.agb_to_byte(agno, agbno), &mut buf).await?;
        if be32(&buf, 0) != spec.magic || !crc::verify(&buf, 52) {
            return Err(corrupt(format!("{name}: block {agbno} magic or checksum")));
        }
        let level = be16(&buf, 4) as u32;
        let n = be16(&buf, 6) as usize;
        if level != want {
            return Err(corrupt(format!("{name}: block {agbno} at level {level}, expected {want}")));
        }
        if level == 0 {
            if n > spec.leaf_max(bs) {
                return Err(corrupt(format!("{name}: block {agbno} holds {n} records")));
            }
            recs.extend((0..n).map(|i| buf[btree::SHORT_HDR + i * spec.rec_len..][..spec.rec_len].to_vec()));
        } else {
            let max = spec.node_max(bs);
            if n > max {
                return Err(corrupt(format!("{name}: block {agbno} holds {n} entries")));
            }
            let slot = spec.key_len * if spec.overlapping { 2 } else { 1 };
            let ptrs = btree::SHORT_HDR + max * slot;
            // Pushed in reverse so the leftmost child is read first and the
            // records come out in order.
            for i in (0..n).rev() {
                stack.push((be32(&buf, ptrs + i * 4), level - 1));
            }
        }
    }
    Ok((recs, blocks))
}

impl Ag {
    /// Byte offset of the AG's `n`th header sector.
    fn header_at(sb: &Superblock, agno: u32, n: u64) -> u64 {
        sb.agb_to_byte(agno, 0) + n * sb.sector_size as u64
    }

    /// Read an AG's headers: its AGF and AGI, checked.
    pub async fn read_headers<D: BlockDevice + ?Sized>(dev: &D, sb: &Superblock, agno: u32) -> Result<(Vec<u8>, Vec<u8>)> {
        let mut agf = vec![0u8; sb.sect()];
        let mut agi = vec![0u8; sb.sect()];
        dev.read_at(Self::header_at(sb, agno, 1), &mut agf).await?;
        dev.read_at(Self::header_at(sb, agno, 2), &mut agi).await?;
        if be32(&agf, 0) != AGF_MAGIC || !crc::verify(&agf, AGF_CRC) || be32(&agf, 8) != agno {
            return Err(corrupt(format!("AG {agno}: AGF")));
        }
        if be32(&agi, 0) != AGI_MAGIC || !crc::verify(&agi, AGI_CRC) || be32(&agi, 8) != agno {
            return Err(corrupt(format!("AG {agno}: AGI")));
        }
        Ok((agf, agi))
    }

    /// Free blocks the AGF accounts for, as the superblock counts them:
    /// free extents, the AGFL, and the free-space trees' blocks beyond their
    /// roots.
    pub fn agf_free_blocks(agf: &[u8]) -> u64 {
        be32(agf, 52) as u64 + be32(agf, 48) as u64 + be32(agf, 60) as u64
    }

    /// Inodes allocated and free, as the AGI counts them.
    pub fn agi_inodes(agi: &[u8]) -> (u64, u64) {
        (be32(agi, 16) as u64, be32(agi, 28) as u64)
    }

    /// The AGF as it stands (rebuilt, after [`Ag::rebuild`]).
    pub fn agf(&self) -> &[u8] {
        &self.agf
    }

    /// The AGI as it stands.
    pub fn agi(&self) -> &[u8] {
        &self.agi
    }

    /// Read everything about AG `agno` that a writer keeps up to date.
    pub async fn load<D: BlockDevice + ?Sized>(dev: &D, sb: &Superblock, agno: u32) -> Result<Ag> {
        let (agf, agi) = Self::read_headers(dev, sb, agno).await?;
        let len = be32(&agf, 12);
        if len != sb.ag_len(agno) {
            return Err(corrupt(format!("AG {agno}: AGF length {len}")));
        }
        let mut old_tree_blocks = Vec::new();

        let (recs, blocks) = walk_tree(dev, sb, agno, &BNO, be32(&agf, 16), be32(&agf, 28)).await?;
        old_tree_blocks.extend(blocks);
        let mut free = BTreeMap::new();
        let mut last_end = 0u32;
        for r in &recs {
            let (start, n) = (be32(r, 0), be32(r, 4));
            if n == 0 || start < last_end || start as u64 + n as u64 > len as u64 {
                return Err(corrupt(format!("AG {agno}: free extent {start}+{n}")));
            }
            last_end = start + n;
            free.insert(start, n);
        }
        if free.values().map(|&n| n as u64).sum::<u64>() != be32(&agf, 52) as u64 {
            return Err(corrupt(format!("AG {agno}: free space disagrees with the AGF")));
        }
        let (_, blocks) = walk_tree(dev, sb, agno, &CNT, be32(&agf, 20), be32(&agf, 32)).await?;
        old_tree_blocks.extend(blocks);

        let mut rmap = BTreeSet::new();
        if sb.has_rmapbt() {
            let (recs, blocks) = walk_tree(dev, sb, agno, &RMAP, be32(&agf, 24), be32(&agf, 36)).await?;
            old_tree_blocks.extend(blocks);
            for r in recs {
                rmap.insert(Rmap { start: be32(&r, 0), len: be32(&r, 4), owner: be64(&r, 8), offset: be64(&r, 16) });
            }
        }

        let (recs, blocks) = walk_tree(dev, sb, agno, &INOBT, be32(&agi, 20), be32(&agi, 24)).await?;
        old_tree_blocks.extend(blocks);
        let mut chunks = BTreeMap::new();
        for r in recs {
            let start = be32(&r, 0);
            let chunk = if sb.has_sparse_inodes() {
                Chunk { holemask: be16(&r, 4), count: r[6], free: be64(&r, 8) }
            } else {
                Chunk { holemask: 0, count: 64, free: be64(&r, 8) }
            };
            chunks.insert(start, chunk);
        }
        if sb.has_finobt() {
            let (_, blocks) = walk_tree(dev, sb, agno, &FINOBT, be32(&agi, 328), be32(&agi, 332)).await?;
            old_tree_blocks.extend(blocks);
        }

        let mut agfl_buf = vec![0u8; sb.sect()];
        dev.read_at(Self::header_at(sb, agno, 3), &mut agfl_buf).await?;
        if be32(&agfl_buf, 0) != AGFL_MAGIC || !crc::verify(&agfl_buf, AGFL_CRC) {
            return Err(corrupt(format!("AG {agno}: AGFL")));
        }
        let size = (sb.sect() - 36) / 4;
        let (first, last, count) = (be32(&agf, 40) as usize, be32(&agf, 44) as usize, be32(&agf, 48) as usize);
        let mut agfl = Vec::with_capacity(count);
        if count > 0 {
            if first >= size || last >= size {
                return Err(corrupt(format!("AG {agno}: AGFL from {first} to {last}")));
            }
            let mut i = first;
            loop {
                agfl.push(be32(&agfl_buf, 36 + i * 4));
                if i == last {
                    break;
                }
                i = (i + 1) % size;
            }
            if agfl.len() != count {
                return Err(corrupt(format!("AG {agno}: AGFL holds {} blocks, the AGF says {count}", agfl.len())));
            }
        }

        Ok(Ag { agno, len, agf, agi, free, agfl, old_tree_blocks, chunks, rmap, dirty: false })
    }

    // ---- free space --------------------------------------------------------

    /// Free blocks in this AG.
    pub fn free_blocks(&self) -> u64 {
        self.free.values().map(|&n| n as u64).sum()
    }

    /// Take `start..start+len` out of the free extent that holds it.
    fn take(&mut self, start: u32, len: u32) {
        let (&fs, &fl) = self.free.range(..=start).next_back().expect("taking a block that is not free");
        assert!(fs + fl >= start + len, "taking blocks that are not free");
        self.free.remove(&fs);
        if fs < start {
            self.free.insert(fs, start - fs);
        }
        if start + len < fs + fl {
            self.free.insert(start + len, fs + fl - start - len);
        }
        self.dirty = true;
    }

    /// Up to `want` contiguous blocks, at or after `near` when possible:
    /// the first free extent long enough, else the longest there is.
    pub fn alloc(&mut self, want: u32, near: u32) -> Option<(u32, u32)> {
        let want = want.min(MAX_EXTENT);
        let pick = self
            .free
            .range(near..)
            .chain(self.free.range(..near))
            .find(|(_, &n)| n >= want)
            .map(|(&s, _)| (s, want))
            .or_else(|| self.free.iter().max_by_key(|(_, &n)| n).map(|(&s, &n)| (s, n.min(want))))?;
        self.take(pick.0, pick.1);
        Some(pick)
    }

    /// One free block that is not `not`.
    pub fn alloc_block_except(&mut self, not: u32) -> Option<u32> {
        let b = self.free.iter().find_map(|(&s, &n)| {
            if s != not {
                Some(s)
            } else if n > 1 {
                Some(s + 1)
            } else {
                None
            }
        })?;
        self.take(b, 1);
        Some(b)
    }

    /// `len` contiguous blocks starting at a multiple of `align`.
    pub fn alloc_aligned(&mut self, len: u32, align: u32) -> Option<u32> {
        let start = self.free.iter().find_map(|(&s, &n)| {
            let a = s.div_ceil(align) * align;
            (a as u64 + len as u64 <= s as u64 + n as u64).then_some(a)
        })?;
        self.take(start, len);
        Some(start)
    }

    /// Give blocks back.
    pub fn free_extent(&mut self, start: u32, len: u32) -> Result<()> {
        let end = start as u64 + len as u64;
        if len == 0 || end > self.len as u64 {
            return Err(corrupt(format!("AG {}: freeing {start}+{len}", self.agno)));
        }
        let mut s = start;
        let mut n = len;
        if let Some((&ps, &pn)) = self.free.range(..start).next_back() {
            if ps + pn > start {
                return Err(corrupt(format!("AG {}: freeing {start}+{len}, already free", self.agno)));
            }
            if ps + pn == start {
                self.free.remove(&ps);
                s = ps;
                n += pn;
            }
        }
        if let Some((&ns, &nn)) = self.free.range(start..).next() {
            if (ns as u64) < end {
                return Err(corrupt(format!("AG {}: freeing {start}+{len}, already free", self.agno)));
            }
            if ns as u64 == end {
                self.free.remove(&ns);
                n += nn;
            }
        }
        self.free.insert(s, n);
        self.dirty = true;
        Ok(())
    }

    // ---- reverse mappings --------------------------------------------------

    /// Record that `start..start+len` belongs to `owner` at `offset`.
    pub fn rmap_add(&mut self, sb: &Superblock, start: u32, len: u32, owner: u64, offset: u64) {
        if sb.has_rmapbt() {
            self.rmap.insert(Rmap { start, len, owner, offset });
            self.dirty = true;
        }
    }

    /// Forget `owner`'s mapping of `start..start+len`, splitting any record
    /// that covers more.
    pub fn rmap_remove(&mut self, sb: &Superblock, start: u32, len: u32, owner: u64) -> Result<()> {
        if !sb.has_rmapbt() {
            return Ok(());
        }
        let end = start + len;
        let probe = Rmap { start, owner: 0, offset: 0, len: 0 };
        let mut hits: Vec<Rmap> = self.rmap.range(..probe).next_back().copied().into_iter().collect();
        hits.extend(self.rmap.range(probe..).take_while(|r| r.start < end).copied());
        let mut removed = 0u64;
        for r in hits {
            if r.owner != owner || r.end() <= start || r.start >= end {
                continue;
            }
            self.rmap.remove(&r);
            let (cut_s, cut_e) = (r.start.max(start), r.end().min(end));
            removed += (cut_e - cut_s) as u64;
            let shift = |by: u32| {
                if r.inode_owned() && r.offset & rmapf::BMBT == 0 {
                    r.offset + by as u64
                } else {
                    r.offset
                }
            };
            if r.start < cut_s {
                self.rmap.insert(Rmap { start: r.start, len: cut_s - r.start, owner, offset: r.offset });
            }
            if cut_e < r.end() {
                self.rmap.insert(Rmap { start: cut_e, len: r.end() - cut_e, owner, offset: shift(cut_e - r.start) });
            }
        }
        if removed != len as u64 {
            return Err(corrupt(format!(
                "AG {}: reverse mappings of {start}+{len} for owner {owner:#x} cover {removed} blocks",
                self.agno
            )));
        }
        self.dirty = true;
        Ok(())
    }

    /// The records merged wherever the kernel would keep them as one.
    fn merged_rmap(&self) -> Vec<Rmap> {
        let mut out: Vec<Rmap> = Vec::with_capacity(self.rmap.len());
        // Sorting by (start, owner, offset) puts a record and the one that
        // continues it next to each other, since nothing here overlaps.
        for r in &self.rmap {
            if let Some(last) = out.last_mut() {
                if last.merges_with(r) {
                    last.len += r.len;
                    continue;
                }
            }
            out.push(*r);
        }
        out
    }

    // ---- inodes ------------------------------------------------------------

    /// A free inode in an existing chunk, marked in use.
    pub fn alloc_inode(&mut self) -> Option<u32> {
        let (&start, chunk) = self.chunks.iter_mut().find(|(_, c)| c.free_count() > 0)?;
        let i = chunk.first_free()?;
        chunk.free &= !(1u64 << i);
        self.dirty = true;
        Some(start + i)
    }

    /// Allocate a new, wholly free inode chunk; returns its first AG block.
    pub fn alloc_chunk(&mut self, sb: &Superblock) -> Option<u32> {
        let blocks = CHUNK_INODES / sb.inodes_per_block as u32;
        let align = sb.inode_align.max(blocks).max(1);
        let agbno = self.alloc_aligned(blocks, align)?;
        let start = agbno << sb.inopb_log;
        self.chunks.insert(start, Chunk { holemask: 0, count: CHUNK_INODES as u8, free: u64::MAX });
        self.rmap_add(sb, agbno, blocks, owner::INODES, 0);
        self.dirty = true;
        Some(agbno)
    }

    /// Mark an AG inode free.
    pub fn free_inode(&mut self, agino: u32) -> Result<()> {
        let (&start, chunk) = self
            .chunks
            .range_mut(..=agino)
            .next_back()
            .filter(|(&s, _)| agino < s + CHUNK_INODES)
            .ok_or_else(|| corrupt(format!("AG {}: inode {agino} is in no chunk", self.agno)))?;
        let bit = 1u64 << (agino - start);
        if chunk.free & bit != 0 {
            return Err(corrupt(format!("AG {}: inode {agino} is already free", self.agno)));
        }
        chunk.free |= bit;
        self.dirty = true;
        Ok(())
    }

    // ---- writing it out ----------------------------------------------------

    /// Lay the trees out afresh and return every block and header to write.
    ///
    /// The old trees' blocks are freed first. The new ones are carved from
    /// the end of the longest free extent: two runs, one for the free-space
    /// and reverse-mapping trees (owned by the AG) and one for the inode
    /// trees. Carving changes the very counts the trees are sized by — an
    /// extent used up is one fewer free record, a run next to the AGFL one
    /// fewer mapping — so the sizes are worked out again until they hold.
    pub fn rebuild(&mut self, sb: &Superblock) -> Result<Vec<(u64, Vec<u8>)>> {
        let bs = sb.block_size as usize;
        for b in std::mem::take(&mut self.old_tree_blocks) {
            self.free_extent(b, 1)?;
        }
        self.rmap.retain(|r| r.owner != owner::AG && r.owner != owner::INOBT);
        for b in self.agfl.clone() {
            self.rmap_add(sb, b, 1, owner::AG, 0);
        }

        let n_chunks = self.chunks.len();
        let n_free_chunks = self.chunks.values().filter(|c| c.free_count() > 0).count();
        let ino_blocks = INOBT.blocks(n_chunks, bs) + if sb.has_finobt() { FINOBT.blocks(n_free_chunks, bs) } else { 0 };
        let ag_need = |free_recs: usize, rmap_recs: usize| {
            BNO.blocks(free_recs, bs)
                + CNT.blocks(free_recs, bs)
                + if sb.has_rmapbt() { RMAP.blocks(rmap_recs, bs) } else { 0 }
        };

        let base_free = self.free.clone();
        let base_rmap = self.rmap.clone();
        let mut ag_blocks = ag_need(self.free.len(), self.merged_rmap().len() + 2);
        let mut tries = 0;
        let (ag_run, ino_run) = loop {
            tries += 1;
            if tries > 32 {
                return Err(corrupt(format!("AG {}: B+tree sizes do not settle", self.agno)));
            }
            self.free = base_free.clone();
            self.rmap = base_rmap.clone();
            let ino_run = self.carve(ino_blocks as u32)?;
            let ag_run = self.carve(ag_blocks as u32)?;
            for &(s, n) in &ag_run {
                self.rmap_add(sb, s, n, owner::AG, 0);
            }
            for &(s, n) in &ino_run {
                self.rmap_add(sb, s, n, owner::INOBT, 0);
            }
            let need = ag_need(self.free.len(), self.merged_rmap().len());
            if need == ag_blocks {
                break (ag_run, ino_run);
            }
            ag_blocks = need;
        };

        let expand = |runs: &[(u32, u32)]| -> Vec<u32> { runs.iter().flat_map(|&(s, n)| s..s + n).collect() };
        let ag_list = expand(&ag_run);
        let ino_list = expand(&ino_run);
        let mut writes = Vec::new();
        let agno = self.agno;
        let mut emit = |built: &btree::Built<u32>| {
            for (b, buf) in &built.blocks {
                writes.push((sb.agb_to_byte(agno, *b), buf.clone()));
            }
        };

        // Free space, by block and by length.
        let bno_recs: Vec<Rec> = self
            .free
            .iter()
            .map(|(&s, &n)| {
                let mut d = vec![0u8; 8];
                put32(&mut d, 0, s);
                put32(&mut d, 4, n);
                Rec { low: d.clone(), high: d.clone(), data: d }
            })
            .collect();
        let mut cnt_recs = bno_recs.clone();
        cnt_recs.sort_by_key(|r| (be32(&r.data, 4), be32(&r.data, 0)));
        let rmap_recs: Vec<Rec> = self.merged_rmap().iter().map(Rmap::encode).collect();

        let (nb, nc) = (BNO.blocks(bno_recs.len(), bs), CNT.blocks(cnt_recs.len(), bs));
        let bno = btree::build_ag(sb, agno, &BNO, &bno_recs, &ag_list[..nb]);
        let cnt = btree::build_ag(sb, agno, &CNT, &cnt_recs, &ag_list[nb..nb + nc]);
        emit(&bno);
        emit(&cnt);
        let rmap = if sb.has_rmapbt() {
            let r = btree::build_ag(sb, agno, &RMAP, &rmap_recs, &ag_list[nb + nc..]);
            emit(&r);
            Some(r)
        } else {
            None
        };

        let ino_recs: Vec<Rec> = self.chunks.iter().map(|(&s, c)| c.encode(sb, s)).collect();
        let fino_recs: Vec<Rec> =
            self.chunks.iter().filter(|(_, c)| c.free_count() > 0).map(|(&s, c)| c.encode(sb, s)).collect();
        let ni = INOBT.blocks(ino_recs.len(), bs);
        let inobt = btree::build_ag(sb, agno, &INOBT, &ino_recs, &ino_list[..ni]);
        emit(&inobt);
        let finobt = if sb.has_finobt() {
            let f = btree::build_ag(sb, agno, &FINOBT, &fino_recs, &ino_list[ni..]);
            emit(&f);
            Some(f)
        } else {
            None
        };

        // The AGF.
        let agf = &mut self.agf;
        put32(agf, 16, bno.root);
        put32(agf, 20, cnt.root);
        put32(agf, 28, bno.levels);
        put32(agf, 32, cnt.levels);
        let mut btreeblks = (nb - 1) + (nc - 1);
        if let Some(r) = &rmap {
            put32(agf, 24, r.root);
            put32(agf, 36, r.levels);
            put32(agf, 80, r.blocks.len() as u32);
            btreeblks += r.blocks.len() - 1;
        }
        put32(agf, 52, self.free.values().sum());
        put32(agf, 56, self.free.values().copied().max().unwrap_or(0));
        put32(agf, 60, btreeblks as u32);
        crc::stamp(agf, AGF_CRC);

        // The AGI.
        let agi = &mut self.agi;
        let count: u32 = self.chunks.values().map(|c| c.count as u32).sum();
        let freecount: u32 = self.chunks.values().map(|c| c.free_count()).sum();
        put32(agi, 16, count);
        put32(agi, 20, inobt.root);
        put32(agi, 24, inobt.levels);
        put32(agi, 28, freecount);
        if let Some(&last) = self.chunks.keys().next_back() {
            if be32(agi, 32) == NULL_AGBLOCK {
                put32(agi, 32, last);
            }
        }
        if let Some(f) = &finobt {
            put32(agi, 328, f.root);
            put32(agi, 332, f.levels);
        }
        if sb.has_inobtcount() {
            put32(agi, 336, inobt.blocks.len() as u32);
            put32(agi, 340, finobt.as_ref().map_or(0, |f| f.blocks.len() as u32));
        }
        crc::stamp(agi, AGI_CRC);

        writes.push((Self::header_at(sb, agno, 1), self.agf.clone()));
        writes.push((Self::header_at(sb, agno, 2), self.agi.clone()));

        // Everything is in the new trees now; another flush frees them and
        // works out the AG- and inobt-owned mappings again.
        self.old_tree_blocks = ag_list.into_iter().chain(ino_list).collect();
        self.dirty = false;
        Ok(writes)
    }

    /// Take `n` blocks from the end of the longest free extents.
    fn carve(&mut self, mut n: u32) -> Result<Vec<(u32, u32)>> {
        let mut runs = Vec::new();
        while n > 0 {
            let (&s, &len) = self
                .free
                .iter()
                .max_by_key(|(&s, &len)| (len, s))
                .ok_or_else(|| Error::NoSpace(format!("AG {}: no room for its B+trees", self.agno)))?;
            let take = len.min(n);
            let at = s + len - take;
            self.take(at, take);
            runs.push((at, take));
            n -= take;
        }
        runs.sort();
        Ok(runs)
    }
}
