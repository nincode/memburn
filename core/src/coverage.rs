//! Physical memory coverage: which byte ranges of System RAM a run tested.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Sorted, non-overlapping, non-adjacent `[start, end)` byte ranges.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Ranges(Vec<(u64, u64)>);

/// Longest `cov=` field written to a history line, bytes.
pub const MAX_ENCODED: usize = 4000;

impl Ranges {
    pub fn new(mut v: Vec<(u64, u64)>) -> Ranges {
        v.retain(|&(s, e)| s < e);
        v.sort_unstable();
        let mut out: Vec<(u64, u64)> = Vec::with_capacity(v.len());
        for (s, e) in v {
            match out.last_mut() {
                Some(last) if s <= last.1 => last.1 = last.1.max(e),
                _ => out.push((s, e)),
            }
        }
        Ranges(out)
    }

    pub fn as_slice(&self) -> &[(u64, u64)] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn bytes(&self) -> u64 {
        self.0.iter().map(|&(s, e)| e - s).sum()
    }

    pub fn union(&self, o: &Ranges) -> Ranges {
        Ranges::new(self.0.iter().chain(&o.0).copied().collect())
    }

    pub fn intersect(&self, o: &Ranges) -> Ranges {
        let (mut i, mut j, mut out) = (0, 0, Vec::new());
        while i < self.0.len() && j < o.0.len() {
            let (a, b) = (self.0[i], o.0[j]);
            let (s, e) = (a.0.max(b.0), a.1.min(b.1));
            if s < e {
                out.push((s, e));
            }
            if a.1 < b.1 { i += 1 } else { j += 1 }
        }
        Ranges(out)
    }

    /// Parts of `self` not in `o`.
    pub fn minus(&self, o: &Ranges) -> Ranges {
        let mut out = Vec::new();
        let mut j = 0;
        for &(mut s, e) in &self.0 {
            while j < o.0.len() && o.0[j].1 <= s {
                j += 1;
            }
            let mut k = j;
            while k < o.0.len() && o.0[k].0 < e {
                if o.0[k].0 > s {
                    out.push((s, o.0[k].0));
                }
                s = s.max(o.0[k].1);
                k += 1;
            }
            if s < e {
                out.push((s, e));
            }
        }
        Ranges(out)
    }

    /// Whole units of `1 << shift` that are at least half covered.
    pub fn coarsen(&self, shift: u32) -> Ranges {
        let g = 1u64 << shift;
        let mut units: Vec<(u64, u64)> = Vec::new();
        let mut part = (u64::MAX, 0u64);
        let close = |part: (u64, u64), units: &mut Vec<(u64, u64)>| {
            if part.0 != u64::MAX && part.1 * 2 >= g {
                units.push((part.0 * g, part.0 * g + g));
            }
        };
        for &(s, e) in &self.0 {
            let (first, last) = (s / g, (e - 1) / g);
            for u in [first, last] {
                if u != part.0 {
                    close(part, &mut units);
                    part = (u, 0);
                }
                if u == first {
                    part.1 += e.min(u * g + g) - s;
                }
                if first == last {
                    break;
                }
                if u == first && last > first + 1 {
                    units.push(((first + 1) * g, last * g));
                }
                if u == last {
                    part.1 += e - u * g;
                }
            }
        }
        close(part, &mut units);
        Ranges::new(units)
    }

    /// `<shift>:<gap>+<length>,...`, each gap from the previous end, in hex units of `1 << shift`, at the finest shift from 12 that fits [`MAX_ENCODED`].
    pub fn encode(&self) -> String {
        let mut shift = 12;
        loop {
            let r = self.coarsen(shift);
            let mut at = 0;
            let body: Vec<String> =
                r.0.iter()
                    .map(|&(s, e)| {
                        let gap = (s - at) >> shift;
                        at = e;
                        format!("{gap:x}+{:x}", (e - s) >> shift)
                    })
                    .collect();
            let text = format!("{shift}:{}", body.join(","));
            if text.len() <= MAX_ENCODED || shift >= 40 {
                return text;
            }
            shift += 1;
        }
    }

    pub fn decode(text: &str) -> Option<Ranges> {
        let (shift, body) = text.split_once(':')?;
        let shift: u32 = shift.parse().ok().filter(|&s| s < 64)?;
        let (mut v, mut at) = (Vec::new(), 0u64);
        for part in body.split(',').filter(|p| !p.is_empty()) {
            let (gap, len) = part.split_once('+')?;
            let (gap, len) = (u64::from_str_radix(gap, 16).ok()?, u64::from_str_radix(len, 16).ok()?);
            let s = at.checked_add(gap.checked_shl(shift)?)?;
            at = s.checked_add(len.checked_shl(shift)?)?;
            v.push((s, at));
        }
        Some(Ranges::new(v))
    }

    /// Up to `n` ranges, largest first.
    pub fn largest(&self, n: usize) -> Vec<(u64, u64)> {
        let mut v = self.0.clone();
        v.sort_by_key(|&(s, e)| (core::cmp::Reverse(e - s), s));
        v.truncate(n);
        v
    }
}

/// Share of `ram` inside `tested`, percent.
pub fn percent(tested: &Ranges, ram: &Ranges) -> f64 {
    let total = ram.bytes();
    if total == 0 { 0.0 } else { tested.intersect(ram).bytes() as f64 * 100.0 / total as f64 }
}

/// "99.4%"; one more decimal near 100 so a gap stays visible.
pub fn fmt_percent(p: f64) -> String {
    if p < 99.95 {
        format!("{p:.1}%")
    } else if p < 100.0 {
        format!("{p:.2}%")
    } else {
        String::from("100%")
    }
}

/// "0x1_2340_0000-0x1_2380_0000 (4 MiB)".
pub fn fmt_range((s, e): (u64, u64)) -> String {
    let mib = (e - s) as f64 / (1u64 << 20) as f64;
    let size = if mib >= 1.0 { format!("{mib:.0} MiB") } else { format!("{} KiB", (e - s) >> 10) };
    format!("{s:#x}-{e:#x} ({size})")
}

/// Physical ranges behind `/proc/self/pagemap` entries of consecutive 4 KiB pages; pages without a PFN are skipped.
pub fn pagemap_ranges(entries: &[u64]) -> Ranges {
    let mut v: Vec<(u64, u64)> = Vec::new();
    for &e in entries {
        let pfn = e & ((1 << 55) - 1);
        if e >> 63 != 1 || pfn == 0 {
            continue;
        }
        let a = pfn * 4096;
        match v.last_mut() {
            Some(last) if last.1 == a => last.1 += 4096,
            _ => v.push((a, a + 4096)),
        }
    }
    Ranges::new(v)
}

/// "System RAM" entries of `/sys/firmware/memmap`: `(start, inclusive end, type)`.
pub fn system_ram(entries: &[(u64, u64, &str)]) -> Ranges {
    Ranges::new(entries.iter().filter(|e| e.2.trim() == "System RAM").map(|&(s, e, _)| (s, e + 1)).collect())
}

/// FNV-1a of the memory module lines, to tell machines apart in a shared history file.
pub fn host_id<'a>(parts: impl IntoIterator<Item = &'a str>) -> u32 {
    let mut h = 0x811c_9dc5u32;
    for b in parts.into_iter().flat_map(|p| p.bytes().chain(*b"\n")) {
        h = (h ^ b as u32).wrapping_mul(0x0100_0193);
    }
    h
}

/// What a run tested on one machine: System RAM size, host id and physical ranges.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Footprint {
    pub ram_mib: u64,
    pub host: u32,
    pub tested: Ranges,
}

impl Footprint {
    /// ` ram=<MiB> host=<hex> cov=<ranges>`, appended to a history result line.
    pub fn fields(&self) -> String {
        format!(" ram={} host={:08x} cov={}", self.ram_mib, self.host, self.tested.encode())
    }

    pub fn parse(line: &str) -> Option<Footprint> {
        let field = |k: &str| line.split_whitespace().find_map(|t| t.strip_prefix(k));
        Some(Footprint {
            ram_mib: field("ram=")?.parse().ok()?,
            host: u32::from_str_radix(field("host=")?, 16).ok()?,
            tested: Ranges::decode(field("cov=")?)?,
        })
    }
}

/// The kernel's `memtest=N` run before the OS started, from its log.
#[derive(Clone, Default, PartialEq, Debug)]
pub struct EarlyTest {
    pub patterns: u32,
    pub tested: Ranges,
    pub bad: Vec<(u64, u64)>,
    pub ms: u64,
}

fn hex(t: &str) -> Option<u64> {
    u64::from_str_radix(t.strip_prefix("0x")?, 16).ok()
}

fn two_addresses(msg: &str) -> Option<(u64, u64)> {
    let mut it = msg.split_whitespace().filter_map(hex);
    Some((it.next()?, it.next()?))
}

/// Parses `/dev/kmsg` records (`level,seq,usec,flags;message`) or plain `dmesg` lines.
pub fn parse_early(text: &str) -> Option<EarlyTest> {
    let mut t = EarlyTest::default();
    let mut tested = Vec::new();
    let (mut first_us, mut last_us, mut in_test) = (None, 0u64, false);
    for line in text.lines() {
        let (us, msg) = match line.split_once(';') {
            Some((head, msg)) if head.split(',').count() >= 3 => {
                (head.split(',').nth(2).and_then(|u| u.parse().ok()), msg)
            }
            _ => (None, line),
        };
        if let Some(n) = msg.split("early_memtest: # of tests:").nth(1) {
            t.patterns = n.trim().parse().ok()?;
            first_us = us;
            in_test = true;
            continue;
        }
        if !in_test {
            continue;
        }
        if msg.contains("bad mem addr") {
            t.bad.extend(two_addresses(msg));
        } else if msg.contains(" pattern ") {
            tested.extend(two_addresses(msg));
        } else if !line.starts_with(' ') {
            in_test = false;
        }
        if let Some(u) = us {
            last_us = u;
        }
    }
    if t.patterns == 0 {
        return None;
    }
    t.tested = Ranges::new(tested);
    t.ms = first_us.map_or(0, |f| last_us.saturating_sub(f) / 1000);
    Some(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn set_math() {
        let a = Ranges::new(vec![(10, 20), (0, 5), (18, 30), (30, 31), (40, 40)]);
        assert_eq!(a.as_slice(), &[(0, 5), (10, 31)]);
        let b = Ranges::new(vec![(3, 12), (25, 50)]);
        assert_eq!(a.union(&b).as_slice(), &[(0, 50)]);
        assert_eq!(a.intersect(&b).as_slice(), &[(3, 5), (10, 12), (25, 31)]);
        assert_eq!(a.minus(&b).as_slice(), &[(0, 3), (12, 25)]);
        assert_eq!(b.minus(&a).as_slice(), &[(5, 10), (31, 50)]);
        assert_eq!(a.bytes(), 26);
        assert_eq!(percent(&b, &a), 10.0 * 100.0 / 26.0);
        assert_eq!(b.largest(1), vec![(25, 50)]);
        assert_eq!(fmt_percent(99.43), "99.4%");
        assert_eq!(fmt_percent(99.96), "99.96%");
        assert_eq!(fmt_range((0x10_0000, 0x50_0000)), "0x100000-0x500000 (4 MiB)");
    }

    #[test]
    fn pagemap_merge() {
        let page = |pfn: u64| 1 << 63 | pfn;
        let e = [page(100), page(101), page(102), 0, page(200), page(201), page(50), 1 << 63];
        let r = pagemap_ranges(&e);
        assert_eq!(r.as_slice(), &[(50 * 4096, 51 * 4096), (100 * 4096, 103 * 4096), (200 * 4096, 202 * 4096)]);
        let ram = system_ram(&[
            (0, 0x9_ffff, "System RAM\n"),
            (0xa_0000, 0xf_ffff, "Reserved"),
            (0x10_0000, 0x3fff_ffff, "System RAM"),
        ]);
        assert_eq!(ram.as_slice(), &[(0, 0xa_0000), (0x10_0000, 0x4000_0000)]);
    }

    #[test]
    fn encoding_round_trips_and_stays_short() {
        let r = Ranges::new(vec![(0x10_0000, 0x9f00_0000), (0x1_0000_0000, 0x1_2345_6000)]);
        let s = r.encode();
        assert_eq!(s, "12:100+9ef00,61000+23456");
        assert_eq!(Ranges::decode(&s), Some(r));
        let many = Ranges::new((0..5000u64).map(|i| (i << 22, (i << 22) + (3 << 20))).collect());
        let s = many.encode();
        assert!(s.len() <= MAX_ENCODED, "{}", s.len());
        let back = Ranges::decode(&s).unwrap();
        assert_eq!(back, many.coarsen(s.split(':').next().unwrap().parse().unwrap()));
        assert_eq!(back.bytes(), 5000 << 22);
        let r = Ranges::new(vec![(0x800, 0x1000), (0x1000, 0x1400), (0x2c00, 0x5100), (0x7000, 0x7900)]);
        assert_eq!(r.coarsen(12).as_slice(), &[(0, 0x1000), (0x3000, 0x5000), (0x7000, 0x8000)]);
        assert_eq!(Ranges::decode("x"), None);
        assert_eq!(Ranges::decode("12:"), Some(Ranges::default()));
    }

    #[test]
    fn footprints_in_lines() {
        let f = Footprint {
            ram_mib: 4096,
            host: host_id(["DIMM 0  4 GiB  QEMU"]),
            tested: Ranges::new(vec![(1 << 20, 1 << 30)]),
        };
        let line = format!("2026-10-12 09:00 UTC mem=0 cpu=0 bits=0x0{}", f.fields());
        assert_eq!(Footprint::parse(&line), Some(f));
        assert_eq!(Footprint::parse("2026-10-12 09:00 UTC mem=0 cpu=0 bits=0x0"), None);
        assert_ne!(host_id(["a", "b"]), host_id(["ab"]));
    }

    const KMSG: &str = "6,1,0,-;Linux version 7.0.0-38-generic\n\
6,140,51234,-;early_memtest: # of tests: 2\n\
6,141,51240,-;  0x0000000000010000 - 0x000000000009f000 pattern aaaaaaaaaaaaaaaa\n\
6,142,51300,-;  0x0000000000100000 - 0x0000000001000000 pattern aaaaaaaaaaaaaaaa\n\
6,143,61300,-;  0x0000000003000000 - 0x00000000bffdf000 pattern aaaaaaaaaaaaaaaa\n\
6,144,801300,-;  0x0000000000010000 - 0x000000000009f000 pattern 0000000000000000\n\
6,145,801310,-;  0x0000000000100000 - 0x0000000001000000 pattern 0000000000000000\n\
6,146,801400,-;  0000000000000000 bad mem addr 0x0000000004000000 - 0x0000000004000008 reserved\n\
6,147,801500,-;  0x0000000003000000 - 0x00000000bffdf000 pattern 0000000000000000\n\
6,148,1551300,-;ACPI: Early table checksum verification disabled\n\
6,149,1551400,-;  0x0000000000000000 - 0x0000000000001000 pattern 1111111111111111\n";

    #[test]
    fn early_memtest_from_kmsg() {
        let t = parse_early(KMSG).unwrap();
        assert_eq!(t.patterns, 2);
        assert_eq!(t.tested.as_slice(), &[(0x1_0000, 0x9_f000), (0x10_0000, 0x100_0000), (0x300_0000, 0xbffd_f000)]);
        assert_eq!(t.bad, vec![(0x400_0000, 0x400_0008)]);
        assert_eq!(t.ms, 1500);
        let dmesg = "[    0.051234] early_memtest: # of tests: 1\n[    0.051240]   0x0000000000100000 - 0x0000000001000000 pattern 0\n";
        let t = parse_early(dmesg).unwrap();
        assert_eq!((t.patterns, t.tested.bytes()), (1, 0xf0_0000));
        assert_eq!(parse_early("6,1,0,-;Linux version\n"), None);
    }
}
