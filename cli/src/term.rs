//! Full-screen terminal: alternate screen, raw keys, and cell-diffed drawing of a [`Frame`].

use std::io::{self, Write};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::{cursor, execute, terminal};
use memburn_core::tui::{Cell, Color, Frame};

pub enum Key {
    Char(char),
    Enter,
    Esc,
    CtrlC,
}

pub struct Term {
    prev: Option<Frame>,
}

impl Term {
    pub fn open() -> io::Result<Term> {
        terminal::enable_raw_mode()?;
        execute!(
            io::stdout(),
            terminal::EnterAlternateScreen,
            cursor::Hide,
            terminal::Clear(terminal::ClearType::All)
        )?;
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            hook(info);
        }));
        Ok(Term { prev: None })
    }

    pub fn size() -> (usize, usize) {
        terminal::size().map_or((80, 25), |(w, h)| (w as usize, h as usize))
    }

    /// Writes only the cells that changed since the last frame.
    pub fn draw(&mut self, f: Frame) {
        let full = self.prev.as_ref().is_none_or(|p| p.w != f.w || p.h != f.h);
        let mut out = String::with_capacity(f.w * f.h);
        if full {
            out.push_str("\x1b[0m\x1b[2J");
        }
        let mut pen: Option<(Color, Color)> = None;
        for y in 0..f.h {
            let mut at: Option<usize> = None;
            for x in 0..f.w {
                let c = f.get(x, y);
                if !full && self.prev.as_ref().is_some_and(|p| p.get(x, y) == c) {
                    continue;
                }
                if at != Some(x) {
                    out.push_str(&format!("\x1b[{};{}H", y + 1, x + 1));
                }
                if pen != Some((c.fg, c.bg)) {
                    out.push_str(&sgr(c));
                    pen = Some((c.fg, c.bg));
                }
                out.push(c.ch);
                at = Some(x + 1);
            }
        }
        // The last cell scrolls some terminals; it is always blank in the dashboard.
        let mut stdout = io::stdout().lock();
        let _ = stdout.write_all(out.as_bytes());
        let _ = stdout.flush();
        self.prev = Some(f);
    }

    /// Next key press, waiting at most `wait`.
    pub fn key(&self, wait: Duration) -> Option<Key> {
        if !event::poll(wait).ok()? {
            return None;
        }
        match event::read().ok()? {
            Event::Key(k) if k.kind != KeyEventKind::Release => match k.code {
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => Some(Key::CtrlC),
                KeyCode::Char(c) => Some(Key::Char(c.to_ascii_uppercase())),
                KeyCode::Enter => Some(Key::Enter),
                KeyCode::Esc => Some(Key::Esc),
                _ => None,
            },
            _ => None,
        }
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        restore();
    }
}

fn restore() {
    let _ = execute!(io::stdout(), crossterm::style::ResetColor, cursor::Show, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
}

/// VGA order to ANSI: 30–37 / 90–97 foreground, 40–47 background.
fn sgr(c: Cell) -> String {
    const ANSI: [u8; 8] = [0, 4, 2, 6, 1, 5, 3, 7];
    let fg = c.fg as usize;
    let fg = if fg < 8 { 30 + ANSI[fg] } else { 90 + ANSI[fg - 8] };
    let bg = 40 + ANSI[c.bg as usize & 7];
    format!("\x1b[0;{fg};{bg}m")
}
