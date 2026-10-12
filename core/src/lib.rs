#![cfg_attr(not(test), no_std)]
#![allow(clippy::missing_safety_doc)]

extern crate alloc;

pub mod coverage;
pub mod cpu;
pub mod cycler;
pub mod dram;
pub mod engine;
pub mod errlog;
pub mod eta;
pub mod fft;
pub mod font;
pub mod hash;
pub mod history;
pub mod memtest;
pub mod rng;
pub mod smbios;
pub mod thermal;
pub mod tui;
