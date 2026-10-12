//! Stick self-update: checks the signed release manifest, then replaces the stick's boot files.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use memburn_proto::release::{self, Envelope, Manifest};

/// The check gives up this long after start; each request has [`REQUEST`].
const CHECK_FOR: Duration = Duration::from_secs(10);
const REQUEST: Duration = Duration::from_secs(3);
const RETRY: Duration = Duration::from_secs(2);
/// Free space kept on the stick beyond the new files.
const SPARE: u64 = 1 << 20;
const GRUB_CFGS: [&str; 2] = ["EFI/ubuntu/grub.cfg", "EFI/BOOT/grub.cfg"];
const VERSIONS: &str = "EFI/memburn";

#[derive(Clone)]
pub enum Check {
    Pending,
    /// No newer release, no answer, or the stick is not updatable.
    Nothing,
    /// The manifest failed its checks.
    Rejected(String),
    /// A newer release; `fits` is false when the stick has too little space.
    Offer {
        manifest: Box<Manifest>,
        fits: bool,
    },
}

/// Starts the background check of `{server}/memburn/api/latest` against the stick at `stick`.
pub fn start(server: &str, stick: PathBuf) -> Arc<Mutex<Check>> {
    let state = Arc::new(Mutex::new(Check::Pending));
    let (s, url) = (state.clone(), format!("{server}/memburn/api/latest"));
    std::thread::spawn(move || {
        let result = check(&url, &stick);
        *s.lock().unwrap() = result;
    });
    state
}

/// Waits until the check is done.
pub fn wait(state: &Mutex<Check>) -> Check {
    loop {
        let c = state.lock().unwrap().clone();
        if !matches!(c, Check::Pending) {
            return c;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn check(url: &str, stick: &Path) -> Check {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(REQUEST)).build().into();
    let t0 = Instant::now();
    let env: Envelope = loop {
        match agent.get(url).call() {
            Ok(mut r) => match r.body_mut().read_json() {
                Ok(e) => break e,
                Err(e) => return Check::Rejected(format!("bad manifest: {e}")),
            },
            Err(ureq::Error::StatusCode(_)) => return Check::Nothing,
            Err(_) if t0.elapsed() + RETRY < CHECK_FOR => std::thread::sleep(RETRY),
            Err(_) => return Check::Nothing,
        }
    };
    let manifest = match release::verify(&env, release::RELEASE_KEY) {
        Ok(m) => m,
        Err(e) => return Check::Rejected(e),
    };
    let Some(current) = installed(stick) else { return Check::Nothing };
    if !release::newer(&manifest.version, env!("CARGO_PKG_VERSION")) || !release::newer(&manifest.version, &current) {
        return Check::Nothing;
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
    Check::Offer { manifest: Box::new(manifest), fits }
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

/// Regular files of a ustar archive, as (path, contents).
pub fn untar(data: &[u8]) -> Result<Vec<(String, &[u8])>, String> {
    let field = |h: &[u8], a: usize, b: usize| {
        let f = &h[a..b];
        String::from_utf8_lossy(&f[..f.iter().position(|&c| c == 0).unwrap_or(f.len())]).into_owned()
    };
    let mut files = Vec::new();
    let mut at = 0;
    while at + 512 <= data.len() {
        let h = &data[at..at + 512];
        if h.iter().all(|&b| b == 0) {
            return Ok(files);
        }
        let size = u64::from_str_radix(field(h, 124, 136).trim(), 8).map_err(|_| "bad tar header")? as usize;
        let prefix = field(h, 345, 500);
        let name = field(h, 0, 100);
        let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        let body = at + 512;
        let end = body.checked_add(size).filter(|&e| e <= data.len()).ok_or("truncated tar")?;
        if matches!(h[156], b'0' | 0) {
            files.push((path, &data[body..end]));
        }
        at = body + size.div_ceil(512) * 512;
    }
    Err("truncated tar".into())
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

/// Downloads `m`'s files, checks every hash, writes them beside the stick's files and swaps them in,
/// grub.cfg last; the version GRUB booted so far stays as the previous entry. `say` gets progress.
/// On an error before the swap the stick is unchanged.
pub fn apply(m: &Manifest, stick: &Path, say: &dyn Fn(String)) -> Result<(), String> {
    let (old_version, old_block) = fs::read_to_string(stick.join(GRUB_CFGS[1]))
        .ok()
        .and_then(|c| block(&c))
        .ok_or("the stick's grub.cfg has no version block")?;
    let tar = download(m, say)?;
    say(String::from("checking"));
    let files = untar(&tar)?;
    let mut writes: Vec<(String, Vec<u8>)> = Vec::new();
    for f in &m.files {
        let (_, data) = files.iter().find(|(p, _)| *p == f.path).ok_or(format!("{} missing", f.path))?;
        if data.len() as u64 != f.size || release::sha256_hex(data) != f.sha256 {
            return Err(format!("{}: hash mismatch", f.path));
        }
        let data = if GRUB_CFGS.contains(&f.path.as_str()) {
            let cfg = String::from_utf8_lossy(data);
            with_previous(&cfg, &old_version, &old_block).into_bytes()
        } else {
            data.to_vec()
        };
        if fs::read(stick.join(&f.path)).ok().as_deref() != Some(&data[..]) {
            writes.push((f.path.clone(), data));
        }
    }
    let need: u64 = writes.iter().map(|(_, d)| d.len() as u64).sum::<u64>() + SPARE;
    let free = || crate::os::free_bytes(stick).unwrap_or(u64::MAX);
    let stale_dirs = stale(stick, &old_version, &m.version);
    if free() < need && !stale_dirs.is_empty() {
        say(String::from("removing the previous version"));
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
    let staged = |p: &str| stick.join(format!("{p}.new"));
    let clean = |n: usize| {
        for (p, _) in &writes[..n] {
            let _ = fs::remove_file(staged(p));
        }
        sync();
    };
    for (k, (p, data)) in writes.iter().enumerate() {
        say(format!("writing {p}"));
        if let Err(e) = write_synced(&staged(p), data) {
            clean(k + 1);
            return Err(format!("{p}: {e}"));
        }
    }
    sync();
    say(String::from("switching"));
    let (cfgs, rest): (Vec<_>, Vec<_>) = writes.iter().partition(|(p, _)| GRUB_CFGS.contains(&p.as_str()));
    for (p, _) in rest.iter().chain(cfgs.iter()) {
        fs::rename(staged(p), stick.join(p)).map_err(|e| format!("{p}: {e}"))?;
    }
    sync();
    for d in stale(stick, &old_version, &m.version) {
        let _ = fs::remove_dir_all(d);
    }
    sync();
    Ok(())
}

fn download(m: &Manifest, say: &dyn Fn(String)) -> Result<Vec<u8>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_body(Some(Duration::from_secs(600)))
        .build()
        .into();
    let mut r = agent.get(&m.tar.url).call().map_err(|e| format!("download: {e}"))?;
    let mut body = r.body_mut().with_config().limit(m.tar.size + 1).reader();
    let mut data = Vec::with_capacity(m.tar.size as usize);
    let mut buf = vec![0u8; 1 << 16];
    let mut shown = Instant::now();
    loop {
        let n = body.read(&mut buf).map_err(|e| format!("download: {e}"))?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
        if shown.elapsed() > Duration::from_millis(250) {
            shown = Instant::now();
            say(format!("downloading {} / {} MiB", data.len() >> 20, m.tar.size >> 20));
        }
    }
    if data.len() as u64 != m.tar.size {
        return Err(format!("download: {} of {} bytes", data.len(), m.tar.size));
    }
    if release::sha256_hex(&data) != m.tar.sha256 {
        return Err(String::from("download: hash mismatch"));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use memburn_proto::release::{Download, StickFile};

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
    fn tar_files() {
        let t = tar(&[("a/b.txt", b"hello"), ("c", &[7u8; 600])]);
        let f = untar(&t).unwrap();
        assert_eq!(f[0], ("a/b.txt".to_string(), &b"hello"[..]));
        assert_eq!(f[1].1.len(), 600);
        assert!(untar(&t[..1000]).is_err());
    }

    /// A stick tree at version 0.5.0 and a manifest for 0.5.1 served from a one-shot local HTTP server.
    fn setup(name: &str, tamper: bool) -> (PathBuf, Manifest, std::thread::JoinHandle<()>) {
        let stick = std::env::temp_dir().join(format!("memburn-update-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&stick);
        for (p, data) in [
            ("EFI/BOOT/grub.cfg", cfg("0.5.0").into_bytes()),
            ("EFI/ubuntu/grub.cfg", cfg("0.5.0").into_bytes()),
            ("EFI/memburn/0.5.0/vmlinuz", b"old kernel".to_vec()),
            ("EFI/memburn/0.4.9/vmlinuz", b"older kernel".to_vec()),
            ("memburn.cfg", b"mode=quick".to_vec()),
            ("README.txt", b"old readme".to_vec()),
        ] {
            write_synced(&stick.join(p), &data).unwrap();
        }
        let new_cfg = cfg("0.5.1");
        let files: Vec<(&str, &[u8])> = vec![
            ("EFI/BOOT/grub.cfg", new_cfg.as_bytes()),
            ("EFI/ubuntu/grub.cfg", new_cfg.as_bytes()),
            ("EFI/memburn/0.5.1/vmlinuz", b"new kernel"),
            ("README.txt", b"new readme"),
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
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/s.tar", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut c, _) = listener.accept().unwrap();
            let mut req = [0u8; 4096];
            let _ = c.read(&mut req);
            let _ = write!(c, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", t.len());
            let _ = c.write_all(&t);
        });
        (stick, Manifest { tar: Download { url, ..manifest.tar }, ..manifest }, server)
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
    fn update_swaps_files_and_keeps_the_previous_version() {
        let (stick, m, server) = setup("ok", false);
        apply(&m, &stick, &|_| {}).unwrap();
        server.join().unwrap();
        let read = |p: &str| fs::read_to_string(stick.join(p)).unwrap_or_default();
        assert_eq!(read("EFI/memburn/0.5.1/vmlinuz"), "new kernel");
        assert_eq!(read("EFI/memburn/0.5.0/vmlinuz"), "old kernel");
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
    fn a_bad_hash_leaves_the_stick_unchanged() {
        let (stick, m, server) = setup("hash", true);
        let before = snapshot(&stick);
        assert_eq!(apply(&m, &stick, &|_| {}), Err("EFI/memburn/0.5.1/vmlinuz: hash mismatch".into()));
        server.join().unwrap();
        assert_eq!(snapshot(&stick), before);
        let _ = fs::remove_dir_all(stick);
    }

    #[test]
    fn a_short_download_leaves_the_stick_unchanged() {
        let (stick, m, server) = setup("short", false);
        let before = snapshot(&stick);
        let m = Manifest { tar: Download { size: m.tar.size + 512, ..m.tar.clone() }, ..m };
        assert!(apply(&m, &stick, &|_| {}).unwrap_err().starts_with("download:"));
        server.join().unwrap();
        assert_eq!(snapshot(&stick), before);
        let _ = fs::remove_dir_all(stick);
    }
}
