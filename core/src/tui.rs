//! Text-mode dashboard shared by every front end: a [`View`] rendered into a [`Frame`] of cells.

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::dram::{Hint, Modules};
use crate::errlog::{ErrorLog, MemError};
use crate::fft::{BurnError, Fault};
use crate::history::History;
use crate::memtest::TestId;
use crate::smbios::Dimm;

mod fire;
mod map;

pub use fire::Fire;
pub use map::{ErrRange, MemMap, RANGE_BYTES, Sample, range_label};

/// The 16 text-mode colors in VGA/UEFI order; backgrounds use only the first 8.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Color {
    Black,
    Blue,
    Green,
    Cyan,
    Red,
    Magenta,
    Brown,
    LightGray,
    DarkGray,
    LightBlue,
    LightGreen,
    LightCyan,
    LightRed,
    LightMagenta,
    Yellow,
    White,
}

use Color::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
}

const BLANK: Cell = Cell { ch: ' ', fg: LightGray, bg: Black };

pub struct Frame {
    pub w: usize,
    pub h: usize,
    pub cells: Vec<Cell>,
}

#[derive(Clone, Copy)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

impl Frame {
    pub fn new(w: usize, h: usize) -> Frame {
        Frame { w, h, cells: vec![BLANK; w * h] }
    }

    pub fn get(&self, x: usize, y: usize) -> Cell {
        self.cells[y * self.w + x]
    }

    fn set(&mut self, x: usize, y: usize, ch: char, fg: Color, bg: Color) {
        if x < self.w && y < self.h {
            self.cells[y * self.w + x] = Cell { ch, fg, bg };
        }
    }

    /// Writes `s` clipped to `max` columns; returns the column after it.
    fn text(&mut self, x: usize, y: usize, s: &str, max: usize, fg: Color, bg: Color) -> usize {
        let mut cx = x;
        for ch in s.chars().take(max) {
            self.set(cx, y, ch, fg, bg);
            cx += 1;
        }
        cx
    }

    /// Right-aligns `s` so it ends just before column `end`.
    fn text_right(&mut self, end: usize, y: usize, s: &str, fg: Color, bg: Color) {
        let n = s.chars().count();
        self.text(end.saturating_sub(n), y, s, n, fg, bg);
    }

    fn fill(&mut self, r: Rect, bg: Color) {
        for y in r.y..r.y + r.h {
            for x in r.x..r.x + r.w {
                self.set(x, y, ' ', LightGray, bg);
            }
        }
    }

    /// Single-line box with a title in the top border; returns the inside.
    fn panel(&mut self, r: Rect, title: &str, badge: Option<(&str, Color)>) -> Rect {
        if r.w < 4 || r.h < 2 {
            return Rect { x: r.x, y: r.y, w: 0, h: 0 };
        }
        let (fg, bg) = (DarkGray, Black);
        let (x1, y1) = (r.x + r.w - 1, r.y + r.h - 1);
        for x in r.x + 1..x1 {
            self.set(x, r.y, '─', fg, bg);
            self.set(x, y1, '─', fg, bg);
        }
        for y in r.y + 1..y1 {
            self.set(r.x, y, '│', fg, bg);
            self.set(x1, y, '│', fg, bg);
        }
        self.set(r.x, r.y, '┌', fg, bg);
        self.set(x1, r.y, '┐', fg, bg);
        self.set(r.x, y1, '└', fg, bg);
        self.set(x1, y1, '┘', fg, bg);
        let end = self.text(r.x + 2, r.y, &format!(" {title} "), r.w.saturating_sub(4), White, bg);
        if let Some((s, c)) = badge {
            self.text(end, r.y, &format!(" {s} "), x1.saturating_sub(end + 1), c, bg);
        }
        Rect { x: r.x + 2, y: r.y + 1, w: r.w.saturating_sub(4), h: r.h - 2 }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Waiting for a choice; the default starts after this many seconds, or never.
    Menu(Option<u32>),
    Running,
    Stopping,
    Pass,
    Fail,
    Stopped,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Pending,
    Running,
    Done { ms: u64, errors: u64 },
}

pub struct Step {
    pub name: String,
    /// Shown while pending, e.g. the planned duration.
    pub note: String,
    pub state: StepState,
}

#[derive(Default)]
pub enum Current {
    #[default]
    Idle,
    /// A short phase with no progress figures, e.g. "Allocating 45.4 GiB".
    Busy(String),
    /// Start choices: (key, name, duration).
    Menu(Vec<(&'static str, String, String)>),
    /// Result figures: (label, value).
    Summary(Vec<(String, String)>),
    Mem {
        test: TestId,
        pass: u32,
        passes: u32,
        index: u32,
        count: u32,
        step: u32,
        steps: u32,
        done: u64,
        total: u64,
        /// This test's time left; None while estimating.
        left_ms: Option<u64>,
        /// The job's time left, or the cycle's when `job_label` is "cycle"; None while estimating.
        job_left_ms: Option<u64>,
        job_label: &'static str,
    },
    /// Bit-fade wait: memory is left idle.
    Hold { left_ms: u64 },
    Burn {
        isa: &'static str,
        threads: u32,
        sizes: String,
        ffts: u64,
        elapsed_ms: u64,
        total_ms: u64,
        /// Worst rounding error as f64 bits.
        worst: u64,
    },
    /// Core cycler: one logical CPU under load.
    Core {
        /// Row label of the CPU, e.g. "CPU 3".
        label: String,
        /// Position in the cycle, 1-based, and the CPU count.
        index: u32,
        count: u32,
        cycle: u32,
        /// Instruction set running now; None while idling.
        isa: Option<&'static str>,
        ffts: u64,
        elapsed_ms: u64,
        total_ms: u64,
    },
}

#[derive(Default)]
pub struct Perf {
    /// Memory throughput samples in MB/s, oldest first.
    pub mem: Vec<u32>,
    /// Compute samples in 0.1 GFLOP/s, oldest first.
    pub gflops: Vec<u32>,
    pub temp_c: Option<i32>,
    /// Hottest memory module sensor, °C; None when there is none.
    pub ram_temp_c: Option<i32>,
    /// Highest memory sample of the whole run, MB/s, including samples no longer in `mem`.
    pub peak_mem: u32,
    /// Highest compute sample of the whole run, 0.1 GFLOP/s.
    pub peak_gflops: u32,
}

/// Samples kept per series.
pub const MAX_SAMPLES: usize = 4000;

impl Perf {
    /// Appends samples, keeps the last [`MAX_SAMPLES`] of each series and the whole-run peaks.
    pub fn push(&mut self, mem: Option<u32>, gflops: Option<u32>) {
        if let Some(m) = mem {
            self.mem.push(m);
            self.peak_mem = self.peak_mem.max(m);
        }
        if let Some(g) = gflops {
            self.gflops.push(g);
            self.peak_gflops = self.peak_gflops.max(g);
        }
        for s in [&mut self.mem, &mut self.gflops] {
            let extra = s.len().saturating_sub(MAX_SAMPLES);
            s.drain(..extra);
        }
    }

    /// Whole-run (memory MB/s, 0.1 GFLOP/s) peaks.
    pub fn peaks(&self) -> (u32, u32) {
        let max = |s: &[u32]| s.iter().copied().max().unwrap_or(0);
        (self.peak_mem.max(max(&self.mem)), self.peak_gflops.max(max(&self.gflops)))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ErrKind {
    Mem,
    Cpu,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct BitStat {
    pub bit: u8,
    /// Flips 0->1.
    pub up: u32,
    /// Flips 1->0.
    pub down: u32,
    /// Distinct failing addresses.
    pub places: u32,
    /// Earlier runs in which this bit failed.
    pub runs: u32,
}

/// Distinct (bit, address) pairs seen in this run, at most [`MAX_PLACES`].
#[derive(Default)]
pub struct Places(BTreeSet<(u8, usize)>);

pub const MAX_PLACES: usize = 10_000;

impl Places {
    pub fn add(&mut self, e: &MemError) {
        let x = e.expected ^ e.actual;
        for b in (0..64u8).filter(|b| x >> b & 1 == 1) {
            if self.0.len() < MAX_PLACES {
                self.0.insert((b, e.addr));
            }
        }
    }

    pub fn count(&self, bit: u8) -> u32 {
        self.0.range((bit, 0)..=(bit, usize::MAX)).count() as u32
    }
}

pub struct ErrLine {
    pub at_ms: u64,
    pub kind: ErrKind,
    pub text: String,
    /// Fields of a memory error.
    pub mem: Option<MemAt>,
    /// Likely module or channel, e.g. "DIMM_B2"; empty when unknown.
    pub module: String,
}

/// A module or channel that errors decode to, with their count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suspect {
    pub label: String,
    pub errors: u64,
    /// Indices into [`View::dimms`].
    pub dimms: Vec<usize>,
}

/// A memory error's flipped bits, address and test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MemAt {
    /// Bits that differ.
    pub flipped: u32,
    /// Lowest flipped bit.
    pub bit: u8,
    /// The lowest flipped bit read 1 instead of 0.
    pub up: bool,
    /// `addr` is physical, else virtual.
    pub phys: bool,
    pub addr: u64,
    pub test: u8,
}

impl MemAt {
    pub fn new(e: &MemError, phys: Option<u64>) -> MemAt {
        let x = e.expected ^ e.actual;
        let bit = x.trailing_zeros().min(63) as u8;
        MemAt {
            flipped: x.count_ones(),
            bit,
            up: e.actual >> bit & 1 == 1,
            phys: phys.is_some(),
            addr: phys.unwrap_or(e.addr as u64),
            test: e.test,
        }
    }
}

impl ErrLine {
    pub fn mem(at_ms: u64, e: &MemError, phys: Option<u64>) -> ErrLine {
        ErrLine {
            at_ms,
            kind: ErrKind::Mem,
            text: mem_line(e, phys),
            mem: Some(MemAt::new(e, phys)),
            module: String::new(),
        }
    }

    pub fn cpu(at_ms: u64, e: &BurnError) -> ErrLine {
        ErrLine { at_ms, kind: ErrKind::Cpu, text: cpu_line(e), mem: None, module: String::new() }
    }
}

pub struct Qr {
    pub size: usize,
    /// Row-major modules, true = dark.
    pub dark: Vec<bool>,
}

pub struct Remote {
    pub url: String,
    pub status: String,
    pub ok: bool,
    pub qr: Option<Qr>,
}

pub struct View {
    pub version: &'static str,
    /// CPU, threads, ISA, RAM; trailing parts are dropped when the header is too narrow.
    pub system: Vec<String>,
    /// Memory modules from SMBIOS.
    pub dimms: Vec<Dimm>,
    /// DRAM address map matched to [`View::dimms`]; None where it cannot be read or checked.
    pub dram: Option<Modules>,
    /// Modules or channels the errors decode to, in order of first error.
    pub suspects: Vec<Suspect>,
    pub elapsed_ms: u64,
    pub verdict: Verdict,
    /// Name of the run, e.g. "Quick check".
    pub job: String,
    pub plan: Vec<Step>,
    pub current: Current,
    pub perf: Perf,
    pub mem_errors: u64,
    pub cpu_errors: u64,
    /// Every bit that has flipped, most flips first.
    pub bits: Vec<BitStat>,
    /// Newest last.
    pub errors: Vec<ErrLine>,
    pub advice: String,
    pub remote: Option<Remote>,
    pub keys: Vec<(&'static str, &'static str)>,
    /// Draw the QR code full-screen.
    pub qr_zoom: bool,
    /// Shown in the band until the next run starts, e.g. that the previous run did not finish.
    pub notice: String,
    /// Memory map of the running or last memory test.
    pub map: Option<MemMap>,
    /// The display draws braille patterns; else the map uses half blocks.
    pub braille: bool,
    /// Drawn under the QR code.
    pub fire: Fire,
}

impl Default for View {
    fn default() -> Self {
        View {
            version: "",
            system: Vec::new(),
            dimms: Vec::new(),
            dram: None,
            suspects: Vec::new(),
            elapsed_ms: 0,
            verdict: Verdict::Running,
            job: String::new(),
            plan: Vec::new(),
            current: Current::Idle,
            perf: Perf::default(),
            mem_errors: 0,
            cpu_errors: 0,
            bits: Vec::new(),
            errors: Vec::new(),
            advice: String::new(),
            remote: None,
            keys: Vec::new(),
            qr_zoom: false,
            notice: String::new(),
            map: None,
            braille: false,
            fire: Fire::default(),
        }
    }
}

impl View {
    /// Adds a memory error line; returns the likely module or channel when the DRAM map names one.
    pub fn push_mem(&mut self, at_ms: u64, e: &MemError, phys: Option<u64>) -> Option<String> {
        let hint: Option<Hint> = phys.and_then(|p| self.dram.as_ref()?.hint(p, &self.dimms));
        let mut line = ErrLine::mem(at_ms, e, phys);
        if let Some(h) = &hint {
            line.text = format!("{}  {}", line.text, h.label);
            line.module = h.label.clone();
            self.suspect(h);
        }
        self.errors.push(line);
        hint.map(|h| h.label)
    }

    /// The likely module of a bad physical range `start..end`, counted as one error, when both ends agree.
    pub fn suspect_range(&mut self, start: u64, end: u64) -> Option<String> {
        let m = self.dram.as_ref()?;
        let last = m.hint(end.checked_sub(1)?, &self.dimms);
        let h = m.hint(start, &self.dimms).filter(|h| last.as_ref() == Some(h))?;
        self.suspect(&h);
        Some(h.label)
    }

    fn suspect(&mut self, h: &Hint) {
        match self.suspects.iter_mut().find(|s| s.label == h.label) {
            Some(s) => s.errors += 1,
            None => self.suspects.push(Suspect { label: h.label.clone(), errors: 1, dimms: h.dimms.clone() }),
        }
    }

    /// Clears the errors of the previous run.
    pub fn clear_errors(&mut self) {
        self.errors.clear();
        self.suspects.clear();
    }
}

/// Modules of a live-view URL's QR code, for sizing the column before the code exists.
const QR_SIZE: usize = 29;

/// Width of the QR column, or 0 when the screen has no room for it.
fn qr_column(v: &View, w: usize, h: usize) -> usize {
    let Some(r) = &v.remote else { return 0 };
    let size = r.qr.as_ref().map_or(QR_SIZE, |q| q.size);
    if w >= 120 && h >= size / 2 + 12 { size + 8 } else { 0 }
}

pub fn render(v: &View, f: &mut Frame) {
    f.cells.fill(BLANK);
    let (w, h) = (f.w, f.h);
    if w < 40 || h < 12 {
        f.text(0, 0, "memburn: window too small", w, LightRed, Black);
        return;
    }
    header(v, f);
    band(v, f);
    footer(v, f);
    if v.qr_zoom
        && let Some(Remote { qr: Some(q), url, .. }) = &v.remote
    {
        let r = Rect { x: 0, y: 3, w, h: h - 4 };
        let inner = f.panel(r, "Live view", None);
        let qx = inner.x + inner.w.saturating_sub(q.size + 4) / 2;
        draw_qr(f, q, qx, inner.y);
        let ux = inner.x + inner.w.saturating_sub(url.chars().count()) / 2;
        f.text(ux, inner.y + q.size.div_ceil(2) + 3, url, inner.w, LightCyan, Black);
        return;
    }
    let body = Rect { x: 0, y: 3, w, h: h - 4 };
    let qw = qr_column(v, w, h);
    let lw = (body.w - qw) / 2;
    let rw = body.w - qw - lw;
    let top_h = (body.h * 2 / 5).clamp(7, 16);
    let bot_h = body.h - top_h;
    current(v, f, Rect { x: 0, y: body.y, w: lw, h: top_h });
    plan(v, f, Rect { x: 0, y: body.y + top_h, w: lw, h: bot_h });
    perf(v, f, Rect { x: lw, y: body.y, w: rw, h: top_h });
    let mod_h = modules_height(v.dimms.len(), bot_h);
    errors(v, f, Rect { x: lw, y: body.y + top_h, w: rw, h: bot_h - mod_h });
    if mod_h > 0 {
        modules(v, f, Rect { x: lw, y: body.y + top_h + bot_h - mod_h, w: rw, h: mod_h });
    }
    if qw > 0 {
        remote(v, f, Rect { x: lw + rw, y: body.y, w: qw, h: body.h });
    }
}

fn header(v: &View, f: &mut Frame) {
    let w = f.w;
    f.fill(Rect { x: 0, y: 0, w, h: 1 }, LightGray);
    let x = f.text(1, 0, &format!("memburn {}", v.version), w, Black, LightGray);
    let room = w.saturating_sub(x + 14);
    let mut n = v.system.len();
    let mut line = v.system.join("  ·  ");
    while n > 1 && line.chars().count() > room {
        n -= 1;
        line = v.system[..n].join("  ·  ");
    }
    f.text(x + 3, 0, &line, room, Blue, LightGray);
    f.text_right(w - 1, 0, &clock(v.elapsed_ms), Black, LightGray);
}

fn band(v: &View, f: &mut Frame) {
    let w = f.w;
    let errors = v.mem_errors + v.cpu_errors;
    let notice = matches!(v.verdict, Verdict::Menu(_)) && !v.notice.is_empty();
    let (bg, fg) = match v.verdict {
        Verdict::Pass => (Green, Black),
        Verdict::Fail => (Red, White),
        _ if errors > 0 => (Red, White),
        _ if notice => (Brown, White),
        Verdict::Menu(_) => (Blue, White),
        Verdict::Running => (Blue, White),
        Verdict::Stopping | Verdict::Stopped => (Brown, White),
    };
    f.fill(Rect { x: 0, y: 1, w, h: 2 }, bg);
    let found = error_summary(v);
    let (main, sub) = match v.verdict {
        Verdict::Menu(s) => (
            if notice { format!("READY  ·  {}", v.notice) } else { String::from("READY") },
            s.map_or(String::from("choose a test"), |s| format!("{} starts in {s} s", v.job)),
        ),
        Verdict::Running => (format!("RUNNING  ·  {found}"), step_label(v)),
        Verdict::Stopping => (format!("STOPPING  ·  {found}"), String::from("after the current step")),
        Verdict::Pass => (format!("PASS  ·  {found}"), v.advice.clone()),
        Verdict::Fail => (format!("FAIL  ·  {found}"), v.advice.clone()),
        Verdict::Stopped => (format!("STOPPED EARLY  ·  {found}"), v.advice.clone()),
    };
    f.text(2, 1, &main, w - 4, fg, bg);
    f.text(2, 2, &sub, w - 4, fg, bg);
}

pub fn error_summary(v: &View) -> String {
    match (v.mem_errors, v.cpu_errors) {
        (0, 0) => String::from("no errors"),
        (m, 0) => format!("{} MEMORY ERROR{}", group(m), plural_s(m)),
        (0, c) => format!("{} CPU ERROR{}", group(c), plural_s(c)),
        (m, c) => format!("{} MEMORY ERROR{}, {} CPU ERROR{}", group(m), plural_s(m), group(c), plural_s(c)),
    }
}

fn plural_s(n: u64) -> &'static str {
    if n == 1 { "" } else { "S" }
}

fn step_label(v: &View) -> String {
    let n = v.plan.len();
    match v.plan.iter().position(|s| s.state == StepState::Running) {
        Some(i) => format!("{}  ·  step {} of {n}: {}", v.job, i + 1, v.plan[i].name),
        None => v.job.clone(),
    }
}

fn footer(v: &View, f: &mut Frame) {
    let y = f.h - 1;
    let mut x = 1;
    for (key, action) in &v.keys {
        x = f.text(x, y, &format!(" {key} "), f.w.saturating_sub(x), Black, LightGray);
        x = f.text(x + 1, y, action, f.w.saturating_sub(x + 1), LightGray, Black) + 3;
    }
    if let Some(r) = &v.remote
        && qr_column(v, f.w, f.h) == 0
    {
        let s = format!("Live: {}", if r.url.is_empty() { &r.status } else { &r.url });
        if x + s.chars().count() < f.w {
            f.text_right(f.w - 1, y, &s, if r.ok { LightCyan } else { DarkGray }, Black);
        }
    }
}

fn current(v: &View, f: &mut Frame, r: Rect) {
    let title = match v.current {
        Current::Menu(_) => "Start",
        Current::Summary(_) => "Summary",
        _ => "Current test",
    };
    let i = f.panel(r, title, None);
    let line = |n: usize| i.y + n;
    match &v.current {
        Current::Idle => {
            f.text(i.x, line(0), "-", i.w, DarkGray, Black);
        }
        Current::Busy(what) => {
            f.text(i.x, line(0), what, i.w, White, Black);
        }
        Current::Menu(items) => {
            for (n, (key, name, note)) in items.iter().take(i.h).enumerate() {
                let x = f.text(i.x, line(n), &format!(" {key} "), 7, Black, LightGray);
                f.text(x + 1, line(n), name, i.w.saturating_sub(x + 1 - i.x), White, Black);
                f.text_right(i.x + i.w, line(n), note, DarkGray, Black);
            }
        }
        Current::Summary(rows) => {
            for (n, (label, value)) in rows.iter().take(i.h).enumerate() {
                f.text(i.x, line(n), label, 16, LightGray, Black);
                f.text(i.x + 16, line(n), value, i.w.saturating_sub(16), White, Black);
            }
        }
        Current::Mem {
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
        } => {
            f.text(i.x, line(0), test.name(), i.w, White, Black);
            let pos = format!("pass {pass}/{passes}  ·  test {index}/{count}  ·  step {step}/{steps}");
            f.text(i.x, line(1), &pos, i.w, LightGray, Black);
            let pct = (done * 100).checked_div(*total).unwrap_or(0);
            bar(f, i.x, line(2), i.w, pct as usize, LightCyan);
            f.text(i.x, line(3), &format!("{} / {}", gib(*done), gib(*total)), i.w, LightGray, Black);
            f.text(i.x, line(4), &time_left(*left_ms, *job_left_ms, job_label), i.w, LightGray, Black);
        }
        Current::Hold { left_ms } => {
            f.text(i.x, line(0), TestId::BitFade.name(), i.w, White, Black);
            f.text(i.x, line(1), "memory idle", i.w, LightGray, Black);
            f.text(i.x, line(2), &format!("{} left", clock(*left_ms)), i.w, LightGray, Black);
        }
        Current::Burn { isa, threads, sizes, ffts, elapsed_ms, total_ms, worst } => {
            f.text(i.x, line(0), "FFT burn-in", i.w, White, Black);
            f.text(i.x, line(1), &format!("{isa}  ·  {threads} threads  ·  2^{sizes}"), i.w, LightGray, Black);
            let pct = (elapsed_ms * 100).checked_div(*total_ms).unwrap_or(0).min(100);
            bar(f, i.x, line(2), i.w, pct as usize, Yellow);
            let t = format!("{} / {}", clock(*elapsed_ms), clock(*total_ms));
            f.text(i.x, line(3), &t, i.w, LightGray, Black);
            f.text(i.x, line(4), &format!("{} FFTs checked", group(*ffts)), i.w, LightGray, Black);
            if i.h > 5 {
                let s = format!("worst rounding {:.4}  ·  limit 0.4", f64::from_bits(*worst));
                f.text(i.x, line(5), &s, i.w, LightGray, Black);
            }
        }
        Current::Core { label, index, count, cycle, isa, ffts, elapsed_ms, total_ms } => {
            let (head, what) = core_lines(label, *index, *count, *cycle, *isa);
            f.text(i.x, line(0), "Core cycler", i.w, White, Black);
            f.text(i.x, line(1), &head, i.w, LightGray, Black);
            f.text(i.x, line(2), &what, i.w, if isa.is_some() { LightGray } else { DarkGray }, Black);
            let pct = (elapsed_ms * 100).checked_div(*total_ms).unwrap_or(0).min(100);
            bar(f, i.x, line(3), i.w, pct as usize, Yellow);
            f.text(i.x, line(4), &format!("{} / {}", clock(*elapsed_ms), clock(*total_ms)), i.w, LightGray, Black);
            if i.h > 5 {
                f.text(i.x, line(5), &format!("{} FFTs checked", group(*ffts)), i.w, LightGray, Black);
            }
        }
    }
}

/// "test ~0:00:31  ·  job ~0:03:40", or "time left: estimating".
pub fn time_left(test: Option<u64>, job: Option<u64>, job_label: &str) -> String {
    match (test, job) {
        (Some(t), Some(j)) => format!("test ~{}  ·  {job_label} ~{}", clock(t), clock(j)),
        (Some(t), None) => format!("test ~{}", clock(t)),
        _ => String::from("time left: estimating"),
    }
}

/// Core cycler result from its plan rows: "8 of 8 CPUs ok" or the failing rows.
pub fn cores_summary(plan: &[Step]) -> String {
    let done = plan.iter().filter(|s| matches!(s.state, StepState::Done { .. })).count();
    let failed: Vec<&str> = plan
        .iter()
        .filter(|s| matches!(s.state, StepState::Done { errors, .. } if errors > 0))
        .map(|s| s.name.as_str())
        .collect();
    if failed.is_empty() { format!("{done} of {} CPUs ok", plan.len()) } else { failed.join(", ") }
}

/// The core cycler's position and what runs: ("CPU 3  ·  4 of 8  ·  cycle 1", "AVX2+FMA  ·  FFT 2^{10..13}").
pub fn core_lines(label: &str, index: u32, count: u32, cycle: u32, isa: Option<&str>) -> (String, String) {
    let what = isa.map_or(String::from("idle"), |i| format!("{i}  ·  FFT 2^{{10..13}}"));
    (format!("{label}  ·  {index} of {count}  ·  cycle {cycle}"), what)
}

fn bar(f: &mut Frame, x: usize, y: usize, w: usize, pct: usize, color: Color) {
    let n = w.saturating_sub(5);
    let filled = n * pct.min(100) / 100;
    for k in 0..n {
        let (ch, c) = if k < filled { ('█', color) } else { ('░', DarkGray) };
        f.set(x + k, y, ch, c, Black);
    }
    f.text_right(x + w, y, &format!("{pct}%"), White, Black);
}

/// Rows the memory map needs below the plan: a title row and two map rows.
const MAP_MIN_H: usize = 3;
const MAP_MIN_W: usize = 24;

fn plan(v: &View, f: &mut Frame, r: Rect) {
    let i = f.panel(r, &v.job, None);
    if let Some(m) = &v.map
        && m.total > 0
        && i.w >= MAP_MIN_W
        && i.h >= v.plan.len() + MAP_MIN_H
    {
        let y = i.y + v.plan.len();
        for x in i.x - 1..i.x + 1 {
            f.set(x, y, '─', DarkGray, Black);
        }
        let title = " Memory map ";
        let x = if title.len() + map::legend_width(m) <= i.w {
            f.text(i.x + 1, y, title, i.w, LightGray, Black) + 1
        } else {
            i.x + 2
        };
        map::legend(f, m, x, i.x + i.w, y);
        let area = Rect { x: i.x, y: y + 1, w: i.w, h: i.y + i.h - y - 1 };
        map::draw(f, m, area, v.braille, v.elapsed_ms);
    }
    let running = v.plan.iter().position(|s| s.state == StepState::Running).unwrap_or(0);
    let skip = (running + 1).saturating_sub(i.h);
    for (n, s) in v.plan.iter().skip(skip).take(i.h).enumerate() {
        let y = i.y + n;
        let (mark, name_c) = match s.state {
            StepState::Running => ('►', White),
            StepState::Done { .. } => (' ', LightGray),
            StepState::Pending => (' ', DarkGray),
        };
        let timed = i.w >= 48;
        f.set(i.x, y, mark, Yellow, Black);
        f.text(i.x + 2, y, &s.name, i.w.saturating_sub(if timed { 19 } else { 10 }), name_c, Black);
        let end = i.x + i.w;
        match s.state {
            StepState::Running => f.text_right(end, y, "running", Yellow, Black),
            StepState::Pending => f.text_right(end, y, &s.note, DarkGray, Black),
            StepState::Done { ms, errors } => {
                if timed {
                    f.text_right(end - 9, y, &clock(ms), LightGray, Black);
                }
                if errors == 0 {
                    f.text_right(end, y, "ok", LightGreen, Black);
                } else {
                    f.text_right(end, y, &format!("{} ERR", group(errors)), LightRed, Black);
                }
            }
        }
    }
}

fn perf(v: &View, f: &mut Frame, r: Rect) {
    let i = f.panel(r, "Performance", None);
    let burning = matches!(v.current, Current::Burn { .. } | Current::Core { .. });
    let mem_now = v.perf.mem.last().copied();
    let fl_now = v.perf.gflops.last().copied();
    let (mem_peak, fl_peak) = v.perf.peaks();
    let row = |f: &mut Frame, y: usize, label: &str, now: Option<String>, peak: String, active: bool, c: Color| {
        f.text(i.x, y, label, 9, LightGray, Black);
        let x = f.text(i.x + 9, y, &now.unwrap_or(String::from("-")), 15, if active { c } else { LightGray }, Black);
        if !peak.is_empty() {
            f.text(x + 2, y, &peak, i.w.saturating_sub(x + 2 - i.x), DarkGray, Black);
        }
    };
    let mem_active = !burning && matches!(v.current, Current::Mem { .. });
    let mem_s = mem_now.map(|m| format!("{} GB/s", tenths(m as u64 / 100)));
    let mem_peak_s = if mem_peak > 0 { format!("peak {}", tenths(mem_peak as u64 / 100)) } else { String::new() };
    row(f, i.y, "Memory", mem_s, mem_peak_s, mem_active, LightCyan);
    let fl_s = fl_now.map(|g| format!("{} GFLOP/s", tenths(g as u64)));
    let fl_peak_s = if fl_peak > 0 { format!("peak {}", tenths(fl_peak as u64)) } else { String::new() };
    row(f, i.y + 1, "Compute", fl_s, fl_peak_s, burning, Yellow);
    let temp = v.perf.temp_c.map(|t| format!("{t} °C"));
    row(f, i.y + 2, "CPU temp", temp, String::new(), false, LightGray);
    let mut top = 3;
    if let Some(t) = v.perf.ram_temp_c {
        row(f, i.y + 3, "RAM temp", Some(format!("{t} °C")), String::new(), false, LightGray);
        top += 1;
    }
    let rows = i.h.saturating_sub(top + 1);
    if rows == 0 {
        return;
    }
    let (series, color, unit) =
        if burning { (&v.perf.gflops, Yellow, "GFLOP/s") } else { (&v.perf.mem, LightCyan, "GB/s") };
    let shown = &series[series.len().saturating_sub(i.w)..];
    let Some(&hi) = shown.iter().max() else { return };
    let lo = shown.iter().copied().min().unwrap_or(0) * 4 / 5;
    let span = (hi - lo).max(1) as u64;
    let scale = if burning { 1 } else { 100 };
    let axis = format!("{}-{} {unit}", tenths(lo as u64 / scale), tenths(hi as u64 / scale));
    f.text_right(i.x + i.w, i.y + top, &axis, DarkGray, Black);
    let x0 = i.x + i.w - shown.len();
    for (k, &s) in shown.iter().enumerate() {
        let level = ((s - lo) as u64 * rows as u64 * 2).div_ceil(span) as usize;
        for row in 0..rows {
            let y = i.y + i.h - 1 - row;
            let ch = match level.saturating_sub(row * 2) {
                0 => continue,
                1 => '▄',
                _ => '█',
            };
            f.set(x0 + k, y, ch, color, Black);
        }
    }
}

fn errors(v: &View, f: &mut Frame, r: Rect) {
    let total = v.mem_errors + v.cpu_errors;
    let badge = if total == 0 { (String::from("0"), LightGreen) } else { (group(total), LightRed) };
    let i = f.panel(r, "Errors", Some((&badge.0, badge.1)));
    if total == 0 {
        f.text(i.x, i.y, "none", i.w, LightGreen, Black);
        return;
    }
    let mut y = i.y;
    if !v.bits.is_empty() {
        let shown = if v.bits.len() > 3 { 2 } else { 3 };
        for s in v.bits.iter().take(shown) {
            let dirs: Vec<String> = [(s.up, "0->1"), (s.down, "1->0")]
                .iter()
                .filter(|(n, _)| *n > 0)
                .map(|(n, d)| format!("{d} ×{}", group(*n as u64)))
                .collect();
            let mut line = format!("bit {:>2}  lane {}  {:<20}", s.bit, s.bit / 8, dirs.join(", "));
            if s.places > 1 {
                line.push_str(&format!("  {} places", s.places));
            }
            if s.runs > 0 {
                line.push_str(&format!("  {} earlier run{}", s.runs, if s.runs == 1 { "" } else { "s" }));
            }
            f.text(i.x, y, &line, i.w, Yellow, Black);
            y += 1;
        }
        if v.bits.len() > shown {
            let rest: Vec<String> = v.bits[shown..].iter().map(|s| format!("{}", s.bit)).collect();
            f.text(i.x, y, &format!("+ bits {}", rest.join(", ")), i.w, Yellow, Black);
            y += 1;
        }
        if let Some(stuck) = stuck_bits(&v.bits) {
            f.text(i.x, y, &stuck, i.w, Yellow, Black);
            y += 1;
        }
    }
    let room = (i.y + i.h).saturating_sub(y);
    if room == 0 {
        return;
    }
    let hidden = v.errors.len().saturating_sub(room);
    let (hidden, room) = if hidden > 0 { (hidden + 1, room - 1) } else { (0, room) };
    if hidden > 0 {
        f.text(i.x, y, &format!("... {} earlier", group(hidden as u64)), i.w, DarkGray, Black);
        y += 1;
    }
    let timed = i.w >= 48;
    for e in v.errors.iter().skip(v.errors.len() - room.min(v.errors.len())) {
        let x = if timed { f.text(i.x, y, &clock(e.at_ms), 8, DarkGray, Black) + 1 } else { i.x };
        let c = if e.kind == ErrKind::Mem { LightRed } else { LightMagenta };
        f.text(x, y, &e.text, (i.x + i.w).saturating_sub(x), c, Black);
        y += 1;
    }
}

/// Rows the errors panel keeps below the modules panel.
const ERRORS_MIN_H: usize = 5;

/// Height of the modules panel under the errors panel in a column `h` rows high; 0 when it does not fit.
fn modules_height(n: usize, h: usize) -> usize {
    let rows = n.min(h.saturating_sub(ERRORS_MIN_H + 2));
    if rows == 0 { 0 } else { rows + 2 }
}

/// Short slot name: the locator when it tells the modules apart, else the bank, else both.
pub fn slot(d: &Dimm, all: &[Dimm]) -> String {
    let unique = |f: fn(&Dimm) -> &str| !f(d).is_empty() && all.iter().filter(|o| f(o) == f(d)).count() == 1;
    if unique(|d| &d.locator) {
        d.locator.clone()
    } else if unique(|d| &d.bank) {
        d.bank.clone()
    } else {
        format!("{} {}", d.bank, d.locator).trim().into()
    }
}

/// One line per module: slot, size, speed, part; the last row counts the ones that do not fit.
fn modules(v: &View, f: &mut Frame, r: Rect) {
    let i = f.panel(r, "Memory modules", None);
    let slots: Vec<String> = v.dimms.iter().map(|d| slot(d, &v.dimms)).collect();
    let sw = slots.iter().map(|s| s.chars().count()).max().unwrap_or(0);
    let more = v.dimms.len().saturating_sub(i.h);
    let shown = if more > 0 { i.h.saturating_sub(1) } else { i.h };
    for (n, (d, s)) in v.dimms.iter().zip(&slots).take(shown).enumerate() {
        let speed = match (d.speed, d.configured) {
            (0, 0) => String::new(),
            (s, 0) => format!("{s} MT/s"),
            (_, c) => format!("{c} MT/s"),
        };
        let size = crate::smbios::size(d.size_mib);
        let likely = v.suspects.iter().any(|x| x.dimms.contains(&n));
        let x = f.text(i.x, i.y + n, &format!("{s:<sw$}"), i.w, if likely { LightRed } else { White }, Black);
        let x = f.text(
            x + 2,
            i.y + n,
            &format!("{size:>7}  {speed:>9}"),
            (i.x + i.w).saturating_sub(x + 2),
            LightGray,
            Black,
        );
        let x = if likely {
            f.text(x + 2, i.y + n, "likely", (i.x + i.w).saturating_sub(x + 2), LightRed, Black)
        } else {
            x
        };
        f.text(x + 2, i.y + n, &d.part, (i.x + i.w).saturating_sub(x + 2), DarkGray, Black);
    }
    if more > 0 {
        let s = format!("+ {} more", v.dimms.len() - shown);
        f.text(i.x, i.y + shown, &s, i.w, DarkGray, Black);
    }
}

/// Bits that flipped at least 3 times, always in the same direction: "stuck at 1: bit 17".
pub fn stuck_bits(bits: &[BitStat]) -> Option<String> {
    let at = |one: bool| -> Vec<String> {
        bits.iter()
            .filter(|s| if one { s.up >= 3 && s.down == 0 } else { s.down >= 3 && s.up == 0 })
            .map(|s| format!("{}", s.bit))
            .collect()
    };
    let (ones, zeros) = (at(true), at(false));
    let mut parts = Vec::new();
    if !ones.is_empty() {
        parts.push(format!("stuck at 1: bit {}", ones.join(", ")));
    }
    if !zeros.is_empty() {
        parts.push(format!("stuck at 0: bit {}", zeros.join(", ")));
    }
    (!parts.is_empty()).then(|| parts.join("  ·  "))
}

fn remote(v: &View, f: &mut Frame, r: Rect) {
    let Some(rem) = &v.remote else { return };
    let i = f.panel(r, "Live view", None);
    let Some(q) = &rem.qr else {
        f.text(i.x, i.y, &rem.status, i.w, if rem.ok { LightGreen } else { LightGray }, Black);
        return;
    };
    draw_qr(f, q, i.x + i.w.saturating_sub(q.size + 4) / 2, i.y);
    let y = i.y + q.size.div_ceil(2) + 3;
    f.text(i.x, y, &rem.url, i.w, LightCyan, Black);
    f.text(i.x, y + 1, &rem.status, i.w, if rem.ok { LightGreen } else { DarkGray }, Black);
    let top = y + 3;
    v.fire.draw(f, Rect { x: i.x - 1, y: top, w: i.w + 2, h: (i.y + i.h).saturating_sub(top) });
}

/// Two modules per cell, dark on light, with a 2-module quiet zone.
fn draw_qr(f: &mut Frame, q: &Qr, x: usize, y: usize) {
    let n = q.size as isize;
    let dark = |r: isize, c: isize| r >= 0 && c >= 0 && r < n && c < n && q.dark[(r * n + c) as usize];
    for row in 0..(n + 5) / 2 {
        for col in 0..n + 4 {
            let (top, bottom) = (dark(row * 2 - 2, col - 2), dark(row * 2 - 1, col - 2));
            let ch = match (top, bottom) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            };
            f.set(x + col as usize, y + row as usize, ch, Black, LightGray);
        }
    }
}

/// Error-list text for a memory error; `phys` is its physical address when known.
pub fn mem_line(e: &MemError, phys: Option<u64>) -> String {
    let test = TestId::from_u8(e.test).map_or("?", |t| t.key());
    let x = e.expected ^ e.actual;
    let flips = match x.count_ones() {
        1 => {
            let b = x.trailing_zeros();
            format!("bit {b:>2} {}", if e.actual >> b & 1 == 1 { "0->1" } else { "1->0" })
        }
        n => format!("{n} bits"),
    };
    format!("{flips:<11}  {}  {test}", at(e.addr, phys))
}

/// "phys 0x1dac5a60eb0" when the physical address is known, else "virt 0x7f3a2c000eb0".
pub fn at(addr: usize, phys: Option<u64>) -> String {
    match phys {
        Some(p) => format!("phys {p:#x}"),
        None => format!("virt {addr:#x}"),
    }
}

/// Error-list text for a CPU (burn-in) error.
pub fn cpu_line(e: &BurnError) -> String {
    let what = match e.fault {
        Fault::WrongResult => String::from("wrong result"),
        Fault::Rounding => format!("rounding {:.3}", f64::from_bits(e.max_err)),
        Fault::NonFinite => String::from("NaN or infinity"),
    };
    format!("cpu {:<3}  FFT 2^{:<2}  {what}", e.worker, e.log2n)
}

/// Summary rows: how to find the faulty stick when bits in `flipped` failed, then one per memory module.
pub fn module_rows(dimms: &[Dimm], suspects: &[Suspect], flipped: u64) -> Vec<(String, String)> {
    let find = (flipped != 0).then(|| (String::from("Find the stick"), find_stick(suspects, flipped)));
    find.into_iter().chain(dimms.iter().map(|d| (String::from("Module"), d.line()))).collect()
}

/// "likely DIMM_B2" from the DRAM map, most errors first; else the swap advice of [`locate`].
pub fn find_stick(suspects: &[Suspect], flipped: u64) -> String {
    let mut s: Vec<&Suspect> = suspects.iter().collect();
    s.sort_by_key(|x| core::cmp::Reverse(x.errors));
    match s.as_slice() {
        [] => locate(flipped),
        [one] => format!("likely {}", one.label),
        many => {
            let parts: Vec<String> = many
                .iter()
                .map(|x| format!("{} ({} error{})", x.label, x.errors, crate::errlog::plural(x.errors)))
                .collect();
            format!("likely {}", parts.join(", "))
        }
    }
}

/// "swap two, retest; lanes 0, 2": errors that move with a stick mark it; lanes are bit/8.
pub fn locate(flipped: u64) -> String {
    let lanes: Vec<String> = (0..8).filter(|l| flipped >> (l * 8) & 0xFF != 0).map(|l| format!("{l}")).collect();
    let word = if lanes.len() == 1 { "lane" } else { "lanes" };
    format!("swap two, retest; {word} {}", lanes.join(", "))
}

/// Every flipped bit, most flips first.
pub fn bit_stats(log: &ErrorLog, places: &Places, history: Option<&History>) -> Vec<BitStat> {
    let mut v: Vec<BitStat> = (0..64u8)
        .map(|bit| {
            let (up, down) = log.flips(bit as usize);
            let runs = history.map_or(0, |h| h.per_bit[bit as usize]);
            BitStat { bit, up, down, places: places.count(bit), runs }
        })
        .filter(|s| s.up + s.down > 0)
        .collect();
    v.sort_by_key(|s| core::cmp::Reverse(s.up + s.down));
    v
}

pub fn clock(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// One decimal from an integer count of tenths.
pub fn tenths(t: u64) -> String {
    format!("{}.{}", t / 10, t % 10)
}

pub fn gib(bytes: u64) -> String {
    format!("{} GiB", tenths(bytes * 10 / (1 << 30)))
}

/// Thousands separators: 3,045,300.
pub fn group(n: u64) -> String {
    let s = format!("{n}");
    let mut out = String::new();
    for (k, ch) in s.chars().enumerate() {
        if k > 0 && (s.len() - k).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> View {
        View {
            version: "0.2.0",
            system: vec![String::from("Test CPU"), String::from("4 threads")],
            dimms: vec![],
            dram: None,
            suspects: vec![],
            elapsed_ms: 61_000,
            verdict: Verdict::Running,
            job: String::from("Quick check"),
            plan: vec![
                Step {
                    name: String::from("Own address"),
                    note: String::new(),
                    state: StepState::Done { ms: 3000, errors: 0 },
                },
                Step { name: String::from("Burn-in"), note: String::from("2:00"), state: StepState::Running },
            ],
            current: Current::Mem {
                test: TestId::MovingInvOnes,
                pass: 1,
                passes: 1,
                index: 2,
                count: 3,
                step: 3,
                steps: 6,
                done: 5 << 30,
                total: 8 << 30,
                left_ms: Some(9000),
                job_left_ms: Some(130_000),
                job_label: "job",
            },
            perf: Perf { mem: vec![20_000, 22_000, 25_000], temp_c: Some(64), ..Perf::default() },
            mem_errors: 4,
            cpu_errors: 1,
            bits: vec![BitStat { bit: 17, up: 4, down: 0, places: 2, runs: 1 }],
            errors: (0..30)
                .map(|k| ErrLine {
                    at_ms: k * 1000,
                    kind: ErrKind::Mem,
                    text: format!("e{k}"),
                    mem: None,
                    module: String::new(),
                })
                .collect(),
            advice: String::new(),
            remote: Some(Remote {
                url: String::from("https://reljam.com/m/ABCD1234"),
                status: String::from("ok"),
                ok: true,
                qr: Some(Qr { size: 25, dark: vec![true; 625] }),
            }),
            keys: vec![("Esc", "stop")],
            qr_zoom: false,
            notice: String::new(),
            ..View::default()
        }
    }

    #[test]
    fn live_view_shows_its_state_before_the_qr_code() {
        let mut v = sample();
        v.remote =
            Some(Remote { url: String::new(), status: String::from("waiting for network"), ok: false, qr: None });
        let mut f = Frame::new(160, 50);
        render(&v, &mut f);
        let row = |y: usize| (0..f.w).map(|x| f.get(x, y).ch).collect::<String>();
        assert!(row(3).contains("Live view"));
        assert!(row(4).contains("waiting for network"));
        let mut f = Frame::new(100, 31);
        render(&v, &mut f);
        assert!(row_of(&f, 30).ends_with("Live: waiting for network "));
    }

    fn row_of(f: &Frame, y: usize) -> String {
        (0..f.w).map(|x| f.get(x, y).ch).collect()
    }

    #[test]
    fn places_count_distinct_addresses() {
        let mut p = Places::default();
        let e = |addr| MemError { addr, expected: 0, actual: 0b10, test: 0, pass: 0 };
        for a in [8, 8, 16] {
            p.add(&e(a));
        }
        assert_eq!((p.count(1), p.count(0)), (2, 0));
    }

    #[test]
    fn renders_at_common_sizes() {
        let v = sample();
        for (w, h) in [(80, 25), (100, 31), (128, 40), (160, 50), (240, 67), (40, 12), (10, 5)] {
            let mut f = Frame::new(w, h);
            render(&v, &mut f);
            assert_eq!(f.cells.len(), w * h);
        }
    }

    #[test]
    fn error_row_fits_80_columns() {
        let e = MemError { addr: 0x7f3a_2c00_0eb0, expected: 0, actual: 1 << 17, test: 2, pass: 0 };
        assert_eq!(mem_line(&e, Some(0x1da_c5a6_0eb0)), "bit 17 0->1  phys 0x1dac5a60eb0  mi8");
        assert_eq!(mem_line(&e, None), "bit 17 0->1  virt 0x7f3a2c000eb0  mi8");
        let mut v = sample();
        let one = MemError { actual: 1 << 5, ..e };
        assert_eq!(mem_line(&one, None), "bit  5 0->1  virt 0x7f3a2c000eb0  mi8");
        v.errors = vec![ErrLine::mem(0, &e, Some(0x1da_c5a6_0eb0))];
        assert_eq!(
            v.errors[0].mem,
            Some(MemAt { flipped: 1, bit: 17, up: true, phys: true, addr: 0x1da_c5a6_0eb0, test: 2 })
        );
        let mut f = Frame::new(80, 25);
        render(&v, &mut f);
        let rows: Vec<String> = (0..f.h).map(|y| (0..f.w).map(|x| f.get(x, y).ch).collect()).collect();
        assert!(rows.iter().any(|r| r.contains("phys 0x1dac5a60eb0")), "{rows:#?}");
    }

    #[test]
    fn errors_name_the_likely_module() {
        use crate::dram::{Df, Modules, tests as d};
        let mut v = sample();
        v.clear_errors();
        v.dimms = d::four();
        v.dram = Some(Modules::new(Df::parse(&d::bg(0xC, 0, d::ALL_HASH, 64)).unwrap(), &v.dimms).unwrap());
        let e = MemError { addr: 0, expected: 0, actual: 1 << 20, test: 2, pass: 0 };
        assert_eq!(v.push_mem(0, &e, Some(0xabd16f218)).as_deref(), Some("channel A"));
        assert_eq!(v.push_mem(1, &e, Some(0xabd168348)).as_deref(), Some("channel A"));
        assert_eq!(v.push_mem(2, &e, None), None);
        assert_eq!(v.suspect_range(0xabd16f000, 0xabd16f040).as_deref(), Some("channel A"));
        assert_eq!(v.suspect_range(0xabd16f000, 0xabd170000), None);
        v.suspects[0].errors -= 1;
        assert_eq!(v.errors[0].text, "bit 20 0->1  phys 0xabd16f218  mi8  channel A");
        assert_eq!(v.suspects, vec![Suspect { label: "channel A".into(), errors: 2, dimms: vec![0, 1] }]);
        let mut f = Frame::new(160, 50);
        render(&v, &mut f);
        let rows: Vec<String> = (0..f.h).map(|y| (0..f.w).map(|x| f.get(x, y).ch).collect()).collect();
        assert!(rows.iter().any(|r| r.contains("DIMM_A1") && r.contains("likely")), "{rows:#?}");
        assert!(!rows.iter().any(|r| r.contains("DIMM_B1") && r.contains("likely")), "{rows:#?}");
        v.clear_errors();
        assert!(v.suspects.is_empty() && v.errors.is_empty());
    }

    #[test]
    fn module_rows_add_the_locate_row_on_errors() {
        let d = Dimm { locator: "A1".into(), size_mib: 8192, ..Dimm::default() };
        assert_eq!(
            module_rows(core::slice::from_ref(&d), &[], 0),
            vec![(String::from("Module"), String::from("A1  8 GiB"))]
        );
        let rows = module_rows(core::slice::from_ref(&d), &[], 1 << 17 | 1 << 3);
        assert_eq!(rows[1].0, "Module");
        assert_eq!(rows[0].1, "swap two, retest; lanes 0, 2");
        assert_eq!(locate(1 << 63), "swap two, retest; lane 7");
        let s = |label: &str, errors| Suspect { label: label.into(), errors, dimms: vec![0] };
        let rows = module_rows(&[d], &[s("DIMM_B2", 3)], 1);
        assert_eq!(rows[0].1, "likely DIMM_B2");
        assert_eq!(
            find_stick(&[s("channel A", 2), s("DIMM_B2", 37)], 1),
            "likely DIMM_B2 (37 errors), channel A (2 errors)"
        );
    }

    #[test]
    fn menu_shows_the_notice_without_a_countdown() {
        let v = View {
            verdict: Verdict::Menu(None),
            notice: String::from("Previous run did not finish: Burn-in, 2026-10-12 11:00 UTC"),
            ..View::default()
        };
        let mut f = Frame::new(100, 31);
        render(&v, &mut f);
        let row = |y: usize| (0..f.w).map(|x| f.get(x, y).ch).collect::<String>();
        assert!(row(1).contains("READY  ·  Previous run did not finish: Burn-in"), "{}", row(1));
        assert!(row(2).contains("choose a test"));
        assert_eq!(f.get(0, 1).bg, Brown);
    }

    #[test]
    fn ram_temp_row_only_with_a_sensor() {
        let mut v = sample();
        let mut f = Frame::new(100, 31);
        let has =
            |f: &Frame| (0..f.h).any(|y| (0..f.w).map(|x| f.get(x, y).ch).collect::<String>().contains("RAM temp"));
        render(&v, &mut f);
        assert!(!has(&f));
        v.perf.ram_temp_c = Some(47);
        render(&v, &mut f);
        assert!(has(&f));
    }

    #[test]
    fn modules_panel_fits_without_overlap() {
        let dimm = |k: usize| Dimm {
            locator: format!("DIMM_{}{}", ["A", "B"][k % 2], 1 + k / 2),
            size_mib: 16384,
            speed: 3200,
            part: "F4-3200C16-16GVK".into(),
            ..Dimm::default()
        };
        for n in [2, 4, 8] {
            for (w, h) in [(80, 25), (160, 50), (100, 31)] {
                let mut v = sample();
                v.dimms = (0..n).map(dimm).collect();
                let mut f = Frame::new(w, h);
                render(&v, &mut f);
                let rows: Vec<String> = (0..h).map(|y| (0..w).map(|x| f.get(x, y).ch).collect()).collect();
                let title = rows.iter().position(|r| r.contains(" Memory modules ")).expect("panel");
                assert!(rows.iter().any(|r| r.contains(" Errors ")), "{rows:#?}");
                let errors = rows.iter().position(|r| r.contains(" Errors ")).unwrap();
                assert!(title >= errors + ERRORS_MIN_H, "{w}x{h} {n}: {rows:#?}");
                assert!(rows[title + 1].contains("DIMM_A1") && rows[title + 1].contains("16 GiB"), "{rows:#?}");
                assert!(rows[h - 2].matches('└').count() >= 2, "{rows:#?}");
            }
        }
        assert_eq!(modules_height(4, 13), 6);
        assert_eq!(modules_height(8, 13), 8);
        assert_eq!(modules_height(2, 6), 0);
        let same = |b: &str| Dimm { locator: "DIMM 0".into(), bank: b.into(), ..Dimm::default() };
        let all = [same("P0 CHANNEL A"), same("P0 CHANNEL B")];
        assert_eq!(slot(&all[1], &all), "P0 CHANNEL B");
    }

    #[test]
    fn stuck_bit_detection() {
        let b = |bit, up, down| BitStat { bit, up, down, ..BitStat::default() };
        assert_eq!(stuck_bits(&[b(17, 4, 0), b(3, 1, 1)]).as_deref(), Some("stuck at 1: bit 17"));
        assert_eq!(stuck_bits(&[b(3, 2, 2)]), None);
        assert_eq!(group(3_045_300), "3,045,300");
    }
}
