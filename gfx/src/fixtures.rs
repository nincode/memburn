//! Fixed dashboard states shared by the snapshot tests and the headless renders (`--example visuals -- <dir> states`).

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use core::sync::atomic::Ordering::Relaxed;
use memburn_core::dram::{self, Dump};
use memburn_core::engine;
use memburn_core::errlog::MemError;

use memburn_core::fft::lane::hist_bin;
use memburn_core::fft::{BurnError, BurnStats, Fault};
use memburn_core::memtest::TestId;
use memburn_core::smbios::Dimm;
use memburn_core::tui::{
    self, BitStat, Current, ErrLine, FftView, MemMap, Qr, Remote, Step, StepState, UpdateLine, UpdateTone, Verdict,
    View,
};

use crate::{Canvas, Painter};

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

pub fn qr(url: &str) -> Qr {
    let q = qrcodegen::QrCode::encode_text(url, qrcodegen::QrCodeEcc::Medium).unwrap();
    let n = q.size();
    Qr { size: n as usize, dark: (0..n * n).map(|k| q.get_module(k % n, k / n)).collect() }
}

/// Ryzen 9 5950X DF3 map: 64 GiB, hole at 3 GiB, 2 channels hashed at bit 8.
pub fn dump() -> Dump {
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

pub fn dimms() -> Vec<Dimm> {
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

/// i9-13900HX laptop: `n` SK Hynix DDR5 SODIMMs behind Intel's long locators.
pub fn laptop_dimms(n: usize) -> Vec<Dimm> {
    (0..n)
        .map(|k| Dimm {
            locator: format!("Controller{}-Channel{}-DIMM0", k / 2, ["A", "B"][k % 2]),
            bank: format!("BANK {k}"),
            size_mib: 16384,
            speed: 5600,
            configured: 5600,
            manufacturer: "SK Hynix".into(),
            part: "HMCG78AGBSA095N".into(),
            serial: format!("8A1B2C{k:02X}"),
        })
        .collect()
}

/// Quick check on the laptop of [`laptop_dimms`]: `running` is the plan row running, the rows before it passed.
pub fn laptop(n: usize, running: Option<usize>) -> View {
    let mut v = base(false);
    v.system =
        ["Intel Core i9-13900HX", "32 threads", "AVX2+FMA", "31.7 GiB", "Secure Boot on"].map(String::from).to_vec();
    v.dimms = laptop_dimms(n);
    v.dram = None;
    v.job = "Quick check".into();
    v.plan = ["Own address", "Moving inversions, 8-bit walk", "Random sequence ×5", "Hashed pattern", "FFT burn-in"]
        .iter()
        .enumerate()
        .map(|(i, name)| Step {
            name: (*name).into(),
            note: if i == 4 { "0:02:00" } else { "" }.into(),
            state: match running {
                Some(r) if i == r => StepState::Running,
                Some(r) if i > r => StepState::Pending,
                _ => StepState::Done { ms: [4_100, 38_200, 61_900, 21_400, 120_000][i], errors: 0 },
            },
        })
        .collect();
    let total = 296 * GIB / 10;
    v.current = Current::Mem {
        test: TestId::RandomSeq,
        pass: 1,
        passes: 1,
        index: 3,
        count: 4,
        step: 7,
        steps: 10,
        done: total * 31 / 100,
        total,
        left_ms: Some(18_000),
        job_left_ms: Some(156_000),
        job_label: "job",
        section: 612,
    };
    v.elapsed_ms = 57_000;
    v
}

/// The laptop's result: the rows memburn shows on Linux, then the modules.
pub fn laptop_result(n: usize) -> View {
    let mut v = laptop(n, None);
    v.verdict = Verdict::Pass;
    let mut rows: Vec<(String, String)> = [
        ("Memory tested", "29.6 GiB, 4 tests"),
        ("Burn-in", "2:00, 1 284 556 FFTs, no errors"),
        ("Coverage", "29.6 of 31.7 GiB System RAM (over 3 runs 30.1)"),
        ("Earlier runs", "2, no errors"),
        ("Peak CPU temp", "94 °C"),
        ("Peak RAM temp", "51 °C"),
        ("Total time", "0:04:06"),
    ]
    .map(|(l, v)| (l.into(), v.into()))
    .to_vec();
    rows.extend(tui::module_rows(&v.dimms, &[], 0));
    v.current = Current::Summary(rows);
    v.advice = "29.6 GiB memory and 1 284 556 FFTs checked in 0:04:06".into();
    v.elapsed_ms = 246_000;
    v.keys = vec![("F", "full memory"), ("B", "burn-in"), ("A", "all"), ("R", "restart"), ("P", "power off")];
    v
}

/// (bit, 0->1, phys, test) as a failing run found them.
pub fn error_seq() -> Vec<(u32, bool, u64, TestId)> {
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

pub fn mem_series(n: usize) -> Vec<u32> {
    (0..n)
        .map(|k| {
            let x = k as f64;
            let noise = ((k * 7919) % 29) as f64 / 29.0 - 0.5;
            let dip = if k % 37 == 0 { -7_000.0 } else { 0.0 };
            (34_000.0 + 4_500.0 * (x * 0.17).sin() + 2_500.0 * (x * 0.051 + 1.0).sin() + 3_000.0 * noise + dip) as u32
        })
        .collect()
}

pub fn plan(errs: [u64; 9], running: Option<usize>) -> Vec<Step> {
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

pub fn map(swept_pct: u64) -> MemMap {
    let regions: Vec<(usize, usize)> = TESTED.iter().map(|&(a, b)| (a as usize, (b - a) as usize)).collect();
    let total: u64 = TESTED.iter().map(|(a, b)| b - a).sum();
    let chunks = engine::chunk_regions(&regions, engine::chunk_bytes(total, THREADS));
    let mut m = MemMap::new(&chunks, THREADS);
    m.phase(false);
    m.swept = total * swept_pct / 100 / 4096 * 4096;
    m
}

pub fn base(failing: bool) -> View {
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
            section: 667,
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

pub fn menu() -> View {
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

pub fn summary(v: &mut View, pass: bool) {
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

pub fn scenes() -> Vec<(&'static str, View)> {
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

/// `threads` burners at `gflops` each through the sampler at 100 ms for `samples` samples, sizes 2^10..2^21
/// rotating every 4 s, rounding errors spread around each size's typical one; `fault` breaks one transform on that
/// thread 1.5 s before the end.
pub fn fft(threads: usize, gflops: f64, fault: Option<usize>, samples: u64) -> FftView {
    let mut sim = FftSim::new(threads, gflops, fault.map(|i| (i, samples - 15)));
    for _ in 0..samples {
        sim.step();
    }
    sim.view
}

/// The burners behind [`fft`], one 100 ms sample per [`FftSim::step`]; `fault` is (thread, sample).
pub struct FftSim {
    pub view: FftView,
    pub stats: BurnStats,
    /// Samples taken.
    pub t: u64,
    gflops: f64,
    fault: Option<(usize, u64)>,
    acc: Vec<u64>,
    rng: u64,
}

impl FftSim {
    pub fn new(threads: usize, gflops: f64, fault: Option<(usize, u64)>) -> FftSim {
        let stats = BurnStats::with_lanes(threads);
        let view = FftView::new(&stats, threads, 0);
        FftSim { view, stats, t: 0, gflops, fault, acc: vec![0; threads], rng: 0x9e37_79b9_7f4a_7c15 }
    }

    pub fn step(&mut self) {
        const SPREAD: [u64; 9] = [1, 3, 8, 15, 20, 15, 8, 3, 1];
        self.t += 1;
        let t = self.t;
        let rng = &mut self.rng;
        let mut next = move || {
            *rng ^= *rng << 13;
            *rng ^= *rng >> 7;
            *rng ^= *rng << 17;
            *rng
        };
        for (i, l) in self.stats.lanes.iter().enumerate() {
            let log2n = 10 + ((i * 5 + t as usize / 40) % 12) as u32;
            let per = 10 * (1u64 << log2n) * log2n as u64;
            self.acc[i] += (self.gflops * 1e8 * (0.9 + (next() % 200) as f64 / 1000.0)) as u64;
            let done = self.acc[i] / per;
            self.acc[i] %= per;
            let mid = hist_bin(10f64.powf(-3.6 + 0.12 * (log2n as f64 - 10.0)).to_bits());
            let mut left = done;
            for (k, w) in SPREAD.iter().enumerate() {
                let n = (done * w * (60 + next() % 80) / 100 / 74).min(left);
                l.hist[(mid + k).saturating_sub(4)].fetch_add(n as u32, Relaxed);
                left -= n;
            }
            l.hist[mid].fetch_add(left as u32, Relaxed);
            if done > 0 {
                let e = 10f64.powf(-3.6 + 0.12 * (log2n as f64 - 10.0) + (next() % 100) as f64 / 250.0);
                l.last_err.store(e.to_bits(), Relaxed);
            }
            if self.fault == Some((i, t)) {
                l.faults.fetch_add(1, Relaxed);
                l.fault.store(Fault::WrongResult.code(), Relaxed);
            }
            l.ffts.fetch_add(done, Relaxed);
            l.flops.fetch_add(done * per, Relaxed);
            l.log2n.store(log2n, Relaxed);
            l.slice.store(((t % 40) * 500_000_000).min(memburn_core::fft::SLICE_FLOPS), Relaxed);
        }
        self.stats.max_err.store(0.0629f64.to_bits(), Relaxed);
        self.view.sample(&self.stats, t * 100);
    }
}

/// GFLOP/s of each fixture burner.
const LANE_GFLOPS: f64 = 4.0;

/// Compute samples in tenths of GFLOP/s around `n` fixture burners' total.
fn gflops_series(n: usize, len: usize) -> Vec<u32> {
    let base = (n as f64 * LANE_GFLOPS * 10.0) as u32;
    (0..len).map(|k| base * 95 / 100 + ((k * 104_729) % 17) as u32 * base / 160).collect()
}

/// A Full memory test's FFT burn-in after its memory tests.
pub fn burn(failing: bool) -> View {
    let mut v = base(failing);
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
    v.perf.gflops = gflops_series(THREADS, 200);
    v.map = None;
    v.fft = Some(fft(THREADS, LANE_GFLOPS, failing.then_some(6), 150));
    if failing {
        cpu_fault(&mut v);
    }
    v
}

/// The fault [`fft`] puts on thread 6, in the errors panel.
fn cpu_fault(v: &mut View) {
    v.cpu_errors += 1;
    let cpu = BurnError { worker: 6, log2n: 16, seed: 1, fault: Fault::WrongResult, max_err: 0 };
    v.errors.push(ErrLine::cpu(v.elapsed_ms - 1500, &cpu));
}

/// Stress: memory test on half the threads, FFTs on the other half.
pub fn stress(failing: bool) -> View {
    let mut v = base(failing);
    v.job = "Stress".into();
    let note = String::from("0:30:00");
    v.plan = vec![Step { name: "Memory test + FFT together".into(), note, state: StepState::Running }];
    if let Current::Mem { test, pass, passes, index, count, job_label, .. } = &mut v.current {
        (*test, *pass, *passes, *index, *count, *job_label) = (TestId::RandomSeq, 3, 3, 6, 9, "job");
    }
    v.perf.gflops = gflops_series(THREADS / 2, 240);
    v.fft = Some(fft(THREADS / 2, LANE_GFLOPS, None, 150));
    v
}

/// Review-only states, rendered with the snapshot scenes.
pub fn extra() -> Vec<(&'static str, View)> {
    let mut out = Vec::new();
    let mut v = menu();
    v.verdict = Verdict::Menu(None);
    v.notice =
        "Previous run did not finish: Quick check, 2026-10-12 11:00 UTC  ·  memburn-crash-2026-10-12-1102.txt".into();
    v.keys = vec![("Enter", "quick"), ("F", "full memory"), ("B", "burn-in"), ("R", "restart"), ("P", "power off")];
    out.push(("menu-notice", v));
    out.push(("burn-in", burn(false)));
    out.push(("burn-in-fail", burn(true)));
    let mut v = burn(false);
    v.verdict = Verdict::Stopping;
    out.push(("burn-stopping", v));
    let mut v = burn(false);
    v.fft = Some(fft(THREADS, LANE_GFLOPS, Some(6), 150));
    cpu_fault(&mut v);
    out.push(("burn-in-cpu-fault", v));
    for n in [16, 4] {
        let mut v = burn(false);
        v.fft = Some(fft(n, LANE_GFLOPS, None, 150));
        v.perf.gflops = gflops_series(n, 200);
        if let Current::Burn { threads, .. } = &mut v.current {
            *threads = n as u32;
        }
        out.push((if n == 16 { "burn-in-16" } else { "burn-in-4" }, v));
    }
    out.push(("stress", stress(false)));
    out.push(("stress-fail", stress(true)));
    let mut v = base(false);
    v.remote = Some(Remote { url: String::new(), status: "waiting for network".into(), ok: false, qr: None });
    v.keys.truncate(1);
    out.push(("waiting-network", v));
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
    let line = |text: &str, detail: &str, bar: Option<(u64, u64)>, tone| UpdateLine {
        text: text.into(),
        detail: detail.into(),
        bar,
        tone,
    };
    let mut v = menu();
    v.update = Some(line("checking for updates", "", None, UpdateTone::Busy));
    out.push(("update-checking", v));
    let mut v = base(false);
    v.update = Some(line("downloading 0.9.1", "12.3 of 35.1 MB", Some((123, 351)), UpdateTone::Busy));
    v.keys.insert(0, ("U", "update to 0.9.1"));
    out.push(("update-downloading", v));
    let mut v = base(false);
    v.update = Some(line("verifying 0.9.1", "file 4 of 13", Some((3, 13)), UpdateTone::Busy));
    v.keys.insert(0, ("U", "update to 0.9.1"));
    out.push(("update-verifying", v));
    let mut v = base(false);
    v.update = Some(line("0.9.1 ready", "U to start now", None, UpdateTone::Ready));
    v.keys.insert(0, ("U", "update to 0.9.1"));
    out.push(("update-ready", v));
    let mut v = base(false);
    v.remote = None;
    v.update = Some(line("verifying 0.9.1", "file 4 of 13", Some((3, 13)), UpdateTone::Busy));
    out.push(("update-no-live", v));
    let mut v = base(false);
    summary(&mut v, true);
    v.update = Some(line("0.9.1 installed", "starts on R", None, UpdateTone::Ready));
    v.keys.insert(0, ("U", "start 0.9.1"));
    out.push(("update-installed", v));
    let mut v = menu();
    v.update = Some(line("up to date (0.9.1)", "", None, UpdateTone::Quiet));
    out.push(("update-up-to-date", v));
    let mut v = menu();
    v.remote = None;
    v.update = Some(line("update check", "offline", None, UpdateTone::Error));
    out.push(("update-offline", v));
    out.push(("quick-memory-mid", laptop(2, Some(2))));
    let mut v = base(false);
    v.current = Current::Hold { left_ms: 41_000, section: 702 };
    out.push(("memory-hold", v));
    let mut v = laptop(2, Some(4));
    v.current = Current::Burn {
        isa: "AVX2+FMA",
        threads: 32,
        sizes: "{10..21}".into(),
        ffts: 512_300,
        elapsed_ms: 48_000,
        total_ms: 120_000,
        worst: 0.21f64.to_bits(),
    };
    v.map = None;
    v.perf.gflops = gflops_series(THREADS, 200);
    v.fft = Some(fft(THREADS, LANE_GFLOPS, None, 150));
    out.push(("quick-burn-in", v));
    out.push(("result-2-modules", laptop_result(2)));
    out.push(("result-4-modules", laptop_result(4)));
    out
}

/// Every named state: the snapshot scenes, then [`extra`].
pub fn states() -> Vec<(&'static str, View)> {
    let mut v = scenes();
    v.extend(extra());
    v
}

/// FNV-1a over the pixels.
pub fn fnv(px: &[u32]) -> u64 {
    px.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &p| (h ^ p as u64).wrapping_mul(0x100_0000_01b3))
}

pub fn render(v: &View, w: usize, h: usize) -> Canvas {
    let mut c = Canvas::new(w, h);
    Painter::new().paint(v, &mut c);
    c
}

/// [`fnv`] of each of [`scenes`] at 1280×800.
pub const SNAPSHOTS: [(&str, u64); 5] = [
    ("running-clean", 0xb3c7a2947a35fa64),
    ("running-fail", 0xaeac9069cc1c3452),
    ("menu", 0x63505246340c6297),
    ("result-pass", 0x5209d34ce6edff6e),
    ("result-fail", 0xbeb0423afa32abec),
];
