//! Word hash of the hashed-pattern test: integer multiply/xorshift rounds in inline asm (the UEFI target is soft-float).

use crate::cpu::Isa;

/// Mixing rounds per word.
pub const ROUNDS: u32 = 6;
const M1: u64 = 0x9E37_79B1;
const M2: u64 = 0x85EB_CA77;
const K: u64 = 0x632B_E59B_D9B4_E019;

/// One round: xorshift, then the two 32-bit halves multiplied (as `pmuludq` does), swapped and folded.
#[inline(always)]
fn round(x: u64) -> u64 {
    let x = x ^ (x >> 29);
    let lo = (x & 0xFFFF_FFFF) * M1;
    let hi = (x >> 32) * M2;
    (lo ^ hi.rotate_left(32)).wrapping_add(K)
}

/// The value the test writes at global word index `g`.
pub fn word(g: u64, key: u64) -> u64 {
    (0..ROUNDS).fold(g ^ key, |x, _| round(x))
}

/// Broadcast constants, read as aligned memory operands.
#[repr(C, align(32))]
struct Consts {
    m1: [u64; 4],
    m2: [u64; 4],
    k: [u64; 4],
}

static C: Consts = Consts { m1: [M1; 4], m2: [M2; 4], k: [K; 4] };

/// Kernels use only xmm0-5/ymm0-5 (volatile in the Win64 ABI); declared clobbered off UEFI.
#[cfg(not(target_os = "uefi"))]
macro_rules! simd_asm {
    ($($body:tt)*) => {
        core::arch::asm!($($body)*,
            out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _, out("xmm4") _, out("xmm5") _,
            options(nostack))
    };
}
#[cfg(target_os = "uefi")]
macro_rules! simd_asm {
    ($($body:tt)*) => { core::arch::asm!($($body)*, options(nostack)) };
}

#[cfg(not(target_os = "uefi"))]
macro_rules! avx_asm {
    ($($body:tt)*) => {
        core::arch::asm!($($body)*,
            out("ymm0") _, out("ymm1") _, out("ymm2") _, out("ymm3") _, out("ymm4") _, out("ymm5") _,
            options(nostack))
    };
}
#[cfg(target_os = "uefi")]
macro_rules! avx_asm {
    ($($body:tt)*) => { core::arch::asm!($($body)*, options(nostack)) };
}

/// `buf[k] = word(g + k, key)` for `k < buf.len()`.
pub fn words(isa: Isa, buf: &mut [u64], g: u64, key: u64) {
    for (k, v) in buf.iter_mut().enumerate() {
        *v = (g + k as u64) ^ key;
    }
    let lanes = match isa {
        Isa::Sse2 => 4,
        Isa::Avx2Fma => 8,
    };
    let simd = buf.len() / lanes * lanes;
    let (head, tail) = buf.split_at_mut(simd);
    if !head.is_empty() {
        let r = head.as_mut_ptr_range();
        unsafe {
            match isa {
                Isa::Sse2 => rounds_sse2(r.start, r.end),
                Isa::Avx2Fma => rounds_avx2(r.start, r.end),
            }
        }
    }
    for v in tail {
        *v = (0..ROUNDS).fold(*v, |x, _| round(x));
    }
}

/// [`ROUNDS`] rounds in place on `p..end`, four words per step; `end - p` is a multiple of 32 bytes.
unsafe fn rounds_sse2(p: *mut u64, end: *mut u64) {
    unsafe {
        simd_asm!(
            "3:",
            "movdqu xmm0, [{p}]",
            "movdqu xmm1, [{p} + 16]",
            "mov {r:e}, {rounds:e}",
            "2:",
            "movdqa xmm2, xmm0",
            "psrlq xmm2, 29",
            "pxor xmm0, xmm2",
            "movdqa xmm4, xmm1",
            "psrlq xmm4, 29",
            "pxor xmm1, xmm4",
            "movdqa xmm3, xmm0",
            "psrlq xmm3, 32",
            "movdqa xmm5, xmm1",
            "psrlq xmm5, 32",
            "pmuludq xmm0, [{c}]",
            "pmuludq xmm1, [{c}]",
            "pmuludq xmm3, [{c} + 32]",
            "pmuludq xmm5, [{c} + 32]",
            "movdqa xmm2, xmm3",
            "psllq xmm3, 32",
            "psrlq xmm2, 32",
            "por xmm3, xmm2",
            "pxor xmm0, xmm3",
            "movdqa xmm4, xmm5",
            "psllq xmm5, 32",
            "psrlq xmm4, 32",
            "por xmm5, xmm4",
            "pxor xmm1, xmm5",
            "paddq xmm0, [{c} + 64]",
            "paddq xmm1, [{c} + 64]",
            "dec {r:e}",
            "jnz 2b",
            "movdqu [{p}], xmm0",
            "movdqu [{p} + 16], xmm1",
            "add {p}, 32",
            "cmp {p}, {end}",
            "jb 3b",
            p = inout(reg) p => _,
            r = out(reg) _,
            end = in(reg) end,
            c = in(reg) core::ptr::addr_of!(C),
            rounds = in(reg) ROUNDS
        );
    }
}

/// [`ROUNDS`] rounds in place on `p..end`, eight words per step; `end - p` is a multiple of 64 bytes.
unsafe fn rounds_avx2(p: *mut u64, end: *mut u64) {
    unsafe {
        avx_asm!(
            "3:",
            "vmovdqu ymm0, [{p}]",
            "vmovdqu ymm1, [{p} + 32]",
            "mov {r:e}, {rounds:e}",
            "2:",
            "vpsrlq ymm2, ymm0, 29",
            "vpsrlq ymm4, ymm1, 29",
            "vpxor ymm0, ymm0, ymm2",
            "vpxor ymm1, ymm1, ymm4",
            "vpsrlq ymm3, ymm0, 32",
            "vpsrlq ymm5, ymm1, 32",
            "vpmuludq ymm0, ymm0, [{c}]",
            "vpmuludq ymm1, ymm1, [{c}]",
            "vpmuludq ymm3, ymm3, [{c} + 32]",
            "vpmuludq ymm5, ymm5, [{c} + 32]",
            "vpsrlq ymm2, ymm3, 32",
            "vpsrlq ymm4, ymm5, 32",
            "vpsllq ymm3, ymm3, 32",
            "vpsllq ymm5, ymm5, 32",
            "vpxor ymm0, ymm0, ymm2",
            "vpxor ymm1, ymm1, ymm4",
            "vpxor ymm0, ymm0, ymm3",
            "vpxor ymm1, ymm1, ymm5",
            "vpaddq ymm0, ymm0, [{c} + 64]",
            "vpaddq ymm1, ymm1, [{c} + 64]",
            "dec {r:e}",
            "jnz 2b",
            "vmovdqu [{p}], ymm0",
            "vmovdqu [{p} + 32], ymm1",
            "add {p}, 64",
            "cmp {p}, {end}",
            "jb 3b",
            "vzeroupper",
            p = inout(reg) p => _,
            r = out(reg) _,
            end = in(reg) end,
            c = in(reg) core::ptr::addr_of!(C),
            rounds = in(reg) ROUNDS
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simd_matches_scalar() {
        let mut isas = vec![Isa::Sse2];
        if crate::cpu::best_isa() == Isa::Avx2Fma {
            isas.push(Isa::Avx2Fma);
        }
        for isa in isas {
            for n in [0, 3, 8, 64, 77] {
                let mut buf = vec![0u64; n];
                words(isa, &mut buf, 1000, 0xDEAD_BEEF_0123_4567);
                let want: Vec<u64> = (0..n as u64).map(|k| word(1000 + k, 0xDEAD_BEEF_0123_4567)).collect();
                assert_eq!(buf, want, "{isa:?} n={n}");
            }
        }
    }

    #[test]
    fn every_bit_takes_both_values() {
        let (mut ones, mut zeros) = (0u64, 0u64);
        for g in 0..64 {
            let w = word(g, 7);
            ones |= w;
            zeros |= !w;
        }
        assert_eq!((ones, zeros), (!0, !0));
        assert_ne!(word(5, 1), word(5, 2));
    }
}
