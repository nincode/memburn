//! The dashboard as pixels for the stick's framebuffer and the UEFI GOP: same [`View`] as the text dashboard.
#![cfg_attr(not(any(test, feature = "fixtures")), no_std)]

extern crate alloc;

mod dash;
mod draw;
mod text;

pub use draw::{Canvas, Span, blend, damage, rects};

use draw::{Masks, Pen};
use memburn_core::tui::View;
use text::Fonts;

/// Smallest screen the pixel dashboard is drawn on; smaller ones keep the text dashboard.
pub const MIN_SIZE: (usize, usize) = (640, 480);

/// Fonts and caches that persist between frames.
#[derive(Default)]
pub struct Painter {
    fonts: Fonts,
    masks: Masks,
}

impl Painter {
    pub fn new() -> Painter {
        Painter::default()
    }

    /// Draws `v` over the whole canvas.
    pub fn paint(&mut self, v: &View, c: &mut Canvas) {
        let mut pen = Pen { c, fonts: &mut self.fonts, masks: &mut self.masks };
        dash::paint(v, &mut pen);
    }
}

#[cfg(any(test, feature = "fixtures"))]
pub mod fixtures;
#[cfg(test)]
mod tests;
