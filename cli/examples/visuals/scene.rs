//! The bg run as a [`View`] at time `t` s into a 6 s clip: 5950X, 4 × 16 GiB DDR4, full memory test at Modulo 20.
use memburn_core::dram::{self, Dump};
use memburn_core::engine;
use memburn_core::errlog::MemError;
use memburn_core::memtest::TestId;
use memburn_core::smbios::Dimm;
use memburn_core::tui::{BitStat, Current, MemMap, Qr, Remote, Step, StepState, Verdict, View};

pub const FPS: usize = 10;
pub const FRAMES: usize = 60;
pub const STILL: usize = 43;
pub const GIB: u64 = 1 << 30;
/// Physical address space drawn by the maps: 64 GiB DRAM with the 3-4 GiB MMIO hole remapped above 64 GiB.
pub const TOP: u64 = 65 * GIB;
pub const THREADS: usize = 32;
/// Seconds per Modulo 20 sweep of 61.6 GiB at ~34 GB/s.
pub const SWEEP_S: f64 = 1.9;
/// When the 39th error lands in the clip, s.
pub const NEW_ERROR_S: f64 = 3.0;

/// Tested physical ranges; the gaps are the MMIO hole and firmware/kernel reservations.
pub const TESTED: [(u64, u64); 5] = [
    (0x0010_0000, 0x0900_0000),
    (0x0a00_0000, 0xb800_0000),
    (0x1_0000_0000, 0x3_c000_0000),
    (0x4_4000_0000, 0xf_f000_0000),
    (0x10_0000_0000, 0x10_3c00_0000),
];

pub const CLUSTERS: [(u64, u64, &str, &str); 2] = [
    (0xa_bd16_8000, 0xa_bd17_0000, "0xabd168000", "bits 20, 22"),
    (0xf_e785_0000, 0xf_e785_8000, "0xfe7850000", "bits 1, 3"),
];

pub fn tested_total() -> u64 {
    TESTED.iter().map(|(a, b)| b - a).sum()
}

/// Offset of `phys` in the concatenated tested ranges.
pub fn offset(phys: u64) -> Option<u64> {
    let mut o = 0;
    for &(a, b) in &TESTED {
        if (a..b).contains(&phys) {
            return Some(o + phys - a);
        }
        o += b - a;
    }
    None
}

/// Head position of thread `k` in its chunk, 0..=1; threads run at slightly different speeds and wait at 1.
pub fn head(k: usize, t: f64) -> f64 {
    let speed = 1.0 + ((k * 7919) % 23) as f64 / 90.0;
    let phase = (t / SWEEP_S + 0.35).fract();
    (phase * speed * 1.1).min(1.0)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum S {
    /// Not tested: MMIO hole or reserved.
    Hole,
    Pending,
    Done,
    Head,
    Error,
    /// An error found in the last second.
    Fresh,
}

/// State of the physical slice `a..b` at `t`.
pub fn state(a: u64, b: u64, t: f64, failing: bool) -> S {
    if let Some(e) = error_at(|c0, c1| c0 < b && a < c1, t, failing) {
        return e;
    }
    match offset(a / 2 + b / 2).or_else(|| offset(a)).or_else(|| offset(b - 1)) {
        Some(o) => sweep(o, t),
        None => S::Hole,
    }
}

/// State of the tested-offset slice `a..b` at `t`: offsets run through the tested ranges, thread by thread.
pub fn state_off(a: u64, b: u64, t: f64, failing: bool) -> S {
    let hit = |c0: u64, c1: u64| match (offset(c0), offset(c1 - 1)) {
        (Some(o0), Some(o1)) => o0 < b && a <= o1,
        _ => false,
    };
    error_at(hit, t, failing).unwrap_or_else(|| sweep(a / 2 + b / 2, t))
}

fn error_at(hit: impl Fn(u64, u64) -> bool, t: f64, failing: bool) -> Option<S> {
    if !failing {
        return None;
    }
    CLUSTERS
        .iter()
        .enumerate()
        .find(|(_, c)| hit(c.0, c.1))
        .map(|(k, _)| if k == 0 && (NEW_ERROR_S..NEW_ERROR_S + 1.2).contains(&t) { S::Fresh } else { S::Error })
}

fn sweep(o: u64, t: f64) -> S {
    let chunk = tested_total() / THREADS as u64;
    let k = ((o / chunk) as usize).min(THREADS - 1);
    let f = (o - k as u64 * chunk) as f64 / chunk as f64;
    let p = head(k, t);
    let trail = 0.05;
    if p >= 1.0 || f < p - trail {
        S::Done
    } else if f < p {
        S::Head
    } else {
        S::Pending
    }
}

pub fn qr(url: &str) -> Qr {
    let q = qrcodegen::QrCode::encode_text(url, qrcodegen::QrCodeEcc::Medium).unwrap();
    let n = q.size();
    Qr { size: n as usize, dark: (0..n * n).map(|k| q.get_module(k % n, k / n)).collect() }
}

/// Ryzen 9 5950X DF3 map: 64 GiB, hole at 3 GiB, 2 channels hashed at bit 8.
fn bg_dump() -> Dump {
    let top = 65u64 << 30;
    let base = (0xC << 2) | 0b11;
    let limit = (((top - 1) >> 28) as u32) << 12;
    let regs = [
        (0x18, 0, 0x000, 0x1440_1022),
        (0x18, 0, 0x040, 0x0000_0010),
        (0x18, 0, 0x104, 0xC000_0003),
        (0x18, 0, 0x110, base),
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

/// (bit, 0->1, phys, test) in the order the bg run found them; the last lands at [`NEW_ERROR_S`].
pub fn error_seq() -> Vec<(u32, bool, u64, TestId)> {
    use TestId::*;
    let (a, b, c) = (0xabd16f218, 0xabd168348, 0xabd16af08);
    let (d, e, g, h) = (0xfe78503e0, 0xfe7853db8, 0xfe7857eb0, 0xfe7857cc0);
    vec![
        (20, true, a, AddressOwn),
        (3, true, e, AddressOwn),
        (20, false, a, AddressOwn),
        (1, true, d, MovingInvOnes),
        (3, true, g, MovingInvOnes),
        (22, true, b, MovingInvOnes),
        (20, true, a, MovingInvOnes),
        (20, true, a, MovingInvWalk8),
        (3, true, e, MovingInvWalk8),
        (1, true, d, MovingInvWalk8),
        (3, true, g, MovingInvWalk8),
        (22, true, b, MovingInvWalk8),
        (20, true, a, MovingInvWalk8),
        (3, true, e, MovingInvWalk8),
        (1, true, d, MovingInvWalk8),
        (3, true, g, MovingInvWalk8),
        (20, true, c, MovingInvWalk8),
        (3, true, e, MovingInvWalk8),
        (20, true, a, MovingInvWalk8),
        (1, true, d, MovingInvWalk8),
        (3, true, e, MovingInvWalk8),
        (3, true, g, MovingInvWalk8),
        (3, true, e, MovingInvWalk8),
        (3, true, g, MovingInvWalk8),
        (3, false, h, MovingInvWalk8),
        (3, true, e, MovingInvWalk8),
        (3, true, g, MovingInvWalk8),
        (1, true, d, MovingInvRandom),
        (20, true, a, MovingInvRandom),
        (22, true, b, MovingInvRandom),
        (1, true, d, MovingInvRandom),
        (20, true, a, MovingInvRandom),
        (20, true, a, MovingInvWalk64),
        (3, true, g, MovingInvWalk64),
        (1, true, d, MovingInvWalk64),
        (20, true, a, MovingInvWalk64),
        (20, true, a, RandomSeq),
        (22, true, b, RandomSeq),
        (20, true, c, Modulo20),
    ]
}

fn mem_series(n: usize, shift: usize) -> Vec<u32> {
    (shift..shift + n)
        .map(|k| {
            let x = k as f64;
            let noise = ((k * 7919) % 29) as f64 / 29.0 - 0.5;
            let dip = if k % 37 == 0 { -7_000.0 } else { 0.0 };
            (34_000.0 + 4_500.0 * (x * 0.17).sin() + 2_500.0 * (x * 0.051 + 1.0).sin() + 3_000.0 * noise + dip) as u32
        })
        .collect()
}

/// The dashboard at `t` s; `failing` adds the bg errors.
pub fn view(t: f64, failing: bool) -> View {
    use StepState::*;
    let url = "https://reljam.com/m/8DYEAMWR";
    let errs: [u64; 9] = if failing { [3, 4, 20, 5, 4, 0, 2, 0, 0] } else { [0; 9] };
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
    let ms = [21_000, 38_000, 61_000, 44_000, 57_000, 19_000, 33_000];
    let plan: Vec<Step> = names
        .iter()
        .enumerate()
        .map(|(i, n)| Step {
            name: (*n).into(),
            note: String::new(),
            state: match i {
                0..7 => Done { ms: ms[i], errors: errs[i] },
                7 => Running,
                8 => Pending,
                _ => Done { ms: 3_000, errors: 0 },
            },
        })
        .collect();
    let total = (616u64 << 30) / 10;
    let step = 35 + (t / SWEEP_S + 0.35) as u32;
    let done = total * (7_600 + (t * 20.0) as u64) / 10_000;
    let left = 88_000u64.saturating_sub((t * 1000.0) as u64);
    let elapsed = 284_000 + (t * 1000.0) as u64;
    let mut v = View {
        version: "0.7.0",
        system: ["AMD Ryzen 9 5950X 16-Core Processor", "32 threads", "AVX2+FMA", "62.7 GiB", "Secure Boot off"]
            .map(String::from)
            .to_vec(),
        dimms: dimms(),
        elapsed_ms: elapsed,
        verdict: Verdict::Running,
        job: "Full memory test".into(),
        plan,
        current: Current::Mem {
            test: TestId::Modulo20,
            pass: 1,
            passes: 1,
            index: 8,
            count: 9,
            step,
            steps: 60,
            done,
            total,
            left_ms: Some(left),
            job_left_ms: Some(left + 61_000),
            job_label: "job",
        },
        remote: Some(Remote { url: url.into(), status: "uploading  ·  0 s ago".into(), ok: true, qr: Some(qr(url)) }),
        keys: vec![("Esc", "stop"), ("Q", "bigger QR")],
        ..View::default()
    };
    v.perf.mem = mem_series(240, (t * 2.0) as usize);
    v.perf.peak_mem = 54_800;
    v.perf.temp_c = Some(55 + (t as i32 % 3) / 2);
    v.dram = dram::setup(&bg_dump(), &v.dimms).0;
    if failing {
        let seq = error_seq();
        let n = if t >= NEW_ERROR_S { seq.len() } else { seq.len() - 1 };
        for (k, &(bit, up, phys, test)) in seq.iter().take(n).enumerate() {
            let expected = if up { 0 } else { u64::MAX };
            let e = MemError {
                addr: 0x7f3a_0000_0000 + (phys as usize & 0xfff_ffff),
                expected,
                actual: expected ^ (1 << bit),
                test: test as u8,
                pass: 0,
            };
            let at = if k + 1 == seq.len() { elapsed } else { 20_000 + k as u64 * 6_500 };
            v.push_mem(at, &e, Some(phys));
        }
        v.mem_errors = n as u64;
        v.bits = vec![
            BitStat { bit: 20, up: 14 + u32::from(n == seq.len()), down: 1, places: 3, runs: 0 },
            BitStat { bit: 3, up: 13, down: 1, places: 3, runs: 0 },
            BitStat { bit: 1, up: 6, down: 0, places: 1, runs: 0 },
            BitStat { bit: 22, up: 4, down: 0, places: 1, runs: 0 },
        ];
    }
    v
}

/// Live view panel before the run exists.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Net {
    Up,
    Waiting,
}

/// [`view`] with the shipped memory map and fire: the engine's chunks over [`TESTED`], swept in address order.
pub fn shipped(t: f64, failing: bool, braille: bool, net: Net, quick: bool) -> View {
    let mut v = view(t, failing);
    if quick {
        quick_check(&mut v);
    }
    let regions: Vec<(usize, usize)> = TESTED.iter().map(|&(a, b)| (a as usize, (b - a) as usize)).collect();
    let total = tested_total();
    let chunks = engine::chunk_regions(&regions, engine::chunk_bytes(total, THREADS));
    let mut m = MemMap::new(&chunks, THREADS);
    m.phase(false);
    m.swept = ((t / SWEEP_S + 0.35).fract() * total as f64) as u64 / 4096 * 4096;
    if failing {
        let seq = error_seq();
        let n = if t >= NEW_ERROR_S { seq.len() } else { seq.len() - 1 };
        let new_at = 284_000 + (NEW_ERROR_S * 1000.0) as u64;
        for (k, &(bit, _, phys, _)) in seq.iter().take(n).enumerate() {
            m.error(phys, phys, 1 << bit, if k + 1 == seq.len() { new_at } else { 0 });
        }
    }
    v.map = Some(m);
    v.braille = braille;
    v.keys = vec![("Esc", "stop"), ("Q", "bigger QR")];
    if net == Net::Waiting {
        v.remote = Some(Remote { url: String::new(), status: "waiting for network".into(), ok: false, qr: None });
        v.keys.truncate(1);
    }
    v
}

/// Turns [`view`] into a Quick check at its hashed pattern, for screens too short for the full plan and a map.
fn quick_check(v: &mut View) {
    use StepState::*;
    let errs = |k: usize| v.plan.get(k).map_or(0, |s| if let Done { errors, .. } = s.state { errors } else { 0 });
    let names = ["Address + block moves", "Moving inversions, 8-bit walk", "Random sequence ×5", "Hashed pattern"];
    let done = [errs(0) + errs(5), errs(2), errs(6)];
    let mut plan: Vec<Step> = names
        .iter()
        .enumerate()
        .map(|(i, n)| Step {
            name: (*n).into(),
            note: String::new(),
            state: if i < 3 { Done { ms: [24_000, 61_000, 33_000][i], errors: done[i] } } else { Running },
        })
        .collect();
    plan.push(Step { name: "FFT burn-in".into(), note: "0:02:00".into(), state: Pending });
    v.plan = plan;
    v.job = "Quick check".into();
    if let Current::Mem { test, index, count, .. } = &mut v.current {
        (*test, *index, *count) = (TestId::Hashed, 4, 4);
    }
}
