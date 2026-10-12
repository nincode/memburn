use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::cpu::Isa;
use crate::errlog::ErrorLog;
use crate::memtest::{Ctx, RawWords, TestId};

#[derive(Clone, Copy, Debug)]
pub struct Chunk {
    pub ptr: *mut u64,
    pub words: usize,
    /// Global word index of `ptr` across all memory under test.
    pub base: u64,
}

unsafe impl Send for Chunk {}
unsafe impl Sync for Chunk {}

/// Largest chunk, bytes; progress and traffic counters advance once per chunk.
pub const MAX_CHUNK: usize = 64 << 20;

/// Chunk size for `total` bytes on `threads` workers: about 8 chunks each, 1 MiB to [`MAX_CHUNK`].
pub fn chunk_bytes(total: u64, threads: usize) -> usize {
    (total / (threads.max(1) as u64 * 8)).clamp(1 << 20, MAX_CHUNK as u64) as usize
}

/// Splits `(addr, bytes)` regions into 4 KiB-trimmed chunks of at most `max_bytes`.
pub fn chunk_regions(regions: &[(usize, usize)], max_bytes: usize) -> Vec<Chunk> {
    const PAGE: usize = 4096;
    let max_bytes = (max_bytes / PAGE).max(1) * PAGE;
    let mut out = Vec::new();
    let mut base = 0u64;
    for &(addr, bytes) in regions {
        let start = addr.next_multiple_of(PAGE);
        let end = (addr + bytes) / PAGE * PAGE;
        let mut a = start;
        while a < end {
            let len = (end - a).min(max_bytes);
            out.push(Chunk { ptr: a as *mut u64, words: len / 8, base });
            base += (len / 8) as u64;
            a += len;
        }
    }
    out
}

/// Memory progress counters, bytes: `tested` drives progress and ETA, `traffic` the rates.
#[derive(Default)]
pub struct Moved {
    pub tested: AtomicU64,
    pub traffic: AtomicU64,
}

impl Moved {
    pub fn tested(&self) -> u64 {
        self.tested.load(Ordering::Relaxed)
    }

    pub fn traffic(&self) -> u64 {
        self.traffic.load(Ordering::Relaxed)
    }
}

/// One phase of one test over every chunk; done once every CPU calling `work`/`work_one` returns.
pub struct PhaseJob<'a> {
    pub test: TestId,
    pub phase: usize,
    pub pass: u64,
    chunks: &'a [Chunk],
    next: AtomicUsize,
    pub moved: &'a Moved,
    pub log: &'a ErrorLog,
    pub stop: &'a AtomicBool,
    pub isa: Isa,
}

impl<'a> PhaseJob<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        test: TestId,
        phase: usize,
        pass: u64,
        chunks: &'a [Chunk],
        moved: &'a Moved,
        log: &'a ErrorLog,
        stop: &'a AtomicBool,
        isa: Isa,
    ) -> Self {
        PhaseJob { test, phase, pass, chunks, next: AtomicUsize::new(0), moved, log, stop, isa }
    }

    /// Runs one chunk; false once none are left or a stop was requested.
    pub fn work_one(&self) -> bool {
        if self.stop.load(Ordering::Relaxed) {
            return false;
        }
        let k = self.next.fetch_add(1, Ordering::Relaxed);
        let n = self.chunks.len();
        if k >= n {
            return false;
        }
        let c = self.chunks[if self.test.descending(self.phase) { n - 1 - k } else { k }];
        let mut w = unsafe { RawWords::new(c.ptr, c.words) };
        let ctx = Ctx { pass: self.pass, base: c.base, test: self.test, log: self.log, isa: self.isa };
        self.test.run(self.phase, &mut w, &ctx);
        let bytes = (c.words * 8) as u64;
        let (num, den) = self.test.traffic(self.phase);
        self.moved.tested.fetch_add(bytes, Ordering::Relaxed);
        self.moved.traffic.fetch_add(bytes * num / den, Ordering::Relaxed);
        true
    }

    pub fn work(&self) {
        while self.work_one() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memtest::ALL;

    #[test]
    fn chunks_cover_regions() {
        let c = chunk_regions(&[(0x1000, 0x5000), (0x10_0800, 0x3000)], 0x2000);
        let v: Vec<_> = c.iter().map(|c| (c.ptr as usize, c.words * 8, c.base)).collect();
        assert_eq!(
            v,
            [(0x1000, 0x2000, 0), (0x3000, 0x2000, 0x400), (0x5000, 0x1000, 0x800), (0x10_1000, 0x2000, 0xA00)]
        );
    }

    #[test]
    fn counters_advance_at_least_every_64_mib() {
        assert_eq!(chunk_bytes(60 << 30, 32), MAX_CHUNK);
        assert_eq!(chunk_bytes(8 << 30, 16), MAX_CHUNK);
        assert_eq!(chunk_bytes(256 << 20, 16), 2 << 20);
        assert_eq!(chunk_bytes(4 << 20, 16), 1 << 20);
        let regions = [(0x10_0000, 60usize << 30), (0x20_0000_0000, (3 << 30) + 4096)];
        let chunks = chunk_regions(&regions, chunk_bytes(63 << 30, 32));
        assert!(chunks.iter().all(|c| c.words * 8 <= MAX_CHUNK));
        assert_eq!(chunks.iter().map(|c| c.words as u64 * 8).sum::<u64>(), (63 << 30) + 4096);
    }

    #[test]
    fn threads_run_every_test_on_real_memory() {
        let mut buf = vec![0u64; 1 << 18];
        let regions = [(buf.as_mut_ptr() as usize, buf.len() * 8)];
        let chunks = chunk_regions(&regions, 64 << 10);
        let moved = Moved::default();
        let log = ErrorLog::new();
        let stop = AtomicBool::new(false);
        for t in ALL {
            for ph in 0..t.phases() {
                let job = PhaseJob::new(t, ph, 1, &chunks, &moved, &log, &stop, crate::cpu::best_isa());
                std::thread::scope(|s| {
                    for _ in 0..4 {
                        s.spawn(|| job.work());
                    }
                });
            }
        }
        assert_eq!(log.count(), 0);
        let sweeps: usize = ALL.iter().map(|t| t.phases()).sum();
        let tested: usize = chunks.iter().map(|c| c.words * 8).sum();
        assert!(tested >= buf.len() * 8 - 8192);
        assert_eq!(moved.tested(), (sweeps * tested) as u64);
        let traffic: u64 = ALL
            .iter()
            .flat_map(|&t| (0..t.phases()).map(move |ph| t.traffic(ph)))
            .flat_map(|(num, den)| chunks.iter().map(move |c| (c.words * 8) as u64 * num / den))
            .sum();
        assert_eq!(moved.traffic(), traffic);
    }
}
