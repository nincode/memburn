use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::arch::asm;
use core::fmt::Write;

use memburn_core::tui::{self, Frame, View};
use uefi::CStr16;
use uefi::proto::console::text::{Color, Key, ScanCode};
use uefi::system;

use crate::gop::{self, Display};

/// UEFI colors in VGA order, matching `tui::Color`.
const COLORS: [Color; 16] = [
    Color::Black,
    Color::Blue,
    Color::Green,
    Color::Cyan,
    Color::Red,
    Color::Magenta,
    Color::Brown,
    Color::LightGray,
    Color::DarkGray,
    Color::LightBlue,
    Color::LightGreen,
    Color::LightCyan,
    Color::LightRed,
    Color::LightMagenta,
    Color::Yellow,
    Color::White,
];

/// ASCII stand-ins for dashboard glyphs the console font may lack.
const FALLBACK: [(char, char); 14] = [
    ('─', '-'),
    ('│', '|'),
    ('┌', '+'),
    ('┐', '+'),
    ('└', '+'),
    ('┘', '+'),
    ('█', '#'),
    ('░', '.'),
    ('►', '>'),
    ('▄', '_'),
    ('▀', '"'),
    ('·', '.'),
    ('°', ' '),
    ('×', 'x'),
];

/// Log lines kept from the start of the session and from its end.
const LOG_HEAD: usize = 1000;
const LOG_TAIL: usize = 3000;

/// Full-screen dashboard on every GOP display, else on the firmware text console; only changed cells are written.
pub struct Screen {
    cols: usize,
    rows: usize,
    shown: Option<Frame>,
    swap: Vec<(char, char)>,
    /// The first [`LOG_HEAD`] log lines, for memburn.log.
    head: Vec<String>,
    /// The latest [`LOG_TAIL`] lines after `head`.
    tail: VecDeque<String>,
    /// Lines dropped between `head` and `tail`.
    dropped: u64,
    debugcon: bool,
    /// Empty: the text console draws.
    displays: Vec<Display>,
}

impl Screen {
    pub fn new(debugcon: bool) -> Screen {
        let mut displays = gop::find();
        if !displays.is_empty() {
            let (cols, rows) = gop::grid(&mut displays);
            let lines: Vec<String> = displays
                .iter()
                .enumerate()
                .map(|(i, d)| {
                    let scale = if d.scale > 1 { format!(", {}x", d.scale) } else { String::new() };
                    format!("Display {i}: {}x{} {}{scale}", d.width, d.height, d.pci)
                })
                .collect();
            let mut s = Screen::with(cols, rows, Vec::new(), debugcon, displays);
            for l in lines {
                s.log(l);
            }
            return s;
        }
        let mut s = system::with_stdout(|out| {
            let best = out.modes().filter(|m| m.columns() <= 160).max_by_key(|m| m.rows() * m.columns());
            if let Some(m) = best {
                let _ = out.set_mode(m);
            }
            let _ = out.set_color(Color::LightGray, Color::Black);
            let _ = out.clear();
            let _ = out.enable_cursor(false);
            let (cols, rows) = out.current_mode().ok().flatten().map_or((80, 25), |m| (m.columns(), m.rows()));
            let swap = FALLBACK
                .iter()
                .filter(|&&(ch, _)| {
                    let mut buf = [0u16; 2];
                    let mut tmp = [0u8; 4];
                    !CStr16::from_str_with_buf(ch.encode_utf8(&mut tmp), &mut buf)
                        .is_ok_and(|s| out.test_string(s).unwrap_or(false))
                })
                .copied()
                .collect();
            Screen::with(cols, rows, swap, debugcon, Vec::new())
        });
        s.log(format!("Display: text console {}x{}", s.cols, s.rows));
        s
    }

    fn with(cols: usize, rows: usize, swap: Vec<(char, char)>, debugcon: bool, displays: Vec<Display>) -> Screen {
        Screen {
            cols,
            rows,
            shown: None,
            swap,
            head: Vec::new(),
            tail: VecDeque::new(),
            dropped: 0,
            debugcon,
            displays,
        }
    }

    /// Adds a line to memburn.log and, with `debugcon`, to I/O port 0xE9.
    pub fn log(&mut self, line: String) {
        if self.debugcon {
            for b in line.bytes().chain(*b"\n") {
                unsafe { asm!("out dx, al", in("dx") 0xE9u16, in("al") b, options(nomem, nostack)) };
            }
        }
        if self.head.len() < LOG_HEAD {
            self.head.push(line);
            return;
        }
        if self.tail.len() == LOG_TAIL {
            self.tail.pop_front();
            self.dropped += 1;
        }
        self.tail.push_back(line);
    }

    /// The kept log lines, CRLF-terminated, with a marker where lines were dropped.
    pub fn log_text(&self) -> String {
        let mut text = String::new();
        let marker = (self.dropped > 0).then(|| format!("... {} more lines", self.dropped));
        for l in self.head.iter().chain(marker.iter()).chain(self.tail.iter()) {
            text.push_str(l);
            text.push_str("\r\n");
        }
        text
    }

    pub fn draw(&mut self, v: &View) {
        if !self.displays.is_empty() {
            return self.draw_gop(v);
        }
        // The last column stays empty: writing the bottom-right cell scrolls some consoles.
        let mut f = Frame::new(self.cols.saturating_sub(1), self.rows);
        tui::render(v, &mut f);
        let old = self.shown.take();
        let same = |x: usize, y: usize| old.as_ref().is_some_and(|o| o.get(x, y) == f.get(x, y));
        let swap = &self.swap;
        system::with_stdout(|out| {
            for y in 0..f.h {
                let mut x = 0;
                while x < f.w {
                    if same(x, y) {
                        x += 1;
                        continue;
                    }
                    let c = f.get(x, y);
                    let start = x;
                    let mut run = String::new();
                    while x < f.w && !same(x, y) && (f.get(x, y).fg, f.get(x, y).bg) == (c.fg, c.bg) {
                        let ch = f.get(x, y).ch;
                        run.push(swap.iter().find(|s| s.0 == ch).map_or(ch, |s| s.1));
                        x += 1;
                    }
                    let _ = out.set_color(COLORS[c.fg as usize], COLORS[c.bg as usize & 7]);
                    let _ = out.set_cursor_position(start, y);
                    let _ = out.write_str(&run);
                }
            }
        });
        self.shown = Some(f);
    }

    fn draw_gop(&mut self, v: &View) {
        let mut f = Frame::new(self.cols, self.rows);
        tui::render(v, &mut f);
        let old = self.shown.take();
        let same = |x: usize, y: usize| old.as_ref().is_some_and(|o| o.get(x, y) == f.get(x, y));
        for y in 0..f.h {
            let mut x = 0;
            while x < f.w {
                if same(x, y) {
                    x += 1;
                    continue;
                }
                let start = x;
                while x < f.w && !same(x, y) {
                    x += 1;
                }
                let run = &f.cells[y * f.w + start..y * f.w + x];
                for d in &mut self.displays {
                    d.put(start, y, run);
                }
            }
        }
        self.shown = Some(f);
    }
}

pub enum Input {
    None,
    Escape,
    /// Printable key, upper-cased; Enter is '\r'.
    Char(char),
}

pub fn read_key() -> Input {
    match system::with_stdin(|i| i.read_key()) {
        Ok(Some(Key::Special(ScanCode::ESCAPE))) => Input::Escape,
        Ok(Some(Key::Printable(c))) => Input::Char(char::from(c).to_ascii_uppercase()),
        _ => Input::None,
    }
}
