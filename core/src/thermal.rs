//! CPU temperature register decodes, as the Linux k10temp and coretemp drivers read them.

/// AMD family 17h+ SMN address of the reported temperature control register.
pub const AMD_SMN_TEMP: u32 = 0x0005_9800;
pub const MSR_TEMPERATURE_TARGET: u32 = 0x1A2;
pub const MSR_THERM_STATUS: u32 = 0x19C;
pub const MSR_PACKAGE_THERM_STATUS: u32 = 0x1B1;

/// Tctl in millidegrees: bits 31:21 × 0.125 °C, less 49 °C with range-select (bit 19) or both Tj-select bits (17:16).
pub fn amd_tctl_mc(reg: u32) -> i32 {
    let t = (reg >> 21) as i32 * 125;
    if reg & (1 << 19) != 0 || reg & (3 << 16) == 3 << 16 { t - 49_000 } else { t }
}

/// k10temp's Tctl offsets, millidegrees, for family 17h CPUs whose Tctl reads above the die temperature.
pub fn amd_tctl_offset_mc(family: u32, brand: &str) -> i32 {
    const TABLE: [(&str, i32); 6] = [
        ("AMD Ryzen 5 1600X", 20_000),
        ("AMD Ryzen 7 1700X", 20_000),
        ("AMD Ryzen 7 1800X", 20_000),
        ("AMD Ryzen 7 2700X", 10_000),
        ("AMD Ryzen Threadripper 19", 27_000),
        ("AMD Ryzen Threadripper 29", 27_000),
    ];
    if family != 0x17 {
        return 0;
    }
    TABLE.iter().find(|(name, _)| brand.contains(name)).map_or(0, |&(_, off)| off)
}

/// Die temperature in °C from the SMN register value.
pub fn amd_temp_c(reg: u32, family: u32, brand: &str) -> i32 {
    (amd_tctl_mc(reg) - amd_tctl_offset_mc(family, brand)).div_euclid(1000)
}

/// TjMax in °C from IA32_TEMPERATURE_TARGET bits 23:16; 100 when it reads 0.
pub fn intel_tjmax(target: u64) -> i32 {
    match (target >> 16) & 0xFF {
        0 => 100,
        t => t as i32,
    }
}

/// °C from a (package) thermal status MSR: TjMax less the digital readout in bits 22:16.
/// `need_valid` is for the per-core MSR, whose bit 31 marks the readout valid.
pub fn intel_temp_c(status: u64, target: u64, need_valid: bool) -> Option<i32> {
    if need_valid && status & (1 << 31) == 0 {
        return None;
    }
    Some(intel_tjmax(target) - ((status >> 16) & 0x7F) as i32)
}

/// CPUID family: base family plus extended family when the base is 0xF.
pub fn family(eax1: u32) -> u32 {
    let base = (eax1 >> 8) & 0xF;
    if base == 0xF { base + ((eax1 >> 20) & 0xFF) } else { base }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amd_decodes() {
        assert_eq!(amd_tctl_mc(500 << 21), 62_500);
        assert_eq!(amd_tctl_mc((500 << 21) | (1 << 19)), 13_500);
        assert_eq!(amd_tctl_mc((500 << 21) | (3 << 16)), 13_500);
        assert_eq!(amd_tctl_mc((500 << 21) | (1 << 16)), 62_500);
        assert_eq!(amd_temp_c(500 << 21, 0x19, "AMD Ryzen 9 5950X 16-Core Processor"), 62);
        assert_eq!(amd_temp_c(600 << 21, 0x17, "AMD Ryzen 7 1700X Eight-Core Processor"), 55);
        assert_eq!(amd_temp_c(600 << 21, 0x17, "AMD Ryzen 7 2700X Eight-Core Processor"), 65);
        assert_eq!(amd_temp_c(600 << 21, 0x17, "AMD Ryzen Threadripper 2950X 16-Core Processor"), 48);
        assert_eq!(amd_temp_c(600 << 21, 0x19, "AMD Ryzen 7 1700X"), 75);
        assert_eq!(amd_temp_c(100 << 21 | 1 << 19, 0x19, ""), -37);
    }

    #[test]
    fn intel_decodes() {
        let target = 100 << 16;
        assert_eq!(intel_temp_c(0x8823_0000, target, true), Some(65));
        assert_eq!(intel_temp_c(0x0823_0000, target, true), None);
        assert_eq!(intel_temp_c(0x0823_0000, target, false), Some(65));
        assert_eq!(intel_temp_c(0x8823_0000, 105 << 16, true), Some(70));
        assert_eq!(intel_temp_c(0x8800_0000, 0, true), Some(100));
    }

    #[test]
    fn families() {
        assert_eq!(family(0x00A2_0F12), 0x19);
        assert_eq!(family(0x0080_0F82), 0x17);
        assert_eq!(family(0x0009_06A3), 6);
    }
}
