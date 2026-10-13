//! Snapshot hashes, frame times, and PPM renders into `$MEMBURN_GFX_OUT` for review.

use std::time::Instant;

use super::fixtures::*;
use super::*;

/// Writes `c` as `dir/name.ppm`.
fn ppm(c: &Canvas, dir: &str, name: &str) {
    let mut out = format!("P6\n{} {}\n255\n", c.w, c.h).into_bytes();
    for &p in &c.px {
        out.extend([(p >> 16) as u8, (p >> 8) as u8, p as u8]);
    }
    std::fs::write(format!("{dir}/{name}.ppm"), out).unwrap();
}

#[test]
fn renders_for_review() {
    let Ok(dir) = std::env::var("MEMBURN_GFX_OUT") else { return };
    std::fs::create_dir_all(&dir).unwrap();
    for (name, v) in states() {
        for (w, h) in [(1280, 800), (1920, 1080), (1024, 768), (3840, 2160)] {
            ppm(&render(&v, w, h), &dir, &format!("{name}-{w}x{h}"));
        }
    }
}

#[test]
fn every_state_renders_at_every_size() {
    for (_, v) in states() {
        for (w, h) in [(640, 480), (1024, 768), (1280, 1024), (2560, 1080), (3840, 2160)] {
            let c = render(&v, w, h);
            assert!(c.px.iter().any(|&p| p != c.px[0]));
        }
    }
}

#[test]
fn snapshots() {
    let got: Vec<(&str, u64)> = scenes().iter().map(|(n, v)| (*n, fnv(&render(v, 1280, 800).px))).collect();
    let want = SNAPSHOTS;
    assert_eq!(got, want, "{got:#x?}");
}

/// Frame times at 1920×1080 and 3840×2160; `cargo test --release -p memburn-gfx frame_time -- --ignored --nocapture`.
#[test]
#[ignore]
fn frame_time() {
    let mut p = Painter::new();
    for (w, h) in [(1920, 1080), (3840, 2160)] {
        let mut c = Canvas::new(w, h);
        let mut prev = Canvas::new(w, h);
        let fft = extra().into_iter().filter(|(n, _)| ["burn-in", "burn-in-16", "stress"].contains(n));
        for (name, v) in scenes().into_iter().chain(fft) {
            p.paint(&v, &mut c);
            let n = 50;
            let t = Instant::now();
            let mut spans = 0;
            for _ in 0..n {
                p.paint(&v, &mut c);
                spans += damage(Some(&prev), &c).len();
                std::mem::swap(&mut prev, &mut c);
            }
            let ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
            println!("{w}x{h} {name}: {ms:.2} ms per frame (paint + damage), {} changed rows", spans / n);
        }
    }
    let v = burn(false);
    for (w, h) in [(160, 50), (80, 25)] {
        let mut f = memburn_core::tui::Frame::new(w, h);
        let n = 500;
        let t = Instant::now();
        for _ in 0..n {
            memburn_core::tui::render(&v, &mut f);
        }
        println!("text {w}x{h} burn-in: {:.1} µs per frame", t.elapsed().as_secs_f64() * 1e6 / n as f64);
    }
    let stats = memburn_core::fft::BurnStats::with_lanes(32);
    let mut fv = memburn_core::tui::FftView::new(&stats, 32, 0);
    let n = 2000u64;
    let t = Instant::now();
    for k in 1..=n {
        fv.sample(&stats, k * 100);
    }
    println!("sampling 32 lanes: {:.2} µs", t.elapsed().as_secs_f64() * 1e6 / n as f64);
}
