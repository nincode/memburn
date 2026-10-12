use core::cell::UnsafeCell;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::memtest::TestId;

/// Counter plus a spin-locked ring of the most recent entries; usable from CPUs with no OS (UEFI APs).
pub struct Ring<T: Copy, const N: usize> {
    count: AtomicU64,
    lock: AtomicBool,
    ring: UnsafeCell<[T; N]>,
}

unsafe impl<T: Copy + Send, const N: usize> Sync for Ring<T, N> {}

impl<T: Copy, const N: usize> Ring<T, N> {
    pub const fn new(zero: T) -> Self {
        Ring { count: AtomicU64::new(0), lock: AtomicBool::new(false), ring: UnsafeCell::new([zero; N]) }
    }

    #[cold]
    pub fn record(&self, e: T) {
        self.with_lock(|ring| {
            let n = self.count.load(Ordering::Relaxed);
            ring[(n % N as u64) as usize] = e;
            self.count.store(n + 1, Ordering::Release);
        });
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Acquire)
    }

    /// Copies retained entries `from..count()` into `out`; returns (written, next index).
    pub fn since(&self, from: u64, out: &mut [T]) -> (usize, u64) {
        self.with_lock(|ring| {
            let n = self.count.load(Ordering::Relaxed);
            let mut i = from.max(n.saturating_sub(N as u64));
            let mut w = 0;
            while i < n && w < out.len() {
                out[w] = ring[(i % N as u64) as usize];
                w += 1;
                i += 1;
            }
            (w, i)
        })
    }

    fn with_lock<R>(&self, f: impl FnOnce(&mut [T; N]) -> R) -> R {
        while self.lock.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            core::hint::spin_loop();
        }
        let r = f(unsafe { &mut *self.ring.get() });
        self.lock.store(false, Ordering::Release);
        r
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct MemError {
    pub addr: usize,
    pub expected: u64,
    pub actual: u64,
    pub test: u8,
    pub pass: u32,
}

impl fmt::Display for MemError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let test = TestId::from_u8(self.test).map_or("?", |t| t.name());
        write!(
            f,
            "wrote {:016x}, read {:016x}, {} ({test}, pass {})",
            self.expected,
            self.actual,
            FlippedBits(self.expected, self.actual),
            self.pass + 1
        )
    }
}

/// "bit 17 1->0", "bits 3,17 flipped" or "12 bits flipped".
pub struct FlippedBits(pub u64, pub u64);

impl fmt::Display for FlippedBits {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let x = self.0 ^ self.1;
        match x.count_ones() {
            0 => write!(f, "no bits differ (unstable read)"),
            1 => {
                let b = x.trailing_zeros();
                let was = (self.0 >> b) & 1;
                write!(f, "bit {b} flipped {was}->{}", was ^ 1)
            }
            2..=4 => {
                write!(f, "bits ")?;
                let mut first = true;
                for b in (0..64).filter(|b| x >> b & 1 == 1) {
                    write!(f, "{}{b}", if first { "" } else { "," })?;
                    first = false;
                }
                write!(f, " flipped")
            }
            n => write!(f, "{n} bits flipped"),
        }
    }
}

const MEM_RING: usize = 64;

pub struct ErrorLog {
    ring: Ring<MemError, MEM_RING>,
    bits: AtomicU64,
    /// Per data bit: reads that came back 1 where 0 was written.
    rose: [AtomicU32; 64],
    /// Per data bit: reads that came back 0 where 1 was written.
    fell: [AtomicU32; 64],
}

impl Default for ErrorLog {
    fn default() -> Self {
        Self::new()
    }
}

impl ErrorLog {
    pub const fn new() -> Self {
        ErrorLog {
            ring: Ring::new(MemError { addr: 0, expected: 0, actual: 0, test: 0, pass: 0 }),
            bits: AtomicU64::new(0),
            rose: [const { AtomicU32::new(0) }; 64],
            fell: [const { AtomicU32::new(0) }; 64],
        }
    }

    #[cold]
    pub fn record(&self, e: MemError) {
        let x = e.expected ^ e.actual;
        self.bits.fetch_or(x, Ordering::Relaxed);
        for b in (0..64).filter(|b| x >> b & 1 == 1) {
            let dir = if e.actual >> b & 1 == 1 { &self.rose } else { &self.fell };
            dir[b].fetch_add(1, Ordering::Relaxed);
        }
        self.ring.record(e);
    }

    pub fn count(&self) -> u64 {
        self.ring.count()
    }

    /// Union of all flipped bits seen so far.
    pub fn bits(&self) -> u64 {
        self.bits.load(Ordering::Relaxed)
    }

    /// (0->1, 1->0) flip counts for one data bit.
    pub fn flips(&self, bit: usize) -> (u32, u32) {
        (self.rose[bit].load(Ordering::Relaxed), self.fell[bit].load(Ordering::Relaxed))
    }

    pub fn since(&self, from: u64, out: &mut [MemError]) -> (usize, u64) {
        self.ring.since(from, out)
    }
}

pub const MEM_BANNER: &str =
    "MEMORY ERROR: RAM returned data different from what was written. The memory is faulty or unstable.";
pub const BURN_BANNER: &str =
    "CPU ERROR: a CPU computed a wrong result under load (overclock, too little voltage, heat, or failing hardware).";
pub const CONTINUING: &str = "Testing continues to collect detail; every error is listed below.";

/// What to do about the result, one line per outcome.
pub fn advice(mem_errors: u64, burn_errors: u64) -> &'static str {
    match (mem_errors > 0, burn_errors > 0) {
        (false, false) => "No errors found.",
        (true, false) => {
            "Reset memory to stock (disable XMP/EXPO), reseat it, then test one stick at a time to find the bad one."
        }
        (false, true) => "Reset CPU and memory to stock settings, check cooling, then run the burn-in again.",
        (true, true) => "Reset CPU and memory to stock, check cooling, then retest memory one stick at a time.",
    }
}

/// "s" unless `n` is 1.
pub fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Bit positions set in a mask: "bit 3", "bits 3, 17" or "12 different bits".
pub struct BitList(pub u64);

impl fmt::Display for BitList {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let n = self.0.count_ones();
        if n == 0 || n > 8 {
            return write!(f, "{n} different bits");
        }
        write!(f, "bit{} ", plural(n as u64))?;
        let mut first = true;
        for b in (0..64).filter(|b| self.0 >> b & 1 == 1) {
            write!(f, "{}{b}", if first { "" } else { ", " })?;
            first = false;
        }
        Ok(())
    }
}
