//! Every fixture dashboard state as PNG: text 160×50 and 80×25 (stick font), pixel 1280×800 and 1920×1080.
use std::time::Instant;

use memburn_core::tui::{self, Frame, View};
use memburn_gfx::fixtures::{self, SNAPSHOTS};
use memburn_gfx::{Canvas, Painter};

use crate::raster;

pub const TEXT: [(usize, usize); 2] = [(160, 50), (80, 25)];
pub const PIXEL: [(usize, usize); 2] = [(1280, 800), (1920, 1080)];

fn png(path: &str, w: usize, h: usize, rgb: impl Iterator<Item = [u8; 3]>) {
    let rgba: Vec<u8> = rgb.flat_map(|[r, g, b]| [r, g, b, 255]).collect();
    let size = tiny_skia::IntSize::from_wh(w as u32, h as u32).unwrap();
    tiny_skia::Pixmap::from_vec(rgba, size).unwrap().save_png(path).unwrap();
}

/// Writes `<name>-text<w>x<h>.png` and `<name>-<w>x<h>.png` for each state.
pub fn write(dir: &str, name: &str, v: &View, p: &mut Painter) {
    for (w, h) in TEXT {
        let mut f = Frame::new(w, h);
        tui::render(v, &mut f);
        let scale = 1280 / (w * 8);
        let rgb = raster::rgb(&f, scale);
        png(
            &format!("{dir}/{name}-text{w}x{h}.png"),
            w * 8 * scale,
            h * 16 * scale,
            rgb.chunks(3).map(|c| [c[0], c[1], c[2]]),
        );
    }
    for (w, h) in PIXEL {
        let mut c = Canvas::new(w, h);
        p.paint(v, &mut c);
        png(
            &format!("{dir}/{name}-{w}x{h}.png"),
            w,
            h,
            c.px.iter().map(|&p| [(p >> 16) as u8, (p >> 8) as u8, p as u8]),
        );
    }
}

/// Renders every state whose name contains `only`; checks the snapshot scenes against the gfx hashes.
pub fn run(dir: &str, only: &str) {
    let t0 = Instant::now();
    let pick = || -> Vec<(&str, View)> { fixtures::states().into_iter().filter(|(n, _)| n.contains(only)).collect() };
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    std::thread::scope(|s| {
        for i in 0..threads {
            s.spawn(move || {
                let mut p = Painter::new();
                for (name, v) in pick().iter().skip(i).step_by(threads) {
                    write(dir, name, v, &mut p);
                }
            });
        }
    });
    let states = pick();
    let mut matched = 0;
    for (name, want) in SNAPSHOTS {
        let Some((_, v)) = states.iter().find(|(n, _)| *n == name) else { continue };
        let got = fixtures::fnv(&fixtures::render(v, 1280, 800).px);
        assert_eq!(got, want, "{name}: snapshot hash {got:#x}, gfx/src/fixtures.rs has {want:#x}");
        matched += 1;
    }
    println!(
        "{} states × {} sizes in {:.2} s; {matched} snapshot hashes match",
        states.len(),
        TEXT.len() + PIXEL.len(),
        t0.elapsed().as_secs_f64()
    );
}
