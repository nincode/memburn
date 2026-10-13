//! FFT burn-in per thread and per rounding error: the burners' lanes diffed once per draw tick.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::Ordering::Relaxed;

use super::Color::{self, *};
use super::{Frame, Rect};
use crate::fft::lane::{HIST_BINS, HIST_PER_DEC, hist_pos};
use crate::fft::{BurnStats, Fault, Lane, MAX_ERR};

/// Samples kept per lane and error rows kept.
pub const KEEP: usize = 300;

/// One lane over one sample.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Tick {
    /// Flops of the transforms finished in the sample.
    pub flops: u64,
    pub ms: u32,
    pub log2n: u8,
    pub fault: bool,
}

#[derive(Clone, Default, Debug)]
pub struct LaneView {
    pub log2n: u32,
    /// Flops done of the current size's slice of [`crate::fft::SLICE_FLOPS`].
    pub slice: u64,
    /// Newest last.
    pub ticks: VecDeque<Tick>,
    /// Worst rounding error of the last transform, f64 bits.
    pub last_err: u64,
    /// Faults since the view started, and the last one with its rounding error as f64 bits.
    pub faults: u32,
    pub fault: Option<(Fault, u64)>,
}

impl LaneView {
    /// GFLOP/s over the newest samples covering about `ms`.
    pub fn gflops(&self, ms: u32) -> f64 {
        let (mut f, mut t) = (0u64, 0u32);
        for k in self.ticks.iter().rev() {
            if t >= ms {
                break;
            }
            (f, t) = (f + k.flops, t + k.ms);
        }
        if t == 0 { 0.0 } else { f as f64 / t as f64 / 1e6 }
    }
}

/// Passing transforms of all lanes per error bin over one sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ErrRow {
    pub hist: [u32; HIST_BINS],
    pub faults: u32,
    pub ms: u32,
}

#[derive(Clone, Copy)]
struct Seen {
    ffts: u64,
    flops: u64,
    faults: u32,
    hist: [u32; HIST_BINS],
}

impl Seen {
    fn of(l: &Lane) -> Seen {
        Seen {
            ffts: l.ffts.load(Relaxed),
            flops: l.flops.load(Relaxed),
            faults: l.faults.load(Relaxed),
            hist: core::array::from_fn(|b| l.hist[b].load(Relaxed)),
        }
    }
}

#[derive(Clone)]
pub struct FftView {
    pub lanes: Vec<LaneView>,
    /// Newest last.
    pub rows: VecDeque<ErrRow>,
    /// Since the view started.
    pub all: [u64; HIST_BINS],
    pub ffts: u64,
    pub faults: u64,
    /// Worst passing rounding error, f64 bits.
    pub worst: u64,
    seen: Vec<Seen>,
    at_ms: u64,
}

impl FftView {
    /// Lanes `0..n` of `stats`, counted from now on.
    pub fn new(stats: &BurnStats, n: usize, now_ms: u64) -> FftView {
        let lanes = &stats.lanes[..n.min(stats.lanes.len())];
        FftView {
            lanes: lanes.iter().map(|_| LaneView::default()).collect(),
            rows: VecDeque::new(),
            all: [0; HIST_BINS],
            ffts: 0,
            faults: 0,
            worst: stats.max_err.load(Relaxed),
            seen: lanes.iter().map(Seen::of).collect(),
            at_ms: now_ms,
        }
    }

    /// Adds what the burners did since the last call.
    pub fn sample(&mut self, stats: &BurnStats, now_ms: u64) {
        let ms = now_ms.saturating_sub(self.at_ms).min(u32::MAX as u64) as u32;
        if ms == 0 {
            return;
        }
        self.at_ms = now_ms;
        self.worst = stats.max_err.load(Relaxed);
        let mut row = ErrRow { hist: [0; HIST_BINS], faults: 0, ms };
        for ((l, v), seen) in stats.lanes.iter().zip(&mut self.lanes).zip(&mut self.seen) {
            let now = Seen::of(l);
            let faults = now.faults.wrapping_sub(seen.faults);
            for (b, h) in row.hist.iter_mut().enumerate() {
                *h += now.hist[b].wrapping_sub(seen.hist[b]);
            }
            row.faults += faults;
            self.ffts += now.ffts.wrapping_sub(seen.ffts);
            if v.ticks.len() == KEEP {
                v.ticks.pop_front();
            }
            v.log2n = l.log2n.load(Relaxed);
            let tick = Tick { flops: now.flops.wrapping_sub(seen.flops), ms, log2n: v.log2n as u8, fault: faults > 0 };
            v.ticks.push_back(tick);
            v.slice = l.slice.load(Relaxed);
            v.last_err = l.last_err.load(Relaxed);
            if faults > 0 {
                v.faults += faults;
                v.fault = l.last_fault();
            }
            *seen = now;
        }
        for (a, &h) in self.all.iter_mut().zip(&row.hist) {
            *a += h as u64;
        }
        self.faults += row.faults as u64;
        if self.rows.len() == KEEP {
            self.rows.pop_front();
        }
        self.rows.push_back(row);
    }

    /// GFLOP/s of all lanes over about the last second.
    pub fn gflops(&self) -> f64 {
        self.lanes.iter().map(|l| l.gflops(1000)).sum()
    }

    /// The first lane with a fault, by index.
    pub fn faulty(&self) -> Option<usize> {
        self.lanes.iter().position(|l| l.faults > 0)
    }
}

/// "wrong result", "rounding 3.4e10", "NaN or infinity".
pub fn fault_text(f: Fault, err: u64) -> String {
    match f {
        Fault::WrongResult => String::from("wrong result"),
        Fault::Rounding => format!("rounding {}", sci(f64::from_bits(err))),
        Fault::NonFinite => String::from("NaN or infinity"),
    }
}

/// 0.0213 below 10, else 3.4e10.
pub fn sci(e: f64) -> String {
    if !e.is_finite() {
        return String::from("inf");
    }
    if e < 10.0 {
        return format!("{e:.4}");
    }
    let (mut m, mut p) = (e, 0);
    while m >= 10.0 {
        m /= 10.0;
        p += 1;
    }
    format!("{m:.1}e{p}")
}

/// Headroom to the limit, e.g. "7×".
pub fn margin(worst: f64) -> String {
    if worst <= 0.0 { String::from("-") } else { format!("{:.0}×", MAX_ERR / worst) }
}

/// Size colour in text: small transforms cyan, large ones yellow.
fn size_color(n: u32) -> Color {
    match n {
        0..=13 => Cyan,
        14..=17 => LightCyan,
        _ => Yellow,
    }
}

/// Rule across `r`'s top row with a title and, right-aligned, `value`.
fn rule(f: &mut Frame, r: Rect, title: &str, value: &str, vc: Color) {
    for x in r.x.saturating_sub(1)..r.x + r.w + 1 {
        f.set(x, r.y, '─', DarkGray, Black);
    }
    f.text(r.x + 1, r.y, &format!(" {title} "), r.w.saturating_sub(2), LightGray, Black);
    if !value.is_empty() && value.chars().count() + title.chars().count() + 6 <= r.w {
        f.text_right(r.x + r.w - 1, r.y, &format!(" {value} "), vc, Black);
    }
}

/// Braille column heights 0..=4 as one cell's left and right halves.
fn bars(l: u8, r: u8) -> char {
    const LEFT: [u8; 5] = [0, 0x40, 0x44, 0x46, 0x47];
    const RIGHT: [u8; 5] = [0, 0x80, 0xa0, 0xb0, 0xb8];
    char::from_u32(0x2800 + (LEFT[l.min(4) as usize] | RIGHT[r.min(4) as usize]) as u32).unwrap_or(' ')
}

/// Lane `l`'s newest `cols * 2` samples as braille bars of finished flops, scaled to the lane's mean.
fn activity(f: &mut Frame, x: usize, y: usize, cols: usize, l: &LaneView, c: Color) {
    let n = cols * 2;
    let shown: Vec<u64> = l.ticks.iter().rev().take(n).map(|t| t.flops).collect();
    let mean = (shown.iter().sum::<u64>() as f64 / shown.len().max(1) as f64).max(1.0);
    let h = |k: usize| -> u8 {
        let Some(&fl) = n.checked_sub(k + 1).and_then(|i| shown.get(i)) else { return 0 };
        if fl == 0 { 0 } else { ((fl as f64 / mean * 4.0) as u8).clamp(1, 4) }
    };
    for k in 0..cols {
        f.set(x + k, y, bars(h(2 * k), h(2 * k + 1)), c, Black);
    }
}

/// Fewest rows [`lanes`] shows all `n` threads in, `w` columns wide: a title row and the compact grid.
pub(super) fn lanes_rows(n: usize, w: usize) -> usize {
    1 + n.div_ceil((w / 9).max(1))
}

/// The thread lanes in `r`: one row per thread, or a compact grid when `r` is short or narrow.
pub(super) fn lanes(f: &mut Frame, v: &FftView, r: Rect) {
    if r.h < 2 || r.w < 16 {
        return;
    }
    let (value, vc) = match v.faulty() {
        Some(i) => (format!("CPU {i} fault"), LightRed),
        None => (format!("{:.1} GFLOP/s", v.gflops()), Yellow),
    };
    rule(f, r, "Threads", &value, vc);
    let (n, rows) = (v.lanes.len(), r.h - 1);
    let cols = n.div_ceil(rows);
    if r.w >= 40 * cols {
        let cw = r.w / cols;
        for (i, l) in v.lanes.iter().enumerate() {
            let (x, y) = (r.x + (i / rows) * cw, r.y + 1 + i % rows);
            let bad = l.faults > 0;
            f.text(x, y, &format!("CPU {i}"), 7, if bad { LightRed } else { LightGray }, Black);
            f.text(x + 7, y, &format!("2^{}", l.log2n), 5, size_color(l.log2n), Black);
            let sw = cw.saturating_sub(24 + usize::from(cols > 1));
            activity(f, x + 12, y, sw, l, if bad { LightRed } else { size_color(l.log2n) });
            let tail = match l.fault {
                Some((k, e)) if bad => fault_text(k, e),
                _ => format!("{:>4.1} GF", l.gflops(1000)),
            };
            f.text(x + 13 + sw, y, &tail, 10, if bad { LightRed } else { DarkGray }, Black);
        }
        return;
    }
    let per = n.div_ceil(rows).max(r.w / 9).min(n.max(1));
    let cw = r.w / per;
    for (i, l) in v.lanes.iter().enumerate().take(per * rows) {
        let (x, y) = (r.x + (i % per) * cw, r.y + 1 + i / per);
        let bad = l.faults > 0;
        f.text(x, y, &format!("{i:>2}"), 3, if bad { LightRed } else { DarkGray }, Black);
        f.text(x + 3, y, &format!("{:>2}", l.log2n), 2, if bad { LightRed } else { size_color(l.log2n) }, Black);
        activity(f, x + 6, y, cw.saturating_sub(7), l, if bad { LightRed } else { size_color(l.log2n) });
    }
}

/// Text rows each waterfall row should cover, ms.
const ROW_MS: u32 = 200;

/// The error waterfall in `r`: newest row on top, one column band per error bin, faults on the right.
pub(super) fn waterfall(f: &mut Frame, v: &FftView, r: Rect) {
    if r.h < 4 || r.w < 24 {
        return;
    }
    let (value, vc) = if v.faults > 0 {
        (format!("{} fault{}", v.faults, if v.faults == 1 { "" } else { "s" }), LightRed)
    } else {
        (format!("worst {:.4}", f64::from_bits(v.worst)), Yellow)
    };
    rule(f, r, "Error distribution", &value, vc);
    let (gh, gw) = (r.h - 2, r.w - 3);
    let mean_ms = v.rows.iter().map(|r| r.ms as u64).sum::<u64>() / v.rows.len().max(1) as u64;
    let per = (ROW_MS as u64).div_ceil(mean_ms.max(1)).max(1) as usize;
    let lim = (hist_pos(MAX_ERR) / HIST_BINS as f32 * gw as f32) as usize;
    let newest = v.rows.len();
    for i in 0..gh {
        let y = r.y + 1 + i;
        let end = newest.saturating_sub(i * per);
        let rows = v.rows.range(end.saturating_sub(per)..end);
        let mut hist = [0u32; HIST_BINS];
        let mut faults = 0;
        for row in rows {
            for (a, &h) in hist.iter_mut().zip(&row.hist) {
                *a += h;
            }
            faults += row.faults;
        }
        let cells: Vec<u32> = (0..gw)
            .map(|c| {
                let lo = c * HIST_BINS / gw;
                let hi = ((c + 1) * HIST_BINS / gw).max(lo + 1).min(HIST_BINS);
                hist[lo..hi].iter().sum()
            })
            .collect();
        let peak = cells.iter().copied().max().unwrap_or(0).max(1);
        for (c, &n) in cells.iter().enumerate() {
            let k = if n == 0 { 0.0 } else { libm::log1pf(n as f32) / libm::log1pf(peak as f32) };
            let (ch, col) = match k {
                0.0 if c == lim => ('│', Red),
                0.0 => (' ', LightGray),
                k if k < 0.2 => ('·', DarkGray),
                k if k < 0.4 => ('░', Brown),
                k if k < 0.6 => ('░', Yellow),
                k if k < 0.8 => ('█', Brown),
                _ => ('█', Yellow),
            };
            f.set(r.x + c, y, ch, col, Black);
        }
        if faults > 0 {
            f.text(r.x + gw + 1, y, "██", 2, LightRed, Black);
        } else if end > 0 {
            f.set(r.x + gw + 1, y, '·', DarkGray, Black);
        }
    }
    let y = r.y + r.h - 1;
    for (d, s) in [(0, "1e-5"), (2, "0.001"), (4, "0.1")] {
        f.text(r.x + d * HIST_PER_DEC * gw / HIST_BINS, y, s, 6, DarkGray, Black);
    }
    f.text(r.x + lim, y, "0.4", 3, Red, Black);
    f.text_right(r.x + r.w, y, "fault", if v.faults > 0 { LightRed } else { DarkGray }, Black);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fft::{Burner, Plan};
    use core::sync::atomic::AtomicBool;

    #[test]
    fn samples_diff_the_lanes_into_ticks_and_rows() {
        let stats = BurnStats::with_lanes(2);
        let plans = [Plan::new(10)];
        let (mut a, mut b) =
            (Burner::new(crate::cpu::Isa::Sse2, &plans, 0), Burner::new(crate::cpu::Isa::Sse2, &plans, 1));
        a.step(&stats);
        let mut v = FftView::new(&stats, 2, 1000);
        for _ in 0..3 {
            a.step(&stats);
        }
        b.step(&stats);
        v.sample(&stats, 1100);
        let fl = plans[0].flops();
        assert_eq!(v.lanes[0].ticks.back(), Some(&Tick { flops: 3 * fl, ms: 100, log2n: 10, fault: false }));
        assert_eq!(v.lanes[1].ticks.back(), Some(&Tick { flops: fl, ms: 100, log2n: 10, fault: false }));
        assert_eq!(v.ffts, 4);
        assert_eq!(v.rows.back().unwrap().hist.iter().sum::<u32>(), 4);
        assert_eq!(v.all.iter().sum::<u64>(), 4);
        assert!((v.lanes[0].gflops(1000) - 3.0 * fl as f64 / 1e8).abs() < 1e-9);
        assert_eq!(v.lanes[0].log2n, 10);
        v.sample(&stats, 1100);
        assert_eq!(v.rows.len(), 1, "a zero-length sample adds nothing");
        stats.inject.store(true, Relaxed);
        b.step(&stats);
        v.sample(&stats, 1350);
        assert_eq!((v.faults, v.lanes[1].faults, v.lanes[0].faults), (1, 1, 0));
        assert!(v.lanes[1].ticks.back().unwrap().fault);
        assert!(v.lanes[1].fault.is_some());
        assert_eq!(v.rows.back().unwrap().faults, 1);
        assert_eq!(v.faulty(), Some(1));
        let stop = AtomicBool::new(true);
        a.run(&stats, &stop);
    }

    #[test]
    fn rings_keep_the_newest() {
        let stats = BurnStats::with_lanes(1);
        let mut v = FftView::new(&stats, 1, 0);
        for t in 1..=KEEP as u64 + 5 {
            v.sample(&stats, t * 10);
        }
        assert_eq!((v.rows.len(), v.lanes[0].ticks.len()), (KEEP, KEEP));
    }

    #[test]
    fn sci_and_margin() {
        assert_eq!(sci(0.0213), "0.0213");
        assert_eq!(sci(3.4e10), "3.4e10");
        assert_eq!(sci(f64::INFINITY), "inf");
        assert_eq!(margin(0.05), "8×");
    }
}
