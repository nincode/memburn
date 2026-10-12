//! Writes dashboard scenes as cell dumps (chars, fg, bg rows) for UI review renders.
use std::fmt::Write as _;

use memburn_core::errlog::MemError;
use memburn_core::fft::{BurnError, Fault};
use memburn_core::memtest::TestId;
use memburn_core::smbios::Dimm;
use memburn_core::tui::{self, BitStat, Current, ErrLine, Frame, Perf, Qr, Remote, Step, StepState, Verdict, View};

fn qr(url: &str) -> Qr {
    let q = qrcodegen::QrCode::encode_text(url, qrcodegen::QrCodeEcc::Medium).unwrap();
    let n = q.size();
    Qr { size: n as usize, dark: (0..n * n).map(|k| q.get_module(k % n, k / n)).collect() }
}

fn quick_plan(states: [StepState; 4]) -> Vec<Step> {
    let hashed =
        if states[3] == StepState::Pending { StepState::Pending } else { StepState::Done { ms: 2000, errors: 0 } };
    let names = [
        ("Address + block moves", "", states[0]),
        ("Moving inversions, 8-bit walk", "", states[1]),
        ("Random sequence ×5", "", states[2]),
        ("Hashed pattern", "", hashed),
        ("FFT burn-in", "0:02:00", states[3]),
    ];
    names.iter().map(|&(n, note, state)| Step { name: n.into(), note: note.into(), state }).collect()
}

fn base(verdict: Verdict, plan: Vec<Step>, current: Current) -> View {
    let url = "https://reljam.com/m/K7Q2XH9P";
    View {
        version: "0.2.0",
        system: ["AMD Ryzen 7 8845HS", "16 threads", "AVX2+FMA", "31.3 GiB"].map(String::from).to_vec(),
        dimms: ["A", "B"]
            .map(|ch| Dimm {
                locator: "DIMM 0".into(),
                bank: format!("P0 CHANNEL {ch}"),
                size_mib: 16384,
                speed: 5600,
                configured: 5600,
                part: "CT16G56C46S5.M8G1".into(),
                serial: format!("E816{ch}5AB"),
                ..Dimm::default()
            })
            .to_vec(),
        elapsed_ms: 0,
        verdict,
        job: "Quick check".into(),
        plan,
        current,
        perf: Perf::default(),
        mem_errors: 0,
        cpu_errors: 0,
        bits: vec![],
        errors: vec![],
        advice: String::new(),
        remote: Some(Remote { url: url.into(), status: "uploading  ·  2 s ago".into(), ok: true, qr: Some(qr(url)) }),
        keys: vec![("Esc", "stop"), ("Q", "QR code")],
        ..View::default()
    }
}

fn mem_series(n: usize) -> Vec<u32> {
    (0..n).map(|k| 21_000 + ((k * 7919) % 13) as u32 * 300 + if k % 9 == 0 { 2500 } else { 0 }).collect()
}

fn fl_series(n: usize) -> Vec<u32> {
    (0..n).map(|k| 640 + ((k * 104_729) % 17) as u32 * 14 + (k as u32).min(20) * 3).collect()
}

fn mem_err(addr: usize, expected: u64, bit: u32, test: TestId) -> MemError {
    MemError { addr, expected, actual: expected ^ (1 << bit), test: test as u8, pass: 0 }
}

fn mem_test(test: TestId, index: u32, step: u32, steps: u32, pct: u64) -> Current {
    let total = 30u64 << 30;
    Current::Mem {
        test,
        pass: 1,
        passes: 1,
        index,
        count: 3,
        step,
        steps,
        done: total * pct / 100,
        total,
        left_ms: Some(31_000),
        job_left_ms: Some(152_000),
        job_label: "job",
    }
}

pub fn scenes() -> Vec<(&'static str, usize, usize, View)> {
    use StepState::*;
    let mut out = Vec::new();

    let menu = || {
        Current::Menu(vec![
            ("Enter", "Quick check".into(), "~3 min".into()),
            ("F", "Full memory test".into(), "~25 min".into()),
            ("B", "Burn-in".into(), "30 min".into()),
            ("A", "All, repeating".into(), "until Esc".into()),
        ])
    };
    let mut v = base(Verdict::Menu(Some(7)), quick_plan([Pending; 4]), menu());
    v.keys = vec![("R", "restart"), ("P", "power off")];
    out.push(("1-menu", 100, 31, v));

    let mut v = base(Verdict::Menu(None), quick_plan([Pending; 4]), menu());
    v.notice =
        "Previous run did not finish: Quick check, 2026-10-12 11:00 UTC  ·  memburn-crash-2026-10-12-1102.txt".into();
    v.keys = vec![("Enter", "quick"), ("F", "full memory"), ("B", "burn-in"), ("R", "restart"), ("P", "power off")];
    out.push(("1b-menu-unfinished", 100, 31, v));

    let mut v = base(
        Verdict::Running,
        quick_plan([Done { ms: 4000, errors: 0 }, Running, Pending, Pending]),
        mem_test(TestId::MovingInvOnes, 2, 3, 6, 48),
    );
    v.elapsed_ms = 9_000;
    v.perf.mem = mem_series(60);
    v.perf.temp_c = Some(64);
    out.push(("2-memory-running", 100, 31, v));

    let mut v = base(
        Verdict::Running,
        quick_plan([Done { ms: 4000, errors: 0 }, Done { ms: 11_000, errors: 9 }, Running, Pending]),
        mem_test(TestId::RandomSeq, 3, 2, 3, 71),
    );
    v.elapsed_ms = 24_000;
    v.perf.mem = mem_series(90);
    v.perf.temp_c = Some(66);
    v.mem_errors = 14;
    v.bits = vec![
        BitStat { bit: 17, up: 12, down: 0, places: 2, runs: 1 },
        BitStat { bit: 3, up: 1, down: 1, places: 2, runs: 0 },
    ];
    let errs = [
        (6_100, mem_err(0x2_8850_ee08, 0, 17, TestId::MovingInvOnes)),
        (6_900, mem_err(0x2_8850_ee08, 0, 17, TestId::MovingInvOnes)),
        (7_400, mem_err(0x6_fa81_3d10, 0xffff_ffff_ffff_fff7, 3, TestId::MovingInvOnes)),
        (12_200, mem_err(0x2_8850_ee08, 0, 17, TestId::MovingInvOnes)),
        (16_300, mem_err(0x1_154a_6180, 0x1010_1010_1010_1010, 17, TestId::RandomSeq)),
        (19_800, mem_err(0x2_8850_ee08, 0x2056_abfc_1d11_0646, 17, TestId::RandomSeq)),
        (23_100, mem_err(0x3_31f9_7498, 0xdfdf_dfdf_dfdf_dfdf, 3, TestId::RandomSeq)),
    ];
    v.errors = errs.iter().map(|(t, e)| ErrLine::mem(*t, e, Some(e.addr as u64))).collect();
    out.push(("3-memory-errors", 100, 31, v));

    let burn = Current::Burn {
        isa: "AVX2+FMA",
        threads: 16,
        sizes: "{10..21}".into(),
        ffts: 3_045_300,
        elapsed_ms: 42_000,
        total_ms: 120_000,
        worst: 0.0629f64.to_bits(),
    };
    let mut v = base(
        Verdict::Running,
        quick_plan([
            Done { ms: 4000, errors: 0 },
            Done { ms: 11_000, errors: 0 },
            Done { ms: 6000, errors: 0 },
            Running,
        ]),
        burn,
    );
    v.elapsed_ms = 63_000;
    v.perf.mem = mem_series(80);
    v.perf.gflops = fl_series(84);
    v.perf.temp_c = Some(88);
    out.push(("4-burnin-running-160x50", 160, 50, v));

    let mut v = base(
        Verdict::Fail,
        quick_plan([
            Done { ms: 4000, errors: 0 },
            Done { ms: 11_000, errors: 9 },
            Done { ms: 6000, errors: 5 },
            Done { ms: 120_000, errors: 1 },
        ]),
        Current::Summary(vec![
            ("Memory tested".into(), "30.0 GiB, 3 tests".into()),
            ("Burn-in".into(), "0:02:00, 9,812,440 FFTs".into()),
            ("Worst rounding".into(), "0.0629 (limit 0.4)".into()),
            ("Peak CPU temp".into(), "88 °C".into()),
            ("Total time".into(), "0:02:21".into()),
        ]),
    );
    v.elapsed_ms = 141_000;
    v.perf.mem = mem_series(90);
    v.perf.gflops = fl_series(240);
    v.perf.temp_c = Some(71);
    v.mem_errors = 14;
    v.cpu_errors = 1;
    v.bits = vec![
        BitStat { bit: 17, up: 12, down: 0, places: 2, runs: 1 },
        BitStat { bit: 3, up: 1, down: 1, places: 2, runs: 0 },
    ];
    let mut errors: Vec<ErrLine> = errs.iter().map(|(t, e)| ErrLine::mem(*t, e, Some(e.addr as u64))).collect();
    let cpu = BurnError { worker: 6, log2n: 15, seed: 0x6_0000_0001, fault: Fault::WrongResult, max_err: 0 };
    errors.push(ErrLine::cpu(97_000, &cpu));
    v.errors = errors;
    if let Current::Summary(rows) = &mut v.current {
        rows.extend(tui::module_rows(&v.dimms, 1 << 17 | 1 << 3));
    }
    v.advice = "Reset CPU and memory to stock, check cooling, then retest memory one stick at a time.".into();
    v.keys = vec![
        ("F", "full memory test"),
        ("B", "burn-in 30 min"),
        ("A", "all, repeating"),
        ("R", "restart"),
        ("P", "power off"),
    ];
    out.push(("5-result-fail", 100, 31, v));

    let mut v = base(
        Verdict::Pass,
        quick_plan([
            Done { ms: 4000, errors: 0 },
            Done { ms: 11_000, errors: 0 },
            Done { ms: 6000, errors: 0 },
            Done { ms: 120_000, errors: 0 },
        ]),
        Current::Summary(vec![
            ("Memory tested".into(), "30.0 GiB, 3 tests".into()),
            ("Burn-in".into(), "0:02:00, 9,812,440 FFTs".into()),
            ("Worst rounding".into(), "0.0629 (limit 0.4)".into()),
            ("Peak CPU temp".into(), "88 °C".into()),
            ("Total time".into(), "0:02:21".into()),
        ]),
    );
    v.elapsed_ms = 141_000;
    v.perf.mem = mem_series(90);
    v.perf.gflops = fl_series(240);
    v.perf.temp_c = Some(58);
    v.advice = "30.0 GiB memory and 9,812,440 FFTs checked in 0:02:21".into();
    v.remote = None;
    v.keys = vec![("F", "full memory"), ("B", "burn-in"), ("A", "all"), ("P", "power off")];
    out.push(("6-result-pass-80x25-uefi", 80, 25, v));

    let mut v = base(
        Verdict::Running,
        quick_plan([Done { ms: 4000, errors: 0 }, Running, Pending, Pending]),
        mem_test(TestId::MovingInvOnes, 2, 3, 6, 48),
    );
    v.qr_zoom = true;
    v.keys = vec![("Q", "back"), ("Esc", "stop")];
    out.push(("7-qr-zoom", 100, 31, v));

    let four: Vec<Dimm> = ["A1", "A2", "B1", "B2"]
        .map(|slot| Dimm {
            locator: format!("DIMM_{slot}"),
            bank: format!("BANK {}", &slot[1..]),
            size_mib: 16384,
            speed: 3600,
            configured: 3200,
            part: "F4-3600C16-16GTZNC".into(),
            ..Dimm::default()
        })
        .to_vec();
    for (name, w, h, dimms) in [
        ("8-modules-2-80x25", 80, 25, None),
        ("8-modules-4-80x25", 80, 25, Some(four.clone())),
        ("8-modules-2-160x50", 160, 50, None),
        ("8-modules-4-160x50", 160, 50, Some(four.clone())),
    ] {
        let mut v = base(
            Verdict::Running,
            quick_plan([Done { ms: 4000, errors: 0 }, Running, Pending, Pending]),
            mem_test(TestId::MovingInvWalk8, 2, 9, 24, 37),
        );
        v.elapsed_ms = 19_000;
        v.perf.mem = mem_series(120);
        v.perf.temp_c = Some(61);
        if let Some(d) = dimms {
            v.dimms = d;
        }
        out.push((name, w, h, v));
    }
    out
}

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "target/renders".into());
    std::fs::create_dir_all(&dir).unwrap();
    for (name, w, h, v) in scenes() {
        let mut f = Frame::new(w, h);
        tui::render(&v, &mut f);
        let mut s = String::new();
        for y in 0..h {
            let row: Vec<_> = (0..w).map(|x| f.get(x, y)).collect();
            s.extend(row.iter().map(|c| c.ch));
            s.push('\n');
            s.extend(row.iter().map(|c| char::from_digit(c.fg as u32, 16).unwrap()));
            s.push('\n');
            s.extend(row.iter().map(|c| char::from_digit(c.bg as u32, 16).unwrap()));
            s.push('\n');
        }
        let _ = writeln!(s);
        std::fs::write(format!("{dir}/{name}.cells"), s).unwrap();
        let snap = memburn_proto::Snapshot::from_view(&v);
        std::fs::write(format!("{dir}/{name}.json"), serde_json::to_vec(&snap).unwrap()).unwrap();
    }
}
