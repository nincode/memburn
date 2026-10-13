pub mod kernels;
pub mod lane;

use alloc::alloc::{Layout, alloc_zeroed, dealloc};
use alloc::vec::Vec;
use core::fmt;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::cpu::Isa;
use crate::errlog::Ring;
use crate::rng::Rng;
use kernels::MAGIC;
pub use lane::Lane;

pub const MIN_LOG2N: u32 = 4;
pub const MAX_LOG2N: u32 = 24;
pub const MAX_ERR: f64 = 0.4;
const MAX_ERR_BITS: u64 = MAX_ERR.to_bits();
/// Flops each burner spends on one FFT size before moving to the next.
pub const SLICE_FLOPS: u64 = 20_000_000_000;

/// 64-byte aligned, zeroed f64 buffer.
pub struct Buf {
    ptr: NonNull<f64>,
    len: usize,
}

unsafe impl Send for Buf {}
unsafe impl Sync for Buf {}

impl Buf {
    pub fn new(len: usize) -> Buf {
        let p = unsafe { alloc_zeroed(Self::layout(len)) } as *mut f64;
        Buf { ptr: NonNull::new(p).expect("out of memory"), len }
    }

    fn layout(len: usize) -> Layout {
        Layout::from_size_align(len.max(1) * 8, 64).unwrap()
    }

    fn p(&self) -> *mut f64 {
        self.ptr.as_ptr()
    }

    fn bits_mut(&mut self) -> &mut [u64] {
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr() as *mut u64, self.len) }
    }
}

impl Drop for Buf {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr.as_ptr() as *mut u8, Self::layout(self.len)) }
    }
}

/// Twiddles for one FFT size; stage span h uses entries h..2h.
pub struct Plan {
    pub log2n: u32,
    pub n: usize,
    /// Bits per input digit; sized so the convolution stays exactly representable.
    pub bits: u32,
    tw_re: Buf,
    tw_im: Buf,
}

impl Plan {
    pub fn new(log2n: u32) -> Plan {
        assert!((MIN_LOG2N..=MAX_LOG2N).contains(&log2n));
        let n = 1usize << log2n;
        let half = n / 2;
        let mut tw_re = Buf::new(n);
        let mut tw_im = Buf::new(n);
        let (re, im) = (tw_re.bits_mut(), tw_im.bits_mut());
        let step = -2.0 * core::f64::consts::PI / n as f64;
        for j in 0..half {
            let t = step * j as f64;
            re[half + j] = libm::cos(t).to_bits();
            im[half + j] = libm::sin(t).to_bits();
        }
        let mut h = half / 2;
        while h >= 1 {
            let stride = half / h;
            for j in 0..h {
                re[h + j] = re[half + j * stride];
                im[h + j] = im[half + j * stride];
            }
            h /= 2;
        }
        Plan { log2n, n, bits: bits_for(log2n), tw_re, tw_im }
    }

    /// Heap bytes of the twiddle tables for size 2^log2n.
    pub fn bytes(log2n: u32) -> usize {
        16 << log2n
    }

    /// Two radix-2 transforms at 5 n log2(n) each.
    pub fn flops(&self) -> u64 {
        10 * self.n as u64 * self.log2n as u64
    }
}

/// Largest possible output n * 2^(2 bits - 2) stays within 2^50, inside the 2^51 rounding range.
pub fn bits_for(log2n: u32) -> u32 {
    ((52 - log2n) / 2).clamp(4, 24)
}

pub struct Work {
    re: Buf,
    im: Buf,
}

impl Work {
    pub fn new(max_log2n: u32) -> Work {
        Work { re: Buf::new(1 << max_log2n), im: Buf::new(1 << max_log2n) }
    }

    /// Heap bytes of one burner's buffers for sizes up to 2^max_log2n.
    pub fn bytes(max_log2n: u32) -> usize {
        16 << max_log2n
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Outcome {
    /// Largest distance from an integer, f64 bits.
    pub max_err: u64,
    pub sums_ok: bool,
    pub non_finite: bool,
}

impl Outcome {
    pub fn ok(&self) -> bool {
        self.sums_ok && !self.non_finite && self.max_err <= MAX_ERR_BITS
    }

    pub fn fault(&self) -> Option<Fault> {
        if self.non_finite {
            Some(Fault::NonFinite)
        } else if !self.sums_ok {
            Some(Fault::WrongResult)
        } else if self.max_err > MAX_ERR_BITS {
            Some(Fault::Rounding)
        } else {
            None
        }
    }
}

/// Squares random balanced digits by FFT convolution; checks rounding, sum and alternating-sum identities.
pub fn square_mod(isa: Isa, plan: &Plan, work: &mut Work, seed: u64) -> Outcome {
    run(isa, plan, work, seed, None)
}

fn run(isa: Isa, plan: &Plan, work: &mut Work, seed: u64, fault: Option<(usize, u64)>) -> Outcome {
    let n = plan.n;
    assert!(work.re.len >= n);
    let (mut sum, mut alt) = (0i64, 0i64);
    {
        let half = 1i64 << (plan.bits - 1);
        let mask = (1u64 << plan.bits) - 1;
        let mut rng = Rng::new(seed);
        let im = &mut work.im.bits_mut()[..n];
        im.fill(0);
        let re = &mut work.re.bits_mut()[..n];
        for (i, r) in re.iter_mut().enumerate() {
            let d = (rng.next_u64() & mask) as i64 - half;
            *r = MAGIC.wrapping_add(d as u64);
            sum += d;
            alt += if i & 1 == 0 { d } else { -d };
        }
    }
    let (re, im) = (work.re.p(), work.im.p());
    let (wr, wi) = (plan.tw_re.p(), plan.tw_im.p());
    let stage_isa = |h: usize| if h >= 4 { isa } else { Isa::Sse2 };
    unsafe {
        kernels::unmagic(re, n);
        let mut h = n / 2;
        while h >= 2 {
            kernels::dif(stage_isa(h), re, im, wr.add(h), wi.add(h), h, n);
            h /= 2;
        }
        kernels::pairs(re, n);
        kernels::pairs(im, n);
        kernels::square(isa, re, im, n);
        if let Some((i, bit)) = fault {
            *work.re.bits_mut().get_mut(i).unwrap() ^= bit;
        }
        kernels::pairs(re, n);
        kernels::pairs(im, n);
        let mut h = 2;
        while h < n {
            kernels::dit(stage_isa(h), re, im, wr.add(h), wi.add(h), h, n);
            h *= 2;
        }
    }
    let scale = ((1023 - plan.log2n) as u64) << 52;
    let (max_err, non_finite) = unsafe { kernels::round_check(isa, re, im, n, scale) };
    let (mut csum, mut calt) = (0i128, 0i128);
    for (i, &b) in work.re.bits_mut()[..n].iter().enumerate() {
        let c = b.wrapping_sub(MAGIC) as i64 as i128;
        csum += c;
        calt += if i & 1 == 0 { c } else { -c };
    }
    let sums_ok = csum == sum as i128 * sum as i128 && calt == alt as i128 * alt as i128;
    Outcome { max_err, sums_ok, non_finite }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Exact checksums disagree: some value came out wrong.
    WrongResult,
    /// An output drifted too far from an integer.
    Rounding,
    NonFinite,
}

impl Fault {
    /// 1.., so that 0 can stand for none.
    pub fn code(self) -> u32 {
        self as u32 + 1
    }

    pub fn from_code(c: u32) -> Option<Fault> {
        [Fault::WrongResult, Fault::Rounding, Fault::NonFinite].get((c as usize).wrapping_sub(1)).copied()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct BurnError {
    pub worker: u32,
    pub log2n: u32,
    pub seed: u64,
    pub fault: Fault,
    /// f64 bits.
    pub max_err: u64,
}

impl fmt::Display for BurnError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "CPU {}: FFT 2^{} ", self.worker, self.log2n)?;
        match self.fault {
            Fault::WrongResult => write!(f, "gave a wrong result (checksums differ)")?,
            Fault::Rounding => {
                write!(f, "rounding error {:.3} over the {MAX_ERR} limit", f64::from_bits(self.max_err))?
            }
            Fault::NonFinite => write!(f, "produced NaN or infinity")?,
        }
        write!(f, " (seed {:#x})", self.seed)
    }
}

pub struct BurnStats {
    pub iters: AtomicU64,
    pub flops: AtomicU64,
    /// f64 bits; non-negative doubles order like their bit patterns.
    pub max_err: AtomicU64,
    pub errors: Ring<BurnError, 16>,
    /// Self-test: the next FFT any burner runs gets a corrupted value.
    pub inject: AtomicBool,
    /// Per-burner figures for the dashboard, indexed by worker; empty when nothing draws them.
    pub lanes: Vec<Lane>,
}

impl Default for BurnStats {
    fn default() -> Self {
        BurnStats {
            iters: AtomicU64::new(0),
            flops: AtomicU64::new(0),
            max_err: AtomicU64::new(0),
            errors: Ring::new(BurnError { worker: 0, log2n: 0, seed: 0, fault: Fault::WrongResult, max_err: 0 }),
            inject: AtomicBool::new(false),
            lanes: Vec::new(),
        }
    }
}

impl BurnStats {
    /// Stats with a [`Lane`] for each of workers `0..n`.
    pub fn with_lanes(n: usize) -> Self {
        BurnStats { lanes: (0..n).map(|_| Lane::default()).collect(), ..BurnStats::default() }
    }

    fn record(&self, plan: &Plan, worker: u32, seed: u64, o: &Outcome) {
        self.iters.fetch_add(1, Ordering::Relaxed);
        self.flops.fetch_add(plan.flops(), Ordering::Relaxed);
        match o.fault() {
            None => {
                self.max_err.fetch_max(o.max_err, Ordering::Relaxed);
            }
            Some(fault) => self.errors.record(BurnError { worker, log2n: plan.log2n, seed, fault, max_err: o.max_err }),
        }
    }
}

/// One CPU's burn-in state: cycles through `plans`, spending about [`SLICE_FLOPS`] on each.
pub struct Burner<'a> {
    isa: Isa,
    plans: &'a [Plan],
    work: Work,
    plan: usize,
    left: u64,
    seed: u64,
    worker: u32,
}

impl<'a> Burner<'a> {
    pub fn new(isa: Isa, plans: &'a [Plan], worker: u64) -> Self {
        let max = plans.iter().map(|p| p.log2n).max().expect("no FFT sizes");
        let plan = worker as usize % plans.len();
        Burner { isa, plans, work: Work::new(max), plan, left: SLICE_FLOPS, seed: worker << 40, worker: worker as u32 }
    }

    pub fn current(&self) -> &Plan {
        &self.plans[self.plan]
    }

    pub fn step(&mut self, stats: &BurnStats) {
        let plan = &self.plans[self.plan];
        self.seed += 1;
        let fault = (stats.inject.load(Ordering::Relaxed) && stats.inject.swap(false, Ordering::Relaxed))
            .then_some((plan.n / 3, 1u64 << 51));
        let o = run(self.isa, plan, &mut self.work, self.seed, fault);
        stats.record(plan, self.worker, self.seed, &o);
        self.left = self.left.saturating_sub(plan.flops());
        if let Some(l) = stats.lanes.get(self.worker as usize) {
            l.record(plan, &o, SLICE_FLOPS - self.left);
        }
        if self.left == 0 {
            self.plan = (self.plan + 1) % self.plans.len();
            self.left = SLICE_FLOPS;
        }
    }

    pub fn run(&mut self, stats: &BurnStats, stop: &AtomicBool) {
        while !stop.load(Ordering::Relaxed) {
            self.step(stats);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn isas() -> Vec<Isa> {
        let mut v = vec![Isa::Sse2];
        if crate::cpu::best_isa() == Isa::Avx2Fma {
            v.push(Isa::Avx2Fma);
        }
        v
    }

    fn naive_square(d: &[i64]) -> Vec<i128> {
        let n = d.len();
        let mut c = vec![0i128; n];
        for i in 0..n {
            for j in 0..n {
                c[(i + j) % n] += d[i] as i128 * d[j] as i128;
            }
        }
        c
    }

    #[test]
    fn matches_naive_convolution() {
        for isa in isas() {
            for log2n in [4, 5, 6, 9] {
                let plan = Plan::new(log2n);
                let mut w = Work::new(log2n);
                let seed = 42 + log2n as u64;
                let o = square_mod(isa, &plan, &mut w, seed);
                assert!(o.ok(), "{isa:?} n=2^{log2n}: {o:?}");
                let half = 1i64 << (plan.bits - 1);
                let mask = (1u64 << plan.bits) - 1;
                let mut rng = Rng::new(seed);
                let d: Vec<i64> = (0..plan.n).map(|_| (rng.next_u64() & mask) as i64 - half).collect();
                let got: Vec<i128> =
                    w.re.bits_mut()[..plan.n].iter().map(|&b| b.wrapping_sub(MAGIC) as i64 as i128).collect();
                assert_eq!(got, naive_square(&d), "{isa:?} n=2^{log2n}");
            }
        }
    }

    #[test]
    fn isas_agree_on_large_sizes() {
        for log2n in [12, 16, 20] {
            let plan = Plan::new(log2n);
            let mut outs = vec![];
            for isa in isas() {
                let mut w = Work::new(log2n);
                assert!(square_mod(isa, &plan, &mut w, 7).ok());
                outs.push(w.re.bits_mut()[..plan.n].to_vec());
            }
            assert!(outs.windows(2).all(|p| p[0] == p[1]), "n=2^{log2n}");
        }
    }

    #[test]
    fn corruption_detected() {
        for isa in isas() {
            let plan = Plan::new(12);
            let mut w = Work::new(12);
            for (i, bit) in [(0, 1u64 << 51), (100, 1 << 45), (4095, 1 << 63), (2048, 0x7FF << 52)] {
                let o = run(isa, &plan, &mut w, 3, Some((i, bit)));
                assert!(!o.ok(), "{isa:?} flip {bit:#x} at {i} missed: {o:?}");
            }
        }
    }

    #[test]
    fn sizes_match_buffers() {
        let (p, w) = (Plan::new(10), Work::new(10));
        assert_eq!(Plan::bytes(10), (p.tw_re.len + p.tw_im.len) * 8);
        assert_eq!(Work::bytes(10), (w.re.len + w.im.len) * 8);
    }

    #[test]
    fn burner_rotates_and_counts() {
        let plans = [Plan::new(6), Plan::new(8)];
        let stats = BurnStats::default();
        let mut b = Burner::new(crate::cpu::best_isa(), &plans, 1);
        b.left = plans[1].flops() * 2;
        for _ in 0..3 {
            b.step(&stats);
        }
        assert_eq!(b.current().log2n, 6);
        assert_eq!(stats.iters.load(Ordering::Relaxed), 3);
        assert_eq!(stats.errors.count(), 0);
    }

    /// Prints the worst rounding error per size; `bits_for` must keep it far below MAX_ERR.
    #[test]
    #[ignore]
    fn error_margin() {
        let isa = crate::cpu::best_isa();
        for log2n in 10..=22 {
            let plan = Plan::new(log2n);
            let mut w = Work::new(log2n);
            let iters = (1u64 << 26 >> log2n).clamp(4, 400);
            let worst = (0..iters).map(|s| square_mod(isa, &plan, &mut w, s).max_err).max().unwrap();
            println!("2^{log2n:2} bits {:2} worst {:.4}", plan.bits, f64::from_bits(worst));
        }
    }
}
