use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use memburn_core::{cpu, cycler};
use uefi::boot::{self, EventType, ScopedProtocol, Tpl};
use uefi::proto::pi::mp::MpServices;

pub struct Smp {
    mp: Option<ScopedProtocol<MpServices>>,
    /// CPUs used, BSP included.
    pub cpus: usize,
}

struct ApTask<'a> {
    f: &'a (dyn Fn(usize) + Sync),
    next: AtomicUsize,
    limit: usize,
}

extern "efiapi" fn ap_entry(arg: *mut c_void) {
    let task = unsafe { &*(arg as *const ApTask) };
    let idx = task.next.fetch_add(1, Ordering::Relaxed);
    if idx >= task.limit {
        return;
    }
    unsafe { cpu::enable_simd() };
    (task.f)(idx);
}

struct OneTask<'a> {
    f: &'a (dyn Fn() + Sync),
}

extern "efiapi" fn one_entry(arg: *mut c_void) {
    let task = unsafe { &*(arg as *const OneTask) };
    unsafe { cpu::enable_simd() };
    (task.f)();
}

impl Smp {
    pub fn new(limit: Option<usize>) -> Smp {
        let mp =
            boot::get_handle_for_protocol::<MpServices>().and_then(boot::open_protocol_exclusive::<MpServices>).ok();
        let enabled = mp.as_ref().and_then(|m| m.get_number_of_processors().ok()).map_or(1, |c| c.enabled);
        let cpus = limit.map_or(enabled, |l| l.min(enabled)).max(1);
        Smp { mp, cpus }
    }

    /// Processor numbers in use with their row labels, "CPU 2 (core 1, thread 0)", in APIC id order.
    pub fn order(&self) -> Vec<(usize, String)> {
        let Some(mp) = &self.mp else { return alloc::vec![(0, String::from("CPU 0"))] };
        let info: Vec<(usize, u64, String)> = (0..self.cpus)
            .map(|n| match mp.get_processor_info(n) {
                Ok(i) => {
                    (n, i.processor_id, format!("CPU {n} (core {}, thread {})", i.location.core, i.location.thread))
                }
                Err(_) => (n, n as u64, format!("CPU {n}")),
            })
            .collect();
        let ids: Vec<(usize, u64)> = info.iter().map(|&(n, id, _)| (n, id)).collect();
        cycler::order_by_apic(&ids).into_iter().map(|n| (n, info[n].2.clone())).collect()
    }

    /// Processor number of the BSP.
    pub fn bsp(&self) -> usize {
        self.mp.as_ref().and_then(|m| m.who_am_i().ok()).unwrap_or(0)
    }

    /// Runs `f` on AP `n`, calling `tick` on the BSP until it returns; false when the AP could not start.
    pub fn run_on(&self, n: usize, f: &(dyn Fn() + Sync), mut tick: impl FnMut()) -> bool {
        let Some(mp) = &self.mp else { return false };
        let task = OneTask { f };
        let Ok(ev) = (unsafe { boot::create_event(EventType::empty(), Tpl::CALLBACK, None, None) }) else {
            return false;
        };
        let arg = &task as *const OneTask as *mut c_void;
        if mp.startup_this_ap(n, one_entry, arg, Some(unsafe { ev.unsafe_clone() }), None).is_err() {
            let _ = boot::close_event(ev);
            return false;
        }
        while !boot::check_event(&ev).unwrap_or(true) {
            tick();
            boot::stall(Duration::from_micros(500));
        }
        let _ = boot::close_event(ev);
        true
    }

    /// Runs `ap(i)` on APs 1..cpus while the BSP loops `bsp()` until false, calling `tick` in between.
    pub fn run(&self, ap: &(dyn Fn(usize) + Sync), mut bsp: impl FnMut() -> bool, mut tick: impl FnMut()) {
        let task = ApTask { f: ap, next: AtomicUsize::new(1), limit: self.cpus };
        let done = match &self.mp {
            Some(mp) if self.cpus > 1 => {
                unsafe { boot::create_event(EventType::empty(), Tpl::CALLBACK, None, None) }.ok().and_then(|ev| {
                    let arg = &task as *const ApTask as *mut c_void;
                    match mp.startup_all_aps(false, ap_entry, arg, Some(unsafe { ev.unsafe_clone() }), None) {
                        Ok(()) => Some(ev),
                        Err(_) => {
                            let _ = boot::close_event(ev);
                            None
                        }
                    }
                })
            }
            _ => None,
        };
        while bsp() {
            tick();
        }
        if let Some(ev) = done {
            while !boot::check_event(&ev).unwrap_or(true) {
                tick();
                boot::stall(Duration::from_micros(500));
            }
            let _ = boot::close_event(ev);
        }
    }
}
