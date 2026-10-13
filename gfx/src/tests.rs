//! Fixed dashboards: snapshot hashes, frame times, and PPM renders into `$MEMBURN_GFX_OUT` for review.

use std::time::Instant;

use memburn_core::dram::{self, Dump};
use memburn_core::engine;
use memburn_core::errlog::MemError;
use memburn_core::fft::{BurnError, Fault};
use memburn_core::memtest::TestId;
use memburn_core::smbios::Dimm;
use memburn_core::tui::{self, BitStat, Current, ErrLine, MemMap, Qr, Remote, Step, StepState, Verdict, View};

use super::*;

const GIB: u64 = 1 << 30;
const THREADS: usize = 32;
/// Tested physical ranges of a 64 GiB machine with the MMIO hole remapped above 64 GiB.
const TESTED: [(u64, u64); 5] = [
    (0x0010_0000, 0x0900_0000),
    (0x0a00_0000, 0xb800_0000),
    (0x1_0000_0000, 0x3_c000_0000),
    (0x4_4000_0000, 0xf_f000_0000),
    (0x10_0000_0000, 0x10_3c00_0000),
];

fn qr(url: &str) -> Qr {
    let q = qrcodegen::QrCode::encode_text(url, qrcodegen::QrCodeEcc::Medium).unwrap();
    let n = q.size();
    Qr { size: n as usize, dark: (0..n * n).map(|k| q.get_module(k % n, k / n)).collect() }
}

/// Ryzen 9 5950X DF3 map: 64 GiB, hole at 3 GiB, 2 channels hashed at bit 8.
fn dump() -> Dump {
    let top = 65u64 << 30;
    let limit = (((top - 1) >> 28) as u32) << 12;
    let regs = [
        (0x18, 0, 0x000, 0x1440_1022),
        (0x18, 0, 0x040, 0x0000_0010),
        (0x18, 0, 0x104, 0xC000_0003),
        (0x18, 0, 0x110, (0xC << 2) | 0b11),
        (0x18, 0, 0x114, limit),
        (0x18, 0, 0x118, 0),
        (0x18, 0, 0x11C, 0),
        (0x18, 0, 0x1B4, 0),
        (0x18, 0, 0x3F8, 0x0070_0000),
        (0x18, 1, 0x150, 0),
        (0x18, 1, 0x208, 0x0300_003F),
        (0x18, 1, 0x20C, 0x0000_0008),
        (0x19, 0, 0x000, u32::MAX),
    ];
    Dump { cpuid: 0x00A2_0F10, regs: regs.to_vec() }
}

fn dimms() -> Vec<Dimm> {
    ["A1", "A2", "B1", "B2"]
        .map(|s| Dimm {
            locator: format!("DIMM_{s}"),
            bank: format!("P0 CHANNEL {}", &s[..1]),
            size_mib: 16384,
            speed: 3200,
            configured: 3200,
            ..Dimm::default()
        })
        .to_vec()
}

/// (bit, 0->1, phys, test) as a failing run found them.
fn error_seq() -> Vec<(u32, bool, u64, TestId)> {
    use TestId::*;
    let (a, b, c) = (0xabd16f218, 0xabd168348, 0xabd16af08);
    let (d, e, g) = (0xfe78503e0, 0xfe7853db8, 0xfe7857eb0);
    let mut v = vec![(20, true, a, AddressOwn), (3, true, e, AddressOwn), (1, true, d, MovingInvOnes)];
    for k in 0..30 {
        let (bit, phys) = [(20, a), (3, g), (22, b), (1, d), (3, e)][k % 5];
        v.push((bit, true, phys, if k < 20 { MovingInvWalk8 } else { RandomSeq }));
    }
    v.push((20, true, c, Modulo20));
    v
}

fn mem_series(n: usize) -> Vec<u32> {
    (0..n)
        .map(|k| {
            let x = k as f64;
            let noise = ((k * 7919) % 29) as f64 / 29.0 - 0.5;
            let dip = if k % 37 == 0 { -7_000.0 } else { 0.0 };
            (34_000.0 + 4_500.0 * (x * 0.17).sin() + 2_500.0 * (x * 0.051 + 1.0).sin() + 3_000.0 * noise + dip) as u32
        })
        .collect()
}

fn plan(errs: [u64; 9], running: Option<usize>) -> Vec<Step> {
    let names = [
        "Own address",
        "Moving inversions, 0s/1s",
        "Moving inversions, 8-bit walk",
        "Moving inversions, random",
        "Moving inversions, 64-bit walk",
        "Block move",
        "Random sequence",
        "Modulo 20",
        "Hashed pattern",
        "Kernel early test 2 patterns",
    ];
    names
        .iter()
        .enumerate()
        .map(|(i, n)| Step {
            name: (*n).into(),
            note: String::new(),
            state: match (i, running) {
                (9, _) => StepState::Done { ms: 3_000, errors: 0 },
                (i, Some(r)) if i == r => StepState::Running,
                (i, Some(r)) if i > r => StepState::Pending,
                _ => StepState::Done { ms: 20_000 + i as u64 * 4_000, errors: errs[i] },
            },
        })
        .collect()
}

fn map(swept_pct: u64) -> MemMap {
    let regions: Vec<(usize, usize)> = TESTED.iter().map(|&(a, b)| (a as usize, (b - a) as usize)).collect();
    let total: u64 = TESTED.iter().map(|(a, b)| b - a).sum();
    let chunks = engine::chunk_regions(&regions, engine::chunk_bytes(total, THREADS));
    let mut m = MemMap::new(&chunks, THREADS);
    m.phase(false);
    m.swept = total * swept_pct / 100 / 4096 * 4096;
    m
}

fn base(failing: bool) -> View {
    let url = "https://reljam.com/m/8DYEAMWR";
    let errs: [u64; 9] = if failing { [3, 4, 20, 5, 4, 0, 2, 0, 0] } else { [0; 9] };
    let total = 616 * GIB / 10;
    let mut v = View {
        version: "0.9.0",
        system: ["AMD Ryzen 9 5950X 16-Core Processor", "32 threads", "AVX2+FMA", "62.7 GiB", "Secure Boot off"]
            .map(String::from)
            .to_vec(),
        dimms: dimms(),
        elapsed_ms: 288_000,
        verdict: Verdict::Running,
        job: "Full memory test".into(),
        plan: plan(errs, Some(7)),
        current: Current::Mem {
            test: TestId::Modulo20,
            pass: 1,
            passes: 1,
            index: 8,
            count: 9,
            step: 37,
            steps: 60,
            done: total * 77 / 100,
            total,
            left_ms: Some(83_000),
            job_left_ms: Some(144_000),
            job_label: "job",
        },
        remote: Some(Remote { url: url.into(), status: "uploading  ·  0 s ago".into(), ok: true, qr: Some(qr(url)) }),
        keys: vec![("Esc", "stop"), ("Q", "bigger QR")],
        ..View::default()
    };
    v.perf.mem = mem_series(240);
    v.perf.peak_mem = 54_800;
    v.perf.temp_c = Some(55);
    v.dram = dram::setup(&dump(), &v.dimms).0;
    let mut m = map(52);
    if failing {
        let seq = error_seq();
        for (k, &(bit, up, phys, test)) in seq.iter().enumerate() {
            let expected = if up { 0 } else { u64::MAX };
            let e =
                MemError { addr: phys as usize, expected, actual: expected ^ (1 << bit), test: test as u8, pass: 0 };
            let at = if k + 1 == seq.len() { v.elapsed_ms - 400 } else { 20_000 + k as u64 * 6_500 };
            v.push_mem(at, &e, Some(phys));
            m.error(phys, phys, 1 << bit, at);
        }
        v.mem_errors = seq.len() as u64;
        v.bits = vec![
            BitStat { bit: 20, up: 15, down: 1, places: 3, runs: 0 },
            BitStat { bit: 3, up: 13, down: 1, places: 3, runs: 0 },
            BitStat { bit: 1, up: 6, down: 0, places: 1, runs: 2 },
            BitStat { bit: 22, up: 4, down: 0, places: 1, runs: 0 },
        ];
    }
    v.map = Some(m);
    v
}

fn menu() -> View {
    let mut v = base(false);
    v.verdict = Verdict::Menu(Some(7));
    v.job = "Quick check".into();
    v.elapsed_ms = 0;
    v.plan = ["Own address", "Moving inversions, 8-bit walk", "Random sequence ×5", "Hashed pattern", "FFT burn-in"]
        .iter()
        .map(|n| Step {
            name: (*n).into(),
            note: if *n == "FFT burn-in" { "0:02:00" } else { "" }.into(),
            state: StepState::Pending,
        })
        .collect();
    v.current = Current::Menu(vec![
        ("Enter", "Quick check".into(), "~3 min".into()),
        ("F", "Full memory test".into(), "~6 min".into()),
        ("B", "Burn-in".into(), "30 min".into()),
        ("S", "Stress: memory + FFT".into(), "30 min".into()),
        ("C", "Core cycler".into(), "2 min per CPU".into()),
        ("A", "All, repeating".into(), "until Esc".into()),
    ]);
    v.perf = Default::default();
    v.perf.temp_c = Some(41);
    v.map = None;
    v.remote = Some(Remote { url: String::new(), status: "waiting for network".into(), ok: false, qr: None });
    v.keys = vec![("R", "restart"), ("P", "power off")];
    v
}

fn summary(v: &mut View, pass: bool) {
    v.verdict = if pass { Verdict::Pass } else { Verdict::Fail };
    let mut rows: Vec<(String, String)> = vec![
        ("Memory tested".into(), "61.6 GiB, 9 tests".into()),
        ("Coverage".into(), "61.6 of 62.7 GiB System RAM".into()),
        ("Peak CPU temp".into(), "71 °C".into()),
        ("Total time".into(), "0:06:12".into()),
    ];
    let flipped = v.bits.iter().fold(0u64, |m, b| m | 1 << b.bit);
    rows.extend(tui::module_rows(&v.dimms, &v.suspects, flipped));
    v.current = Current::Summary(rows);
    v.plan = plan(if pass { [0; 9] } else { [3, 4, 20, 5, 4, 0, 2, 1, 0] }, None);
    v.elapsed_ms = 372_000;
    v.advice = if pass {
        "61.6 GiB checked by 9 tests in 0:06:12".into()
    } else {
        "Reset memory to stock speed, then test one stick at a time to find the faulty one.".into()
    };
    v.keys = vec![("F", "full memory"), ("B", "burn-in"), ("A", "all"), ("R", "restart"), ("P", "power off")];
}

fn scenes() -> Vec<(&'static str, View)> {
    let mut out = vec![("running-clean", base(false)), ("running-fail", base(true)), ("menu", menu())];
    let mut v = base(false);
    summary(&mut v, true);
    out.push(("result-pass", v));
    let mut v = base(true);
    v.cpu_errors = 1;
    let cpu = BurnError { worker: 6, log2n: 15, seed: 1, fault: Fault::WrongResult, max_err: 0 };
    v.errors.push(ErrLine::cpu(300_000, &cpu));
    summary(&mut v, false);
    out.push(("result-fail", v));
    out
}

/// Review-only states, rendered with the snapshot scenes.
fn extra() -> Vec<(&'static str, View)> {
    let mut out = Vec::new();
    let mut v = menu();
    v.verdict = Verdict::Menu(None);
    v.notice =
        "Previous run did not finish: Quick check, 2026-10-12 11:00 UTC  ·  memburn-crash-2026-10-12-1102.txt".into();
    v.keys = vec![("Enter", "quick"), ("F", "full memory"), ("B", "burn-in"), ("R", "restart"), ("P", "power off")];
    out.push(("menu-notice", v));
    let mut v = base(false);
    v.plan[7].state = StepState::Done { ms: 40_000, errors: 0 };
    v.plan[8].state = StepState::Done { ms: 40_000, errors: 0 };
    v.plan.push(Step { name: "FFT burn-in".into(), note: String::new(), state: StepState::Running });
    v.current = Current::Burn {
        isa: "AVX2+FMA",
        threads: 32,
        sizes: "{10..21}".into(),
        ffts: 3_045_300,
        elapsed_ms: 42_000,
        total_ms: 120_000,
        worst: 0.0629f64.to_bits(),
    };
    v.perf.gflops = (0..200).map(|k| 6400 + ((k * 104_729) % 17) as u32 * 30).collect();
    v.verdict = Verdict::Stopping;
    out.push(("burn-stopping", v));
    let mut v = base(true);
    v.qr_zoom = true;
    v.keys = vec![("Q", "smaller QR"), ("Esc", "stop")];
    out.push(("qr-zoom", v));
    let mut v = base(true);
    v.remote = None;
    v.dimms.truncate(2);
    v.verdict = Verdict::Stopped;
    summary(&mut v, false);
    v.verdict = Verdict::Stopped;
    out.push(("stopped-uefi", v));
    out
}

fn fnv(px: &[u32]) -> u64 {
    px.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &p| (h ^ p as u64).wrapping_mul(0x100_0000_01b3))
}

fn render(v: &View, w: usize, h: usize) -> Canvas {
    let mut c = Canvas::new(w, h);
    Painter::new().paint(v, &mut c);
    c
}

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
    for (name, v) in scenes().into_iter().chain(extra()) {
        for (w, h) in [(1280, 800), (1920, 1080), (1024, 768), (3840, 2160)] {
            ppm(&render(&v, w, h), &dir, &format!("{name}-{w}x{h}"));
        }
    }
}

#[test]
fn every_state_renders_at_every_size() {
    for (_, v) in scenes().into_iter().chain(extra()) {
        for (w, h) in [(640, 480), (1024, 768), (1280, 1024), (2560, 1080), (3840, 2160)] {
            let c = render(&v, w, h);
            assert!(c.px.iter().any(|&p| p != c.px[0]));
        }
    }
}

#[test]
fn snapshots() {
    let got: Vec<(&str, u64)> = scenes().iter().map(|(n, v)| (*n, fnv(&render(v, 1280, 800).px))).collect();
    let want = [
        ("running-clean", 0xe9132a0aa48d5705),
        ("running-fail", 0x8c80078706813487),
        ("menu", 0x63505246340c6297),
        ("result-pass", 0xaad68a7df3469492),
        ("result-fail", 0x5f34db03814885f8),
    ];
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
        for (name, v) in scenes() {
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
}
