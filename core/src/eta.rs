//! Time left in memory tests: the bus traffic still to move over a smoothed traffic rate.

use crate::memtest::TestId;

/// Rough time to move one GiB of test traffic ([`TestId::traffic`] units), ms.
pub const TRAFFIC_MS_PER_GIB: u64 = 29;
/// Smoothing window of the traffic rate, ms.
const WINDOW_MS: u64 = 10_000;
/// No estimate before this much measured time, ms.
const WARMUP_MS: u64 = 3_000;
/// A longer gap between samples (a bit-fade hold, a new allocation) is not measured, ms.
const GAP_MS: u64 = 2_000;
/// The shown time jumps to a new estimate only when they differ by more than this, ms, or [`JUMP_PCT`].
const JUMP_MS: u64 = 5_000;
const JUMP_PCT: u64 = 5;

/// Traffic bytes of `rounds` rounds of `t` over `bytes` tested bytes, counted as the engine counts it.
pub fn test_traffic(t: TestId, rounds: u32, bytes: u64) -> u64 {
    let one: u64 = (0..t.phases())
        .map(|ph| {
            let (num, den) = t.traffic(ph);
            bytes * num / den
        })
        .sum();
    one * rounds as u64
}

/// Traffic bytes of one pass over (test, rounds).
pub fn pass_traffic(tests: &[(TestId, u32)], bytes: u64) -> u64 {
    tests.iter().map(|&(t, r)| test_traffic(t, r, bytes)).sum()
}

/// Bit-fade holds in `rounds` rounds of `t`.
pub fn holds(t: TestId, rounds: u32) -> u64 {
    (0..t.phases()).filter(|&ph| t.pause_after(ph)).count() as u64 * rounds as u64
}

/// Planned time of one pass over `gib` GiB at [`TRAFFIC_MS_PER_GIB`], ms.
pub fn pass_ms(tests: &[(TestId, u32)], gib: f64) -> u64 {
    (pass_traffic(tests, 1 << 30) as f64 / (1u64 << 30) as f64 * gib * TRAFFIC_MS_PER_GIB as f64) as u64
}

/// Traffic rate from (ms, traffic) samples: the average while under [`WINDOW_MS`] measured, then an
/// exponential average over about [`WINDOW_MS`].
#[derive(Default, Clone)]
pub struct Rate {
    last: Option<(u64, u64)>,
    measured_ms: u64,
    moved: u64,
    /// Bytes per ms.
    ema: f64,
}

impl Rate {
    /// Adds the traffic counter `traffic` read at `now_ms`; a counter that went back starts a new series.
    pub fn sample(&mut self, now_ms: u64, traffic: u64) {
        let Some((t0, b0)) = self.last else {
            self.last = Some((now_ms, traffic));
            return;
        };
        self.last = Some((now_ms, traffic));
        let dt = now_ms.saturating_sub(t0);
        if dt == 0 || dt > GAP_MS || traffic < b0 {
            return;
        }
        let db = traffic - b0;
        let now = db as f64 / dt as f64;
        self.ema =
            if self.measured_ms == 0 { now } else { self.ema + (now - self.ema) * dt as f64 / (WINDOW_MS + dt) as f64 };
        self.measured_ms += dt;
        self.moved += db;
    }

    /// Forgets the previous sample, so the next one starts a new interval (after a pause).
    pub fn pause(&mut self) {
        self.last = None;
    }

    /// Bytes per ms, once [`WARMUP_MS`] are measured.
    pub fn per_ms(&self) -> Option<f64> {
        if self.measured_ms < WARMUP_MS || self.moved == 0 {
            return None;
        }
        Some(if self.measured_ms < WINDOW_MS { self.moved as f64 / self.measured_ms as f64 } else { self.ema })
    }

    /// Time to move `traffic` bytes, ms.
    pub fn ms(&self, traffic: u64) -> Option<u64> {
        self.per_ms().map(|r| (traffic as f64 / r) as u64)
    }
}

/// A shown time left that counts down by itself and takes a new estimate only when it moved more than
/// [`JUMP_PCT`] % or [`JUMP_MS`].
#[derive(Default, Clone)]
pub struct Shown {
    /// (when, time left then), ms.
    at: Option<(u64, u64)>,
}

impl Shown {
    pub fn update(&mut self, now_ms: u64, estimate: Option<u64>) -> Option<u64> {
        let est = estimate?;
        if let Some((t, left)) = self.at {
            let counted = left.saturating_sub(now_ms.saturating_sub(t));
            if counted.abs_diff(est) <= JUMP_MS.max(est * JUMP_PCT / 100) {
                return Some(counted);
            }
        }
        self.at = Some((now_ms, est));
        Some(est)
    }

    pub fn reset(&mut self) {
        self.at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memtest::{DEFAULT, QUICK, sweeps};
    use alloc::vec::Vec;

    const GIB: u64 = 1 << 30;

    #[test]
    fn pass_traffic_against_sweeps() {
        assert_eq!(pass_traffic(&QUICK, GIB), (7 + 48 + 25 + 3) * GIB);
        for tests in [QUICK.to_vec(), DEFAULT.iter().map(|&t| (t, 1)).collect::<Vec<_>>()] {
            let phases: Vec<(TestId, usize)> = tests
                .iter()
                .flat_map(|&(t, r)| (0..r).flat_map(move |_| (0..t.phases()).map(move |p| (t, p))))
                .collect();
            assert_eq!(phases.len() as u64, sweeps(&tests));
            let sum: u64 = phases
                .iter()
                .map(|&(t, p)| {
                    let (n, d) = t.traffic(p);
                    GIB * n / d
                })
                .sum();
            assert_eq!(pass_traffic(&tests, GIB), sum);
            assert!(pass_traffic(&tests, GIB) >= sweeps(&tests) * GIB);
        }
        assert_eq!(holds(TestId::BitFade, 2), 4);
        assert_eq!(holds(TestId::Hashed, 1), 0);
    }

    /// Run G56259RN (5950X, 30.3 GiB quick check): per-test seconds; the estimate after test 1 is within 10 %.
    #[test]
    fn replay_of_a_quick_check() {
        let bytes = 30_300 * GIB / 1000;
        let secs = [6.0, 43.9, 22.1, 2.6];
        let mut rate = Rate::default();
        let mut shown = Shown::default();
        let (mut now, mut moved) = (0u64, 0u64);
        let total = pass_traffic(&QUICK, bytes);
        let all_ms: u64 = secs.iter().map(|s| (s * 1000.0) as u64).sum();
        let mut checked = false;
        for (k, (&(t, r), s)) in QUICK.iter().zip(secs).enumerate() {
            let traffic = test_traffic(t, r, bytes);
            let ms = (s * 1000.0) as u64;
            let start = moved;
            for step in 1..=ms / 250 {
                now += 250;
                moved = start + traffic * step * 250 / ms;
                rate.sample(now, moved);
                let est = rate.ms(total - moved);
                let left = shown.update(now, est);
                if now < WARMUP_MS {
                    assert_eq!(left, None);
                }
            }
            if k == 0 {
                let est = rate.ms(total - moved).unwrap() as f64;
                let actual = (all_ms - now) as f64;
                assert!((est - actual).abs() < actual * 0.1, "after test 1: {est} vs {actual}");
                checked = true;
            }
        }
        assert!(checked);
    }

    #[test]
    fn shown_counts_down_between_jumps() {
        let mut s = Shown::default();
        assert_eq!(s.update(0, None), None);
        assert_eq!(s.update(0, Some(100_000)), Some(100_000));
        assert_eq!(s.update(1_000, Some(101_000)), Some(99_000));
        assert_eq!(s.update(2_000, Some(110_000)), Some(110_000));
        assert_eq!(s.update(3_000, Some(1_000)), Some(1_000));
    }

    #[test]
    fn gaps_and_resets_are_not_measured() {
        let mut r = Rate::default();
        for k in 0..=20 {
            r.sample(k * 250, k * 250 * 1000);
        }
        assert_eq!(r.per_ms(), Some(1000.0));
        r.sample(60_000, 5_100_000);
        assert_eq!(r.per_ms(), Some(1000.0));
        r.sample(60_250, 0);
        r.sample(60_500, 500_000);
        assert!(r.per_ms().unwrap() > 1000.0);
        assert_eq!(pass_ms(&QUICK, 30.0), (83.0 * 30.0 * 29.0) as u64);
    }
}
