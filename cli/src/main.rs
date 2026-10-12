mod os;
mod term;
mod ui;
mod update;
mod upload;

use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use memburn_core::coverage::{self, EarlyTest, Footprint, Ranges};
use memburn_core::cpu::{self, Isa};
use memburn_core::cycler;
use memburn_core::engine::{Moved, PhaseJob, chunk_bytes, chunk_regions};
use memburn_core::errlog::{self, BitList, ErrorLog};
use memburn_core::eta::{self, Rate, Shown};
use memburn_core::fft::{BurnError, BurnStats, Burner, Fault, MAX_ERR, MAX_LOG2N, MIN_LOG2N, Plan, Work};
use memburn_core::history::{self, History};
use memburn_core::memtest::{self, TestId};
use memburn_core::smbios;
use memburn_core::tui::{self, Current, ErrKind, ErrLine, Step, StepState, Verdict, View};

use term::Key;
use ui::{Tone, Ui, errors_word};

/// Exit codes the USB stick's init acts on.
const EXIT_RESTART: u8 = 10;
const EXIT_POWER_OFF: u8 = 11;
/// Init runs the binary at [`update::NEXT`] with the same arguments and those in [`update::NEXT_ARGS`].
const EXIT_REEXEC: u8 = 12;
const MENU_SECS: u32 = 10;
/// Test memory shrinks by this much per retry when it cannot be locked.
const LOCK_STEP: usize = 256 << 20;
const EARLY_ROW: &str = "Kernel early test";
/// Runs whose coverage is combined into the multi-run figure, this one included.
const COVERAGE_RUNS: usize = 10;

#[derive(Parser)]
#[command(version, about = "Memory test and FFT burn-in")]
struct Args {
    /// What to run [default: a menu on a terminal, otherwise quick]
    #[arg(short, long, value_enum)]
    mode: Option<Mode>,
    /// Memory to test: size with K/M/G suffix, a percentage of available RAM, or -SIZE for available RAM less SIZE
    #[arg(long, default_value = "85%", allow_hyphen_values = true)]
    mem: String,
    /// Worker threads [default: all logical CPUs]
    #[arg(short, long)]
    threads: Option<usize>,
    /// Memory test passes per cycle
    #[arg(short, long, default_value_t = 1, value_parser = clap::value_parser!(u64).range(1..))]
    passes: u64,
    /// Comma-separated tests (addr,mi01,mi8,mirand,mi64,block,random,mod20,hash,fade,addrmove) or "all" [default: all but fade and addrmove; stress also without hash]
    #[arg(long)]
    tests: Option<String>,
    /// Bit-fade hold time in seconds
    #[arg(long, default_value_t = 300)]
    fade_secs: u64,
    /// Burn-in or stress minutes per cycle [default: quick 2, both 10, burn and stress 30]
    #[arg(short = 'd', long)]
    minutes: Option<f64>,
    /// FFT sizes: small (in cache, most heat), large (spills to RAM), mixed, or log2 sizes such as 12,16,20
    #[arg(long, default_value = "mixed")]
    fft: String,
    /// Instruction set for the burn-in kernels
    #[arg(long, value_enum, default_value_t = IsaArg::Auto)]
    isa: IsaArg,
    /// Repeat this many times; 0 runs until stopped
    #[arg(short, long, default_value_t = 1)]
    cycles: u64,
    /// Line output instead of the full-screen dashboard
    #[arg(long)]
    plain: bool,
    /// Do not send progress to the live-view server
    #[arg(long)]
    no_upload: bool,
    /// Live-view server
    #[arg(long, default_value = "https://reljam.com")]
    server: String,
    /// Append every result and error line to this file as it is printed
    #[arg(long)]
    log: Option<std::path::PathBuf>,
    /// Self-test: corrupt test memory and one FFT on purpose to show how errors are reported
    #[arg(long, hide = true)]
    inject: bool,
    /// Running from the USB stick: restart and power-off keys, exit codes 10 and 11
    #[arg(long, hide = true)]
    boot: bool,
    /// Exit after the run instead of showing the result screen
    #[arg(long, hide = true)]
    exit_when_done: bool,
    /// Print every chart sample (memory MB/s, compute 0.1 GFLOP/s)
    #[arg(long, hide = true)]
    trace_rates: bool,
    /// Extra header item, such as "Secure Boot on"
    #[arg(long, hide = true)]
    tag: Vec<String>,
    /// The stick's init saved a kernel crash log under this name: menu without countdown, `--mode` waits for it
    #[arg(long, hide = true)]
    crash_log: Option<String>,
    /// Test hook: panic the kernel this many seconds into the first run (Linux, root)
    #[arg(long, hide = true)]
    crash_after: Option<u64>,
    /// The USB stick's root: with --boot, offers newer signed releases as updates
    #[arg(long, hide = true)]
    stick: Option<std::path::PathBuf>,
    /// With --stick: start a newer signed release as soon as it is downloaded and checked
    #[arg(long, hide = true)]
    update_auto: bool,
    /// With --stick: no update check
    #[arg(long, hide = true)]
    no_update: bool,
    /// Start this menu job: quick, full, burn, stress, cores or all
    #[arg(long, hide = true)]
    job: Option<String>,
    /// This binary replaced that version in the running session
    #[arg(long, hide = true)]
    updated_from: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Mode {
    /// Three memory tests, then 2 minutes of FFT burn-in
    Quick,
    Mem,
    Burn,
    Both,
    /// Memory test and FFT burn-in at the same time
    Stress,
    /// Small FFTs on one logical CPU at a time; -d is minutes per CPU
    Cores,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum IsaArg {
    Auto,
    Sse2,
    Avx2,
}

#[derive(Clone)]
struct Job {
    name: String,
    /// The menu job's `--job` name; empty for `--mode` jobs.
    key: &'static str,
    /// (test, rounds per pass).
    tests: Vec<(TestId, u32)>,
    passes: u64,
    burn: Option<Duration>,
    stress: Option<Duration>,
    /// Core cycler: time per logical CPU.
    cores: Option<Duration>,
    cycles: u64,
}

struct Ctx<'a> {
    args: &'a Args,
    threads: usize,
    isa: Isa,
    plans: Vec<Plan>,
    mem_bytes: usize,
    /// `--mem` is a share of available RAM, so FFT memory comes out of it.
    mem_share: bool,
    /// Smallest `mem_bytes` to retry with when the test memory cannot be locked.
    mem_floor: usize,
    stop: &'static AtomicBool,
    log: ErrorLog,
    burn: BurnStats,
    out: Ui,
    history_path: Option<std::path::PathBuf>,
    /// Seconds into the next run to crash the kernel; a test hook.
    crash_after: Option<u64>,
    /// The kernel's memtest=N run at boot.
    early: Option<EarlyTest>,
    /// System RAM ranges, when the OS tells.
    ram: Option<Ranges>,
    /// Physical ranges the current job's memory test covered.
    covered: Ranges,
    /// Logical CPUs this process may run on, for the core cycler.
    cpus: Vec<usize>,
    /// The stick's update.
    update: Option<Arc<update::Updater>>,
    /// Memory traffic rate and the shown times left of the test and the job.
    rate: Rate,
    test_left: Shown,
    job_left: Shown,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

impl Ctx<'_> {
    /// What this job's memory test and the kernel's early test covered, with the machine's RAM size and id.
    fn footprint(&self) -> Option<Footprint> {
        let ram = self.ram.as_ref()?;
        let early = self.early.as_ref().map(|e| e.tested.clone()).unwrap_or_default();
        let tested = self.covered.union(&early).intersect(ram);
        let lines: Vec<String> = self.out.view.dimms.iter().map(|d| d.line()).collect();
        let host = coverage::host_id(lines.iter().map(String::as_str));
        (!tested.is_empty()).then_some(Footprint { ram_mib: ram.bytes() >> 20, host, tested })
    }

    /// Bytes the memory test allocates for `job`.
    fn test_bytes(&self, job: &Job) -> usize {
        if !self.mem_share {
            return self.mem_bytes;
        }
        let sizes: Vec<u32> = self.plans.iter().map(|p| p.log2n).collect();
        let burners = if job.stress.is_some() { self.threads - self.threads / 2 } else { 0 };
        mem_budget(self.mem_bytes, &sizes, burners)
    }
}

/// `mem` less the FFT tables and `burners` work buffers, in 2 MiB steps, at least 2 MiB.
fn mem_budget(mem: usize, sizes: &[u32], burners: usize) -> usize {
    let plans: usize = sizes.iter().map(|&s| Plan::bytes(s)).sum();
    let work = sizes.iter().max().map_or(0, |&m| Work::bytes(m));
    let left = mem.saturating_sub(plans + burners * work);
    (left / (2 << 20) * (2 << 20)).max(2 << 20)
}

/// One round of each test.
fn once(tests: &[TestId]) -> Vec<(TestId, u32)> {
    tests.iter().map(|&t| (t, 1)).collect()
}

fn minutes(m: f64) -> Duration {
    Duration::from_secs_f64(m.max(0.0) * 60.0)
}

impl Job {
    fn from_mode(mode: Mode, a: &Args, tests: &[TestId]) -> Job {
        let m = |default: f64| Some(minutes(a.minutes.unwrap_or(default)));
        let tests = once(tests);
        if mode == Mode::Cores {
            let per = m(cycler::DEFAULT_MINUTES);
            return Job {
                name: "Core cycler".into(),
                key: "",
                tests: vec![],
                passes: 1,
                burn: None,
                stress: None,
                cores: per,
                cycles: a.cycles,
            };
        }
        let (name, tests, burn, stress) = match mode {
            Mode::Quick => ("Quick check", memtest::QUICK.to_vec(), m(2.0), None),
            Mode::Mem => ("Memory test", tests, None, None),
            Mode::Burn => ("Burn-in", vec![], m(30.0), None),
            Mode::Both => ("Memory test + burn-in", tests, m(10.0), None),
            Mode::Stress if a.tests.is_none() => ("Stress", once(&memtest::STRESS), None, m(30.0)),
            Mode::Stress => ("Stress", tests, None, m(30.0)),
            Mode::Cores => unreachable!(),
        };
        Job { name: name.into(), key: "", tests, passes: a.passes, burn, stress, cores: None, cycles: a.cycles }
    }

    fn plan(&self, cpus: &[usize]) -> Vec<Step> {
        let step = |name: &str, note: String| Step { name: name.into(), note, state: StepState::Pending };
        if let Some(d) = self.cores {
            return cpus.iter().map(|&n| step(&cpu_label(n), tui::clock(d.as_millis() as u64))).collect();
        }
        if let Some(d) = self.stress {
            return vec![step("Memory test + FFT together", tui::clock(d.as_millis() as u64))];
        }
        let mut v: Vec<Step> = self.tests.iter().map(|&(t, r)| step(&memtest::label(t, r), String::new())).collect();
        if let Some(d) = self.burn {
            v.push(step("FFT burn-in", tui::clock(d.as_millis() as u64)));
        }
        v
    }
}

fn run(args: &Args) -> Result<u8, String> {
    let threads = args.threads.unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get())).max(1);
    let tests = parse_tests(args.tests.as_deref())?;
    let sizes = parse_fft(&args.fft)?;
    let isa = match args.isa {
        IsaArg::Auto => cpu::best_isa(),
        IsaArg::Sse2 => Isa::Sse2,
        IsaArg::Avx2 if cpu::best_isa() == Isa::Avx2Fma => Isa::Avx2Fma,
        IsaArg::Avx2 => return Err("this CPU or OS does not support AVX2+FMA".into()),
    };
    let mem_bytes = parse_mem(&args.mem)?;
    let terminal = std::io::stdout().is_terminal() && std::io::stdin().is_terminal();
    let full_screen = terminal && !args.plain;

    let stop: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
    ctrlc::set_handler(move || {
        if stop.swap(true, Ordering::Relaxed) {
            std::process::exit(130);
        }
    })
    .map_err(|e| e.to_string())?;

    let mut brand = [0u8; 48];
    let brand = cpu::brand(&mut brand).to_string();
    let view = View {
        version: env!("CARGO_PKG_VERSION"),
        system: [brand.clone(), format!("{threads} threads"), isa.name().into(), fmt_bytes(os::total())]
            .into_iter()
            .chain(args.tag.iter().cloned())
            .collect(),
        dimms: smbios::dimms(&os::smbios()),
        ..View::default()
    };
    let mut out = Ui::new(full_screen, view, stop);
    out.trace = args.trace_rates;
    if let Some(path) = &args.log {
        let file = std::fs::OpenOptions::new().create(true).append(true).open(path);
        out.log = Some(file.map_err(|e| format!("cannot open log {}: {e}", path.display()))?);
    }
    out.line(&format!("memburn {}  |  {brand}  |  {threads} threads  |  {}", env!("CARGO_PKG_VERSION"), isa.name()));
    for d in &out.view.dimms.clone() {
        out.line(&format!("Memory module: {}", d.line()));
    }
    out.flush_log();
    if !args.no_upload {
        out.upload(args.server.trim_end_matches('/').to_string());
    }
    let update = match &args.stick {
        Some(stick) if args.boot && !args.no_update => {
            let u = update::start(args.server.trim_end_matches('/'), stick.clone());
            if args.update_auto {
                u.opt_in();
            }
            Some(u)
        }
        _ => None,
    };
    out.updater = update.clone();

    let history_path = history_path(args);
    if let Some(p) = &history_path {
        out.history = Some(std::fs::read_to_string(p).map_or_else(|_| History::default(), |t| History::parse(&t)));
    }
    let unfinished = out.history.as_ref().and_then(|h| h.last_unfinished.as_ref());
    let notice: Vec<String> = unfinished
        .map(|u| format!("Previous run did not finish: {}, {}", u.job, u.date))
        .into_iter()
        .chain(args.crash_log.iter().map(|f| format!("kernel crash log {f}")))
        .chain(args.updated_from.iter().map(|v| format!("updated from {v}")))
        .collect();
    out.view.notice = notice.join("  ·  ");
    if !out.view.notice.is_empty() {
        out.styled(Tone::Warn, &out.view.notice.clone());
        out.flush_log();
    }

    let plans = sizes.iter().map(|&s| Plan::new(s)).collect();
    let (log, burn) = (ErrorLog::new(), BurnStats::default());
    let mem = args.mem.trim();
    let mem_share = mem.ends_with('%') || mem.starts_with('-');
    let mem_floor = if mem.starts_with('-') { mem_bytes.min(parse_mem("95%")?) } else { mem_bytes };
    let crash_after = args.crash_after;
    let early = coverage::parse_early(&os::kernel_log());
    if let Some(e) = &early {
        out.line(&format!(
            "Kernel early test: {} pattern{} over {} in {:.1} s, {} bad range{}",
            e.patterns,
            if e.patterns == 1 { "" } else { "s" },
            fmt_bytes(e.tested.bytes()),
            e.ms as f64 / 1000.0,
            e.bad.len(),
            if e.bad.len() == 1 { "" } else { "s" }
        ));
    }
    let mut c = Ctx {
        args,
        threads,
        isa,
        plans,
        mem_bytes,
        mem_share,
        mem_floor,
        stop,
        log,
        burn,
        out,
        history_path,
        crash_after,
        early,
        ram: os::system_ram(),
        covered: Ranges::default(),
        cpus: os::cpu_list(threads).into_iter().take(args.threads.unwrap_or(usize::MAX)).collect(),
        update,
        rate: Rate::default(),
        test_left: Shown::default(),
        job_left: Shown::default(),
    };
    let mut job = match args.mode {
        _ if args.job.is_some() => {
            let k = args.job.as_deref().unwrap_or_default();
            let j = menu_jobs(args, &tests).into_iter().find(|j| j.key == k);
            Some(j.ok_or(format!("unknown job {k}"))?)
        }
        _ if full_screen && args.crash_log.is_some() => None,
        Some(m) => Some(Job::from_mode(m, args, &tests)),
        None if full_screen => None,
        None => Some(Job::from_mode(Mode::Quick, args, &tests)),
    };
    let mut code = 0;
    let mut first = true;
    let mut countdown = args.crash_log.is_none().then_some(MENU_SECS);
    loop {
        let j = match job.take() {
            Some(j) => j,
            None => match choose(&mut c, &tests, first, countdown.take(), code) {
                Pick::Job(j) => j,
                Pick::Exit(n) => {
                    c.out.close();
                    return Ok(n);
                }
            },
        };
        first = false;
        countdown = None;
        code = run_job(&mut c, &j)?;
        if let Some(n) = start_update(&mut c, Some(&j)) {
            c.out.close();
            return Ok(n);
        }
        if !c.out.full_screen() || args.exit_when_done {
            c.out.close();
            return Ok(code);
        }
    }
}

enum Pick {
    Job(Job),
    Exit(u8),
}

/// Swaps in a downloaded and checked update; the result screen's row for it.
fn install_update(c: &mut Ctx) -> Option<(String, String)> {
    let u = c.update.clone()?;
    let (line, row) = match u.install()? {
        Ok(v) if u.opted() => (format!("Updated to {v}, starting it"), format!("{v}, starting it")),
        Ok(v) => (format!("Updated to {v}, used at the next boot"), format!("{v}, used at the next boot")),
        Err(e) => (format!("Update failed: {e}"), format!("failed: {e}")),
    };
    c.out.line(&line);
    c.out.flush_log();
    c.out.view.notice = line;
    Some(("Update".into(), row))
}

/// Opted in and installed: hands the new version to init; the exit code that starts it.
fn start_update(c: &mut Ctx, job: Option<&Job>) -> Option<u8> {
    let u = c.update.clone().filter(|u| u.opted())?;
    install_update(c);
    let update::State::Installed { version, next } = u.state() else { return None };
    let stick = c.args.stick.clone()?;
    let code = match next {
        update::Next::Reexec { bin, sha256 } => {
            let mut extra = vec!["--updated-from".to_string(), env!("CARGO_PKG_VERSION").to_string()];
            if let Some(j) = job.filter(|j| !j.key.is_empty()) {
                extra.extend(["--job".to_string(), j.key.to_string()]);
            }
            if update::hand_over(&stick, &bin, &sha256, &extra) { EXIT_REEXEC } else { EXIT_RESTART }
        }
        update::Next::Reboot => EXIT_RESTART,
    };
    let how = if code == EXIT_REEXEC { "starting it now" } else { "restarting" };
    c.out.line(&format!("Update {version}: {how}"));
    c.out.flush_log();
    c.out.view.verdict = Verdict::Menu(None);
    c.out.view.notice = format!("Updated to {version}, {how}");
    c.out.view.current = Current::Busy(format!("starting memburn {version}"));
    c.out.view.keys.clear();
    c.out.redraw_now();
    Some(code)
}

/// The menu item for the update: (key, label, note).
fn update_item(u: &update::Updater) -> Option<(&'static str, String, String)> {
    let pct = |done: u64, total: u64| format!("downloading {} %", done * 100 / total.max(1));
    let key = if u.opted() { " " } else { "U" };
    let verb = if u.opted() { "Updating to" } else { "Update to" };
    Some(match u.state() {
        update::State::Staging { version, done, total } => (key, format!("{verb} {version}"), pct(done, total)),
        update::State::Ready(s) => (key, format!("{verb} {}", s.version), String::from("ready")),
        update::State::Installed { version, .. } => (key, format!("Start {version}"), String::from("installed")),
        update::State::NoSpace(v) => (" ", format!("Update {v}"), String::from("re-flash needed")),
        _ => return None,
    })
}

/// The menu's jobs, in key order: quick, full, burn, stress, cores, all.
fn menu_jobs(a: &Args, tests: &[TestId]) -> [Job; 6] {
    let quick = Job { key: "quick", ..Job::from_mode(Mode::Quick, a, tests) };
    let full = Job { name: "Full memory test".into(), key: "full", ..Job::from_mode(Mode::Mem, a, &memtest::DEFAULT) };
    let burn = Job { burn: Some(minutes(30.0)), key: "burn", ..Job::from_mode(Mode::Burn, a, tests) };
    let stress =
        Job { stress: Some(minutes(30.0)), key: "stress", ..Job::from_mode(Mode::Stress, a, &memtest::STRESS) };
    let cores = Job {
        cycles: 0,
        cores: Some(minutes(cycler::DEFAULT_MINUTES)),
        key: "cores",
        ..Job::from_mode(Mode::Cores, a, tests)
    };
    let all = Job {
        name: "All, repeating".into(),
        key: "all",
        cycles: 0,
        burn: Some(minutes(10.0)),
        ..Job::from_mode(Mode::Both, a, &memtest::DEFAULT)
    };
    [quick, full, burn, stress, cores, all]
}

fn est(secs: f64) -> String {
    format!("~{} min", (secs / 60.0).ceil().max(1.0) as u64)
}

/// Menu (with a countdown to the quick check, if any) or, after a run, the result screen with the same choices.
fn choose(c: &mut Ctx, tests: &[TestId], menu: bool, countdown: Option<u32>, last: u8) -> Pick {
    let gib = c.mem_bytes as f64 / (1u64 << 30) as f64;
    let a = c.args;
    let [quick, full, burn, stress, cores, all] = menu_jobs(a, tests);
    let sweep_secs = |j: &Job| eta::pass_ms(&j.tests, gib) as f64 / 1000.0;
    let quick_secs = sweep_secs(&quick) + quick.burn.map_or(0.0, |d| d.as_secs_f64());
    let full_secs = sweep_secs(&full);
    let mut keys: Vec<(&'static str, &'static str)> =
        vec![("Enter", "quick"), ("F", "full memory"), ("B", "burn-in"), ("S", "stress"), ("C", "cores"), ("A", "all")];
    if countdown.is_some() {
        keys.clear();
    }
    if a.boot {
        keys.extend([("R", "restart"), ("P", "power off")]);
    } else {
        keys.push(("X", "exit"));
    }
    let v = &mut c.out.view;
    v.keys = keys;
    v.qr_zoom = false;
    if menu {
        v.verdict = Verdict::Menu(countdown);
        v.job = quick.name.clone();
        v.plan = quick.plan(&c.cpus);
        v.current = Current::Menu(vec![
            ("Enter", "Quick check".into(), est(quick_secs)),
            ("F", "Full memory test".into(), est(full_secs)),
            ("B", "Burn-in".into(), "30 min".into()),
            ("S", "Stress: memory + FFT".into(), "30 min".into()),
            ("C", "Core cycler".into(), format!("{} min per CPU", cycler::DEFAULT_MINUTES)),
            ("A", "All, repeating".into(), "until Esc".into()),
        ]);
    }
    let t0 = Instant::now();
    loop {
        if let Some(u) = c.update.clone() {
            if let Some(n) = start_update(c, None) {
                return Pick::Exit(n);
            }
            if !menu
                && let Some(row) = install_update(c)
                && let Current::Summary(rows) = &mut c.out.view.current
            {
                rows.push(row);
            }
            if menu && let Current::Menu(items) = &mut c.out.view.current {
                items.truncate(6);
                items.extend(update_item(&u));
            }
        }
        if let Some(n) = countdown {
            let left = n.saturating_sub(t0.elapsed().as_secs() as u32);
            c.out.view.verdict = Verdict::Menu(Some(left));
            if left == 0 {
                return Pick::Job(quick);
            }
        }
        match c.out.wait_key(Duration::from_millis(200)) {
            Some(Key::Enter) => return Pick::Job(quick),
            Some(Key::Char('F')) => return Pick::Job(full),
            Some(Key::Char('B')) => return Pick::Job(burn),
            Some(Key::Char('S')) => return Pick::Job(stress),
            Some(Key::Char('C')) => return Pick::Job(cores),
            Some(Key::Char('A')) => return Pick::Job(all),
            Some(Key::Char('R')) if a.boot => return Pick::Exit(EXIT_RESTART),
            Some(Key::Char('P')) if a.boot => return Pick::Exit(EXIT_POWER_OFF),
            Some(Key::Char('X') | Key::Esc | Key::CtrlC) if !a.boot => return Pick::Exit(last),
            _ => {}
        }
    }
}

fn run_job(c: &mut Ctx, job: &Job) -> Result<u8, String> {
    c.stop.store(false, Ordering::Relaxed);
    c.log = ErrorLog::new();
    c.burn = BurnStats::default();
    c.out.reset();
    append_history(c, &history::start_line(&history::date(unix_now()), &job.name));
    if let Some(secs) = c.crash_after.take() {
        crash_kernel_after(secs);
    }
    c.covered = Ranges::default();
    (c.rate, c.test_left, c.job_left) = (Rate::default(), Shown::default(), Shown::default());
    let v = &mut c.out.view;
    v.notice.clear();
    v.job = job.name.clone();
    v.plan = job.plan(&c.cpus);
    if let Some(e) = c.early.as_ref().filter(|_| !job.tests.is_empty()) {
        let errors = e.bad.len() as u64;
        v.plan.insert(
            job.tests.len(),
            Step {
                name: format!("{EARLY_ROW} {} patterns", e.patterns),
                note: String::new(),
                state: StepState::Done { ms: e.ms, errors },
            },
        );
        for &r in &e.bad {
            v.errors.push(ErrLine {
                at_ms: 0,
                kind: ErrKind::Mem,
                text: format!("early: bad memory {}, reserved by the kernel", coverage::fmt_range(r)),
                mem: None,
            });
        }
        v.mem_errors = errors;
        c.out.extra_mem = errors;
    }
    v.verdict = Verdict::Running;
    v.current = Current::Idle;
    v.qr_zoom = false;
    v.keys = vec![("Esc", "stop")];
    if !c.args.no_upload {
        v.keys.push(("Q", "QR code"));
    }
    let gib = fmt_bytes(c.test_bytes(job) as u64);
    let what = match (job.tests.is_empty(), job.burn, job.stress) {
        _ if job.cores.is_some() => {
            format!("{} FFT load on each of {} CPUs in turn", fmt_dur(job.cores.unwrap_or_default()), c.cpus.len())
        }
        (_, _, Some(d)) => format!("{gib} memory test and FFT burn-in together for {}", fmt_dur(d)),
        (false, Some(d), _) => {
            format!("memory test of {gib} ({} tests), then {} FFT burn-in", job.tests.len(), fmt_dur(d))
        }
        (false, None, _) => format!("memory test of {gib} ({} tests x {} pass)", job.tests.len(), job.passes),
        (true, Some(d), _) => format!("{} FFT burn-in", fmt_dur(d)),
        (true, None, _) => "nothing".into(),
    };
    let repeat = match job.cycles {
        0 => ", repeating until stopped".to_string(),
        1 => String::new(),
        n => format!(", {n} times"),
    };
    c.out.line(&format!("{}: {what}{repeat}. Errors are reported as they happen; Ctrl+C stops.", job.name));
    c.out.flush_log();
    c.out.redraw_now();

    let mut cycle = 0;
    while (job.cycles == 0 || cycle < job.cycles) && !c.stop.load(Ordering::Relaxed) {
        if job.cycles != 1 {
            c.out.line(&format!("-- cycle {}", cycle + 1));
            c.out.view.job = format!("{}  ·  cycle {}", job.name, cycle + 1);
            for s in c.out.view.plan.iter_mut().filter(|s| !s.name.starts_with(EARLY_ROW)) {
                s.state = StepState::Pending;
            }
        }
        if !job.tests.is_empty() {
            run_mem(c, job, cycle)?;
        }
        if let Some(per) = job.cores {
            run_cores(c, per, cycle);
        }
        if let Some(d) = job.burn
            && !c.stop.load(Ordering::Relaxed)
        {
            let early_row = c.early.is_some() && !job.tests.is_empty();
            run_burn(c, d, job.tests.len() + usize::from(early_row));
        }
        cycle += 1;
    }
    Ok(summary(c, job))
}

fn summary(c: &mut Ctx, job: &Job) -> u8 {
    c.out.report(&c.log, &c.burn);
    let mem_errors = c.log.count() + c.out.extra_mem;
    let burn_errors = c.burn.errors.count();
    let stopped = c.stop.load(Ordering::Relaxed);
    let has_mem = !job.tests.is_empty();
    let has_burn = job.burn.is_some() || job.stress.is_some() || job.cores.is_some();
    let (verdict, tone, code) = if mem_errors + burn_errors > 0 {
        (Verdict::Fail, Tone::Bad, 1)
    } else if stopped {
        (Verdict::Stopped, Tone::Warn, 3)
    } else {
        (Verdict::Pass, Tone::Good, 0)
    };
    let word = match verdict {
        Verdict::Fail => "FAIL",
        Verdict::Stopped => "STOPPED EARLY, no errors so far",
        _ => "PASS",
    };
    let ffts = c.burn.iters.load(Ordering::Relaxed);
    let worst = f64::from_bits(c.burn.max_err.load(Ordering::Relaxed));
    let total = fmt_dur(c.out.elapsed());
    c.out.line("");
    c.out.styled(tone, &format!("Result: {word}"));
    let mut rows = Vec::new();
    if has_mem {
        let bits = if mem_errors > 0 { format!(" in {}", BitList(c.log.bits())) } else { String::new() };
        c.out.line(&format!("  Memory:  {mem_errors} error{}{bits}", errlog::plural(mem_errors)));
        rows.push((
            "Memory tested".into(),
            format!("{}, {} tests", fmt_bytes(c.test_bytes(job) as u64), job.tests.len()),
        ));
        if mem_errors > 0 {
            rows.push(("Memory errors".into(), format!("{}{bits}", tui::group(mem_errors))));
        }
    }
    if has_burn {
        c.out.line(&format!(
            "  Burn-in: {burn_errors} error{} in {ffts} FFTs, worst rounding {worst:.4} (limit {MAX_ERR})",
            errlog::plural(burn_errors)
        ));
        rows.push(("FFTs checked".into(), tui::group(ffts)));
        rows.push(("Worst rounding".into(), format!("{worst:.4} (limit {MAX_ERR})")));
    }
    if job.cores.is_some() {
        let value = tui::cores_summary(&c.out.view.plan);
        c.out.line(&format!("  Cores: {value}"));
        rows.push(("Cores".into(), value));
    }
    let date = history::date(unix_now());
    let footprint = if has_mem { c.footprint() } else { None };
    if let (Some(f), Some(ram)) = (&footprint, &c.ram) {
        let pct = coverage::fmt_percent(coverage::percent(&f.tested, ram));
        let (union, runs) = c.out.history.as_ref().map_or((f.tested.clone(), 1), |h| h.union_with(f, COVERAGE_RUNS));
        let over = coverage::fmt_percent(coverage::percent(&union, ram));
        let value = if runs > 1 { format!("{pct} (over {runs} runs {over})") } else { pct };
        c.out.line(&format!("  Coverage: {value} of {} System RAM", fmt_bytes(ram.bytes())));
        let gaps: Vec<String> = ram.minus(&f.tested).largest(3).into_iter().map(coverage::fmt_range).collect();
        if !gaps.is_empty() {
            c.out.line(&format!("  Largest untested: {}", gaps.join(", ")));
        }
        rows.push(("Coverage".into(), value));
    }
    if has_mem && let Some(h) = &mut c.out.history {
        if h.runs + h.unfinished > 0 {
            rows.push(("Earlier runs".into(), h.summary()));
        }
        h.add(c.log.bits());
        if let Some(f) = &footprint {
            h.footprints.push(f.clone());
        }
        append_history(c, &history::line(&date, mem_errors, burn_errors, c.log.bits(), footprint.as_ref()));
    } else {
        append_history(c, &history::end_line(&date, burn_errors));
    }
    if mem_errors > 0 {
        c.out.line(&format!("  Find the stick: {}", tui::locate(c.log.bits())));
    }
    let temps: Vec<(&str, i32)> = [("CPU", c.out.peak_temp), ("RAM", c.out.peak_ram_temp)]
        .into_iter()
        .filter_map(|(k, t)| Some((k, t?)))
        .collect();
    for (k, t) in &temps {
        rows.push((format!("Peak {k} temp"), format!("{t} °C")));
    }
    if !temps.is_empty() {
        let parts: Vec<String> = temps.iter().map(|(k, t)| format!("{k} {t} °C")).collect();
        c.out.line(&format!("  Peak temperature: {}", parts.join(", ")));
    }
    rows.push(("Total time".into(), total.clone()));
    rows.extend(install_update(c));
    rows.extend(tui::module_rows(&c.out.view.dimms, c.log.bits()));
    let advice = if mem_errors + burn_errors > 0 || stopped {
        errlog::advice(mem_errors, burn_errors).to_string()
    } else {
        let mut parts = Vec::new();
        if has_mem {
            parts.push(format!("{} memory", fmt_bytes(c.test_bytes(job) as u64)));
        }
        if has_burn {
            parts.push(format!("{} FFTs", tui::group(ffts)));
        }
        format!("{} checked in {total}", parts.join(" and "))
    };
    c.out.line(&format!("  {advice}"));
    c.out.flush_log();
    let v = &mut c.out.view;
    v.verdict = verdict;
    v.current = Current::Summary(rows);
    v.advice = advice;
    c.out.finish();
    c.out.redraw_now();
    code
}

fn run_mem(c: &mut Ctx, job: &Job, cycle: u64) -> Result<(), String> {
    let a = c.args;
    let stress = job.stress;
    let tests = &job.tests;
    let region = loop {
        let bytes = c.test_bytes(job);
        c.out.view.current = Current::Busy(format!("Allocating {}", fmt_bytes(bytes as u64)));
        c.out.redraw_now();
        let region = os::alloc(bytes)?;
        if region.locked || c.mem_bytes < c.mem_floor + LOCK_STEP || !os::can_lock(bytes) {
            break region;
        }
        drop(region);
        c.mem_bytes -= LOCK_STEP;
        c.out.line(&format!(
            "Could not lock {}; retrying with {}",
            fmt_bytes(bytes as u64),
            fmt_bytes(c.test_bytes(job) as u64)
        ));
    };
    let bytes = region.len;
    c.out.line(&format!(
        "Test memory: {}, {}",
        fmt_bytes(bytes as u64),
        match region.huge {
            Some(0) => String::from("no huge pages"),
            Some(h) => format!("{} in huge pages", fmt_bytes(h)),
            None => String::from("huge pages unknown"),
        }
    ));
    c.out.base = region.ptr as usize;
    if let Some(r) = os::phys_ranges(region.ptr, region.len) {
        c.covered = c.covered.union(&r);
    }
    if !region.locked {
        c.out.styled(
            Tone::Warn,
            "Warning: test memory is not locked and may be paged out, which weakens the test. \
             Run as administrator (Linux: raise ulimit -l).",
        );
        if !c.out.view.system.iter().any(|s| s == "memory not locked") {
            c.out.view.system.push("memory not locked".into());
        }
    }
    let (mem_threads, burn_threads) =
        if stress.is_some() { ((c.threads / 2).max(1), c.threads - c.threads / 2) } else { (c.threads, 0) };
    let chunks = chunk_regions(&[(region.ptr as usize, region.len)], chunk_bytes(bytes as u64, mem_threads));
    let tested: u64 = chunks.iter().map(|ch| ch.words as u64 * 8).sum();
    let per_pass: u64 = memtest::sweeps(tests) * tested;
    let pass_traffic = eta::pass_traffic(tests, tested);
    let pass_holds: u64 = tests.iter().map(|&(t, r)| eta::holds(t, r)).sum();
    let hold_ms = a.fade_secs * 1000;
    let burn_ms = job.burn.map_or(0, |d| d.as_millis() as u64);
    let later_cycles = if job.cycles == 0 { 0 } else { job.cycles - cycle - 1 };
    let mut held = 0u64;
    let moved = Moved::default();
    let start = Instant::now();
    let deadline = stress.map(|d| start + d);
    let over = |stop: &AtomicBool| stop.load(Ordering::Relaxed) || deadline.is_some_and(|d| Instant::now() >= d);
    let side_done = AtomicBool::new(false);
    if stress.is_some() {
        if a.inject {
            c.burn.inject.store(true, Ordering::Relaxed);
        }
        c.out.step(0, StepState::Running);
        c.out.line(&format!(
            "Stress: memory test on {mem_threads} threads and FFT burn-in on {burn_threads} threads, {}",
            fmt_dur(stress.unwrap_or_default())
        ));
    }

    std::thread::scope(|side| {
        if a.inject {
            let (base, words, done) = (region.ptr as usize, region.len / 8, &side_done);
            side.spawn(move || {
                let mut k = 1u64;
                while !done.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(400));
                    let w = (memburn_core::rng::splitmix64(k) as usize) % words;
                    unsafe { *(base as *mut u64).add(w) ^= 1 << (k % 64) };
                    k += 1;
                }
            });
        }
        for i in 0..burn_threads {
            let (halt, burn, isa, plans) = (&side_done, &c.burn, c.isa, &c.plans);
            side.spawn(move || Burner::new(isa, plans, i as u64).run(burn, halt));
        }
        let mut shown = Instant::now();
        c.out.rebase(0, c.burn.flops.load(Ordering::Relaxed));
        let mut p = 0;
        while if stress.is_some() { !over(c.stop) } else { p < job.passes && !c.stop.load(Ordering::Relaxed) } {
            let pass = cycle * job.passes + p;
            c.out.line(&format!("Memory test, pass {}: {}", p + 1, fmt_bytes(tested)));
            for (ti, &(t, rounds)) in tests.iter().enumerate() {
                let (t0, b0, e0) = (Instant::now(), moved.traffic(), c.log.count());
                let (test_traffic, test_holds, held0) =
                    (eta::test_traffic(t, rounds, tested), eta::holds(t, rounds), held);
                c.test_left.reset();
                if stress.is_none() {
                    c.out.step(ti, StepState::Running);
                }
                for (r, ph) in (0..rounds).flat_map(|r| (0..t.phases()).map(move |ph| (r, ph))) {
                    let job_ph =
                        PhaseJob::new(t, ph, memtest::round_pass(pass, r), &chunks, &moved, &c.log, c.stop, c.isa);
                    std::thread::scope(|s| {
                        let hs: Vec<_> = (0..mem_threads).map(|_| s.spawn(|| job_ph.work())).collect();
                        while !hs.iter().all(|h| h.is_finished()) {
                            std::thread::sleep(Duration::from_millis(2));
                            if c.out.tick().is_some() || shown.elapsed() < Duration::from_millis(250) {
                                continue;
                            }
                            shown = Instant::now();
                            c.out.report(&c.log, &c.burn);
                            let (d, m) = (moved.tested(), moved.traffic());
                            c.out.sample(Some(m), stress.map(|_| c.burn.flops.load(Ordering::Relaxed)));
                            let live = c.out.view.perf.mem.last().map_or(0.0, |&v| v as f64 * 1e6);
                            let gflops = c.out.view.perf.gflops.last().map_or(0.0, |&v| v as f64 / 10.0);
                            let in_pass = d - (p * per_pass).min(d);
                            let now_ms = c.out.elapsed().as_millis() as u64;
                            c.rate.sample(now_ms, m);
                            let (test_ms, job_ms) = match (deadline, stress) {
                                (Some(dl), Some(per)) => {
                                    let left = dl.saturating_duration_since(shown).as_millis() as u64;
                                    (Some(left), Some(left + later_cycles * per.as_millis() as u64))
                                }
                                _ => {
                                    let r = &c.rate;
                                    let test = r.ms(test_traffic.saturating_sub(m - b0));
                                    let test = test.map(|ms| ms + (test_holds - (held - held0)) * hold_ms);
                                    let all = job.passes * pass_traffic;
                                    let cycle_ms = r.ms(all.saturating_sub(m));
                                    let holds_left = job.passes * pass_holds - held;
                                    let later = r
                                        .ms(all)
                                        .map(|ms| later_cycles * (ms + job.passes * pass_holds * hold_ms + burn_ms));
                                    let job_ms =
                                        cycle_ms.zip(later).map(|(ms, l)| ms + holds_left * hold_ms + burn_ms + l);
                                    (c.test_left.update(now_ms, test), c.job_left.update(now_ms, job_ms))
                                }
                            };
                            c.out.view.current = Current::Mem {
                                test: t,
                                pass: p as u32 + 1,
                                passes: if stress.is_some() { p as u32 + 1 } else { job.passes as u32 },
                                index: ti as u32 + 1,
                                count: tests.len() as u32,
                                step: r * t.phases() as u32 + ph as u32 + 1,
                                steps: rounds * t.phases() as u32,
                                done: (in_pass.min(per_pass) as u128 * tested as u128 / per_pass.max(1) as u128) as u64,
                                total: tested,
                                left_ms: test_ms,
                                job_left_ms: job_ms,
                                job_label: if job.cycles == 0 { "cycle" } else { "job" },
                            };
                            let burn_part =
                                if stress.is_some() { format!(" | {gflops:.1} GFLOP/s") } else { String::new() };
                            c.out.status(
                                c.log.count() + c.burn.errors.count() > 0,
                                &format!(
                                    "[mem] test {}/{} {} | phase {}/{} | pass {:.0}%, {} | live {} (test avg {}){burn_part} | errors {}",
                                    ti + 1,
                                    tests.len(),
                                    memtest::label(t, rounds),
                                    r as usize * t.phases() + ph + 1,
                                    rounds as usize * t.phases(),
                                    in_pass as f64 * 100.0 / per_pass as f64,
                                    tui::time_left(test_ms, job_ms, if job.cycles == 0 { "cycle" } else { "job" }),
                                    fmt_rate(live),
                                    fmt_rate((m - b0) as f64 / t0.elapsed().as_secs_f64().max(1e-3)),
                                    c.log.count() + c.burn.errors.count()
                                ),
                            );
                        }
                    });
                    if over(c.stop) {
                        break;
                    }
                    if t.pause_after(ph) {
                        let until = Instant::now() + Duration::from_secs(a.fade_secs);
                        while Instant::now() < until && !over(c.stop) {
                            let left = until.saturating_duration_since(Instant::now());
                            c.out.view.current = Current::Hold { left_ms: left.as_millis() as u64 };
                            c.out.status(
                                false,
                                &format!("[mem] {}: letting memory sit idle, {} left", t.name(), fmt_dur(left)),
                            );
                            c.out.report(&c.log, &c.burn);
                            c.out.wait_key(Duration::from_millis(250));
                        }
                        held += 1;
                        c.rate.pause();
                    }
                }
                c.out.report(&c.log, &c.burn);
                let secs = t0.elapsed().as_secs_f64();
                let errors = c.log.count() - e0;
                if stress.is_none() {
                    c.out.step(ti, StepState::Done { ms: (secs * 1000.0) as u64, errors });
                }
                let line = format!(
                    "  {:<32} {:>8.1} s  {:>10}  {}",
                    memtest::label(t, rounds),
                    secs,
                    fmt_rate((moved.traffic() - b0) as f64 / secs.max(1e-3)),
                    errors_word(errors)
                );
                c.out.styled(if errors == 0 { Tone::Plain } else { Tone::Bad }, &line);
                c.out.flush_log();
                if over(c.stop) {
                    break;
                }
            }
            p += 1;
        }
        side_done.store(true, Ordering::Relaxed);
    });
    if stress.is_some() {
        let errors = c.log.count() + c.burn.errors.count();
        c.out.step(0, StepState::Done { ms: start.elapsed().as_millis() as u64, errors });
    }
    c.out.report(&c.log, &c.burn);
    Ok(())
}

fn run_burn(c: &mut Ctx, dur: Duration, step: usize) {
    let sizes: Vec<String> = c.plans.iter().map(|p| p.log2n.to_string()).collect();
    let span = match (sizes.first(), sizes.last()) {
        (Some(a), Some(b)) if sizes.len() > 1 => format!("{{{a}..{b}}}"),
        (Some(a), _) => a.clone(),
        _ => String::new(),
    };
    c.out.line(&format!(
        "Burn-in: {} on {} threads, {}, FFT sizes 2^{{{}}}; every result is checked exactly",
        fmt_dur(dur),
        c.threads,
        c.isa.name(),
        sizes.join(",")
    ));
    c.out.step(step, StepState::Running);
    let halt = AtomicBool::new(false);
    let (f0, i0, e0) =
        (c.burn.flops.load(Ordering::Relaxed), c.burn.iters.load(Ordering::Relaxed), c.burn.errors.count());
    let t0 = Instant::now();
    if c.args.inject {
        c.burn.inject.store(true, Ordering::Relaxed);
    }
    std::thread::scope(|s| {
        for i in 0..c.threads {
            let (halt, burn, isa, plans) = (&halt, &c.burn, c.isa, &c.plans);
            s.spawn(move || Burner::new(isa, plans, i as u64).run(burn, halt));
        }
        c.out.rebase(0, f0);
        let (mut last_t, mut last_f) = (t0, f0);
        loop {
            c.out.wait_key(Duration::from_millis(250));
            c.out.report(&c.log, &c.burn);
            let now = Instant::now();
            let el = now.duration_since(t0);
            if el >= dur || c.stop.load(Ordering::Relaxed) {
                halt.store(true, Ordering::Relaxed);
                break;
            }
            if now.duration_since(last_t) < Duration::from_millis(500) {
                continue;
            }
            let f = c.burn.flops.load(Ordering::Relaxed);
            let gflops = (f - last_f) as f64 / now.duration_since(last_t).as_secs_f64() / 1e9;
            (last_t, last_f) = (now, f);
            let errors = c.burn.errors.count() - e0;
            let worst = c.burn.max_err.load(Ordering::Relaxed);
            let ffts = c.burn.iters.load(Ordering::Relaxed) - i0;
            c.out.sample(None, Some(f));
            c.out.view.current = Current::Burn {
                isa: c.isa.name(),
                threads: c.threads as u32,
                sizes: span.clone(),
                ffts,
                elapsed_ms: el.as_millis() as u64,
                total_ms: dur.as_millis() as u64,
                worst,
            };
            c.out.status(
                errors > 0,
                &format!(
                    "[burn] {} of {} | live {:.1} GFLOP/s (avg {:.1}) | {} FFTs | worst rounding {:.4} | errors {}",
                    fmt_dur(el),
                    fmt_dur(dur),
                    gflops,
                    (f - f0) as f64 / el.as_secs_f64().max(1e-3) / 1e9,
                    ffts,
                    f64::from_bits(worst),
                    errors
                ),
            );
        }
    });
    c.out.report(&c.log, &c.burn);
    let secs = t0.elapsed().as_secs_f64();
    let errors = c.burn.errors.count() - e0;
    c.out.step(step, StepState::Done { ms: (secs * 1000.0) as u64, errors });
    let line = format!(
        "  {:<32} {:>8.1} s  {:.1} GFLOP/s avg, {} FFTs  {}",
        "Burn-in",
        secs,
        (c.burn.flops.load(Ordering::Relaxed) - f0) as f64 / secs / 1e9,
        c.burn.iters.load(Ordering::Relaxed) - i0,
        errors_word(errors)
    );
    c.out.styled(if errors == 0 { Tone::Plain } else { Tone::Bad }, &line);
    c.out.flush_log();
}

fn cpu_label(n: usize) -> String {
    format!("CPU {n}")
}

/// One turn per logical CPU: the FFTs run on a thread pinned to it while this thread draws.
fn run_cores(c: &mut Ctx, per: Duration, cycle: u64) {
    let isas = cycler::isas(c.isa);
    let names: Vec<&str> = isas.iter().map(|i| i.name()).collect();
    let plans: Vec<Plan> = cycler::SIZES.iter().map(|&s| Plan::new(s)).collect();
    let cpus = c.cpus.clone();
    c.out.line(&format!(
        "Core cycler, cycle {}: {} per CPU on {} CPUs, {}, FFT sizes 2^{{10..13}}, {} s idle each minute",
        cycle + 1,
        fmt_dur(per),
        cpus.len(),
        names.join(" and "),
        cycler::IDLE_MS / 1000
    ));
    let per_ms = per.as_millis() as u64;
    for (idx, &cpu) in cpus.iter().enumerate() {
        if c.stop.load(Ordering::Relaxed) {
            break;
        }
        c.out.step(idx, StepState::Running);
        if c.args.inject && idx == 1 % cpus.len() {
            c.burn.inject.store(true, Ordering::Relaxed);
        }
        let (i0, e0, f0) =
            (c.burn.iters.load(Ordering::Relaxed), c.burn.errors.count(), c.burn.flops.load(Ordering::Relaxed));
        let state = AtomicU32::new(0);
        let done = AtomicBool::new(false);
        let pinned = AtomicBool::new(false);
        let t0 = Instant::now();
        std::thread::scope(|s| {
            let (burn, stop, plans, isas, state, done, pinned) =
                (&c.burn, c.stop, &plans, &isas, &state, &done, &pinned);
            s.spawn(move || {
                pinned.store(os::pin_current(cpu), Ordering::Relaxed);
                let mut b = cycler::burners(isas, plans, cpu);
                let now = || t0.elapsed().as_millis() as u64;
                let mut after = |idle: bool| {
                    if idle {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                };
                cycler::run_turn(&mut b, burn, stop, per_ms, &now, &mut after, state);
                done.store(true, Ordering::Relaxed);
            });
            c.out.rebase(0, f0);
            while !done.load(Ordering::Relaxed) {
                c.out.wait_key(Duration::from_millis(250));
                c.out.report(&c.log, &c.burn);
                let isa = match state.load(Ordering::Relaxed) {
                    0 => None,
                    k => names.get(k as usize - 1).copied(),
                };
                let ffts = c.burn.iters.load(Ordering::Relaxed) - i0;
                let el = t0.elapsed();
                c.out.sample(None, Some(c.burn.flops.load(Ordering::Relaxed)));
                c.out.view.current = Current::Core {
                    label: cpu_label(cpu),
                    index: idx as u32 + 1,
                    count: cpus.len() as u32,
                    cycle: cycle as u32 + 1,
                    isa,
                    ffts,
                    elapsed_ms: el.as_millis() as u64,
                    total_ms: per_ms,
                };
                c.out.status(
                    c.burn.errors.count() > e0,
                    &format!(
                        "[cores] CPU {cpu} ({}/{}) | {} | {} of {} | {} FFTs | errors {}",
                        idx + 1,
                        cpus.len(),
                        isa.unwrap_or("idle"),
                        fmt_dur(el),
                        fmt_dur(per),
                        ffts,
                        c.burn.errors.count() - e0
                    ),
                );
            }
        });
        c.out.report(&c.log, &c.burn);
        let mut r = cycler::CoreResult::new(cpu, cpu_label(cpu));
        r.ms = t0.elapsed().as_millis() as u64;
        r.ffts = c.burn.iters.load(Ordering::Relaxed) - i0;
        r.errors = c.burn.errors.count() - e0;
        r.fault = (r.errors > 0).then(|| last_fault(&c.burn)).flatten();
        if let Some(s) = c.out.view.plan.get_mut(idx) {
            s.name = r.row();
        }
        c.out.step(idx, StepState::Done { ms: r.ms, errors: r.errors });
        let pin = if pinned.load(Ordering::Relaxed) { "" } else { " (not pinned)" };
        let line = format!(
            "  {:<32} {:>8.1} s  {} FFTs  {}{pin}",
            r.row(),
            r.ms as f64 / 1000.0,
            r.ffts,
            errors_word(r.errors)
        );
        c.out.styled(if r.errors == 0 { Tone::Plain } else { Tone::Bad }, &line);
        c.out.flush_log();
    }
}

/// Fault of the newest recorded CPU error.
fn last_fault(burn: &BurnStats) -> Option<Fault> {
    let n = burn.errors.count();
    let zero = BurnError { worker: 0, log2n: 0, seed: 0, fault: Fault::WrongResult, max_err: 0 };
    let mut e = [zero];
    let (got, _) = burn.errors.since(n.checked_sub(1)?, &mut e);
    (got == 1).then_some(e[0].fault)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Appends a line to the history file and syncs it, so a crash right after still leaves it on the disk.
fn append_history(c: &mut Ctx, line: &str) {
    let Some(p) = &c.history_path else { return };
    let _ = p.parent().map(std::fs::create_dir_all);
    let w = std::fs::OpenOptions::new().create(true).append(true).open(p).and_then(|mut f| {
        use std::io::Write;
        writeln!(f, "{line}")?;
        f.sync_data()
    });
    if let Err(e) = w {
        c.out.styled(Tone::Warn, &format!("Could not update {}: {e}", p.display()));
    }
}

/// Test hook for the stick's crash logs: a sysrq crash `secs` from now.
fn crash_kernel_after(secs: u64) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(secs));
        let _ = std::fs::write("/proc/sys/kernel/sysrq", "1");
        let _ = std::fs::write("/proc/sysrq-trigger", "c");
    });
}

/// Next to the log when there is one (the stick), else the per-user state directory.
fn history_path(a: &Args) -> Option<std::path::PathBuf> {
    if let Some(log) = &a.log {
        return Some(log.with_file_name(history::FILE));
    }
    let dir = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from)
    } else {
        std::env::var_os("XDG_STATE_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".local/state")))
    };
    dir.map(|d| d.join("memburn").join(history::FILE))
}

fn parse_tests(s: Option<&str>) -> Result<Vec<TestId>, String> {
    match s {
        None => Ok(memtest::DEFAULT.to_vec()),
        Some("all") => Ok(memtest::ALL.to_vec()),
        Some(list) => {
            list.split(',').map(|k| TestId::from_key(k.trim()).ok_or_else(|| format!("unknown test '{k}'"))).collect()
        }
    }
}

fn parse_fft(s: &str) -> Result<Vec<u32>, String> {
    match s {
        "small" => Ok((10..=15).collect()),
        "large" => Ok((17..=21).collect()),
        "mixed" => Ok((10..=21).collect()),
        list => list
            .split(',')
            .map(|k| match k.trim().parse::<u32>() {
                Ok(v) if (MIN_LOG2N..=MAX_LOG2N).contains(&v) => Ok(v),
                _ => Err(format!("FFT size must be a log2 between {MIN_LOG2N} and {MAX_LOG2N}, got '{k}'")),
            })
            .collect(),
    }
}

fn parse_mem(s: &str) -> Result<usize, String> {
    mem_bytes(s, os::available())
}

/// `--mem` against `available` bytes, rounded down to 2 MiB.
fn mem_bytes(s: &str, available: u64) -> Result<usize, String> {
    let s = s.trim();
    let bytes = if let Some(pct) = s.strip_suffix('%') {
        let pct: f64 = pct.parse().map_err(|_| format!("bad percentage '{s}'"))?;
        (available as f64 * pct / 100.0) as u64
    } else if let Some(less) = s.strip_prefix('-') {
        available.saturating_sub(size(less).map_err(|_| format!("bad size '{s}'"))?)
    } else {
        size(s).map_err(|_| format!("bad size '{s}'"))?
    };
    let bytes = bytes / (2 << 20) * (2 << 20);
    if bytes == 0 {
        return Err(format!("memory size '{s}' is under 2 MiB"));
    }
    Ok(bytes as usize)
}

/// "512M", "8G", "4096K" or bytes.
fn size(s: &str) -> Result<u64, ()> {
    let (num, mul) = match s.chars().last().map(|c| c.to_ascii_uppercase()) {
        Some('K') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M') => (&s[..s.len() - 1], 1 << 20),
        Some('G') => (&s[..s.len() - 1], 1 << 30),
        _ => (s, 1),
    };
    let v: f64 = num.parse().map_err(|_| ())?;
    if v < 0.0 { Err(()) } else { Ok((v * mul as f64) as u64) }
}

fn fmt_bytes(b: u64) -> String {
    if b >= 1 << 30 { format!("{:.1} GiB", b as f64 / (1u64 << 30) as f64) } else { format!("{} MiB", b >> 20) }
}

fn fmt_rate(bytes_per_s: f64) -> String {
    format!("{:.1} GB/s", bytes_per_s / 1e9)
}

fn fmt_dur(d: Duration) -> String {
    tui::clock(d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_forms() {
        let avail = 10u64 << 30;
        assert_eq!(mem_bytes("8G", avail), Ok(8 << 30));
        assert_eq!(mem_bytes("50%", avail), Ok(5 << 30));
        assert_eq!(mem_bytes("-512M", avail), Ok((10 << 30) - (512 << 20)));
        assert_eq!(mem_bytes(" -1G ", avail), Ok(9 << 30));
        assert_eq!(mem_bytes("-1025K", 4 << 20), Ok(2 << 20));
        assert!(mem_bytes("-12G", avail).is_err());
        assert!(mem_bytes("--1G", avail).is_err());
        assert!(mem_bytes("-x", avail).is_err());
        assert!(Args::try_parse_from(["memburn", "--mem", "-512M"]).is_ok_and(|a| a.mem == "-512M"));
    }

    #[test]
    fn stress_budget_leaves_room_for_ffts() {
        let mixed: Vec<u32> = (10..=21).collect();
        let plans = (16usize << 22) - (16 << 10);
        assert_eq!(mem_budget(8 << 30, &mixed, 0), ((8 << 30) - plans) / (2 << 20) * (2 << 20));
        assert_eq!(mem_budget(8 << 30, &mixed, 8), ((8 << 30) - plans - 8 * (32 << 20)) / (2 << 20) * (2 << 20));
        assert_eq!(mem_budget(64 << 20, &mixed, 8), 2 << 20);
        assert_eq!(mem_budget(64 << 20, &[], 8), 64 << 20);
    }
}
