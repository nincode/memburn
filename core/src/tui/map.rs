//! Live map of the memory under test: the tested regions back to back, lit as the sweep passes, errors in red.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::Color::{self, *};
use super::{Frame, Rect};
use crate::engine::Chunk;

/// Failing words marked on the map.
pub const MAX_MARKS: usize = 4096;
/// Error ranges kept for the legend.
pub const MAX_RANGES: usize = 16;
/// Legend ranges are this many bytes, aligned.
pub const RANGE_BYTES: u64 = 1 << 16;
/// A new error blinks this long, ms.
const FRESH_MS: u64 = 1200;

#[derive(Clone, Default, Debug)]
pub struct MemMap {
    /// Bytes under test.
    pub total: u64,
    /// Bytes the current phase has finished, counted from the end it starts at.
    pub swept: u64,
    /// Bytes in flight past `swept`: one chunk per thread.
    pub front: u64,
    /// The current phase runs from the top down.
    pub descending: bool,
    /// (address, bytes, offset) of each contiguous tested span, by address.
    spans: Vec<(u64, u64, u64)>,
    /// Offsets of failing words, sorted.
    marks: Vec<u64>,
    /// Error ranges, first found first.
    pub ranges: Vec<ErrRange>,
    /// Ranges beyond [`MAX_RANGES`].
    pub more_ranges: u32,
    /// Offset of the newest error and the view's `elapsed_ms` when it came.
    fresh: Option<(u64, u64)>,
    /// Chunks in flight at once.
    pub threads: u32,
}

/// Errors within one aligned [`RANGE_BYTES`] block of shown addresses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ErrRange {
    /// Aligned shown address.
    pub addr: u64,
    /// Flipped-bit mask.
    pub bits: u64,
    /// Offset of its first error in the tested bytes.
    pub at: u64,
    pub errors: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sample {
    Pending,
    Done,
    Head,
    Error,
    Fresh,
}

impl Sample {
    fn rank(self) -> u8 {
        self as u8
    }
}

impl MemMap {
    /// A map of `chunks` (as the engine tests them) with `threads` chunks in flight at once.
    pub fn new(chunks: &[Chunk], threads: usize) -> MemMap {
        let mut spans: Vec<(u64, u64, u64)> = Vec::new();
        for c in chunks {
            let (addr, bytes, off) = (c.ptr as u64, c.words as u64 * 8, c.base * 8);
            match spans.last_mut() {
                Some(s) if s.0 + s.1 == addr && s.2 + s.1 == off => s.1 += bytes,
                _ => spans.push((addr, bytes, off)),
            }
        }
        spans.sort_unstable();
        let total = chunks.iter().map(|c| c.words as u64 * 8).sum();
        let chunk = chunks.iter().map(|c| c.words as u64 * 8).max().unwrap_or(0);
        MemMap { total, front: chunk * threads as u64, spans, threads: threads as u32, ..MemMap::default() }
    }

    /// Offset of `addr` in the tested regions.
    pub fn offset(&self, addr: u64) -> Option<u64> {
        let i = self.spans.partition_point(|s| s.0 <= addr).checked_sub(1)?;
        let (a, n, off) = self.spans[i];
        (addr < a + n).then(|| off + addr - a)
    }

    /// Starts a phase.
    pub fn phase(&mut self, descending: bool) {
        self.descending = descending;
        self.swept = 0;
    }

    /// Marks an error at `addr` (the address the test wrote); `shown` is the address the dashboard prints for it.
    pub fn error(&mut self, addr: u64, shown: u64, flipped: u64, at_ms: u64) {
        let Some(off) = self.offset(addr) else { return };
        if let Err(i) = self.marks.binary_search(&off)
            && self.marks.len() < MAX_MARKS
        {
            self.marks.insert(i, off);
        }
        self.fresh = Some((off, at_ms));
        let key = shown & !(RANGE_BYTES - 1);
        let full = self.ranges.len() >= MAX_RANGES;
        match self.ranges.iter_mut().find(|r| r.addr == key) {
            Some(r) => {
                r.bits |= flipped;
                r.errors += 1;
            }
            None if !full => self.ranges.push(ErrRange { addr: key, bits: flipped, at: off, errors: 1 }),
            None => self.more_ranges += 1,
        }
    }

    /// State of offsets `a..b` from the sweep alone.
    fn sweep(&self, a: u64, b: u64) -> Sample {
        let mid = a / 2 + b / 2;
        let pos = if self.descending { self.total.saturating_sub(mid + 1) } else { mid };
        if pos < self.swept {
            Sample::Done
        } else if pos < self.swept + self.front {
            Sample::Head
        } else {
            Sample::Pending
        }
    }

    /// States of `n` equal slices of the tested bytes at `now_ms`.
    pub fn samples(&self, n: usize, now_ms: u64) -> Vec<Sample> {
        let total = self.total.max(1);
        let at = |k: usize| (k as u128 * total as u128 / n as u128) as u64;
        let mut s: Vec<Sample> = (0..n).map(|k| self.sweep(at(k), at(k + 1))).collect();
        let slot = |off: u64| ((off as u128 * n as u128 / total as u128) as usize).min(n - 1);
        for &m in &self.marks {
            s[slot(m)] = Sample::Error;
        }
        if let Some((off, t)) = self.fresh
            && now_ms.saturating_sub(t) < FRESH_MS
        {
            s[slot(off)] = Sample::Fresh;
        }
        s
    }
}

fn color(s: Sample, now_ms: u64) -> Color {
    match s {
        Sample::Pending => Black,
        Sample::Done => Cyan,
        Sample::Head => White,
        Sample::Error => LightRed,
        Sample::Fresh if (now_ms / 200).is_multiple_of(2) => White,
        Sample::Fresh => LightRed,
    }
}

/// Nearest of the 8 background colours.
fn as_bg(c: Color) -> Color {
    match c {
        White => LightGray,
        LightRed => Red,
        c if (c as u8) < 8 => c,
        _ => Black,
    }
}

/// "12G", or "512M" below 4 GiB.
fn label(off: u64, total: u64) -> String {
    if total >= 4 << 30 { format!("{}G", off >> 30) } else { format!("{}M", off >> 20) }
}

fn mask(st: &[Sample], on: impl Fn(Sample) -> bool) -> u32 {
    st.iter().enumerate().filter(|(_, s)| on(**s)).map(|(i, _)| 1 << i).sum()
}

/// Draws the map into `r`: 2×4 samples per cell in braille, else 1×2 in half blocks; row labels every other row.
pub(super) fn draw(f: &mut Frame, m: &MemMap, r: Rect, braille: bool, now_ms: u64) {
    let lw = label(m.total, m.total).len() + 1;
    let x0 = r.x + lw;
    let cols = (r.x + r.w).saturating_sub(x0);
    if cols == 0 || r.h == 0 {
        return;
    }
    let (dw, dh) = if braille { (2, 4) } else { (1, 2) };
    let per_row = cols * dw * dh;
    let st = m.samples(per_row * r.h, now_ms);
    for cy in 0..r.h {
        if cy % 2 == 0 {
            let off = (cy * per_row) as u128 * m.total as u128 / st.len() as u128;
            f.text(r.x, r.y + cy, &format!("{:>w$}", label(off as u64, m.total), w = lw - 1), lw, DarkGray, Black);
        }
        for cx in 0..cols {
            let (x, y) = (x0 + cx, r.y + cy);
            let at = |dx: usize, dy: usize| st[cy * per_row + dy * cols * dw + cx * dw + dx];
            if braille {
                let order = [(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2), (0, 3), (1, 3)];
                let cell = order.map(|(dx, dy)| at(dx, dy));
                let top = cell.iter().copied().max_by_key(|s| s.rank()).unwrap_or(Sample::Pending);
                let (bits, c) = match top {
                    Sample::Error | Sample::Fresh => (0xff, color(top, now_ms)),
                    Sample::Head => (mask(&cell, |s| matches!(s, Sample::Done | Sample::Head)), White),
                    Sample::Done => (mask(&cell, |s| s == Sample::Done), Cyan),
                    Sample::Pending => (0xff, DarkGray),
                };
                let ch = if matches!(top, Sample::Error | Sample::Fresh) { '█' } else { braille_char(bits) };
                f.set(x, y, ch, c, Black);
            } else {
                half(f, x, y, at(0, 0), at(0, 1), now_ms);
            }
        }
    }
}

fn braille_char(bits: u32) -> char {
    char::from_u32(0x2800 + bits).unwrap_or(' ')
}

/// One cell holding a top and a bottom sample.
fn half(f: &mut Frame, x: usize, y: usize, a: Sample, b: Sample, now_ms: u64) {
    let (ca, cb) = (color(a, now_ms), color(b, now_ms));
    if matches!(a, Sample::Error | Sample::Fresh) || matches!(b, Sample::Error | Sample::Fresh) {
        let c = if a == Sample::Fresh || b == Sample::Fresh { color(Sample::Fresh, now_ms) } else { LightRed };
        return f.set(x, y, '█', c, Black);
    }
    if ca == cb {
        return if ca == Black { f.set(x, y, '·', DarkGray, Black) } else { f.set(x, y, '█', ca, Black) };
    }
    if a.rank() >= b.rank() { f.set(x, y, '▀', ca, as_bg(cb)) } else { f.set(x, y, '▄', cb, as_bg(ca)) }
}

/// "0xabd160000 bits 20, 22".
pub fn range_label(addr: u64, flipped: u64) -> String {
    let bits: Vec<String> = (0..64).filter(|b| flipped >> b & 1 == 1).map(|b| format!("{b}")).collect();
    let shown = bits.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
    let more = if bits.len() > 4 { ", …" } else { "" };
    format!("0x{addr:x} bit{} {shown}{more}", if bits.len() == 1 { "" } else { "s" })
}

/// Columns the full legend takes.
pub(super) fn legend_width(m: &MemMap) -> usize {
    if m.ranges.is_empty() {
        return ["tested", "sweep", "pending"].iter().map(|s| s.len() + 4).sum();
    }
    m.ranges.iter().map(|r| range_label(r.addr, r.bits).chars().count() + 4).sum::<usize>()
        + if m.more_ranges > 0 { format!("+{}", m.more_ranges).len() } else { 0 }
}

/// Key on the title row from `x` to `end`: error ranges when there are any, else the colours.
pub(super) fn legend(f: &mut Frame, m: &MemMap, x: usize, end: usize, y: usize) {
    let mut cx = x;
    if m.ranges.is_empty() {
        for (c, s) in [(Cyan, "tested"), (White, "sweep"), (DarkGray, "pending")] {
            if cx + s.len() + 2 > end {
                return;
            }
            f.set(cx, y, '■', c, Black);
            cx = f.text(cx + 2, y, s, end - cx - 2, DarkGray, Black) + 2;
        }
        return;
    }
    let n = m.ranges.len() + m.more_ranges as usize;
    for (k, r) in m.ranges.iter().enumerate() {
        let s = range_label(r.addr, r.bits);
        let rest = n - k - 1;
        let tail = if rest > 0 { format!("+{rest}").len() + 1 } else { 0 };
        if cx + s.chars().count() + 2 + tail > end {
            f.text(cx, y, &format!("+{}", n - k), end.saturating_sub(cx), LightRed, Black);
            return;
        }
        f.set(cx, y, '■', LightRed, Black);
        cx = f.text(cx + 2, y, &s, end - cx - 2, LightRed, Black) + 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(spans: &[(u64, usize)], chunk: usize) -> Vec<Chunk> {
        let r: Vec<(usize, usize)> = spans.iter().map(|&(a, n)| (a as usize, n)).collect();
        crate::engine::chunk_regions(&r, chunk)
    }

    #[test]
    fn addresses_map_to_offsets_across_regions() {
        let m = MemMap::new(&chunks(&[(0x10_0000, 0x40_0000), (0x1_0000_0000, 0x20_0000)], 0x10_0000), 4);
        assert_eq!(m.total, 0x60_0000);
        assert_eq!(m.front, 4 * 0x10_0000);
        assert_eq!(m.offset(0x10_0000), Some(0));
        assert_eq!(m.offset(0x4f_fff8), Some(0x3f_fff8));
        assert_eq!(m.offset(0x50_0000), None);
        assert_eq!(m.offset(0x1_0000_0008), Some(0x40_0008));
        assert_eq!(m.offset(0xf_ffff), None);
    }

    #[test]
    fn sweep_lights_done_head_and_pending() {
        let mut m = MemMap::new(&chunks(&[(0, 1 << 30)], 1 << 24), 8);
        assert_eq!(m.front, 1 << 27);
        m.phase(false);
        m.swept = 1 << 29;
        let s = m.samples(8, 0);
        assert_eq!(&s[..5], &[Sample::Done, Sample::Done, Sample::Done, Sample::Done, Sample::Head]);
        assert_eq!(s[7], Sample::Pending);
        m.phase(true);
        m.swept = 1 << 29;
        let s = m.samples(8, 0);
        assert_eq!(s[0], Sample::Pending);
        assert_eq!(s[3], Sample::Head);
        assert_eq!(&s[4..], &[Sample::Done; 4]);
    }

    #[test]
    fn errors_mark_their_cell_and_name_their_range() {
        let mut m = MemMap::new(&chunks(&[(1 << 30, 1 << 30)], 1 << 24), 2);
        m.error((1 << 30) + (3 << 27) + 8, 0xa_bd16_f218, 1 << 20, 1000);
        m.error((1 << 30) + (3 << 27) + 64, 0xa_bd16_8348, 1 << 22, 1500);
        m.error(5, 5, 1, 1500);
        let s = m.samples(8, 10_000);
        assert_eq!(s[3], Sample::Error);
        assert_eq!(s.iter().filter(|&&x| x == Sample::Error).count(), 1);
        assert_eq!(m.samples(8, 2000)[3], Sample::Fresh);
        let at = (3 << 27) + 8;
        assert_eq!(m.ranges, [ErrRange { addr: 0xa_bd16_0000, bits: (1 << 20) | (1 << 22), at, errors: 2 }]);
        assert_eq!(range_label(0xa_bd16_0000, (1 << 20) | (1 << 22)), "0xabd160000 bits 20, 22");
        assert_eq!(range_label(0x1000, 1 << 3), "0x1000 bit 3");
    }

    #[test]
    fn braille_and_half_blocks_fill_the_rect() {
        let mut m = MemMap::new(&chunks(&[(0, 8 << 30)], 1 << 26), 4);
        m.swept = 2 << 30;
        m.error(7 << 30, 7 << 30, 1, 0);
        for braille in [true, false] {
            let mut f = Frame::new(40, 6);
            draw(&mut f, &m, Rect { x: 0, y: 0, w: 40, h: 6 }, braille, 5000);
            let row = |y: usize| (0..40).map(|x| f.get(x, y).ch).collect::<String>();
            assert!(row(0).starts_with("0G "));
            assert!(row(2).starts_with("2G "), "{}", row(2));
            assert_eq!(f.get(3, 0).fg, Cyan);
            assert!((0..40).any(|x| f.get(x, 5).fg == LightRed));
            assert_eq!(f.get(39, 3).fg, DarkGray);
            assert_eq!(f.get(39, 3).ch, if braille { '⣿' } else { '·' });
        }
    }
}
