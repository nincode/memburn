//! Memory modules from the SMBIOS table (type 17, Memory Device), DSP0134.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// One populated memory slot.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Dimm {
    /// Slot name on the board, e.g. "DIMM_A1".
    pub locator: String,
    pub bank: String,
    pub size_mib: u64,
    /// Rated speed, MT/s; 0 when unknown.
    pub speed: u32,
    /// Speed the memory runs at, MT/s; 0 when unknown.
    pub configured: u32,
    pub manufacturer: String,
    pub part: String,
    pub serial: String,
}

impl Dimm {
    /// "DIMM_A1  16 GiB  5600 MT/s (runs at 4800)  Kingston KF556C40-16  SN 1A2B3C4D"
    pub fn line(&self) -> String {
        let mut parts = Vec::new();
        parts.push(if self.bank.is_empty() {
            self.locator.clone()
        } else {
            format!("{} / {}", self.bank, self.locator)
        });
        parts.push(size(self.size_mib));
        match (self.speed, self.configured) {
            (0, 0) => {}
            (s, c) if c == 0 || c == s => parts.push(format!("{s} MT/s")),
            (0, c) => parts.push(format!("runs at {c} MT/s")),
            (s, c) => parts.push(format!("{s} MT/s (runs at {c})")),
        }
        let make = [self.manufacturer.as_str(), self.part.as_str()]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" ");
        if !make.is_empty() {
            parts.push(make);
        }
        if !self.serial.is_empty() {
            parts.push(format!("SN {}", self.serial));
        }
        parts.join("  ")
    }
}

/// "512 MiB", "16 GiB".
pub fn size(mib: u64) -> String {
    if mib >= 1024 && mib.is_multiple_of(1024) { format!("{} GiB", mib / 1024) } else { format!("{mib} MiB") }
}

const MEMORY_DEVICE: u8 = 17;
const END: u8 = 127;

/// Populated slots in a raw structure table, in table order.
pub fn dimms(table: &[u8]) -> Vec<Dimm> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 4 <= table.len() {
        let (kind, len) = (table[at], table[at + 1] as usize);
        if len < 4 || at + len > table.len() {
            break;
        }
        let body = &table[at..at + len];
        let strings_at = at + len;
        let mut end = strings_at;
        while end + 1 < table.len() && !(table[end] == 0 && table[end + 1] == 0) {
            end += 1;
        }
        let strings = &table[strings_at..end.min(table.len())];
        if kind == MEMORY_DEVICE
            && let Some(d) = device(body, strings)
        {
            out.push(d);
        }
        if kind == END {
            break;
        }
        at = end + 2;
    }
    out
}

fn device(s: &[u8], strings: &[u8]) -> Option<Dimm> {
    let b = |o: usize| s.get(o).copied();
    let w = |o: usize| Some(u16::from_le_bytes([b(o)?, b(o + 1)?]));
    let d = |o: usize| Some(u32::from_le_bytes([b(o)?, b(o + 1)?, b(o + 2)?, b(o + 3)?]));
    let text = |o: usize| b(o).map_or(String::new(), |n| string(strings, n));
    let size_mib = match w(0x0C)? {
        0 | 0xFFFF => return None,
        0x7FFF => u64::from(d(0x1C)? & 0x7FFF_FFFF),
        v if v & 0x8000 != 0 => u64::from(v & 0x7FFF) / 1024,
        v => u64::from(v),
    };
    let speed = |o: usize, ext: usize| match w(o) {
        Some(0xFFFF) => d(ext).unwrap_or(0),
        Some(v) => u32::from(v),
        None => 0,
    };
    Some(Dimm {
        locator: text(0x10),
        bank: text(0x11),
        size_mib,
        speed: speed(0x15, 0x54),
        configured: speed(0x20, 0x58),
        manufacturer: text(0x17),
        part: text(0x1A),
        serial: text(0x18),
    })
}

/// String `n` (1-based) of a structure, trimmed; placeholder vendor strings read as empty.
fn string(strings: &[u8], n: u8) -> String {
    if n == 0 {
        return String::new();
    }
    let Some(raw) = strings.split(|&c| c == 0).nth(n as usize - 1) else { return String::new() };
    let t: String = raw.iter().map(|&c| if (0x20..0x7F).contains(&c) { c as char } else { ' ' }).collect();
    let t = t.trim();
    let blank = ["Unknown", "Not Specified", "NO DIMM", "None", "To Be Filled By O.E.M."];
    if blank.iter().any(|x| t.eq_ignore_ascii_case(x)) || t.bytes().all(|c| c == b'0') {
        String::new()
    } else {
        String::from(t)
    }
}

/// Structure table (address, length) from an SMBIOS 3 ("_SM3_") or 2 ("_SM_") entry point.
pub fn table_at(entry: &[u8]) -> Option<(u64, usize)> {
    let w = |o: usize| Some(u16::from_le_bytes(entry.get(o..o + 2)?.try_into().ok()?));
    let d = |o: usize| Some(u32::from_le_bytes(entry.get(o..o + 4)?.try_into().ok()?));
    let q = |o: usize| Some(u64::from_le_bytes(entry.get(o..o + 8)?.try_into().ok()?));
    if entry.starts_with(b"_SM3_") {
        Some((q(0x10)?, d(0x0C)? as usize))
    } else if entry.starts_with(b"_SM_") && entry.get(0x10..0x15) == Some(b"_DMI_") {
        Some((u64::from(d(0x18)?), w(0x16)? as usize))
    } else {
        None
    }
}

/// Upper bound on an entry point's length, for reading it before its length field is known.
pub const ENTRY_LEN: usize = 0x20;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// A structure: header, formatted area of `len` bytes with `set` patches, strings.
    fn structure(kind: u8, len: usize, set: &[(usize, &[u8])], strings: &[&str]) -> Vec<u8> {
        let mut v = vec![0u8; len];
        v[0] = kind;
        v[1] = len as u8;
        for (o, bytes) in set.iter().filter(|(o, b)| o + b.len() <= len) {
            v[*o..*o + bytes.len()].copy_from_slice(bytes);
        }
        for s in strings {
            v.extend_from_slice(s.as_bytes());
            v.push(0);
        }
        if strings.is_empty() {
            v.push(0);
        }
        v.push(0);
        v
    }

    fn dimm(len: usize, size: u16, ext: u32, speed: u16, conf: u16, strings: &[&str]) -> Vec<u8> {
        structure(
            17,
            len,
            &[
                (0x0C, &size.to_le_bytes()),
                (0x10, &[1, 2]),
                (0x15, &speed.to_le_bytes()),
                (0x17, &[3, 4, 0, 5]),
                (0x1C, &ext.to_le_bytes()),
                (0x20, &conf.to_le_bytes()),
            ],
            strings,
        )
    }

    #[test]
    fn reads_memory_devices() {
        let mut t = structure(0, 0x18, &[(0x04, &[1])], &["Vendor"]);
        t.extend(dimm(0x28, 16384, 0, 5600, 4800, &["DIMM_A1", "BANK 0", "Kingston", "1A2B3C4D", "KF556C40-16   "]));
        t.extend(dimm(0x28, 0, 0, 0, 0, &["DIMM_A2", "BANK 1", "Unknown", "Unknown", "Unknown"]));
        t.extend(dimm(0x28, 0x7FFF, 65536, 6400, 6400, &["DIMM_B1", "BANK 2", "Samsung", "00000000", "M321R8GA0PB0"]));
        t.extend(dimm(0x1C, 0x8000 | 512, 0, 0, 0, &["SLOT 1"]));
        t.extend(structure(127, 4, &[], &[]));
        t.extend(dimm(0x28, 8192, 0, 0, 0, &["AFTER END"]));
        let d = dimms(&t);
        assert_eq!(d.len(), 3, "{d:#?}");
        assert_eq!(
            d[0],
            Dimm {
                locator: "DIMM_A1".into(),
                bank: "BANK 0".into(),
                size_mib: 16384,
                speed: 5600,
                configured: 4800,
                manufacturer: "Kingston".into(),
                part: "KF556C40-16".into(),
                serial: "1A2B3C4D".into(),
            }
        );
        assert_eq!(
            d[0].line(),
            "BANK 0 / DIMM_A1  16 GiB  5600 MT/s (runs at 4800)  Kingston KF556C40-16  SN 1A2B3C4D"
        );
        assert_eq!((d[1].size_mib, d[1].serial.as_str()), (65536, ""));
        assert_eq!(d[1].line(), "BANK 2 / DIMM_B1  64 GiB  6400 MT/s  Samsung M321R8GA0PB0");
        assert_eq!(d[2].line(), "SLOT 1  0 MiB");
    }

    #[test]
    fn truncated_tables_stop_cleanly() {
        let t = dimm(0x28, 4096, 0, 3200, 3200, &["A", "B", "C", "D", "E"]);
        for n in 0..t.len() {
            let _ = dimms(&t[..n]);
        }
        assert_eq!(dimms(&t).len(), 1);
    }

    #[test]
    fn finds_the_table_from_either_entry_point() {
        let mut e3 = vec![0u8; 0x18];
        e3[..5].copy_from_slice(b"_SM3_");
        e3[0x0C..0x10].copy_from_slice(&0x1234u32.to_le_bytes());
        e3[0x10..0x18].copy_from_slice(&0x7_8000_0000u64.to_le_bytes());
        assert_eq!(table_at(&e3), Some((0x7_8000_0000, 0x1234)));
        let mut e2 = vec![0u8; 0x1F];
        e2[..4].copy_from_slice(b"_SM_");
        e2[0x10..0x15].copy_from_slice(b"_DMI_");
        e2[0x16..0x18].copy_from_slice(&0x0ABCu16.to_le_bytes());
        e2[0x18..0x1C].copy_from_slice(&0x000F_0000u32.to_le_bytes());
        assert_eq!(table_at(&e2), Some((0xF_0000, 0xABC)));
        assert_eq!(table_at(b"nothing here"), None);
    }
}
