//! Software rasteriser on 0x00RRGGBB pixels: rectangles, anti-aliased rounded rectangles, circles, segments and text.

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use crate::text::{Fonts, Style};

/// Pixels, 0x00RRGGBB, row-major without padding.
pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
}

impl Canvas {
    pub fn new(w: usize, h: usize) -> Canvas {
        Canvas { w, h, px: vec![0; w * h] }
    }

    pub fn row(&self, y: usize) -> &[u32] {
        &self.px[y * self.w..(y + 1) * self.w]
    }
}

/// Columns `x0..x1` of row `y` that differ between two frames of the same size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub y: usize,
    pub x0: usize,
    pub x1: usize,
}

/// Changed spans of `cur` against `prev`, one per changed row; every row when the sizes differ.
pub fn damage(prev: Option<&Canvas>, cur: &Canvas) -> Vec<Span> {
    let w = cur.w;
    let Some(prev) = prev.filter(|p| p.w == cur.w && p.h == cur.h) else {
        return (0..cur.h).map(|y| Span { y, x0: 0, x1: w }).collect();
    };
    let mut out = Vec::new();
    for y in 0..cur.h {
        let (a, b) = (prev.row(y), cur.row(y));
        if a == b {
            continue;
        }
        let x0 = a.iter().zip(b).position(|(p, q)| p != q).unwrap_or(0);
        let x1 = w - a.iter().rev().zip(b.iter().rev()).position(|(p, q)| p != q).unwrap_or(0);
        out.push(Span { y, x0, x1 });
    }
    out
}

/// Bounding rectangles of runs of changed rows, as (x, y, w, h).
pub fn rects(spans: &[Span]) -> Vec<(usize, usize, usize, usize)> {
    let mut out: Vec<(usize, usize, usize, usize)> = Vec::new();
    for s in spans {
        match out.last_mut() {
            Some(r) if r.1 + r.3 == s.y => {
                let (x0, x1) = (r.0.min(s.x0), (r.0 + r.2).max(s.x1));
                *r = (x0, r.1, x1 - x0, r.3 + 1);
            }
            _ => out.push((s.x0, s.y, s.x1 - s.x0, 1)),
        }
    }
    out
}

/// `src` over `dst` at coverage `a` of 255.
#[inline]
pub fn blend(dst: u32, src: u32, a: u32) -> u32 {
    if a >= 255 {
        return src;
    }
    if a == 0 {
        return dst;
    }
    let rb = ((src & 0xff00ff) * a + (dst & 0xff00ff) * (255 - a) + 0x800080) >> 8 & 0xff00ff;
    let g = ((src & 0xff00) * a + (dst & 0xff00) * (255 - a) + 0x8000) >> 8 & 0xff00;
    rb | g
}

/// `a` towards `b` by `k` of 255.
pub fn mix(a: u32, b: u32, k: u32) -> u32 {
    blend(a, b, k.min(255))
}

/// Coverage masks of quarter circles, by radius in pixels.
#[derive(Default)]
pub struct Masks(BTreeMap<u32, Vec<u8>>);

impl Masks {
    /// `r` × `r` coverage of a circle of radius `r` centred at the mask's bottom-right corner.
    fn corner(&mut self, r: u32) -> &[u8] {
        self.0.entry(r).or_insert_with(|| {
            let n = r as usize;
            let rf = r as f32;
            let mut m = vec![0u8; n * n];
            for y in 0..n {
                for x in 0..n {
                    let mut hits = 0u32;
                    for sy in 0..4 {
                        for sx in 0..4 {
                            let dx = rf - (x as f32 + (sx as f32 + 0.5) / 4.0);
                            let dy = rf - (y as f32 + (sy as f32 + 0.5) / 4.0);
                            hits += u32::from(dx * dx + dy * dy <= rf * rf);
                        }
                    }
                    m[y * n + x] = (hits * 255 / 16) as u8;
                }
            }
            m
        })
    }
}

/// Draws into a [`Canvas`] with the glyph and mask caches.
pub struct Pen<'a> {
    pub c: &'a mut Canvas,
    pub fonts: &'a mut Fonts,
    pub masks: &'a mut Masks,
}

impl Pen<'_> {
    pub fn fill(&mut self, color: u32) {
        self.c.px.fill(color);
    }

    /// Clips `x, y, w, h` to the canvas as pixel ranges.
    fn clip(&self, x: i32, y: i32, w: i32, h: i32) -> Option<(usize, usize, usize, usize)> {
        let x0 = x.max(0) as usize;
        let y0 = y.max(0) as usize;
        let x1 = (x.saturating_add(w)).clamp(0, self.c.w as i32) as usize;
        let y1 = (y.saturating_add(h)).clamp(0, self.c.h as i32) as usize;
        (x0 < x1 && y0 < y1).then_some((x0, y0, x1, y1))
    }

    pub fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        let Some((x0, y0, x1, y1)) = self.clip(x, y, w, h) else { return };
        let cw = self.c.w;
        for row in y0..y1 {
            self.c.px[row * cw + x0..row * cw + x1].fill(color);
        }
    }

    #[inline]
    pub fn put(&mut self, x: i32, y: i32, color: u32, a: u32) {
        if x >= 0 && y >= 0 && (x as usize) < self.c.w && (y as usize) < self.c.h {
            let p = &mut self.c.px[y as usize * self.c.w + x as usize];
            *p = blend(*p, color, a);
        }
    }

    /// Filled rounded rectangle with anti-aliased corners of radius `r`.
    pub fn rrect(&mut self, x: i32, y: i32, w: i32, h: i32, r: i32, color: u32) {
        let r = r.min(w / 2).min(h / 2).max(0);
        if r == 0 {
            return self.rect(x, y, w, h, color);
        }
        self.rect(x + r, y, w - 2 * r, h, color);
        self.rect(x, y + r, r, h - 2 * r, color);
        self.rect(x + w - r, y + r, r, h - 2 * r, color);
        let m: Vec<u8> = self.masks.corner(r as u32).to_vec();
        let n = r as usize;
        for my in 0..n {
            for mx in 0..n {
                let a = m[my * n + mx] as u32;
                if a == 0 {
                    continue;
                }
                let (dx, dy) = (mx as i32, my as i32);
                self.put(x + dx, y + dy, color, a);
                self.put(x + w - 1 - dx, y + dy, color, a);
                self.put(x + dx, y + h - 1 - dy, color, a);
                self.put(x + w - 1 - dx, y + h - 1 - dy, color, a);
            }
        }
    }

    /// Rounded rectangle with a `t`-pixel border in `line` around a `fill` inside.
    #[allow(clippy::too_many_arguments)]
    pub fn frame(&mut self, x: i32, y: i32, w: i32, h: i32, r: i32, t: i32, fill: u32, line: u32) {
        self.rrect(x, y, w, h, r, line);
        self.rrect(x + t, y + t, w - 2 * t, h - 2 * t, (r - t).max(0), fill);
    }

    /// Filled circle of radius `r` around pixel (`cx`, `cy`).
    pub fn circle(&mut self, cx: i32, cy: i32, r: i32, color: u32, a: u32) {
        if r <= 0 {
            return;
        }
        let m: Vec<u8> = self.masks.corner(r as u32).to_vec();
        let n = r as usize;
        for my in 0..n {
            for mx in 0..n {
                let k = m[my * n + mx] as u32 * a / 255;
                if k == 0 {
                    continue;
                }
                let (dx, dy) = (r - mx as i32, r - my as i32);
                self.put(cx - dx, cy - dy, color, k);
                self.put(cx + dx - 1, cy - dy, color, k);
                self.put(cx - dx, cy + dy - 1, color, k);
                self.put(cx + dx - 1, cy + dy - 1, color, k);
            }
        }
    }

    /// Anti-aliased segment `width` pixels wide.
    pub fn segment(&mut self, (ax, ay): (f32, f32), (bx, by): (f32, f32), width: f32, color: u32, a: u32) {
        let hw = width / 2.0;
        let x0 = (ax.min(bx) - hw - 1.0) as i32;
        let x1 = (ax.max(bx) + hw + 2.0) as i32;
        let y0 = (ay.min(by) - hw - 1.0) as i32;
        let y1 = (ay.max(by) + hw + 2.0) as i32;
        let (dx, dy) = (bx - ax, by - ay);
        let len2 = (dx * dx + dy * dy).max(1e-6);
        for py in y0..y1 {
            for px in x0..x1 {
                let (qx, qy) = (px as f32 + 0.5 - ax, py as f32 + 0.5 - ay);
                let t = ((qx * dx + qy * dy) / len2).clamp(0.0, 1.0);
                let (ex, ey) = (qx - t * dx, qy - t * dy);
                let d = libm::sqrtf(ex * ex + ey * ey);
                let cov = (hw + 0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.put(px, py, color, (cov * a as f32) as u32);
                }
            }
        }
    }

    /// Draws `s` with its baseline at `y`, clipped at `max` pixels wide; returns the x after it.
    pub fn text(&mut self, st: Style, x: i32, y: i32, s: &str, color: u32, max: i32) -> i32 {
        let Pen { c, fonts, .. } = self;
        fonts.draw(c, st, x, y, s, color, max)
    }

    pub fn measure(&mut self, st: Style, s: &str) -> i32 {
        self.fonts.measure(st, s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blend_ends_and_middle() {
        assert_eq!(blend(0x000000, 0xffffff, 255), 0xffffff);
        assert_eq!(blend(0x123456, 0xffffff, 0), 0x123456);
        assert_eq!(blend(0x000000, 0xff8040, 128), 0x804020);
    }

    #[test]
    fn damage_finds_changed_spans() {
        let a = Canvas::new(8, 4);
        let mut b = Canvas::new(8, 4);
        b.px[8 + 2] = 1;
        b.px[8 + 5] = 1;
        b.px[2 * 8 + 3] = 1;
        let d = damage(Some(&a), &b);
        assert_eq!(d, [Span { y: 1, x0: 2, x1: 6 }, Span { y: 2, x0: 3, x1: 4 }]);
        assert_eq!(rects(&d), [(2, 1, 4, 2)]);
        assert_eq!(damage(None, &b).len(), 4);
    }
}
