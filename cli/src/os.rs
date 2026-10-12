use memburn_core::coverage::Ranges;

/// Page-aligned test buffer, committed up front and locked in RAM when the OS allows it.
pub struct Region {
    pub ptr: *mut u8,
    pub len: usize,
    pub locked: bool,
    /// Bytes of the process in transparent huge pages, where the OS tells.
    pub huge: Option<u64>,
}

unsafe impl Send for Region {}
unsafe impl Sync for Region {}

#[cfg(windows)]
mod imp {
    use super::Region;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree, VirtualLock,
    };
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};

    pub fn available() -> u64 {
        let mut s: MEMORYSTATUSEX = unsafe { core::mem::zeroed() };
        s.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
        if unsafe { GlobalMemoryStatusEx(&mut s) } == 0 { 0 } else { s.ullAvailPhys }
    }

    pub fn total() -> u64 {
        let mut s: MEMORYSTATUSEX = unsafe { core::mem::zeroed() };
        s.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
        if unsafe { GlobalMemoryStatusEx(&mut s) } == 0 { 0 } else { s.ullTotalPhys }
    }

    pub fn alloc(len: usize) -> Result<Region, String> {
        let ptr = unsafe { VirtualAlloc(core::ptr::null(), len, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
        if ptr.is_null() {
            return Err(format!("VirtualAlloc of {len} bytes failed"));
        }
        let slack = 256 << 20;
        let locked = unsafe {
            SetProcessWorkingSetSize(GetCurrentProcess(), len + slack, len + 2 * slack) != 0
                && VirtualLock(ptr, len) != 0
        };
        Ok(Region { ptr: ptr as *mut u8, len, locked, huge: None })
    }

    pub fn can_lock(_len: usize) -> bool {
        false
    }

    /// SMBIOS structure table, after the 8-byte RawSMBIOSData header.
    pub fn smbios() -> Vec<u8> {
        use windows_sys::Win32::System::SystemInformation::{GetSystemFirmwareTable, RSMB};
        let n = unsafe { GetSystemFirmwareTable(RSMB, 0, core::ptr::null_mut(), 0) };
        let mut buf = vec![0u8; n as usize];
        let got = unsafe { GetSystemFirmwareTable(RSMB, 0, buf.as_mut_ptr(), n) };
        if n < 8 || got != n {
            return Vec::new();
        }
        let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        buf.drain(..8);
        buf.truncate(len);
        buf
    }

    pub fn free(r: &mut Region) {
        unsafe { VirtualFree(r.ptr as _, 0, MEM_RELEASE) };
    }
}

#[cfg(unix)]
mod imp {
    use super::Region;

    pub fn available() -> u64 {
        meminfo("MemAvailable:")
    }

    pub fn total() -> u64 {
        meminfo("MemTotal:")
    }

    fn meminfo(key: &str) -> u64 {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|s| {
                s.lines().find(|l| l.starts_with(key)).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
            })
            .map_or(0, |kib| kib * 1024)
    }

    const HUGE: usize = 2 << 20;

    /// Maps `len` bytes on a 2 MiB boundary, asks for huge pages, then locks or touches every page.
    pub fn alloc(len: usize) -> Result<Region, String> {
        let raw = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                len + HUGE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if raw == libc::MAP_FAILED {
            return Err(format!("mmap of {len} bytes failed"));
        }
        let start = (raw as usize).next_multiple_of(HUGE);
        let head = start - raw as usize;
        unsafe {
            if head > 0 {
                libc::munmap(raw, head);
            }
            if head < HUGE {
                libc::munmap((start + len) as *mut libc::c_void, HUGE - head);
            }
        }
        let ptr = start as *mut u8;
        #[cfg(target_os = "linux")]
        unsafe {
            libc::madvise(ptr as _, len, libc::MADV_HUGEPAGE);
        }
        let locked = unsafe { libc::mlock(ptr as _, len) } == 0;
        if !locked {
            for off in (0..len).step_by(4096) {
                unsafe { ptr.add(off).write_volatile(0) };
            }
        }
        Ok(Region { ptr, len, locked, huge: huge_bytes() })
    }

    /// AnonHugePages of this process.
    fn huge_bytes() -> Option<u64> {
        let s = std::fs::read_to_string("/proc/self/smaps_rollup").ok()?;
        let l = s.lines().find(|l| l.starts_with("AnonHugePages:"))?;
        Some(l.split_whitespace().nth(1)?.parse::<u64>().ok()? * 1024)
    }

    /// Locking `len` bytes is allowed: root, or a memlock limit that covers it.
    pub fn can_lock(len: usize) -> bool {
        let mut r: libc::rlimit = unsafe { core::mem::zeroed() };
        let limit = unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut r) } == 0
            && (r.rlim_cur == libc::RLIM_INFINITY || r.rlim_cur >= len as libc::rlim_t);
        limit || unsafe { libc::geteuid() } == 0
    }

    pub fn free(r: &mut Region) {
        unsafe { libc::munmap(r.ptr as _, r.len) };
    }

    /// SMBIOS structure table; readable by root only.
    pub fn smbios() -> Vec<u8> {
        std::fs::read("/sys/firmware/dmi/tables/DMI").unwrap_or_default()
    }
}

pub use imp::{alloc, available, can_lock, smbios, total};

impl Drop for Region {
    fn drop(&mut self) {
        imp::free(self);
    }
}

/// Physical address behind `addr` from a `/proc/self/pagemap` entry: bit 63 present, bits 0-54 PFN.
/// The PFN reads as 0 without CAP_SYS_ADMIN, which means unknown.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn pagemap_phys(entry: u64, addr: usize) -> Option<u64> {
    let pfn = entry & ((1 << 55) - 1);
    (entry >> 63 == 1 && pfn != 0).then(|| pfn * 4096 + (addr as u64 & 0xfff))
}

/// Physical address of `addr` in this process; None when the OS does not tell.
pub fn phys(addr: usize) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::FileExt;
        let f = std::fs::File::open("/proc/self/pagemap").ok()?;
        let mut b = [0u8; 8];
        f.read_exact_at(&mut b, (addr / 4096 * 8) as u64).ok()?;
        pagemap_phys(u64::from_le_bytes(b), addr)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = addr;
        None
    }
}

/// Physical ranges behind `len` bytes at `ptr` in this process; None without CAP_SYS_ADMIN or off Linux.
pub fn phys_ranges(ptr: *const u8, len: usize) -> Option<Ranges> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::FileExt;
        let f = std::fs::File::open("/proc/self/pagemap").ok()?;
        let (first, pages) = (ptr as u64 / 4096, len.div_ceil(4096));
        let mut ranges = Ranges::default();
        let mut buf = vec![0u8; 8 << 20];
        for at in (0..pages).step_by(buf.len() / 8) {
            let n = (pages - at).min(buf.len() / 8);
            f.read_exact_at(&mut buf[..n * 8], (first + at as u64) * 8).ok()?;
            let entries: Vec<u64> = buf[..n * 8].as_chunks::<8>().0.iter().map(|&b| u64::from_le_bytes(b)).collect();
            ranges = ranges.union(&memburn_core::coverage::pagemap_ranges(&entries));
        }
        (!ranges.is_empty()).then_some(ranges)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (ptr, len);
        None
    }
}

/// "System RAM" from `/sys/firmware/memmap`; None off Linux.
pub fn system_ram() -> Option<Ranges> {
    #[cfg(target_os = "linux")]
    {
        let mut entries = Vec::new();
        for d in std::fs::read_dir("/sys/firmware/memmap").ok()?.flatten() {
            let read = |k: &str| std::fs::read_to_string(d.path().join(k)).unwrap_or_default();
            let num = |k: &str| u64::from_str_radix(read(k).trim().trim_start_matches("0x"), 16).ok();
            if let (Some(s), Some(e)) = (num("start"), num("end")) {
                entries.push((s, e, read("type")));
            }
        }
        let entries: Vec<(u64, u64, &str)> = entries.iter().map(|(s, e, t)| (*s, *e, t.as_str())).collect();
        Some(memburn_core::coverage::system_ram(&entries)).filter(|r| !r.is_empty())
    }
    #[cfg(not(target_os = "linux"))]
    None
}

/// The kernel log as `/dev/kmsg` records, empty when unreadable.
pub fn kernel_log() -> String {
    #[cfg(target_os = "linux")]
    {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        let Ok(mut f) = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open("/dev/kmsg") else {
            return String::new();
        };
        let mut out = String::new();
        let mut rec = vec![0u8; 8192];
        loop {
            match f.read(&mut rec) {
                Ok(0) => break,
                Ok(n) => out.push_str(&String::from_utf8_lossy(&rec[..n])),
                // EPIPE: a record was overwritten while reading; the next read continues.
                Err(e) if e.raw_os_error() == Some(libc::EPIPE) => continue,
                Err(_) => break,
            }
        }
        out
    }
    #[cfg(not(target_os = "linux"))]
    String::new()
}

/// Logical CPUs this process may run on; `0..fallback` where the OS does not tell.
pub fn cpu_list(fallback: usize) -> Vec<usize> {
    #[cfg(target_os = "linux")]
    unsafe {
        let mut set: libc::cpu_set_t = core::mem::zeroed();
        if libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set) == 0 {
            let v: Vec<usize> = (0..libc::CPU_SETSIZE as usize).filter(|&n| libc::CPU_ISSET(n, &set)).collect();
            if !v.is_empty() {
                return v;
            }
        }
    }
    (0..fallback).collect()
}

/// Pins the calling thread to logical CPU `cpu`.
pub fn pin_current(cpu: usize) -> bool {
    #[cfg(target_os = "linux")]
    unsafe {
        let mut set: libc::cpu_set_t = core::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set) == 0
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadAffinityMask};
        cpu < usize::BITS as usize && SetThreadAffinityMask(GetCurrentThread(), 1 << cpu) != 0
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = cpu;
        false
    }
}

/// Bytes free to an unprivileged writer on the file system holding `path`.
pub fn free_bytes(path: &std::path::Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Terminal columns, or 0 when unknown.
pub fn term_width() -> usize {
    #[cfg(unix)]
    unsafe {
        let mut ws: libc::winsize = core::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 {
            return ws.ws_col as usize;
        }
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::{
            CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo, GetStdHandle, STD_OUTPUT_HANDLE,
        };
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = core::mem::zeroed();
        if GetConsoleScreenBufferInfo(GetStdHandle(STD_OUTPUT_HANDLE), &mut info) != 0 {
            return (info.srWindow.Right - info.srWindow.Left + 1) as usize;
        }
    }
    0
}

/// Turns on ANSI escape handling where the console needs it; false when stdout is not a terminal.
pub fn enable_ansi() -> bool {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        return false;
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::{
            ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_OUTPUT_HANDLE, SetConsoleMode,
        };
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(h, &mut mode) == 0 || SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) == 0 {
            return false;
        }
    }
    true
}

/// Hottest CPU sensor in °C (Linux hwmon k10temp/coretemp/zenpower); None elsewhere.
pub fn temp_c() -> Option<i32> {
    hwmon_max(&["k10temp", "coretemp", "zenpower"])
}

/// Hottest memory module sensor in °C (Linux hwmon jc42 for DDR4, spd5118 for DDR5); None elsewhere.
pub fn dimm_temps() -> Option<i32> {
    hwmon_max(&["jc42", "spd5118"])
}

/// Highest `temp*_input` of the hwmon devices with one of `names`, in °C.
fn hwmon_max(names: &[&str]) -> Option<i32> {
    #[cfg(target_os = "linux")]
    {
        let mut best: Option<i32> = None;
        for dev in std::fs::read_dir("/sys/class/hwmon").ok()?.flatten() {
            let p = dev.path();
            let name = std::fs::read_to_string(p.join("name")).unwrap_or_default();
            if !names.contains(&name.trim()) {
                continue;
            }
            for f in std::fs::read_dir(&p).into_iter().flatten().flatten() {
                let n = f.file_name();
                let n = n.to_string_lossy();
                if n.starts_with("temp") && n.ends_with("_input") {
                    let v = std::fs::read_to_string(f.path()).ok().and_then(|s| s.trim().parse::<i32>().ok());
                    if let Some(v) = v {
                        best = Some(best.map_or(v / 1000, |b| b.max(v / 1000)));
                    }
                }
            }
        }
        best
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = names;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_pagemap_entries() {
        assert_eq!(pagemap_phys((1 << 63) | 0x1dac5a60, 0x7f3a_2c00_0eb0), Some(0x01da_c5a6_0eb0));
        assert_eq!(pagemap_phys((1 << 63) | (1 << 55) | 5, 0x1234), Some(0x5234));
        assert_eq!(pagemap_phys(1 << 63, 0x1234), None);
        assert_eq!(pagemap_phys(0x1dac5a60, 0x1234), None);
    }
}
