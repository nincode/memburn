//! Option 2: the dashboard drawn as pixels with tiny-skia and fontdue, the way the UEFI GOP or /dev/fb0 build would.
// Fonts: Inter and JetBrains Mono latin subsets, SIL Open Font License 1.1 (gfx/fonts/*-OFL.txt).
use std::collections::HashMap;

use fontdue::{Font, FontSettings, Metrics};
use memburn_core::tui::{self, Current, ErrKind, Qr, StepState, View};
use tiny_skia::{
    Color, FillRule, GradientStop, LinearGradient, Paint, PathBuilder, Pixmap, Point, Rect, SpreadMode, Stroke,
    Transform,
};

use crate::scene::{self, CLUSTERS, THREADS};

pub const W: usize = 1280;
pub const H: usize = 800;

#[derive(Clone, Copy)]
pub enum Style {
    Faithful,
    Graphite,
}

const SANS: usize = 0;
const SANS_B: usize = 1;
const MONO: usize = 2;
const MONO_B: usize = 3;

/// Colours of one style, 0xRRGGBB.
struct Pal {
    bg: u32,
    card: u32,
    hair: u32,
    ink: u32,
    soft: u32,
    mid: u32,
    dim: u32,
    cyan: u32,
    cyan_dim: u32,
    red: u32,
    red_fill: u32,
    green: u32,
    yellow: u32,
    blue_fill: u32,
    pending: u32,
}

const VGA: Pal = Pal {
    bg: 0x000000,
    card: 0x000000,
    hair: 0x555555,
    ink: 0xffffff,
    soft: 0xaaaaaa,
    mid: 0x808080,
    dim: 0x555555,
    cyan: 0x55ffff,
    cyan_dim: 0x00aaaa,
    red: 0xff5555,
    red_fill: 0xaa0000,
    green: 0x55ff55,
    yellow: 0xffff55,
    blue_fill: 0x0000aa,
    pending: 0x1c1c1c,
};

const GRAPHITE: Pal = Pal {
    bg: 0x1e1d1b,
    card: 0x272623,
    hair: 0x3b3a36,
    ink: 0xe9e7e1,
    soft: 0xc6c4bd,
    mid: 0x9a9893,
    dim: 0x6b6a65,
    cyan: 0x5cd0e0,
    cyan_dim: 0x2f8792,
    red: 0xff6155,
    red_fill: 0xd9261c,
    green: 0x7cc68a,
    yellow: 0xe8c547,
    blue_fill: 0x2a4a6b,
    pending: 0x34332f,
};

fn rgb(c: u32) -> (u8, u8, u8) {
    ((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

fn color(c: u32, a: f32) -> Color {
    let (r, g, b) = rgb(c);
    Color::from_rgba8(r, g, b, (a * 255.0) as u8)
}

fn mix(a: u32, b: u32, k: f32) -> u32 {
    let (ar, ag, ab) = rgb(a);
    let (br, bg, bb) = rgb(b);
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * k).round() as u32;
    m(ar, br) << 16 | m(ag, bg) << 8 | m(ab, bb)
}

pub struct Ui {
    style: Style,
    pm: Pixmap,
    fonts: [Font; 4],
    glyphs: HashMap<(usize, char, u32), (Metrics, Vec<u8>)>,
}

impl Ui {
    pub fn new(style: Style) -> Ui {
        let load = |b: &[u8]| Font::from_bytes(b, FontSettings::default()).unwrap();
        Ui {
            style,
            pm: Pixmap::new(W as u32, H as u32).unwrap(),
            fonts: [
                load(include_bytes!("../../../gfx/fonts/Inter-Regular.ttf")),
                load(include_bytes!("../../../gfx/fonts/Inter-SemiBold.ttf")),
                load(include_bytes!("../../../gfx/fonts/JetBrainsMono-Regular.ttf")),
                load(include_bytes!("../../../gfx/fonts/JetBrainsMono-SemiBold.ttf")),
            ],
            glyphs: HashMap::new(),
        }
    }

    /// RGB24 of the dashboard at `t`.
    pub fn frame(&mut self, v: &View, t: f64, failing: bool) -> Vec<u8> {
        match self.style {
            Style::Faithful => self.faithful(v, t, failing),
            Style::Graphite => self.graphite(v, t, failing),
        }
        self.pm.data().as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect()
    }

    // ---- primitives

    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: u32, a: f32) {
        let mut p = Paint::default();
        p.set_color(color(c, a));
        if let Some(r) = Rect::from_xywh(x, y, w, h) {
            self.pm.fill_rect(r, &p, Transform::identity(), None);
        }
    }

    fn rrect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> tiny_skia::Path {
        let mut pb = PathBuilder::new();
        pb.move_to(x + r, y);
        pb.line_to(x + w - r, y);
        pb.quad_to(x + w, y, x + w, y + r);
        pb.line_to(x + w, y + h - r);
        pb.quad_to(x + w, y + h, x + w - r, y + h);
        pb.line_to(x + r, y + h);
        pb.quad_to(x, y + h, x, y + h - r);
        pb.line_to(x, y + r);
        pb.quad_to(x, y, x + r, y);
        pb.close();
        pb.finish().unwrap()
    }

    fn rrect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, fill: Option<u32>, stroke: Option<u32>) {
        let path = Self::rrect_path(x, y, w, h, r);
        let mut p = Paint::default();
        p.anti_alias = true;
        if let Some(c) = fill {
            p.set_color(color(c, 1.0));
            self.pm.fill_path(&path, &p, FillRule::Winding, Transform::identity(), None);
        }
        if let Some(c) = stroke {
            p.set_color(color(c, 1.0));
            let s = Stroke { width: 1.0, ..Stroke::default() };
            self.pm.stroke_path(&path, &p, &s, Transform::identity(), None);
        }
    }

    fn circle(&mut self, x: f32, y: f32, r: f32, c: u32, a: f32) {
        let mut p = Paint::default();
        p.anti_alias = true;
        p.set_color(color(c, a));
        if let Some(path) = PathBuilder::from_circle(x, y, r) {
            self.pm.fill_path(&path, &p, FillRule::Winding, Transform::identity(), None);
        }
    }

    fn line(&mut self, pts: &[(f32, f32)], c: u32, a: f32, width: f32) {
        let mut pb = PathBuilder::new();
        pb.move_to(pts[0].0, pts[0].1);
        for &(x, y) in &pts[1..] {
            pb.line_to(x, y);
        }
        let mut p = Paint::default();
        p.anti_alias = true;
        p.set_color(color(c, a));
        let s = Stroke { width, ..Stroke::default() };
        self.pm.stroke_path(&pb.finish().unwrap(), &p, &s, Transform::identity(), None);
    }

    fn tri(&mut self, x: f32, y: f32, s: f32, c: u32) {
        let mut pb = PathBuilder::new();
        pb.move_to(x, y - s);
        pb.line_to(x + s * 1.4, y);
        pb.line_to(x, y + s);
        pb.close();
        let mut p = Paint::default();
        p.anti_alias = true;
        p.set_color(color(c, 1.0));
        self.pm.fill_path(&pb.finish().unwrap(), &p, FillRule::Winding, Transform::identity(), None);
    }

    fn glyph(&mut self, font: usize, ch: char, px: f32) -> Option<(Metrics, Vec<u8>)> {
        let key = (font, ch, (px * 10.0) as u32);
        if let Some(g) = self.glyphs.get(&key) {
            return Some(g.clone());
        }
        let f = &self.fonts[font];
        if f.lookup_glyph_index(ch) == 0 && ch != ' ' {
            return None;
        }
        let g = f.rasterize(ch, px);
        self.glyphs.insert(key, g.clone());
        Some(g)
    }

    /// Draws `s` with its baseline at `y`; returns the x after it.
    fn text(&mut self, font: usize, px: f32, x: f32, y: f32, s: &str, c: u32) -> f32 {
        let (cr, cg, cb) = rgb(c);
        let mut cx = x;
        let mut prev = None;
        for ch in s.chars() {
            let (font, ch) = match self.glyph(font, ch, px) {
                Some(_) => (font, ch),
                None if ch == '→' => {
                    cx = self.text(font, px, cx, y, "->", c);
                    prev = None;
                    continue;
                }
                None => (font ^ 2, ch),
            };
            if let Some(p) = prev {
                cx += self.fonts[font].horizontal_kern(p, ch, px).unwrap_or(0.0);
            }
            let Some((m, bmp)) = self.glyph(font, ch, px) else { continue };
            let gx = (cx + m.xmin as f32).round() as i32;
            let gy = (y - m.height as f32 - m.ymin as f32).round() as i32;
            let data = self.pm.data_mut();
            for row in 0..m.height {
                for col in 0..m.width {
                    let a = bmp[row * m.width + col] as u32;
                    let (px_, py) = (gx + col as i32, gy + row as i32);
                    if a == 0 || px_ < 0 || py < 0 || px_ >= W as i32 || py >= H as i32 {
                        continue;
                    }
                    let o = (py as usize * W + px_ as usize) * 4;
                    for (k, s) in [cr, cg, cb].into_iter().enumerate() {
                        let d = data[o + k] as u32;
                        data[o + k] = ((s as u32 * a + d * (255 - a)) / 255) as u8;
                    }
                }
            }
            cx += m.advance_width;
            prev = Some(ch);
        }
        cx
    }

    fn measure(&mut self, font: usize, px: f32, s: &str) -> f32 {
        s.chars()
            .map(|ch| {
                self.glyph(font, ch, px).or_else(|| self.glyph(font ^ 2, ch, px)).map_or(0.0, |g| g.0.advance_width)
            })
            .sum()
    }

    fn text_right(&mut self, font: usize, px: f32, end: f32, y: f32, s: &str, c: u32) {
        let w = self.measure(font, px, s);
        self.text(font, px, end - w, y, s, c);
    }

    fn put(&mut self, x: usize, y: usize, c: u32) {
        if x < W && y < H {
            let (r, g, b) = rgb(c);
            let o = (y * W + x) * 4;
            let d = self.pm.data_mut();
            d[o] = r;
            d[o + 1] = g;
            d[o + 2] = b;
            d[o + 3] = 255;
        }
    }

    // ---- shared widgets

    fn chart(&mut self, x: f32, y: f32, w: f32, h: f32, series: &[u32], c: u32, grid: u32) {
        let n = (w as usize / 5).min(series.len());
        let shown = &series[series.len() - n..];
        let hi = *shown.iter().max().unwrap() as f32 * 1.05;
        let lo = *shown.iter().min().unwrap() as f32 * 0.8;
        for k in 0..4 {
            let gy = y + h * k as f32 / 3.0;
            self.line(&[(x, gy), (x + w, gy)], grid, 1.0, 1.0);
        }
        let pt = |i: usize| (x + w * i as f32 / (n - 1) as f32, y + h - h * (shown[i] as f32 - lo) / (hi - lo));
        let mut pb = PathBuilder::new();
        let p0 = pt(0);
        pb.move_to(p0.0, p0.1);
        for i in 1..n {
            let (a, b) = (pt(i - 1), pt(i));
            let mx = (a.0 + b.0) / 2.0;
            pb.cubic_to(mx, a.1, mx, b.1, b.0, b.1);
        }
        let stroke_path = pb.clone().finish().unwrap();
        pb.line_to(x + w, y + h);
        pb.line_to(x, y + h);
        pb.close();
        let fill = pb.finish().unwrap();
        let mut p = Paint::default();
        p.anti_alias = true;
        p.shader = LinearGradient::new(
            Point::from_xy(0.0, y),
            Point::from_xy(0.0, y + h),
            vec![GradientStop::new(0.0, color(c, 0.45)), GradientStop::new(1.0, color(c, 0.02))],
            SpreadMode::Pad,
            Transform::identity(),
        )
        .unwrap();
        self.pm.fill_path(&fill, &p, FillRule::Winding, Transform::identity(), None);
        let mut p = Paint::default();
        p.anti_alias = true;
        p.set_color(color(c, 1.0));
        let s = Stroke { width: 1.8, ..Stroke::default() };
        self.pm.stroke_path(&stroke_path, &p, &s, Transform::identity(), None);
        let last = pt(n - 1);
        self.circle(last.0, last.1, 3.5, c, 1.0);
    }

    /// One pixel row band per thread: tested this sweep, sweep head with a fading trail, pending; error marks on top.
    fn memmap(&mut self, pal: &Pal, x: usize, y: usize, w: usize, h: usize, t: f64, failing: bool) -> Vec<(f32, f32)> {
        let row_h = h / THREADS;
        let gap = usize::from(row_h >= 4);
        let total = scene::tested_total();
        let chunk = total / THREADS as u64;
        for k in 0..THREADS {
            let p = scene::head(k, t);
            for c in 0..w {
                let f = c as f64 / w as f64;
                let col = if p >= 1.0 || f < p - 0.12 {
                    pal.cyan_dim
                } else if f < p {
                    let a = ((p - f) / 0.12) as f32;
                    mix(pal.ink, pal.cyan_dim, a.powf(0.6))
                } else {
                    pal.pending
                };
                for r in 0..row_h - gap {
                    self.put(x + c, y + k * row_h + r, col);
                }
            }
        }
        let mut marks = Vec::new();
        if failing {
            for (i, &(c0, ..)) in CLUSTERS.iter().enumerate() {
                let o = scene::offset(c0).unwrap();
                let k = (o / chunk) as usize;
                let fx = x as f32 + w as f32 * ((o - k as u64 * chunk) as f32 / chunk as f32);
                let fy = (y + k * row_h) as f32 + (row_h - gap) as f32 / 2.0;
                self.circle(fx, fy, 9.0, pal.red, 0.18);
                self.circle(fx, fy, 5.0, pal.red, 0.35);
                self.rect(fx - 2.0, (y + k * row_h) as f32 - 2.0, 4.0, (row_h - gap) as f32 + 4.0, pal.red, 1.0);
                if i == 0 && (scene::NEW_ERROR_S..scene::NEW_ERROR_S + 1.5).contains(&t) {
                    let age = ((t - scene::NEW_ERROR_S) / 1.5) as f32;
                    let mut p = Paint::default();
                    p.anti_alias = true;
                    p.set_color(color(pal.red, 1.0 - age));
                    let ring = PathBuilder::from_circle(fx, fy, 6.0 + age * 28.0).unwrap();
                    self.pm.stroke_path(
                        &ring,
                        &p,
                        &Stroke { width: 2.0, ..Stroke::default() },
                        Transform::identity(),
                        None,
                    );
                }
                marks.push((fx, fy));
            }
        }
        marks
    }

    fn qr(&mut self, q: &Qr, x: f32, y: f32, size: f32, light: u32, dark: u32) {
        let n = q.size + 4;
        let m = (size / n as f32).floor();
        self.rrect(x, y, m * n as f32, m * n as f32, 6.0, Some(light), None);
        for r in 0..q.size {
            for c in 0..q.size {
                if q.dark[r * q.size + c] {
                    self.rect(x + (c + 2) as f32 * m, y + (r + 2) as f32 * m, m, m, dark, 1.0);
                }
            }
        }
    }

    /// A DDR4 module seen from the front: board, eight chips, contacts.
    fn dimm(&mut self, x: f32, y: f32, w: f32, h: f32, board: u32, chip: u32, gold: u32, outline: Option<u32>) {
        self.rrect(x, y, w, h, 2.0, Some(board), outline);
        let cw = w / 8.0 * 0.7;
        for i in 0..8 {
            let cx = x + w * (i as f32 + 0.15) / 8.0 + if i >= 4 { 2.0 } else { 0.0 };
            self.rect(cx, y + h * 0.18, cw, h * 0.48, chip, 1.0);
        }
        self.rect(x + 3.0, y + h - 4.0, w * 0.46, 3.0, gold, 1.0);
        self.rect(x + w * 0.52, y + h - 4.0, w * 0.46 - 3.0, 3.0, gold, 1.0);
    }

    // ---- 2a: the text dashboard's layout, drawn as graphics

    fn panel(&mut self, pal: &Pal, x: f32, y: f32, w: f32, h: f32, title: &str, badge: Option<(&str, u32)>) {
        self.rrect(x + 0.5, y + 0.5, w - 1.0, h - 1.0, 7.0, None, Some(pal.hair));
        let tw = self.measure(SANS_B, 14.0, title);
        let bw = badge.map_or(0.0, |(b, _)| self.measure(SANS_B, 14.0, b) + 10.0);
        self.rect(x + 12.0, y - 2.0, tw + bw + 12.0, 5.0, pal.bg, 1.0);
        let end = self.text(SANS_B, 14.0, x + 18.0, y + 5.0, title, pal.ink);
        if let Some((b, c)) = badge {
            self.text(SANS_B, 14.0, end + 8.0, y + 5.0, b, c);
        }
    }

    fn faithful(&mut self, v: &View, t: f64, failing: bool) {
        let p = &VGA;
        self.pm.fill(color(p.bg, 1.0));
        // header
        self.rect(0.0, 0.0, W as f32, 26.0, 0xb4b4b4, 1.0);
        let x = self.text(SANS_B, 14.0, 12.0, 18.0, &format!("memburn {}", v.version), 0x000000);
        self.text(SANS, 14.0, x + 18.0, 18.0, &v.system.join("   ·   "), 0x0000aa);
        self.text_right(MONO_B, 14.0, W as f32 - 12.0, 18.0, &tui::clock(v.elapsed_ms), 0x000000);
        // band
        let errors = v.mem_errors;
        let band = if errors > 0 { p.red_fill } else { p.blue_fill };
        self.rect(0.0, 26.0, W as f32, 50.0, band, 1.0);
        let main =
            if errors > 0 { format!("RUNNING  ·  {errors} MEMORY ERRORS") } else { "RUNNING  ·  no errors".into() };
        self.text(SANS_B, 18.0, 16.0, 48.0, &main, 0xffffff);
        self.text(SANS, 14.0, 16.0, 68.0, "Full memory test  ·  step 8 of 10: Modulo 20", 0xdddddd);
        let (c1, c2, c3) = (8.0, 468.0, 928.0);
        let (top, mid, bot) = (88.0, 338.0, 768.0);
        // current test
        self.panel(p, c1, top, 452.0, mid - top - 8.0, "Current test", None);
        if let Current::Mem { test, step, steps, done, total, left_ms, job_left_ms, .. } = &v.current {
            self.text(SANS_B, 20.0, c1 + 18.0, top + 38.0, test.name(), p.ink);
            let pos = format!("pass 1/1   ·   test 8/9   ·   step {step}/{steps}");
            self.text(SANS, 14.0, c1 + 18.0, top + 62.0, &pos, p.soft);
            let pct = *done as f32 / *total as f32;
            self.rrect(c1 + 18.0, top + 76.0, 360.0, 14.0, 7.0, Some(0x262626), None);
            self.rrect(c1 + 18.0, top + 76.0, 360.0 * pct, 14.0, 7.0, Some(p.cyan), None);
            self.text_right(MONO_B, 16.0, c1 + 434.0, top + 89.0, &format!("{:.0}%", pct * 100.0), p.ink);
            let gib = |b: u64| b as f64 / (1u64 << 30) as f64;
            self.text(
                MONO,
                14.0,
                c1 + 18.0,
                top + 116.0,
                &format!("{:.1} GiB / {:.1} GiB", gib(*done), gib(*total)),
                p.soft,
            );
            self.text(MONO, 14.0, c1 + 18.0, top + 138.0, &tui::time_left(*left_ms, *job_left_ms, "job"), p.soft);
        }
        // performance
        self.panel(p, c2, top, 452.0, mid - top - 8.0, "Performance", None);
        let now = *v.perf.mem.last().unwrap() as f32 / 1000.0;
        let x = self.text(SANS, 14.0, c2 + 18.0, top + 34.0, "Memory", p.soft);
        let x = self.text(MONO_B, 15.0, x.max(c2 + 100.0), top + 34.0, &format!("{now:.1} GB/s"), p.cyan);
        self.text(MONO, 14.0, x + 14.0, top + 34.0, "peak 54.8", p.mid);
        self.text(SANS, 14.0, c2 + 18.0, top + 56.0, "Compute", p.soft);
        self.text(MONO, 14.0, c2 + 100.0, top + 56.0, "-", p.soft);
        self.text(SANS, 14.0, c2 + 18.0, top + 78.0, "CPU temp", p.soft);
        self.text(MONO, 14.0, c2 + 100.0, top + 78.0, &format!("{} °C", v.perf.temp_c.unwrap()), p.soft);
        self.chart(c2 + 18.0, top + 100.0, 416.0, 130.0, &v.perf.mem, p.cyan, 0x1a1a1a);
        // plan
        self.panel(p, c1, mid, 452.0, bot - mid, &v.job, None);
        for (n, s) in v.plan.iter().enumerate() {
            let y = mid + 32.0 + n as f32 * 21.0;
            let (nc, right, rc) = match s.state {
                StepState::Running => (p.ink, "running".to_string(), p.yellow),
                StepState::Pending => (p.dim, String::new(), p.dim),
                StepState::Done { errors: 0, .. } => (p.soft, "ok".into(), p.green),
                StepState::Done { errors, .. } => (p.soft, format!("{errors} ERR"), p.red),
            };
            if s.state == StepState::Running {
                self.tri(c1 + 18.0, y - 5.0, 5.0, p.yellow);
            }
            self.text(SANS, 14.5, c1 + 34.0, y, &s.name, nc);
            if let StepState::Done { ms, .. } = s.state {
                self.text_right(MONO, 13.0, c1 + 370.0, y, &tui::clock(ms), p.mid);
            }
            self.text_right(MONO_B, 13.0, c1 + 434.0, y, &right, rc);
        }
        let my = mid + 32.0 + 10.0 * 21.0 + 6.0;
        self.text(SANS_B, 13.0, c1 + 18.0, my, "Memory map", p.soft);
        self.text_right(SANS, 12.0, c1 + 434.0, my, "32 threads  ·  1.9 GiB each  ·  4.7 MiB per pixel", p.mid);
        let marks = self.memmap(p, (c1 + 18.0) as usize, (my + 10.0) as usize, 416, 128, t, failing);
        for (i, &(_, _, at, bits)) in CLUSTERS.iter().enumerate().take(marks.len()) {
            let y = bot - 16.0;
            let x = c1 + 18.0 + i as f32 * 210.0;
            self.rect(x, y - 9.0, 8.0, 8.0, p.red, 1.0);
            self.text(MONO, 12.0, x + 14.0, y, &format!("{at} {bits}"), p.red);
        }
        if marks.is_empty() {
            let y = bot - 16.0;
            let mut x = c1 + 18.0;
            for (c, s) in [(p.cyan_dim, "tested this sweep"), (p.ink, "sweep head"), (0x3a3a3a, "pending")] {
                self.rect(x, y - 9.0, 8.0, 8.0, c, 1.0);
                x = self.text(SANS, 12.0, x + 14.0, y, s, p.mid) + 18.0;
            }
        }
        // errors and modules
        let mod_top = bot - 110.0;
        let badge = if errors > 0 { (format!("{errors}"), p.red) } else { ("0".into(), p.green) };
        self.panel(p, c2, mid, 452.0, mod_top - mid - 8.0, "Errors", Some((&badge.0, badge.1)));
        if errors == 0 {
            self.text(SANS_B, 15.0, c2 + 18.0, mid + 34.0, "none", p.green);
        } else {
            let mut y = mid + 34.0;
            for b in v.bits.iter().take(2) {
                let s = format!(
                    "bit {:>2}  lane {}  0→1 ×{}, 1→0 ×{}   {} places",
                    b.bit,
                    b.bit / 8,
                    b.up,
                    b.down,
                    b.places
                );
                self.text(MONO, 13.0, c2 + 18.0, y, &s, p.yellow);
                y += 19.0;
            }
            self.text(MONO, 13.0, c2 + 18.0, y, "+ bits 1, 22   ·   stuck at 1: bit 1, 22", p.yellow);
            y += 25.0;
            let room = ((mod_top - 20.0 - y) / 18.0) as usize;
            let hidden = v.errors.len().saturating_sub(room);
            for e in v.errors.iter().skip(hidden) {
                if e.kind != ErrKind::Mem {
                    continue;
                }
                let fresh = e.at_ms == v.elapsed_ms && t < scene::NEW_ERROR_S + 1.5;
                if fresh {
                    self.rect(c2 + 12.0, y - 13.0, 428.0, 18.0, p.red_fill, 0.35);
                }
                self.text(MONO, 12.0, c2 + 18.0, y, &tui::clock(e.at_ms), p.dim);
                self.text(MONO, 12.0, c2 + 80.0, y, &e.text.replace("->", "→"), p.red);
                y += 18.0;
            }
        }
        self.panel(p, c2, mod_top, 452.0, bot - mod_top, "Memory modules", None);
        for (n, d) in v.dimms.iter().enumerate() {
            let y = mod_top + 32.0 + n as f32 * 19.0;
            let likely = v.suspects.iter().any(|s| s.dimms.contains(&n));
            let c = if likely { p.red } else { p.ink };
            self.dimm(c2 + 18.0, y - 11.0, 34.0, 12.0, 0x1f5f2f, 0x111111, 0xc8a040, likely.then_some(p.red));
            self.text(MONO_B, 13.0, c2 + 62.0, y, &d.locator, c);
            self.text(MONO, 13.0, c2 + 150.0, y, "16 GiB   3200 MT/s", p.soft);
            if likely {
                self.text(SANS_B, 13.0, c2 + 320.0, y, "likely", p.red);
            }
        }
        // live view
        self.panel(p, c3, top, W as f32 - 8.0 - c3, bot - top, "Live view", None);
        if let Some(r) = &v.remote {
            self.qr(r.qr.as_ref().unwrap(), c3 + 22.0, top + 24.0, 300.0, 0xaaaaaa, 0x000000);
            self.text(MONO, 14.0, c3 + 22.0, top + 350.0, &r.url, p.cyan);
            self.text(SANS, 14.0, c3 + 22.0, top + 372.0, "uploading  ·  0 s ago", p.green);
        }
        // footer
        let mut x = 10.0;
        for (k, a) in &v.keys {
            let kw = self.measure(SANS_B, 13.0, k) + 14.0;
            self.rrect(x, 776.0, kw, 20.0, 4.0, Some(0xaaaaaa), None);
            self.text(SANS_B, 13.0, x + 7.0, 791.0, k, 0x000000);
            x = self.text(SANS, 14.0, x + kw + 8.0, 791.0, a, p.soft) + 22.0;
        }
    }

    // ---- 2b: redesigned in warm graphite

    fn card(&mut self, p: &Pal, x: f32, y: f32, w: f32, h: f32, label: &str) {
        self.rrect(x, y, w, h, 10.0, Some(p.card), Some(p.hair));
        self.text(SANS_B, 12.0, x + 18.0, y + 26.0, &label.to_uppercase(), p.mid);
    }

    fn graphite(&mut self, v: &View, t: f64, failing: bool) {
        let p = &GRAPHITE;
        self.pm.fill(color(p.bg, 1.0));
        let (l, r) = (32.0, W as f32 - 32.0);
        // top line
        let x = self.text(SANS_B, 20.0, l, 38.0, "memburn", p.ink);
        self.text(SANS, 14.0, x + 10.0, 38.0, v.version, p.mid);
        let sys = "Ryzen 9 5950X  ·  32 threads  ·  AVX2+FMA  ·  62.7 GiB  ·  4 × 16 GiB DDR4-3200  ·  Secure Boot off";
        self.text(SANS, 14.0, 250.0, 38.0, sys, p.soft);
        self.text_right(MONO_B, 20.0, r, 38.0, &tui::clock(v.elapsed_ms), p.ink);
        self.line(&[(l, 56.5), (r, 56.5)], p.hair, 1.0, 1.0);
        // status
        let errors = v.mem_errors;
        let (pill, pill_fill, pill_text) = if errors > 0 {
            (format!("{errors} MEMORY ERRORS"), p.red_fill, 0xffffff)
        } else {
            ("RUNNING  ·  NO ERRORS".to_string(), p.hair, p.ink)
        };
        let pw = self.measure(SANS_B, 13.0, &pill) + 24.0;
        self.rrect(l, 76.0, pw, 26.0, 13.0, Some(pill_fill), None);
        self.text(SANS_B, 13.0, l + 12.0, 94.0, &pill, pill_text);
        if let Current::Mem { test, step, steps, done, total, left_ms, job_left_ms, .. } = &v.current {
            let pct = *done as f32 / *total as f32;
            let x = self.text(SANS_B, 72.0, l - 3.0, 182.0, &format!("{:.0}", pct * 100.0), p.ink);
            self.text(SANS, 32.0, x + 2.0, 182.0, "%", p.mid);
            let tx = l + 150.0;
            self.text(SANS_B, 24.0, tx, 136.0, test.name(), p.ink);
            let line = format!("Full memory test  ·  test 8 of 9  ·  step {step} of {steps}");
            self.text(SANS, 15.0, tx, 160.0, &line, p.soft);
            let gib = |b: u64| b as f64 / (1u64 << 30) as f64;
            let line = format!(
                "{:.1} of {:.1} GiB  ·  {}",
                gib(*done),
                gib(*total),
                tui::time_left(*left_ms, *job_left_ms, "job")
            );
            self.text(SANS, 15.0, tx, 182.0, &line, p.mid);
            self.rrect(l, 198.0, 560.0, 4.0, 2.0, Some(p.hair), None);
            self.rrect(l, 198.0, 560.0 * pct, 4.0, 2.0, Some(p.ink), None);
        }
        // throughput
        let cx = 660.0;
        let now = *v.perf.mem.last().unwrap() as f32 / 1000.0;
        self.text(SANS_B, 12.0, cx, 92.0, "MEMORY THROUGHPUT", p.mid);
        let x = self.text(SANS_B, 40.0, cx, 140.0, &format!("{now:.1}"), p.cyan);
        self.text(SANS, 16.0, x + 6.0, 140.0, "GB/s", p.mid);
        self.text(SANS, 13.0, cx, 166.0, "peak 54.8", p.mid);
        self.text(SANS, 13.0, cx, 186.0, &format!("CPU {} °C", v.perf.temp_c.unwrap()), p.mid);
        self.chart(cx + 180.0, 80.0, r - cx - 180.0, 120.0, &v.perf.mem, p.cyan, p.hair);
        // memory map
        let (my, mh) = (222.0, 236.0);
        self.card(p, l, my, r - l, mh, "Memory  ·  32 threads sweeping");
        let note = "each row 1.9 GiB  ·  4.0 MiB per pixel";
        self.text_right(SANS, 12.0, r - 18.0, my + 26.0, note, p.mid);
        let (mx, mw) = (l + 18.0, if failing { 820.0 } else { r - l - 36.0 });
        let marks = self.memmap(p, mx as usize, (my + 40.0) as usize, mw as usize, 160, t, failing);
        let ly = my + mh - 16.0;
        if marks.is_empty() {
            let mut x = mx;
            for (c, s) in [(p.cyan_dim, "tested this sweep"), (p.ink, "sweep head"), (p.pending, "pending")] {
                self.rrect(x, ly - 9.0, 10.0, 10.0, 2.0, Some(c), None);
                x = self.text(SANS, 13.0, x + 16.0, ly, s, p.soft) + 22.0;
            }
        } else {
            let lx = mx + mw + 28.0;
            for (i, &(fx, fy)) in marks.iter().enumerate() {
                let (c0, c1, _, bits) = CLUSTERS[i];
                let ty = my + 74.0 + i as f32 * 78.0;
                self.line(&[(fx + 6.0, fy), (lx - 40.0, fy), (lx - 8.0, ty - 5.0)], p.red, 0.8, 1.2);
                self.text(MONO_B, 15.0, lx, ty, &format!("{c0:#x}–{:x}", c1 - 1), p.red);
                self.text(SANS, 13.5, lx, ty + 20.0, &format!("{bits}  ·  channel A  ·  DIMM_A1 or A2"), p.soft);
                let n = scene::error_seq().iter().filter(|e| (c0..c1).contains(&e.2)).count()
                    - usize::from(i == 0 && t < scene::NEW_ERROR_S);
                self.text(SANS, 13.5, lx, ty + 39.0, &format!("{n} errors"), p.mid);
            }
            let mut x = mx;
            for (c, s) in [(p.cyan_dim, "tested this sweep"), (p.ink, "sweep head"), (p.red, "error")] {
                self.rrect(x, ly - 9.0, 10.0, 10.0, 2.0, Some(c), None);
                x = self.text(SANS, 13.0, x + 16.0, ly, s, p.soft) + 22.0;
            }
        }
        // bottom cards
        let (by, bh) = (474.0, 290.0);
        let widths = [376.0, 404.0, 196.0, 192.0];
        let mut xs = [l; 4];
        for i in 1..4 {
            xs[i] = xs[i - 1] + widths[i - 1] + 16.0;
        }
        self.card(p, xs[0], by, widths[0], bh, "Plan");
        for (n, s) in v.plan.iter().enumerate() {
            let y = by + 56.0 + n as f32 * 23.0;
            let (nc, right, rc) = match s.state {
                StepState::Running => (p.ink, "running".to_string(), p.yellow),
                StepState::Pending => (p.dim, String::new(), p.dim),
                StepState::Done { errors: 0, .. } => (p.soft, "ok".into(), p.green),
                StepState::Done { errors, .. } => (p.soft, format!("{errors} err"), p.red),
            };
            if s.state == StepState::Running {
                self.circle(xs[0] + 22.0, y - 5.0, 4.0, p.yellow, 1.0);
            }
            self.text(if s.state == StepState::Running { SANS_B } else { SANS }, 14.0, xs[0] + 34.0, y, &s.name, nc);
            self.text_right(SANS_B, 13.0, xs[0] + widths[0] - 18.0, y, &right, rc);
        }
        let ex = xs[1];
        self.card(p, ex, by, widths[1], bh, "Errors");
        if errors == 0 {
            self.circle(ex + 50.0, by + 92.0, 24.0, p.green, 0.16);
            self.line(&[(ex + 39.0, by + 92.0), (ex + 47.0, by + 100.0), (ex + 62.0, by + 84.0)], p.green, 1.0, 3.0);
            self.text(SANS_B, 22.0, ex + 90.0, by + 100.0, "No errors", p.ink);
            self.text(SANS, 14.0, ex + 18.0, by + 150.0, "7 of 9 tests passed  ·  61.5 GiB", p.soft);
        } else {
            let cols = [18.0, 70.0, 120.0, 190.0, 260.0];
            for (c, h) in cols.iter().zip(["bit", "lane", "0→1", "1→0", "places"]) {
                self.text(SANS_B, 12.0, ex + c, by + 54.0, h, p.mid);
            }
            for (n, b) in v.bits.iter().enumerate() {
                let y = by + 76.0 + n as f32 * 21.0;
                let vals = [
                    b.bit.to_string(),
                    (b.bit / 8).to_string(),
                    b.up.to_string(),
                    b.down.to_string(),
                    b.places.to_string(),
                ];
                for (c, s) in cols.iter().zip(vals) {
                    self.text(MONO, 14.0, ex + c, y, &s, if *c == 18.0 { p.yellow } else { p.soft });
                }
                if b.down == 0 {
                    self.text(SANS, 12.0, ex + 320.0, y, "stuck at 1", p.yellow);
                }
            }
            self.line(&[(ex + 18.0, by + 166.0), (ex + widths[1] - 18.0, by + 166.0)], p.hair, 1.0, 1.0);
            let shown = 5;
            for (n, e) in v.errors.iter().rev().take(shown).enumerate() {
                let y = by + 188.0 + n as f32 * 19.0;
                let fresh = n == 0 && e.at_ms == v.elapsed_ms && t < scene::NEW_ERROR_S + 1.5;
                if fresh {
                    self.rrect(ex + 10.0, y - 14.0, widths[1] - 20.0, 19.0, 4.0, Some(0x4a2420), None);
                }
                let txt = e.text.replace("->", "→").replace("  channel A", "");
                self.text(MONO, 12.5, ex + 18.0, y, &txt, p.red);
            }
        }
        let dx = xs[2];
        self.card(p, dx, by, widths[2], bh, "Modules");
        for (n, d) in v.dimms.iter().enumerate() {
            let y = by + 48.0 + n as f32 * 58.0;
            let likely = v.suspects.iter().any(|s| s.dimms.contains(&n));
            let c = if likely { p.red } else { p.soft };
            self.dimm(dx + 18.0, y, 160.0, 26.0, 0x2d4a36, 0x1b1a18, 0xb8963e, likely.then_some(p.red));
            self.text(SANS_B, 12.5, dx + 18.0, y + 44.0, &d.locator, c);
            self.text_right(SANS, 12.5, dx + 178.0, y + 44.0, if likely { "likely" } else { "16 GiB" }, c);
        }
        let qx = xs[3];
        self.card(p, qx, by, widths[3], bh, "Live view");
        if let Some(rem) = &v.remote {
            self.qr(rem.qr.as_ref().unwrap(), qx + 18.0, by + 42.0, 156.0, p.ink, p.bg);
            self.text(MONO, 12.0, qx + 18.0, by + 226.0, "reljam.com/m/", p.soft);
            self.text(MONO_B, 15.0, qx + 18.0, by + 246.0, "8DYEAMWR", p.cyan);
            self.circle(qx + 22.0, by + 266.0, 3.5, p.green, 1.0);
            self.text(SANS, 12.5, qx + 32.0, by + 271.0, "uploading  ·  0 s ago", p.mid);
        }
        let mut x = l;
        for (k, a) in &v.keys {
            let kw = self.measure(SANS_B, 12.0, k) + 14.0;
            self.rrect(x, 774.0, kw, 20.0, 4.0, None, Some(p.dim));
            self.text(SANS_B, 12.0, x + 7.0, 788.0, k, p.soft);
            x = self.text(SANS, 13.0, x + kw + 8.0, 788.0, a, p.mid) + 22.0;
        }
    }
}
