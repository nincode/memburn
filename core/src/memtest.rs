use alloc::format;
use alloc::string::String;

use crate::cpu::Isa;
use crate::errlog::{ErrorLog, MemError};
use crate::hash;
use crate::rng::splitmix64;

/// Word-addressed view of the memory under test; generic so unit tests can inject faults.
#[allow(clippy::len_without_is_empty)]
pub trait Words {
    fn len(&self) -> usize;
    fn get(&self, i: usize) -> u64;
    fn set(&mut self, i: usize, v: u64);
    fn addr(&self, i: usize) -> usize;
    /// memmove of `n` words from index `src` to index `dst`.
    fn copy(&mut self, src: usize, dst: usize, n: usize);
}

pub struct RawWords {
    ptr: *mut u64,
    len: usize,
}

impl RawWords {
    /// `ptr..ptr+len` must be valid, exclusively owned, 8-byte aligned memory.
    pub unsafe fn new(ptr: *mut u64, len: usize) -> Self {
        RawWords { ptr, len }
    }
}

impl Words for RawWords {
    #[inline(always)]
    fn len(&self) -> usize {
        self.len
    }
    #[inline(always)]
    fn get(&self, i: usize) -> u64 {
        debug_assert!(i < self.len);
        unsafe { self.ptr.add(i).read_volatile() }
    }
    #[inline(always)]
    fn set(&mut self, i: usize, v: u64) {
        debug_assert!(i < self.len);
        unsafe { self.ptr.add(i).write_volatile(v) }
    }
    #[inline(always)]
    fn addr(&self, i: usize) -> usize {
        self.ptr as usize + i * 8
    }
    fn copy(&mut self, src: usize, dst: usize, n: usize) {
        debug_assert!(src + n <= self.len && dst + n <= self.len);
        unsafe { core::ptr::copy(self.ptr.add(src), self.ptr.add(dst), n) }
    }
}

pub struct Ctx<'a> {
    pub pass: u64,
    /// Global word index of word 0 of this chunk across all memory under test.
    pub base: u64,
    pub test: TestId,
    pub log: &'a ErrorLog,
    /// Kernel set for computed patterns.
    pub isa: Isa,
}

impl Ctx<'_> {
    #[inline(always)]
    fn check<W: Words>(&self, m: &W, i: usize, expected: u64) {
        let actual = m.get(i);
        if actual != expected {
            self.fail(m.addr(i), expected, actual);
        }
    }

    #[cold]
    #[inline(never)]
    fn fail(&self, addr: usize, expected: u64, actual: u64) {
        self.log.record(MemError { addr, expected, actual, test: self.test as u8, pass: self.pass as u32 });
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum TestId {
    AddressOwn,
    MovingInvOnes,
    MovingInvWalk8,
    MovingInvRandom,
    MovingInvWalk64,
    BlockMove,
    RandomSeq,
    Modulo20,
    BitFade,
    AddressMove,
    Hashed,
}

pub const ALL: [TestId; 11] = [
    TestId::AddressOwn,
    TestId::MovingInvOnes,
    TestId::MovingInvWalk8,
    TestId::MovingInvRandom,
    TestId::MovingInvWalk64,
    TestId::BlockMove,
    TestId::RandomSeq,
    TestId::Modulo20,
    TestId::BitFade,
    TestId::AddressMove,
    TestId::Hashed,
];

/// Full memory test.
pub const DEFAULT: [TestId; 9] = [
    TestId::AddressOwn,
    TestId::MovingInvOnes,
    TestId::MovingInvWalk8,
    TestId::MovingInvRandom,
    TestId::MovingInvWalk64,
    TestId::BlockMove,
    TestId::RandomSeq,
    TestId::Modulo20,
    TestId::Hashed,
];

/// Memory side of the stress mode.
pub const STRESS: [TestId; 8] = [
    TestId::AddressOwn,
    TestId::MovingInvOnes,
    TestId::MovingInvWalk8,
    TestId::MovingInvRandom,
    TestId::MovingInvWalk64,
    TestId::BlockMove,
    TestId::RandomSeq,
    TestId::Modulo20,
];

/// Quick check as (test, rounds): address lines with block moves, the 8-bit walk, five rounds of random data, hashed words.
pub const QUICK: [(TestId, u32); 4] =
    [(TestId::AddressMove, 1), (TestId::MovingInvWalk8, 1), (TestId::RandomSeq, 5), (TestId::Hashed, 1)];

/// Pass value for round `round` of a repeated test: new seeds, same reported pass (errors keep the low 32 bits).
pub fn round_pass(pass: u64, round: u32) -> u64 {
    pass | (round as u64) << 32
}

/// Memory sweeps in one pass over (test, rounds).
pub fn sweeps(tests: &[(TestId, u32)]) -> u64 {
    tests.iter().map(|&(t, r)| t.phases() as u64 * r as u64).sum()
}

/// "Random sequence ×5", or the plain name for one round.
pub fn label(t: TestId, rounds: u32) -> String {
    if rounds > 1 { format!("{} ×{rounds}", t.name()) } else { String::from(t.name()) }
}

const WALK8: u64 = 0x0101_0101_0101_0101;

impl TestId {
    pub fn from_u8(v: u8) -> Option<TestId> {
        ALL.get(v as usize).copied()
    }

    pub fn key(self) -> &'static str {
        match self {
            TestId::AddressOwn => "addr",
            TestId::MovingInvOnes => "mi01",
            TestId::MovingInvWalk8 => "mi8",
            TestId::MovingInvRandom => "mirand",
            TestId::MovingInvWalk64 => "mi64",
            TestId::BlockMove => "block",
            TestId::RandomSeq => "random",
            TestId::Modulo20 => "mod20",
            TestId::BitFade => "fade",
            TestId::AddressMove => "addrmove",
            TestId::Hashed => "hash",
        }
    }

    pub fn from_key(key: &str) -> Option<TestId> {
        ALL.iter().copied().find(|t| t.key() == key)
    }

    pub fn name(self) -> &'static str {
        match self {
            TestId::AddressOwn => "Own address",
            TestId::MovingInvOnes => "Moving inversions, 0s/1s",
            TestId::MovingInvWalk8 => "Moving inversions, 8-bit walk",
            TestId::MovingInvRandom => "Moving inversions, random",
            TestId::MovingInvWalk64 => "Moving inversions, 64-bit walk",
            TestId::BlockMove => "Block move",
            TestId::RandomSeq => "Random sequence",
            TestId::Modulo20 => "Modulo 20",
            TestId::BitFade => "Bit fade",
            TestId::AddressMove => "Address + block moves",
            TestId::Hashed => "Hashed pattern",
        }
    }

    pub fn phases(self) -> usize {
        match self {
            TestId::AddressOwn => 4,
            TestId::MovingInvOnes => 6,
            TestId::MovingInvWalk8 => 24,
            TestId::MovingInvRandom => 6,
            TestId::MovingInvWalk64 => 6,
            TestId::BlockMove => 5,
            TestId::RandomSeq => 3,
            TestId::Modulo20 => 60,
            TestId::BitFade => 4,
            TestId::AddressMove => 5,
            TestId::Hashed => 2,
        }
    }

    /// Phase runs high to low addresses, chunk order included.
    pub fn descending(self, phase: usize) -> bool {
        match self {
            TestId::AddressOwn => phase >= 2,
            TestId::MovingInvOnes | TestId::MovingInvWalk8 | TestId::MovingInvRandom | TestId::MovingInvWalk64 => {
                phase % 3 == 2
            }
            _ => false,
        }
    }

    /// Bytes moved on the memory bus per tested byte in this phase, as (num, den); stores count a read-for-ownership.
    pub fn traffic(self, phase: usize) -> (u64, u64) {
        const WRITE: (u64, u64) = (2, 1);
        const READ: (u64, u64) = (1, 1);
        const READ_WRITE: (u64, u64) = (2, 1);
        const COPY_HALF: (u64, u64) = (3, 2);
        match self {
            TestId::AddressOwn | TestId::BitFade | TestId::Hashed => {
                if phase.is_multiple_of(2) {
                    WRITE
                } else {
                    READ
                }
            }
            TestId::MovingInvOnes | TestId::MovingInvWalk8 | TestId::MovingInvRandom | TestId::MovingInvWalk64 => {
                if phase.is_multiple_of(3) {
                    WRITE
                } else {
                    READ_WRITE
                }
            }
            TestId::BlockMove => match phase {
                0 => WRITE,
                1 => COPY_HALF,
                3 => READ_WRITE,
                _ => READ,
            },
            TestId::AddressMove => match phase {
                0 => WRITE,
                1 | 3 => COPY_HALF,
                _ => READ,
            },
            TestId::RandomSeq => match phase {
                0 => WRITE,
                1 => READ_WRITE,
                _ => READ,
            },
            // One word in 20 (160 bytes) touches one 64-byte line.
            TestId::Modulo20 => match phase % 3 {
                0 => (4, 5),
                1 => (4, 1),
                _ => (2, 5),
            },
        }
    }

    /// The platform waits the bit-fade delay after this phase.
    pub fn pause_after(self, phase: usize) -> bool {
        self == TestId::BitFade && (phase == 0 || phase == 2)
    }

    pub fn run<W: Words>(self, phase: usize, m: &mut W, ctx: &Ctx) {
        let pass = ctx.pass;
        match self {
            TestId::AddressOwn => address_own(phase, m, ctx),
            TestId::MovingInvOnes => {
                let p = if phase / 3 == 0 { 0 } else { !0 };
                moving_inv(phase % 3, m, ctx, |_| p)
            }
            TestId::MovingInvWalk8 => {
                let p = WALK8 << (phase / 3);
                moving_inv(phase % 3, m, ctx, |_| p)
            }
            TestId::MovingInvRandom => {
                let p = splitmix64(pass.wrapping_mul(2).wrapping_add((phase / 3) as u64) ^ 0x5EED_0001);
                moving_inv(phase % 3, m, ctx, |_| p)
            }
            TestId::MovingInvWalk64 => {
                let inv = if phase / 3 == 0 { 0 } else { !0 };
                moving_inv(phase % 3, m, ctx, |g| 1u64.rotate_left((g.wrapping_add(pass) & 63) as u32) ^ inv)
            }
            TestId::BlockMove => block_move(phase, m, ctx),
            TestId::AddressMove => address_move(phase, m, ctx),
            TestId::RandomSeq => random_seq(phase, m, ctx),
            TestId::Modulo20 => modulo20(phase, m, ctx),
            TestId::Hashed => hashed(phase, m, ctx),
            TestId::BitFade => {
                let p = if phase < 2 { 0 } else { !0 };
                let n = m.len();
                if phase.is_multiple_of(2) {
                    for i in 0..n {
                        m.set(i, p);
                    }
                } else {
                    for i in 0..n {
                        ctx.check(m, i, p);
                    }
                }
            }
        }
    }
}

fn address_own<W: Words>(phase: usize, m: &mut W, ctx: &Ctx) {
    let n = m.len();
    let inv = if ctx.pass & 1 == 0 { 0 } else { !0 };
    match phase {
        0 => (0..n).for_each(|i| m.set(i, m.addr(i) as u64 ^ inv)),
        1 => (0..n).for_each(|i| ctx.check(m, i, m.addr(i) as u64 ^ inv)),
        2 => (0..n).rev().for_each(|i| m.set(i, !(m.addr(i) as u64) ^ inv)),
        _ => (0..n).rev().for_each(|i| ctx.check(m, i, !(m.addr(i) as u64) ^ inv)),
    }
}

/// Moving inversions: fill p; ascending check p, write !p; descending check !p, write p.
#[inline(always)]
fn moving_inv<W: Words>(sub: usize, m: &mut W, ctx: &Ctx, pat: impl Fn(u64) -> u64) {
    let n = m.len();
    let b = ctx.base;
    match sub {
        0 => {
            for i in 0..n {
                m.set(i, pat(b + i as u64));
            }
        }
        1 => {
            for i in 0..n {
                let p = pat(b + i as u64);
                ctx.check(m, i, p);
                m.set(i, !p);
            }
        }
        _ => {
            for i in (0..n).rev() {
                let p = pat(b + i as u64);
                ctx.check(m, i, !p);
                m.set(i, p);
            }
        }
    }
}

fn block_move<W: Words>(phase: usize, m: &mut W, ctx: &Ctx) {
    let n = m.len() & !1;
    if n < 4 {
        return;
    }
    let h = n / 2;
    let b = ctx.base;
    let seed = 0x0123_4567_89AB_CDEF ^ pair_pattern(ctx.pass, 0x5EED_0004);
    let f = |k: usize| {
        let g = b + k as u64;
        seed.rotate_left((g & 63) as u32) ^ g
    };
    // After phase 1 the upper half holds a copy of the lower half.
    let copied = |i: usize| if i < h { f(i) } else { f(i - h) };
    match phase {
        0 => (0..n).for_each(|i| m.set(i, f(i))),
        1 => m.copy(0, h, h),
        2 => (0..n).for_each(|i| ctx.check(m, i, copied(i))),
        3 => m.copy(1, 0, n - 1),
        _ => {
            (0..n - 1).for_each(|i| ctx.check(m, i, copied(i + 1)));
            ctx.check(m, n - 1, copied(n - 1));
        }
    }
}

/// Superblock of the address-move test, in words (1 MiB); each splits into two moved blocks of at least 4 KiB.
const MOVE_SPAN: usize = 1 << 17;
const MOVE_MIN: usize = 512;

/// Own address, then the lower half copied up and the upper half copied back down in random blocks.
fn address_move<W: Words>(phase: usize, m: &mut W, ctx: &Ctx) {
    let n = m.len() & !1;
    if n < 4 {
        return;
    }
    let h = n / 2;
    let inv = if ctx.pass & 1 == 0 { 0 } else { !0 };
    let a = |m: &W, i: usize| m.addr(i) as u64 ^ inv;
    let seed = splitmix64(ctx.pass ^ ctx.base ^ 0x5EED_0005);
    match phase {
        0 => (0..n).for_each(|i| m.set(i, a(m, i))),
        1 => move_blocks(h, seed, |off, len| m.copy(off, h + off, len)),
        3 => move_blocks(h, !seed, |off, len| m.copy(h + off, off, len)),
        _ => (0..n).for_each(|i| ctx.check(m, i, a(m, if i < h { i } else { i - h }))),
    }
}

/// Calls `f(offset, len)` for blocks covering `0..h` exactly once: 1 MiB superblocks in a seeded order,
/// each cut at a random word into two blocks of 4 KiB to 1 MiB, the two in random order.
fn move_blocks(h: usize, seed: u64, mut f: impl FnMut(usize, usize)) {
    let spans = h.div_ceil(MOVE_SPAN);
    let r = splitmix64(seed);
    let mut step = (r as usize % spans) | 1;
    while gcd(step, spans) != 1 {
        step += 2;
    }
    let start = (r >> 32) as usize % spans;
    for k in 0..spans {
        let j = (start + k * step) % spans;
        let off = j * MOVE_SPAN;
        let len = MOVE_SPAN.min(h - off);
        let rj = splitmix64(seed ^ (j as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        if len < 2 * MOVE_MIN {
            f(off, len);
            continue;
        }
        let cut = MOVE_MIN + rj as usize % (len - 2 * MOVE_MIN + 1);
        if rj >> 63 == 0 {
            f(off, cut);
            f(off + cut, len - cut);
        } else {
            f(off + cut, len - cut);
            f(off, cut);
        }
    }
}

fn gcd(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Random pattern for passes 2k and 2k+1, complemented on the odd pass so each bit sees both values.
fn pair_pattern(pass: u64, salt: u64) -> u64 {
    splitmix64((pass / 2) ^ salt) ^ if pass & 1 == 0 { 0 } else { !0 }
}

fn random_seq<W: Words>(phase: usize, m: &mut W, ctx: &Ctx) {
    let n = m.len();
    let seed = splitmix64(ctx.pass ^ 0x5EED_0002);
    let r = |i: usize| splitmix64(seed.wrapping_add(ctx.base + i as u64));
    match phase {
        0 => (0..n).for_each(|i| m.set(i, r(i))),
        1 => (0..n).for_each(|i| {
            let v = r(i);
            ctx.check(m, i, v);
            m.set(i, !v);
        }),
        _ => (0..n).for_each(|i| ctx.check(m, i, !r(i))),
    }
}

/// Every word a hash of its global index and a key per pass pair, complemented on the odd pass; written, then checked.
fn hashed<W: Words>(phase: usize, m: &mut W, ctx: &Ctx) {
    const BLOCK: usize = 256;
    let key = splitmix64((ctx.pass / 2) ^ 0x5EED_0006);
    let inv = if ctx.pass & 1 == 0 { 0 } else { !0 };
    let mut buf = [0u64; BLOCK];
    let n = m.len();
    let mut i = 0;
    while i < n {
        let len = BLOCK.min(n - i);
        hash::words(ctx.isa, &mut buf[..len], ctx.base + i as u64, key);
        if phase == 0 {
            (0..len).for_each(|k| m.set(i + k, buf[k] ^ inv));
        } else {
            (0..len).for_each(|k| ctx.check(m, i + k, buf[k] ^ inv));
        }
        i += len;
    }
}

fn modulo20<W: Words>(phase: usize, m: &mut W, ctx: &Ctx) {
    let n = m.len();
    let off = (phase / 3) as u64;
    let p = pair_pattern(ctx.pass, 0x5EED_0003);
    let first = ((off + 20 - ctx.base % 20) % 20) as usize;
    match phase % 3 {
        0 => (first..n).step_by(20).for_each(|i| m.set(i, p)),
        1 => {
            for _ in 0..2 {
                let mut c = first;
                for i in 0..n {
                    if c != 0 {
                        m.set(i, !p);
                    }
                    c = if c == 0 { 19 } else { c - 1 };
                }
            }
        }
        _ => (first..n).step_by(20).for_each(|i| ctx.check(m, i, p)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vec-backed memory with an optional stuck-at-one bit and an `alias.0` -> `alias.1` alias.
    struct FaultyWords {
        data: Vec<u64>,
        stuck: Option<(usize, u64)>,
        alias: Option<(usize, usize)>,
    }

    impl FaultyWords {
        fn new(n: usize) -> Self {
            FaultyWords { data: vec![0; n], stuck: None, alias: None }
        }
        fn map(&self, i: usize) -> usize {
            match self.alias {
                Some((from, to)) if i == from => to,
                _ => i,
            }
        }
    }

    impl Words for FaultyWords {
        fn len(&self) -> usize {
            self.data.len()
        }
        fn get(&self, i: usize) -> u64 {
            let j = self.map(i);
            let v = self.data[j];
            match self.stuck {
                Some((k, mask)) if k == j => v | mask,
                _ => v,
            }
        }
        fn set(&mut self, i: usize, v: u64) {
            let j = self.map(i);
            self.data[j] = v;
        }
        fn addr(&self, i: usize) -> usize {
            0x1000_0000 + i * 8
        }
        fn copy(&mut self, src: usize, dst: usize, n: usize) {
            let tmp: Vec<u64> = (src..src + n).map(|i| self.get(i)).collect();
            for (k, v) in tmp.into_iter().enumerate() {
                self.set(dst + k, v);
            }
        }
    }

    fn run_all(t: TestId, m: &mut FaultyWords, pass: u64) -> u64 {
        let log = ErrorLog::new();
        let ctx = Ctx { pass, base: 77, test: t, log: &log, isa: Isa::Sse2 };
        for ph in 0..t.phases() {
            t.run(ph, m, &ctx);
        }
        log.count()
    }

    #[test]
    fn clean_memory_passes() {
        for t in ALL {
            for pass in 0..3 {
                let mut m = FaultyWords::new(1000);
                assert_eq!(run_all(t, &mut m, pass), 0, "{t:?} pass {pass}");
            }
        }
    }

    #[test]
    fn stuck_bit_detected_by_every_test_within_two_passes() {
        for t in ALL {
            for (idx, bit) in [(0usize, 0u32), (513, 17), (999, 63)] {
                let mut m = FaultyWords::new(1000);
                m.stuck = Some((idx, 1 << bit));
                let errors = run_all(t, &mut m, 0) + run_all(t, &mut m, 1);
                assert!(errors > 0, "{t:?} missed stuck bit {bit} at {idx}");
            }
        }
    }

    #[test]
    fn address_alias_detected() {
        for t in [TestId::AddressOwn, TestId::MovingInvWalk64, TestId::RandomSeq, TestId::AddressMove] {
            let mut m = FaultyWords::new(1000);
            m.alias = Some((900, 100));
            assert!(run_all(t, &mut m, 0) > 0, "{t:?} missed alias");
        }
    }

    #[test]
    fn move_blocks_cover_the_half_once() {
        for h in [100, 1023, 1024, MOVE_SPAN, 3 * MOVE_SPAN + 700, 5 * MOVE_SPAN] {
            let mut seen = vec![0u8; h];
            let mut sizes = Vec::new();
            move_blocks(h, 42 + h as u64, |off, len| {
                seen[off..off + len].iter_mut().for_each(|s| *s += 1);
                sizes.push(len);
            });
            assert!(seen.iter().all(|&s| s == 1), "h={h}");
            if h >= 2 * MOVE_MIN {
                assert!(sizes.iter().all(|&s| (MOVE_MIN..=MOVE_SPAN).contains(&s)), "h={h} {sizes:?}");
            }
        }
    }

    #[test]
    fn address_move_reports_a_flip_after_the_moves() {
        let t = TestId::AddressMove;
        let n = 4 * MOVE_SPAN + 2 * 777;
        for pass in 0..2 {
            let mut m = FaultyWords::new(n);
            let log = ErrorLog::new();
            let ctx = Ctx { pass, base: 0, test: t, log: &log, isa: Isa::Sse2 };
            for ph in 0..t.phases() {
                t.run(ph, &mut m, &ctx);
            }
            assert_eq!(log.count(), 0, "clean pass {pass}");
            let k = n / 2 + MOVE_SPAN + 123;
            t.run(0, &mut m, &ctx);
            t.run(1, &mut m, &ctx);
            m.data[k] ^= 1 << 40;
            t.run(2, &mut m, &ctx);
            let mut out = [MemError::default(); 4];
            assert_eq!(log.since(0, &mut out).0, 1);
            let want = m.addr(k - n / 2) as u64 ^ if pass == 1 { !0 } else { 0 };
            assert_eq!(
                out[0],
                MemError { addr: m.addr(k), expected: want, actual: want ^ 1 << 40, test: t as u8, pass: pass as u32 }
            );
        }
    }

    #[test]
    fn rounds_get_new_seeds_and_keep_the_pass() {
        let t = TestId::RandomSeq;
        let runs: Vec<Vec<u64>> = (0..5)
            .map(|r| {
                let mut m = FaultyWords::new(64);
                let log = ErrorLog::new();
                let ctx = Ctx { pass: round_pass(3, r), base: 0, test: t, log: &log, isa: Isa::Sse2 };
                t.run(0, &mut m, &ctx);
                m.data
            })
            .collect();
        assert!((0..5).all(|a| (0..5).all(|b| a == b || runs[a] != runs[b])));
        let mut m = FaultyWords::new(64);
        m.stuck = Some((7, 1));
        let log = ErrorLog::new();
        let ctx = Ctx { pass: round_pass(3, 4), base: 0, test: t, log: &log, isa: Isa::Sse2 };
        (0..t.phases()).for_each(|ph| t.run(ph, &mut m, &ctx));
        let mut out = [MemError::default(); 1];
        log.since(0, &mut out);
        assert_eq!(out[0].pass, 3);
        assert_eq!(label(t, 5), "Random sequence ×5");
        assert_eq!(sweeps(&QUICK), 5 + 24 + 3 * 5 + 2);
    }

    #[test]
    fn traffic_per_phase() {
        let (w, r, rw, half) = ((2, 1), (1, 1), (2, 1), (3, 2));
        let mi = [w, rw, rw, w, rw, rw];
        let m20 = [(4, 5), (4, 1), (2, 5)];
        for t in ALL {
            let want: Vec<(u64, u64)> = match t {
                TestId::AddressOwn | TestId::BitFade => vec![w, r, w, r],
                TestId::Hashed => vec![w, r],
                TestId::MovingInvOnes | TestId::MovingInvRandom | TestId::MovingInvWalk64 => mi.to_vec(),
                TestId::MovingInvWalk8 => mi.repeat(4),
                TestId::BlockMove => vec![w, half, r, rw, r],
                TestId::AddressMove => vec![w, half, r, half, r],
                TestId::RandomSeq => vec![w, rw, r],
                TestId::Modulo20 => m20.repeat(20),
            };
            assert_eq!(want.len(), t.phases(), "{t:?}");
            let got: Vec<(u64, u64)> = (0..t.phases()).map(|ph| t.traffic(ph)).collect();
            assert_eq!(got, want, "{t:?}");
        }
    }

    #[test]
    fn hashed_reports_the_flipped_word() {
        let t = TestId::Hashed;
        let mut isas = vec![Isa::Sse2];
        if crate::cpu::best_isa() == Isa::Avx2Fma {
            isas.push(Isa::Avx2Fma);
        }
        for isa in isas {
            let mut m = FaultyWords::new(1000);
            let log = ErrorLog::new();
            let ctx = Ctx { pass: 5, base: 300, test: t, log: &log, isa };
            let want = !hash::word(917, splitmix64(2 ^ 0x5EED_0006));
            t.run(0, &mut m, &ctx);
            assert_eq!(m.data[617], want);
            t.run(1, &mut m, &ctx);
            assert_eq!(log.count(), 0);
            m.data[617] ^= 1 << 1;
            t.run(1, &mut m, &ctx);
            let mut out = [MemError::default(); 2];
            assert_eq!(log.since(0, &mut out).0, 1);
            assert_eq!(
                out[0],
                MemError { addr: m.addr(617), expected: want, actual: want ^ 2, test: t as u8, pass: 5 }
            );
        }
    }

    #[test]
    fn error_details_recorded() {
        let mut m = FaultyWords::new(64);
        m.stuck = Some((5, 1 << 3));
        let log = ErrorLog::new();
        let t = TestId::MovingInvOnes;
        let ctx = Ctx { pass: 2, base: 0, test: t, log: &log, isa: Isa::Sse2 };
        for ph in 0..3 {
            t.run(ph, &mut m, &ctx);
        }
        let mut out = [MemError::default(); 4];
        let (k, next) = log.since(0, &mut out);
        assert_eq!((k, next), (1, 1));
        assert_eq!(out[0], MemError { addr: 0x1000_0000 + 40, expected: 0, actual: 8, test: 1, pass: 2 });
        assert_eq!(log.bits(), 8);
    }
}
