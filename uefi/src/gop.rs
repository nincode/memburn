use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use memburn_core::font;
use memburn_core::tui::Cell;
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol, SearchType};
use uefi::proto::console::gop::{BltOp, BltPixel, BltRegion, GraphicsOutput, PixelFormat};
use uefi::proto::device_path::DevicePath;
use uefi::proto::unsafe_protocol;
use uefi::{Handle, Status};

/// VGA palette in `tui::Color` order, 0xRRGGBB.
const PALETTE: [u32; 16] = [
    0x000000, 0x0000aa, 0x00aa00, 0x00aaaa, 0xaa0000, 0xaa00aa, 0xaa5500, 0xaaaaaa, 0x555555, 0x5555ff, 0x55ff55,
    0x55ffff, 0xff5555, 0xff55ff, 0xffff55, 0xffffff,
];
const TARGET: (usize, usize) = (1920, 1080);

/// EFI_PCI_IO_PROTOCOL up to GetLocation.
#[repr(C)]
#[unsafe_protocol("4cf5b200-68b8-4ca5-9eec-b23e3f50029a")]
struct PciIo {
    _calls: [usize; 14],
    get_location: unsafe extern "efiapi" fn(*mut PciIo, *mut usize, *mut usize, *mut usize, *mut usize) -> Status,
}

/// One screen driven through its GraphicsOutput protocol.
pub struct Display {
    gop: ScopedProtocol<GraphicsOutput>,
    pub width: usize,
    pub height: usize,
    /// Pixels per framebuffer row; null `fb` draws with blt.
    stride: usize,
    fb: *mut u32,
    rgb: bool,
    /// Glyph pixels are scale × scale screen pixels.
    pub scale: usize,
    origin: (usize, usize),
    buf: Vec<u32>,
    /// "PCI 03:00.0", or "" when the device is not on PCI.
    pub pci: String,
}

/// Connects every PCI device's drivers, then opens each GOP with a device path, one per framebuffer.
pub fn find() -> Vec<Display> {
    if let Ok(hs) = boot::locate_handle_buffer(SearchType::from_proto::<PciIo>()) {
        for &h in hs.iter() {
            let _ = boot::connect_controller(h, &[], None, true);
        }
    }
    let Ok(hs) = boot::locate_handle_buffer(SearchType::from_proto::<GraphicsOutput>()) else { return Vec::new() };
    let mut out: Vec<Display> = Vec::new();
    let mut bases = Vec::new();
    for &h in hs.iter() {
        if open::<DevicePath>(h).is_none() {
            continue;
        }
        let Some(mut gop) = open::<GraphicsOutput>(h) else { continue };
        let blt_only = gop.current_mode_info().pixel_format() == PixelFormat::BltOnly;
        let base = if blt_only { 0 } else { gop.frame_buffer().as_mut_ptr() as usize };
        if base != 0 && bases.contains(&base) {
            continue;
        }
        bases.push(base);
        pick_mode(&mut gop);
        let info = gop.current_mode_info();
        let (width, height) = info.resolution();
        let fb = match info.pixel_format() {
            PixelFormat::Rgb | PixelFormat::Bgr => gop.frame_buffer().as_mut_ptr() as *mut u32,
            _ => core::ptr::null_mut(),
        };
        let scale = (width / TARGET.0).min(height / TARGET.1).max(1);
        out.push(Display {
            rgb: info.pixel_format() == PixelFormat::Rgb,
            stride: info.stride(),
            gop,
            width,
            height,
            fb,
            scale,
            origin: (0, 0),
            buf: Vec::new(),
            pci: pci_name(h),
        });
    }
    out
}

/// Cells that fit on every display, centred on each; clears them all.
pub fn grid(displays: &mut [Display]) -> (usize, usize) {
    let cw = |d: &Display| font::WIDTH * d.scale;
    let ch = |d: &Display| font::HEIGHT * d.scale;
    let cols = displays.iter().map(|d| d.width / cw(d)).min().unwrap_or(0);
    let rows = displays.iter().map(|d| d.height / ch(d)).min().unwrap_or(0);
    for d in displays.iter_mut() {
        d.origin = ((d.width - cols * cw(d)) / 2, (d.height - rows * ch(d)) / 2);
        d.clear();
    }
    (cols, rows)
}

fn open<P: uefi::proto::ProtocolPointer + ?Sized>(handle: Handle) -> Option<ScopedProtocol<P>> {
    let params = OpenProtocolParams { handle, agent: boot::image_handle(), controller: None };
    unsafe { boot::open_protocol::<P>(params, OpenProtocolAttributes::GetProtocol) }.ok()
}

/// The mode closest to 1920×1080, unless the current one is at least that.
fn pick_mode(gop: &mut GraphicsOutput) {
    let cur = gop.current_mode_info().resolution();
    if cur.0 >= TARGET.0 && cur.1 >= TARGET.1 {
        return;
    }
    let best = gop.modes().min_by_key(|m| {
        let (w, h) = m.info().resolution();
        w.abs_diff(TARGET.0) + h.abs_diff(TARGET.1)
    });
    if let Some(m) = best
        && m.info().resolution() != cur
    {
        let _ = gop.set_mode(&m);
    }
}

fn pci_name(h: Handle) -> String {
    let Some(path) = open::<DevicePath>(h) else { return String::new() };
    let mut dp: &DevicePath = &path;
    let Ok(ph) = boot::locate_device_path::<PciIo>(&mut dp) else { return String::new() };
    let Some(mut pci) = open::<PciIo>(ph) else { return String::new() };
    let (mut seg, mut bus, mut dev, mut fun) = (0, 0, 0, 0);
    let p: &mut PciIo = &mut pci;
    let st = unsafe { (p.get_location)(p, &mut seg, &mut bus, &mut dev, &mut fun) };
    if st.is_error() {
        return String::new();
    }
    if seg == 0 {
        format!("PCI {bus:02x}:{dev:02x}.{fun:x}")
    } else {
        format!("PCI {seg:04x}:{bus:02x}:{dev:02x}.{fun:x}")
    }
}

impl Display {
    fn pixel(&self, c: u8) -> u32 {
        let v = PALETTE[c as usize & 15];
        if self.rgb { (v & 0xff00) | (v >> 16) | ((v & 0xff) << 16) } else { v }
    }

    fn clear(&mut self) {
        let (w, h) = (self.width, self.height);
        self.buf = vec![0; w];
        for y in 0..h {
            self.blit(0, y, w, 1);
        }
        self.buf.clear();
    }

    /// Draws `cells` from cell (`x`, `y`) rightwards.
    pub fn put(&mut self, x: usize, y: usize, cells: &[Cell]) {
        let (cw, ch) = (font::WIDTH * self.scale, font::HEIGHT * self.scale);
        let w = cells.len() * cw;
        self.buf.clear();
        self.buf.resize(w * ch, 0);
        for (i, c) in cells.iter().enumerate() {
            let g = font::glyph_or_mark(c.ch);
            let (fg, bg) = (self.pixel(c.fg as u8), self.pixel(c.bg as u8));
            for py in 0..ch {
                let bits = g[py / self.scale];
                let row = &mut self.buf[py * w + i * cw..][..cw];
                for (px, p) in row.iter_mut().enumerate() {
                    *p = if bits & (0x80 >> (px / self.scale)) != 0 { fg } else { bg };
                }
            }
        }
        self.blit(self.origin.0 + x * cw, self.origin.1 + y * ch, w, ch);
    }

    /// Copies `buf`, `w` × `h` pixels, to (`x`, `y`) on screen.
    fn blit(&mut self, x: usize, y: usize, w: usize, h: usize) {
        if self.fb.is_null() {
            let px = unsafe { core::slice::from_raw_parts(self.buf.as_ptr() as *const BltPixel, w * h) };
            let _ = self.gop.blt(BltOp::BufferToVideo { buffer: px, src: BltRegion::Full, dest: (x, y), dims: (w, h) });
            return;
        }
        for r in 0..h {
            unsafe {
                let dst = self.fb.add((y + r) * self.stride + x);
                core::ptr::copy_nonoverlapping(self.buf.as_ptr().add(r * w), dst, w);
            }
        }
    }
}
