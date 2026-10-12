#[allow(dead_code)]
#[path = "../examples/render.rs"]
mod render;

use memburn_core::font;
use memburn_core::tui::{self, Frame};

#[test]
fn font_covers_every_rendered_char() {
    let mut missing = std::collections::BTreeSet::new();
    for (name, w, h, v) in render::scenes() {
        let mut f = Frame::new(w, h);
        tui::render(&v, &mut f);
        for y in 0..h {
            for x in 0..w {
                let c = f.get(x, y).ch;
                if font::glyph(c).is_none() {
                    missing.insert((c, name));
                }
            }
        }
    }
    assert!(missing.is_empty(), "no glyph for {missing:?}");
}
