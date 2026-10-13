//! Background uploader: creates a run on the live-view server, PUTs the latest snapshot every few seconds, then one final snapshot.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use memburn_proto::{Created, Snapshot};

const EVERY: Duration = Duration::from_secs(3);
/// Delay after a failed run creation: the first, doubling up to the second.
const RETRY: (Duration, Duration) = (Duration::from_secs(1), Duration::from_secs(15));
const ROUTE_POLL: Duration = Duration::from_millis(250);
/// Attempts at the final snapshot, this far apart.
const FINAL_TRIES: u32 = 4;
const FINAL_GAP: Duration = Duration::from_secs(2);

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
        Uploader { shared, thread: Some(thread) }
    }

    pub fn push(&self, snap: Snapshot) {
        let mut s = self.shared.lock().unwrap();
        if !s.last {
            s.snap = Some(snap);
            s.generation += 1;
        }
    }

    /// Queues the final snapshot; later pushes are ignored.
    pub fn finish(&self, snap: Snapshot) {
        self.push(snap);
        self.shared.lock().unwrap().last = true;
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
