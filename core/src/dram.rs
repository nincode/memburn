//! AMD Zen DRAM address map (Data Fabric 2 and 3): physical address to memory channel, then SMBIOS slot.
//! Register fields, hashes and normalisation follow Linux drivers/ras/amd/atl (system.c, map.c, dehash.c, denormalize.c, core.c).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::smbios::Dimm;

/// Registers dumped once per boot: (PCI device, function, offset) on bus 0.
pub const REGS: &[(u8, u8, u16)] = &[
    (0x18, 0, 0x000),
    (0x18, 0, 0x040),
    (0x18, 0, 0x104),
    (0x18, 0, 0x110),
    (0x18, 0, 0x114),
    (0x18, 0, 0x118),
    (0x18, 0, 0x11C),
    (0x18, 0, 0x120),
    (0x18, 0, 0x124),
    (0x18, 0, 0x128),
    (0x18, 0, 0x12C),
    (0x18, 0, 0x1B4),
    (0x18, 0, 0x1B8),
    (0x18, 0, 0x3F8),
    (0x18, 1, 0x150),
    (0x18, 1, 0x208),
    (0x18, 1, 0x20C),
    (0x19, 0, 0x000),
];

const AMD: u32 = 0x1022;

/// Raw register values with the CPUID leaf 1 EAX they were read on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dump {
    pub cpuid: u32,
    pub regs: Vec<(u8, u8, u16, u32)>,
}

impl Dump {
    /// Reads every register in [`REGS`]; `read` returns None where config space is unreadable.
    pub fn read(cpuid: u32, mut read: impl FnMut(u8, u8, u16) -> Option<u32>) -> Dump {
        let regs = REGS.iter().filter_map(|&(d, f, o)| Some((d, f, o, read(d, f, o)?))).collect();
        Dump { cpuid, regs }
    }

    /// A register's value; None when absent or all ones.
    pub fn get(&self, dev: u8, func: u8, off: u16) -> Option<u32> {
        self.regs.iter().find(|r| (r.0, r.1, r.2) == (dev, func, off)).map(|r| r.3).filter(|&v| v != u32::MAX)
    }

    /// The [`fmt::Display`] form back.
    pub fn parse(s: &str) -> Option<Dump> {
        let mut words = s.split_whitespace();
        if words.next()? != "cpuid" {
            return None;
        }
        let cpuid = u32::from_str_radix(words.next()?, 16).ok()?;
        let mut regs = Vec::new();
        for w in words {
            let (at, v) = w.split_once('=')?;
            let (df, off) = at.split_once(':')?;
            let (d, f) = df.split_once('.')?;
            regs.push((
                u8::from_str_radix(d, 16).ok()?,
                f.parse().ok()?,
                u16::from_str_radix(off, 16).ok()?,
                u32::from_str_radix(v, 16).ok()?,
            ));
        }
        Some(Dump { cpuid, regs })
    }
}

/// "cpuid 00a20f10 18.0:000=14401022 18.0:040=00000000 ..."
impl fmt::Display for Dump {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "cpuid {:08x}", self.cpuid)?;
        for (d, func, off, v) in &self.regs {
            write!(f, " {d:02x}.{func}:{off:03x}={v:08x}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rev {
    Df2,
    Df3,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// Plain channel bits at the interleave bit.
    Plain,
    /// DF2: interleave bit XOR address bits 12, 18, 21, 30.
    Df2,
    /// DF3: first channel bit at the interleave bit, the others at 12 and 13, each hashed.
    Df3,
}

/// One DRAM address map.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Map {
    num: u8,
    base: u64,
    /// Last byte.
    limit: u64,
    hole: bool,
    mode: Mode,
    /// Channels interleaved, a power of two.
    chans: u16,
    /// Interleave bit position.
    bit: u8,
    /// Fabric ID of the first channel.
    dst: u16,
    /// Normalised address of this map's first byte in each channel.
    offset: u64,
}

/// A decoded system address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Target {
    pub map: u8,
    /// Coherent station fabric ID; the UMC (channel) on single-node parts.
    pub fabric: u16,
    /// Address within the channel, as the UMC sees it.
    pub norm: u64,
}

/// The DRAM address maps of a single-node DF2 or DF3 system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Df {
    pub rev: Rev,
    maps: Vec<Map>,
    hole_base: u64,
    /// DF3 DfGlobalCtl hash enables: 64 KiB, 2 MiB, 1 GiB.
    hash: [bool; 3],
}

fn bits(v: u32, hi: u32, lo: u32) -> u32 {
    (v >> lo) & ((1u32 << (hi - lo + 1)) - 1)
}

fn bit(a: u64, n: u8) -> u64 {
    (a >> n) & 1
}

/// `data` with `n` zero bits inserted at `at` (atl expand_bits).
fn expand(at: u8, n: u8, data: u64) -> u64 {
    let low = data & ((1u64 << at) - 1);
    low | ((data >> at) << (at + n))
}

/// `data` with bits `at..at + n` removed (atl remove_bits).
fn remove(at: u8, n: u8, data: u64) -> u64 {
    let low = data & ((1u64 << at) - 1);
    low | ((data >> (at + n)) << at)
}

impl Df {
    /// The maps from a dump, or why this system is not decoded.
    pub fn parse(d: &Dump) -> Result<Df, &'static str> {
        let family = crate::thermal::family(d.cpuid);
        if !matches!(family, 0x17 | 0x19) {
            return Err("not an AMD family 17h or 19h CPU");
        }
        let reg = |f: u8, o: u16| d.get(0x18, f, o);
        if reg(0, 0x000).map(|v| v & 0xFFFF) != Some(AMD) {
            return Err("no AMD Data Fabric at 00:18.0");
        }
        if d.get(0x19, 0, 0x000).map(|v| v & 0xFFFF) == Some(AMD) {
            return Err("more than one Data Fabric node");
        }
        let need = |f: u8, o: u16| reg(f, o).ok_or("Data Fabric register unreadable");
        if bits(need(0, 0x040)?, 27, 24) != 0 {
            return Err("Data Fabric 4 or later");
        }
        if bits(need(1, 0x150)?, 15, 0) != 0 {
            return Err("Data Fabric 3.5");
        }
        let rev = if bits(need(1, 0x208)?, 9, 0) != 0 { Rev::Df3 } else { Rev::Df2 };
        let ctl = if rev == Rev::Df3 { need(0, 0x3F8)? } else { 0 };
        let hash = [ctl >> 20 & 1 == 1, ctl >> 21 & 1 == 1, ctl >> 22 & 1 == 1];
        let hole_base = u64::from(need(0, 0x104)? & 0xFF00_0000);
        let mut maps = Vec::new();
        for num in 0..2u8 {
            let base = need(0, 0x110 + 8 * u16::from(num))?;
            let limit = need(0, 0x114 + 8 * u16::from(num))?;
            if base & 1 == 0 {
                continue;
            }
            let (raw_mode, sel, dies, sockets, dst) = match rev {
                Rev::Df2 => {
                    (bits(base, 7, 4), bits(base, 10, 8), bits(limit, 11, 10), bits(limit, 8, 8), bits(limit, 7, 0))
                }
                Rev::Df3 => {
                    (bits(base, 5, 2), bits(base, 11, 9), bits(base, 7, 6), bits(base, 8, 8), bits(limit, 9, 0))
                }
            };
            if dies != 0 || sockets != 0 {
                return Err("die or socket interleaving");
            }
            let (mode, chans) = match (rev, raw_mode) {
                (_, 0) => (Mode::Plain, 1),
                (_, 1) => (Mode::Plain, 2),
                (Rev::Df2, 8) => (Mode::Df2, 2),
                (Rev::Df3, 3) => (Mode::Plain, 4),
                (Rev::Df3, 5) => (Mode::Plain, 8),
                (Rev::Df3, 0xC) => (Mode::Df3, 2),
                (Rev::Df3, 0xD) => (Mode::Df3, 4),
                (Rev::Df3, 0xE) => (Mode::Df3, 8),
                _ => return Err("unsupported interleave mode"),
            };
            let bit = sel as u8 + 8;
            if mode != Mode::Plain && !(8..=9).contains(&bit) {
                return Err("hashed interleave above bit 9");
            }
            let offset = if num == 0 {
                0
            } else {
                let r = need(0, 0x1B4 + 4 * u16::from(num - 1))?;
                match (r & 1, rev) {
                    (0, _) => 0,
                    (_, Rev::Df2) => u64::from(bits(r, 31, 20)) << 28,
                    (_, Rev::Df3) => u64::from(bits(r, 31, 12)) << 28,
                }
            };
            maps.push(Map {
                num,
                base: u64::from(bits(base, 31, 12)) << 28,
                limit: (u64::from(bits(limit, 31, 12)) << 28) | 0x0FFF_FFFF,
                hole: base & 2 != 0,
                mode,
                chans,
                bit,
                dst: dst as u16,
                offset,
            });
        }
        if maps.is_empty() {
            return Err("no valid DRAM address map");
        }
        if maps.iter().any(|m| m.hole) && hole_base == 0 {
            return Err("MMIO hole enabled without a base");
        }
        Ok(Df { rev, maps, hole_base, hash })
    }

    /// DRAM bytes the maps cover, without the MMIO hole.
    pub fn dram_bytes(&self) -> u64 {
        self.maps
            .iter()
            .map(|m| {
                let hole = if m.hole && m.base < self.hole_base && m.limit >= 1 << 32 {
                    (1 << 32) - self.hole_base
                } else {
                    0
                };
                m.limit + 1 - m.base - hole
            })
            .sum()
    }

    /// Fabric IDs of every channel the maps interleave.
    pub fn channels(&self) -> Vec<u16> {
        let mut v: Vec<u16> = self.maps.iter().flat_map(|m| m.dst..m.dst + m.chans).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Flips the hashed channel bits; its own inverse (atl dehash.c).
    fn hash(&self, m: &Map, a: u64) -> u64 {
        let [h64k, h2m, h1g] = self.hash.map(u64::from);
        match m.mode {
            Mode::Plain => a,
            Mode::Df2 => a ^ ((bit(a, 12) ^ bit(a, 18) ^ bit(a, 21) ^ bit(a, 30)) << m.bit),
            Mode::Df3 => {
                let mut a = a ^ ((bit(a, 14) ^ bit(a, 18) & h64k ^ bit(a, 23) & h2m ^ bit(a, 32) & h1g) << m.bit);
                if m.chans >= 4 {
                    a ^= (bit(a, 16) & h64k ^ bit(a, 21) & h2m ^ bit(a, 30) & h1g) << 12;
                }
                if m.chans >= 8 {
                    a ^= (bit(a, 17) & h64k ^ bit(a, 22) & h2m ^ bit(a, 31) & h1g) << 13;
                }
                a
            }
        }
    }

    fn hole_size(&self, m: &Map) -> u64 {
        if m.hole { (1 << 32) - self.hole_base } else { 0 }
    }

    /// The channel and normalised address of system address `spa`; None outside DRAM.
    pub fn decode(&self, spa: u64) -> Option<Target> {
        let m = self.maps.iter().find(|m| m.base <= spa && spa <= m.limit)?;
        if m.hole && (self.hole_base..1 << 32).contains(&spa) {
            return None;
        }
        let x = self.hash(m, spa);
        let x = if m.hole && x >= self.hole_base { x - self.hole_size(m) } else { x };
        let d = x.checked_sub(m.base)?;
        let n = m.chans.trailing_zeros() as u8;
        let (id, norm) = match m.mode {
            Mode::Plain | Mode::Df2 => ((d >> m.bit) & ((1 << n) - 1), remove(m.bit, n, d)),
            Mode::Df3 => {
                let id = bit(d, m.bit) | (((d >> 12) & ((1 << (n - 1)) - 1)) << 1);
                (id, remove(m.bit, 1, remove(12, n - 1, d)))
            }
        };
        Some(Target { map: m.num, fabric: m.dst + id as u16, norm: norm + m.offset })
    }

    /// The system address of a channel's normalised address (atl norm_to_sys_addr).
    pub fn system(&self, fabric: u16, norm: u64) -> Option<u64> {
        let m = self
            .maps
            .iter()
            .filter(|m| m.offset != 0 && norm >= m.offset)
            .max_by_key(|m| m.offset)
            .or_else(|| self.maps.iter().find(|m| m.num == 0))?;
        let d = norm - m.offset;
        let id = u64::from(fabric.checked_sub(m.dst).filter(|&i| i < m.chans)?);
        let n = m.chans.trailing_zeros() as u8;
        let d = match m.mode {
            Mode::Plain | Mode::Df2 => expand(m.bit, n, d) | (id << m.bit),
            Mode::Df3 => {
                let e = expand(12, n - 1, expand(m.bit, 1, d));
                e | ((id & 1) << m.bit) | ((id >> 1) << 12)
            }
        };
        let x = d + m.base;
        let x = if m.hole && x >= self.hole_base { x + self.hole_size(m) } else { x };
        let spa = self.hash(m, x);
        (spa <= m.limit).then_some(spa)
    }
}

/// Channel letter of a module from "P0 CHANNEL A" in the bank or "DIMM_A1" in the locator; None when they disagree.
pub fn channel_letter(d: &Dimm) -> Option<u8> {
    let after_channel = |s: &str| {
        let u = s.to_ascii_uppercase();
        let at = u.find("CHANNEL")? + "CHANNEL".len();
        u[at..].trim_start_matches([' ', '_', '-']).bytes().next().filter(u8::is_ascii_uppercase)
    };
    let slot = |s: &str| {
        let u = s.trim().to_ascii_uppercase();
        let last = u.rsplit([' ', '_', '-']).next()?.as_bytes();
        match last {
            [l @ b'A'..=b'H', b'0'..=b'9'] => Some(*l),
            _ => None,
        }
    };
    let bank = after_channel(&d.bank);
    let loc = after_channel(&d.locator).or_else(|| slot(&d.locator));
    match (bank, loc) {
        (Some(a), Some(b)) if a != b => None,
        (a, b) => a.or(b),
    }
}

/// The module or channel a memory error most likely sits in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hint {
    /// "DIMM_B2", or "channel A" when the channel holds more than one module.
    pub label: String,
    /// Indices into the module list.
    pub dimms: Vec<usize>,
}

/// DRAM map matched to the SMBIOS modules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Modules {
    df: Df,
    /// Channel index (A = 0) of each module.
    channel: Vec<u8>,
}

impl Modules {
    /// The map, checked against the modules: same total size, one letter per channel, A for UMC 0.
    pub fn new(df: Df, dimms: &[Dimm]) -> Result<Modules, String> {
        if dimms.is_empty() {
            return Err("no memory modules in SMBIOS".into());
        }
        let channel = dimms
            .iter()
            .map(|d| channel_letter(d).map(|l| l - b'A'))
            .collect::<Option<Vec<u8>>>()
            .ok_or("module locators name no channel")?;
        let mib: u64 = dimms.iter().map(|d| d.size_mib).sum();
        let map_mib = df.dram_bytes() >> 20;
        if mib.abs_diff(map_mib) > 256 {
            return Err(format!("map covers {map_mib} MiB, modules {mib} MiB"));
        }
        let mut letters: Vec<u16> = channel.iter().map(|&c| u16::from(c)).collect();
        letters.sort_unstable();
        letters.dedup();
        if letters != df.channels() {
            return Err(format!("map channels {:?}, module channels {letters:?}", df.channels()));
        }
        Ok(Modules { df, channel })
    }

    pub fn df(&self) -> &Df {
        &self.df
    }

    pub fn hint(&self, phys: u64, dimms: &[Dimm]) -> Option<Hint> {
        let t = self.df.decode(phys)?;
        let ch = u8::try_from(t.fabric).ok()?;
        let idx: Vec<usize> = (0..self.channel.len()).filter(|&i| self.channel[i] == ch).collect();
        let label = match idx.as_slice() {
            [one] => dimms.get(*one).map(|d| d.locator.clone()).filter(|l| !l.is_empty())?,
            [] => return None,
            _ => format!("channel {}", char::from(b'A' + ch)),
        };
        Some(Hint { label, dimms: idx })
    }
}

/// The checked map for `dimms`, and log lines: the raw registers on AMD Data Fabric parts, then the outcome.
pub fn setup(dump: &Dump, dimms: &[Dimm]) -> (Option<Modules>, Vec<String>) {
    let mut log = Vec::new();
    if dump.get(0x18, 0, 0x000).is_some_and(|v| v & 0xFFFF == AMD) {
        log.push(format!("DRAM map registers: {dump}"));
    }
    let mods = Df::parse(dump).map_err(String::from).and_then(|df| Modules::new(df, dimms));
    match mods {
        Ok(m) => {
            let letters: Vec<String> =
                m.df.channels().iter().map(|&c| format!("{}", char::from(b'A' + c as u8))).collect();
            log.push(format!("DRAM map: {:?}, channels {}", m.df.rev, letters.join(", ")));
            (Some(m), log)
        }
        Err(e) => {
            log.push(format!("DRAM map: not decoded, {e}"));
            (None, log)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    const VERMEER: u32 = 0x00A2_0F10;

    /// Ryzen 9 5950X-like DF3 map: 64 GiB, hole at 3 GiB, 2 channels hashed at bit 8 with `ctl` hash enables.
    pub(crate) fn bg(mode: u32, sel: u32, ctl: u32, dram_gib: u64) -> Dump {
        let top = (dram_gib + 1) << 30;
        let base = (sel << 9) | (mode << 2) | 0b11;
        let limit = (((top - 1) >> 28) as u32) << 12;
        let regs = [
            (0x18, 0, 0x000, 0x1440_1022),
            (0x18, 0, 0x040, 0x0000_0010),
            (0x18, 0, 0x104, 0xC000_0003),
            (0x18, 0, 0x110, base),
            (0x18, 0, 0x114, limit),
            (0x18, 0, 0x118, 0),
            (0x18, 0, 0x11C, 0),
            (0x18, 0, 0x1B4, 0),
            (0x18, 0, 0x3F8, ctl),
            (0x18, 1, 0x150, 0),
            (0x18, 1, 0x208, 0x0300_003F),
            (0x18, 1, 0x20C, 0x0000_0008),
            (0x19, 0, 0x000, u32::MAX),
        ];
        Dump { cpuid: VERMEER, regs: regs.to_vec() }
    }

    fn dimm(locator: &str, bank: &str, gib: u64) -> Dimm {
        Dimm { locator: locator.into(), bank: bank.into(), size_mib: gib << 10, ..Dimm::default() }
    }

    pub(crate) fn four() -> Vec<Dimm> {
        ["A1", "A2", "B1", "B2"]
            .iter()
            .map(|s| dimm(&format!("DIMM_{s}"), &format!("P0 CHANNEL {}", &s[..1]), 16))
            .collect()
    }

    pub(crate) const ALL_HASH: u32 = 0x0070_0000;
    const CLUSTER1: [u64; 3] = [0xabd16f218, 0xabd168348, 0xabd16af08];
    const CLUSTER2: [u64; 4] = [0xfe78503e0, 0xfe7853db8, 0xfe7857eb0, 0xfe7857cc0];

    #[test]
    fn dump_text_round_trips() {
        let d = bg(0xC, 0, ALL_HASH, 64);
        let s = d.to_string();
        assert!(s.starts_with("cpuid 00a20f10 18.0:000=14401022 18.0:040=00000010"), "{s}");
        assert_eq!(Dump::parse(&s), Some(d));
        assert_eq!(Dump::parse("nonsense"), None);
    }

    #[test]
    fn decodes_and_round_trips_every_mode() {
        for (mode, sel, ctl) in [
            (0xC, 0, ALL_HASH),
            (0xC, 1, 0),
            (0xC, 0, 0x0010_0000),
            (1, 0, 0),
            (1, 3, 0),
            (0, 0, 0),
            (0xD, 0, ALL_HASH),
        ] {
            let df = Df::parse(&bg(mode, sel, ctl, 64)).unwrap();
            assert_eq!(df.dram_bytes(), 64 << 30);
            let mut x = 0x1234_5678_9abcu64;
            for k in 0..20_000u64 {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let spa = (((x >> 20) % (65 << 30)) & !63) | ((k & 1) * 0x40);
                match df.decode(spa) {
                    Some(t) => assert_eq!(df.system(t.fabric, t.norm), Some(spa), "mode {mode:#x} {spa:#x} {t:?}"),
                    None => assert!((0xC000_0000..1 << 32).contains(&spa), "{spa:#x}"),
                }
            }
        }
    }

    #[test]
    fn each_channel_gets_a_dense_normalised_range() {
        let df = Df::parse(&bg(0xC, 0, ALL_HASH, 64)).unwrap();
        let lines = 1u64 << 20;
        let mut seen = vec![[false; 2]; lines as usize / 2];
        for l in 0..lines {
            let t = df.decode(0x2_0000_0000 + l * 256).unwrap();
            let n = (t.norm - 0xE000_0000) / 256;
            let slot = &mut seen[n as usize][t.fabric as usize];
            assert!(!*slot, "line {l} maps twice");
            *slot = true;
        }
    }

    #[test]
    fn bg_clusters_land_on_one_channel_each() {
        let mods = Modules::new(Df::parse(&bg(0xC, 0, ALL_HASH, 64)).unwrap(), &four()).unwrap();
        for c in [&CLUSTER1[..], &CLUSTER2[..]] {
            let ch: Vec<u16> = c.iter().map(|&a| mods.df().decode(a).unwrap().fabric).collect();
            assert!(ch.windows(2).all(|w| w[0] == w[1]), "{ch:?}");
            let h = mods.hint(c[0], &four()).unwrap();
            assert_eq!((h.label.as_str(), h.dimms.as_slice()), ("channel A", &[0, 1][..]));
        }
        let none = Modules::new(Df::parse(&bg(0xC, 0, 0, 64)).unwrap(), &four()).unwrap();
        assert_eq!(none.hint(CLUSTER1[0], &four()).unwrap().label, "channel B");
        let plain = Df::parse(&bg(1, 0, 0, 64)).unwrap();
        let ch: Vec<u16> = CLUSTER1.iter().map(|&a| plain.decode(a).unwrap().fabric).collect();
        assert_eq!(ch, [0, 1, 1], "unhashed interleave splits the cluster");
    }

    #[test]
    fn one_module_per_channel_names_the_slot() {
        let two = vec![dimm("DIMM_A2", "P0 CHANNEL A", 16), dimm("DIMM_B2", "P0 CHANNEL B", 16)];
        let mods = Modules::new(Df::parse(&bg(0xC, 0, ALL_HASH, 32)).unwrap(), &two).unwrap();
        let h = mods.hint(0x7fdf504c8, &two).unwrap();
        assert_eq!((h.label.as_str(), h.dimms.as_slice()), ("DIMM_B2", &[1][..]));
        assert_eq!(mods.hint(0xC000_1000, &two), None);
    }

    #[test]
    fn refuses_what_it_cannot_check() {
        let df = || Df::parse(&bg(0xC, 0, ALL_HASH, 64)).unwrap();
        assert!(Modules::new(df(), &four()[..3]).unwrap_err().contains("MiB"));
        let mut odd = four();
        odd[0].bank = "P0 CHANNEL B".into();
        assert_eq!(Modules::new(df(), &odd).unwrap_err(), "module locators name no channel");
        let unnamed: Vec<Dimm> = (0..4).map(|k| dimm(&format!("DIMM {k}"), "", 16)).collect();
        assert!(Modules::new(df(), &unnamed).is_err());
        let mut d = bg(0xC, 0, ALL_HASH, 64);
        d.cpuid = 0x0009_06A3;
        assert_eq!(Df::parse(&d), Err("not an AMD family 17h or 19h CPU"));
        let mut d = bg(0xC, 0, ALL_HASH, 64);
        d.regs.retain(|r| r.0 != 0x18);
        assert_eq!(Df::parse(&d), Err("no AMD Data Fabric at 00:18.0"));
        let mut d = bg(0xC, 0, ALL_HASH, 64);
        d.regs.last_mut().unwrap().3 = 0x1440_1022;
        assert_eq!(Df::parse(&d), Err("more than one Data Fabric node"));
        let mut d = bg(0xC, 0, ALL_HASH, 64);
        d.regs[1].3 = 0x0400_0000;
        assert_eq!(Df::parse(&d), Err("Data Fabric 4 or later"));
        assert_eq!(Df::parse(&bg(6, 0, 0, 64)), Err("unsupported interleave mode"));
        assert_eq!(Df::parse(&bg(0xC, 2, 0, 64)), Err("hashed interleave above bit 9"));
    }

    #[test]
    fn setup_logs_the_registers_on_amd_only() {
        let (m, log) = setup(&bg(0xC, 0, ALL_HASH, 64), &four());
        assert!(m.is_some());
        assert!(log[0].starts_with("DRAM map registers: cpuid 00a20f10 18.0:000=14401022"), "{log:?}");
        assert_eq!(log[1], "DRAM map: Df3, channels A, B");
        let qemu = Dump::read(VERMEER, |_, _, _| Some(u32::MAX));
        let (m, log) = setup(&qemu, &four());
        assert_eq!((m, log), (None, vec![String::from("DRAM map: not decoded, no AMD Data Fabric at 00:18.0")]));
    }

    #[test]
    fn reads_channel_letters() {
        let l = |loc: &str, bank: &str| channel_letter(&dimm(loc, bank, 1)).map(char::from);
        assert_eq!(l("DIMM_B2", "P0 CHANNEL B"), Some('B'));
        assert_eq!(l("DDR4_A1", "BANK 0"), Some('A'));
        assert_eq!(l("ChannelA-DIMM0", "BANK 0"), Some('A'));
        assert_eq!(l("DIMM 0", "P0_Node0_Channel1_Dimm0"), None);
        assert_eq!(l("A2", ""), Some('A'));
        assert_eq!(l("DIMM_A1", "P0 CHANNEL B"), None);
    }

    #[test]
    fn df2_hash_round_trips() {
        let mut d = bg(0, 0, 0, 16);
        d.regs.iter_mut().find(|r| r.2 == 0x208).unwrap().3 = 0;
        let limit = d.regs.iter().find(|r| r.2 == 0x114).unwrap().3;
        d.regs.iter_mut().find(|r| r.2 == 0x110).unwrap().3 = (8 << 4) | 0b11;
        d.regs.iter_mut().find(|r| r.2 == 0x114).unwrap().3 = limit;
        let df = Df::parse(&d).unwrap();
        assert_eq!(df.rev, Rev::Df2);
        for spa in (0x1_0000_0000u64..0x1_0100_0000).step_by(0x1040) {
            let t = df.decode(spa).unwrap();
            assert_eq!(df.system(t.fabric, t.norm), Some(spa));
        }
    }
}
