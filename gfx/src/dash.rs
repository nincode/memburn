//! The dashboard in warm graphite, laid out in points of a 1280 × 800 design scaled to the screen.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use memburn_core::memtest::TestId;
use memburn_core::smbios;
use memburn_core::tui::{
    self, Current, ErrKind, MemMap, Qr, RANGE_BYTES, Sample, StepState, UpdateLine, UpdateTone, Verdict, View, clock,
    group, tenths,
};

use crate::draw::{Pen, mix};
use crate::text::{Face, Style};

const BG: u32 = 0x1e1d1b;
const CARD: u32 = 0x272623;
const HAIR: u32 = 0x3b3a36;
const INK: u32 = 0xe9e7e1;
const SOFT: u32 = 0xc6c4bd;
const MID: u32 = 0x9a9893;
const DIM: u32 = 0x6b6a65;
const CYAN: u32 = 0x5cd0e0;
const CYAN_DIM: u32 = 0x2f8792;
const RED: u32 = 0xff6155;
const RED_FILL: u32 = 0xd9261c;
const RED_ROW: u32 = 0x4a2420;
const GREEN: u32 = 0x7cc68a;
const GREEN_FILL: u32 = 0x2f6b3c;
const YELLOW: u32 = 0xe8c547;
const AMBER_FILL: u32 = 0x8a6a12;
const VIOLET: u32 = 0xc792ea;
const PENDING: u32 = 0x34332f;
const BOARD: u32 = 0x2d4a36;
const CHIP: u32 = 0x1b1a18;
const GOLD: u32 = 0xb8963e;

/// Design size in points.
const DW: f32 = 1280.0;
const DH: f32 = 800.0;
const L: f32 = 32.0;
/// Errors this recent are highlighted, ms.
const FRESH_MS: u64 = 1500;

use Face::*;

mod fft;

struct D<'p, 'a> {
    pen: &'p mut Pen<'a>,
    s: f32,
    /// Screen size in points.
    w: f32,
    h: f32,
}

impl D<'_, '_> {
    fn px(&self, v: f32) -> i32 {
        libm::roundf(v * self.s) as i32
    }

    /// Hairline thickness in pixels.
    fn t(&self) -> i32 {
        (self.s as i32).max(1)
    }

    fn st(&self, face: Face, pt: f32) -> Style {
        Style { face, px: pt * self.s }
    }

    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: u32) {
        let (x0, y0) = (self.px(x), self.px(y));
        let (x1, y1) = (self.px(x + w), self.px(y + h));
        self.pen.rect(x0, y0, x1 - x0, y1 - y0, c);
    }

    fn hline(&mut self, x: f32, y: f32, w: f32, c: u32) {
        let (x0, x1, t) = (self.px(x), self.px(x + w), self.t());
        self.pen.rect(x0, self.px(y), x1 - x0, t, c);
    }

    fn rrect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, c: u32) {
        let (x0, y0) = (self.px(x), self.px(y));
        let (x1, y1) = (self.px(x + w), self.px(y + h));
        let r = self.px(r);
        self.pen.rrect(x0, y0, x1 - x0, y1 - y0, r, c);
    }

    #[allow(clippy::too_many_arguments)]
    fn outline(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, fill: u32, line: u32) {
        let (x0, y0) = (self.px(x), self.px(y));
        let (x1, y1) = (self.px(x + w), self.px(y + h));
        let (r, t) = (self.px(r), self.t());
        self.pen.frame(x0, y0, x1 - x0, y1 - y0, r, t, fill, line);
    }

    fn circle(&mut self, x: f32, y: f32, r: f32, c: u32, a: u32) {
        let (cx, cy, r) = (self.px(x), self.px(y), self.px(r).max(1));
        self.pen.circle(cx, cy, r, c, a);
    }

    fn seg(&mut self, a: (f32, f32), b: (f32, f32), width: f32, c: u32, alpha: u32) {
        let s = self.s;
        self.pen.segment((a.0 * s, a.1 * s), (b.0 * s, b.1 * s), (width * s).max(1.0), c, alpha);
    }

    /// Text with its baseline at `y`, at most `max` points wide; returns the x after it.
    #[allow(clippy::too_many_arguments)]
    fn text(&mut self, face: Face, pt: f32, x: f32, y: f32, s: &str, c: u32, max: f32) -> f32 {
        let st = self.st(face, pt);
        let (px, py, m) = (self.px(x), self.px(y), self.px(max));
        self.pen.text(st, px, py, s, c, m) as f32 / self.s
    }

    fn measure(&mut self, face: Face, pt: f32, s: &str) -> f32 {
        let st = self.st(face, pt);
        self.pen.measure(st, s) as f32 / self.s
    }

    fn text_r(&mut self, face: Face, pt: f32, end: f32, y: f32, s: &str, c: u32) -> f32 {
        let w = self.measure(face, pt, s);
        self.text(face, pt, end - w, y, s, c, w + 2.0);
        end - w
    }

    fn text_c(&mut self, face: Face, pt: f32, mid: f32, y: f32, s: &str, c: u32) {
        let w = self.measure(face, pt, s);
        self.text(face, pt, mid - w / 2.0, y, s, c, w + 2.0);
    }

    /// Splits `s` at spaces into lines at most `max` points wide.
    fn wrap(&mut self, face: Face, pt: f32, s: &str, max: f32) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        let mut cur = String::new();
        for word in s.split(' ').filter(|w| !w.is_empty()) {
            let next = if cur.is_empty() { String::from(word) } else { format!("{cur} {word}") };
            if !cur.is_empty() && self.measure(face, pt, &next) > max {
                lines.push(core::mem::replace(&mut cur, String::from(word)));
            } else {
                cur = next;
            }
        }
        if !cur.is_empty() {
            lines.push(cur);
        }
        lines
    }

    fn card(&mut self, x: f32, y: f32, w: f32, h: f32, label: &str) {
        self.outline(x, y, w, h, 10.0, CARD, HAIR);
        let up = label.to_uppercase();
        self.text(SansBold, 12.0, x + 18.0, y + 26.0, &up, MID, w - 36.0);
    }

    fn keycap(&mut self, x: f32, y: f32, key: &str, pt: f32) -> f32 {
        let kw = self.measure(SansBold, pt, key) + 14.0;
        let h = pt + 8.0;
        self.outline(x, y, kw, h, 4.0, BG, DIM);
        self.text(SansBold, pt, x + 7.0, y + h - 6.0, key, SOFT, kw);
        x + kw
    }
}

/// Paints the whole dashboard for `v` into the pen's canvas.
pub fn paint(v: &View, pen: &mut Pen) {
    let (pw, ph) = (pen.c.w as f32, pen.c.h as f32);
    let s = (pw / DW).min(ph / DH).max(0.25);
    let mut d = D { pen, s, w: pw / s, h: ph / s };
    d.pen.fill(BG);
    header(v, &mut d);
    footer(v, &mut d);
    let (top, bottom) = (222.0, d.h - 36.0);
    if v.qr_zoom
        && let Some(r) = &v.remote
        && let Some(q) = &r.qr
    {
        zoom(&mut d, q, &r.url, &r.status, r.ok, 76.0, bottom);
        return;
    }
    hero(v, &mut d);
    throughput(v, &mut d);
    let middle = match &v.current {
        Current::Menu(_) => Middle::Menu,
        Current::Summary(_) => Middle::Summary,
        Current::Burn { .. } if v.fft.is_some() => Middle::Fft,
        _ if v.map.as_ref().is_some_and(|m| m.total > 0) && v.fft.is_some() => Middle::MapFft,
        _ if v.map.as_ref().is_some_and(|m| m.total > 0) => Middle::Map,
        _ => Middle::None,
    };
    let avail = bottom - top;
    let low_top = if middle == Middle::None {
        top
    } else {
        let mid_h = (avail * 0.435).max(150.0);
        let r = (L, top, d.w - 2.0 * L, mid_h);
        match middle {
            Middle::Menu => menu_card(v, &mut d, r),
            Middle::Summary => summary_card(v, &mut d, r),
            Middle::Map => map_card(v, &mut d, r),
            Middle::Fft | Middle::MapFft => {
                let (x, y, w, h) = r;
                let fv = v.fft.as_ref().expect("fft middle without fft");
                if middle == Middle::MapFft {
                    let mw = libm::roundf((w - 16.0) * 0.5);
                    map_card(v, &mut d, (x, y, mw, h));
                    fft::lanes(fv, &mut d, (x + mw + 16.0, y, w - mw - 16.0, h));
                } else if w >= SPLIT_MIN_W {
                    let lw = libm::roundf((w - 16.0) * 0.56);
                    fft::lanes(fv, &mut d, (x, y, lw, h));
                    fft::waterfall(fv, &mut d, (x + lw + 16.0, y, w - lw - 16.0, h));
                } else {
                    fft::lanes(fv, &mut d, r);
                }
            }
            Middle::None => {}
        }
        top + mid_h + 16.0
    };
    cards(v, &mut d, low_top, bottom - low_top);
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Middle {
    None,
    Menu,
    Summary,
    Map,
    /// Burn-in: thread lanes and the error waterfall side by side.
    Fft,
    /// Stress: the memory map and the thread lanes.
    MapFft,
}

/// Narrower middle cards show the thread lanes alone, points.
const SPLIT_MIN_W: f32 = 1000.0;

fn header(v: &View, d: &mut D) {
    let x = d.text(SansBold, 20.0, L, 38.0, "memburn", INK, 200.0);
    let x = d.text(Sans, 14.0, x + 10.0, 38.0, v.version, MID, 120.0);
    let cx = d.text_r(MonoBold, 20.0, d.w - L, 38.0, &clock(v.elapsed_ms), INK);
    let sx = (x + 48.0).max(250.0);
    let room = cx - 32.0 - sx;
    let mut n = v.system.len();
    let mut line = v.system.join("  ·  ");
    while n > 1 && d.measure(Sans, 14.0, &line) > room {
        n -= 1;
        line = v.system[..n].join("  ·  ");
    }
    d.text(Sans, 14.0, sx, 38.0, &line, SOFT, room);
    d.hline(L, 56.0, d.w - 2.0 * L, HAIR);
}

fn footer(v: &View, d: &mut D) {
    let y = d.h - 26.0;
    let mut x = L;
    for (key, action) in &v.keys {
        let end = d.keycap(x, y, key, 12.0);
        x = d.text(Sans, 13.0, end + 8.0, y + 14.0, action, MID, d.w - L - end) + 22.0;
    }
}

/// Status pill: text and fill, text colour.
fn pill(v: &View) -> (String, u32, u32) {
    let errors = v.mem_errors + v.cpu_errors;
    let found = tui::error_summary(v).to_uppercase();
    match v.verdict {
        Verdict::Menu(_) if !v.notice.is_empty() => (String::from("READY"), AMBER_FILL, INK),
        Verdict::Menu(_) => (String::from("READY"), HAIR, INK),
        Verdict::Running if errors > 0 => (found, RED_FILL, 0xffffff),
        Verdict::Running => (format!("RUNNING  ·  {found}"), HAIR, INK),
        Verdict::Stopping => (format!("STOPPING  ·  {found}"), if errors > 0 { RED_FILL } else { AMBER_FILL }, INK),
        Verdict::Pass => (format!("PASS  ·  {found}"), GREEN_FILL, INK),
        Verdict::Fail => (format!("FAIL  ·  {found}"), RED_FILL, 0xffffff),
        Verdict::Stopped => (format!("STOPPED EARLY  ·  {found}"), if errors > 0 { RED_FILL } else { HAIR }, INK),
    }
}

/// Percent done of the current step, for the big figure and the bar.
fn percent(c: &Current) -> Option<u64> {
    match *c {
        Current::Mem { done, total, .. } => Some((done * 100).checked_div(total).unwrap_or(0).min(100)),
        Current::Burn { elapsed_ms, total_ms, .. } | Current::Core { elapsed_ms, total_ms, .. } => {
            Some((elapsed_ms * 100).checked_div(total_ms).unwrap_or(0).min(100))
        }
        _ => None,
    }
}

/// Title and two lines under it for the current step.
fn lines(v: &View) -> (String, String, String) {
    let e = String::new;
    match &v.current {
        Current::Idle => (v.job.clone(), e(), e()),
        Current::Busy(what) => (what.clone(), v.job.clone(), e()),
        Current::Menu(_) => match v.verdict {
            Verdict::Menu(Some(n)) => (v.job.clone(), format!("starts in {n} s"), v.notice.clone()),
            _ => (String::from("Choose a test"), e(), v.notice.clone()),
        },
        Current::Summary(_) => (v.job.clone(), v.advice.clone(), e()),
        Current::Mem {
            test,
            pass,
            passes,
            index,
            count,
            step,
            steps,
            done,
            total,
            left_ms,
            job_left_ms,
            job_label,
        } => {
            let pass = if *passes > 1 { format!("  ·  pass {pass} of {passes}") } else { e() };
            let pos = format!("{}{pass}  ·  test {index} of {count}  ·  step {step} of {steps}", v.job);
            let gib = |b: u64| tenths(b * 10 / (1 << 30));
            let amount = format!("{} of {} GiB", gib(*done), gib(*total));
            (
                String::from(test.name()),
                pos,
                format!("{amount}  ·  {}", tui::time_left(*left_ms, *job_left_ms, job_label)),
            )
        }
        Current::Hold { left_ms } => {
            (String::from(TestId::BitFade.name()), String::from("memory idle"), format!("{} left", clock(*left_ms)))
        }
        Current::Burn { isa, threads, sizes, ffts, elapsed_ms, total_ms, worst } => (
            String::from("FFT burn-in"),
            format!("{isa}  ·  {threads} threads  ·  2^{sizes}"),
            format!(
                "{} of {}  ·  {} FFTs  ·  worst {:.4}",
                clock(*elapsed_ms),
                clock(*total_ms),
                group(*ffts),
                f64::from_bits(*worst)
            ),
        ),
        Current::Core { label, index, count, cycle, isa, ffts, elapsed_ms, total_ms } => {
            let (head, what) = tui::core_lines(label, *index, *count, *cycle, *isa);
            let t = format!("{} of {}  ·  {} FFTs", clock(*elapsed_ms), clock(*total_ms), group(*ffts));
            (String::from("Core cycler"), head, format!("{what}  ·  {t}"))
        }
    }
}

fn hero(v: &View, d: &mut D) {
    let split = L + (d.w - 2.0 * L) * 0.47;
    let (text, fill, ink) = pill(v);
    let pw = d.measure(SansBold, 13.0, &text) + 24.0;
    let pw = pw.min(split - L);
    d.rrect(L, 76.0, pw, 26.0, 13.0, fill);
    d.text(SansBold, 13.0, L + 12.0, 94.0, &text, ink, pw - 24.0);
    let pct = percent(&v.current);
    let big: Option<(String, &str, u32)> = match v.verdict {
        Verdict::Pass => Some((String::from("PASS"), "", GREEN)),
        Verdict::Fail => Some((String::from("FAIL"), "", RED)),
        Verdict::Stopped => Some((String::from("STOPPED"), "", YELLOW)),
        Verdict::Menu(Some(n)) => Some((format!("{n}"), "s", INK)),
        _ => pct.map(|p| (format!("{p}"), "%", INK)),
    };
    let mut tx = L;
    if let Some((num, unit, c)) = &big {
        let pt = if unit.is_empty() { 56.0 } else { 72.0 };
        let x = d.text(SansBold, pt, L - 3.0, 182.0, num, *c, split - L);
        let x = if unit.is_empty() { x } else { d.text(Sans, 32.0, x + 2.0, 182.0, unit, MID, 60.0) };
        tx = (L + 150.0).max(x + 24.0);
    }
    let room = split - tx;
    let (title, l1, l2) = lines(v);
    d.text(SansBold, 24.0, tx, 136.0, &title, INK, room);
    let mut body: Vec<(String, u32)> = Vec::new();
    if matches!(v.current, Current::Summary(_)) {
        body.extend(d.wrap(Sans, 15.0, &l1, room).into_iter().map(|l| (l, SOFT)));
    } else if matches!(v.current, Current::Menu(_)) {
        if !l1.is_empty() {
            body.push((l1, SOFT));
        }
        body.extend(d.wrap(Sans, 15.0, &l2, room).into_iter().map(|l| (l, YELLOW)));
    } else {
        body.extend([(l1, SOFT), (l2, MID)]);
    }
    for (k, (line, c)) in body.iter().take(2).enumerate() {
        d.text(Sans, 15.0, tx, 160.0 + 22.0 * k as f32, line, *c, room);
    }
    if let Some(p) = pct.filter(|_| !matches!(v.current, Current::Summary(_))) {
        let bw = split - L;
        d.rrect(L, 198.0, bw, 4.0, 2.0, HAIR);
        let fill = (bw * p as f32 / 100.0).max(4.0);
        d.rrect(L, 198.0, fill, 4.0, 2.0, INK);
    }
}

fn throughput(v: &View, d: &mut D) {
    let cx = L + (d.w - 2.0 * L) * 0.515;
    let burning = matches!(v.current, Current::Burn { .. } | Current::Core { .. });
    let (series, label, color, unit) = if burning {
        (&v.perf.gflops, "COMPUTE", YELLOW, "GFLOP/s")
    } else {
        (&v.perf.mem, "MEMORY THROUGHPUT", CYAN, "GB/s")
    };
    let to_tenths = |x: u32| if burning { x as u64 } else { x as u64 / 100 };
    d.text(SansBold, 12.0, cx, 92.0, label, MID, 180.0);
    let now = series.last().map_or(String::from("-"), |&x| tenths(to_tenths(x)));
    let room = 172.0 - d.measure(Sans, 16.0, unit) - 6.0;
    let pt = if d.measure(SansBold, 40.0, &now) > room { 30.0 } else { 40.0 };
    let x = d.text(SansBold, pt, cx, 140.0, &now, color, room);
    d.text(Sans, 16.0, x + 6.0, 140.0, unit, MID, cx + 176.0 - x);
    let (mp, fp) = v.perf.peaks();
    let peak = if burning { fp } else { mp };
    if peak > 0 {
        d.text(Sans, 13.0, cx, 166.0, &format!("peak {}", tenths(to_tenths(peak))), MID, 170.0);
    }
    let mut temps = Vec::new();
    if let Some(t) = v.perf.temp_c {
        temps.push(format!("CPU {t} °C"));
    }
    if let Some(t) = v.perf.ram_temp_c {
        temps.push(format!("RAM {t} °C"));
    }
    d.text(Sans, 13.0, cx, 186.0, &temps.join("  ·  "), MID, 176.0);
    let x0 = cx + 180.0;
    chart(d, x0, 80.0, d.w - L - x0, 120.0, series, color);
}

/// Area chart of the newest samples, one per 5 points, smoothed between samples.
fn chart(d: &mut D, x: f32, y: f32, w: f32, h: f32, series: &[u32], c: u32) {
    for k in 0..4 {
        d.hline(x, y + h * k as f32 / 3.0, w, HAIR);
    }
    let n = ((w / 5.0) as usize).min(series.len());
    if n < 2 || w <= 0.0 {
        return;
    }
    let shown = &series[series.len() - n..];
    let hi = *shown.iter().max().unwrap_or(&1) as f32 * 1.05;
    let lo = *shown.iter().min().unwrap_or(&0) as f32 * 0.8;
    let span = (hi - lo).max(1.0);
    let (px0, px1) = (d.px(x), d.px(x + w));
    let (py0, py1) = (d.px(y) as f32, d.px(y + h) as f32);
    let cols = (px1 - px0).max(2);
    let at = |col: f32| -> f32 {
        let t = (col / (cols - 1) as f32).clamp(0.0, 1.0) * (n - 1) as f32;
        let i = (t as usize).min(n - 2);
        let f = t - i as f32;
        let e = f * f * (3.0 - 2.0 * f);
        let val = shown[i] as f32 + (shown[i + 1] as f32 - shown[i] as f32) * e;
        py1 - (py1 - py0) * (val - lo) / span
    };
    let th = (1.8 * d.s).max(1.0);
    for k in 0..cols {
        let px = px0 + k;
        let top = at(k as f32).max(py0);
        let first = top as i32;
        for py in first..py1 as i32 {
            let depth = (py as f32 - py0) / (py1 - py0);
            let a = (0.45 - 0.43 * depth) * 255.0;
            let cover = if py == first { 1.0 - (top - first as f32) } else { 1.0 };
            d.pen.put(px, py, c, (a * cover) as u32);
        }
        let (ya, yb) = (at(k as f32 - 0.5), at(k as f32 + 0.5));
        let (lo_y, hi_y) = (ya.min(yb) - th / 2.0, ya.max(yb) + th / 2.0);
        for py in lo_y as i32..=hi_y as i32 {
            let cover = ((py as f32 + 1.0).min(hi_y) - (py as f32).max(lo_y)).clamp(0.0, 1.0);
            if cover > 0.0 {
                d.pen.put(px, py, c, (cover * 255.0) as u32);
            }
        }
    }
    let last = at((cols - 1) as f32);
    let r = (3.5 * d.s) as i32;
    d.pen.circle(px1 - 1, last as i32, r.max(2), c, 255);
}

fn menu_card(v: &View, d: &mut D, (x, y, w, h): (f32, f32, f32, f32)) {
    d.card(x, y, w, h, "Start");
    let Current::Menu(items) = &v.current else { return };
    if items.is_empty() {
        return;
    }
    let rows = if items.len() <= 3 { 1 } else { 2 };
    let cols = items.len().div_ceil(rows);
    let cw = (w - 36.0) / cols as f32;
    let pitch = ((h - 56.0) / rows as f32).min(84.0);
    let kw = items.iter().map(|(k, ..)| d.measure(SansBold, 14.0, k) + 14.0).fold(0.0, f32::max);
    for (k, (key, name, note)) in items.iter().enumerate() {
        let (cx, cy) = (x + 18.0 + (k % cols) as f32 * cw, y + 48.0 + (k / cols) as f32 * pitch);
        d.keycap(cx, cy, key, 14.0);
        let tx = cx + kw + 14.0;
        d.text(SansBold, 17.0, tx, cy + 17.0, name, INK, cx + cw - tx - 12.0);
        d.text(Sans, 13.5, tx, cy + 39.0, note, MID, cx + cw - tx - 12.0);
    }
}

fn summary_card(v: &View, d: &mut D, (x, y, w, h): (f32, f32, f32, f32)) {
    d.card(x, y, w, h, "Summary");
    let Current::Summary(rows) = &v.current else { return };
    let pitch = 22.0;
    let per = (((h - 70.0) / pitch) as usize + 1).max(1);
    let cols = rows.len().div_ceil(per).max(1);
    let cw = (w - 36.0) / cols as f32;
    let lw = rows.iter().map(|(l, _)| d.measure(Sans, 13.0, l)).fold(0.0, f32::max) + 16.0;
    for (k, (label, value)) in rows.iter().enumerate() {
        let (cx, cy) = (x + 18.0 + (k / per) as f32 * cw, y + 56.0 + (k % per) as f32 * pitch);
        d.text(Sans, 13.0, cx, cy, label, MID, lw);
        let c = if value.starts_with("likely") { RED } else { INK };
        d.text(Sans, 14.0, cx + lw, cy, value, c, cw - lw - 16.0);
    }
}

/// "0xabd160000–abd16ffff".
fn span_label(addr: u64) -> String {
    format!("{addr:#x}–{:x}", addr + RANGE_BYTES - 1)
}

/// "bit 3" or "bits 20, 22" (four at most).
fn bits_label(mask: u64) -> String {
    let bits: Vec<String> = (0..64).filter(|b| mask >> b & 1 == 1).map(|b| format!("{b}")).collect();
    let more = if bits.len() > 4 { ", …" } else { "" };
    let s = if bits.len() == 1 { "" } else { "s" };
    format!("bit{s} {}{more}", bits[..bits.len().min(4)].join(", "))
}

/// The likely module of errors in the range at `addr`, and the slots it covers when it names a channel.
fn range_module(v: &View, addr: u64) -> Option<String> {
    let e = v
        .errors
        .iter()
        .find(|e| e.mem.is_some_and(|m| m.phys && m.addr & !(RANGE_BYTES - 1) == addr) && !e.module.is_empty())?;
    let label = e.module.clone();
    let slots: Vec<String> = v
        .suspects
        .iter()
        .find(|s| s.label == label)
        .map(|s| s.dimms.iter().filter_map(|&i| v.dimms.get(i)).map(|m| tui::slot(m, &v.dimms)).collect())
        .unwrap_or_default();
    if slots.len() > 1 { Some(format!("{label}  ·  {}", slots.join(" or "))) } else { Some(label) }
}

fn map_card(v: &View, d: &mut D, (x, y, w, h): (f32, f32, f32, f32)) {
    let Some(m) = &v.map else { return };
    let label = if m.threads > 0 && matches!(v.current, Current::Mem { .. }) {
        format!("Memory  ·  {} threads sweeping", m.threads)
    } else {
        String::from("Memory")
    };
    d.card(x, y, w, h, &label);
    let callouts = !m.ranges.is_empty();
    let (mx, my) = (x + 18.0, y + 40.0);
    let mw = if callouts { w - 36.0 - 360.0 } else { w - 36.0 };
    let mh = h - 40.0 - 36.0;
    let rows = ((mh / 5.0) as usize).clamp(4, 64);
    let (px0, py0) = (d.px(mx), d.px(my));
    let cols = (d.px(mx + mw) - px0).max(1) as usize;
    let row_px = ((d.px(my + mh) - py0) as usize / rows).max(1);
    let gap = usize::from(row_px >= 4);
    let per_row = m.total.div_ceil(rows as u64);
    let note = format!("each row {}  ·  {} per pixel", size(per_row), size(per_row / cols as u64));
    d.text_r(Sans, 12.0, x + w - 18.0, y + 26.0, &note, MID);
    let n = rows * cols;
    let st = m.samples(n, v.elapsed_ms);
    let head = st.iter().position(|&s| s == Sample::Head).zip(st.iter().rposition(|&s| s == Sample::Head));
    let blink = (v.elapsed_ms / 200).is_multiple_of(2);
    let cw = d.pen.c.w;
    for r in 0..rows {
        let mut line: Vec<u32> = Vec::with_capacity(cols);
        for k in 0..cols {
            let i = r * cols + k;
            line.push(match st[i] {
                Sample::Pending => PENDING,
                Sample::Done => CYAN_DIM,
                Sample::Error => RED,
                Sample::Fresh if blink => INK,
                Sample::Fresh => RED,
                Sample::Head => {
                    let (a, b) = head.unwrap_or((i, i));
                    let span = (b - a).max(1) as u32;
                    let lead = if m.descending { (b - i) as u32 } else { (i - a) as u32 };
                    mix(CYAN_DIM, INK, 90 + lead * 165 / span)
                }
            });
        }
        let y0 = py0 as usize + r * row_px;
        for py in y0..y0 + row_px - gap {
            if py < d.pen.c.h && px0 >= 0 && px0 as usize + cols <= cw {
                let o = py * cw + px0 as usize;
                d.pen.c.px[o..o + cols].copy_from_slice(&line);
            }
        }
    }
    let t = d.t();
    for (i, s) in st.iter().enumerate() {
        if matches!(s, Sample::Error | Sample::Fresh) {
            let (r, k) = (i / cols, i % cols);
            let c = if *s == Sample::Fresh && blink { INK } else { RED };
            let (fx, fy) = (px0 + k as i32, py0 + (r * row_px) as i32);
            d.pen.rect(fx - 2 * t, fy - 2 * t, 4 * t, (row_px - gap) as i32 + 4 * t, c);
        }
    }
    let ly = y + h - 16.0;
    let mut lx = mx;
    let pend = if callouts { (RED, "error") } else { (PENDING, "pending") };
    for (c, s) in [(CYAN_DIM, "tested this sweep"), (INK, "sweep head"), pend] {
        d.rrect(lx, ly - 9.0, 10.0, 10.0, 2.0, c);
        lx = d.text(Sans, 13.0, lx + 16.0, ly, s, SOFT, 200.0) + 22.0;
    }
    if callouts {
        callout(v, d, m, (mx, my, mw, mh), (rows, cols, row_px), y + h - 30.0);
    }
}

/// Error ranges right of the map, each tied to its first error by a leader line.
fn callout(
    v: &View,
    d: &mut D,
    m: &MemMap,
    (mx, my, mw, _): (f32, f32, f32, f32),
    grid: (usize, usize, usize),
    end: f32,
) {
    let (rows, cols, row_px) = grid;
    let lx = mx + mw + 28.0;
    let pitch = 74.0;
    let fit = (((end - my - 10.0) / pitch) as usize).max(1);
    let total = m.ranges.len() + m.more_ranges as usize;
    let shown = if total > fit { fit - 1 } else { total };
    let n = (rows * cols) as u128;
    for (k, r) in m.ranges.iter().take(shown).enumerate() {
        let i = ((r.at as u128 * n / m.total.max(1) as u128) as usize).min(rows * cols - 1);
        let (row, col) = (i / cols, i % cols);
        let fx = (d.px(mx) as f32 + col as f32) / d.s;
        let fy = (d.px(my) as f32 + (row * row_px) as f32 + row_px as f32 / 2.0) / d.s;
        let ty = my + 24.0 + k as f32 * pitch;
        d.circle(fx, fy, 9.0, RED, 46);
        d.circle(fx, fy, 5.0, RED, 90);
        d.seg((fx + 6.0, fy), (lx - 40.0, fy), 1.2, RED, 204);
        d.seg((lx - 40.0, fy), (lx - 8.0, ty - 5.0), 1.2, RED, 204);
        d.text(MonoBold, 15.0, lx, ty, &span_label(r.addr), RED, 330.0);
        let mut info = bits_label(r.bits);
        if let Some(module) = range_module(v, r.addr) {
            info = format!("{info}  ·  {module}");
        }
        d.text(Sans, 13.5, lx, ty + 20.0, &info, SOFT, 330.0);
        let errs = format!("{} error{}", group(r.errors as u64), if r.errors == 1 { "" } else { "s" });
        d.text(Sans, 13.5, lx, ty + 39.0, &errs, MID, 330.0);
    }
    if shown < total {
        let ty = my + 24.0 + shown as f32 * pitch;
        d.text(Sans, 13.5, lx, ty, &format!("+ {} more ranges", total - shown), MID, 330.0);
    }
}

fn cards(v: &View, d: &mut D, y: f32, h: f32) {
    let inner = d.w - 2.0 * L;
    let mods = !v.dimms.is_empty();
    let live = v.remote.is_some() || v.update.is_some();
    let (mw, lw) = (196.0, 192.0);
    let fixed = if mods { mw + 16.0 } else { 0.0 } + if live { lw + 16.0 } else { 0.0 };
    let rest = inner - fixed - 16.0;
    let pw = rest * 0.48;
    let ew = rest - pw;
    let mut x = L;
    plan_card(v, d, (x, y, pw, h));
    x += pw + 16.0;
    errors_card(v, d, (x, y, ew, h));
    x += ew + 16.0;
    if mods {
        modules_card(v, d, (x, y, mw, h));
        x += mw + 16.0;
    }
    if live {
        live_card(v, d, (x, y, lw, h));
    }
}

fn plan_card(v: &View, d: &mut D, (x, y, w, h): (f32, f32, f32, f32)) {
    d.card(x, y, w, h, "Plan");
    let pitch = 23.0;
    let fit = (((h - 68.0) / pitch) as usize + 1).max(1);
    let running = v.plan.iter().position(|s| s.state == StepState::Running).unwrap_or(0);
    let skip = (running + 1).saturating_sub(fit);
    let timed = w >= 440.0;
    for (n, s) in v.plan.iter().skip(skip).take(fit).enumerate() {
        let ry = y + 56.0 + n as f32 * pitch;
        let (nc, right, rc, face) = match s.state {
            StepState::Running => (INK, String::from("running"), YELLOW, SansBold),
            StepState::Pending => (DIM, s.note.clone(), DIM, Sans),
            StepState::Done { errors: 0, .. } => (SOFT, String::from("ok"), GREEN, SansBold),
            StepState::Done { errors, .. } => (SOFT, format!("{} err", group(errors)), RED, SansBold),
        };
        if s.state == StepState::Running {
            d.circle(x + 22.0, ry - 5.0, 4.0, YELLOW, 255);
        }
        let rx = d.text_r(face, 13.0, x + w - 18.0, ry, &right, rc);
        let mut end = rx - 12.0;
        if let StepState::Done { ms, .. } = s.state
            && timed
        {
            end = d.text_r(Mono, 12.0, rx - 16.0, ry, &clock(ms), DIM) - 12.0;
        }
        let nf = if s.state == StepState::Running { SansBold } else { Sans };
        d.text(nf, 14.0, x + 34.0, ry, &s.name, nc, end - x - 34.0);
    }
}

fn errors_card(v: &View, d: &mut D, (x, y, w, h): (f32, f32, f32, f32)) {
    d.card(x, y, w, h, "Errors");
    let total = v.mem_errors + v.cpu_errors;
    if total > 0 {
        d.text_r(SansBold, 12.0, x + w - 18.0, y + 26.0, &group(total), RED);
    }
    if total == 0 {
        if matches!(v.verdict, Verdict::Menu(_)) {
            return;
        }
        let (cx, cy) = (x + 50.0, y + 92.0);
        d.circle(cx, cy, 24.0, GREEN, 40);
        d.seg((cx - 11.0, cy), (cx - 3.0, cy + 8.0), 3.0, GREEN, 255);
        d.seg((cx - 3.0, cy + 8.0), (cx + 12.0, cy - 8.0), 3.0, GREEN, 255);
        d.text(SansBold, 22.0, x + 90.0, y + 100.0, "No errors", INK, w - 108.0);
        let done = v.plan.iter().filter(|s| matches!(s.state, StepState::Done { .. })).count();
        if done > 0 {
            let line = format!("{done} of {} steps passed", v.plan.len());
            d.text(Sans, 14.0, x + 18.0, y + 150.0, &line, SOFT, w - 36.0);
        }
        return;
    }
    let mut ty = y + 54.0;
    if !v.bits.is_empty() {
        let cols = [18.0, 64.0, 112.0, 176.0, 240.0, 300.0];
        for (c, t) in cols.iter().zip(["bit", "lane", "0->1", "1->0", "places"]) {
            d.text(SansBold, 12.0, x + c, ty, t, MID, 60.0);
        }
        let fit = (((h * 0.5 - 50.0) / 21.0) as usize).max(1);
        let more = v.bits.len() > fit;
        let shown = if more { fit - 1 } else { v.bits.len() }.max(1);
        for b in v.bits.iter().take(shown) {
            ty += 21.0;
            let vals = [b.bit as u32, b.bit as u32 / 8, b.up, b.down, b.places];
            for (k, (c, val)) in cols.iter().zip(vals).enumerate() {
                d.text(Mono, 14.0, x + c, ty, &format!("{val}"), if k == 0 { YELLOW } else { SOFT }, 60.0);
            }
            let note = if b.up >= 3 && b.down == 0 {
                String::from("stuck at 1")
            } else if b.down >= 3 && b.up == 0 {
                String::from("stuck at 0")
            } else if b.runs > 0 {
                format!("{} earlier run{}", b.runs, if b.runs == 1 { "" } else { "s" })
            } else {
                String::new()
            };
            d.text(Sans, 12.0, x + cols[5] + 50.0, ty, &note, YELLOW, w - cols[5] - 68.0);
        }
        if more {
            ty += 21.0;
            let rest: Vec<String> = v.bits[shown..].iter().map(|b| format!("{}", b.bit)).collect();
            d.text(Mono, 13.0, x + 18.0, ty, &format!("+ bits {}", rest.join(", ")), YELLOW, w - 36.0);
        }
        ty += 18.0;
        d.hline(x + 18.0, ty, w - 36.0, HAIR);
        ty += 4.0;
    }
    let pitch = 19.0;
    let fit = ((y + h - 12.0 - ty) / pitch) as usize;
    for (n, e) in v.errors.iter().rev().take(fit).enumerate() {
        let ry = ty + 18.0 + n as f32 * pitch;
        if v.elapsed_ms.saturating_sub(e.at_ms) < FRESH_MS && e.at_ms <= v.elapsed_ms {
            d.rrect(x + 10.0, ry - 14.0, w - 20.0, 19.0, 4.0, RED_ROW);
        }
        let c = if e.kind == ErrKind::Mem { RED } else { VIOLET };
        d.text(Mono, 12.5, x + 18.0, ry, &e.text, c, w - 36.0);
    }
}

/// A DDR module seen from the front: board, eight chips, contacts.
fn dimm(d: &mut D, x: f32, y: f32, w: f32, h: f32, likely: bool) {
    if likely {
        d.outline(x, y, w, h, 2.0, BOARD, RED);
    } else {
        d.rrect(x, y, w, h, 2.0, BOARD);
    }
    let cw = w / 8.0 * 0.7;
    for i in 0..8 {
        let cx = x + w * (i as f32 + 0.15) / 8.0 + if i >= 4 { 2.0 } else { 0.0 };
        d.rect(cx, y + h * 0.18, cw, h * 0.48, CHIP);
    }
    d.rect(x + 3.0, y + h - 4.0, w * 0.46, 3.0, GOLD);
    d.rect(x + w * 0.52, y + h - 4.0, w * 0.46 - 3.0, 3.0, GOLD);
}

fn modules_card(v: &View, d: &mut D, (x, y, w, h): (f32, f32, f32, f32)) {
    d.card(x, y, w, h, "Modules");
    let likely = |n: usize| v.suspects.iter().any(|s| s.dimms.contains(&n));
    let pictures = 48.0 + v.dimms.len() as f32 * 58.0 <= h - 6.0;
    let pitch = if pictures { 58.0 } else { 21.0 };
    let fit = ((h - 52.0) / pitch) as usize + usize::from(!pictures);
    let more = v.dimms.len() > fit;
    let shown = if more { fit.saturating_sub(1) } else { v.dimms.len() };
    for (n, m) in v.dimms.iter().enumerate().take(shown) {
        let lk = likely(n);
        let c = if lk { RED } else { SOFT };
        let size = smbios::size(m.size_mib);
        let right = if lk { "likely" } else { size.as_str() };
        let name = tui::slot(m, &v.dimms);
        let ry = if pictures {
            let py = y + 48.0 + n as f32 * pitch;
            dimm(d, x + 18.0, py, w - 36.0, 26.0, lk);
            py + 44.0
        } else {
            y + 56.0 + n as f32 * pitch
        };
        let rx = d.text_r(Sans, 12.5, x + w - 18.0, ry, right, c);
        d.text(SansBold, 12.5, x + 18.0, ry, &name, c, rx - x - 26.0);
    }
    if more {
        let ry = y + 56.0 + shown as f32 * pitch;
        d.text(Sans, 12.5, x + 18.0, ry, &format!("+ {} more", v.dimms.len() - shown), MID, w - 36.0);
    }
}

/// QR code at whole pixels per module with a 2-module light border; returns its size in points.
fn qr(d: &mut D, q: &Qr, x: f32, y: f32, size: f32) -> f32 {
    let n = q.size + 4;
    let m = (d.px(size) / n as i32).max(1);
    let (x0, y0) = (d.px(x), d.px(y));
    let side = m * n as i32;
    d.pen.rrect(x0, y0, side, side, d.px(6.0).min(m * 2), INK);
    for r in 0..q.size {
        for c in 0..q.size {
            if q.dark[r * q.size + c] {
                d.pen.rect(x0 + (c as i32 + 2) * m, y0 + (r as i32 + 2) * m, m, m, BG);
            }
        }
    }
    side as f32 / d.s
}

/// Room [`update_block`] takes under the live view's status line.
const UPDATE_H: f32 = 48.0;

fn live_card(v: &View, d: &mut D, (x, y, w, h): (f32, f32, f32, f32)) {
    let Some(r) = &v.remote else {
        d.card(x, y, w, h, "Update");
        if let Some(u) = &v.update {
            update_block(d, u, x + 18.0, y + 54.0, w - 36.0);
        }
        return;
    };
    d.card(x, y, w, h, "Live view");
    let extra = if v.update.is_some() { UPDATE_H } else { 0.0 };
    let side = (w - 36.0).min(h - 134.0 - extra).max(40.0);
    let size = match &r.qr {
        Some(q) => qr(d, q, x + 18.0, y + 42.0, side),
        None => {
            d.outline(x + 18.0, y + 42.0, side, side, 6.0, CARD, HAIR);
            side
        }
    };
    let mut ty = y + 42.0 + size + 28.0;
    let url = r.url.trim_start_matches("https://").trim_start_matches("http://");
    if !url.is_empty() {
        let cut = url.rfind('/').map_or(0, |i| i + 1);
        d.text(Mono, 12.0, x + 18.0, ty, &url[..cut], SOFT, w - 36.0);
        d.text(MonoBold, 15.0, x + 18.0, ty + 20.0, &url[cut..], CYAN, w - 36.0);
        ty += 45.0;
    }
    d.circle(x + 22.0, ty - 4.5, 3.5, if r.ok { GREEN } else { DIM }, 255);
    d.text(Sans, 12.5, x + 32.0, ty, &r.status, MID, w - 50.0);
    if let Some(u) = &v.update {
        d.hline(x + 18.0, ty + 10.0, w - 36.0, HAIR);
        update_block(d, u, x + 18.0, ty + 28.0, w - 36.0);
    }
}

/// The update's text, detail and bar, the text's baseline at `y`.
fn update_block(d: &mut D, u: &UpdateLine, x: f32, y: f32, w: f32) {
    let c = match u.tone {
        UpdateTone::Busy => INK,
        UpdateTone::Ready => GREEN,
        UpdateTone::Quiet => MID,
        UpdateTone::Error => RED,
    };
    d.text(SansBold, 13.0, x, y, &u.text, c, w);
    let ly = y + 18.0;
    let Some(p) = u.percent() else {
        d.text(Sans, 12.5, x, ly, &u.detail, MID, w);
        return;
    };
    let dw = d.measure(Sans, 12.5, &u.detail);
    d.text_r(Sans, 12.5, x + w, ly, &u.detail, MID);
    let bw = (w - dw - 10.0).max(24.0);
    d.rrect(x, ly - 7.0, bw, 5.0, 2.5, PENDING);
    if p > 0 {
        d.rrect(x, ly - 7.0, (bw * p as f32 / 100.0).max(5.0), 5.0, 2.5, CYAN);
    }
}

fn zoom(d: &mut D, q: &Qr, url: &str, status: &str, ok: bool, y: f32, bottom: f32) {
    let w = d.w - 2.0 * L;
    let h = bottom - y;
    d.card(L, y, w, h, "Live view");
    let side = (h - 130.0).min(w * 0.6);
    let mid = L + w / 2.0;
    let size = qr(d, q, mid - side / 2.0, y + 40.0, side);
    let ty = y + 40.0 + size + 40.0;
    d.text_c(MonoBold, 22.0, mid, ty, url, CYAN);
    d.text_c(Sans, 14.0, mid, ty + 28.0, status, if ok { GREEN } else { MID });
}

/// Bytes in the largest of GiB, MiB and KiB that is at least one, to a tenth.
fn size(b: u64) -> String {
    let (unit, shift) =
        [("GiB", 30), ("MiB", 20), ("KiB", 10)].into_iter().find(|&(_, s)| b >= 1 << s).unwrap_or(("KiB", 10));
    format!("{} {unit}", tenths((b * 10) >> shift))
}
