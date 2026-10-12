use alloc::vec::Vec;
use core::ptr::NonNull;

use memburn_core::coverage::Ranges;

use uefi::boot::{self, AllocateType, MemoryType};
use uefi::mem::memory_map::MemoryMap;

const PAGE: u64 = 4096;
pub const RESERVE_BYTES: u64 = 64 << 20;
/// Left free for firmware allocations while the test owns everything else.
const RESERVE_PAGES: usize = (RESERVE_BYTES / PAGE) as usize;
const PIECE_PAGES: u64 = (64 << 20) / PAGE;
/// Real-mode area stays untouched; MP wake-up code lives there.
const LOW_LIMIT: u64 = 1 << 20;

/// Memory owned by the test: `(physical address, bytes)`, identity-mapped.
pub struct Claimed {
    pub regions: Vec<(usize, usize)>,
    /// Where the firmware's reserve was while the regions were claimed.
    reserve: Option<u64>,
}

/// RAM an OS would call System RAM: what the EFI stub hands Linux as E820 RAM.
pub fn system_ram() -> Ranges {
    let Ok(map) = boot::memory_map(MemoryType::LOADER_DATA) else {
        return Ranges::default();
    };
    Ranges::new(
        map.entries()
            .filter(|d| {
                matches!(
                    d.ty,
                    MemoryType::CONVENTIONAL
                        | MemoryType::LOADER_CODE
                        | MemoryType::LOADER_DATA
                        | MemoryType::BOOT_SERVICES_CODE
                        | MemoryType::BOOT_SERVICES_DATA
                )
            })
            .map(|d| (d.phys_start, d.phys_start + d.page_count * PAGE))
            .collect(),
    )
}

/// Bytes the firmware reports as RAM an OS could use.
pub fn usable_total() -> u64 {
    let Ok(map) = boot::memory_map(MemoryType::LOADER_DATA) else {
        return 0;
    };
    map.entries()
        .filter(|d| {
            matches!(
                d.ty,
                MemoryType::CONVENTIONAL
                    | MemoryType::LOADER_CODE
                    | MemoryType::LOADER_DATA
                    | MemoryType::BOOT_SERVICES_CODE
                    | MemoryType::BOOT_SERVICES_DATA
                    | MemoryType::ACPI_RECLAIM
            )
        })
        .map(|d| d.page_count * PAGE)
        .sum()
}

/// Allocates every free page above 1 MiB, up to `limit` bytes.
pub fn claim(limit: u64) -> Claimed {
    let reserve = boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, RESERVE_PAGES).ok();
    let mut free: Vec<(u64, u64)> = Vec::with_capacity(1024);
    let mut regions: Vec<(usize, usize)> = Vec::with_capacity(4096);
    if let Ok(map) = boot::memory_map(MemoryType::LOADER_DATA) {
        for d in map.entries().filter(|d| d.ty == MemoryType::CONVENTIONAL) {
            let start = d.phys_start.max(LOW_LIMIT);
            let end = d.phys_start + d.page_count * PAGE;
            if start < end && free.len() < free.capacity() {
                free.push((start, end));
            }
        }
    }
    let mut left = limit / PAGE;
    'outer: for (start, end) in free {
        let mut a = start;
        while a < end {
            if left == 0 || regions.len() == regions.capacity() {
                break 'outer;
            }
            let pages = ((end - a) / PAGE).min(PIECE_PAGES).min(left);
            if boot::allocate_pages(AllocateType::Address(a), MemoryType::LOADER_DATA, pages as usize).is_ok() {
                left -= pages;
                match regions.last_mut() {
                    Some(r) if r.0 + r.1 == a as usize => r.1 += (pages * PAGE) as usize,
                    _ => regions.push((a as usize, (pages * PAGE) as usize)),
                }
            }
            a += pages * PAGE;
        }
    }
    if let Some(p) = reserve {
        let _ = unsafe { boot::free_pages(p, RESERVE_PAGES) };
    }
    Claimed { regions, reserve: reserve.map(|p| p.as_ptr() as u64) }
}

impl Claimed {
    pub fn bytes(&self) -> u64 {
        self.regions.iter().map(|r| r.1 as u64).sum()
    }

    pub fn ranges(&self) -> Ranges {
        Ranges::new(self.regions.iter().map(|&(a, n)| (a as u64, (a + n) as u64)).collect())
    }

    /// Hands a tested 64 MiB piece to the firmware and claims what is free of the old reserve; returns those regions.
    pub fn rotate(&mut self) -> Vec<(usize, usize)> {
        let Some(reserve) = self.reserve else { return Vec::new() };
        let bytes = RESERVE_BYTES as usize;
        if let Some(r) = self.regions.iter_mut().rev().find(|r| r.1 >= bytes)
            && let Some(p) = NonNull::new((r.0 + r.1 - bytes) as *mut u8)
            && unsafe { boot::free_pages(p, RESERVE_PAGES) }.is_ok()
        {
            r.1 -= bytes;
        }
        self.regions.retain(|r| r.1 > 0);
        let window = (reserve, reserve + RESERVE_BYTES);
        let free: Vec<(u64, u64)> = boot::memory_map(MemoryType::LOADER_DATA)
            .map(|map| {
                map.entries()
                    .filter(|d| d.ty == MemoryType::CONVENTIONAL)
                    .map(|d| (d.phys_start.max(window.0), (d.phys_start + d.page_count * PAGE).min(window.1)))
                    .filter(|&(s, e)| s < e)
                    .collect()
            })
            .unwrap_or_default();
        let mut got = Vec::new();
        for (s, e) in free {
            let pages = ((e - s) / PAGE) as usize;
            if boot::allocate_pages(AllocateType::Address(s), MemoryType::LOADER_DATA, pages).is_ok() {
                got.push((s as usize, pages * PAGE as usize));
            }
        }
        self.regions.extend(&got);
        got
    }

    pub fn release(self) {
        for (addr, len) in self.regions {
            if let Some(p) = NonNull::new(addr as *mut u8) {
                let _ = unsafe { boot::free_pages(p, len / PAGE as usize) };
            }
        }
    }
}
