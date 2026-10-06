//! The log: was the filesystem cleanly unmounted?
//!
//! Nothing here replays the log. It finds the log's head and tail the way
//! the kernel does before it mounts (`xlog_find_head` and `xlog_find_tail`
//! in `fs/xfs/xfs_log_recover.c`) and asks the one question that matters to
//! a reader of the image: is the last record written an unmount record?
//! If it is, every change in the log is already in place on disk. If not,
//! some may not be, and what the image says about them is stale.
//!
//! The log is a ring of 512-byte basic blocks. Every basic block starts
//! with the cycle number it was written in (a record header keeps it in its
//! second word, after its magic), and each pass round the ring bumps the
//! cycle, so the head is where the cycle number drops.

use crate::bytes::{be32, be64};
use crate::device::BlockDevice;
use crate::error::{corrupt, Result};
use crate::sb::Superblock;

/// A record header's first word, `0xFEEDBABE` — never a cycle number.
const HEADER_MAGIC: u32 = 0xFEED_BABE;
/// An operation header flag: this transaction unmounts the filesystem.
const UNMOUNT_TRANS: u8 = 0x20;
/// Bytes in a basic block.
const BB: u64 = 512;
/// The cycle data one header block covers; bigger records add headers.
const HEADER_CYCLE_SIZE: u32 = 32 * 1024;
/// Log records in flight at once, at most.
const MAX_ICLOGS: u64 = 8;

/// What the log says about how the filesystem was left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogState {
    /// Unmounted cleanly (or the log was never written): everything is in
    /// place on disk.
    Clean,
    /// Not cleanly unmounted: the log holds changes, from `tail` up to
    /// `head` (basic blocks into the log), that may not be on disk yet.
    /// Mounting it once replays them.
    Dirty {
        /// Where the next log write would go.
        head: u64,
        /// The oldest record whose changes may not be on disk.
        tail: u64,
    },
    /// The log is on a device of its own, which this crate cannot see.
    External,
    /// The log could not be made sense of; only
    /// [`crate::Volume::open_norecovery`] opens such a filesystem.
    Unreadable(String),
}

impl LogState {
    /// Whether the filesystem can be read as it stands on disk.
    pub fn is_clean(&self) -> bool {
        *self == LogState::Clean
    }
}

impl std::fmt::Display for LogState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogState::Clean => write!(f, "clean"),
            LogState::Dirty { head, tail } => write!(f, "dirty (head {head}, tail {tail})"),
            LogState::External => write!(f, "external (not checked)"),
            LogState::Unreadable(why) => write!(f, "unreadable: {why}"),
        }
    }
}

/// Find out how the filesystem on `dev` was left.
pub async fn state<D: BlockDevice>(dev: &D, sb: &Superblock) -> Result<LogState> {
    if sb.has_external_log() {
        return Ok(LogState::External);
    }
    if sb.log_blocks == 0 {
        return Err(corrupt("internal log of no blocks"));
    }
    let start = sb.fsb_to_byte(sb.log_start)?;
    // An internal log lies inside one AG, so its blocks are contiguous.
    sb.fsb_to_byte(sb.log_start + sb.log_blocks as u64 - 1)?;
    let log = Log {
        dev,
        start,
        bbs: (sb.log_blocks as u64) << (sb.block_log - 9),
        align: sb.block_size as u64,
        v2: sb.has_logv2(),
    };
    log.state().await
}

/// The log's basic blocks, read in whole filesystem blocks.
pub(crate) struct Log<'a, D> {
    pub(crate) dev: &'a D,
    /// Byte offset of the log on the device.
    pub(crate) start: u64,
    /// Basic blocks in the log.
    pub(crate) bbs: u64,
    /// Reads are rounded out to this (the filesystem block size).
    pub(crate) align: u64,
    pub(crate) v2: bool,
}

fn cycle_of(b: &[u8]) -> u32 {
    if be32(b, 0) == HEADER_MAGIC {
        be32(b, 4)
    } else {
        be32(b, 0)
    }
}

fn bytes_to_bbs(n: u32) -> u64 {
    (n as u64).div_ceil(BB)
}

impl<D: BlockDevice> Log<'_, D> {
    /// The largest record, in basic blocks (`XLOG_REC_SHIFT`).
    fn rec_bbs(&self) -> u64 {
        if self.v2 { (256 * 1024) / BB } else { (32 * 1024) / BB }
    }

    /// All the records that can be in flight (`XLOG_TOTAL_REC_SHIFT`).
    fn total_rec_bbs(&self) -> u64 {
        MAX_ICLOGS * self.rec_bbs()
    }

    /// Basic blocks `first..first + count`, which must not wrap.
    async fn read(&self, first: u64, count: u64) -> Result<Vec<u8>> {
        if count == 0 || first + count > self.bbs {
            return Err(corrupt(format!("log read of {count} blocks at {first} outside the log")));
        }
        let from = self.start + first * BB;
        let to = from + count * BB;
        let a = from & !(self.align - 1);
        let b = to.div_ceil(self.align) * self.align;
        let mut buf = vec![0u8; (b - a) as usize];
        self.dev.read_at(a, &mut buf).await?;
        Ok(buf[(from - a) as usize..(to - a) as usize].to_vec())
    }

    async fn cycle(&self, bb: u64) -> Result<u32> {
        Ok(cycle_of(&self.read(bb, 1).await?))
    }

    /// Header blocks of the record whose header is `h`.
    fn hblks(&self, h: &[u8]) -> u64 {
        let size = be32(h, 320);
        if self.v2 && be32(h, 8) & 2 != 0 && size > HEADER_CYCLE_SIZE {
            size.div_ceil(HEADER_CYCLE_SIZE) as u64
        } else {
            1
        }
    }

    /// Binary search between `first` and `last` for the first block in
    /// `cycle` (`xlog_find_cycle_start`).
    async fn find_cycle_start(&self, mut first: u64, last: u64, cycle: u32) -> Result<u64> {
        let mut end = last;
        let mut mid = (first + end) / 2;
        while mid != first && mid != end {
            if self.cycle(mid).await? == cycle {
                end = mid;
            } else {
                first = mid;
            }
            mid = (first + end) / 2;
        }
        Ok(end)
    }

    /// The first block in `start..start + n` stamped `cycle`
    /// (`xlog_find_verify_cycle`).
    async fn find_cycle(&self, start: u64, n: u64, cycle: u32) -> Result<Option<u64>> {
        const CHUNK: u64 = 2048;
        let mut i = start;
        while i < start + n {
            let count = CHUNK.min(start + n - i);
            let buf = self.read(i, count).await?;
            for j in 0..count {
                if cycle_of(&buf[(j * BB) as usize..]) == cycle {
                    return Ok(Some(i + j));
                }
            }
            i += count;
        }
        Ok(None)
    }

    /// Back `last` up to the header of the record it is in the middle of,
    /// looking no further back than `start` (`xlog_find_verify_log_record`).
    /// `Ok(false)` when the start of the log came first.
    async fn back_up_to_record(&self, start: u64, last: &mut u64, extra: u64) -> Result<bool> {
        if *last <= start {
            return if start == 0 { Ok(false) } else { Err(corrupt("log: no record header before the head")) };
        }
        let buf = self.read(start, *last - start).await?;
        let mut i = *last;
        let h = loop {
            if i == start {
                if start == 0 {
                    return Ok(false);
                }
                return Err(corrupt("log: no record header before the head"));
            }
            i -= 1;
            let b = &buf[((i - start) * BB) as usize..((i - start + 1) * BB) as usize];
            if be32(b, 0) == HEADER_MAGIC {
                break b;
            }
        };
        if *last - i + extra != bytes_to_bbs(be32(h, 12)) + self.hblks(h) {
            *last = i;
        }
        Ok(true)
    }

    /// Where the next log write would go (`xlog_find_head`); `None` for a
    /// log that is all zeroes.
    async fn find_head(&self) -> Result<Option<u64>> {
        let bbs = self.bbs;
        let first_cycle = self.cycle(0).await?;
        if first_cycle == 0 {
            return Ok(None);
        }
        let last_cycle = self.cycle(bbs - 1).await?;

        if last_cycle == 0 {
            // Written part way round once (`xlog_find_zeroed`): the head is
            // where the zeroes start.
            let mut last = self.find_cycle_start(0, bbs - 1, 0).await?;
            let n = self.total_rec_bbs().min(last);
            let start = last - n;
            if let Some(b) = self.find_cycle(start, n, 0).await? {
                last = b;
            }
            if !self.back_up_to_record(start, &mut last, 0).await? {
                return Err(corrupt("log: no record header before the zeroed blocks"));
            }
            return Ok(Some(last));
        }

        let (mut head, stop) = if first_cycle == last_cycle {
            // The whole log in one cycle: the head is at the end, unless a
            // hole of the previous cycle says otherwise.
            (bbs, last_cycle.wrapping_sub(1))
        } else {
            (self.find_cycle_start(0, bbs - 1, last_cycle).await?, last_cycle)
        };

        // Records are written out of order: look back over everything
        // that could have been in flight for a block the search skipped.
        let n = self.total_rec_bbs().min(bbs);
        let mut wrapped_hole = false;
        if head >= n {
            if let Some(b) = self.find_cycle(head - n, n, stop).await? {
                head = b;
            }
        } else {
            let start = bbs - (n - head);
            if let Some(b) = self.find_cycle(start, n - head, stop.wrapping_sub(1)).await? {
                head = b;
                wrapped_hole = true;
            }
            if !wrapped_hole {
                if let Some(b) = self.find_cycle(0, head, stop).await? {
                    head = b;
                }
            }
        }

        // The head must not be in the middle of a record.
        let n = self.rec_bbs();
        if head >= n {
            if !self.back_up_to_record(head - n, &mut head, 0).await? {
                return Err(corrupt("log: no record header before the head"));
            }
        } else if !self.back_up_to_record(0, &mut head, 0).await? {
            // The record before the head wraps round the end of the log.
            let start = bbs.checked_sub(n - head).ok_or_else(|| corrupt("log too small"))?;
            let mut end = bbs;
            if !self.back_up_to_record(start, &mut end, head).await? {
                return Err(corrupt("log: no record header before the head"));
            }
            if end != bbs {
                head = end;
            }
        }
        Ok(Some(if head == bbs { 0 } else { head }))
    }

    /// The last record header before `head`, searching back round the
    /// whole ring (`xlog_rseek_logrec_hdr` with the tail at the head).
    async fn header_before(&self, head: u64) -> Result<(u64, Vec<u8>)> {
        const CHUNK: u64 = 256;
        for (lo, hi) in [(0, head), (head, self.bbs)] {
            let mut end = hi;
            while end > lo {
                let begin = end.saturating_sub(CHUNK).max(lo);
                let buf = self.read(begin, end - begin).await?;
                for i in (begin..end).rev() {
                    let b = &buf[((i - begin) * BB) as usize..((i - begin + 1) * BB) as usize];
                    if be32(b, 0) == HEADER_MAGIC {
                        return Ok((i, b.to_vec()));
                    }
                }
                end = begin;
            }
        }
        Err(corrupt("log: no record header anywhere"))
    }

    /// Clean, or dirty from where to where (`xlog_find_tail` and
    /// `xlog_check_unmount_rec`).
    ///
    /// Where the kernel finds a dirty log it goes on to check the records
    /// at the head and trims torn writes, which once in a while leaves a
    /// clean unmount record at the head after all. That is not done here:
    /// such a log is reported dirty, which errs the safe way.
    pub(crate) async fn state(&self) -> Result<LogState> {
        let Some(head) = self.find_head().await? else {
            return Ok(LogState::Clean);
        };
        if head == 0 && self.cycle(0).await? == 0 {
            return Ok(LogState::Clean);
        }
        let (rblk, h) = self.header_before(head).await?;
        let tail = be64(&h, 24) & 0xffff_ffff;
        let hblks = self.hblks(&h);
        let after = (rblk + hblks + bytes_to_bbs(be32(&h, 12))) % self.bbs;
        if head == after && be32(&h, 40) == 1 {
            let data = self.read((rblk + hblks) % self.bbs, 1).await?;
            // The operation header: tid, len, clientid, then the flags.
            if data[9] & UNMOUNT_TRANS != 0 {
                return Ok(LogState::Clean);
            }
        }
        Ok(LogState::Dirty { head, tail })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::MemDevice;

    const LOG_BBS: u64 = 4096;

    /// A log of `LOG_BBS` basic blocks, written by hand.
    struct Ring(Vec<u8>);

    impl Ring {
        fn new() -> Self {
            Ring(vec![0; (LOG_BBS * BB) as usize])
        }

        fn put32(&mut self, bb: u64, off: usize, v: u32) {
            let at = (bb * BB) as usize + off;
            self.0[at..at + 4].copy_from_slice(&v.to_be_bytes());
        }

        fn put64(&mut self, bb: u64, off: usize, v: u64) {
            let at = (bb * BB) as usize + off;
            self.0[at..at + 8].copy_from_slice(&v.to_be_bytes());
        }

        /// A record at `at`: one header block and `data` data blocks, each
        /// stamped with `cycle`; its one operation unmounts if `unmount`.
        /// Data blocks past the end of the log wrap to its start.
        fn record(&mut self, at: u64, cycle: u32, data: u64, unmount: bool, tail: u64) {
            self.0[(at * BB) as usize..((at + 1) * BB) as usize].fill(0);
            self.put32(at, 0, HEADER_MAGIC);
            self.put32(at, 4, cycle);
            self.put32(at, 8, 2);
            self.put32(at, 12, (data * BB) as u32);
            self.put64(at, 16, (cycle as u64) << 32 | at);
            self.put64(at, 24, (cycle as u64) << 32 | tail);
            self.put32(at, 40, if unmount { 1 } else { 3 });
            self.put32(at, 320, 32768);
            for i in 1..=data {
                let bb = (at + i) % LOG_BBS;
                self.0[(bb * BB) as usize..((bb + 1) * BB) as usize].fill(0);
                self.put32(bb, 0, cycle);
                if i == 1 && unmount {
                    self.0[(bb * BB) as usize + 9] = UNMOUNT_TRANS;
                }
            }
        }

        /// Records of eight blocks, of no interest, over `from..to`.
        fn fill(&mut self, from: u64, to: u64, cycle: u32) {
            let mut at = from;
            while at < to {
                let data = (to - at).min(8) - 1;
                self.record(at, cycle, data, false, from);
                at += data + 1;
            }
        }

        async fn state(self) -> LogState {
            let dev = MemDevice::new(self.0);
            let log = Log { dev: &dev, start: 0, bbs: LOG_BBS, align: 4096, v2: true };
            log.state().await.unwrap()
        }
    }

    #[tokio::test]
    async fn zeroed_log_is_clean() {
        assert_eq!(Ring::new().state().await, LogState::Clean);
    }

    #[tokio::test]
    async fn fresh_unmount_record_is_clean() {
        let mut r = Ring::new();
        r.record(0, 1, 1, true, 0);
        assert_eq!(r.state().await, LogState::Clean);
    }

    #[tokio::test]
    async fn transactions_after_the_unmount_record_are_dirty() {
        let mut r = Ring::new();
        r.record(0, 1, 1, true, 0);
        r.fill(2, 50, 1);
        assert_eq!(r.state().await, LogState::Dirty { head: 50, tail: 2 });
    }

    #[tokio::test]
    async fn wrapped_log_clean_and_dirty() {
        for unmount in [true, false] {
            let mut r = Ring::new();
            r.fill(0, 1000, 5);
            r.record(1000, 5, 1, unmount, 1000);
            r.fill(1002, LOG_BBS, 4);
            let want = if unmount { LogState::Clean } else { LogState::Dirty { head: 1002, tail: 1000 } };
            assert_eq!(r.state().await, want);
        }
    }

    #[tokio::test]
    async fn head_at_the_end_of_the_log() {
        let mut r = Ring::new();
        r.fill(0, LOG_BBS - 2, 7);
        r.record(LOG_BBS - 2, 7, 1, true, LOG_BBS - 2);
        assert_eq!(r.state().await, LogState::Clean);
    }

    #[tokio::test]
    async fn unmount_record_wrapping_round_the_end() {
        let mut r = Ring::new();
        r.fill(0, LOG_BBS - 1, 6);
        // The header in the last block, its data in block 0 of the next
        // cycle, and the rest of the old cycle after it.
        r.record(LOG_BBS - 1, 6, 1, true, LOG_BBS - 1);
        r.put32(0, 0, 7);
        assert_eq!(r.state().await, LogState::Clean);
    }

    #[tokio::test]
    async fn stray_new_cycle_blocks_past_the_head() {
        let mut r = Ring::new();
        r.fill(0, 1000, 5);
        r.record(1000, 5, 1, true, 1000);
        r.fill(1002, LOG_BBS, 4);
        // Blocks of the new cycle beyond a hole of the old one: the binary
        // search may land past the hole, and the scan back finds it.
        r.fill(1010, 1018, 5);
        assert_eq!(r.state().await, LogState::Clean);
    }

    #[tokio::test]
    async fn stray_blocks_after_a_dirty_head() {
        let mut r = Ring::new();
        r.fill(0, 1000, 5);
        r.fill(1002, LOG_BBS, 4);
        r.fill(1000, 1002, 5);
        r.fill(1010, 1018, 5);
        assert_eq!(r.state().await, LogState::Dirty { head: 1002, tail: 1000 });
    }
}
