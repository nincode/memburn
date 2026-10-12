//! Earlier runs: a start line per run, then a result line; bits that failed in runs with a memory test.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::coverage::{Footprint, Ranges};

pub const FILE: &str = "memburn-history.txt";

/// Result of a run with a memory test: `<date> mem=<errors> cpu=<errors> bits=<hex mask of flipped bits>`,
/// then the [`Footprint`] fields when the physical ranges are known.
pub fn line(date: &str, mem: u64, cpu: u64, bits: u64, footprint: Option<&Footprint>) -> String {
    let cov = footprint.map(Footprint::fields).unwrap_or_default();
    format!("{date} mem={mem} cpu={cpu} bits={bits:#x}{cov}")
}

const START: &str = " start: ";
const END: &str = " end cpu=";

/// Written when a run starts: `<date> start: <job>`.
pub fn start_line(date: &str, job: &str) -> String {
    format!("{date}{START}{job}")
}

/// Result of a run without a memory test: `<date> end cpu=<errors>`.
pub fn end_line(date: &str, cpu: u64) -> String {
    format!("{date}{END}{cpu}")
}

/// A run whose start line has no result line after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unfinished {
    pub date: String,
    pub job: String,
}

#[derive(Clone)]
pub struct History {
    pub runs: u32,
    /// Runs in which each bit flipped.
    pub per_bit: [u32; 64],
    /// Started runs that never wrote a result.
    pub unfinished: u32,
    /// The newest run, when it did not finish.
    pub last_unfinished: Option<Unfinished>,
    /// Tested ranges of runs that recorded them, oldest first.
    pub footprints: Vec<Footprint>,
}

impl Default for History {
    fn default() -> Self {
        History { runs: 0, per_bit: [0; 64], unfinished: 0, last_unfinished: None, footprints: Vec::new() }
    }
}

impl History {
    pub fn parse(text: &str) -> History {
        let mut h = History::default();
        let mut open: Option<Unfinished> = None;
        for l in text.lines() {
            if let Some((date, job)) = l.split_once(START) {
                h.unfinished += u32::from(open.is_some());
                open = Some(Unfinished { date: date.into(), job: job.trim().into() });
            } else if let Some(mask) = l.split_whitespace().find_map(|t| t.strip_prefix("bits=")) {
                open = None;
                h.add(u64::from_str_radix(mask.trim_start_matches("0x"), 16).unwrap_or(0));
                h.footprints.extend(Footprint::parse(l));
            } else if l.contains(END) {
                open = None;
            }
        }
        h.unfinished += u32::from(open.is_some());
        h.last_unfinished = open;
        h
    }

    pub fn add(&mut self, bits: u64) {
        self.runs += 1;
        for (b, n) in self.per_bit.iter_mut().enumerate() {
            *n += (bits >> b & 1) as u32;
        }
    }

    /// Union of `now` and the last `n - 1` recorded runs on the same machine and RAM size, and the run count.
    pub fn union_with(&self, now: &Footprint, n: usize) -> (Ranges, usize) {
        let same = self.footprints.iter().rev().filter(|f| f.ram_mib == now.ram_mib && f.host == now.host);
        same.take(n.saturating_sub(1)).fold((now.tested.clone(), 1), |(u, k), f| (u.union(&f.tested), k + 1))
    }

    /// "5 runs, bit 1 in 2, bit 7 in 1" or "5 runs, no memory errors".
    pub fn summary(&self) -> String {
        let mut bits: Vec<(usize, u32)> = self.per_bit.iter().copied().enumerate().filter(|&(_, n)| n > 0).collect();
        bits.sort_by_key(|&(_, n)| core::cmp::Reverse(n));
        let mut runs = format!("{} run{}", self.runs, if self.runs == 1 { "" } else { "s" });
        if self.unfinished > 0 {
            runs.push_str(&format!(", {} did not finish", self.unfinished));
        }
        if bits.is_empty() {
            return format!("{runs}, no memory errors");
        }
        let parts: Vec<String> = bits.iter().take(4).map(|(b, n)| format!("bit {b} in {n}")).collect();
        let more = if bits.len() > 4 { format!(", +{} more bits", bits.len() - 4) } else { String::new() };
        format!("{runs}, {}{more}", parts.join(", "))
    }
}

/// "2026-10-12 11:00 UTC" from Unix seconds.
pub fn date(unix: u64) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_summarises() {
        let mut h =
            History::parse("2026-10-12 09:00 mem=3 cpu=0 bits=0x2\n2026-10-12 10:00 mem=0 cpu=0 bits=0x0\njunk\n");
        h.add(0x82);
        assert_eq!(h.runs, 3);
        assert_eq!(h.summary(), "3 runs, bit 1 in 2, bit 7 in 1");
        assert_eq!(History::default().summary(), "0 runs, no memory errors");
        assert_eq!(line("d", 3, 0, 2, None), "d mem=3 cpu=0 bits=0x2");
        assert_eq!((h.unfinished, h.last_unfinished), (0, None));
    }

    #[test]
    fn started_runs_without_a_result_are_unfinished() {
        let text = [
            start_line("2026-10-12 09:00 UTC", "Quick check"),
            line("2026-10-12 09:03 UTC", 0, 0, 0, None),
            start_line("2026-10-12 10:00 UTC", "Burn-in"),
            start_line("2026-10-12 11:00 UTC", "Burn-in"),
            end_line("2026-10-12 11:30 UTC", 0),
            start_line("2026-10-12 12:00 UTC", "Memory test + burn-in"),
            String::new(),
        ]
        .join("\r\n");
        let h = History::parse(&text);
        assert_eq!((h.runs, h.unfinished), (1, 2));
        let last = Unfinished { date: "2026-10-12 12:00 UTC".into(), job: "Memory test + burn-in".into() };
        assert_eq!(h.last_unfinished, Some(last));
        assert_eq!(h.summary(), "1 run, 2 did not finish, no memory errors");
        assert_eq!(end_line("d", 2), "d end cpu=2");
        let done = History::parse(&format!("{text}{}\n", end_line("d", 0)));
        assert_eq!((done.unfinished, done.last_unfinished), (1, None));
    }

    #[test]
    fn coverage_union_over_runs_on_the_same_machine() {
        let fp = |ram_mib, host, s: u64, e: u64| Footprint { ram_mib, host, tested: Ranges::new(alloc::vec![(s, e)]) };
        let text = [
            String::from("2026-10-11 09:00 UTC mem=0 cpu=0 bits=0x0"),
            line("2026-10-11 10:00 UTC", 0, 0, 0, Some(&fp(4096, 7, 0, 0x1000))),
            line("2026-10-11 11:00 UTC", 0, 0, 0, Some(&fp(8192, 7, 0, 0x8000))),
            line("2026-10-11 12:00 UTC", 0, 0, 0, Some(&fp(4096, 9, 0, 0x8000))),
            line("2026-10-11 13:00 UTC", 1, 0, 4, Some(&fp(4096, 7, 0x3000, 0x5000))),
            line("2026-10-11 14:00 UTC", 0, 0, 0, Some(&fp(4096, 7, 0x6000, 0x7000))),
        ]
        .join("\r\n");
        let h = History::parse(&text);
        assert_eq!((h.runs, h.footprints.len(), h.per_bit[2]), (6, 5, 1));
        let now = fp(4096, 7, 0x1000, 0x2000);
        let (u, n) = h.union_with(&now, 3);
        assert_eq!((u.as_slice(), n), (&[(0x1000, 0x2000), (0x3000, 0x5000), (0x6000, 0x7000)][..], 3));
        let (u, n) = h.union_with(&now, 10);
        assert_eq!((u.bytes(), n), (0x5000, 4));
        assert_eq!(h.union_with(&fp(1, 1, 0, 1), 10).1, 1);
    }

    #[test]
    fn dates() {
        assert_eq!(date(0), "1970-01-01 00:00 UTC");
        assert_eq!(date(1_791_802_800), "2026-10-12 11:00 UTC");
    }
}
