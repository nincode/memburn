//! Core cycler: small FFTs on one logical CPU at a time, alternating instruction sets, with idle gaps.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::cpu::Isa;
use crate::fft::{BurnStats, Burner, Fault, Plan};

/// FFT sizes, log2: small enough to stay in L1/L2 and load one core hard.
pub const SIZES: [u32; 4] = [10, 11, 12, 13];
/// Each core's turn idles this long at the end of every minute.
pub const IDLE_MS: u64 = 5_000;
const MINUTE_MS: u64 = 60_000;
/// Instruction set switches this often while running.
pub const ISA_MS: u64 = 3_000;
pub const DEFAULT_MINUTES: f64 = 2.0;

/// AVX2+FMA then SSE2 when the CPU has AVX2, else SSE2 only.
pub fn isas(best: Isa) -> Vec<Isa> {
    if best == Isa::Avx2Fma { vec![Isa::Avx2Fma, Isa::Sse2] } else { vec![Isa::Sse2] }
}

/// Index into `isas` to run at `ms` into a turn, or None while idling.
pub fn phase(ms: u64, isas: usize) -> Option<usize> {
    if ms % MINUTE_MS >= MINUTE_MS - IDLE_MS {
        return None;
    }
    Some((ms / ISA_MS) as usize % isas.max(1))
}

/// `phase` as stored in a turn's state word: 0 idle, else isa index + 1.
pub fn state_word(p: Option<usize>) -> u32 {
    p.map_or(0, |i| i as u32 + 1)
}

/// Logical CPU numbers sorted by APIC id, so sibling threads of a core follow each other.
pub fn order_by_apic(apic: &[(usize, u64)]) -> Vec<usize> {
    let mut v = apic.to_vec();
    v.sort_by_key(|&(n, id)| (id, n));
    v.into_iter().map(|(n, _)| n).collect()
}

/// One burner per instruction set, its errors attributed to logical CPU `cpu`.
pub fn burners<'a>(isas: &[Isa], plans: &'a [Plan], cpu: usize) -> Vec<Burner<'a>> {
    isas.iter().map(|&isa| Burner::new(isa, plans, cpu as u64)).collect()
}

/// Runs one core's turn for `turn_ms`: FFTs on `burners` by [`phase`], `state` updated as [`state_word`].
/// `after(idle)` follows every FFT and, in the gaps, waits a little. Call it on the CPU under test.
pub fn run_turn(
    burners: &mut [Burner],
    stats: &BurnStats,
    stop: &AtomicBool,
    turn_ms: u64,
    now_ms: &dyn Fn() -> u64,
    after: &mut dyn FnMut(bool),
    state: &AtomicU32,
) {
    let t0 = now_ms();
    loop {
        let t = now_ms().saturating_sub(t0);
        if t >= turn_ms || stop.load(Ordering::Relaxed) {
            break;
        }
        let p = phase(t, burners.len());
        state.store(state_word(p), Ordering::Relaxed);
        if let Some(i) = p {
            burners[i].step(stats);
        }
        after(p.is_none());
    }
    state.store(0, Ordering::Relaxed);
}

/// A core's result so far.
#[derive(Clone, Debug, PartialEq)]
pub struct CoreResult {
    pub cpu: usize,
    pub label: String,
    pub ms: u64,
    pub ffts: u64,
    pub errors: u64,
    pub fault: Option<Fault>,
}

impl CoreResult {
    pub fn new(cpu: usize, label: String) -> CoreResult {
        CoreResult { cpu, label, ms: 0, ffts: 0, errors: 0, fault: None }
    }

    /// Plan row name: "CPU 3" or "CPU 3: wrong result".
    pub fn row(&self) -> String {
        match self.fault {
            None => self.label.clone(),
            Some(f) => format!("{}: {}", self.label, fault_name(f)),
        }
    }
}

pub fn fault_name(f: Fault) -> &'static str {
    match f {
        Fault::WrongResult => "wrong result",
        Fault::Rounding => "rounding error",
        Fault::NonFinite => "NaN or infinity",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fft::BurnError;
    use core::cell::Cell;

    #[test]
    fn phases_alternate_and_idle_each_minute() {
        assert_eq!(phase(0, 2), Some(0));
        assert_eq!(phase(ISA_MS, 2), Some(1));
        assert_eq!(phase(2 * ISA_MS, 2), Some(0));
        assert_eq!(phase(4_000, 1), Some(0));
        assert_eq!(phase(53_999, 2), Some(1));
        assert_eq!(phase(55_000, 2), None);
        assert_eq!(phase(59_999, 2), None);
        assert_eq!(phase(60_000, 2), Some(0));
        let idle = (0..120_000).step_by(1000).filter(|&t| phase(t, 2).is_none()).count();
        assert_eq!(idle, 10);
        assert_eq!(order_by_apic(&[(0, 0), (1, 2), (2, 1), (3, 3)]), vec![0, 2, 1, 3]);
    }

    /// Runs `cycles` cycles over `order`, injecting a fault during `inject_on`'s turns; returns turns per CPU and errors.
    fn cycle(order: &[usize], cycles: usize, inject_on: usize) -> (Vec<usize>, Vec<BurnError>) {
        let plans: Vec<Plan> = [6, 7].iter().map(|&s| Plan::new(s)).collect();
        let stats = BurnStats::default();
        let stop = AtomicBool::new(false);
        let state = AtomicU32::new(0);
        let mut turns = vec![0; order.len()];
        for _ in 0..cycles {
            for &cpu in order {
                turns[cpu] += 1;
                let mut b = burners(&isas(crate::cpu::best_isa()), &plans, cpu);
                if cpu == inject_on {
                    stats.inject.store(true, Ordering::Relaxed);
                }
                let clock = Cell::new(0u64);
                let now = || {
                    clock.set(clock.get() + 100);
                    clock.get()
                };
                run_turn(&mut b, &stats, &stop, 10_000, &now, &mut |_| {}, &state);
            }
        }
        let mut errs = [BurnError { worker: 0, log2n: 0, seed: 0, fault: Fault::WrongResult, max_err: 0 }; 16];
        let (n, _) = stats.errors.since(0, &mut errs);
        (turns, errs[..n].to_vec())
    }

    #[test]
    fn every_cpu_once_per_cycle_and_faults_name_their_cpu() {
        let order = order_by_apic(&[(0, 0), (1, 4), (2, 2), (3, 6)]);
        for k in 0..4 {
            let (turns, errs) = cycle(&order, 2, k);
            assert_eq!(turns, vec![2; 4]);
            assert_eq!(errs.len(), 2, "one injected fault per cycle");
            assert!(errs.iter().all(|e| e.worker as usize == k), "{errs:?}");
        }
        let mut r = CoreResult::new(3, String::from("CPU 3"));
        assert_eq!(r.row(), "CPU 3");
        r.fault = Some(Fault::WrongResult);
        assert_eq!(r.row(), "CPU 3: wrong result");
    }
}
