//! Background uploader: creates a run on the live-view server, PUTs the latest snapshot every few seconds, then one final snapshot.

use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use memburn_proto::{Created, Snapshot, Verdict};

const EVERY: Duration = Duration::from_secs(3);
/// Delay after a failed run creation: the first, doubling up to the second.
const RETRY: (Duration, Duration) = (Duration::from_secs(1), Duration::from_secs(15));
const ROUTE_POLL: Duration = Duration::from_millis(250);
/// Attempts at the final snapshot, this far apart.
const FINAL_TRIES: u32 = 4;
const FINAL_GAP: Duration = Duration::from_secs(2);
/// How long [`abort`] waits for the stopped snapshot; Windows ends a closed console's process about 5 s after the event.
pub const ABORT_WAIT: Duration = Duration::from_millis(2500);

/// The newest uploader's shared state, for [`abort`] from signal, console and panic handlers.
static ACTIVE: Mutex<Option<Arc<Mutex<Shared>>>> = Mutex::new(None);

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum Phase {
    #[default]
    Waiting,
    Connecting,
    Up,
}

#[derive(Clone, Default)]
pub struct State {
    pub phase: Phase,
    /// When a default route appeared, and how long creating the run took after it.
    pub route_at: Option<Instant>,
    pub created_after: Option<Duration>,
    pub url: Option<String>,
    pub ok: bool,
    pub last_ok: Option<Instant>,
    pub error: Option<String>,
    /// The final snapshot went up (true) or every attempt failed (false); nothing more is sent.
    pub done: Option<bool>,
}

#[derive(Default)]
struct Shared {
    snap: Option<Snapshot>,
    generation: u64,
    state: State,
    /// `snap` is the final snapshot.
    last: bool,
}

pub struct Uploader {
    shared: Arc<Mutex<Shared>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Uploader {
    pub fn start(server: String) -> Uploader {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let s = shared.clone();
        let thread = std::thread::spawn(move || run(&server, &s));
        if let Some(mut a) = lock_by(&ACTIVE, Instant::now() + Duration::from_secs(1)) {
            *a = Some(shared.clone());
        }
        Uploader { shared, thread: Some(thread) }
    }

    pub fn push(&self, snap: Snapshot) {
        self.queue(snap, false);
    }

    /// Queues the final snapshot; later pushes are ignored.
    pub fn finish(&self, snap: Snapshot) {
        self.queue(snap, true);
    }

    fn queue(&self, snap: Snapshot, last: bool) {
        let mut s = self.shared.lock().unwrap();
        if !s.last {
            s.snap = Some(snap);
            s.generation += 1;
            s.last = last;
        }
    }

    /// See [`abort`].
    #[cfg(test)]
    pub fn abort(&self, why: &str, limit: Duration) -> bool {
        abort_shared(&self.shared, why, limit)
    }

    pub fn state(&self) -> State {
        self.shared.lock().unwrap().state.clone()
    }

    /// Waits at most `limit` for the final snapshot to go up.
    pub fn wait(mut self, limit: Duration) {
        let t0 = Instant::now();
        while t0.elapsed() < limit && self.thread.as_ref().is_some_and(|t| !t.is_finished()) {
            std::thread::sleep(Duration::from_millis(50));
        }
        self.thread.take();
    }
}

/// The process is about to end: queues the latest snapshot as stopped with `why` as its advice, unless a final
/// snapshot is already queued, then waits at most `limit` for the final snapshot to go up. True when it did.
pub fn abort(why: &str, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    let Some(shared) = lock_by(&ACTIVE, deadline).and_then(|a| a.clone()) else { return false };
    abort_shared(&shared, why, deadline.saturating_duration_since(Instant::now()))
}

fn abort_shared(shared: &Mutex<Shared>, why: &str, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    {
        let Some(mut s) = lock_by(shared, deadline) else { return false };
        if s.state.done.is_some() {
            return s.state.done == Some(true);
        }
        if !s.last {
            let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            if let Some(snap) = s.snap.clone().and_then(|snap| stopped(snap, why, unix)) {
                s.snap = Some(snap);
                s.generation += 1;
            }
            s.last = true;
        }
    }
    while Instant::now() < deadline {
        if let Some(done) = lock_by(shared, deadline).and_then(|s| s.state.done) {
            return done;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// `snap` as the final snapshot of a run cut short at `unix` seconds; None when it is final already or no job runs.
fn stopped(snap: Snapshot, why: &str, unix: u64) -> Option<Snapshot> {
    if snap.is_finished() || !matches!(snap.verdict, Verdict::Running | Verdict::Stopping) {
        return None;
    }
    Some(Snapshot { verdict: Verdict::Stopped, finished: Some(unix), advice: why.to_string(), ..snap })
}

/// Locks `m` unless another holder keeps it past `deadline`; a poisoned lock is still used.
fn lock_by<T>(m: &Mutex<T>, deadline: Instant) -> Option<MutexGuard<'_, T>> {
    loop {
        match m.try_lock() {
            Ok(g) => return Some(g),
            Err(TryLockError::Poisoned(e)) => return Some(e.into_inner()),
            Err(TryLockError::WouldBlock) if Instant::now() >= deadline => return None,
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

fn run(server: &str, shared: &Mutex<Shared>) {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).build().into();
    let mut delay = RETRY.0;
    let created = loop {
        while !crate::update::has_route() {
            if pause(shared, ROUTE_POLL) {
                shared.lock().unwrap().state.done = Some(false);
                return;
            }
        }
        {
            let mut s = shared.lock().unwrap();
            s.state.phase = Phase::Connecting;
            s.state.route_at.get_or_insert_with(Instant::now);
        }
        match agent
            .post(format!("{server}/memburn/api/runs"))
            .send_empty()
            .and_then(|mut r| r.body_mut().read_json::<Created>())
        {
            Ok(c) => break c,
            Err(e) => {
                let mut s = shared.lock().unwrap();
                s.state.error = Some(short(&e));
                if s.last {
                    s.state.done = Some(false);
                    return;
                }
            }
        }
        if pause(shared, delay) {
            shared.lock().unwrap().state.done = Some(false);
            return;
        }
        delay = (delay * 2).min(RETRY.1);
    };
    {
        let mut s = shared.lock().unwrap();
        s.state.phase = Phase::Up;
        s.state.created_after = s.state.route_at.map(|t| t.elapsed());
        s.state.url = Some(created.url.clone());
        s.state.error = None;
    }
    let put = format!("{server}/memburn/api/runs/{}", created.id);
    let auth = format!("Bearer {}", created.token);
    let mut sent = 0;
    let mut tries = 0;
    loop {
        let (snap, generation, last) = {
            let s = shared.lock().unwrap();
            (if s.generation > sent { s.snap.clone() } else { None }, s.generation, s.last)
        };
        if let Some(snap) = snap {
            let r = agent.put(&put).header("Authorization", &auth).send_json(&snap);
            let mut s = shared.lock().unwrap();
            match r {
                Ok(_) => {
                    sent = generation;
                    s.state.ok = true;
                    s.state.last_ok = Some(Instant::now());
                    s.state.error = None;
                }
                Err(e) => {
                    s.state.ok = false;
                    s.state.error = Some(short(&e));
                }
            }
        }
        if last {
            let ok = sent >= generation;
            tries += 1;
            if ok || tries >= FINAL_TRIES {
                shared.lock().unwrap().state.done = Some(ok);
                return;
            }
            std::thread::sleep(FINAL_GAP);
        } else {
            let t0 = Instant::now();
            while t0.elapsed() < EVERY && !shared.lock().unwrap().last {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Sleeps `d` in short steps; true when the final snapshot was queued meanwhile.
fn pause(shared: &Mutex<Shared>, d: Duration) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < d {
        if shared.lock().unwrap().last {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    shared.lock().unwrap().last
}

fn short(e: &ureq::Error) -> String {
    let s = e.to_string();
    s.chars().take(60).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    #[test]
    fn only_a_running_job_becomes_stopped() {
        let snap = |verdict| Snapshot { verdict, advice: "old".into(), ..Snapshot::default() };
        for v in [Verdict::Running, Verdict::Stopping] {
            let s = stopped(snap(v), "window closed", 7).unwrap();
            assert_eq!((s.verdict, s.finished, s.advice.as_str()), (Verdict::Stopped, Some(7), "window closed"));
        }
        for v in [Verdict::Menu, Verdict::Pass, Verdict::Fail, Verdict::Stopped] {
            assert!(stopped(snap(v), "x", 7).is_none(), "{v:?}");
        }
        assert!(stopped(Snapshot { finished: Some(1), ..snap(Verdict::Running) }, "x", 7).is_none());
    }

    /// A live-view server that creates one run and sends each PUT body down the channel.
    fn server() -> (String, mpsc::Receiver<Snapshot>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for c in listener.incoming() {
                let Ok(c) = c else { return };
                let tx = tx.clone();
                std::thread::spawn(move || {
                    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                    let mut w = c.try_clone().unwrap();
                    let mut r = BufReader::new(c);
                    loop {
                        let mut line = String::new();
                        if r.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        let mut len = 0;
                        loop {
                            let mut h = String::new();
                            if r.read_line(&mut h).unwrap_or(0) == 0 {
                                return;
                            }
                            if h.trim().is_empty() {
                                break;
                            }
                            if let Some((k, v)) = h.split_once(':')
                                && k.eq_ignore_ascii_case("content-length")
                            {
                                len = v.trim().parse().unwrap();
                            }
                        }
                        let mut body = vec![0; len];
                        r.read_exact(&mut body).unwrap();
                        let reply = if line.starts_with("POST") {
                            String::from(r#"{"id":"AAAAAAAA","token":"t","url":"http://x/m/AAAAAAAA"}"#)
                        } else {
                            let _ = tx.send(serde_json::from_slice(&body).unwrap());
                            String::new()
                        };
                        let status = if reply.is_empty() { "204 No Content" } else { "201 Created" };
                        let _ = write!(w, "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{reply}", reply.len());
                    }
                });
            }
        });
        (base, rx)
    }

    fn running() -> Snapshot {
        Snapshot { verdict: Verdict::Running, elapsed_ms: 110_000, ..Snapshot::default() }
    }

    #[test]
    fn abort_sends_the_latest_snapshot_as_stopped() {
        let (base, rx) = server();
        let up = Uploader::start(base);
        up.push(running());
        let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(first.verdict, Verdict::Running);
        let t0 = Instant::now();
        assert!(up.abort("Stopped: the console window was closed", ABORT_WAIT));
        assert!(t0.elapsed() < ABORT_WAIT);
        let last = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(last.verdict, Verdict::Stopped);
        assert_eq!(last.elapsed_ms, 110_000);
        assert_eq!(last.advice, "Stopped: the console window was closed");
        assert!(last.finished.is_some());
        up.push(running());
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    }

    #[test]
    fn abort_keeps_a_queued_final_snapshot() {
        let (base, rx) = server();
        let up = Uploader::start(base);
        up.finish(Snapshot { verdict: Verdict::Pass, finished: Some(5), ..Snapshot::default() });
        assert!(up.abort("closed", Duration::from_secs(5)));
        let last = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!((last.verdict, last.advice.as_str()), (Verdict::Pass, ""));
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    }

    #[test]
    fn abort_uploads_a_pushed_snapshot_it_cannot_stop() {
        for verdict in [Verdict::Menu, Verdict::Pass] {
            let (base, rx) = server();
            let up = Uploader::start(base);
            up.push(Snapshot { verdict, finished: (verdict == Verdict::Pass).then_some(5), ..Snapshot::default() });
            assert!(up.abort("closed", Duration::from_secs(5)));
            let mut last = None;
            while let Ok(s) = rx.recv_timeout(Duration::from_millis(300)) {
                last = Some(s);
            }
            let last = last.expect("the pushed snapshot went up");
            assert_eq!((last.verdict, last.advice.as_str()), (verdict, ""));
        }
    }

    #[test]
    fn abort_gives_up_on_an_unreachable_server() {
        let up = Uploader::start(String::from("http://127.0.0.1:9"));
        up.push(running());
        let t0 = Instant::now();
        assert!(!up.abort("closed", Duration::from_millis(500)));
        assert!(t0.elapsed() < Duration::from_secs(2));
    }
}
