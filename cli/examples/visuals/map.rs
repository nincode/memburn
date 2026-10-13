//! Option 1: a live map of physical memory drawn in text cells.
use Color::*;
use memburn_core::memtest::TestId;
use memburn_core::tui::{Color, Frame};

use crate::raster::{R, put, text};
use crate::scene::{self, CLUSTERS, GIB, S, TOP};

fn rank(s: S) -> u8 {
    match s {
        S::Hole => 0,
        S::Pending => 1,
        S::Done => 2,
        S::Head => 3,
        S::Error => 4,
        S::Fresh => 5,
    }
}

fn color(s: S, t: f64) -> Color {
    match s {
        S::Hole | S::Pending => Black,
        S::Done => Cyan,
        S::Head => White,
        S::Error => LightRed,
        S::Fresh => {
            if ((t * 5.0) as u32).is_multiple_of(2) {
                White
            } else {
                LightRed
            }
        }
    }
}

/// Nearest of the 8 background colours.
fn as_bg(c: Color) -> Color {
    match c {
        DarkGray => Black,
        White => LightGray,
        LightRed => Red,
        LightCyan => Cyan,
        c if (c as u8) < 8 => c,
        _ => Black,
    }
}

/// One cell holding two samples: `first` is the top (or left) half.
fn pair(f: &mut Frame, x: usize, y: usize, a: S, b: S, t: f64, vertical: bool) {
    let (ca, cb) = (color(a, t), color(b, t));
    if a == S::Hole && b == S::Hole {
        return put(f, x, y, '░', DarkGray, Black);
    }
    if matches!(a, S::Error | S::Fresh) || matches!(b, S::Error | S::Fresh) {
        let c = if a == S::Fresh || b == S::Fresh { color(S::Fresh, t) } else { LightRed };
        return put(f, x, y, '█', c, Black);
    }
    if ca == cb {
        return if ca == Black { put(f, x, y, '·', DarkGray, Black) } else { put(f, x, y, '█', ca, Black) };
    }
    let (halves, first_up) =
        if vertical { (['▀', '▄'], rank(a) >= rank(b)) } else { (['▌', '▐'], rank(a) >= rank(b)) };
    if first_up {
        put(f, x, y, halves[0], ca, as_bg(cb));
    } else {
        put(f, x, y, halves[1], cb, as_bg(ca));
    }
}

/// Tested-offset range of sample `i` of `n`.
fn slice(i: usize, n: usize) -> (u64, u64) {
    let total = scene::tested_total();
    (i as u64 * total / n as u64, (i as u64 + 1) * total / n as u64)
}

/// Key to the colours; returns the column after it.
pub fn legend(f: &mut Frame, x: usize, y: usize, failing: bool) -> usize {
    let mut cx = x;
    if failing {
        for &(_, _, at, bits) in &CLUSTERS {
            put(f, cx, y, '■', LightRed, Black);
            cx = text(f, cx + 2, y, &format!("{at} {bits}"), LightRed) + 3;
        }
    } else {
        for (c, s) in [(Cyan, "tested"), (White, "sweep"), (DarkGray, "pending")] {
            put(f, cx, y, if c == DarkGray { '·' } else { '■' }, c, Black);
            cx = text(f, cx + 2, y, s, DarkGray) + 3;
        }
    }
    cx
}

/// Right-aligned scale note after the legend, when it fits.
fn scale(f: &mut Frame, after: usize, end: usize, y: usize, s: &str) {
    let n = s.chars().count();
    if after + n <= end {
        text(f, end - n, y, s, DarkGray);
    }
}

/// Address of the first sample of each map row, as "48G".
fn row_label(f: &mut Frame, x: usize, y: usize, a: u64) {
    text(f, x, y, &format!("{:>2}G", a / GIB), DarkGray);
}

/// Rows that put whole threads' chunks on whole half-rows where they fit.
fn aligned(avail: usize) -> usize {
    [32, 16, 8, 4, 2].into_iter().find(|&n| n <= avail).unwrap_or(avail)
}

/// Half-block map: two samples per cell, address increasing along rows; legend on the last row when `with_legend`.
pub fn half(f: &mut Frame, r: R, t: f64, failing: bool, with_legend: bool) {
    let rows = aligned(r.h - usize::from(with_legend));
    let label = r.w >= 30;
    let x0 = r.x + if label { 4 } else { 0 };
    let cols = r.x + r.w - x0;
    let n = cols * rows * 2;
    for y in 0..rows {
        if label && y % 2 == 0 {
            row_label(f, r.x, r.y + y, slice(y * 2 * cols, n).0);
        }
        for x in 0..cols {
            let (a0, a1) = slice(y * 2 * cols + x, n);
            let (b0, b1) = slice((y * 2 + 1) * cols + x, n);
            let (a, b) = (scene::state_off(a0, a1, t, failing), scene::state_off(b0, b1, t, failing));
            pair(f, x0 + x, r.y + y, a, b, t, true);
        }
    }
    if with_legend {
        let end = legend(f, r.x, r.y + rows, failing);
        scale(
            f,
            end,
            r.x + r.w,
            r.y + rows,
            &format!("{} MiB per half cell", (scene::tested_total() / n as u64) >> 20),
        );
    }
}

/// One row, two samples per cell side by side.
pub fn strip(f: &mut Frame, x: usize, y: usize, w: usize, t: f64, failing: bool) {
    let n = w * 2;
    for c in 0..w {
        let (a0, a1) = slice(c * 2, n);
        let (b0, b1) = slice(c * 2 + 1, n);
        pair(f, x + c, y, scene::state_off(a0, a1, t, failing), scene::state_off(b0, b1, t, failing), t, false);
    }
}

/// Braille map: 2×4 samples per cell, one colour per cell; the legend goes on the caller's title row.
pub fn braille(f: &mut Frame, r: R, t: f64, failing: bool) {
    let rows = aligned(r.h);
    let x0 = r.x + 4;
    let cols = r.x + r.w - x0;
    let (dw, dh) = (cols * 2, rows * 4);
    let n = dw * dh;
    let order = [(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2), (0, 3), (1, 3)];
    for cy in 0..rows {
        if cy % 2 == 0 {
            row_label(f, r.x, r.y + cy, slice(cy * 4 * dw, n).0);
        }
        for cx in 0..cols {
            let mut st = [S::Hole; 8];
            for (i, &(dx, dy)) in order.iter().enumerate() {
                let k = (cy * 4 + dy) * dw + cx * 2 + dx;
                let (a, b) = slice(k, n);
                st[i] = scene::state_off(a, b, t, failing);
            }
            let top = st.iter().copied().max_by_key(|&s| rank(s)).unwrap();
            if matches!(top, S::Error | S::Fresh) {
                put(f, x0 + cx, r.y + cy, '█', color(top, t), Black);
                continue;
            }
            let (bits, c) = match top {
                S::Error | S::Fresh => unreachable!(),
                S::Head => (mask(&st, |s| matches!(s, S::Done | S::Head)), White),
                S::Done => (mask(&st, |s| s == S::Done), Cyan),
                S::Pending => (mask(&st, |s| s == S::Pending), DarkGray),
                S::Hole => (0, Black),
            };
            let ch = char::from_u32(0x2800 + bits).unwrap();
            put(f, x0 + cx, r.y + cy, ch, c, Black);
        }
    }
}

fn mask(st: &[S; 8], on: impl Fn(S) -> bool) -> u32 {
    st.iter().enumerate().filter(|(_, s)| on(**s)).map(|(i, _)| 1 << i).sum()
}

/// Tests × address: one row per test of the job, one ■ per slice; finished tests lit, the running one sweeping.
pub fn matrix(f: &mut Frame, r: R, t: f64, failing: bool) {
    use TestId::*;
    let tests = [
        AddressOwn,
        MovingInvOnes,
        MovingInvWalk8,
        MovingInvRandom,
        MovingInvWalk64,
        BlockMove,
        RandomSeq,
        Modulo20,
        Hashed,
    ];
    let x0 = r.x + 7;
    let cols = r.x + r.w - x0;
    let seq = scene::error_seq();
    let shown = if t >= scene::NEW_ERROR_S { seq.len() } else { seq.len() - 1 };
    let col_of = |p: u64| (p * cols as u64 / TOP) as usize;
    let ruler_y = r.y;
    for g in [0u64, 16, 32, 48, 64] {
        let c = col_of(g * GIB).min(cols - 1);
        let s = format!("{g}G");
        let sx = (x0 + c).min(r.x + r.w - s.len());
        text(f, sx, ruler_y, &s, DarkGray);
    }
    for (row, test) in tests.iter().enumerate() {
        let y = r.y + 1 + row;
        let name_c = match row {
            0..7 => LightGray,
            7 => White,
            _ => DarkGray,
        };
        text(f, r.x, y, test.key(), name_c);
        for c in 0..cols {
            let (a, b) = (c as u64 * TOP / cols as u64, (c as u64 + 1) * TOP / cols as u64);
            let st = scene::state(a, b, t, false);
            let (ch, col) = match (row, st) {
                (_, S::Hole) => (' ', Black),
                (0..7, _) => ('■', Cyan),
                (7, S::Done) => ('■', Cyan),
                (7, S::Head) => ('■', White),
                (7, _) => ('■', DarkGray),
                _ => ('·', DarkGray),
            };
            put(f, x0 + c, y, ch, col, Black);
        }
        if failing {
            for (k, &(_, _, phys, tid)) in seq.iter().take(shown).enumerate() {
                if tid == *test {
                    let fresh = k + 1 == seq.len() && t < scene::NEW_ERROR_S + 1.2;
                    let c = if fresh { color(S::Fresh, t) } else { LightRed };
                    put(f, x0 + col_of(phys), y, '■', c, Black);
                }
            }
        }
    }
    let y = r.y + 1 + tests.len() + 1;
    if y < r.y + r.h {
        let gib = 7.0 * scene::tested_total() as f64 / GIB as f64;
        text(f, r.x, y, &format!("{gib:.0} GiB tested of {:.0} GiB planned", gib * 9.0 / 7.0), LightGray);
    }
    if y + 2 < r.y + r.h {
        if failing {
            for (k, &(_, _, at, bits)) in CLUSTERS.iter().enumerate() {
                put(f, r.x, y + 2 + k, '■', LightRed, Black);
                text(f, r.x + 2, y + 2 + k, &format!("{at}  {bits}"), LightRed);
            }
        } else {
            legend(f, r.x, y + 2, false);
        }
        let s = format!("{:.1} GiB per cell", TOP as f64 / cols as f64 / GIB as f64);
        text(f, r.x, y + 5, &s, DarkGray);
    }
}
