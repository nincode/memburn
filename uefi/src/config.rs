use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use memburn_core::memtest::{self, TestId};
use uefi::boot;
use uefi::cstr16;
use uefi::fs::FileSystem;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Quick,
    Mem,
    Burn,
    Both,
    Cores,
}

/// Settings from `memburn.cfg` (`key=value` lines) next to the boot image.
pub struct Config {
    /// Set: start at once without the menu.
    pub mode: Option<Mode>,
    pub passes: u64,
    /// Unset: quick 2 min, both 10, burn 30; per CPU for cores, unset 2.
    pub burn_secs: Option<u64>,
    /// 0 repeats until Esc.
    pub cycles: u64,
    pub fft: Vec<u32>,
    pub tests: Vec<TestId>,
    pub fade_secs: u64,
    pub cpus: Option<usize>,
    pub mem_limit: Option<u64>,
    pub shutdown: bool,
    /// Mirror log lines to I/O port 0xE9 (QEMU/Bochs debug console).
    pub debugcon: bool,
    /// Self-test: corrupt test memory and one FFT on purpose to show how errors are reported.
    pub inject: bool,
    pub problems: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            mode: None,
            passes: 1,
            burn_secs: None,
            cycles: 0,
            fft: (10..=20).collect(),
            tests: memtest::DEFAULT.to_vec(),
            fade_secs: 300,
            cpus: None,
            mem_limit: None,
            shutdown: false,
            debugcon: false,
            inject: false,
            problems: Vec::new(),
        }
    }
}

pub fn load() -> Config {
    let mut c = Config::default();
    let Ok(sfs) = boot::get_image_file_system(boot::image_handle()) else {
        return c;
    };
    let mut fs = FileSystem::new(sfs);
    let text =
        fs.read_to_string(cstr16!("\\EFI\\BOOT\\memburn.cfg")).or_else(|_| fs.read_to_string(cstr16!("\\memburn.cfg")));
    if let Ok(text) = text {
        c.cycles = 1;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if let Err(e) = apply(&mut c, line) {
                c.problems.push(format!("memburn.cfg: {e}"));
            }
        }
    }
    c
}

fn apply(c: &mut Config, line: &str) -> Result<(), String> {
    let (k, v) = line.split_once('=').ok_or_else(|| format!("expected key=value: '{line}'"))?;
    let (k, v) = (k.trim(), v.trim());
    let num = || v.parse::<u64>().map_err(|_| format!("{k}: not a number: '{v}'"));
    let flag = || Ok::<bool, String>(matches!(v, "1" | "yes" | "true" | "on"));
    match k {
        "mode" => {
            c.mode = Some(match v {
                "quick" => Mode::Quick,
                "mem" => Mode::Mem,
                "burn" => Mode::Burn,
                "both" => Mode::Both,
                "cores" => Mode::Cores,
                "stress" => {
                    c.problems.push(String::from("memburn.cfg: stress runs only from the Linux boot; running both"));
                    Mode::Both
                }
                _ => return Err(format!("mode: expected quick, mem, burn, both, stress or cores, got '{v}'")),
            })
        }
        "passes" => c.passes = num()?.max(1),
        "minutes" => c.burn_secs = Some(num()? * 60),
        "seconds" => c.burn_secs = Some(num()?),
        "cycles" => c.cycles = num()?,
        "fade_secs" => c.fade_secs = num()?,
        "cpus" => c.cpus = Some(num()?.max(1) as usize),
        "mem_mib" => c.mem_limit = Some(num()? << 20),
        "shutdown" => c.shutdown = flag()?,
        "debugcon" => c.debugcon = flag()?,
        "inject" => c.inject = flag()?,
        // Read by the Linux boot only: UEFI has no network, and GRUB passes early_memtest to the kernel.
        "upload" | "early_memtest" | "server" | "update" => {}
        "fft" => {
            c.fft = match v {
                "small" => (10..=15).collect(),
                "large" => (17..=20).collect(),
                "mixed" => (10..=20).collect(),
                list => list
                    .split(',')
                    .map(|s| match s.trim().parse::<u32>() {
                        Ok(n) if (memburn_core::fft::MIN_LOG2N..=20).contains(&n) => Ok(n),
                        _ => Err(format!("fft: sizes are log2 values 4..20, got '{s}'")),
                    })
                    .collect::<Result<_, _>>()?,
            }
        }
        "tests" => {
            c.tests = if v == "all" {
                memtest::ALL.to_vec()
            } else {
                v.split(',')
                    .map(|s| TestId::from_key(s.trim()).ok_or_else(|| format!("tests: unknown '{s}'")))
                    .collect::<Result<_, _>>()?
            }
        }
        _ => return Err(format!("unknown key '{k}'")),
    }
    Ok(())
}
