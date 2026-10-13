//! Inter and JetBrains Mono, rasterised by fontdue on first use and cached per size.
// Fonts: SIL Open Font License 1.1, gfx/fonts/*-OFL.txt.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use fontdue::{Font, FontSettings, Metrics};

use crate::draw::{Canvas, blend};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Face {
    Sans,
    SansBold,
    Mono,
    MonoBold,
}

impl Face {
    /// The same weight in the other family, for glyphs a subset lacks.
    fn other(self) -> Face {
        match self {
            Face::Sans => Face::Mono,
            Face::SansBold => Face::MonoBold,
            Face::Mono => Face::Sans,
            Face::MonoBold => Face::SansBold,
        }
    }
}

/// A face at a size in pixels.
#[derive(Clone, Copy, Debug)]
pub struct Style {
    pub face: Face,
    pub px: f32,
}

struct Glyph {
    m: Metrics,
    bmp: Vec<u8>,
}

pub struct Fonts {
    fonts: [Font; 4],
    /// (face, char, size in quarter pixels).
    cache: BTreeMap<(Face, char, u32), Option<Glyph>>,
}

const FILES: [&[u8]; 4] = [
    include_bytes!("../fonts/Inter-Regular.ttf"),
    include_bytes!("../fonts/Inter-SemiBold.ttf"),
    include_bytes!("../fonts/JetBrainsMono-Regular.ttf"),
    include_bytes!("../fonts/JetBrainsMono-SemiBold.ttf"),
];

impl Default for Fonts {
    fn default() -> Self {
        Self::new()
    }
}

impl Fonts {
    pub fn new() -> Fonts {
        let load = |b: &'static [u8]| match Font::from_bytes(b, FontSettings::default()) {
            Ok(f) => f,
            Err(e) => panic!("font: {e}"),
        };
        Fonts { fonts: FILES.map(load), cache: BTreeMap::new() }
    }

    fn font(&self, f: Face) -> &Font {
        &self.fonts[f as usize]
    }

    /// The face that has `ch`, preferring `f`.
    fn pick(&self, f: Face, ch: char) -> Face {
        if ch == ' ' || self.font(f).lookup_glyph_index(ch) != 0 { f } else { f.other() }
    }

    fn glyph(&mut self, f: Face, ch: char, px: f32) -> Option<&Glyph> {
        let key = (f, ch, (px * 4.0) as u32);
        if !self.cache.contains_key(&key) {
            let font = &self.fonts[f as usize];
            let g = (ch == ' ' || font.lookup_glyph_index(ch) != 0).then(|| {
                let (m, bmp) = font.rasterize(ch, key.2 as f32 / 4.0);
                Glyph { m, bmp }
            });
            self.cache.insert(key, g);
        }
        self.cache.get(&key).and_then(|g| g.as_ref())
    }

    fn kern(&self, f: Face, a: char, b: char, px: f32) -> f32 {
        self.font(f).horizontal_kern(a, b, px).unwrap_or(0.0)
    }

    pub fn measure(&mut self, st: Style, s: &str) -> i32 {
        let mut x = 0.0;
        let mut prev: Option<(Face, char)> = None;
        for ch in s.chars() {
            let f = self.pick(st.face, ch);
            if let Some((pf, pc)) = prev
                && pf == f
            {
                x += self.kern(f, pc, ch, st.px);
            }
            x += self.glyph(f, ch, st.px).map_or(0.0, |g| g.m.advance_width);
            prev = Some((f, ch));
        }
        libm::ceilf(x) as i32
    }

    /// Draws `s` with its baseline at `y`, stopping before a glyph would end past `x + max`; returns the x after it.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(&mut self, c: &mut Canvas, st: Style, x: i32, y: i32, s: &str, color: u32, max: i32) -> i32 {
        let mut cx = x as f32;
        let end = x.saturating_add(max) as f32;
        let mut prev: Option<(Face, char)> = None;
        for ch in s.chars() {
            let f = self.pick(st.face, ch);
            if let Some((pf, pc)) = prev
                && pf == f
            {
                cx += self.kern(f, pc, ch, st.px);
            }
            prev = Some((f, ch));
            let Some(g) = self.glyph(f, ch, st.px) else { continue };
            if cx + g.m.advance_width > end + 0.5 {
                break;
            }
            let gx = libm::roundf(cx) as i32 + g.m.xmin;
            let gy = y - g.m.height as i32 - g.m.ymin;
            let (gw, gh) = (g.m.width, g.m.height);
            for row in 0..gh {
                let py = gy + row as i32;
                if py < 0 || py as usize >= c.h {
                    continue;
                }
                let line = &mut c.px[py as usize * c.w..(py as usize + 1) * c.w];
                for (col, &a) in g.bmp[row * gw..(row + 1) * gw].iter().enumerate() {
                    let px = gx + col as i32;
                    if a != 0 && px >= 0 && (px as usize) < line.len() {
                        let p = &mut line[px as usize];
                        *p = blend(*p, color, a as u32);
                    }
                }
            }
            cx += g.m.advance_width;
        }
        libm::ceilf(cx) as i32
    }
}
