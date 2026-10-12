#![no_std]
#![no_main]

extern crate alloc;

mod config;
mod gop;
mod ram;
mod screen;
mod smp;
mod thermal;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use core::time::Duration;

use memburn_core::coverage::{self, Footprint, Ranges};
use memburn_core::cpu::{self, Isa};
use memburn_core::cycler;
use memburn_core::engine::{Chunk, Moved, PhaseJob, chunk_bytes, chunk_regions};
use memburn_core::errlog::{self, BitList, ErrorLog, MemError};
use memburn_core::fft::{BurnError, BurnStats, Burner, Fault, MAX_ERR, Plan};
use memburn_core::history::{self, History};
use memburn_core::memtest::{self, TestId};
use memburn_core::rng::splitmix64;
use memburn_core::smbios;
use memburn_core::tui::{self, Current, ErrLine, Perf, Places, Step, StepState, Verdict, View};
use uefi::boot;
use uefi::fs::FileSystem;
use uefi::prelude::*;
use uefi::runtime::{self, ResetType};

use config::{Config, Mode};
use screen::{Input, Screen};
use smp::Smp;

const REFRESH_MS: u64 = 250;
const SAMPLE_MS: u64 = 1000;
const MENU_SECS: u64 = 10;
const INJECT_MS: u64 = 400;
const QUICK_BURN_SECS: u64 = 120;
const BURN_SECS: u64 = 600;
const MAX_ERR_LINES: usize = 500;
const RESERVE_ROW: &str = "Firmware reserve, moved";
/// Runs whose coverage is combined into the multi-run figure, this one included.
const COVERAGE_RUNS: usize = 10;
/// `since` calls per error kind per report; 4 x 16 covers the 64-entry memory ring.
const REPORT_BATCHES: usize = 4;

#[derive(Clone, Copy)]
struct Clock {
    hz: u64,
}

impl Clock {
    fn calibrate() -> Clock {
        let t0 = cpu::rdtsc();
        boot::stall(Duration::from_millis(100));
        Clock { hz: (cpu::rdtsc() - t0) * 10 }
    }

    fn ms_since(&self, t0: u64) -> u64 {
        ((cpu::rdtsc().wrapping_sub(t0)) as u128 * 1000 / self.hz.max(1) as u128) as u64
    }
}

/// State every CPU reads or updates.
struct Shared {
    stop: AtomicBool,
    log: ErrorLog,
    burn: BurnStats,
}

struct Job {
    name: &'static str,
    /// (test, rounds per pass).
    tests: Vec<(TestId, u32)>,
    passes: u64,
    burn_secs: Option<u64>,
    /// Core cycler: seconds per CPU.
    cores_secs: Option<u64>,
    /// 0 repeats until Esc.
    cycles: u64,
}

impl Job {
    fn from_mode(mode: Mode, cfg: &Config) -> Job {
        let tests: Vec<(TestId, u32)> = cfg.tests.iter().map(|&t| (t, 1)).collect();
        if mode == Mode::Cores {
            let per = cfg.burn_secs.unwrap_or((cycler::DEFAULT_MINUTES * 60.0) as u64);
            let (cores_secs, cycles) = (Some(per), cfg.cycles);
            return Job { name: "Core cycler", tests: Vec::new(), passes: 1, burn_secs: None, cores_secs, cycles };
        }
        let (name, tests, burn_secs) = match mode {
            Mode::Quick => ("Quick check", memtest::QUICK.to_vec(), Some(cfg.burn_secs.unwrap_or(QUICK_BURN_SECS))),
            Mode::Mem => ("Memory test", tests, None),
            Mode::Burn => ("Burn-in", Vec::new(), Some(cfg.burn_secs.unwrap_or(3 * BURN_SECS))),
            Mode::Both => ("Memory test + burn-in", tests, Some(cfg.burn_secs.unwrap_or(BURN_SECS))),
            Mode::Cores => unreachable!(),
        };
        let cycles = if mode == Mode::Quick { 1 } else { cfg.cycles };
        Job { name, tests, passes: cfg.passes, burn_secs, cores_secs: None, cycles }
    }

    fn plan(&self, cores: &[(usize, String)]) -> Vec<Step> {
        let step = |name: &str, note: String| Step { name: name.into(), note, state: StepState::Pending };
        if let Some(s) = self.cores_secs {
            return cores.iter().map(|(_, label)| step(label, tui::clock(s * 1000))).collect();
        }
        let mut v: Vec<Step> = self.tests.iter().map(|&(t, r)| step(&memtest::label(t, r), String::new())).collect();
        if !self.tests.is_empty() {
            v.push(step(RESERVE_ROW, String::new()));
        }
        if let Some(s) = self.burn_secs {
            v.push(step("FFT burn-in", tui::clock(s * 1000)));
        }
        v
    }
}

/// BSP-only display state.
struct Ui {
    screen: Screen,
    view: View,
    clock: Clock,
    t0: u64,
    drawn: u64,
    sampled: u64,
    next_mem: u64,
    next_burn: u64,
    places: Places,
    /// Earlier runs, from the history file on the boot drive.
    history: History,
    /// Self-test: time of the last injected memory fault and a counter.
    inject: Option<(u64, u64)>,
    /// TSC and counter value at the previous chart sample.
    prev: (u64, u64),
    /// Bytes under test in the last memory pass.
    tested: u64,
    sensor: Option<thermal::Sensor>,
    /// Highest CPU temperature of the current job, °C.
    peak_temp: Option<i32>,
    /// Physical ranges the current job's memory test covered.
    covered: Ranges,
    /// Processor numbers and row labels in core cycler order.
    cores: Vec<(usize, String)>,
}

impl Ui {
    /// True at most every [`REFRESH_MS`]; the caller then updates the view and calls `refresh`.
    fn due(&mut self) -> bool {
        if self.clock.ms_since(self.drawn) < REFRESH_MS {
            return false;
        }
        self.drawn = cpu::rdtsc();
        true
    }

    /// Starts the next chart sample's window at `value`, the memory traffic or flop counter.
    fn rebase(&mut self, value: u64) {
        self.prev = (cpu::rdtsc(), value);
        self.sampled = self.prev.0;
    }

    /// Records a chart sample at most once a second, the rate since the previous one: MB/s or tenths of GFLOP/s.
    fn sample(&mut self, traffic: Option<u64>, flops: Option<u64>) {
        if self.clock.ms_since(self.sampled) < SAMPLE_MS {
            return;
        }
        let (t, v) = self.prev;
        let value = traffic.or(flops).unwrap_or(v);
        let ms = self.clock.ms_since(t).max(1);
        self.sampled = cpu::rdtsc();
        self.prev = (self.sampled, value);
        let delta = value.saturating_sub(v);
        let mem = traffic.map(|_| (delta / ms / 1000) as u32);
        let gflops10 = flops.map(|_| (delta / ms / 100_000) as u32);
        self.view.perf.push(mem, gflops10);
        let temp = self.sensor.as_ref().and_then(thermal::Sensor::read);
        self.view.perf.temp_c = temp;
        self.peak_temp = self.peak_temp.max(temp);
    }

    fn draw(&mut self) {
        if matches!(self.view.verdict, Verdict::Running | Verdict::Stopping) {
            self.view.elapsed_ms = self.clock.ms_since(self.t0);
        }
        self.screen.draw(&self.view);
    }

    /// Handles Esc, reports new errors and redraws.
    fn refresh(&mut self, sh: &Shared) {
        if let Input::Escape = screen::read_key() {
            sh.stop.store(true, Ordering::Relaxed);
        }
        if sh.stop.load(Ordering::Relaxed) && self.view.verdict == Verdict::Running {
            self.view.verdict = Verdict::Stopping;
        }
        self.report(sh);
        self.draw();
    }

    fn step(&mut self, index: usize, state: StepState) {
        if let Some(s) = self.view.plan.get_mut(index) {
            s.state = state;
        }
    }

    /// Adds up to [`REPORT_BATCHES`] batches of errors found since the last call to the view and the log.
    fn report(&mut self, sh: &Shared) {
        let before = self.next_mem + self.next_burn;
        let at_ms = self.clock.ms_since(self.t0);
        let mut mem = [MemError::default(); 16];
        for _ in 0..REPORT_BATCHES {
            if self.next_mem >= sh.log.count() {
                break;
            }
            if self.next_mem == 0 {
                self.screen.log(format!("!! {}", errlog::MEM_BANNER));
            }
            let (n, next) = sh.log.since(self.next_mem, &mut mem);
            let skipped = next - self.next_mem - n as u64;
            if skipped > 0 {
                self.screen.log(format!("  ... {skipped} more memory errors not shown"));
            }
            for e in &mem[..n] {
                let phys = Some(e.addr as u64);
                self.screen.log(format!(
                    "  MEMORY ERROR at {}: {e}, {} GiB from 0",
                    tui::at(e.addr, phys),
                    gib_frac(e.addr as u64)
                ));
                self.places.add(e);
                self.view.errors.push(ErrLine::mem(at_ms, e, phys));
            }
            self.next_mem = next;
        }
        let zero = BurnError { worker: 0, log2n: 0, seed: 0, fault: Fault::WrongResult, max_err: 0 };
        let mut cpu_errs = [zero; 16];
        for _ in 0..REPORT_BATCHES {
            if self.next_burn >= sh.burn.errors.count() {
                break;
            }
            if self.next_burn == 0 {
                self.screen.log(format!("!! {}", errlog::BURN_BANNER));
            }
            let (n, next) = sh.burn.errors.since(self.next_burn, &mut cpu_errs);
            let skipped = next - self.next_burn - n as u64;
            if skipped > 0 {
                self.screen.log(format!("  ... {skipped} more CPU errors not shown"));
            }
            for e in &cpu_errs[..n] {
                self.screen.log(format!("  CPU ERROR {e}"));
                self.view.errors.push(ErrLine::cpu(at_ms, e));
            }
            self.next_burn = next;
        }
        if before != self.next_mem + self.next_burn {
            let extra = self.view.errors.len().saturating_sub(MAX_ERR_LINES);
            self.view.errors.drain(..extra);
            self.view.mem_errors = sh.log.count();
            self.view.cpu_errors = sh.burn.errors.count();
            self.view.bits = tui::bit_stats(&sh.log, &self.places, Some(&self.history));
        }
    }

    /// Self-test: flips one bit in test memory every [`INJECT_MS`].
    fn inject(&mut self, chunks: &[Chunk]) {
        let Some((at, k)) = self.inject else { return };
        if self.clock.ms_since(at) < INJECT_MS || chunks.is_empty() {
            return;
        }
        let c = chunks[splitmix64(k) as usize % chunks.len()];
        let w = splitmix64(!k) as usize % c.words;
        unsafe { *c.ptr.add(w) ^= 1 << (k % 64) };
        self.inject = Some((cpu::rdtsc(), k + 1));
    }
}

#[entry]
fn main() -> Status {
    uefi::helpers::init().unwrap();
    let _ = boot::set_watchdog_timer(0, 0x10000, None);
    unsafe { cpu::enable_simd() };
    let cfg = config::load();
    let smp = Smp::new(cfg.cpus);
    let isa = cpu::best_isa();
    let clock = Clock::calibrate();
    let mut brand = [0u8; 48];
    let brand = String::from(cpu::brand(&mut brand));
    let view = View {
        version: env!("CARGO_PKG_VERSION"),
        system: [brand.clone(), format!("{} CPUs", smp.cpus), isa.name().into(), gib(ram::usable_total())]
            .into_iter()
            .chain([String::from("UEFI, no network")])
            .chain(cfg.problems.iter().cloned())
            .collect(),
        dimms: dimms(),
        current: Current::Busy(String::from("Preparing FFT tables")),
        ..View::default()
    };
    let mut ui = Ui {
        screen: Screen::new(cfg.debugcon),
        view,
        clock,
        t0: cpu::rdtsc(),
        drawn: 0,
        sampled: 0,
        next_mem: 0,
        next_burn: 0,
        places: Places::default(),
        history: History::parse(&read_history()),
        inject: cfg.inject.then(|| (cpu::rdtsc(), 1)),
        prev: (0, 0),
        tested: 0,
        sensor: thermal::Sensor::find(),
        peak_temp: None,
        covered: Ranges::default(),
        cores: smp.order(),
    };
    ui.screen.log(format!(
        "memburn {}  |  {brand}  |  {} CPUs  |  {}",
        env!("CARGO_PKG_VERSION"),
        smp.cpus,
        isa.name()
    ));
    for p in &cfg.problems {
        ui.screen.log(p.clone());
    }
    for d in &ui.view.dimms {
        ui.screen.log(format!("Memory module: {}", d.line()));
    }
    if let Some(u) = &ui.history.last_unfinished {
        ui.view.notice = format!("Previous run did not finish: {}, {}", u.job, u.date);
        ui.screen.log(ui.view.notice.clone());
    }
    let t = cpu::rdtsc();
    ui.draw();
    ui.screen.log(format!("Full redraw: {} ms", ui.clock.ms_since(t)));
    let plans: Vec<Plan> = cfg.fft.iter().map(|&n| Plan::new(n)).collect();

    let mut job = cfg.mode.map(|m| Job::from_mode(m, &cfg));
    let mut first = true;
    loop {
        let j = match job.take() {
            Some(j) => j,
            None => match choose(&mut ui, &cfg, first) {
                Some(j) => j,
                None => return Status::SUCCESS,
            },
        };
        first = false;
        run_job(&mut ui, &smp, &cfg, isa, &plans, &j);
        if cfg.shutdown {
            runtime::reset(ResetType::SHUTDOWN, Status::SUCCESS, None);
        }
    }
}

/// One round of each test.
fn once(tests: &[TestId]) -> Vec<(TestId, u32)> {
    tests.iter().map(|&t| (t, 1)).collect()
}

fn est(secs: u64) -> String {
    format!("~{} min", secs.div_ceil(60).max(1))
}

/// Menu with a countdown to the quick check or, after a run, the result screen with the same choices.
fn choose(ui: &mut Ui, cfg: &Config, countdown: bool) -> Option<Job> {
    let gib10 = (ram::usable_total() * 10) >> 30;
    let sweep_secs = |j: &Job| gib10 * memtest::sweeps(&j.tests) * memtest::SWEEP_MS_PER_GIB / 10_000;
    let quick = Job::from_mode(Mode::Quick, cfg);
    let full =
        Job { name: "Full memory test", tests: once(&memtest::DEFAULT), cycles: 1, ..Job::from_mode(Mode::Mem, cfg) };
    let burn = Job { burn_secs: Some(30 * 60), cycles: 1, ..Job::from_mode(Mode::Burn, cfg) };
    let all = Job {
        name: "All, repeating",
        tests: once(&memtest::DEFAULT),
        burn_secs: Some(10 * 60),
        cycles: 0,
        ..Job::from_mode(Mode::Both, cfg)
    };
    let cores = Job {
        cores_secs: Some((cycler::DEFAULT_MINUTES * 60.0) as u64),
        cycles: 0,
        ..Job::from_mode(Mode::Cores, cfg)
    };
    let v = &mut ui.view;
    v.keys = if countdown {
        Vec::new()
    } else {
        vec![("Enter", "quick"), ("F", "full memory"), ("B", "burn-in"), ("C", "cores"), ("A", "all")]
    };
    v.keys.extend([("R", "restart"), ("P", "power off"), ("Esc", "firmware")]);
    if countdown {
        v.job = String::from(quick.name);
        v.plan = quick.plan(&ui.cores);
        v.current = Current::Menu(vec![
            ("Enter", "Quick check".into(), est(sweep_secs(&quick) + QUICK_BURN_SECS)),
            ("F", "Full memory test".into(), est(sweep_secs(&full))),
            ("B", "Burn-in".into(), "30 min".into()),
            ("C", "Core cycler".into(), format!("{} min per CPU", cycler::DEFAULT_MINUTES)),
            ("A", "All, repeating".into(), "until Esc".into()),
        ]);
    }
    let t0 = cpu::rdtsc();
    loop {
        if countdown {
            let left = MENU_SECS.saturating_sub(ui.clock.ms_since(t0) / 1000);
            ui.view.verdict = Verdict::Menu(Some(left as u32));
            if left == 0 {
                return Some(quick);
            }
        }
        ui.draw();
        match screen::read_key() {
            Input::Char('\r') => return Some(quick),
            Input::Char('F') => return Some(full),
            Input::Char('B') => return Some(burn),
            Input::Char('C') => return Some(cores),
            Input::Char('A') => return Some(all),
            Input::Char('R') => runtime::reset(ResetType::COLD, Status::SUCCESS, None),
            Input::Char('P') => runtime::reset(ResetType::SHUTDOWN, Status::SUCCESS, None),
            Input::Escape => return None,
            _ => boot::stall(Duration::from_millis(50)),
        }
    }
}

fn run_job(ui: &mut Ui, smp: &Smp, cfg: &Config, isa: Isa, plans: &[Plan], job: &Job) {
    let sh = Shared { stop: AtomicBool::new(false), log: ErrorLog::new(), burn: BurnStats::default() };
    if let Err(e) = append_history(&history::start_line(&now(), job.name)) {
        ui.screen.log(format!("Could not update {}: {e}", history::FILE));
    }
    ui.view.notice.clear();
    ui.t0 = cpu::rdtsc();
    ui.next_mem = 0;
    ui.next_burn = 0;
    ui.places = Places::default();
    let v = &mut ui.view;
    v.job = String::from(job.name);
    v.plan = job.plan(&ui.cores);
    v.verdict = Verdict::Running;
    v.current = Current::Idle;
    v.perf = Perf::default();
    ui.peak_temp = None;
    ui.covered = Ranges::default();
    v.mem_errors = 0;
    v.cpu_errors = 0;
    v.bits.clear();
    v.errors.clear();
    v.advice.clear();
    v.keys = vec![("Esc", "stop")];
    let repeat = match job.cycles {
        0 => String::from(", repeating until Esc"),
        1 => String::new(),
        n => format!(", {n} times"),
    };
    let burn = job.burn_secs.map_or(String::new(), |s| format!(", {} burn-in", tui::clock(s * 1000)));
    match job.cores_secs {
        Some(c) => ui.screen.log(format!("{}: {} per CPU{repeat}", job.name, tui::clock(c * 1000))),
        None => ui.screen.log(format!("{}: {} memory tests{burn}{repeat}", job.name, job.tests.len())),
    }
    if cfg.inject {
        ui.screen.log(String::from("Self-test: errors are being injected on purpose."));
    }
    let mut cycle = 0;
    while (job.cycles == 0 || cycle < job.cycles) && !sh.stop.load(Ordering::Relaxed) {
        if job.cycles != 1 {
            ui.view.job = format!("{}  ·  cycle {}", job.name, cycle + 1);
            for s in &mut ui.view.plan {
                s.state = StepState::Pending;
            }
        }
        if !job.tests.is_empty() {
            run_mem(&sh, ui, smp, cfg, isa, job, cycle);
        }
        if let Some(secs) = job.cores_secs {
            run_cores(&sh, ui, smp, cfg, isa, secs, cycle);
        }
        if let Some(secs) = job.burn_secs
            && !sh.stop.load(Ordering::Relaxed)
        {
            let step = job.tests.len() + usize::from(!job.tests.is_empty());
            run_burn(&sh, ui, smp, cfg, isa, plans, secs, step);
        }
        cycle += 1;
    }
    summary(&sh, ui, job);
}

fn summary(sh: &Shared, ui: &mut Ui, job: &Job) {
    ui.report(sh);
    let mem_errors = sh.log.count();
    let burn_errors = sh.burn.errors.count();
    let stopped = sh.stop.load(Ordering::Relaxed);
    let (verdict, word) = if mem_errors + burn_errors > 0 {
        (Verdict::Fail, "FAIL")
    } else if stopped {
        (Verdict::Stopped, "STOPPED EARLY, no errors so far")
    } else {
        (Verdict::Pass, "PASS")
    };
    let ffts = sh.burn.iters.load(Ordering::Relaxed);
    let worst = f64::from_bits(sh.burn.max_err.load(Ordering::Relaxed));
    let total = tui::clock(ui.clock.ms_since(ui.t0));
    let mut rows: Vec<(String, String)> = Vec::new();
    let bits = if mem_errors > 0 { format!(" in {}", BitList(sh.log.bits())) } else { String::new() };
    ui.screen.log(format!("Result: {word}"));
    if !job.tests.is_empty() {
        ui.screen.log(format!("  Memory:  {mem_errors} error{}{bits}", errlog::plural(mem_errors)));
        rows.push(("Memory tested".into(), format!("{}, {} tests", gib(ui.tested), job.tests.len())));
        if mem_errors > 0 {
            rows.push(("Memory errors".into(), format!("{}{bits}", tui::group(mem_errors))));
        }
    }
    if job.burn_secs.is_some() || job.cores_secs.is_some() {
        ui.screen.log(format!(
            "  Burn-in: {burn_errors} error{} in {ffts} FFTs, worst rounding {worst:.4} (limit {MAX_ERR})",
            errlog::plural(burn_errors)
        ));
        rows.push(("FFTs checked".into(), tui::group(ffts)));
        rows.push(("Worst rounding".into(), format!("{worst:.4} (limit {MAX_ERR})")));
    }
    if job.cores_secs.is_some() {
        let value = tui::cores_summary(&ui.view.plan);
        ui.screen.log(format!("  Cores: {value}"));
        rows.push(("Cores".into(), value));
    }
    let ram = ram::system_ram();
    let footprint = (!ui.covered.is_empty() && !ram.is_empty()).then(|| {
        let lines: Vec<String> = ui.view.dimms.iter().map(|d| d.line()).collect();
        let host = coverage::host_id(lines.iter().map(String::as_str));
        Footprint { ram_mib: ram.bytes() >> 20, host, tested: ui.covered.intersect(&ram) }
    });
    if let Some(f) = &footprint {
        let pct = coverage::fmt_percent(coverage::percent(&f.tested, &ram));
        let (union, runs) = ui.history.union_with(f, COVERAGE_RUNS);
        let over = coverage::fmt_percent(coverage::percent(&union, &ram));
        let value = if runs > 1 { format!("{pct} (over {runs} runs {over})") } else { pct };
        ui.screen.log(format!("  Coverage: {value} of {} System RAM", gib(ram.bytes())));
        let gaps: Vec<String> = ram.minus(&f.tested).largest(3).into_iter().map(coverage::fmt_range).collect();
        if !gaps.is_empty() {
            ui.screen.log(format!("  Largest untested: {}", gaps.join(", ")));
        }
        rows.push(("Coverage".into(), value));
    }
    let line = if job.tests.is_empty() {
        history::end_line(&now(), burn_errors)
    } else {
        if ui.history.runs + ui.history.unfinished > 0 {
            rows.push(("Earlier runs".into(), ui.history.summary()));
        }
        ui.history.add(sh.log.bits());
        if let Some(f) = &footprint {
            ui.history.footprints.push(f.clone());
        }
        history::line(&now(), mem_errors, burn_errors, sh.log.bits(), footprint.as_ref())
    };
    if let Err(e) = append_history(&line) {
        ui.screen.log(format!("Could not update {}: {e}", history::FILE));
    }
    if mem_errors > 0 {
        ui.screen.log(format!("  Find the stick: {}", tui::locate(sh.log.bits())));
    }
    if let Some(t) = ui.peak_temp {
        ui.screen.log(format!("  Peak CPU temperature: {t} °C"));
        rows.push(("Peak CPU temp".into(), format!("{t} °C")));
    }
    rows.push(("Total time".into(), total.clone()));
    let advice = if mem_errors + burn_errors > 0 || stopped {
        String::from(errlog::advice(mem_errors, burn_errors))
    } else {
        format!("No errors in {total}")
    };
    ui.screen.log(format!("  {advice}"));
    rows.push((
        "Log".into(),
        match save_log(&ui.screen.log_text()) {
            Ok(()) => String::from("memburn.log on the boot drive"),
            Err(e) => format!("not saved: {e}"),
        },
    ));
    rows.extend(tui::module_rows(&ui.view.dimms, sh.log.bits()));
    let v = &mut ui.view;
    v.verdict = verdict;
    v.current = Current::Summary(rows);
    v.advice = advice;
    ui.draw();
}

fn run_mem(sh: &Shared, ui: &mut Ui, smp: &Smp, cfg: &Config, isa: Isa, job: &Job, cycle: u64) {
    ui.view.current = Current::Busy(String::from("Claiming free memory"));
    ui.draw();
    let mut claimed = ram::claim(cfg.mem_limit.unwrap_or(u64::MAX));
    let chunks = chunk_regions(&claimed.regions, chunk_bytes(claimed.bytes(), smp.cpus));
    let tested: u64 = chunks.iter().map(|c| c.words as u64 * 8).sum();
    ui.tested = tested;
    ui.screen.log(format!(
        "Cycle {}: memory test of {} on {} CPUs ({} in use by firmware and memburn is not tested)",
        cycle + 1,
        gib(tested),
        smp.cpus,
        gib(ram::usable_total().saturating_sub(tested))
    ));
    let done = test_passes(sh, ui, smp, cfg, isa, job, &chunks, cycle * job.passes, job.passes, None);
    drop(chunks);
    if done {
        ui.covered = ui.covered.union(&claimed.ranges());
        let row = job.tests.len();
        ui.step(row, StepState::Running);
        let (t0, e0) = (cpu::rdtsc(), sh.log.count());
        let regions = claimed.rotate();
        let bytes: u64 = regions.iter().map(|r| r.1 as u64).sum();
        let chunks = chunk_regions(&regions, chunk_bytes(bytes, smp.cpus));
        if !chunks.is_empty() && test_passes(sh, ui, smp, cfg, isa, job, &chunks, cycle * job.passes, 1, Some(row)) {
            let tested = Ranges::new(regions.iter().map(|&(a, n)| (a as u64, (a + n) as u64)).collect());
            ui.covered = ui.covered.union(&tested);
        }
        let errors = sh.log.count() - e0;
        ui.screen.log(format!(
            "  firmware reserve: {} MiB of {} MiB moved and tested  {}",
            bytes >> 20,
            ram::RESERVE_BYTES >> 20,
            errors_word(errors)
        ));
        ui.step(row, StepState::Done { ms: ui.clock.ms_since(t0), errors });
    }
    ui.report(sh);
    claimed.release();
}

/// Runs `passes` passes of the job's tests over `chunks`; per-test plan rows, or all on `row`. False when stopped.
#[allow(clippy::too_many_arguments)]
fn test_passes(
    sh: &Shared,
    ui: &mut Ui,
    smp: &Smp,
    cfg: &Config,
    isa: Isa,
    job: &Job,
    chunks: &[Chunk],
    first_pass: u64,
    passes: u64,
    row: Option<usize>,
) -> bool {
    let tested: u64 = chunks.iter().map(|c| c.words as u64 * 8).sum();
    let tests = &job.tests;
    let per_pass = memtest::sweeps(tests) * tested;
    let moved = Moved::default();
    let start = cpu::rdtsc();
    ui.rebase(0);
    let clock = ui.clock;

    for p in 0..passes {
        let pass = first_pass + p;
        for (ti, &(t, rounds)) in tests.iter().enumerate() {
            let (t0, b0, e0) = (cpu::rdtsc(), moved.traffic(), sh.log.count());
            if row.is_none() {
                ui.step(ti, StepState::Running);
            }
            for (r, ph) in (0..rounds).flat_map(|r| (0..t.phases()).map(move |ph| (r, ph))) {
                let pj = PhaseJob::new(t, ph, memtest::round_pass(pass, r), chunks, &moved, &sh.log, &sh.stop, isa);
                let mut tick = || {
                    ui.inject(chunks);
                    if !ui.due() {
                        return;
                    }
                    let d = moved.tested();
                    let in_pass = (d - (p * per_pass).min(d)).min(per_pass);
                    let ms = clock.ms_since(start);
                    let left_ms = (per_pass - in_pass) as u128 * ms as u128 / d.max(1) as u128;
                    ui.sample(Some(moved.traffic()), None);
                    ui.view.current = Current::Mem {
                        test: t,
                        pass: p as u32 + 1,
                        passes: passes as u32,
                        index: ti as u32 + 1,
                        count: tests.len() as u32,
                        step: r * t.phases() as u32 + ph as u32 + 1,
                        steps: rounds * t.phases() as u32,
                        done: (in_pass as u128 * tested as u128 / per_pass.max(1) as u128) as u64,
                        total: tested,
                        left_ms: (d > 0).then_some(left_ms as u64),
                    };
                    ui.refresh(sh);
                };
                smp.run(&|_| pj.work(), || pj.work_one(), &mut tick);
                if sh.stop.load(Ordering::Relaxed) {
                    return false;
                }
                if t.pause_after(ph) {
                    let h0 = cpu::rdtsc();
                    let hold = cfg.fade_secs * 1000;
                    while clock.ms_since(h0) < hold && !sh.stop.load(Ordering::Relaxed) {
                        boot::stall(Duration::from_millis(10));
                        if ui.due() {
                            ui.view.current = Current::Hold { left_ms: hold.saturating_sub(clock.ms_since(h0)) };
                            ui.refresh(sh);
                        }
                    }
                }
            }
            ui.report(sh);
            if row.is_some() {
                continue;
            }
            let ms = clock.ms_since(t0);
            let errors = sh.log.count() - e0;
            ui.step(ti, StepState::Done { ms, errors });
            ui.screen.log(format!(
                "  pass {}  {:<31} {:>9}  {:>10}  {}",
                pass + 1,
                memtest::label(t, rounds),
                tui::clock(ms),
                rate(moved.traffic() - b0, ms),
                errors_word(errors)
            ));
        }
    }
    !sh.stop.load(Ordering::Relaxed)
}

struct Burners<'a>(Vec<UnsafeCell<Burner<'a>>>);

/// One core cycler turn's burners, used by the one CPU running the turn.
struct Turn<'a>(UnsafeCell<Vec<Burner<'a>>>);

unsafe impl Sync for Turn<'_> {}

impl<'a> Turn<'a> {
    /// Safety: only the CPU running the turn calls this, once.
    #[allow(clippy::mut_from_ref)]
    unsafe fn list(&self) -> &mut Vec<Burner<'a>> {
        unsafe { &mut *self.0.get() }
    }
}

unsafe impl Sync for Burners<'_> {}

#[allow(clippy::too_many_arguments)]
fn run_burn(sh: &Shared, ui: &mut Ui, smp: &Smp, cfg: &Config, isa: Isa, plans: &[Plan], secs: u64, step: usize) {
    let span = match (plans.first(), plans.last()) {
        (Some(a), Some(b)) if plans.len() > 1 => format!("{{{}..{}}}", a.log2n, b.log2n),
        (Some(a), _) => format!("{}", a.log2n),
        _ => String::new(),
    };
    ui.screen.log(format!(
        "Burn-in: {} on {} CPUs, {}, FFT sizes 2^{span}",
        tui::clock(secs * 1000),
        smp.cpus,
        isa.name()
    ));
    ui.step(step, StepState::Running);
    if cfg.inject {
        sh.burn.inject.store(true, Ordering::Relaxed);
    }
    let burners = Burners((0..smp.cpus).map(|i| UnsafeCell::new(Burner::new(isa, plans, i as u64))).collect());
    let burners = &burners;
    let halt = AtomicBool::new(false);
    let (f0, i0, e0) =
        (sh.burn.flops.load(Ordering::Relaxed), sh.burn.iters.load(Ordering::Relaxed), sh.burn.errors.count());
    let t0 = cpu::rdtsc();
    let clock = ui.clock;
    let limit_ms = secs * 1000;
    ui.rebase(f0);
    let ap = |i: usize| unsafe { (*burners.0[i].get()).run(&sh.burn, &halt) };
    let bsp = || {
        unsafe { (*burners.0[0].get()).step(&sh.burn) };
        if clock.ms_since(t0) >= limit_ms || sh.stop.load(Ordering::Relaxed) {
            halt.store(true, Ordering::Relaxed);
        }
        !halt.load(Ordering::Relaxed)
    };
    let tick = || {
        if !ui.due() {
            return;
        }
        ui.sample(None, Some(sh.burn.flops.load(Ordering::Relaxed)));
        ui.view.current = Current::Burn {
            isa: isa.name(),
            threads: smp.cpus as u32,
            sizes: span.clone(),
            ffts: sh.burn.iters.load(Ordering::Relaxed) - i0,
            elapsed_ms: clock.ms_since(t0),
            total_ms: limit_ms,
            worst: sh.burn.max_err.load(Ordering::Relaxed),
        };
        ui.refresh(sh);
    };
    smp.run(&ap, bsp, tick);
    ui.report(sh);
    let ms = clock.ms_since(t0);
    let errors = sh.burn.errors.count() - e0;
    ui.step(step, StepState::Done { ms, errors });
    ui.screen.log(format!(
        "  burn-in  {}  {} GFLOP/s avg  {} FFTs  {}",
        tui::clock(ms),
        rate_g(sh.burn.flops.load(Ordering::Relaxed) - f0, ms),
        sh.burn.iters.load(Ordering::Relaxed) - i0,
        errors_word(errors)
    ));
}

/// One turn per CPU in APIC id order: FFTs on that CPU alone, the BSP drawing in between.
fn run_cores(sh: &Shared, ui: &mut Ui, smp: &Smp, cfg: &Config, isa: Isa, secs: u64, cycle: u64) {
    let isas = cycler::isas(isa);
    let names: Vec<&'static str> = isas.iter().map(|i| i.name()).collect();
    let plans: Vec<Plan> = cycler::SIZES.iter().map(|&s| Plan::new(s)).collect();
    let cores = ui.cores.clone();
    let bsp = smp.bsp();
    let per_ms = secs * 1000;
    ui.screen.log(format!(
        "Core cycler, cycle {}: {} per CPU on {} CPUs, {}, FFT sizes 2^{{10..13}}, {} s idle each minute",
        cycle + 1,
        tui::clock(per_ms),
        cores.len(),
        names.join(" and "),
        cycler::IDLE_MS / 1000
    ));
    let clock = ui.clock;
    for (idx, (n, label)) in cores.iter().enumerate() {
        if sh.stop.load(Ordering::Relaxed) {
            break;
        }
        ui.step(idx, StepState::Running);
        if cfg.inject && idx == 1 % cores.len() {
            sh.burn.inject.store(true, Ordering::Relaxed);
        }
        let (i0, e0, f0) =
            (sh.burn.iters.load(Ordering::Relaxed), sh.burn.errors.count(), sh.burn.flops.load(Ordering::Relaxed));
        let burners = Turn(UnsafeCell::new(cycler::burners(&isas, &plans, *n)));
        let state = AtomicU32::new(0);
        let t0 = cpu::rdtsc();
        let now = || clock.ms_since(t0);
        ui.rebase(f0);
        let tick = |ui: &mut Ui| {
            if !ui.due() {
                return;
            }
            ui.sample(None, Some(sh.burn.flops.load(Ordering::Relaxed)));
            ui.view.current = Current::Core {
                label: label.clone(),
                index: idx as u32 + 1,
                count: cores.len() as u32,
                cycle: cycle as u32 + 1,
                isa: match state.load(Ordering::Relaxed) {
                    0 => None,
                    k => names.get(k as usize - 1).copied(),
                },
                ffts: sh.burn.iters.load(Ordering::Relaxed) - i0,
                elapsed_ms: clock.ms_since(t0),
                total_ms: per_ms,
            };
            ui.refresh(sh);
        };
        let mut ran = true;
        if *n == bsp {
            let mut after = |idle: bool| {
                tick(ui);
                if idle {
                    boot::stall(Duration::from_millis(1));
                }
            };
            let list = unsafe { burners.list() };
            cycler::run_turn(list, &sh.burn, &sh.stop, per_ms, &now, &mut after, &state);
        } else {
            let ap = || {
                let list = unsafe { burners.list() };
                let mut after = |idle: bool| {
                    if idle {
                        let w = cpu::rdtsc();
                        while clock.ms_since(w) < 1 {
                            core::hint::spin_loop();
                        }
                    }
                };
                cycler::run_turn(list, &sh.burn, &sh.stop, per_ms, &now, &mut after, &state);
            };
            ran = smp.run_on(*n, &ap, || tick(ui));
        }
        drop(burners);
        ui.report(sh);
        let mut r = cycler::CoreResult::new(*n, label.clone());
        r.ms = clock.ms_since(t0);
        r.ffts = sh.burn.iters.load(Ordering::Relaxed) - i0;
        r.errors = sh.burn.errors.count() - e0;
        r.fault = (r.errors > 0).then(|| last_fault(&sh.burn)).flatten();
        if let Some(s) = ui.view.plan.get_mut(idx) {
            s.name = r.row();
        }
        ui.step(idx, StepState::Done { ms: r.ms, errors: r.errors });
        let note = if ran { "" } else { " (could not start this CPU)" };
        ui.screen.log(format!(
            "  {:<34} {:>9}  {} FFTs  {}{note}",
            r.row(),
            tui::clock(r.ms),
            r.ffts,
            errors_word(r.errors)
        ));
    }
}

/// Fault of the newest recorded CPU error.
fn last_fault(burn: &BurnStats) -> Option<Fault> {
    let n = burn.errors.count();
    let mut e = [BurnError { worker: 0, log2n: 0, seed: 0, fault: Fault::WrongResult, max_err: 0 }];
    let (got, _) = burn.errors.since(n.checked_sub(1)?, &mut e);
    (got == 1).then_some(e[0].fault)
}

fn save_log(text: &str) -> Result<(), String> {
    let sfs = boot::get_image_file_system(boot::image_handle()).map_err(|e| format!("{e:?}"))?;
    FileSystem::new(sfs).write(uefi::cstr16!("\\memburn.log"), text.as_bytes()).map_err(|e| format!("{e:?}"))
}

/// Memory modules from the firmware's SMBIOS table.
fn dimms() -> Vec<smbios::Dimm> {
    use uefi::table::cfg::ConfigTableEntry;
    let entry = uefi::system::with_config_table(|t| {
        [ConfigTableEntry::SMBIOS3_GUID, ConfigTableEntry::SMBIOS_GUID]
            .iter()
            .find_map(|g| t.iter().find(|e| e.guid == *g).map(|e| e.address as usize))
    });
    let Some(ep) = entry.filter(|&a| a != 0) else { return Vec::new() };
    // Firmware memory is identity-mapped.
    let ep = unsafe { core::slice::from_raw_parts(ep as *const u8, smbios::ENTRY_LEN) };
    match smbios::table_at(ep) {
        Some((addr, len)) if addr != 0 && len > 0 => {
            smbios::dimms(unsafe { core::slice::from_raw_parts(addr as *const u8, len) })
        }
        _ => Vec::new(),
    }
}

fn read_history() -> String {
    boot::get_image_file_system(boot::image_handle())
        .ok()
        .and_then(|sfs| FileSystem::new(sfs).read_to_string(uefi::cstr16!("\\memburn-history.txt")).ok())
        .unwrap_or_default()
}

fn append_history(line: &str) -> Result<(), String> {
    let mut text = read_history();
    let sfs = boot::get_image_file_system(boot::image_handle()).map_err(|e| format!("{e:?}"))?;
    text.push_str(line);
    text.push_str("\r\n");
    FileSystem::new(sfs).write(uefi::cstr16!("\\memburn-history.txt"), text.as_bytes()).map_err(|e| format!("{e:?}"))
}

/// RTC time as "2026-10-12 11:00", or "-" when the firmware has no clock.
fn now() -> String {
    runtime::get_time().map_or(String::from("-"), |t| {
        format!("{:04}-{:02}-{:02} {:02}:{:02}", t.year(), t.month(), t.day(), t.hour(), t.minute())
    })
}

fn errors_word(n: u64) -> String {
    match n {
        0 => String::from("ok"),
        1 => String::from("1 ERROR"),
        n => format!("{n} ERRORS"),
    }
}

fn gib(bytes: u64) -> String {
    format!("{} GiB", gib_frac(bytes))
}

/// Gibibytes with one decimal, integer math only.
fn gib_frac(bytes: u64) -> String {
    let tenths = bytes * 10 / (1 << 30);
    format!("{}.{}", tenths / 10, tenths % 10)
}

/// Decimal GB/s with one decimal.
fn rate(bytes: u64, ms: u64) -> String {
    let tenths = bytes / (ms.max(1) * 100_000);
    format!("{}.{} GB/s", tenths / 10, tenths % 10)
}

fn rate_g(flops: u64, ms: u64) -> String {
    let tenths = flops / (ms.max(1) * 100_000);
    format!("{}.{}", tenths / 10, tenths % 10)
}
