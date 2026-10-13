//! Option 3: decorative animations drawn only into blank cells, dim so the data stays readable.
use Color::*;
use memburn_core::tui::{Color, Frame};

use crate::raster::{R, blank, put};
use crate::scene;

fn rng(mut s: u64) -> impl FnMut() -> u32 {
    move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s >> 32) as u32
    }
}

fn draw(f: &mut Frame, x: usize, y: usize, ch: char, c: Color) {
    if blank(f, x, y) && ch != ' ' {
        put(f, x, y, ch, c, Black);
    }
}

thread_local! {
    static FIRE: std::cell::RefCell<(Vec<u8>, u64)> = const { std::cell::RefCell::new((Vec::new(), 0)) };
}

/// Doom-style fire rising from the bottom of `r`; one simulation step per call, reset when `t` is 0.
pub fn fire(f: &mut Frame, r: R, t: f64) {
    let (w, h) = (r.w, r.h + 1);
    let heat = FIRE.with_borrow_mut(|(heat, seed)| {
        let warm = if t == 0.0 || heat.len() != w * h {
            *heat = vec![0u8; w * h];
            heat[(h - 1) * w..].fill(20);
            *seed = 0x9e37_79b9_7f4a_7c15;
            40
        } else {
            1
        };
        let mut next = rng(*seed);
        for _ in 0..warm {
            for x in 0..w {
                heat[(h - 1) * w + x] = 12 + (next() % 9) as u8;
            }
            for y in 1..h {
                for x in 0..w {
                    let src = heat[y * w + x];
                    let rnd = next() % 4;
                    let dx = (x + 1).saturating_sub(rnd as usize).min(w - 1);
                    heat[(y - 1) * w + dx] = src.saturating_sub((rnd & 1) as u8 + (next() % 3) as u8);
                }
            }
        }
        *seed = u64::from(next()) << 32 | u64::from(next()) | 1;
        heat.clone()
    });
    let ramp = [(' ', Black), ('.', DarkGray), (':', DarkGray), ('-', Red), ('=', Red), ('+', Brown), ('*', Brown)];
    for y in 0..r.h {
        for x in 0..w {
            let v = heat[y * w + x] as usize;
            let (ch, c) = ramp[(v * ramp.len() / 21).min(ramp.len() - 1)];
            draw(f, r.x + x, r.y + y, ch, c);
        }
    }
}

/// Sum-of-sines plasma in blue and grey.
pub fn plasma(f: &mut Frame, r: R, t: f64) {
    for y in 0..r.h {
        for x in 0..r.w {
            let (fx, fy) = (x as f64, y as f64 * 2.0);
            let cx = fx - r.w as f64 / 2.0 + 10.0 * (t * 0.5).sin();
            let cy = fy - r.h as f64 + 6.0 * (t * 0.37).cos();
            let v = (fx / 9.0 + t).sin()
                + (fy / 7.0 - t * 1.3).sin()
                + ((fx + fy) / 13.0 + t * 0.7).sin()
                + ((cx * cx + cy * cy).sqrt() / 6.0 - t * 1.1).sin();
            let v = (v + 4.0) / 8.0;
            let (ch, c) = match v {
                v if v < 0.30 => (' ', Black),
                v if v < 0.42 => ('.', DarkGray),
                v if v < 0.52 => (':', DarkGray),
                v if v < 0.62 => ('-', Blue),
                v if v < 0.72 => ('=', Blue),
                v if v < 0.80 => ('+', DarkGray),
                _ => ('*', Blue),
            };
            draw(f, r.x + x, r.y + y, ch, c);
        }
    }
}

type V3 = [f64; 3];

/// Axis-aligned box in object space and its material: 0 board, 1 chip, 2 contacts.
struct Bx {
    c: V3,
    e: V3,
    m: u8,
}

fn dimm_boxes() -> Vec<Bx> {
    let mut v = vec![
        Bx { c: [0.0, 0.15, 0.0], e: [6.65, 1.4, 0.06], m: 0 },
        Bx { c: [0.0, -1.42, 0.0], e: [6.4, 0.18, 0.065], m: 2 },
    ];
    for side in [-1.0, 1.0] {
        for i in 0..8 {
            let x = -5.45 + i as f64 * 1.45 + if i >= 4 { 0.35 } else { 0.0 };
            v.push(Bx { c: [x, 0.35, side * 0.13], e: [0.52, 0.62, 0.07], m: 1 });
        }
    }
    v
}

/// Ray along +z from `o`; nearest hit distance, normal and material.
fn hit(boxes: &[Bx], o: V3, d: V3) -> Option<(f64, V3, u8)> {
    let mut best: Option<(f64, V3, u8)> = None;
    for b in boxes {
        let (mut t0, mut t1, mut axis, mut sign) = (f64::MIN, f64::MAX, 0, 1.0);
        let mut miss = false;
        for a in 0..3 {
            let lo = b.c[a] - b.e[a];
            let hi = b.c[a] + b.e[a];
            if d[a].abs() < 1e-9 {
                if o[a] < lo || o[a] > hi {
                    miss = true;
                }
                continue;
            }
            let (mut ta, mut tb) = ((lo - o[a]) / d[a], (hi - o[a]) / d[a]);
            let mut s = -1.0;
            if ta > tb {
                std::mem::swap(&mut ta, &mut tb);
                s = 1.0;
            }
            if ta > t0 {
                t0 = ta;
                axis = a;
                sign = s;
            }
            t1 = t1.min(tb);
        }
        if miss || t0 > t1 || t1 < 0.0 {
            continue;
        }
        if best.is_none_or(|(bt, ..)| t0 < bt) {
            let mut n = [0.0; 3];
            n[axis] = sign;
            best = Some((t0, n, b.m));
        }
    }
    best
}

fn rot(v: V3, ay: f64, ax: f64) -> V3 {
    let (s, c) = ay.sin_cos();
    let v = [v[0] * c + v[2] * s, v[1], -v[0] * s + v[2] * c];
    let (s, c) = ax.sin_cos();
    [v[0], v[1] * c - v[2] * s, v[1] * s + v[2] * c]
}

/// A DDR4 module spinning about its vertical axis, shaded with ASCII by facing.
pub fn dimm(f: &mut Frame, r: R, t: f64) {
    let boxes = dimm_boxes();
    let (ay, ax) = (t * 0.8 + 0.6, 0.2);
    let k = (r.w as f64 * 8.0 * 0.92 / 13.3).min(r.h as f64 * 16.0 * 0.9 / 3.4);
    let (px0, py0) = (r.w as f64 * 4.0, r.h as f64 * 8.0);
    let light = {
        let l: V3 = [-0.4, 0.6, -0.7];
        let n = (l[0] * l[0] + l[1] * l[1] + l[2] * l[2]).sqrt();
        [l[0] / n, l[1] / n, l[2] / n]
    };
    let ramp = ['.', ':', '-', '=', '+', '*', '#', '%'];
    for cy in 0..r.h {
        for cx in 0..r.w {
            let wx = ((cx as f64 + 0.5) * 8.0 - px0) / k;
            let wy = -((cy as f64 + 0.5) * 16.0 - py0) / k;
            let cam = 60.0;
            let o = rot(rot([0.0, 0.0, -cam], 0.0, -ax), -ay, 0.0);
            let d = rot(rot([wx / cam, wy / cam, 1.0], 0.0, -ax), -ay, 0.0);
            let Some((_, n, m)) = hit(&boxes, o, d) else { continue };
            let nw = rot(n, ay, ax);
            let lam = (nw[0] * light[0] + nw[1] * light[1] + nw[2] * light[2]).abs();
            let chip = ['-', '=', '#', '#'];
            let (ch, c) = match m {
                0 => (ramp[((lam * ramp.len() as f64) as usize).min(ramp.len() - 1)], Green),
                1 => (chip[((lam * chip.len() as f64) as usize).min(chip.len() - 1)], DarkGray),
                _ => ('|', Brown),
            };
            draw(f, r.x + cx, r.y + cy, ch, c);
        }
    }
}

/// Falling columns of hex: each column spells the address its thread is testing; red where an error sits.
pub fn hexstream(f: &mut Frame, r: R, t: f64, failing: bool) {
    let mut next = rng(0x2545_f491_4f6c_dd1d);
    let cols: Vec<(usize, f64, f64, u64)> = (0..r.w / 3)
        .map(|k| (k * 3 + (next() % 2) as usize, 3.0 + (next() % 50) as f64 / 10.0, (next() % 100) as f64, k as u64))
        .collect();
    let chunk = scene::tested_total() / cols.len() as u64;
    for &(x, speed, off, k) in &cols {
        let len = 6 + (k as usize * 7) % 9;
        let span = (r.h + len) as f64;
        let head = ((t * speed + off) % span) as isize;
        let addr = scene::TESTED[2].0 + k * chunk + ((t * 1e9) as u64 & !0xfff) % chunk;
        let digits: Vec<char> = format!("{addr:010x}").chars().collect();
        let bad = failing && k % 7 == 3;
        let err: Vec<char> = format!("{:x}", scene::CLUSTERS[(k % 2) as usize].0).chars().collect();
        for i in 0..len as isize {
            let y = head - i;
            if y < 0 || y >= r.h as isize {
                continue;
            }
            let src = if bad { &err } else { &digits };
            let ch = src[(y as usize + k as usize) % src.len()];
            let c = match (i, bad) {
                (0, false) => Cyan,
                (0, true) => LightRed,
                (_, true) if i < 3 => Red,
                _ => DarkGray,
            };
            draw(f, r.x + x, r.y + y as usize, ch, c);
        }
    }
}
