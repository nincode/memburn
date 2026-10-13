//! Stick self-update: waits for the network, checks the signed release manifest, stages the new files beside
//! the stick's files in the background, and swaps them in when asked.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use memburn_proto::release::{self, Envelope, Manifest, StickFile};
use sha2::{Digest, Sha256};

const REQUEST: Duration = Duration::from_secs(10);
/// Retry delays after a failed check: doubling from the first to the last.
const BACKOFF: (Duration, Duration) = (Duration::from_secs(2), Duration::from_secs(60));
const ROUTE_POLL: Duration = Duration::from_secs(1);
/// No route for this long counts as offline.
const ROUTE_WAIT: Duration = Duration::from_secs(10);
/// Free space kept on the stick beyond the new files.
const SPARE: u64 = 1 << 20;
const GRUB_CFGS: [&str; 2] = ["EFI/ubuntu/grub.cfg", "EFI/BOOT/grub.cfg"];
const VERSIONS: &str = "EFI/memburn";
/// The new memburn binary, copied off the stick, and its extra arguments (one per line), which the stick's init
/// also runs after exit 12.
pub const NEXT: &str = "/memburn-next";
pub const NEXT_ARGS: &str = "/memburn-next.args";

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    /// Waiting for a server answer.
    Checking,
    /// No network route, or the server did not answer; retrying.
    Offline,
    /// The stick is not updatable or the server has no release.
    Nothing,
    /// No newer release than this one.
    UpToDate(String),
    /// The manifest failed its checks.
    Rejected(String),
    /// A newer release that does not fit on the stick.
    NoSpace(String),
    Staging {
        version: String,
        done: u64,
        total: u64,
    },
    /// Reading the written files back from the stick and checking their hashes; `file` of `files` is done.
    Verifying {
        version: String,
        file: usize,
        files: usize,
    },
    Failed {
        version: String,
        error: String,
    },
    /// Every file is written beside the stick's files and checked; [`Updater::install`] swaps them in.
    Ready(Box<Staged>),
    Installed {
        version: String,
        next: Next,
    },
}

/// How the new version starts in this session; a new kernel and init start at the next boot.
#[derive(Clone, Debug, PartialEq)]
pub enum Next {
    /// memburn runs the new binary at `bin` (a stick path), hash `sha256`, in its own place.
    Reexec { bin: PathBuf, sha256: String },
    /// The release has no memburn binary.
    Reboot,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Staged {
    pub version: String,
    old_version: String,
    /// Staged paths, grub.cfg last.
    paths: Vec<String>,
    pub next: Next,
}

pub struct Updater {
    /// The state and when it was set.
    state: Mutex<(State, Instant)>,
    opted: AtomicBool,
    stick: PathBuf,
    /// The manifest URL.
    url: String,
    /// A check or download thread is running.
    busy: AtomicBool,
}

impl Updater {
    pub fn state(&self) -> State {
        self.state.lock().unwrap().0.clone()
    }

    /// Time since the state last changed.
    pub fn age(&self) -> Duration {
        self.state.lock().unwrap().1.elapsed()
    }

    fn set(&self, s: State) {
        let mut st = self.state.lock().unwrap();
        if st.0 != s {
            *st = (s, Instant::now());
        }
    }

    /// The newer version U starts: downloading, ready, or installed for the next boot.
    pub fn offer(&self) -> Option<String> {
        match &self.state.lock().unwrap().0 {
            State::Staging { version, .. } | State::Verifying { version, .. } | State::Installed { version, .. } => {
                Some(version.clone())
            }
            State::Ready(s) => Some(s.version.clone()),
            _ => None,
        }
    }

    /// Install and start the new version as soon as it is ready (the U key, `update=auto`).
    pub fn opt_in(&self) {
        self.opted.store(true, Ordering::Relaxed);
    }

    pub fn opted(&self) -> bool {
        self.opted.load(Ordering::Relaxed)
    }

    /// No check is running and none would be refused for space: U starts one.
    pub fn idle(&self) -> bool {
        !self.busy.load(Ordering::Relaxed)
            && self.offer().is_none()
            && !matches!(self.state.lock().unwrap().0, State::NoSpace(_))
    }

    /// The U key acts: it opts in to an offered version or checks now.
    pub fn takes_key(&self) -> bool {
        (self.offer().is_some() && !self.opted()) || self.idle()
    }

    /// The U key: checks now, downloads, installs and starts a newer release; opts in to one already offered.
    pub fn press(self: &Arc<Self>) {
        if self.idle() {
            self.opt_in();
            self.check();
        } else if self.offer().is_some() {
            self.opt_in();
        }
    }

    /// Opted in and ready: the run should stop so the new version can start.
    pub fn due(&self) -> bool {
        self.opted() && matches!(self.state.lock().unwrap().0, State::Ready(_))
    }

    /// Swaps in a ready update; the new version, when it did.
    pub fn install(&self) -> Option<Result<String, String>> {
        let staged = {
            let mut s = self.state.lock().unwrap();
            let State::Ready(st) = &s.0 else { return None };
            let st = st.clone();
            *s = (State::Checking, Instant::now());
            st
        };
        let result = swap(&self.stick, &staged);
        self.set(match &result {
            Ok(()) => State::Installed { version: staged.version.clone(), next: staged.next.clone() },
            Err(e) => State::Failed { version: staged.version.clone(), error: e.clone() },
        });
        Some(result.map(|_| staged.version))
    }
}

/// An updater that checks `{server}/memburn/api/latest` against the stick at `stick` only when asked
/// ([`Updater::press`], `update=off`).
pub fn idle(server: &str, stick: PathBuf) -> Arc<Updater> {
    Arc::new(Updater {
        state: Mutex::new((State::Nothing, Instant::now())),
        opted: AtomicBool::new(false),
        stick,
        url: format!("{server}/memburn/api/latest"),
        busy: AtomicBool::new(false),
    })
}

/// [`idle`] with the background check started; a newer release that fits is staged right away.
pub fn start(server: &str, stick: PathBuf) -> Arc<Updater> {
    let u = idle(server, stick);
    u.check();
    u
}

impl Updater {
    /// Checks in a background thread, staging a newer release that fits.
    fn check(self: &Arc<Self>) {
        if self.busy.swap(true, Ordering::Relaxed) {
            return;
        }
        self.set(State::Checking);
        let w = self.clone();
        std::thread::spawn(move || {
            low_priority();
            w.run();
            w.busy.store(false, Ordering::Relaxed);
        });
    }

    fn run(&self) {
        let m = match check(&self.url, &self.stick, &|s| self.set(s)) {
            Ok(m) => m,
            Err(s) => return self.set(s),
        };
        let (version, total) = (m.version.clone(), m.tar.size);
        let progress = |done| self.set(State::Staging { version: version.clone(), done, total });
        let verify = |file, files| self.set(State::Verifying { version: version.clone(), file, files });
        progress(0);
        self.set(match stage(&m, &self.stick, &progress, &verify) {
            Ok(s) => State::Ready(Box::new(s)),
            Err(error) => State::Failed { version, error },
        });
    }
}

/// Nice 19 and idle I/O for the calling thread, so staging does not slow the test.
fn low_priority() {
    #[cfg(target_os = "linux")]
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, 19);
        // IOPRIO_WHO_PROCESS, IOPRIO_CLASS_IDLE << 13.
        libc::syscall(libc::SYS_ioprio_set, 1, tid, 3 << 13);
    }
}

/// A default route exists (Linux); elsewhere assumed.
pub fn has_route() -> bool {
    #[cfg(target_os = "linux")]
    {
        fs::read_to_string("/proc/net/route").is_ok_and(|t| t.lines().skip(1).any(default_route))
    }
    #[cfg(not(target_os = "linux"))]
    true
}

/// A /proc/net/route line for an up default route.
#[cfg(any(target_os = "linux", test))]
fn default_route(line: &str) -> bool {
    let f: Vec<&str> = line.split_whitespace().collect();
    f.len() > 3 && f[1] == "00000000" && u32::from_str_radix(f[3], 16).is_ok_and(|flags| flags & 1 != 0)
}

/// The signed manifest, retried with backoff once a route exists until the server answers; `on` gets
/// [`State::Offline`] while there is no route or answer and [`State::Checking`] while asking.
fn fetch_manifest(url: &str, on: &dyn Fn(State)) -> Result<Manifest, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(REQUEST)).build().into();
    let mut delay = BACKOFF.0;
    let t0 = Instant::now();
    let env: Envelope = loop {
        while !has_route() {
            if t0.elapsed() >= ROUTE_WAIT {
                on(State::Offline);
            }
            std::thread::sleep(ROUTE_POLL);
        }
        on(State::Checking);
        match agent.get(url).call() {
            Ok(mut r) => break r.body_mut().read_json().map_err(|e| format!("bad manifest: {e}"))?,
            Err(ureq::Error::StatusCode(c)) if c < 500 => return Err(format!("HTTP {c}")),
            Err(_) => {
                on(State::Offline);
                std::thread::sleep(delay);
                delay = (delay * 2).min(BACKOFF.1);
            }
        }
    };
    release::verify(&env, release::RELEASE_KEY)
}

/// A newer release that fits on the stick, or the state to stop at.
fn check(url: &str, stick: &Path, on: &dyn Fn(State)) -> Result<Manifest, State> {
    let manifest = match fetch_manifest(url, on) {
        Ok(m) => m,
        Err(e) if e.starts_with("HTTP ") => return Err(State::Nothing),
        Err(e) => return Err(State::Rejected(e)),
    };
    let Some(current) = installed(stick) else { return Err(State::Nothing) };
    if !release::newer(&manifest.version, env!("CARGO_PKG_VERSION")) || !release::newer(&manifest.version, &current) {
        return Err(State::UpToDate(env!("CARGO_PKG_VERSION").into()));
    }
    let fits = crate::os::free_bytes(stick).is_none_or(|free| {
        let need: u64 = manifest
            .files
            .iter()
            .filter(|f| fs::metadata(stick.join(&f.path)).map_or(true, |m| m.len() != f.size))
            .map(|f| f.size)
            .sum();
        need + SPARE <= free + stale(stick, &current, &manifest.version).iter().map(|d| dir_bytes(d)).sum::<u64>()
    });
    if !fits {
        return Err(State::NoSpace(manifest.version));
    }
    Ok(manifest)
}

/// Re-exec when the release has a memburn binary; it runs on the booted kernel and init.
fn next(m: &Manifest) -> Next {
    match m.files.iter().find(|f| f.path == format!("{VERSIONS}/{}/memburn", m.version)) {
        Some(bin) => Next::Reexec { bin: PathBuf::from(&bin.path), sha256: bin.sha256.clone() },
        None => Next::Reboot,
    }
}

/// The version the stick's GRUB menu boots by default, from its `#BEGIN memburn X` line.
fn installed(stick: &Path) -> Option<String> {
    let cfg = fs::read_to_string(stick.join(GRUB_CFGS[1])).ok()?;
    block(&cfg).map(|(v, _)| v)
}

/// The current version's menu block of a grub.cfg: its version and lines, markers included.
pub fn block(cfg: &str) -> Option<(String, String)> {
    let start = cfg.lines().position(|l| l.starts_with("#BEGIN memburn "))?;
    let lines: Vec<&str> = cfg.lines().collect();
    let version = lines[start].strip_prefix("#BEGIN memburn ")?.trim().to_string();
    let end = start + lines[start..].iter().position(|l| l.trim() == "#END")?;
    Some((version, lines[start..=end].join("\n")))
}

/// `new_cfg` with `old` (a [`block`]) in place of its `#PREVIOUS` line, renamed as the previous version.
pub fn with_previous(new_cfg: &str, old_version: &str, old: &str) -> String {
    let title = format!("menuentry \"memburn {old_version}");
    let prev: Vec<String> = old
        .lines()
        .map(|l| {
            if let Some(rest) = l.strip_prefix("#BEGIN memburn ") {
                format!("#BEGIN-PREVIOUS memburn {rest}")
            } else if l.starts_with(&title) {
                l.replacen(&title, &format!("{title} (previous)"), 1)
            } else {
                l.to_string()
            }
        })
        .collect();
    let mut out: Vec<String> = Vec::new();
    for l in new_cfg.lines() {
        if l.trim() == "#PREVIOUS" {
            out.extend(prev.iter().cloned());
        } else {
            out.push(l.to_string());
        }
    }
    out.join("\n") + "\n"
}

/// `cfg` without its previous-version block.
fn without_previous(cfg: &str) -> String {
    let mut out = Vec::new();
    let mut skip = false;
    for l in cfg.lines() {
        if l.starts_with("#BEGIN-PREVIOUS ") {
            skip = true;
        } else if skip && l.trim() == "#END" {
            skip = false;
        } else if !skip {
            out.push(l);
        }
    }
    out.join("\n") + "\n"
}

/// Version directories under `EFI/memburn` other than `keep` and `new`.
fn stale(stick: &Path, keep: &str, new: &str) -> Vec<PathBuf> {
    fs::read_dir(stick.join(VERSIONS))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n != keep && n != new
        })
        .map(|e| e.path())
        .collect()
}

fn dir_bytes(dir: &Path) -> u64 {
    fs::read_dir(dir).into_iter().flatten().flatten().filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum()
}

fn sync() {
    #[cfg(unix)]
    unsafe {
        libc::sync()
    };
}

fn write_synced(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut f = fs::File::create(path)?;
    f.write_all(data)?;
    f.sync_all()
}

/// Swaps in `cfgs` (path, text) by writing each beside the old one and renaming it over.
fn swap_cfgs(stick: &Path, cfgs: &[(&str, String)]) -> Result<(), String> {
    for (p, text) in cfgs {
        let new = stick.join(format!("{p}.new"));
        write_synced(&new, text.as_bytes()).map_err(|e| format!("{p}: {e}"))?;
    }
    sync();
    for (p, _) in cfgs {
        fs::rename(stick.join(format!("{p}.new")), stick.join(p)).map_err(|e| format!("{p}: {e}"))?;
    }
    sync();
    Ok(())
}

fn hex(d: impl AsRef<[u8]>) -> String {
    d.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn file_sha(p: &Path) -> Option<(u64, String)> {
    let mut f = fs::File::open(p).ok()?;
    let (mut h, mut n, mut buf) = (Sha256::new(), 0u64, vec![0u8; 1 << 16]);
    loop {
        let k = f.read(&mut buf).ok()?;
        if k == 0 {
            return Some((n, hex(h.finalize())));
        }
        h.update(&buf[..k]);
        n += k as u64;
    }
}

/// Reads exactly `buf.len()` bytes into `buf`, hashing them into `tar` and counting them.
fn take(r: &mut dyn Read, buf: &mut [u8], tar: &mut Sha256, done: &mut u64) -> Result<(), String> {
    r.read_exact(buf).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            "truncated download".into()
        } else {
            format!("download: {e}")
        }
    })?;
    tar.update(&*buf);
    *done += buf.len() as u64;
    Ok(())
}

fn field(h: &[u8], a: usize, b: usize) -> String {
    let f = &h[a..b];
    String::from_utf8_lossy(&f[..f.iter().position(|&c| c == 0).unwrap_or(f.len())]).into_owned()
}

/// Streams `m`'s tar from `r` into `<path>.new` beside every stick file that differs, checking each file's and the
/// tar's hash. On an error every staged file is removed and the stick is as before.
fn stage_from(
    m: &Manifest,
    stick: &Path,
    r: &mut dyn Read,
    progress: &dyn Fn(u64),
    verify: &dyn Fn(usize, usize),
) -> Result<Staged, String> {
    let (old_version, old_block) = fs::read_to_string(stick.join(GRUB_CFGS[1]))
        .ok()
        .and_then(|c| block(&c))
        .ok_or("the stick's grub.cfg has no version block")?;
    let staged = |p: &str| stick.join(format!("{p}.new"));
    for f in &m.files {
        let _ = fs::remove_file(staged(&f.path));
    }
    let cfg = |p: &str| GRUB_CFGS.contains(&p);
    let differs = |f: &StickFile| cfg(&f.path) || file_sha(&stick.join(&f.path)) != Some((f.size, f.sha256.clone()));
    let need: u64 = m.files.iter().filter(|f| differs(f)).map(|f| f.size).sum::<u64>() + SPARE;
    let free = || crate::os::free_bytes(stick).unwrap_or(u64::MAX);
    let stale_dirs = stale(stick, &old_version, &m.version);
    if free() < need && !stale_dirs.is_empty() {
        let cfgs: Vec<(&str, String)> = GRUB_CFGS
            .iter()
            .filter_map(|p| Some((*p, without_previous(&fs::read_to_string(stick.join(p)).ok()?))))
            .collect();
        swap_cfgs(stick, &cfgs)?;
        for d in &stale_dirs {
            let _ = fs::remove_dir_all(d);
        }
        sync();
    }
    if free() < need {
        return Err(format!("{} MiB free on the stick, {} MiB needed", free() >> 20, need >> 20));
    }
    let mut written: Vec<String> = Vec::new();
    let result = (|| {
        let (mut tar, mut done) = (Sha256::new(), 0u64);
        let mut seen: Vec<&str> = Vec::new();
        let mut buf = vec![0u8; 1 << 16];
        let mut h = [0u8; 512];
        loop {
            take(r, &mut h, &mut tar, &mut done)?;
            if h.iter().all(|&b| b == 0) {
                break;
            }
            let size = u64::from_str_radix(field(&h, 124, 136).trim(), 8).map_err(|_| "bad tar header")?;
            let (prefix, name) = (field(&h, 345, 500), field(&h, 0, 100));
            let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            let regular = matches!(h[156], b'0' | 0);
            let want = m.files.iter().find(|f| regular && f.path == path);
            let write = want.filter(|f| differs(f));
            if let Some(f) = want
                && f.size != size
            {
                return Err(format!("{path}: size mismatch"));
            }
            let mut out = match write {
                Some(f) if !cfg(&f.path) => {
                    let p = staged(&f.path);
                    if let Some(dir) = p.parent() {
                        fs::create_dir_all(dir).map_err(|e| format!("{path}: {e}"))?;
                    }
                    written.push(f.path.clone());
                    Some(fs::File::create(&p).map_err(|e| format!("{path}: {e}"))?)
                }
                _ => None,
            };
            let mut text: Vec<u8> = Vec::new();
            let mut file = Sha256::new();
            let mut left = size;
            while left > 0 {
                let n = left.min(buf.len() as u64) as usize;
                take(r, &mut buf[..n], &mut tar, &mut done)?;
                file.update(&buf[..n]);
                if let Some(o) = &mut out {
                    o.write_all(&buf[..n]).map_err(|e| format!("{path}: {e}"))?;
                } else if write.is_some() {
                    text.extend_from_slice(&buf[..n]);
                }
                left -= n as u64;
                progress(done);
            }
            let pad = (size.div_ceil(512) * 512 - size) as usize;
            take(r, &mut buf[..pad], &mut tar, &mut done)?;
            if let Some(f) = want {
                if hex(file.finalize()) != f.sha256 {
                    return Err(format!("{path}: hash mismatch"));
                }
                seen.push(&f.path);
            }
            if let Some(o) = out {
                o.sync_all().map_err(|e| format!("{path}: {e}"))?;
            } else if let Some(f) = write {
                let new = with_previous(&String::from_utf8_lossy(&text), &old_version, &old_block);
                if fs::read(stick.join(&f.path)).ok().as_deref() != Some(new.as_bytes()) {
                    written.push(f.path.clone());
                    write_synced(&staged(&f.path), new.as_bytes()).map_err(|e| format!("{path}: {e}"))?;
                }
            }
        }
        loop {
            let n = r.read(&mut buf).map_err(|e| format!("download: {e}"))?;
            if n == 0 {
                break;
            }
            tar.update(&buf[..n]);
            done += n as u64;
        }
        if done != m.tar.size {
            return Err(format!("download: {done} of {} bytes", m.tar.size));
        }
        if hex(tar.finalize()) != m.tar.sha256 {
            return Err(String::from("download: hash mismatch"));
        }
        if let Some(f) = m.files.iter().find(|f| !seen.contains(&f.path.as_str())) {
            return Err(format!("{} missing", f.path));
        }
        sync();
        let files: Vec<&StickFile> =
            written.iter().filter_map(|p| m.files.iter().find(|f| f.path == *p && !cfg(p))).collect();
        verify(0, files.len());
        for (k, f) in files.iter().enumerate() {
            let p = staged(&f.path);
            drop_cache(&p);
            if file_sha(&p) != Some((f.size, f.sha256.clone())) {
                return Err(format!("{}: reads back wrong", f.path));
            }
            verify(k + 1, files.len());
        }
        Ok(())
    })();
    sync();
    if let Err(e) = result {
        for p in &written {
            let _ = fs::remove_file(staged(p));
        }
        let _ = fs::remove_dir(stick.join(VERSIONS).join(&m.version));
        sync();
        return Err(e);
    }
    let (cfgs, mut paths): (Vec<String>, Vec<String>) = written.into_iter().partition(|p| cfg(p));
    paths.extend(cfgs);
    Ok(Staged { version: m.version.clone(), old_version, paths, next: next(m) })
}

/// Drops `p`'s cached pages, so the next read comes from the stick.
fn drop_cache(p: &Path) {
    #[cfg(target_os = "linux")]
    if let Ok(f) = fs::File::open(p) {
        use std::os::fd::AsRawFd;
        unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    }
    #[cfg(not(target_os = "linux"))]
    let _ = p;
}

/// Downloads and stages `m` (see [`stage_from`]); `progress` gets the bytes downloaded, `verify` the files
/// read back of all.
fn stage(m: &Manifest, stick: &Path, progress: &dyn Fn(u64), verify: &dyn Fn(usize, usize)) -> Result<Staged, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_body(Some(Duration::from_secs(1800)))
        .build()
        .into();
    let mut r = agent.get(&m.tar.url).call().map_err(|e| format!("download: {e}"))?;
    let mut body = r.body_mut().with_config().limit(m.tar.size + 1).reader();
    stage_from(m, stick, &mut body, progress, verify)
}

/// Renames the staged files over the stick's, grub.cfg last, then removes versions older than the previous one.
fn swap(stick: &Path, s: &Staged) -> Result<(), String> {
    for p in &s.paths {
        fs::rename(stick.join(format!("{p}.new")), stick.join(p)).map_err(|e| format!("{p}: {e}"))?;
    }
    sync();
    for d in stale(stick, &s.old_version, &s.version) {
        let _ = fs::remove_dir_all(d);
    }
    sync();
    Ok(())
}

/// Copies the new binary of a [`Next::Reexec`] to [`NEXT`] and `args` to [`NEXT_ARGS`]; false if it does not check out.
pub fn hand_over(stick: &Path, bin: &Path, sha256: &str, args: &[String]) -> bool {
    let tmp = format!("{NEXT}.tmp");
    let ok = fs::copy(stick.join(bin), &tmp).is_ok()
        && file_sha(Path::new(&tmp)).is_some_and(|(_, h)| h == sha256)
        && {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).is_ok()
            }
            #[cfg(not(unix))]
            true
        }
        && fs::write(NEXT_ARGS, args.join("\n")).is_ok()
        && fs::rename(&tmp, NEXT).is_ok();
    if !ok {
        let _ = fs::remove_file(&tmp);
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use memburn_proto::release::Download;

    const CFG: &str = "set default=0\n#BEGIN memburn @V@\nmenuentry \"memburn @V@  (x)\" {\n    linux /EFI/memburn/@V@/vmlinuz\n}\n#END\n#PREVIOUS\nmenuentry \"UEFI firmware settings\" {\n}\n";

    fn cfg(v: &str) -> String {
        CFG.replace("@V@", v)
    }

    fn tar(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, data) in files {
            let mut h = [0u8; 512];
            h[..name.len()].copy_from_slice(name.as_bytes());
            h[124..135].copy_from_slice(format!("{:011o}", data.len()).as_bytes());
            h[156] = b'0';
            out.extend_from_slice(&h);
            out.extend_from_slice(data);
            out.resize(out.len().div_ceil(512) * 512, 0);
        }
        out.extend_from_slice(&[0; 1024]);
        out
    }

    #[test]
    fn update_off_checks_only_on_u() {
        let u = idle("http://127.0.0.1:9", PathBuf::from("/nonexistent"));
        assert_eq!(u.state(), State::Nothing);
        assert!(u.idle() && u.takes_key() && !u.opted());
        assert!(!u.busy.load(Ordering::Relaxed), "no thread before U");
        u.press();
        assert!(u.opted());
        assert!(!u.idle() && !u.takes_key(), "U does nothing while the check runs");
        assert!(matches!(u.state(), State::Checking | State::Offline | State::Nothing));
    }

    #[test]
    fn u_opts_in_to_an_offer_and_rechecks_after_the_answer() {
        let u = idle("http://127.0.0.1:9", PathBuf::from("/nonexistent"));
        u.set(State::Staging { version: "9.0.0".into(), done: 1, total: 2 });
        assert!(!u.idle() && u.takes_key());
        u.press();
        assert!(u.opted() && !u.takes_key());
        assert!(!u.busy.load(Ordering::Relaxed), "an offer is not checked again");
        u.set(State::UpToDate("0.9.1".into()));
        assert!(u.idle() && u.takes_key());
        u.set(State::NoSpace("9.0.0".into()));
        assert!(!u.idle() && !u.takes_key());
    }

    #[test]
    fn grub_blocks() {
        let (v, b) = block(&cfg("0.5.0")).unwrap();
        assert_eq!(v, "0.5.0");
        let merged = with_previous(&cfg("0.5.1"), &v, &b);
        assert_eq!(block(&merged).unwrap().0, "0.5.1");
        assert!(merged.contains("#BEGIN-PREVIOUS memburn 0.5.0\nmenuentry \"memburn 0.5.0 (previous)  (x)\""));
        assert!(merged.contains("linux /EFI/memburn/0.5.0/vmlinuz") && merged.contains("/EFI/memburn/0.5.1/vmlinuz"));
        assert!(!merged.contains("#PREVIOUS"));
        let stripped = without_previous(&merged);
        assert!(!stripped.contains("0.5.0") && stripped.contains("UEFI firmware settings"));
        let again = with_previous(&cfg("0.5.2"), "0.5.1", &block(&merged).unwrap().1);
        assert!(!again.contains("0.5.0"), "{again}");
    }

    #[test]
    fn routes() {
        let table = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\n\
                     eth0\t0002000A\t00000000\t0001\t0\t0\t0\t00FFFFFF\n\
                     eth0\t00000000\t0202000A\t0003\t0\t0\t0\t00000000\n";
        assert!(table.lines().skip(1).any(default_route));
        assert!(!table.lines().take(2).skip(1).any(default_route));
        assert!(!default_route("eth0\t00000000\t0202000A\t0002\t0\t0\t0\t00000000"));
    }

    /// A stick tree at version 0.5.0 and a 0.5.1 release tar with its manifest.
    fn setup(name: &str, tamper: bool) -> (PathBuf, Manifest, Vec<u8>) {
        let stick = std::env::temp_dir().join(format!("memburn-update-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&stick);
        for (p, data) in [
            ("EFI/BOOT/grub.cfg", cfg("0.5.0").into_bytes()),
            ("EFI/ubuntu/grub.cfg", cfg("0.5.0").into_bytes()),
            ("EFI/memburn/0.5.0/vmlinuz", b"kernel".to_vec()),
            ("EFI/memburn/0.4.9/vmlinuz", b"older kernel".to_vec()),
            ("memburn.cfg", b"mode=quick".to_vec()),
            ("README.txt", b"old readme".to_vec()),
            ("windows/memburn.exe", b"exe".to_vec()),
        ] {
            write_synced(&stick.join(p), &data).unwrap();
        }
        let new_cfg = cfg("0.5.1");
        let files: Vec<(&str, &[u8])> = vec![
            ("EFI/BOOT/grub.cfg", new_cfg.as_bytes()),
            ("EFI/ubuntu/grub.cfg", new_cfg.as_bytes()),
            ("EFI/memburn/0.5.1/vmlinuz", b"kernel"),
            ("EFI/memburn/0.5.1/memburn", &[9u8; 700]),
            ("EFI/memburn/0.5.1/memburn-init", b"init"),
            ("README.txt", b"new readme"),
            ("windows/memburn.exe", b"exe"),
        ];
        let t = tar(&files);
        let manifest = Manifest {
            version: "0.5.1".into(),
            tar: Download { url: String::new(), size: t.len() as u64, sha256: release::sha256_hex(&t) },
            files: files
                .iter()
                .map(|(p, d)| StickFile {
                    path: p.to_string(),
                    size: d.len() as u64,
                    sha256: release::sha256_hex(if tamper && p.ends_with("vmlinuz") { b"x" } else { d }),
                })
                .collect(),
            assets: vec![],
        };
        (stick, manifest, t)
    }

    fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        for e in fs::read_dir(dir).unwrap().flatten() {
            if e.path().is_dir() {
                out.extend(snapshot(&e.path()));
            } else {
                out.push((e.path(), fs::read(e.path()).unwrap()));
            }
        }
        out.sort();
        out
    }

    #[test]
    fn staged_files_swap_in_and_keep_the_previous_version() {
        let (stick, m, t) = setup("ok", false);
        let before = snapshot(&stick);
        let last = std::cell::Cell::new(0);
        let checked = std::cell::RefCell::new(Vec::new());
        let s =
            stage_from(&m, &stick, &mut &t[..], &|d| last.set(d), &|k, n| checked.borrow_mut().push((k, n))).unwrap();
        assert!(last.get() > 0 && last.get() <= t.len() as u64);
        assert_eq!(*checked.borrow(), [(0, 4), (1, 4), (2, 4), (3, 4), (4, 4)]);
        let read = |p: &str| fs::read_to_string(stick.join(p)).unwrap_or_default();
        assert_eq!(read("EFI/memburn/0.5.1/vmlinuz.new"), "kernel");
        assert!(!stick.join("windows/memburn.exe.new").exists());
        assert!(s.paths[s.paths.len() - 2..].iter().all(|p| GRUB_CFGS.contains(&p.as_str())), "{:?}", s.paths);
        assert_eq!(
            s.next,
            Next::Reexec { bin: "EFI/memburn/0.5.1/memburn".into(), sha256: release::sha256_hex(&[9u8; 700]) }
        );
        let unstaged: Vec<_> =
            snapshot(&stick).into_iter().filter(|(p, _)| !p.to_string_lossy().ends_with(".new")).collect();
        assert_eq!(unstaged, before);
        swap(&stick, &s).unwrap();
        assert_eq!(read("EFI/memburn/0.5.1/vmlinuz"), "kernel");
        assert_eq!(read("EFI/memburn/0.5.0/vmlinuz"), "kernel");
        assert!(!stick.join("EFI/memburn/0.4.9").exists());
        assert_eq!(read("README.txt"), "new readme");
        assert_eq!(read("memburn.cfg"), "mode=quick");
        for p in GRUB_CFGS {
            let c = read(p);
            assert_eq!(block(&c).unwrap().0, "0.5.1");
            assert!(c.contains("menuentry \"memburn 0.5.0 (previous)"), "{c}");
        }
        assert!(snapshot(&stick).iter().all(|(p, _)| !p.to_string_lossy().ends_with(".new")));
        assert_eq!(installed(&stick).as_deref(), Some("0.5.1"));
        let _ = fs::remove_dir_all(stick);
    }

    #[test]
    fn only_a_release_without_memburn_reboots() {
        let (stick, m, _) = setup("next", false);
        let bin = Next::Reexec { bin: "EFI/memburn/0.5.1/memburn".into(), sha256: release::sha256_hex(&[9u8; 700]) };
        assert_eq!(next(&m), bin);
        let old = Manifest { files: m.files.iter().filter(|f| !f.path.ends_with("/memburn")).cloned().collect(), ..m };
        assert_eq!(next(&old), Next::Reboot);
        let _ = fs::remove_dir_all(stick);
    }

    #[test]
    fn a_bad_hash_leaves_the_stick_unchanged() {
        let (stick, m, t) = setup("hash", true);
        let before = snapshot(&stick);
        let r = stage_from(&m, &stick, &mut &t[..], &|_| {}, &|_, _| {});
        assert_eq!(r, Err("EFI/memburn/0.5.1/vmlinuz: hash mismatch".into()));
        assert_eq!(snapshot(&stick), before);
        let _ = fs::remove_dir_all(stick);
    }

    #[test]
    fn a_short_or_bad_download_leaves_the_stick_unchanged() {
        let (stick, m, t) = setup("short", false);
        let before = snapshot(&stick);
        let r = stage_from(&m, &stick, &mut &t[..t.len() - 1500], &|_| {}, &|_, _| {});
        assert_eq!(r, Err("truncated download".into()));
        assert_eq!(snapshot(&stick), before);
        let mut bad = t.clone();
        bad[t.len() - 1] = 1;
        let r = stage_from(&m, &stick, &mut &bad[..], &|_| {}, &|_, _| {});
        assert_eq!(r, Err("download: hash mismatch".into()));
        assert_eq!(snapshot(&stick), before);
        let _ = fs::remove_dir_all(stick);
    }

    #[test]
    fn a_bad_signature_stages_nothing() {
        let (stick, m, _) = setup("live", false);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let m = Manifest { tar: Download { url: format!("{base}/s.tar"), ..m.tar.clone() }, ..m };
        let env = Envelope { manifest: serde_json::to_string(&m).unwrap(), sig: String::new() };
        let server = std::thread::spawn(move || {
            let (mut c, _) = listener.accept().unwrap();
            let mut req = [0u8; 4096];
            let _ = c.read(&mut req).unwrap();
            let body = serde_json::to_vec(&env).unwrap();
            let _ = write!(c, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = c.write_all(&body);
        });
        let u = start(&base, stick.clone());
        for _ in 0..100 {
            if !matches!(u.state(), State::Checking) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(u.state(), State::Rejected("bad signature".into()));
        server.join().unwrap();
        assert!(snapshot(&stick).iter().all(|(p, _)| !p.to_string_lossy().ends_with(".new")));
        let _ = fs::remove_dir_all(stick);
    }
}
