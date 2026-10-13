//! FFT burn-in cards: thread lanes and the error waterfall.

use alloc::format;
use alloc::string::String;

use memburn_core::fft::MAX_ERR;
use memburn_core::fft::SLICE_FLOPS;
use memburn_core::fft::lane::{HIST_BINS, HIST_PER_DEC, hist_pos};
use memburn_core::tui::{FftView, LaneView, fault_text, group, margin};

use super::*;

/// Empty bin and fault cell.
const VOID: u32 = 0x2c2b28;

/// Small transforms cyan, large ones yellow, over 2^10..2^21.
fn size_color(log2n: u32) -> u32 {
    let k = (log2n.saturating_sub(10) as f32 / 11.0).clamp(0.0, 1.0);
    if k < 0.5 {
        mix(CYAN_DIM, CYAN, (k * 2.0 * 255.0) as u32)
    } else {
        mix(CYAN, YELLOW, ((k - 0.5) * 2.0 * 255.0) as u32)
    }
}

fn vline(d: &mut D, x: f32, y: f32, h: f32, c: u32) {
    let (x0, y0, y1, t) = (d.px(x), d.px(y), d.px(y + h), d.t());
    d.pen.rect(x0, y0, t, y1 - y0, c);
}

/// Seconds the newest `n` samples of `l` cover.
fn span_s(l: &LaneView, n: usize) -> f32 {
    l.ticks.iter().rev().take(n).map(|t| t.ms).sum::<u32>() as f32 / 1000.0
}

/// One row per thread: size, slice progress, finished flops per sample, GFLOP/s and the last error or fault.
pub(super) fn lanes(v: &FftView, d: &mut D, (x, y, w, h): (f32, f32, f32, f32)) {
    d.card(x, y, w, h, "Threads");
    let n = v.lanes.len().max(1);
    let (top, avail) = (y + 40.0, h - 40.0 - 12.0);
    let per_col = ((avail / 9.0) as usize).max(1);
    let ncol = n.div_ceil(per_col);
    let rows = n.div_ceil(ncol);
    let pitch = (avail / rows as f32).min(22.0);
    let cw = (w - 36.0 - 24.0 * (ncol - 1) as f32) / ncol as f32;
    let wide = cw >= 520.0;
    let tick_pt = 3.0;
    let mut shown_s = 0.0f32;
    for (i, l) in v.lanes.iter().enumerate() {
        let (lx, ry) = (x + 18.0 + (i / rows) as f32 * (cw + 24.0), top + (i % rows) as f32 * pitch);
        let bad = l.faults > 0;
        if bad {
            d.rrect(lx - 10.0, ry, cw + 20.0, pitch - 1.0, 3.0, RED_ROW);
        }
        let base = ry + pitch * 0.78;
        d.text(Mono, 10.5, lx, base, &format!("CPU {i}"), if bad { RED } else { MID }, 50.0);
        d.text(MonoBold, 10.5, lx + 52.0, base, &format!("2^{}", l.log2n), size_color(l.log2n), 44.0);
        let mut sx = lx + 98.0;
        if wide {
            let frac = (l.slice as f32 / SLICE_FLOPS as f32).clamp(0.0, 1.0);
            let (by, bh) = (ry + pitch * 0.35, (pitch * 0.3).max(2.0));
            d.rrect(sx, by, 60.0, bh, 1.5, PENDING);
            d.rrect(sx, by, (60.0 * frac).max(2.0), bh, 1.5, SOFT);
            sx += 78.0;
        }
        let right = if wide { 200.0 } else { 130.0 };
        let sw = (lx + cw - right - 14.0 - sx).max(0.0);
        let k = (sw / tick_pt) as usize;
        shown_s = shown_s.max(span_s(l, k));
        let ticks = || l.ticks.iter().rev().take(k);
        let mean = (ticks().map(|t| t.flops).sum::<u64>() as f32 / ticks().count().max(1) as f32).max(1.0);
        let lane_h = (pitch - 3.0).max(2.0);
        let bottom = ry + pitch - 1.5;
        for (j, t) in ticks().enumerate() {
            if t.flops == 0 {
                continue;
            }
            let hh = (lane_h * (t.flops as f32 / mean * 0.5).min(1.0)).max(1.0);
            let c = if t.fault { RED } else { size_color(t.log2n as u32) };
            let tx = sx + sw - (j + 1) as f32 * tick_pt;
            d.rect(tx, bottom - hh, tick_pt - 0.5, hh, c);
        }
        let glow = match (l.ticks.back(), l.ticks.iter().rev().nth(1)) {
            (Some(t), _) if t.flops > 0 => 255,
            (_, Some(t)) if t.flops > 0 => 120,
            _ => 0,
        };
        d.circle(sx + sw + 8.0, ry + pitch / 2.0, 3.0, mix(PENDING, INK, glow), 255);
        let rx = lx + cw;
        let (et, ec) = match l.fault {
            Some((f, e)) if bad => (fault_text(f, e), RED),
            _ => (format!("{:.4}", f64::from_bits(l.last_err)), SOFT),
        };
        if wide {
            d.text_r(Mono, 10.5, rx, base, &et, ec);
            d.text_r(Mono, 10.5, rx - 110.0, base, &format!("{:.1} GF", l.gflops(1000)), MID);
        } else {
            let s = if bad { et } else { format!("{:.1} GF", l.gflops(1000)) };
            d.text_r(Mono, 10.5, rx, base, &s, if bad { RED } else { MID });
        }
    }
    if shown_s > 0.0 {
        d.text_r(Sans, 12.0, x + w - 18.0, y + 26.0, &format!("last {shown_s:.0} s"), MID);
    }
}

/// Rows of rounding-error bins, newest on top and each to its own peak, the whole burn-in below, figures beside.
pub(super) fn waterfall(v: &FftView, d: &mut D, (x, y, w, h): (f32, f32, f32, f32)) {
    d.card(x, y, w, h, "Error distribution");
    let side = if w >= 480.0 { 150.0 } else { 0.0 };
    let (ax, py) = (x + 18.0 + 34.0, y + 44.0);
    let pw = w - 36.0 - 34.0 - side - if side > 0.0 { 20.0 } else { 0.0 };
    let prof_h = (h * 0.14).clamp(16.0, 34.0);
    let ph = h - 44.0 - 30.0 - prof_h - 8.0;
    let fcol = 26.0;
    let bw = (pw - fcol - 6.0) / HIST_BINS as f32;
    let row_pt = 2.5;
    let rows = (ph / row_pt).max(1.0) as usize;
    let failing = v.faults > 0;
    let ramp = |k: f32| -> u32 {
        if k < 0.33 {
            mix(VOID, AMBER_FILL, (k / 0.33 * 255.0) as u32)
        } else if k < 0.75 {
            mix(AMBER_FILL, YELLOW, ((k - 0.33) / 0.42 * 255.0) as u32)
        } else {
            mix(YELLOW, INK, ((k - 0.75) / 0.25 * 255.0) as u32)
        }
    };
    let mut span_ms = 0u64;
    for (i, row) in v.rows.iter().rev().take(rows).enumerate() {
        span_ms += row.ms as u64;
        let peak = row.hist.iter().copied().max().unwrap_or(0).max(1);
        let ry = py + i as f32 * row_pt;
        for (b, &c) in row.hist.iter().enumerate() {
            let col = if c == 0 { VOID } else { ramp((libm::log1pf(c as f32) / libm::log1pf(peak as f32)).min(1.0)) };
            d.rect(ax + b as f32 * bw, ry, bw - 0.6, row_pt - 0.5, col);
        }
        d.rect(ax + pw - fcol, ry, fcol, row_pt - 0.5, if row.faults > 0 { RED } else { VOID });
    }
    let lx = ax + bw * hist_pos(MAX_ERR);
    vline(d, lx, py - 4.0, ph + 8.0 + prof_h, RED_FILL);
    d.text(Mono, 10.5, lx + 5.0, py + 10.0, "limit 0.4", RED, 80.0);
    let base = py + ph + 8.0 + prof_h;
    let mx = v.all.iter().copied().max().unwrap_or(0).max(1);
    for (b, &c) in v.all.iter().enumerate() {
        let k = libm::log1pf(c as f32) / libm::log1pf(mx as f32);
        let bh = prof_h * k;
        d.rect(ax + b as f32 * bw, base - bh, bw - 0.6, bh, if c > 0 { mix(CARD, YELLOW, 170) } else { CARD });
    }
    if failing {
        d.rect(ax + pw - fcol, base - prof_h, fcol, prof_h, RED);
    }
    d.hline(ax, base, pw, HAIR);
    for (k, lab) in ["1e-5", "1e-4", "0.001", "0.01", "0.1", "1"].iter().enumerate() {
        let xx = ax + (k * HIST_PER_DEC) as f32 * bw;
        vline(d, xx, base, 4.0, HAIR);
        if bw * HIST_PER_DEC as f32 >= 34.0 || k % 2 == 0 {
            d.text(Mono, 10.0, xx - 2.0, base + 16.0, lab, DIM, 50.0);
        }
    }
    d.text(Mono, 10.0, ax + pw - fcol - 2.0, base + 16.0, "fault", if failing { RED } else { DIM }, 40.0);
    d.text_r(Mono, 10.0, ax - 6.0, py + 8.0, "now", DIM);
    if span_ms > 0 {
        d.text_r(Mono, 10.0, ax - 6.0, py + ph, &format!("−{} s", (span_ms + 500) / 1000), DIM);
    }
    d.text_r(Mono, 10.0, ax - 6.0, base - 4.0, "all", DIM);
    if side == 0.0 {
        return;
    }
    let sx = x + w - 18.0 - side;
    let worst = f64::from_bits(v.worst);
    d.text(SansBold, 11.0, sx, py + 10.0, "FFTS CHECKED", MID, side);
    d.text(MonoBold, 22.0, sx, py + 38.0, &group(v.ffts), INK, side);
    d.text(SansBold, 11.0, sx, py + 68.0, "WORST", MID, side);
    d.text(MonoBold, 22.0, sx, py + 96.0, &format!("{worst:.4}"), INK, side);
    d.text(Sans, 13.0, sx, py + 118.0, &format!("margin {}", margin(worst)), SOFT, side);
    if failing {
        let s: String = format!("{} fault{}", group(v.faults), if v.faults == 1 { "" } else { "s" });
        d.text(SansBold, 13.0, sx, py + 146.0, &s, RED, side);
    }
}
