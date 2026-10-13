//! Cell grid to RGB with the stick's 8×16 font, plus the glyphs the prototypes add; layout rects of `tui::render`.
use memburn_core::font;
use memburn_core::tui::{Cell, Color, Frame, View};

pub const PALETTE: [u32; 16] = [
    0x000000, 0x0000aa, 0x00aa00, 0x00aaaa, 0xaa0000, 0xaa00aa, 0xaa5500, 0xaaaaaa, 0x555555, 0x5555ff, 0x55ff55,
    0x55ffff, 0xff5555, 0xff55ff, 0xffff55, 0xffffff,
];

#[derive(Clone, Copy, Debug)]
pub struct R {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

impl R {
    pub fn inner(self) -> R {
        R { x: self.x + 2, y: self.y + 1, w: self.w.saturating_sub(4), h: self.h.saturating_sub(2) }
    }
}

/// Panel rects as `tui::render` lays them out.
pub struct Layout {
    pub current: R,
    pub plan: R,
    pub errors: R,
    pub remote: Option<R>,
}

pub fn layout(v: &View, w: usize, h: usize) -> Layout {
    let qw = match &v.remote {
        Some(r) if w >= 120 => {
            let size = r.qr.as_ref().map_or(29, |q| q.size);
            if h >= size / 2 + 12 { size + 8 } else { 0 }
        }
        _ => 0,
    };
    let (by, bh) = (3, h - 4);
    let lw = (w - qw) / 2;
    let rw = w - qw - lw;
    let top_h = (bh * 2 / 5).clamp(7, 16);
    let bot_h = bh - top_h;
    let rows = v.dimms.len().min(bot_h.saturating_sub(5 + 2));
    let mod_h = if rows == 0 { 0 } else { rows + 2 };
    Layout {
        current: R { x: 0, y: by, w: lw, h: top_h },
        plan: R { x: 0, y: by + top_h, w: lw, h: bot_h },
        errors: R { x: lw, y: by + top_h, w: rw, h: bot_h - mod_h },
        remote: (qw > 0).then_some(R { x: lw + rw, y: by, w: qw, h: bh }),
    }
}

pub fn put(f: &mut Frame, x: usize, y: usize, ch: char, fg: Color, bg: Color) {
    if x < f.w && y < f.h {
        f.cells[y * f.w + x] = Cell { ch, fg, bg };
    }
}

pub fn text(f: &mut Frame, x: usize, y: usize, s: &str, fg: Color) -> usize {
    let mut cx = x;
    for ch in s.chars() {
        put(f, cx, y, ch, fg, Color::Black);
        cx += 1;
    }
    cx
}

pub fn blank(f: &Frame, x: usize, y: usize) -> bool {
    x < f.w && y < f.h && f.cells[y * f.w + x].ch == ' ' && f.cells[y * f.w + x].bg == Color::Black
}

/// Panel border and title, as `tui` draws them; returns the inside.
pub fn panel(f: &mut Frame, r: R, title: &str) -> R {
    use Color::*;
    let (x1, y1) = (r.x + r.w - 1, r.y + r.h - 1);
    for x in r.x..=x1 {
        for y in r.y..=y1 {
            put(f, x, y, ' ', LightGray, Black);
        }
    }
    for x in r.x + 1..x1 {
        put(f, x, r.y, '─', DarkGray, Black);
        put(f, x, y1, '─', DarkGray, Black);
    }
    for y in r.y + 1..y1 {
        put(f, r.x, y, '│', DarkGray, Black);
        put(f, x1, y, '│', DarkGray, Black);
    }
    put(f, r.x, r.y, '┌', DarkGray, Black);
    put(f, x1, r.y, '┐', DarkGray, Black);
    put(f, r.x, y1, '└', DarkGray, Black);
    put(f, x1, y1, '┘', DarkGray, Black);
    text(f, r.x + 2, r.y, &format!(" {title} "), White);
    r.inner()
}

/// Glyphs the prototypes need beyond `core::font`: braille, ■, ▒, ▓, ▌, ▐.
fn extra(c: char) -> Option<[u8; 16]> {
    let mut g = [0u8; 16];
    match c {
        '▪' => g[5..11].fill(0x3c),
        '▒' => (0..16).for_each(|y| g[y] = if y % 2 == 0 { 0x55 } else { 0xaa }),
        '▓' => (0..16).for_each(|y| g[y] = if y % 2 == 0 { 0x77 } else { 0xdd }),
        '▌' => g.fill(0xf0),
        '▐' => g.fill(0x0f),
        _ => return None,
    }
    Some(g)
}

/// RGB24 at `scale` pixels per font pixel.
pub fn rgb(f: &Frame, scale: usize) -> Vec<u8> {
    let (pw, ph) = (f.w * 8 * scale, f.h * 16 * scale);
    let mut out = vec![0u8; pw * ph * 3];
    for cy in 0..f.h {
        for cx in 0..f.w {
            let c = f.cells[cy * f.w + cx];
            let g = font::glyph(c.ch).copied().or_else(|| extra(c.ch)).unwrap_or(*font::glyph_or_mark(c.ch));
            let (fg, bg) = (PALETTE[c.fg as usize], PALETTE[c.bg as usize]);
            for gy in 0..16 {
                for gx in 0..8 {
                    let on = g[gy] >> (7 - gx) & 1 == 1;
                    let px = if on { fg } else { bg };
                    for sy in 0..scale {
                        let row = (cy * 16 + gy) * scale + sy;
                        for sx in 0..scale {
                            let col = (cx * 8 + gx) * scale + sx;
                            let o = (row * pw + col) * 3;
                            out[o] = (px >> 16) as u8;
                            out[o + 1] = (px >> 8) as u8;
                            out[o + 2] = px as u8;
                        }
                    }
                }
            }
        }
    }
    out
}
