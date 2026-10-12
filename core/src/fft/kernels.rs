//! Inline-asm FFT kernels (the UEFI target is soft-float); split-complex arrays, lengths a multiple of 4.

use crate::cpu::Isa;

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

/// 1.5 * 2^52: adding it to |v| < 2^51 rounds v to an integer held in the low mantissa bits.
pub const MAGIC: u64 = 0x4338_0000_0000_0000;
const ABS_MASK: u64 = 0x7FFF_FFFF_FFFF_FFFF;

/// DIF stage, span `h`: (a, b) -> (a + b, (a - b)·w[j]); needs `h >= 2` (SSE2) or `h >= 4` (AVX2).
pub unsafe fn dif(isa: Isa, re: *mut f64, im: *mut f64, wr: *const f64, wi: *const f64, h: usize, n: usize) {
    unsafe {
        match isa {
            Isa::Sse2 => dif_sse2(re, im, wr, wi, h * 8, re.add(n)),
            Isa::Avx2Fma => dif_avx2(re, im, wr, wi, h * 8, re.add(n)),
        }
    }
}

/// DIT stage, inverse of [`dif`] up to 2: t = b·conj(w[j]); (a, b) -> (a + t, a - t).
pub unsafe fn dit(isa: Isa, re: *mut f64, im: *mut f64, wr: *const f64, wi: *const f64, h: usize, n: usize) {
    unsafe {
        match isa {
            Isa::Sse2 => dit_sse2(re, im, wr, wi, h * 8, re.add(n)),
            Isa::Avx2Fma => dit_avx2(re, im, wr, wi, h * 8, re.add(n)),
        }
    }
}

/// Broadcast constants for [`round_check`], read as memory operands.
#[repr(C, align(32))]
struct Consts {
    magic: [u64; 4],
    scale: [u64; 4],
    abs: [u64; 4],
}

// Twiddle operands are 16-byte aligned: Buf is 64-aligned and h >= 2.
unsafe fn dif_sse2(re: *mut f64, im: *mut f64, wr: *const f64, wi: *const f64, hb: usize, end: *mut f64) {
    unsafe {
        simd_asm!(
            "3:",
            "lea {qr}, [{pr} + {hb}]",
            "lea {qi}, [{pi} + {hb}]",
            "xor {j}, {j}",
            "2:",
            "movupd xmm0, [{pr} + {j}]",
            "movupd xmm1, [{pi} + {j}]",
            "movupd xmm2, [{qr} + {j}]",
            "movupd xmm3, [{qi} + {j}]",
            "movapd xmm4, xmm0",
            "addpd xmm4, xmm2",
            "movapd xmm5, xmm1",
            "addpd xmm5, xmm3",
            "movupd [{pr} + {j}], xmm4",
            "movupd [{pi} + {j}], xmm5",
            "subpd xmm0, xmm2",
            "subpd xmm1, xmm3",
            "movapd xmm2, xmm0",
            "mulpd xmm2, [{wr} + {j}]",
            "movapd xmm3, xmm1",
            "mulpd xmm3, [{wi} + {j}]",
            "subpd xmm2, xmm3",
            "mulpd xmm0, [{wi} + {j}]",
            "mulpd xmm1, [{wr} + {j}]",
            "addpd xmm0, xmm1",
            "movupd [{qr} + {j}], xmm2",
            "movupd [{qi} + {j}], xmm0",
            "add {j}, 16",
            "cmp {j}, {hb}",
            "jb 2b",
            "lea {pr}, [{qr} + {hb}]",
            "lea {pi}, [{qi} + {hb}]",
            "cmp {pr}, {end}",
            "jb 3b",
            pr = inout(reg) re => _,
            pi = inout(reg) im => _,
            qr = out(reg) _,
            qi = out(reg) _,
            j = out(reg) _,
            wr = in(reg) wr,
            wi = in(reg) wi,
            hb = in(reg) hb,
            end = in(reg) end
        );
    }
}

unsafe fn dit_sse2(re: *mut f64, im: *mut f64, wr: *const f64, wi: *const f64, hb: usize, end: *mut f64) {
    unsafe {
        simd_asm!(
            "3:",
            "lea {qr}, [{pr} + {hb}]",
            "lea {qi}, [{pi} + {hb}]",
            "xor {j}, {j}",
            "2:",
            "movupd xmm0, [{pr} + {j}]",
            "movupd xmm1, [{pi} + {j}]",
            "movupd xmm2, [{qr} + {j}]",
            "movupd xmm3, [{qi} + {j}]",
            "movapd xmm4, xmm2",
            "mulpd xmm4, [{wr} + {j}]",
            "movapd xmm5, xmm3",
            "mulpd xmm5, [{wi} + {j}]",
            "addpd xmm4, xmm5",
            "mulpd xmm3, [{wr} + {j}]",
            "mulpd xmm2, [{wi} + {j}]",
            "subpd xmm3, xmm2",
            "movapd xmm5, xmm0",
            "addpd xmm0, xmm4",
            "subpd xmm5, xmm4",
            "movapd xmm2, xmm1",
            "addpd xmm1, xmm3",
            "subpd xmm2, xmm3",
            "movupd [{pr} + {j}], xmm0",
            "movupd [{pi} + {j}], xmm1",
            "movupd [{qr} + {j}], xmm5",
            "movupd [{qi} + {j}], xmm2",
            "add {j}, 16",
            "cmp {j}, {hb}",
            "jb 2b",
            "lea {pr}, [{qr} + {hb}]",
            "lea {pi}, [{qi} + {hb}]",
            "cmp {pr}, {end}",
            "jb 3b",
            pr = inout(reg) re => _,
            pi = inout(reg) im => _,
            qr = out(reg) _,
            qi = out(reg) _,
            j = out(reg) _,
            wr = in(reg) wr,
            wi = in(reg) wi,
            hb = in(reg) hb,
            end = in(reg) end
        );
    }
}

#[target_feature(enable = "avx,avx2,fma")]
unsafe fn dif_avx2(re: *mut f64, im: *mut f64, wr: *const f64, wi: *const f64, hb: usize, end: *mut f64) {
    unsafe {
        avx_asm!(
            "3:",
            "lea {qr}, [{pr} + {hb}]",
            "lea {qi}, [{pi} + {hb}]",
            "xor {j}, {j}",
            "2:",
            "vmovupd ymm0, [{pr} + {j}]",
            "vmovupd ymm1, [{pi} + {j}]",
            "vmovupd ymm2, [{qr} + {j}]",
            "vmovupd ymm3, [{qi} + {j}]",
            "vaddpd ymm4, ymm0, ymm2",
            "vaddpd ymm5, ymm1, ymm3",
            "vsubpd ymm0, ymm0, ymm2",
            "vsubpd ymm1, ymm1, ymm3",
            "vmovupd [{pr} + {j}], ymm4",
            "vmovupd [{pi} + {j}], ymm5",
            "vmulpd ymm2, ymm1, [{wi} + {j}]",
            "vfmsub231pd ymm2, ymm0, [{wr} + {j}]",
            "vmulpd ymm3, ymm1, [{wr} + {j}]",
            "vfmadd231pd ymm3, ymm0, [{wi} + {j}]",
            "vmovupd [{qr} + {j}], ymm2",
            "vmovupd [{qi} + {j}], ymm3",
            "add {j}, 32",
            "cmp {j}, {hb}",
            "jb 2b",
            "lea {pr}, [{qr} + {hb}]",
            "lea {pi}, [{qi} + {hb}]",
            "cmp {pr}, {end}",
            "jb 3b",
            "vzeroupper",
            pr = inout(reg) re => _,
            pi = inout(reg) im => _,
            qr = out(reg) _,
            qi = out(reg) _,
            j = out(reg) _,
            wr = in(reg) wr,
            wi = in(reg) wi,
            hb = in(reg) hb,
            end = in(reg) end
        );
    }
}

#[target_feature(enable = "avx,avx2,fma")]
unsafe fn dit_avx2(re: *mut f64, im: *mut f64, wr: *const f64, wi: *const f64, hb: usize, end: *mut f64) {
    unsafe {
        avx_asm!(
            "3:",
            "lea {qr}, [{pr} + {hb}]",
            "lea {qi}, [{pi} + {hb}]",
            "xor {j}, {j}",
            "2:",
            "vmovupd ymm0, [{pr} + {j}]",
            "vmovupd ymm1, [{pi} + {j}]",
            "vmovupd ymm2, [{qr} + {j}]",
            "vmovupd ymm3, [{qi} + {j}]",
            "vmulpd ymm4, ymm3, [{wi} + {j}]",
            "vfmadd231pd ymm4, ymm2, [{wr} + {j}]",
            "vmulpd ymm5, ymm2, [{wi} + {j}]",
            "vfmsub231pd ymm5, ymm3, [{wr} + {j}]",
            "vaddpd ymm2, ymm0, ymm4",
            "vsubpd ymm0, ymm0, ymm4",
            "vaddpd ymm3, ymm1, ymm5",
            "vsubpd ymm1, ymm1, ymm5",
            "vmovupd [{pr} + {j}], ymm2",
            "vmovupd [{pi} + {j}], ymm3",
            "vmovupd [{qr} + {j}], ymm0",
            "vmovupd [{qi} + {j}], ymm1",
            "add {j}, 32",
            "cmp {j}, {hb}",
            "jb 2b",
            "lea {pr}, [{qr} + {hb}]",
            "lea {pi}, [{qi} + {hb}]",
            "cmp {pr}, {end}",
            "jb 3b",
            "vzeroupper",
            pr = inout(reg) re => _,
            pi = inout(reg) im => _,
            qr = out(reg) _,
            qi = out(reg) _,
            j = out(reg) _,
            wr = in(reg) wr,
            wi = in(reg) wi,
            hb = in(reg) hb,
            end = in(reg) end
        );
    }
}

/// Span-1 stage for DIF and DIT: adjacent pairs (a, b) -> (a + b, a - b).
pub unsafe fn pairs(p: *mut f64, n: usize) {
    unsafe {
        simd_asm!(
            "xor {k}, {k}",
            "2:",
            "movupd xmm0, [{p} + {k}]",
            "movapd xmm1, xmm0",
            "shufpd xmm1, xmm1, 1",
            "movapd xmm2, xmm0",
            "addpd xmm2, xmm1",
            "subpd xmm0, xmm1",
            "unpcklpd xmm2, xmm0",
            "movupd [{p} + {k}], xmm2",
            "add {k}, 16",
            "cmp {k}, {nb}",
            "jb 2b",
            p = in(reg) p,
            nb = in(reg) n * 8,
            k = out(reg) _
        );
    }
}

/// Pointwise complex square: (r, i) -> (r^2 - i^2, 2ri).
pub unsafe fn square(isa: Isa, re: *mut f64, im: *mut f64, n: usize) {
    unsafe {
        match isa {
            Isa::Sse2 => simd_asm!(
                "xor {k}, {k}",
                "2:",
                "movupd xmm0, [{re} + {k}]",
                "movupd xmm1, [{im} + {k}]",
                "movapd xmm2, xmm0",
                "mulpd xmm2, xmm0",
                "movapd xmm3, xmm1",
                "mulpd xmm3, xmm1",
                "subpd xmm2, xmm3",
                "mulpd xmm0, xmm1",
                "addpd xmm0, xmm0",
                "movupd [{re} + {k}], xmm2",
                "movupd [{im} + {k}], xmm0",
                "add {k}, 16",
                "cmp {k}, {nb}",
                "jb 2b",
                re = in(reg) re,
                im = in(reg) im,
                nb = in(reg) n * 8,
                k = out(reg) _
            ),
            Isa::Avx2Fma => square_avx2(re, im, n),
        }
    }
}

#[target_feature(enable = "avx,avx2,fma")]
unsafe fn square_avx2(re: *mut f64, im: *mut f64, n: usize) {
    unsafe {
        avx_asm!(
            "xor {k}, {k}",
            "2:",
            "vmovupd ymm0, [{re} + {k}]",
            "vmovupd ymm1, [{im} + {k}]",
            "vmulpd ymm2, ymm0, ymm0",
            "vfnmadd231pd ymm2, ymm1, ymm1",
            "vmulpd ymm0, ymm0, ymm1",
            "vaddpd ymm0, ymm0, ymm0",
            "vmovupd [{re} + {k}], ymm2",
            "vmovupd [{im} + {k}], ymm0",
            "add {k}, 32",
            "cmp {k}, {nb}",
            "jb 2b",
            "vzeroupper",
            re = in(reg) re,
            im = in(reg) im,
            nb = in(reg) n * 8,
            k = out(reg) _
        );
    }
}

/// x -> x - 1.5·2^52: [`MAGIC`]-encoded integers to plain doubles.
pub unsafe fn unmagic(p: *mut f64, n: usize) {
    unsafe {
        simd_asm!(
            "movq xmm1, {magic}",
            "unpcklpd xmm1, xmm1",
            "xor {k}, {k}",
            "2:",
            "movupd xmm0, [{p} + {k}]",
            "subpd xmm0, xmm1",
            "movupd [{p} + {k}], xmm0",
            "add {k}, 16",
            "cmp {k}, {nb}",
            "jb 2b",
            p = in(reg) p,
            nb = in(reg) n * 8,
            magic = in(reg) MAGIC,
            k = out(reg) _
        );
    }
}

/// Scales, rounds real parts to [`MAGIC`] integers; returns (max rounding or |imag| as f64 bits, non-finite).
pub unsafe fn round_check(isa: Isa, re: *mut f64, im: *const f64, n: usize, scale: u64) -> (u64, bool) {
    let c = Consts { magic: [MAGIC; 4], scale: [scale; 4], abs: [ABS_MASK; 4] };
    let (max, bad): (u64, u64);
    unsafe {
        match isa {
            Isa::Sse2 => simd_asm!(
                "xorpd xmm4, xmm4",
                "xorpd xmm5, xmm5",
                "xor {k}, {k}",
                "2:",
                "movupd xmm0, [{re} + {k}]",
                "mulpd xmm0, [{c} + 32]",
                "movapd xmm1, xmm0",
                "addpd xmm1, [{c}]",
                "movupd [{re} + {k}], xmm1",
                "subpd xmm1, [{c}]",
                "subpd xmm0, xmm1",
                "andpd xmm0, [{c} + 64]",
                "movupd xmm2, [{im} + {k}]",
                "mulpd xmm2, [{c} + 32]",
                "andpd xmm2, [{c} + 64]",
                "movapd xmm3, xmm0",
                "cmpunordpd xmm3, xmm2",
                "orpd xmm5, xmm3",
                "maxpd xmm0, xmm2",
                "maxpd xmm4, xmm0",
                "add {k}, 16",
                "cmp {k}, {nb}",
                "jb 2b",
                "movapd xmm0, xmm4",
                "unpckhpd xmm0, xmm0",
                "maxpd xmm4, xmm0",
                "movq {max}, xmm4",
                "movapd xmm0, xmm5",
                "unpckhpd xmm0, xmm0",
                "orpd xmm5, xmm0",
                "movq {bad}, xmm5",
                re = in(reg) re,
                im = in(reg) im,
                nb = in(reg) n * 8,
                c = in(reg) &c,
                k = out(reg) _,
                max = out(reg) max,
                bad = out(reg) bad
            ),
            Isa::Avx2Fma => (max, bad) = round_check_avx2(re, im, n, &c),
        }
    }
    (max, bad != 0)
}

#[target_feature(enable = "avx,avx2,fma")]
unsafe fn round_check_avx2(re: *mut f64, im: *const f64, n: usize, c: &Consts) -> (u64, u64) {
    let (max, bad): (u64, u64);
    unsafe {
        avx_asm!(
            "vxorpd ymm4, ymm4, ymm4",
            "vxorpd ymm5, ymm5, ymm5",
            "xor {k}, {k}",
            "2:",
            "vmovupd ymm0, [{re} + {k}]",
            "vmulpd ymm0, ymm0, [{c} + 32]",
            "vaddpd ymm1, ymm0, [{c}]",
            "vmovupd [{re} + {k}], ymm1",
            "vsubpd ymm1, ymm1, [{c}]",
            "vsubpd ymm0, ymm0, ymm1",
            "vandpd ymm0, ymm0, [{c} + 64]",
            "vmovupd ymm2, [{im} + {k}]",
            "vmulpd ymm2, ymm2, [{c} + 32]",
            "vandpd ymm2, ymm2, [{c} + 64]",
            "vcmpunordpd ymm3, ymm0, ymm2",
            "vorpd ymm5, ymm5, ymm3",
            "vmaxpd ymm0, ymm0, ymm2",
            "vmaxpd ymm4, ymm4, ymm0",
            "add {k}, 32",
            "cmp {k}, {nb}",
            "jb 2b",
            "vextractf128 xmm0, ymm4, 1",
            "vmaxpd xmm4, xmm4, xmm0",
            "vunpckhpd xmm0, xmm4, xmm4",
            "vmaxpd xmm4, xmm4, xmm0",
            "vmovq {max}, xmm4",
            "vextractf128 xmm0, ymm5, 1",
            "vorpd xmm5, xmm5, xmm0",
            "vunpckhpd xmm0, xmm5, xmm5",
            "vorpd xmm5, xmm5, xmm0",
            "vmovq {bad}, xmm5",
            "vzeroupper",
            re = in(reg) re,
            im = in(reg) im,
            nb = in(reg) n * 8,
            c = in(reg) c,
            k = out(reg) _,
            max = out(reg) max,
            bad = out(reg) bad
        );
    }
    (max, bad)
}
