//! The pixel dashboard on every 32-bit Linux framebuffer (efifb, simpledrm or a DRM driver's fbdev), the console
//! switched to graphics mode while it draws.

use std::fs::OpenOptions;
use std::os::fd::{AsRawFd, OwnedFd};

use memburn_core::tui::View;
use memburn_gfx::{Canvas, MIN_SIZE, Painter, damage};

const FBIOGET_VSCREENINFO: libc::Ioctl = 0x4600;
const FBIOGET_FSCREENINFO: libc::Ioctl = 0x4602;
const KDSETMODE: libc::Ioctl = 0x4B3A;
const KD_TEXT: libc::c_ulong = 0;
const KD_GRAPHICS: libc::c_ulong = 1;
/// Framebuffer devices tried, /dev/fb0 up.
const MAX_FB: usize = 8;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Bitfield {
    offset: u32,
    length: u32,
    msb_right: u32,
}

/// `struct fb_var_screeninfo`.
#[repr(C)]
#[derive(Default)]
struct VarInfo {
    xres: u32,
    yres: u32,
    xres_virtual: u32,
    yres_virtual: u32,
    xoffset: u32,
    yoffset: u32,
    bits_per_pixel: u32,
    grayscale: u32,
    red: Bitfield,
    green: Bitfield,
    blue: Bitfield,
    transp: Bitfield,
    rest: [u32; 20],
}

/// `struct fb_fix_screeninfo`.
#[repr(C)]
#[derive(Default)]
struct FixInfo {
    id: [u8; 16],
    smem_start: libc::c_ulong,
    smem_len: u32,
    kind: u32,
    type_aux: u32,
    visual: u32,
    xpanstep: u16,
    ypanstep: u16,
    ywrapstep: u16,
    line_length: u32,
    mmio_start: libc::c_ulong,
    mmio_len: u32,
    accel: u32,
    capabilities: u16,
    reserved: [u16; 2],
}

/// One mapped framebuffer.
struct Out {
    _fd: OwnedFd,
    map: *mut u8,
    len: usize,
    /// Byte offset of the visible top-left pixel.
    origin: usize,
    stride: usize,
    /// Channel shifts (red, green, blue); (16, 8, 0) is the canvas's own layout.
    shifts: (u32, u32, u32),
    /// Index into [`Pixels::sizes`].
    size: usize,
}

/// A canvas per distinct screen size and what each framebuffer of that size shows.
struct Size {
    w: usize,
    h: usize,
    cur: Canvas,
    shown: Option<Canvas>,
}

pub struct Pixels {
    outs: Vec<Out>,
    sizes: Vec<Size>,
    painter: Painter,
    /// Console tty switched to graphics mode.
    tty: Option<i32>,
}

impl Pixels {
    /// Every usable framebuffer, or None when there is none and the text dashboard stays.
    pub fn open() -> Option<Pixels> {
        let mut outs = Vec::new();
        let mut sizes: Vec<Size> = Vec::new();
        for n in 0..MAX_FB {
            let Some((out, w, h)) = map(&format!("/dev/fb{n}")) else { continue };
            let size = match sizes.iter().position(|s| (s.w, s.h) == (w, h)) {
                Some(i) => i,
                None => {
                    sizes.push(Size { w, h, cur: Canvas::new(w, h), shown: None });
                    sizes.len() - 1
                }
            };
            outs.push(Out { size, ..out });
        }
        if outs.is_empty() {
            return None;
        }
        let tty = (unsafe { libc::ioctl(0, KDSETMODE, KD_GRAPHICS) } == 0).then_some(0);
        if tty.is_some() {
            let hook = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                unsafe { libc::ioctl(0, KDSETMODE, KD_TEXT) };
                hook(info);
            }));
        }
        Some(Pixels { outs, sizes, painter: Painter::new(), tty })
    }

    /// "1920x1080" per framebuffer, for the log.
    pub fn describe(&self) -> String {
        let s: Vec<String> =
            self.outs.iter().map(|o| format!("{}x{}", self.sizes[o.size].w, self.sizes[o.size].h)).collect();
        s.join(", ")
    }

    /// Paints `v` once per screen size and copies the rows that changed.
    pub fn draw(&mut self, v: &View) {
        for (i, s) in self.sizes.iter_mut().enumerate() {
            self.painter.paint(v, &mut s.cur);
            let spans = damage(s.shown.as_ref(), &s.cur);
            for o in self.outs.iter().filter(|o| o.size == i) {
                for sp in &spans {
                    let src = &s.cur.row(sp.y)[sp.x0..sp.x1];
                    let at = o.origin + sp.y * o.stride + sp.x0 * 4;
                    if at + src.len() * 4 > o.len {
                        continue;
                    }
                    let dst = unsafe { std::slice::from_raw_parts_mut(o.map.add(at) as *mut u32, src.len()) };
                    if o.shifts == (16, 8, 0) {
                        dst.copy_from_slice(src);
                    } else {
                        let (r, g, b) = o.shifts;
                        for (d, &p) in dst.iter_mut().zip(src) {
                            *d = (p >> 16 & 0xff) << r | (p >> 8 & 0xff) << g | (p & 0xff) << b;
                        }
                    }
                }
            }
            match &mut s.shown {
                Some(shown) => std::mem::swap(shown, &mut s.cur),
                None => s.shown = Some(std::mem::replace(&mut s.cur, Canvas::new(s.w, s.h))),
            }
        }
    }
}

impl Drop for Pixels {
    fn drop(&mut self) {
        for o in &self.outs {
            unsafe { libc::munmap(o.map as *mut libc::c_void, o.len) };
        }
        if let Some(fd) = self.tty {
            unsafe { libc::ioctl(fd, KDSETMODE, KD_TEXT) };
        }
    }
}

/// Maps `path` when it has 8-bit channels in 32-bit pixels and is at least [`MIN_SIZE`].
fn map(path: &str) -> Option<(Out, usize, usize)> {
    let fd: OwnedFd = OpenOptions::new().read(true).write(true).open(path).ok()?.into();
    let mut var = VarInfo::default();
    let mut fix = FixInfo::default();
    unsafe {
        if libc::ioctl(fd.as_raw_fd(), FBIOGET_VSCREENINFO, &mut var) != 0
            || libc::ioctl(fd.as_raw_fd(), FBIOGET_FSCREENINFO, &mut fix) != 0
        {
            return None;
        }
    }
    let (w, h) = (var.xres as usize, var.yres as usize);
    let eight = [var.red, var.green, var.blue].iter().all(|b| b.length == 8 && b.offset % 8 == 0 && b.offset < 32);
    if var.bits_per_pixel != 32 || !eight || w < MIN_SIZE.0 || h < MIN_SIZE.1 {
        return None;
    }
    let (stride, len) = (fix.line_length as usize, fix.smem_len as usize);
    let origin = var.yoffset as usize * stride + var.xoffset as usize * 4;
    if stride < w * 4 || origin + stride * h > len {
        return None;
    }
    let prot = libc::PROT_READ | libc::PROT_WRITE;
    let map = unsafe { libc::mmap(std::ptr::null_mut(), len, prot, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
    if map == libc::MAP_FAILED {
        return None;
    }
    let shifts = (var.red.offset, var.green.offset, var.blue.offset);
    Some((Out { _fd: fd, map: map as *mut u8, len, origin, stride, shifts, size: 0 }, w, h))
}
