//! The pixel dashboard in a Win32 window with its own thread and message pump, and the start mode that opens it.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc;

use memburn_core::tui::View;
use memburn_gfx::{Canvas, MIN_SIZE, Painter, damage, rects};
use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BeginPaint, CreateSolidBrush, DIB_RGB_COLORS, DeleteObject, EndPaint,
    FillRect, GetMonitorInfoW, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, PAINTSTRUCT,
    SetDIBitsToDevice,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::term::Key;
use crate::upload;

/// Client size in points of the dashboard's design, scaled by the monitor's DPI.
const DESIGN: (i32, i32) = (1280, 800);
const STYLE: u32 = WS_OVERLAPPEDWINDOW;
const WM_QUIT_WINDOW: u32 = WM_APP + 1;
/// `ENDSESSION_LOGOFF` in WM_ENDSESSION's lParam.
const LOGOFF: isize = 0x8000_0000;

/// The frame on screen, which WM_PAINT copies from.
static FRAME: Mutex<Option<Canvas>> = Mutex::new(None);
/// Client size in physical pixels.
static SIZE: Mutex<(usize, usize)> = Mutex::new((0, 0));
static KEYS: Mutex<VecDeque<Key>> = Mutex::new(VecDeque::new());
/// Exit code of a closed window: 3 while a job runs, else the last job's.
pub static CLOSE_CODE: AtomicU8 = AtomicU8::new(0);

pub struct Pixels {
    hwnd: isize,
    painter: Painter,
    cur: Canvas,
}

impl Pixels {
    /// A new window, or None when it cannot be created.
    pub fn open() -> Option<Pixels> {
        let hwnd = spawn(true)?;
        Some(Pixels { hwnd, painter: Painter::new(), cur: Canvas::new(0, 0) })
    }

    /// "1280x800 window", for the log.
    pub fn describe(&self) -> String {
        let (w, h) = *SIZE.lock().unwrap();
        format!("{w}x{h} window")
    }

    /// Paints `v` at the client size and invalidates the rectangles that changed.
    pub fn draw(&mut self, v: &View) {
        let (w, h) = *SIZE.lock().unwrap();
        if w < MIN_SIZE.0 || h < MIN_SIZE.1 {
            return;
        }
        if (self.cur.w, self.cur.h) != (w, h) {
            self.cur = Canvas::new(w, h);
        }
        self.painter.paint(v, &mut self.cur);
        let spans = {
            let mut f = FRAME.lock().unwrap();
            let spans = damage(f.as_ref(), &self.cur);
            let old = f.replace(std::mem::replace(&mut self.cur, Canvas::new(0, 0)));
            self.cur = old.filter(|o| (o.w, o.h) == (w, h)).unwrap_or_else(|| Canvas::new(w, h));
            spans
        };
        for (x, y, rw, rh) in rects(&spans) {
            let r = RECT { left: x as i32, top: y as i32, right: (x + rw) as i32, bottom: (y + rh) as i32 };
            unsafe { InvalidateRect(self.hwnd as HWND, &r, 0) };
        }
    }

    /// Next key typed into the window.
    pub fn key(&self) -> Option<Key> {
        KEYS.lock().unwrap().pop_front()
    }
}

impl Drop for Pixels {
    fn drop(&mut self) {
        unsafe { PostMessageW(self.hwnd as HWND, WM_QUIT_WINDOW, 0, 0) };
    }
}

/// A hidden window that sends the stopped snapshot at logoff and shutdown: console processes that load user32 get
/// no CTRL_LOGOFF_EVENT or CTRL_SHUTDOWN_EVENT.
pub fn watch_session() {
    spawn(false);
}

fn spawn(visible: bool) -> Option<isize> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new().name("window".into()).spawn(move || unsafe { run(visible, &tx) }).ok()?;
    rx.recv().ok().flatten()
}

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain([0]).collect()
}

unsafe fn run(visible: bool, tx: &mpsc::Sender<Option<isize>>) {
    unsafe {
        let inst = GetModuleHandleW(std::ptr::null());
        let class = wide("memburn".as_ref());
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(proc),
            hInstance: inst,
            hIcon: LoadIconW(inst, 1 as _),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassExW(&wc);
        let title = wide(format!("memburn {}", env!("CARGO_PKG_VERSION")).as_ref());
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            STYLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            inst,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            let _ = tx.send(None);
            return;
        }
        if visible {
            place(hwnd);
            ShowWindow(hwnd, SW_SHOWNORMAL);
            SetForegroundWindow(hwnd);
        }
        let mut r: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut r);
        *SIZE.lock().unwrap() = (r.right as usize, r.bottom as usize);
        let _ = tx.send(Some(hwnd as isize));
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Sizes the window to the design at the monitor's DPI, within its work area, centred.
unsafe fn place(hwnd: HWND) {
    unsafe {
        let dpi = GetDpiForWindow(hwnd).max(96) as i32;
        let mut r = RECT { left: 0, top: 0, right: DESIGN.0 * dpi / 96, bottom: DESIGN.1 * dpi / 96 };
        AdjustWindowRectExForDpi(&mut r, STYLE, 0, 0, dpi as u32);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..std::mem::zeroed() };
        if GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi) == 0 {
            return;
        }
        let a = mi.rcWork;
        let w = (r.right - r.left).min(a.right - a.left);
        let h = (r.bottom - r.top).min(a.bottom - a.top);
        let (x, y) = (a.left + (a.right - a.left - w) / 2, a.top + (a.bottom - a.top - h) / 2);
        SetWindowPos(hwnd, std::ptr::null_mut(), x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

/// The OS or the user is ending the process: the live view gets the stopped verdict with `why`.
fn end(hwnd: HWND, why: &str) -> ! {
    unsafe { ShowWindow(hwnd, SW_HIDE) };
    upload::abort(why, upload::ABORT_WAIT);
    std::process::exit(CLOSE_CODE.load(Ordering::Relaxed).into());
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                paint(hwnd);
                0
            }
            WM_ERASEBKGND => 1,
            WM_SIZE => {
                *SIZE.lock().unwrap() = ((lp & 0xffff) as usize, (lp >> 16 & 0xffff) as usize);
                0
            }
            WM_GETMINMAXINFO => {
                let dpi = GetDpiForWindow(hwnd).max(96);
                let mut r = RECT { left: 0, top: 0, right: MIN_SIZE.0 as i32, bottom: MIN_SIZE.1 as i32 };
                AdjustWindowRectExForDpi(&mut r, STYLE, 0, 0, dpi);
                let mm = &mut *(lp as *mut MINMAXINFO);
                mm.ptMinTrackSize.x = r.right - r.left;
                mm.ptMinTrackSize.y = r.bottom - r.top;
                0
            }
            WM_DPICHANGED => {
                let r = &*(lp as *const RECT);
                let flags = SWP_NOZORDER | SWP_NOACTIVATE;
                SetWindowPos(hwnd, std::ptr::null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, flags);
                0
            }
            WM_CHAR => {
                let key = match wp as u32 {
                    0x0d => Some(Key::Enter),
                    0x1b => Some(Key::Esc),
                    0x03 => Some(Key::CtrlC),
                    c => char::from_u32(c).filter(|c| !c.is_control()).map(|c| Key::Char(c.to_ascii_uppercase())),
                };
                if let Some(k) = key {
                    KEYS.lock().unwrap().push_back(k);
                }
                0
            }
            WM_CLOSE if IsWindowVisible(hwnd) != 0 => end(hwnd, "Stopped: the window was closed"),
            WM_CLOSE => end(hwnd, "Stopped: memburn was terminated"),
            WM_QUERYENDSESSION => 1,
            WM_ENDSESSION if wp != 0 => {
                end(hwnd, if lp & LOGOFF != 0 { "Stopped: the user logged off" } else { "Stopped: Windows shut down" })
            }
            WM_QUIT_WINDOW => {
                DestroyWindow(hwnd);
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

/// Copies the invalid rows of the frame; the client area outside it takes the frame's background.
unsafe fn paint(hwnd: HWND) {
    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let dc = BeginPaint(hwnd, &mut ps);
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut client);
        let f = FRAME.lock().unwrap();
        let (w, h, bg) = f.as_ref().map_or((0, 0, 0), |c| (c.w as i32, c.h as i32, c.px.first().copied().unwrap_or(0)));
        if let Some(c) = f.as_ref() {
            let (y0, y1) = (ps.rcPaint.top.clamp(0, h), ps.rcPaint.bottom.clamp(0, h));
            if y1 > y0 {
                let rows = y1 - y0;
                let bmi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: w,
                        biHeight: -rows,
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB,
                        ..std::mem::zeroed()
                    },
                    ..std::mem::zeroed()
                };
                let src = c.px[y0 as usize * c.w..].as_ptr().cast();
                SetDIBitsToDevice(dc, 0, y0, w as u32, rows as u32, 0, 0, 0, rows as u32, src, &bmi, DIB_RGB_COLORS);
            }
        }
        drop(f);
        let colorref = (bg >> 16 & 0xff) | (bg & 0xff00) | (bg & 0xff) << 16;
        let brush = CreateSolidBrush(colorref);
        for r in [
            RECT { left: w, top: 0, right: client.right, bottom: client.bottom },
            RECT { left: 0, top: h, right: w, bottom: client.bottom },
        ] {
            if r.right > r.left && r.bottom > r.top {
                FillRect(dc, &r, brush);
            }
        }
        DeleteObject(brush);
        EndPaint(hwnd, &ps);
    }
}

pub enum Start {
    Window,
    Text,
    /// The elevated copy ran; its exit code.
    Relaunched(u8),
}

/// Window when forced, or when started with no arguments and no console to share (a double-click); text from a
/// terminal or when forced. The window asks for elevation once.
pub fn start(gui: bool, no_gui: bool, no_args: bool, relaunched: bool) -> Start {
    use windows_sys::Win32::System::Console::{AllocConsole, FreeConsole, GetConsoleProcessList};
    let mut ids = [0u32; 4];
    let procs = unsafe { GetConsoleProcessList(ids.as_mut_ptr(), ids.len() as u32) };
    let window = !no_gui && (gui || (procs <= 1 && (no_args || procs == 0)));
    if !window {
        if procs == 0 {
            unsafe { AllocConsole() };
        }
        return Start::Text;
    }
    if procs == 1 {
        unsafe { FreeConsole() };
    }
    if !relaunched
        && !elevated()
        && let Some(code) = relaunch()
    {
        return Start::Relaunched(code);
    }
    Start::Window
}

/// An error box, for a window-mode start that has no console.
pub fn error(text: &str) {
    let (text, title) = (wide(text.as_ref()), wide("memburn".as_ref()));
    unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR) };
}

/// Shows a console again when the window cannot open.
pub fn console() {
    unsafe { windows_sys::Win32::System::Console::AllocConsole() };
}

fn elevated() -> bool {
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut e = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut n = 0;
        let size = size_of::<TOKEN_ELEVATION>() as u32;
        let ok = GetTokenInformation(token, TokenElevation, (&raw mut e).cast(), size, &mut n) != 0;
        CloseHandle(token);
        ok && e.TokenIsElevated != 0
    }
}

/// Starts this exe elevated (UAC prompt) with the same arguments and waits for it; None when declined or failed.
fn relaunch() -> Option<u8> {
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };
    let exe = wide(std::env::current_exe().ok()?.as_os_str());
    let dir = wide(std::env::current_dir().ok()?.as_os_str());
    let mut params: Vec<String> = std::env::args().skip(1).filter(|a| a != "--gui").map(|a| quote(&a)).collect();
    params.extend(["--gui".into(), "--relaunched".into()]);
    let params = wide(params.join(" ").as_ref());
    let verb = wide("runas".as_ref());
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: verb.as_ptr(),
        lpFile: exe.as_ptr(),
        lpParameters: params.as_ptr(),
        lpDirectory: dir.as_ptr(),
        nShow: SW_SHOWNORMAL,
        ..unsafe { std::mem::zeroed() }
    };
    unsafe {
        if ShellExecuteExW(&mut info) == 0 || info.hProcess.is_null() {
            return None;
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 0;
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        Some(code as u8)
    }
}

/// One argument for a command line that CommandLineToArgvW splits back unchanged.
fn quote(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '"']) {
        return a.to_string();
    }
    let mut out = String::from('"');
    let mut slashes = 0;
    for ch in a.chars() {
        match ch {
            '\\' => slashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
                slashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', slashes));
                slashes = 0;
            }
        }
        if ch != '\\' {
            out.push(ch);
        }
    }
    out.extend(std::iter::repeat_n('\\', slashes * 2));
    out.push('"');
    out
}
