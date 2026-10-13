//! Per-burner figures for the dashboard: each burner stores only into its own lane, the screen reads them all.
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

use super::{Fault, Outcome, Plan};

/// Error histogram of each FFT's worst rounding error: [`HIST_PER_DEC`] bins per decade from [`HIST_LO`].
pub const HIST_PER_DEC: usize = 8;
pub const HIST_DECADES: usize = 6;
pub const HIST_BINS: usize = HIST_PER_DEC * HIST_DECADES;
pub const HIST_LO: f64 = 1e-5;

/// Lower edge of bins 1.., f64 bits.
const EDGES: [u64; HIST_BINS - 1] = edges();

const fn edges() -> [u64; HIST_BINS - 1] {
    /// 10^(1/8).
    const STEP: f64 = 1.333_521_432_163_324;
    let (mut e, mut x, mut i) = ([0; HIST_BINS - 1], HIST_LO, 0);
    while i < HIST_BINS - 1 {
        x *= STEP;
        e[i] = x.to_bits();
        i += 1;
    }
    e
}

/// Histogram bin of a rounding error given as f64 bits.
pub fn hist_bin(err: u64) -> usize {
    EDGES.partition_point(|&e| e <= err)
}

/// Fractional bin where `err` falls, for drawing marks such as the limit on the bins' axis.
pub fn hist_pos(err: f64) -> f32 {
    let b = hist_bin(err.to_bits());
    let lo = if b == 0 { HIST_LO } else { f64::from_bits(EDGES[b - 1]) };
    let hi = f64::from_bits(EDGES[b.min(HIST_BINS - 2)]);
    let k = if b >= HIST_BINS - 1 || hi <= lo { 0.0 } else { ((err - lo) / (hi - lo)) as f32 };
    b as f32 + k.clamp(0.0, 1.0)
}

/// One burner's counters; only that burner stores, so plain load-and-store replaces read-modify-write.
#[repr(align(64))]
pub struct Lane {
    pub ffts: AtomicU64,
    pub flops: AtomicU64,
    /// Flops done in the current FFT size's slice.
    pub slice: AtomicU64,
    pub log2n: AtomicU32,
    pub faults: AtomicU32,
    /// Worst rounding error of the last transform, f64 bits.
    pub last_err: AtomicU64,
    /// Last fault as [`Fault::code`], 0 for none, and its rounding error as f64 bits.
    pub fault: AtomicU32,
    pub fault_err: AtomicU64,
    /// Passing transforms per [`hist_bin`].
    pub hist: [AtomicU32; HIST_BINS],
}

impl Default for Lane {
    fn default() -> Self {
        Lane {
            ffts: AtomicU64::new(0),
            flops: AtomicU64::new(0),
            slice: AtomicU64::new(0),
            log2n: AtomicU32::new(0),
            faults: AtomicU32::new(0),
            last_err: AtomicU64::new(0),
            fault: AtomicU32::new(0),
            fault_err: AtomicU64::new(0),
            hist: [const { AtomicU32::new(0) }; HIST_BINS],
        }
    }
}

fn add64(a: &AtomicU64, n: u64) {
    a.store(a.load(Relaxed).wrapping_add(n), Relaxed);
}

fn add32(a: &AtomicU32, n: u32) {
    a.store(a.load(Relaxed).wrapping_add(n), Relaxed);
}

impl Lane {
    /// Called by the lane's own burner after each transform.
    pub(super) fn record(&self, plan: &Plan, o: &Outcome, slice: u64) {
        add64(&self.ffts, 1);
        add64(&self.flops, plan.flops());
        self.slice.store(slice, Relaxed);
        self.log2n.store(plan.log2n, Relaxed);
        self.last_err.store(o.max_err, Relaxed);
        match o.fault() {
            None => add32(&self.hist[hist_bin(o.max_err)], 1),
            Some(f) => {
                self.fault_err.store(o.max_err, Relaxed);
                self.fault.store(f.code(), Relaxed);
                add32(&self.faults, 1);
            }
        }
    }

    /// The last fault and its rounding error as f64 bits.
    pub fn last_fault(&self) -> Option<(Fault, u64)> {
        Fault::from_code(self.fault.load(Relaxed)).map(|f| (f, self.fault_err.load(Relaxed)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bins_are_eight_per_decade_from_1e_5() {
        assert_eq!(hist_bin(0.0f64.to_bits()), 0);
        assert_eq!(hist_bin(0.9e-5f64.to_bits()), 0);
        assert_eq!(hist_bin(1.2e-5f64.to_bits()), 0);
        assert_eq!(hist_bin(1.4e-5f64.to_bits()), 1);
        assert_eq!(hist_bin(1.01e-4f64.to_bits()), 8);
        assert_eq!(hist_bin(0.0101f64.to_bits()), 24);
        assert_eq!(hist_bin(0.4f64.to_bits()), 36);
        assert_eq!(hist_bin(9.9f64.to_bits()), HIST_BINS - 1);
        assert_eq!(hist_bin(1e30f64.to_bits()), HIST_BINS - 1);
        let p = hist_pos(super::super::MAX_ERR);
        assert!((36.0..37.0).contains(&p), "{p}");
        for k in 1..HIST_BINS {
            assert!(hist_pos(f64::from_bits(EDGES[k - 1]) * 1.0001) >= k as f32);
        }
    }

    #[test]
    fn lane_counts_passes_by_bin_and_faults_apart() {
        let plan = Plan::new(10);
        let l = Lane::default();
        let ok = Outcome { max_err: 0.02f64.to_bits(), sums_ok: true, non_finite: false };
        let bad = Outcome { max_err: 0.03f64.to_bits(), sums_ok: false, non_finite: false };
        for _ in 0..3 {
            l.record(&plan, &ok, 7);
        }
        l.record(&plan, &bad, 9);
        assert_eq!(l.ffts.load(Relaxed), 4);
        assert_eq!(l.flops.load(Relaxed), 4 * plan.flops());
        assert_eq!(l.hist[hist_bin(ok.max_err)].load(Relaxed), 3);
        assert_eq!(l.hist.iter().map(|h| h.load(Relaxed)).sum::<u32>(), 3);
        assert_eq!(l.faults.load(Relaxed), 1);
        assert_eq!(l.last_fault(), Some((Fault::WrongResult, bad.max_err)));
        assert_eq!((l.slice.load(Relaxed), l.log2n.load(Relaxed)), (9, 10));
    }
}
