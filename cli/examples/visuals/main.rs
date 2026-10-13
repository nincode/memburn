//! Renders of the shipped text dashboard (`5-*`) and the #37 prototypes as PNG stills and GIFs via ffmpeg.
//! `cargo run --release -p memburn --example visuals -- <dir> [name-filter]`
#![allow(clippy::too_many_arguments, clippy::field_reassign_with_default, clippy::needless_range_loop)]
mod deco;
mod map;
mod pixel;
mod raster;
mod scene;

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Instant;

use memburn_core::tui::{self, Cell, Color, Fire, Frame, View};

use raster::{Layout, R};
use scene::{FPS, FRAMES, Net, STILL};

type Overlay = fn(&mut Frame, &Layout, f64, bool);

fn ffmpeg(args: &[&str], w: usize, h: usize) -> std::process::Child {
    Command::new("ffmpeg")
        .args(["-loglevel", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgb24", "-s", &format!("{w}x{h}")])
        .args(["-r", &FPS.to_string(), "-i", "-"])
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .expect("ffmpeg")
}

/// Writes `<name>.png` (frame [`STILL`]), `<name>.gif` and for `5-*` a lossless `<name>.webp`; returns ms per frame, overlay ms.
fn emit(dir: &str, name: &str, colors: u32, mut frame: impl FnMut(f64) -> (Vec<u8>, f64)) -> (f64, f64) {
    let (w, h) = (1280, 800);
    let filter =
        format!("split[a][b];[a]palettegen=max_colors={colors}:stats_mode=full[p];[b][p]paletteuse=dither=none");
    let gif_path = format!("{dir}/{name}.gif");
    let mut gif = ffmpeg(&["-filter_complex", &filter, "-loop", "0", &gif_path], w, h);
    let webp_path = format!("{dir}/{name}.webp");
    let lossless = ["-c:v", "libwebp_anim", "-lossless", "1", "-compression_level", "6", "-loop", "0", &webp_path];
    let mut webp = name.starts_with("5-").then(|| ffmpeg(&lossless, w, h));
    let (mut total, mut over) = (0.0, 0.0);
    for i in 0..FRAMES {
        let t = i as f64 / FPS as f64;
        let start = Instant::now();
        let (rgb, o) = frame(t);
        total += start.elapsed().as_secs_f64() * 1000.0;
        over += o;
        assert_eq!(rgb.len(), w * h * 3, "{name}");
        gif.stdin.as_mut().unwrap().write_all(&rgb).unwrap();
        if let Some(p) = &mut webp {
            p.stdin.as_mut().unwrap().write_all(&rgb).unwrap();
        }
        if i == STILL {
            let png_path = format!("{dir}/{name}.png");
            let mut png = ffmpeg(&["-frames:v", "1", &png_path], w, h);
            png.stdin.take().unwrap().write_all(&rgb).unwrap();
            png.wait().unwrap();
        }
    }
    for p in [Some(&mut gif), webp.as_mut()].into_iter().flatten() {
        drop(p.stdin.take());
        p.wait().unwrap();
    }
    (total / FRAMES as f64, over / FRAMES as f64)
}

/// Rows of the errors panel inside `i`: room for `k` map rows after the bit summary.
fn make_room(f: &mut Frame, i: R, k: usize) -> usize {
    let row = |f: &Frame, r: usize| -> Vec<Cell> { (0..i.w).map(|c| f.get(i.x + c, i.y + r)).collect() };
    let e = (0..i.h).find(|&r| f.get(i.x, i.y + r).ch == '.').unwrap_or(4);
    let rows: Vec<Vec<Cell>> = (0..i.h).map(|r| row(f, r)).collect();
    let earlier: String = rows[e].iter().map(|c| c.ch).collect();
    let n: usize = earlier.split_whitespace().nth(1).and_then(|s| s.replace(',', "").parse().ok()).unwrap_or(0);
    for r in 0..i.h {
        let src = if r < e {
            Some(r)
        } else if r < e + k {
            None
        } else {
            Some(r - k)
        };
        for c in 0..i.w {
            let cell = src.map_or(Cell { ch: ' ', fg: Color::LightGray, bg: Color::Black }, |s| rows[s][c]);
            f.cells[(i.y + r) * f.w + i.x + c] = cell;
        }
    }
    for c in 0..i.w {
        raster::put(f, i.x + c, i.y + e + k, ' ', Color::LightGray, Color::Black);
    }
    raster::text(f, i.x, i.y + e + k, &format!("... {} earlier", n + k), Color::DarkGray);
    e
}

fn rule(f: &mut Frame, x: usize, y: usize, w: usize, title: &str) {
    for c in 0..w {
        raster::put(f, x + c, y, '─', Color::DarkGray, Color::Black);
    }
    raster::text(f, x + 2, y, &format!(" {title} "), Color::LightGray);
}

/// Free rows of the plan panel below its steps.
fn plan_free(l: &Layout, steps: usize) -> R {
    let i = l.plan.inner();
    R { x: i.x, y: i.y + steps + 1, w: i.w, h: i.h - steps - 1 }
}

fn errors_free(l: &Layout) -> R {
    let i = l.errors.inner();
    R { x: i.x, y: i.y + 2, w: i.w, h: i.h - 2 }
}

/// Free rows of the Live view panel below the URL and status.
fn live_free(l: &Layout, qr: usize) -> Option<R> {
    let i = l.remote?.inner();
    let y = i.y + qr.div_ceil(2) + 3 + 3;
    Some(R { x: i.x, y, w: i.w, h: i.y + i.h - y })
}

const QR: usize = 29;

fn o1a(f: &mut Frame, l: &Layout, t: f64, failing: bool) {
    if !failing {
        let r = errors_free(l);
        rule(f, r.x - 1, r.y - 1, r.w + 2, "Memory map");
        map::half(f, r, t, false, true);
    } else {
        let i = l.errors.inner();
        let e = make_room(f, i, 5);
        rule(f, i.x - 1, i.y + e, i.w + 2, "Memory map");
        map::half(f, R { x: i.x, y: i.y + e + 1, w: i.w, h: 4 }, t, true, true);
    }
}

fn o1b(f: &mut Frame, l: &Layout, t: f64, failing: bool) {
    let r = plan_free(l, 10);
    let r = R { y: r.y - 1, h: r.h + 1, ..r };
    rule(f, r.x - 1, r.y, r.w + 2, "Memory map");
    for x in r.x + 14..r.x + r.w {
        raster::put(f, x, r.y, ' ', Color::DarkGray, Color::Black);
    }
    map::legend(f, r.x + 15, r.y, failing);
    map::braille(f, R { x: r.x, y: r.y + 1, w: r.w, h: r.h - 1 }, t, failing);
}

fn o1c(f: &mut Frame, l: &Layout, t: f64, failing: bool) {
    let Some(rem) = l.remote else { return };
    let y0 = rem.inner().y + QR.div_ceil(2) + 3 + 3;
    let lower = R { x: rem.x, y: y0, w: rem.w, h: rem.y + rem.h - y0 };
    for c in 0..rem.w {
        raster::put(f, rem.x + c, y0 - 1, '─', Color::DarkGray, Color::Black);
    }
    raster::put(f, rem.x, y0 - 1, '└', Color::DarkGray, Color::Black);
    raster::put(f, rem.x + rem.w - 1, y0 - 1, '┘', Color::DarkGray, Color::Black);
    let i = raster::panel(f, lower, "Memory map");
    map::matrix(f, i, t, failing);
}

fn o1d(f: &mut Frame, l: &Layout, t: f64, failing: bool) {
    if !failing {
        let i = l.errors.inner();
        map::half(f, R { x: i.x, y: i.y + 1, w: i.w, h: i.h - 1 }, t, false, true);
    } else {
        let i = l.current.inner();
        raster::text(f, i.x, i.y + 5, "map", Color::DarkGray);
        map::strip(f, i.x + 4, i.y + 5, i.w - 4, t, true);
    }
}

fn o3a(f: &mut Frame, l: &Layout, t: f64, _: bool) {
    deco::fire(f, plan_free(l, 10), t);
}

fn o3b(f: &mut Frame, l: &Layout, t: f64, failing: bool) {
    let r = if failing { live_free(l, QR).unwrap() } else { errors_free(l) };
    deco::plasma(f, r, t);
}

fn o3c(f: &mut Frame, l: &Layout, t: f64, failing: bool) {
    let r = if failing { live_free(l, QR).unwrap() } else { errors_free(l) };
    deco::dimm(f, r, t);
}

fn o3d(f: &mut Frame, l: &Layout, t: f64, failing: bool) {
    deco::hexstream(f, plan_free(l, 10), t, failing);
}

/// Mean render time of the view minus the same view without map and Live view panel: map + fire + panel, ms.
fn visuals_ms(w: usize, h: usize, failing: bool, braille: bool, net: Net) -> f64 {
    const N: usize = 500;
    let mut f = Frame::new(w, h);
    let mut time = |v: &View| {
        let start = Instant::now();
        for _ in 0..N {
            tui::render(v, &mut f);
        }
        start.elapsed().as_secs_f64() * 1000.0 / N as f64
    };
    let with = scene::shipped(2.0, failing, braille, net, w < 120);
    let mut without = scene::shipped(2.0, failing, braille, net, w < 120);
    (without.map, without.remote) = (None, None);
    let (b, a) = (time(&without), time(&with));
    a - b
}

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "target/visuals".into());
    let only = std::env::args().nth(2).unwrap_or_default();
    std::fs::create_dir_all(&dir).unwrap();
    let text: [(&str, usize, usize, Overlay); 8] = [
        ("1a-map-halfblock-errors", 160, 50, o1a),
        ("1b-map-braille-plan", 160, 50, o1b),
        ("1c-map-matrix-panel", 160, 50, o1c),
        ("1d-map-halfblock-80x25", 80, 25, o1d),
        ("3a-deco-fire", 160, 50, o3a),
        ("3b-deco-plasma", 160, 50, o3b),
        ("3c-deco-dimm", 160, 50, o3c),
        ("3d-deco-hexstream", 160, 50, o3d),
    ];
    let shipped: [(&str, usize, usize, bool, Net); 4] = [
        ("5-text-dashboard", 160, 50, true, Net::Up),
        ("5-text-halfblock", 160, 50, false, Net::Up),
        ("5-text-quick-80x25", 80, 25, false, Net::Up),
        ("5-text-waiting-network", 160, 50, true, Net::Waiting),
    ];
    let mut report = Vec::new();
    for failing in [false, true] {
        let state = if failing { "fail" } else { "clean" };
        for &(name, w, h, braille, net) in &shipped {
            let name = format!("{name}-{state}");
            if !name.contains(&only) {
                continue;
            }
            let mut fire = Fire::default();
            let (ms, _) = emit(&dir, &name, 32, |t| {
                let mut v = scene::shipped(t, failing, braille, net, w < 120);
                v.fire = std::mem::take(&mut fire);
                let mut f = Frame::new(w, h);
                tui::render(&v, &mut f);
                fire = std::mem::take(&mut v.fire);
                (raster::rgb(&f, 1280 / (w * 8)), 0.0)
            });
            let over = visuals_ms(w, h, failing, braille, net);
            report.push((name, ms, over));
        }
        for &(name, w, h, overlay) in &text {
            let name = format!("{name}-{state}");
            if !name.contains(&only) {
                continue;
            }
            let (ms, over) = emit(&dir, &name, 32, |t| {
                let v = scene::view(t, failing);
                let l = raster::layout(&v, w, h);
                let mut f = Frame::new(w, h);
                tui::render(&v, &mut f);
                let start = Instant::now();
                overlay(&mut f, &l, t, failing);
                let over = start.elapsed().as_secs_f64() * 1000.0;
                (raster::rgb(&f, 1280 / (w * 8)), over)
            });
            report.push((name, ms, over));
        }
        for (name, variant) in
            [("2a-pixel-faithful", pixel::Style::Faithful), ("2b-pixel-graphite", pixel::Style::Graphite)]
        {
            let name = format!("{name}-{state}");
            if !name.contains(&only) {
                continue;
            }
            let mut ui = pixel::Ui::new(variant);
            let (ms, _) = emit(&dir, &name, 256, |t| (ui.frame(&scene::view(t, failing), t, failing), 0.0));
            report.push((name, ms, 0.0));
        }
    }
    for (name, ms, over) in report {
        println!("{name:<36} {ms:7.2} ms/frame  overlay {over:6.3} ms");
    }
}
