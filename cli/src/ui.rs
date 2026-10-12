//! Output for a run: the full-screen dashboard on a terminal, plain lines otherwise; both feed the log and the uploader.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use memburn_core::errlog::{self, ErrorLog, MemError};
use memburn_core::fft::{BurnError, BurnStats, Fault};
use memburn_core::history::History;
use memburn_core::tui::{self, ErrLine, Frame, Places, Qr, Remote, StepState, Verdict, View};
use memburn_proto::Snapshot;

use crate::os;
use crate::term::{Key, Term};
use crate::upload::Uploader;

const DRAW_EVERY: Duration = Duration::from_millis(200);
const SAMPLE_EVERY: Duration = Duration::from_secs(1);
const PUSH_EVERY: Duration = Duration::from_secs(3);
const MAX_ERR_LINES: usize = 500;

#[derive(Clone, Copy)]
pub enum Tone {
    Plain,
    Good,
    Warn,
    Bad,
}

pub struct Ui {
    pub view: View,
    term: Option<Term>,
    ansi: bool,
    tty: bool,
    status_len: usize,
    beat: usize,
    width: usize,
    pub log: Option<std::fs::File>,
    /// Start of the test buffer, to show where in it an error landed.
    pub base: usize,
    next_mem: u64,
    next_burn: u64,
    places: Places,
    /// Earlier runs, from the history file.
    pub history: Option<History>,
    t0: Instant,
    drawn: Option<Instant>,
    sampled: Option<Instant>,
    /// Counters at the previous chart sample: (time, memory traffic bytes, flops).
    prev: (Instant, u64, u64),
    /// Print every chart sample.
    pub trace: bool,
    pushed: Option<Instant>,
    temp_read: Option<Instant>,
    pub peak_temp: Option<i32>,
    pub peak_ram_temp: Option<i32>,
    /// Memory errors found outside the error log: the kernel's early test.
    pub extra_mem: u64,
    up: Option<Uploader>,
    server: Option<String>,
    /// The final snapshot of this job is queued; nothing more is pushed.
    finished: bool,
    url_shown: bool,
    stop: &'static AtomicBool,
}

impl Ui {
    pub fn new(full_screen: bool, view: View, stop: &'static AtomicBool) -> Ui {
        let term = if full_screen { Term::open().ok() } else { None };
        Ui {
            view,
            term,
            ansi: os::enable_ansi(),
            tty: std::io::stdout().is_terminal(),
            status_len: 0,
            beat: 0,
            width: os::term_width(),
            log: None,
            base: 0,
            next_mem: 0,
            next_burn: 0,
            places: Places::default(),
            history: None,
            t0: Instant::now(),
            drawn: None,
            sampled: None,
            prev: (Instant::now(), 0, 0),
            trace: false,
            pushed: None,
            temp_read: None,
            peak_temp: None,
            peak_ram_temp: None,
            extra_mem: 0,
            up: None,
            server: None,
            finished: false,
            url_shown: false,
            stop,
        }
    }

    pub fn full_screen(&self) -> bool {
        self.term.is_some()
    }

    pub fn upload(&mut self, server: String) {
        self.up = Some(Uploader::start(server.clone()));
        self.server = Some(server);
    }

    /// Starts a new job: clears errors, timers and performance history.
    pub fn reset(&mut self) {
        if self.finished {
            self.finished = false;
            self.url_shown = false;
            self.view.remote = None;
            self.up = self.server.clone().map(Uploader::start);
        }
        self.t0 = Instant::now();
        self.next_mem = 0;
        self.next_burn = 0;
        self.places = Places::default();
        self.peak_temp = None;
        self.peak_ram_temp = None;
        self.extra_mem = 0;
        let v = &mut self.view;
        v.elapsed_ms = 0;
        v.perf = tui::Perf { temp_c: v.perf.temp_c, ram_temp_c: v.perf.ram_temp_c, ..tui::Perf::default() };
        v.mem_errors = 0;
        v.cpu_errors = 0;
        v.bits.clear();
        v.errors.clear();
        v.advice.clear();
    }

    pub fn elapsed(&self) -> Duration {
        self.t0.elapsed()
    }

    fn paint(&self, tone: Tone, s: &str) -> String {
        let code = match tone {
            Tone::Plain => return s.to_string(),
            Tone::Good => "32;1",
            Tone::Warn => "33;1",
            Tone::Bad => "31;1",
        };
        if self.ansi { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }

    /// Plain mode only: the `\r`-rewritten progress line.
    pub fn status(&mut self, alarm: bool, s: &str) {
        if self.term.is_some() || !self.tty {
            return;
        }
        self.beat += 1;
        let mut s = format!("{} {s}", ['|', '/', '-', '\\'][self.beat % 4]);
        if self.width > 1 {
            s = s.chars().take(self.width - 1).collect();
        }
        let len = s.chars().count();
        let pad = self.status_len.saturating_sub(len);
        let shown = self.paint(if alarm { Tone::Bad } else { Tone::Plain }, &s);
        print!("\r{shown}{:pad$}", "");
        self.status_len = len;
        let _ = std::io::stdout().flush();
    }

    pub fn line(&mut self, s: &str) {
        self.styled(Tone::Plain, s);
    }

    pub fn styled(&mut self, tone: Tone, s: &str) {
        if self.term.is_none() {
            if self.status_len > 0 {
                print!("\r{:w$}\r", "", w = self.status_len);
                self.status_len = 0;
            }
            println!("{}", self.paint(tone, s));
        }
        if let Some(f) = &mut self.log {
            let _ = writeln!(f, "{s}");
        }
    }

    /// Forces logged lines onto the disk, so they survive a crash or power cut.
    pub fn flush_log(&mut self) {
        if let Some(f) = &self.log {
            let _ = f.sync_data();
        }
    }

    /// Shows errors found since the last call.
    pub fn report(&mut self, log: &ErrorLog, burn: &BurnStats) {
        let errors_before = self.next_mem + self.next_burn;
        let at_ms = self.t0.elapsed().as_millis() as u64;
        let mut mem = [MemError::default(); 16];
        while self.next_mem < log.count() {
            if self.next_mem == 0 {
                self.styled(Tone::Bad, &format!("!! {}", errlog::MEM_BANNER));
                self.styled(Tone::Bad, &format!("!! {}", errlog::CONTINUING));
            }
            let (n, next) = log.since(self.next_mem, &mut mem);
            let skipped = next - self.next_mem - n as u64;
            if skipped > 0 {
                self.styled(Tone::Bad, &format!("  ... {skipped} more memory errors not shown"));
            }
            for e in &mem[..n] {
                let off = e.addr.wrapping_sub(self.base) as f64 / (1u64 << 30) as f64;
                let phys = os::phys(e.addr);
                let virt = if phys.is_some() { format!(" ({})", tui::at(e.addr, None)) } else { String::new() };
                self.styled(
                    Tone::Bad,
                    &format!(
                        "  MEMORY ERROR at {}{virt}: {e}, {off:.3} GiB into the test buffer",
                        tui::at(e.addr, phys)
                    ),
                );
                self.places.add(e);
                self.view.errors.push(ErrLine::mem(at_ms, e, phys));
            }
            self.next_mem = next;
        }
        let zero = BurnError { worker: 0, log2n: 0, seed: 0, fault: Fault::WrongResult, max_err: 0 };
        let mut cpu_errs = [zero; 8];
        while self.next_burn < burn.errors.count() {
            if self.next_burn == 0 {
                self.styled(Tone::Bad, &format!("!! {}", errlog::BURN_BANNER));
                self.styled(Tone::Bad, &format!("!! {}", errlog::CONTINUING));
            }
            let (n, next) = burn.errors.since(self.next_burn, &mut cpu_errs);
            let skipped = next - self.next_burn - n as u64;
            if skipped > 0 {
                self.styled(Tone::Bad, &format!("  ... {skipped} more CPU errors not shown"));
            }
            for e in &cpu_errs[..n] {
                self.styled(Tone::Bad, &format!("  CPU ERROR {e}"));
                self.view.errors.push(ErrLine::cpu(at_ms, e));
            }
            self.next_burn = next;
        }
        if errors_before != self.next_mem + self.next_burn {
            let extra = self.view.errors.len().saturating_sub(MAX_ERR_LINES);
            self.view.errors.drain(..extra);
            self.view.mem_errors = log.count() + self.extra_mem;
            self.view.cpu_errors = burn.errors.count();
            self.view.bits = tui::bit_stats(log, &self.places, self.history.as_ref());
            self.flush_log();
        }
    }

    pub fn step(&mut self, index: usize, state: StepState) {
        if let Some(s) = self.view.plan.get_mut(index) {
            s.state = state;
        }
    }

    /// Starts the next chart sample's window at these counter values: memory traffic bytes and flops.
    pub fn rebase(&mut self, traffic: u64, flops: u64) {
        let now = Instant::now();
        self.prev = (now, traffic, flops);
        self.sampled = Some(now);
    }

    /// Records chart samples at most once a second, each the rate over the whole time since the previous one.
    pub fn sample(&mut self, traffic: Option<u64>, flops: Option<u64>) {
        if !due(self.sampled, SAMPLE_EVERY) {
            return;
        }
        let now = Instant::now();
        self.sampled = Some(now);
        let (t, m0, f0) = self.prev;
        self.prev = (now, traffic.unwrap_or(m0), flops.unwrap_or(f0));
        let secs = now.duration_since(t).as_secs_f64().max(1e-3);
        let mem = traffic.map(|m| (m.saturating_sub(m0) as f64 / secs / 1e6) as u32);
        let gflops10 = flops.map(|f| (f.saturating_sub(f0) as f64 / secs / 1e8) as u32);
        self.view.perf.push(mem, gflops10);
        if self.trace {
            let opt = |v: Option<u32>| v.map_or(String::from("-"), |v| v.to_string());
            self.line(&format!("sample {:.3} s  mem {} MB/s  compute {} dGFLOP/s", secs, opt(mem), opt(gflops10)));
        }
    }

    /// Redraws, uploads and handles Esc/Ctrl+C/Q; call it often. Returns other keys.
    pub fn tick(&mut self) -> Option<Key> {
        if matches!(self.view.verdict, Verdict::Running | Verdict::Stopping) {
            self.view.elapsed_ms = self.t0.elapsed().as_millis() as u64;
        }
        if due(self.temp_read, Duration::from_secs(2)) {
            self.temp_read = Some(Instant::now());
            self.view.perf.temp_c = os::temp_c();
            self.view.perf.ram_temp_c = os::dimm_temps();
            self.peak_temp = self.peak_temp.max(self.view.perf.temp_c);
            self.peak_ram_temp = self.peak_ram_temp.max(self.view.perf.ram_temp_c);
        }
        self.remote();
        if self.view.verdict == Verdict::Running && self.stop.load(Ordering::Relaxed) {
            self.view.verdict = Verdict::Stopping;
        }
        if due(self.pushed, PUSH_EVERY) {
            self.push();
        }
        let mut other = None;
        if let Some(term) = &mut self.term {
            if due(self.drawn, DRAW_EVERY) {
                self.drawn = Some(Instant::now());
                let (w, h) = Term::size();
                let mut f = Frame::new(w, h);
                tui::render(&self.view, &mut f);
                term.draw(f);
            }
            while let Some(k) = term.key(Duration::ZERO) {
                match k {
                    Key::CtrlC if self.stop.load(Ordering::Relaxed) => {
                        self.term = None;
                        std::process::exit(130);
                    }
                    Key::Esc | Key::CtrlC if matches!(self.view.verdict, Verdict::Running | Verdict::Stopping) => {
                        self.stop.store(true, Ordering::Relaxed);
                        self.view.verdict = Verdict::Stopping;
                        self.drawn = None;
                    }
                    Key::Char('Q') if self.view.remote.as_ref().is_some_and(|r| r.qr.is_some()) => {
                        self.view.qr_zoom = !self.view.qr_zoom;
                        self.drawn = None;
                    }
                    k => other = Some(k),
                }
            }
        }
        other
    }

    /// Waits up to `wait` for a key, drawing meanwhile.
    pub fn wait_key(&mut self, wait: Duration) -> Option<Key> {
        let until = Instant::now() + wait;
        loop {
            if let Some(k) = self.tick() {
                return Some(k);
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            std::thread::sleep(left.min(Duration::from_millis(50)));
        }
    }

    pub fn redraw_now(&mut self) {
        self.drawn = None;
        self.pushed = None;
        self.tick();
    }

    fn push(&mut self) {
        self.pushed = Some(Instant::now());
        if let Some(up) = &self.up
            && !self.finished
        {
            up.push(Snapshot::from_view(&self.view));
        }
    }

    /// Queues the job's final snapshot, stamped with the finish time; uploads stop after it.
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        if let Some(up) = &self.up {
            let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            up.finish(Snapshot { finished: Some(unix), ..Snapshot::from_view(&self.view) });
        }
    }

    fn remote(&mut self) {
        let Some(up) = &self.up else { return };
        let st = up.state();
        let Some(url) = st.url else {
            self.view.remote = None;
            return;
        };
        let status = match (st.done, &st.error, st.last_ok) {
            (Some(true), ..) => String::from("uploaded"),
            (Some(false), Some(e), _) => format!("upload failed  ·  {e}"),
            (Some(false), None, _) => String::from("upload failed"),
            (None, Some(e), _) => format!("offline  ·  {e}"),
            (None, None, Some(t)) => format!("uploading  ·  {} s ago", t.elapsed().as_secs()),
            (None, None, None) => String::from("connecting"),
        };
        let ok = st.error.is_none() && st.done != Some(false);
        match &mut self.view.remote {
            Some(r) if r.url == url => {
                r.status = status;
                r.ok = ok;
            }
            r => *r = Some(Remote { qr: qr(&url), url: url.clone(), status, ok }),
        }
        if !self.url_shown {
            self.url_shown = true;
            self.line(&format!("Live view: {url}"));
        }
    }

    /// Sends the final snapshot unless sent, and restores the terminal.
    pub fn close(mut self) {
        self.finish();
        self.term = None;
        if let Some(up) = self.up.take() {
            up.wait(Duration::from_secs(10));
        }
    }
}

fn qr(url: &str) -> Option<Qr> {
    let q = qrcodegen::QrCode::encode_text(url, qrcodegen::QrCodeEcc::Medium).ok()?;
    let n = q.size();
    Some(Qr { size: n as usize, dark: (0..n * n).map(|k| q.get_module(k % n, k / n)).collect() })
}

pub fn errors_word(n: u64) -> String {
    match n {
        0 => "ok".to_string(),
        1 => "1 ERROR".to_string(),
        n => format!("{n} ERRORS"),
    }
}

fn due(last: Option<Instant>, every: Duration) -> bool {
    last.is_none_or(|t| t.elapsed() >= every)
}
