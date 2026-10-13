//! PID 1 of the USB stick's initramfs: mounts, loads drivers, finds the stick, runs memburn.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("memburn-init runs only as PID 1 of the Linux USB stick");
}

#[cfg(target_os = "linux")]
fn main() {
    linux::main()
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::CString;
    use std::fs;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    const STICK: &str = "/stick";
    /// Only the memburn stick has this directory; other FAT partitions are skipped.
    const MARKER: &str = "/stick/EFI/memburn";
    const LOG: &str = "/stick/memburn.log";
    const PSTORE: &str = "/sys/fs/pstore";
    /// memburn exit codes for the Reboot and Power off keys.
    const EXIT_RESTART: i32 = 10;
    const EXIT_POWER_OFF: i32 = 11;
    /// memburn left a new binary at NEXT and extra arguments, one per line, at NEXT_ARGS.
    const EXIT_REEXEC: i32 = 12;
    const NEXT: &str = "/memburn-next";
    const NEXT_ARGS: &str = "/memburn-next.args";
    const MEMBURN: &str = "/bin/memburn";
    /// MemAvailable less this is tested; the rest keeps the kernel out of OOM.
    const DEFAULT_MEM: &str = "-384M";
    const SECURE_BOOT_VAR: &str = "/sys/firmware/efi/efivars/SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c";
    /// finit_module flag for .ko.zst files (kernel decompresses).
    const MODULE_INIT_COMPRESSED_FILE: libc::c_int = 4;

    /// PID 1 only supervises: the work runs in a child, so no fault in it can kill init and panic the kernel.
    pub fn main() {
        loop {
            match unsafe { libc::fork() } {
                0 => {
                    run();
                    unsafe { libc::_exit(0) }
                }
                child if child > 0 => supervise(child),
                _ => {}
            }
            unsafe { libc::sleep(5) };
        }
    }

    /// Reaps orphans until `child` exits, then clears out every other process before the next try.
    fn supervise(child: libc::pid_t) {
        loop {
            let mut status = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
            if pid == child || (pid < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD)) {
                break;
            }
        }
        let msg = b"\x1b[33;1mmemburn-init: the stick's control process stopped; restarting it in 5 s\x1b[0m\n";
        unsafe {
            libc::write(1, msg.as_ptr().cast(), msg.len());
            libc::kill(-1, libc::SIGKILL);
            while libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) > 0 {}
        }
    }

    fn run() {
        mount_all();
        console();
        let _ = fs::write("/proc/sys/kernel/printk", "1 4 1 7");
        let _ = fs::write("/proc/sys/vm/compact_unevictable_allowed", "0");
        let _ = write!(std::io::stdout(), "\x1b[2J\x1b[H");
        let header = format!("memburn USB stick  |  Linux {}  |  Secure Boot: {}", kernel_release(), secure_boot());
        say(&header);
        say("Loading drivers...");
        let failed = load_modules();
        if !failed.is_empty() {
            warn(&format!("Drivers that did not load: {}", failed.join(", ")));
        }
        let sensors = memory_sensors();
        std::thread::spawn(network);
        say("Looking for the memburn USB stick...");
        let stick = find_stick(Duration::from_secs(15));
        if !stick {
            warn("Stick not found: memburn.cfg is ignored and no memburn.log will be written.");
        }
        let sensor_line = if sensors.is_empty() {
            String::from("Memory temperature sensors: none found")
        } else {
            format!("Memory temperature sensors: {}", sensors.join(", "))
        };
        say(&sensor_line);
        if stick {
            let _ = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(LOG)
                .and_then(|mut f| writeln!(f, "{sensor_line}"));
        }
        let crash_log = if stick { save_crash_log() } else { None };
        if let Some(name) = &crash_log {
            let line = format!("Kernel crash log from the previous boot saved as {name}");
            warn(&line);
            let _ = fs::OpenOptions::new().create(true).append(true).open(LOG).and_then(|mut f| writeln!(f, "{line}"));
        }
        let cfg = if stick {
            fs::read_to_string("/stick/EFI/BOOT/memburn.cfg").or_else(|_| fs::read_to_string("/stick/memburn.cfg")).ok()
        } else {
            None
        };
        let mut tags = vec![format!("Secure Boot {}", secure_boot())];
        if !stick {
            tags.push("no log: stick not found".into());
        }
        let mut unattended = cfg.map(|text| translate(&text, crash_log.is_none()));
        let mut later = unattended.as_ref().map(|(args, _)| without_job(args));
        let mut crash_log = crash_log;
        let mut bin = MEMBURN;
        let mut again: Option<(Vec<String>, bool)> = None;
        let mut extra: Vec<String> = Vec::new();
        loop {
            let (args, shutdown) = again.take().unwrap_or_else(|| {
                let (mut args, shutdown) = unattended
                    .take()
                    .or_else(|| Some((later.clone()?, false)))
                    .unwrap_or_else(|| (vec!["--mem".into(), DEFAULT_MEM.into()], false));
                if shutdown {
                    args.push("--exit-when-done".into());
                }
                if let Some(name) = crash_log.take() {
                    args.extend(["--crash-log".into(), name]);
                }
                for t in &tags {
                    args.extend(["--tag".into(), t.clone()]);
                }
                (args, shutdown)
            });
            let mut full = args.clone();
            full.append(&mut extra);
            match run_memburn(bin, &full, stick, &header) {
                Some(EXIT_REEXEC) if Path::new(NEXT).exists() => {
                    bin = NEXT;
                    extra = fs::read_to_string(NEXT_ARGS).unwrap_or_default().lines().map(String::from).collect();
                    again = Some((without_crash_log(args), shutdown));
                }
                Some(EXIT_REEXEC | EXIT_RESTART) => finish(libc::RB_AUTOBOOT),
                Some(EXIT_POWER_OFF) => finish(libc::RB_POWER_OFF),
                _ if shutdown => finish(libc::RB_POWER_OFF),
                Some(2) => {
                    later = None;
                    warn("memburn.cfg has a bad setting (see the message above); starting the menu in 15 s.");
                    std::thread::sleep(Duration::from_secs(15));
                }
                None if bin == NEXT => {
                    warn("The updated memburn crashed or could not be started; starting the booted one in 5 s.");
                    bin = MEMBURN;
                    extra.clear();
                    std::thread::sleep(Duration::from_secs(5));
                }
                None => {
                    warn("memburn crashed or could not be started; restarting it in 5 s.");
                    std::thread::sleep(Duration::from_secs(5));
                }
                Some(_) => {}
            }
        }
    }

    /// Brings up every wired interface as it appears and runs a DHCP client on it.
    fn network() {
        let mut seen: Vec<String> = Vec::new();
        let mut clients: Vec<std::process::Child> = Vec::new();
        loop {
            for e in fs::read_dir("/sys/class/net").into_iter().flatten().flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name == "lo"
                    || seen.contains(&name)
                    || Path::new(&format!("/sys/class/net/{name}/wireless")).exists()
                {
                    continue;
                }
                seen.push(name.clone());
                let quiet = |c: &mut Command| {
                    c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
                };
                let mut up = Command::new("/bin/ip");
                up.args(["link", "set", &name, "up"]);
                quiet(&mut up);
                let _ = up.status();
                let mut dhcp = Command::new("/bin/udhcpc");
                dhcp.args(["-f", "-i", &name, "-s", "/etc/udhcpc.script", "-t", "5", "-T", "2", "-A", "5"]);
                quiet(&mut dhcp);
                if let Ok(child) = dhcp.spawn() {
                    clients.push(child);
                }
            }
            clients.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    fn mount_all() {
        for (src, dst, fstype) in [
            ("devtmpfs", "/dev", "devtmpfs"),
            ("proc", "/proc", "proc"),
            ("sysfs", "/sys", "sysfs"),
            ("efivarfs", "/sys/firmware/efi/efivars", "efivarfs"),
        ] {
            let _ = fs::create_dir_all(dst);
            mount(src, dst, fstype, 0);
        }
    }

    fn mount(src: &str, dst: &str, fstype: &str, flags: libc::c_ulong) -> bool {
        let (src, dst, fstype) = (cstr(src), cstr(dst), cstr(fstype));
        unsafe { libc::mount(src.as_ptr(), dst.as_ptr(), fstype.as_ptr(), flags, std::ptr::null()) == 0 }
    }

    fn cstr(s: &str) -> CString {
        CString::new(s).unwrap_or_default()
    }

    /// Makes tty1 our stdin/stdout/stderr, in text mode; the kernel may have given us no console at all.
    fn console() {
        /// KDSETMODE, KD_TEXT: memburn's pixel dashboard leaves the console in graphics mode if it dies.
        const KDSETMODE: libc::Ioctl = 0x4B3A;
        if let Ok(tty) = fs::OpenOptions::new().read(true).write(true).open("/dev/tty1") {
            unsafe { libc::ioctl(tty.as_raw_fd(), KDSETMODE, 0) };
            for fd in 0..3 {
                unsafe { libc::dup2(tty.as_raw_fd(), fd) };
            }
        }
    }

    /// Write errors are ignored: the tty may be hung up.
    fn say(s: &str) {
        let _ = writeln!(std::io::stdout(), "{s}");
    }

    fn warn(s: &str) {
        let _ = writeln!(std::io::stdout(), "\x1b[33;1m{s}\x1b[0m");
    }

    fn kernel_release() -> String {
        fs::read_to_string("/proc/sys/kernel/osrelease").map_or_else(|_| "?".into(), |s| s.trim().to_string())
    }

    fn secure_boot() -> &'static str {
        match fs::read(SECURE_BOOT_VAR).ok().and_then(|v| v.get(4).copied()) {
            Some(1) => "on",
            Some(_) => "off",
            None => "unknown",
        }
    }

    /// Loads `/lib/modules/load.list` in order (dependencies first); returns the ones that failed.
    fn load_modules() -> Vec<String> {
        let list = fs::read_to_string("/lib/modules/load.list").unwrap_or_default();
        let mut failed = Vec::new();
        for path in list.lines().filter(|l| !l.is_empty()) {
            let name = Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or(path);
            let name = name.split(".ko").next().unwrap_or(name).to_string();
            let Ok(f) = fs::File::open(format!("/lib/modules/{path}")) else {
                failed.push(name);
                continue;
            };
            let flags = if path.ends_with(".zst") { MODULE_INIT_COMPRESSED_FILE } else { 0 };
            let r = unsafe { libc::syscall(libc::SYS_finit_module, f.as_raw_fd(), c"".as_ptr(), flags) };
            let e = std::io::Error::last_os_error();
            // ENODEV: no matching hardware, as coretemp on AMD.
            if r != 0 && !matches!(e.raw_os_error(), Some(libc::EEXIST | libc::ENODEV)) {
                failed.push(format!("{name} ({e})"));
            }
        }
        failed
    }

    const I2C_SLAVE: libc::Ioctl = 0x0703;
    const I2C_SMBUS: libc::Ioctl = 0x0720;
    const I2C_SMBUS_READ: u8 = 1;
    const I2C_SMBUS_BYTE_DATA: u32 = 2;
    const I2C_SMBUS_WORD_DATA: u32 = 3;

    #[repr(C)]
    struct SmbusIoctl {
        read_write: u8,
        command: u8,
        size: u32,
        data: *mut [u8; 34],
    }

    /// One SMBus client on `/dev/i2c-N`, reads only.
    struct Smbus(fs::File);

    impl Smbus {
        fn open(bus: u32, addr: u16) -> Option<Smbus> {
            let f = fs::OpenOptions::new().read(true).write(true).open(format!("/dev/i2c-{bus}")).ok()?;
            // Fails with EBUSY when a driver owns the address.
            (unsafe { libc::ioctl(f.as_raw_fd(), I2C_SLAVE, addr as libc::c_ulong) } == 0).then_some(Smbus(f))
        }

        fn read(&self, reg: u8, size: u32) -> Option<u16> {
            let mut data = [0u8; 34];
            let mut args = SmbusIoctl { read_write: I2C_SMBUS_READ, command: reg, size, data: &mut data };
            let r = unsafe { libc::ioctl(self.0.as_raw_fd(), I2C_SMBUS, &mut args) };
            (r == 0).then(|| u16::from_le_bytes([data[0], data[1]]))
        }

        /// JC42 registers are big-endian words.
        fn word(&self, reg: u8) -> Option<u16> {
            self.read(reg, I2C_SMBUS_WORD_DATA).map(u16::swap_bytes)
        }

        fn byte(&self, reg: u8) -> Option<u8> {
            self.read(reg, I2C_SMBUS_BYTE_DATA).map(|v| v as u8)
        }
    }

    /// The checks of the kernel's jc42_detect: capability, configuration, manufacturer and device registers.
    pub(super) fn is_jc42(cap: u16, config: u16, manufacturer: u16, device: u16) -> bool {
        cap & 0xFF00 == 0 && config & 0xF800 == 0 && !matches!(manufacturer, 0 | 0xFFFF) && device != 0xFFFF
    }

    /// The checks of the kernel's spd5118_detect: MR0/MR1 read 0x51 0x18 (device type SPD5118).
    pub(super) fn is_spd5118(mr0: u8, mr1: u8) -> bool {
        (mr0, mr1) == (0x51, 0x18)
    }

    /// Memory temperature sensors on the chipset SMBus: binds jc42 (DDR4, 0x18-0x1f) and spd5118 (DDR5,
    /// 0x50-0x57) where the drivers did not already; returns the bound ones as "spd5118 i2c-1 0x50".
    fn memory_sensors() -> Vec<String> {
        let dev = Path::new("/sys/bus/i2c/devices");
        let mut buses: Vec<u32> = fs::read_dir(dev)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().strip_prefix("i2c-")?.parse::<u32>().ok()?;
                let name = fs::read_to_string(e.path().join("name")).unwrap_or_default();
                (name.starts_with("SMBus PIIX4") || name.starts_with("SMBus I801")).then_some(n)
            })
            .collect();
        buses.sort_unstable();
        let client = |bus: u32, addr: u16| dev.join(format!("{bus}-{addr:04x}"));
        for &bus in &buses {
            for addr in (0x18..=0x1F).chain(0x50..=0x57) {
                if client(bus, addr).exists() {
                    continue;
                }
                let Some(c) = Smbus::open(bus, addr) else { continue };
                let kind = if addr < 0x50 {
                    let regs = (c.word(0), c.word(1), c.word(6), c.word(7));
                    let found = matches!(regs, (Some(a), Some(b), Some(m), Some(d)) if is_jc42(a, b, m, d));
                    found.then_some("jc42")
                } else {
                    matches!((c.byte(0), c.byte(1)), (Some(a), Some(b)) if is_spd5118(a, b)).then_some("spd5118")
                };
                drop(c);
                let Some(kind) = kind else { continue };
                let adapter = dev.join(format!("i2c-{bus}"));
                if fs::write(adapter.join("new_device"), format!("{kind} {addr:#x}")).is_ok()
                    && !client(bus, addr).join("driver").exists()
                {
                    let _ = fs::write(adapter.join("delete_device"), format!("{addr:#x}"));
                }
            }
        }
        let mut found = Vec::new();
        for &bus in &buses {
            for addr in (0x18..=0x1F).chain(0x50..=0x57) {
                let c = client(bus, addr);
                let name = fs::read_to_string(c.join("name")).unwrap_or_default();
                let name = name.trim();
                if ["jc42", "spd5118"].contains(&name) && c.join("driver").exists() {
                    found.push(format!("{name} i2c-{bus} {addr:#x}"));
                }
            }
        }
        found
    }

    /// Mounts the FAT partition holding [`MARKER`] at [`STICK`], waiting for USB to settle.
    fn find_stick(limit: Duration) -> bool {
        let _ = fs::create_dir_all(STICK);
        let t0 = Instant::now();
        while t0.elapsed() < limit {
            let names: Vec<String> = fs::read_dir("/sys/class/block")
                .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
                .unwrap_or_default();
            for name in names.iter().filter(|n| !["loop", "ram", "zram", "sr"].iter().any(|p| n.starts_with(p))) {
                if mount(&format!("/dev/{name}"), STICK, "vfat", libc::MS_NOATIME) {
                    if Path::new(MARKER).exists() {
                        return true;
                    }
                    unsafe { libc::umount2(cstr(STICK).as_ptr(), 0) };
                }
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        false
    }

    /// Copies the kernel's pstore records (the log of a panic) to one CRLF file on the stick, then deletes them.
    fn save_crash_log() -> Option<String> {
        let _ = fs::create_dir_all(PSTORE);
        mount("pstore", PSTORE, "pstore", 0);
        let mut records: Vec<(String, Vec<u8>, u64)> = fs::read_dir(PSTORE)
            .ok()?
            .flatten()
            .filter_map(|e| {
                let mtime = e.metadata().ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
                Some((e.file_name().to_string_lossy().into_owned(), fs::read(e.path()).ok()?, mtime))
            })
            .collect();
        if records.is_empty() {
            return None;
        }
        // Part 1 of a dump is its end; higher parts are older.
        records.sort_by(|a, b| b.0.cmp(&a.0));
        let newest = records.iter().map(|r| r.2).max().unwrap_or(0);
        let stamp = date(newest);
        let mut text = format!("memburn kernel crash log, {stamp}\r\n");
        for (name, data, _) in &records {
            text.push_str(&format!("\r\n== {name} ==\r\n"));
            for line in String::from_utf8_lossy(data).lines() {
                text.push_str(line);
                text.push_str("\r\n");
            }
        }
        let base = crash_file_name(&stamp);
        let name = (1..100)
            .map(|k| if k == 1 { format!("{base}.txt") } else { format!("{base}-{k}.txt") })
            .find(|n| !Path::new(STICK).join(n).exists())?;
        fs::write(Path::new(STICK).join(&name), text).ok()?;
        unsafe { libc::sync() };
        for (rec, ..) in &records {
            let _ = fs::remove_file(Path::new(PSTORE).join(rec));
        }
        Some(name)
    }

    /// "2026-10-12 11:02 UTC" from Unix seconds.
    pub(super) fn date(unix: u64) -> String {
        let days = (unix / 86_400) as i64;
        let (h, m) = (unix / 3600 % 24, unix / 60 % 60);
        // Howard Hinnant's civil_from_days.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let mo = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + i64::from(mo <= 2);
        format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02} UTC")
    }

    /// `args` without `--crash-log NAME`, which is for the first start only.
    pub(super) fn without_crash_log(args: Vec<String>) -> Vec<String> {
        let mut out = Vec::new();
        let mut it = args.into_iter();
        while let Some(a) = it.next() {
            if a == "--crash-log" {
                it.next();
            } else {
                out.push(a);
            }
        }
        out
    }

    /// memburn.cfg arguments for a start after the first: the menu with the cfg's options, without its job.
    pub(super) fn without_job(args: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if ["--mode", "--crash-after"].contains(&a.as_str()) {
                it.next();
            } else if a != "--exit-when-done" {
                out.push(a.clone());
            }
        }
        out
    }

    /// "2026-10-12 11:02 UTC" -> "memburn-crash-2026-10-12-1102".
    pub(super) fn crash_file_name(stamp: &str) -> String {
        let digits: String = stamp.trim_end_matches(" UTC").replace(':', "").replace(' ', "-");
        format!("memburn-crash-{digits}")
    }

    /// Turns `memburn.cfg` (the boot-stick format, shared with the UEFI build) into memburn arguments.
    /// `crash` lets the `crash_after` test hook through; it is dropped right after a crash.
    pub(super) fn translate(text: &str, crash: bool) -> (Vec<String>, bool) {
        let mut args: Vec<String> = Vec::new();
        let mut shutdown = false;
        let mut mem = DEFAULT_MEM.to_string();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                warn(&format!("memburn.cfg: ignored '{line}' (expected key=value)"));
                continue;
            };
            let (k, v) = (k.trim(), v.trim().to_string());
            let flag = match k {
                "mode" => "--mode",
                "passes" => "-p",
                "minutes" => "-d",
                "cycles" => "-c",
                "fade_secs" => "--fade-secs",
                "cpus" => "-t",
                "fft" => "--fft",
                "tests" => "--tests",
                "seconds" => {
                    match v.parse::<f64>() {
                        Ok(s) => args.extend(["-d".into(), format!("{}", s / 60.0)]),
                        Err(_) => warn(&format!("memburn.cfg: seconds={v} is not a number")),
                    }
                    continue;
                }
                "mem_mib" => {
                    mem = format!("{v}M");
                    continue;
                }
                "inject" => {
                    if v == "1" {
                        args.push("--inject".into());
                    }
                    continue;
                }
                "shutdown" => {
                    shutdown = v == "1";
                    continue;
                }
                "server" => "--server",
                "update" => {
                    match v.as_str() {
                        "auto" => args.push("--update-auto".into()),
                        "off" => args.push("--no-update".into()),
                        _ => warn(&format!("memburn.cfg: update={v} (expected auto or off)")),
                    }
                    continue;
                }
                "upload" => {
                    if v == "0" {
                        args.push("--no-upload".into());
                    }
                    continue;
                }
                // Read by GRUB for the kernel's memtest=N.
                "debugcon" | "early_memtest" => continue,
                "crash_after" => {
                    if crash {
                        args.extend(["--crash-after".into(), v]);
                    }
                    continue;
                }
                _ => {
                    warn(&format!("memburn.cfg: unknown setting '{k}'"));
                    continue;
                }
            };
            args.extend([flag.to_string(), v]);
        }
        args.extend(["--mem".into(), mem]);
        (args, shutdown)
    }

    fn run_memburn(bin: &str, args: &[String], stick: bool, header: &str) -> Option<i32> {
        let mut cmd = Command::new(bin);
        cmd.args(["--boot"]).args(args);
        if stick {
            cmd.args(["--stick", STICK]);
            let _ =
                fs::OpenOptions::new().create(true).append(true).open(LOG).and_then(|mut f| writeln!(f, "{header}"));
            cmd.args(["--log", LOG]);
        }
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY, 1);
                Ok(())
            });
        }
        let code = cmd.status().ok().and_then(|s| s.code());
        // memburn's session ended, which hung up our tty1 descriptors.
        console();
        unsafe { libc::sync() };
        code
    }

    fn finish(how: libc::c_int) -> ! {
        say(if how == libc::RB_POWER_OFF { "Powering off..." } else { "Restarting..." });
        unsafe {
            libc::sync();
            libc::umount2(cstr(STICK).as_ptr(), 0);
            libc::reboot(how);
        }
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::linux::{crash_file_name, date, is_jc42, is_spd5118, translate, without_crash_log, without_job};

    #[test]
    fn sensor_id_checks() {
        assert!(is_jc42(0x006F, 0x0000, 0x1131, 0x2204));
        assert!(!is_jc42(0x0100, 0, 0x1131, 0x2204));
        assert!(!is_jc42(0x006F, 0x0800, 0x1131, 0x2204));
        assert!(!is_jc42(0x006F, 0, 0xFFFF, 0xFFFF));
        assert!(is_spd5118(0x51, 0x18));
        assert!(!is_spd5118(0x23, 0x10));
    }

    #[test]
    fn crash_files_and_the_crash_hook() {
        assert_eq!(crash_file_name("2026-10-12 11:02 UTC"), "memburn-crash-2026-10-12-1102");
        let (args, _) = translate("mode=quick\ncrash_after=10\n", true);
        assert!(args.windows(2).any(|w| w == ["--crash-after", "10"]), "{args:?}");
        let (args, _) = translate("mode=quick\ncrash_after=10\n", false);
        assert!(!args.iter().any(|a| a == "--crash-after"), "{args:?}");
        let (args, _) = translate("server=http://127.0.0.1:3010\nupdate=auto\n", true);
        assert!(args.windows(2).any(|w| w == ["--server", "http://127.0.0.1:3010"]), "{args:?}");
        assert!(args.iter().any(|a| a == "--update-auto"), "{args:?}");
        let (args, _) = translate("update=off\n", true);
        assert!(args.iter().any(|a| a == "--no-update"), "{args:?}");
    }

    #[test]
    fn dates_and_restart_arguments() {
        assert_eq!(date(1_791_802_920), "2026-10-12 11:02 UTC");
        assert_eq!(date(0), "1970-01-01 00:00 UTC");
        let args: Vec<String> = ["--mode", "quick", "--crash-log", "x.txt", "--tag", "t"].map(String::from).into();
        assert_eq!(without_crash_log(args), ["--mode", "quick", "--tag", "t"]);
    }

    #[test]
    fn restarts_keep_the_cfg_options_but_not_its_job() {
        let (args, _) = translate(
            "mode=stress
seconds=30
upload=0
update=off
crash_after=9
",
            true,
        );
        assert_eq!(without_job(&args), ["-d", "0.5", "--no-upload", "--no-update", "--mem", "-384M"]);
    }
}
