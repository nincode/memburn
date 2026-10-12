use core::arch::asm;
#[allow(unused_imports)]
use core::arch::x86_64::{__cpuid, __cpuid_count, _rdtsc};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Isa {
    Sse2,
    Avx2Fma,
}

impl Isa {
    pub fn name(self) -> &'static str {
        match self {
            Isa::Sse2 => "SSE2",
            Isa::Avx2Fma => "AVX2+FMA",
        }
    }
}

#[allow(unused_unsafe)]
pub fn cpuid(leaf: u32, sub: u32) -> (u32, u32, u32, u32) {
    let r = unsafe { __cpuid_count(leaf, sub) };
    (r.eax, r.ebx, r.ecx, r.edx)
}

/// The CPU implements AVX2 and FMA3 (whether or not the OS has enabled AVX state).
pub fn has_avx2_fma() -> bool {
    let (max, ..) = cpuid(0, 0);
    if max < 7 {
        return false;
    }
    let (_, _, c1, _) = cpuid(1, 0);
    let (_, b7, _, _) = cpuid(7, 0);
    let fma = c1 & (1 << 12) != 0;
    let xsave = c1 & (1 << 26) != 0;
    let avx = c1 & (1 << 28) != 0;
    let avx2 = b7 & (1 << 5) != 0;
    fma && xsave && avx && avx2
}

/// CR4.OSXSAVE is set and XCR0 enables SSE and AVX state.
pub fn avx_enabled() -> bool {
    let (_, _, c1, _) = cpuid(1, 0);
    c1 & (1 << 27) != 0 && xgetbv0() & 0b110 == 0b110
}

pub fn best_isa() -> Isa {
    if has_avx2_fma() && avx_enabled() { Isa::Avx2Fma } else { Isa::Sse2 }
}

/// Enables SSE and, if present, AVX state via CR0/CR4/XCR0; ring 0 only (UEFI), once per CPU.
pub unsafe fn enable_simd() {
    unsafe {
        let mut cr0: u64;
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack));
        cr0 = (cr0 & !(1 << 2)) | (1 << 1);
        asm!("mov cr0, {}", in(reg) cr0, options(nostack));
        let mut cr4: u64;
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
        cr4 |= (1 << 9) | (1 << 10);
        let (_, _, c1, _) = cpuid(1, 0);
        if c1 & (1 << 26) != 0 {
            cr4 |= 1 << 18;
        }
        asm!("mov cr4, {}", in(reg) cr4, options(nostack));
        if c1 & (1 << 26) != 0 {
            let (supported, ..) = cpuid(0xD, 0);
            let want = (xgetbv0() | 0b111) & (supported as u64 | 0b11);
            asm!("xsetbv", in("ecx") 0, in("eax") want as u32, in("edx") (want >> 32) as u32, options(nostack));
        }
    }
}

pub fn xgetbv0() -> u64 {
    let (lo, hi): (u32, u32);
    unsafe { asm!("xgetbv", in("ecx") 0, out("eax") lo, out("edx") hi, options(nomem, nostack)) };
    ((hi as u64) << 32) | lo as u64
}

#[allow(unused_unsafe)]
pub fn rdtsc() -> u64 {
    unsafe { _rdtsc() }
}

/// CPUID brand string, trimmed.
pub fn brand(buf: &mut [u8; 48]) -> &str {
    let (max, ..) = cpuid(0x8000_0000, 0);
    if max < 0x8000_0004 {
        return "unknown CPU";
    }
    for (k, leaf) in (0x8000_0002u32..=0x8000_0004).enumerate() {
        let (a, b, c, d) = cpuid(leaf, 0);
        for (r, v) in [a, b, c, d].into_iter().enumerate() {
            buf[k * 16 + r * 4..k * 16 + r * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(48);
    core::str::from_utf8(&buf[..end]).unwrap_or("unknown CPU").trim()
}
