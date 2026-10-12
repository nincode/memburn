//! CPU temperature on the BSP: AMD SMN through the root complex index pair, Intel thermal MSRs.

use core::arch::asm;

use memburn_core::cpu;
use memburn_core::thermal::{self, AMD_SMN_TEMP, MSR_PACKAGE_THERM_STATUS, MSR_TEMPERATURE_TARGET, MSR_THERM_STATUS};

/// Root complex 00:00.0 SMN index and data registers.
const SMN_INDEX: u8 = 0x60;
const SMN_DATA: u8 = 0x64;
const AMD_PCI_VENDOR: u32 = 0x1022;

pub enum Sensor {
    Amd { family: u32, brand_buf: [u8; 48] },
    Intel { status_msr: u32, need_valid: bool, target: u64 },
}

fn outl(port: u16, v: u32) {
    unsafe { asm!("out dx, eax", in("dx") port, in("eax") v, options(nomem, nostack)) };
}

fn inl(port: u16) -> u32 {
    let v: u32;
    unsafe { asm!("in eax, dx", in("dx") port, out("eax") v, options(nomem, nostack)) };
    v
}

/// PCI configuration mechanism 1 address for bus 0, device 0, function 0.
fn root_address(reg: u8) -> u32 {
    0x8000_0000 | (reg as u32 & 0xFC)
}

fn root_read(reg: u8) -> u32 {
    outl(0xCF8, root_address(reg));
    inl(0xCFC)
}

fn root_write(reg: u8, v: u32) {
    outl(0xCF8, root_address(reg));
    outl(0xCFC, v);
}

fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    unsafe { asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack)) };
    (hi as u64) << 32 | lo as u64
}

impl Sensor {
    /// The CPU's sensor when the vendor and feature checks pass; None on hypervisors without one.
    pub fn find() -> Option<Sensor> {
        let (_, b, c, d) = cpu::cpuid(0, 0);
        let mut vendor = [0u8; 12];
        for (k, r) in [b, d, c].into_iter().enumerate() {
            vendor[k * 4..k * 4 + 4].copy_from_slice(&r.to_le_bytes());
        }
        let (eax1, ..) = cpu::cpuid(1, 0);
        let family = thermal::family(eax1);
        match &vendor {
            b"AuthenticAMD" | b"HygonGenuine" if family >= 0x17 => {
                (root_read(0) & 0xFFFF == AMD_PCI_VENDOR).then(|| {
                    let mut brand_buf = [0u8; 48];
                    cpu::brand(&mut brand_buf);
                    Sensor::Amd { family, brand_buf }
                })
            }
            b"GenuineIntel" => {
                let (max, ..) = cpu::cpuid(0, 0);
                if max < 6 {
                    return None;
                }
                let (a6, ..) = cpu::cpuid(6, 0);
                let target = rdmsr(MSR_TEMPERATURE_TARGET);
                if a6 & (1 << 6) != 0 {
                    Some(Sensor::Intel { status_msr: MSR_PACKAGE_THERM_STATUS, need_valid: false, target })
                } else if a6 & 1 != 0 {
                    Some(Sensor::Intel { status_msr: MSR_THERM_STATUS, need_valid: true, target })
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    pub fn read(&self) -> Option<i32> {
        match self {
            Sensor::Amd { family, brand_buf } => {
                root_write(SMN_INDEX, AMD_SMN_TEMP);
                let reg = root_read(SMN_DATA);
                let end = brand_buf.iter().position(|&b| b == 0).unwrap_or(48);
                let brand = core::str::from_utf8(&brand_buf[..end]).unwrap_or("");
                Some(thermal::amd_temp_c(reg, *family, brand))
            }
            Sensor::Intel { status_msr, need_valid, target } => {
                thermal::intel_temp_c(rdmsr(*status_msr), *target, *need_valid)
            }
        }
    }
}
