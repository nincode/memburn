//! Animated pixel dashboard clips as lossless animated WebP at 1280×800, from the gfx fixtures.
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering::Relaxed;

use memburn_core::errlog::MemError;
use memburn_core::fft::{BurnError, Fault};
use memburn_core::memtest::TestId;
use memburn_core::tui::{Current, ErrLine, View};
use memburn_gfx::fixtures::{self, FftSim};
use memburn_gfx::{Canvas, Painter};

const W: usize = 1280;
const H: usize = 800;
/// Milliseconds per frame: the dashboard's redraw interval.
const FRAME_MS: u64 = 250;
/// Milliseconds per Modulo 20 sweep of 61.5 GiB.
const SWEEP_MS: u64 = 2_500;
const SWEEPS: u64 = 3;
/// Clip start on the dashboard clock, ms.
const START_MS: u64 = 290_000;
/// (phys, bit) found again on each sweep: one word in each failing range.
const REFOUND: [(u64, u32); 2] = [(0xabd16f218, 20), (0xfe78503e0, 1)];
const BURN_MS: u64 = 7_000;
/// When thread 6 returns a wrong result, ms into the burn-in clip.
const FAULT_MS: u64 = 2_500;
const FAULT_THREAD: usize = 6;
/// Milliseconds per FFT sampler step.
const SIM_MS: u64 = 100;
/// Sampler steps before the burn-in clip starts.
const SIM_WARM: u64 = 150;

/// Pipes `frames` to ffmpeg as `<dir>/<name>.webp`; returns the file size.
fn encode(dir: &str, name: &str, frames: impl Iterator<Item = Canvas>) -> u64 {
    let path = format!("{dir}/{name}.webp");
    let fps = format!("{}", 1000 / FRAME_MS);
    let mut ff = Command::new("ffmpeg")
        .args(["-loglevel", "error", "-y", "-f", "rawvideo", "-pix_fmt", "bgra", "-s", &format!("{W}x{H}")])
        .args(["-r", &fps, "-i", "-", "-c:v", "libwebp_anim", "-lossless", "1"])
        .args(["-compression_level", "6", "-loop", "0", &path])
        .stdin(Stdio::piped())
        .spawn()
        .expect("ffmpeg");
    let pipe = ff.stdin.as_mut().unwrap();
    for c in frames {
        let bgra: Vec<u8> = c.px.iter().flat_map(|&p| [p as u8, (p >> 8) as u8, (p >> 16) as u8, 255]).collect();
        pipe.write_all(&bgra).unwrap();
    }
    drop(ff.stdin.take());
    assert!(ff.wait().unwrap().success(), "ffmpeg {name}");
    std::fs::metadata(&path).unwrap().len()
}

/// A failing Modulo 20 test `ms` into the clip: the sweep crosses the map and finds both failing ranges each pass.
fn memtest(ms: u64) -> View {
    let mut v = fixtures::base(true);
    v.version = env!("CARGO_PKG_VERSION");
    v.elapsed_ms = START_MS + ms;
    let mut m = v.map.take().unwrap();
    let total = m.total;
    m.phase(false);
    m.swept = total * (ms % SWEEP_MS) / SWEEP_MS / 4096 * 4096;
    for s in 0..=ms / SWEEP_MS {
        for &(phys, bit) in &REFOUND {
            let at = s * SWEEP_MS + m.offset(phys).unwrap() * SWEEP_MS / total;
            if at > ms {
                continue;
            }
            let at = START_MS + at;
            let e =
                MemError { addr: phys as usize, expected: 0, actual: 1 << bit, test: TestId::Modulo20 as u8, pass: 0 };
            v.push_mem(at, &e, Some(phys));
            m.error(phys, phys, 1 << bit, at);
            v.mem_errors += 1;
            if let Some(b) = v.bits.iter_mut().find(|b| b.bit as u32 == bit) {
                b.up += 1;
            }
        }
    }
    if let Current::Mem { done, step, left_ms, job_left_ms, total: t, section, .. } = &mut v.current {
        *section += (ms / 1000) as u16;
        *step += (ms / SWEEP_MS) as u32;
        *done = (*done + *t * ms / 200_000).min(*t);
        *left_ms = left_ms.map(|l| l - ms);
        *job_left_ms = job_left_ms.map(|l| l - ms);
    }
    v.perf.mem.rotate_left((ms / FRAME_MS) as usize);
    v.map = Some(m);
    v
}

/// FFT burn-in after a passing memory test `ms` into the clip; thread 6 returns a wrong result at [`FAULT_MS`].
fn burn(sim: &mut FftSim, ms: u64, fault_n: &mut Option<u32>) -> View {
    while sim.t * SIM_MS < (SIM_WARM * SIM_MS + ms) {
        sim.step();
        if sim.t == SIM_WARM + FAULT_MS / SIM_MS {
            *fault_n = Some(sim.stats.lanes[FAULT_THREAD].log2n.load(Relaxed));
        }
    }
    let mut v = fixtures::burn(false);
    v.version = env!("CARGO_PKG_VERSION");
    v.elapsed_ms += ms;
    let ffts: u64 = sim.stats.lanes.iter().map(|l| l.ffts.load(Relaxed)).sum();
    if let Current::Burn { elapsed_ms, ffts: f, .. } = &mut v.current {
        *elapsed_ms += ms;
        *f = ffts;
    }
    v.perf.gflops.rotate_left((ms / FRAME_MS) as usize);
    if let Some(log2n) = *fault_n {
        v.cpu_errors = 1;
        let e = BurnError { worker: FAULT_THREAD as u32, log2n, seed: 1, fault: Fault::WrongResult, max_err: 0 };
        v.errors.push(ErrLine::cpu(v.elapsed_ms - (ms - FAULT_MS), &e));
    }
    v.fft = Some(sim.view.clone());
    v
}

/// Writes `anim-memtest-fail.webp` and `anim-burn-in-fail.webp` whose names contain `only`.
pub fn run(dir: &str, only: &str) {
    let paint = |v: View, p: &mut Painter| {
        let mut c = Canvas::new(W, H);
        p.paint(&v, &mut c);
        c
    };
    if "anim-memtest-fail".contains(only) {
        let mut p = Painter::new();
        let n = SWEEPS * SWEEP_MS / FRAME_MS;
        let size = encode(dir, "anim-memtest-fail", (0..n).map(|k| paint(memtest(k * FRAME_MS), &mut p)));
        println!("anim-memtest-fail.webp  {n} frames  {} KB", size / 1000);
    }
    if "anim-burn-in-fail".contains(only) {
        let mut p = Painter::new();
        let mut sim = FftSim::new(32, 4.0, Some((FAULT_THREAD, SIM_WARM + FAULT_MS / SIM_MS)));
        let mut fault_n = None;
        let n = BURN_MS / FRAME_MS;
        let size =
            encode(dir, "anim-burn-in-fail", (0..n).map(|k| paint(burn(&mut sim, k * FRAME_MS, &mut fault_n), &mut p)));
        println!("anim-burn-in-fail.webp  {n} frames  {} KB", size / 1000);
    }
}
