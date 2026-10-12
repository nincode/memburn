//! JSON exchanged between memburn clients and the live-view server.

use memburn_core::memtest::TestId;
use memburn_core::tui::{self, Current as TuiCurrent, ErrKind as TuiErrKind, StepState as TuiStep, View};
use serde::{Deserialize, Serialize};

pub mod release;

/// Largest snapshot the server accepts, in bytes.
pub const MAX_BODY: usize = 256 * 1024;
/// Error rows kept in a snapshot.
pub const MAX_ERRORS: usize = 200;
/// Performance samples kept per series.
pub const MAX_SAMPLES: usize = 300;

#[derive(Serialize, Deserialize, Clone, Copy, Default, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    #[default]
    Menu,
    Running,
    Stopping,
    Pass,
    Fail,
    Stopped,
}

#[derive(Serialize, Deserialize, Clone, Copy, Default, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum StepState {
    #[default]
    Pending,
    Running,
    Done,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct Step {
    pub name: String,
    pub note: String,
    pub state: StepState,
    pub ms: u64,
    pub errors: u64,
}

#[derive(Serialize, Deserialize, Clone, Copy, Default, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    #[default]
    Idle,
    Mem,
    Hold,
    Burn,
    Summary,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct Current {
    pub kind: Phase,
    pub title: String,
    pub lines: Vec<String>,
    /// Percent done.
    pub progress: Option<u8>,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct Perf {
    /// MB/s, oldest first.
    pub mem_mbps: Vec<u32>,
    /// 0.1 GFLOP/s, oldest first.
    pub gflops10: Vec<u32>,
    pub temp_c: Option<i32>,
    /// Hottest memory module sensor, °C.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ram_temp_c: Option<i32>,
    /// Whole-run peak, MB/s.
    #[serde(default)]
    pub peak_mem_mbps: u32,
    /// Whole-run peak, 0.1 GFLOP/s.
    #[serde(default)]
    pub peak_gflops10: u32,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum ErrKind {
    Mem,
    Cpu,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Dir {
    /// Read 1 where 0 was written.
    Up,
    Down,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Space {
    Phys,
    Virt,
}

/// An error row; memory errors carry their fields, older clients send only `text`.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct ErrRow {
    pub at_ms: u64,
    pub kind: ErrKind,
    pub text: String,
    /// The flipped bit when exactly one flipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<Dir>,
    /// Bits that differ; 0 for CPU errors.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub flipped: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<Space>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<u64>,
    /// Test key, e.g. "mi8".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test: Option<String>,
    /// Likely module or channel, e.g. "DIMM_B2".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub module: String,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl ErrRow {
    fn from_line(e: &tui::ErrLine) -> ErrRow {
        let m = e.mem;
        let one = m.filter(|m| m.flipped == 1);
        ErrRow {
            at_ms: e.at_ms,
            kind: if e.kind == TuiErrKind::Mem { ErrKind::Mem } else { ErrKind::Cpu },
            text: e.text.clone(),
            bit: one.map(|m| m.bit),
            dir: one.map(|m| if m.up { Dir::Up } else { Dir::Down }),
            flipped: m.map_or(0, |m| m.flipped),
            space: m.map(|m| if m.phys { Space::Phys } else { Space::Virt }),
            addr: m.map(|m| m.addr),
            test: m.map(|m| TestId::from_u8(m.test).map_or("?", |t| t.key()).into()),
            module: e.module.clone(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct Snapshot {
    pub version: String,
    pub system: Vec<String>,
    pub elapsed_ms: u64,
    pub verdict: Verdict,
    pub job: String,
    pub plan: Vec<Step>,
    pub current: Current,
    pub perf: Perf,
    pub mem_errors: u64,
    pub cpu_errors: u64,
    pub bits: Vec<Bit>,
    /// Newest last.
    pub errors: Vec<ErrRow>,
    pub advice: String,
    /// Memory modules from SMBIOS.
    #[serde(default)]
    pub dimms: Vec<Dimm>,
    /// Unix seconds when the run ended; the final snapshot. Older clients never set it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<u64>,
    /// E.g. that the previous run did not finish.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notice: String,
}

/// A populated memory slot.
#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Eq, Debug)]
pub struct Dimm {
    pub locator: String,
    pub bank: String,
    pub size_mib: u64,
    /// Rated MT/s; 0 when unknown.
    pub speed: u32,
    /// MT/s it runs at; 0 when unknown.
    pub configured: u32,
    pub manufacturer: String,
    pub part: String,
    pub serial: String,
    /// Errors decode to this module or its channel.
    #[serde(default, skip_serializing_if = "is_false")]
    pub likely: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// A flipped bit: flips each way, distinct failing addresses, earlier runs in which it failed.
#[derive(Serialize, Deserialize, Clone, Copy, Default, PartialEq, Eq, Debug)]
#[serde(from = "BitWire")]
pub struct Bit {
    pub bit: u8,
    pub up: u32,
    pub down: u32,
    pub places: u32,
    pub runs: u32,
}

/// Older clients send `[bit, up, down]`.
#[derive(Deserialize)]
#[serde(untagged)]
enum BitWire {
    Old(u8, u32, u32),
    New {
        bit: u8,
        up: u32,
        down: u32,
        #[serde(default)]
        places: u32,
        #[serde(default)]
        runs: u32,
    },
}

impl From<BitWire> for Bit {
    fn from(w: BitWire) -> Bit {
        match w {
            BitWire::Old(bit, up, down) => Bit { bit, up, down, ..Bit::default() },
            BitWire::New { bit, up, down, places, runs } => Bit { bit, up, down, places, runs },
        }
    }
}

/// Reply to creating a run.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Created {
    pub id: String,
    pub token: String,
    pub url: String,
}

/// A run as the server returns it to browsers.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RunView {
    pub id: String,
    /// Unix seconds.
    pub created: u64,
    pub updated: u64,
    pub now: u64,
    pub snapshot: Option<Snapshot>,
}

fn tail<T: Clone>(v: &[T], n: usize) -> Vec<T> {
    v[v.len().saturating_sub(n)..].to_vec()
}

impl Snapshot {
    pub fn from_view(v: &View) -> Snapshot {
        let peaks = v.perf.peaks();
        Snapshot {
            version: v.version.into(),
            system: v.system.clone(),
            elapsed_ms: v.elapsed_ms,
            verdict: match v.verdict {
                tui::Verdict::Menu(_) => Verdict::Menu,
                tui::Verdict::Running => Verdict::Running,
                tui::Verdict::Stopping => Verdict::Stopping,
                tui::Verdict::Pass => Verdict::Pass,
                tui::Verdict::Fail => Verdict::Fail,
                tui::Verdict::Stopped => Verdict::Stopped,
            },
            job: v.job.clone(),
            plan: v
                .plan
                .iter()
                .map(|s| {
                    let (state, ms, errors) = match s.state {
                        TuiStep::Pending => (StepState::Pending, 0, 0),
                        TuiStep::Running => (StepState::Running, 0, 0),
                        TuiStep::Done { ms, errors } => (StepState::Done, ms, errors),
                    };
                    Step { name: s.name.clone(), note: s.note.clone(), state, ms, errors }
                })
                .collect(),
            current: current(&v.current),
            perf: Perf {
                mem_mbps: tail(&v.perf.mem, MAX_SAMPLES),
                gflops10: tail(&v.perf.gflops, MAX_SAMPLES),
                temp_c: v.perf.temp_c,
                ram_temp_c: v.perf.ram_temp_c,
                peak_mem_mbps: peaks.0,
                peak_gflops10: peaks.1,
            },
            mem_errors: v.mem_errors,
            cpu_errors: v.cpu_errors,
            bits: v
                .bits
                .iter()
                .map(|b| Bit { bit: b.bit, up: b.up, down: b.down, places: b.places, runs: b.runs })
                .collect(),
            errors: v.errors[v.errors.len().saturating_sub(MAX_ERRORS)..].iter().map(ErrRow::from_line).collect(),
            advice: v.advice.clone(),
            dimms: v
                .dimms
                .iter()
                .enumerate()
                .map(|(n, d)| Dimm {
                    locator: d.locator.clone(),
                    bank: d.bank.clone(),
                    size_mib: d.size_mib,
                    speed: d.speed,
                    configured: d.configured,
                    manufacturer: d.manufacturer.clone(),
                    part: d.part.clone(),
                    serial: d.serial.clone(),
                    likely: v.suspects.iter().any(|s| s.dimms.contains(&n)),
                })
                .collect(),
            finished: None,
            notice: v.notice.clone(),
        }
    }

    /// Ended: stamped final, or (older clients) a pass, fail or stopped verdict.
    pub fn is_finished(&self) -> bool {
        self.finished.is_some() || matches!(self.verdict, Verdict::Pass | Verdict::Fail | Verdict::Stopped)
    }
}

fn current(c: &TuiCurrent) -> Current {
    match c {
        TuiCurrent::Idle | TuiCurrent::Menu(_) => Current::default(),
        TuiCurrent::Busy(what) => Current { title: what.clone(), ..Current::default() },
        TuiCurrent::Summary(rows) => Current {
            kind: Phase::Summary,
            title: "Summary".into(),
            lines: rows.iter().map(|(l, v)| format!("{l}: {v}")).collect(),
            progress: None,
        },
        TuiCurrent::Mem {
            test,
            pass,
            passes,
            index,
            count,
            step,
            steps,
            done,
            total,
            left_ms,
            job_left_ms,
            job_label,
        } => Current {
            kind: Phase::Mem,
            title: test.name().into(),
            lines: vec![
                format!("pass {pass}/{passes} · test {index}/{count} · step {step}/{steps}"),
                format!("{:.1} / {:.1} GiB", *done as f64 / (1u64 << 30) as f64, *total as f64 / (1u64 << 30) as f64),
                tui::time_left(*left_ms, *job_left_ms, job_label).replace("  ·  ", " · "),
            ],
            progress: Some((done * 100).checked_div(*total).unwrap_or(0) as u8),
        },
        TuiCurrent::Hold { left_ms } => Current {
            kind: Phase::Hold,
            title: "Bit fade".into(),
            lines: vec!["memory idle".into(), format!("{} left", tui::clock(*left_ms))],
            progress: None,
        },
        TuiCurrent::Burn { isa, threads, sizes, ffts, elapsed_ms, total_ms, worst } => Current {
            kind: Phase::Burn,
            title: "FFT burn-in".into(),
            lines: vec![
                format!("{isa} · {threads} threads · 2^{sizes}"),
                format!("{} / {}", tui::clock(*elapsed_ms), tui::clock(*total_ms)),
                format!("{} FFTs checked", tui::group(*ffts)),
                format!("worst rounding {:.4} · limit 0.4", f64::from_bits(*worst)),
            ],
            progress: Some((elapsed_ms * 100).checked_div(*total_ms).unwrap_or(0).min(100) as u8),
        },
        TuiCurrent::Core { label, index, count, cycle, isa, ffts, elapsed_ms, total_ms } => {
            let (head, what) = tui::core_lines(label, *index, *count, *cycle, *isa);
            Current {
                kind: Phase::Burn,
                title: "Core cycler".into(),
                lines: vec![
                    head.replace("  ·  ", " · "),
                    what.replace("  ·  ", " · "),
                    format!("{} / {}", tui::clock(*elapsed_ms), tui::clock(*total_ms)),
                    format!("{} FFTs checked", tui::group(*ffts)),
                ],
                progress: Some((elapsed_ms * 100).checked_div(*total_ms).unwrap_or(0).min(100) as u8),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memburn_core::errlog::MemError;

    #[test]
    fn reads_old_and_new_bits() {
        let v: Vec<Bit> = serde_json::from_str(r#"[[1,3,0],{"bit":2,"up":0,"down":1,"places":2,"runs":1}]"#).unwrap();
        assert_eq!(v[0], Bit { bit: 1, up: 3, down: 0, places: 0, runs: 0 });
        assert_eq!(v[1].places, 2);
    }

    #[test]
    fn error_rows_round_trip_with_and_without_fields() {
        let old: ErrRow =
            serde_json::from_str(r#"{"at_ms":5,"kind":"mem","text":"bit 3 0->1  virt 0x10  mi8"}"#).unwrap();
        assert_eq!(
            (old.bit, old.dir, old.flipped, old.space, old.addr, old.test.as_deref()),
            (None, None, 0, None, None, None)
        );
        let e = MemError { addr: 0x7f00_0000_1000, expected: 0, actual: 1 << 9, test: TestId::Hashed as u8, pass: 0 };
        let mut v = view(tui::Perf::default());
        v.errors = vec![tui::ErrLine::mem(7, &e, Some(0x1_2345_6000))];
        let row = &Snapshot::from_view(&v).errors[0];
        assert_eq!(
            (row.bit, row.dir, row.flipped, row.space, row.addr, row.test.as_deref()),
            (Some(9), Some(Dir::Up), 1, Some(Space::Phys), Some(0x1_2345_6000), Some("hash"))
        );
        let back: ErrRow = serde_json::from_str(&serde_json::to_string(row).unwrap()).unwrap();
        assert_eq!(&back, row);
        let two = MemError { actual: 0b110, ..e };
        v.errors = vec![tui::ErrLine::mem(7, &two, None)];
        let row = &Snapshot::from_view(&v).errors[0];
        assert_eq!((row.bit, row.dir, row.flipped, row.space), (None, None, 2, Some(Space::Virt)));
        let plain = serde_json::to_string(&old).unwrap();
        assert_eq!(plain, r#"{"at_ms":5,"kind":"mem","text":"bit 3 0->1  virt 0x10  mi8"}"#);
    }

    #[test]
    fn dimms_round_trip_and_default_to_empty() {
        let mut json = serde_json::to_value(Snapshot::default()).unwrap();
        assert!(json.as_object_mut().unwrap().remove("dimms").is_some());
        let old: Snapshot = serde_json::from_value(json).unwrap();
        assert!(old.dimms.is_empty());
        let mut v = view(tui::Perf::default());
        v.dimms = vec![memburn_core::smbios::Dimm {
            locator: "DIMM_A1".into(),
            size_mib: 16384,
            speed: 5600,
            serial: "1A2B".into(),
            ..Default::default()
        }];
        let s = Snapshot::from_view(&v);
        let back: Snapshot = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.dimms, s.dimms);
        assert_eq!((back.dimms[0].size_mib, back.dimms[0].serial.as_str()), (16384, "1A2B"));
        assert!(!back.dimms[0].likely && !serde_json::to_string(&s).unwrap().contains("likely"));
        v.suspects = vec![tui::Suspect { label: "DIMM_A1".into(), errors: 1, dimms: vec![0] }];
        let e = MemError { addr: 0x10, expected: 0, actual: 1, test: 0, pass: 0 };
        v.errors = vec![tui::ErrLine { module: "DIMM_A1".into(), ..tui::ErrLine::mem(1, &e, Some(0x10)) }];
        let s = Snapshot::from_view(&v);
        let back: Snapshot = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert!(back.dimms[0].likely);
        assert_eq!(back.errors[0].module, "DIMM_A1");
    }

    #[test]
    fn finished_round_trips_and_old_verdicts_count() {
        let s = Snapshot { verdict: Verdict::Pass, finished: Some(1_791_800_000), ..Snapshot::default() };
        let back: Snapshot = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.finished, Some(1_791_800_000));
        let running = Snapshot { verdict: Verdict::Running, ..Snapshot::default() };
        let json = serde_json::to_string(&running).unwrap();
        assert!(!json.contains("finished"));
        let old: Snapshot = serde_json::from_str(&json).unwrap();
        assert!(!old.is_finished());
        assert!(Snapshot { verdict: Verdict::Fail, ..old }.is_finished());
    }

    #[test]
    fn reads_perf_without_peaks() {
        let p: Perf = serde_json::from_str(r#"{"mem_mbps":[1,2],"gflops10":[],"temp_c":null}"#).unwrap();
        assert_eq!((p.peak_mem_mbps, p.peak_gflops10, p.ram_temp_c), (0, 0, None));
        let v = View { perf: tui::Perf { ram_temp_c: Some(47), ..tui::Perf::default() }, ..View::default() };
        let json = serde_json::to_string(&Snapshot::from_view(&v)).unwrap();
        assert!(json.contains(r#""ram_temp_c":47"#), "{json}");
    }

    #[test]
    fn peaks_cover_samples_older_than_the_snapshot() {
        let mut p = tui::Perf::default();
        p.push(Some(90_000), Some(5));
        for k in 0..tui::MAX_SAMPLES as u32 + 10 {
            p.push(Some(20_000 + k % 7), Some(400 + k % 3));
        }
        let s = Snapshot::from_view(&view(p));
        assert_eq!(s.perf.mem_mbps.len(), MAX_SAMPLES);
        assert_eq!((s.perf.peak_mem_mbps, s.perf.peak_gflops10), (90_000, 402));
    }

    fn view(perf: tui::Perf) -> View {
        View { version: "0", perf, ..View::default() }
    }
}
