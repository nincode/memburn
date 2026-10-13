//! Doom-style fire drawn into blank cells, dim so the data around it stays readable.

use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use super::Color::{self, *};
use super::{BLANK, Frame, Rect};

/// Smallest area worth a fire, cells.
pub const MIN_W: usize = 8;
pub const MIN_H: usize = 4;

const RAMP: [(char, Color); 7] =
    [(' ', Black), ('.', DarkGray), (':', DarkGray), ('-', Red), ('=', Red), ('+', Brown), ('*', Brown)];
/// Heat of the hottest cell.
const HOT: u8 = 20;

/// The fire's heat field, advanced one step per drawn frame.
#[derive(Default)]
pub struct Fire(RefCell<State>);

#[derive(Default)]
struct State {
    w: usize,
    h: usize,
    heat: Vec<u8>,
    seed: u64,
}

impl State {
    fn next(&mut self) -> u32 {
        let s = &mut self.seed;
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        (*s >> 32) as u32
    }

    fn step(&mut self) {
        let (w, h) = (self.w, self.h);
        for x in 0..w {
            self.heat[(h - 1) * w + x] = HOT - 8 + (self.next() % 9) as u8;
        }
        for y in 1..h {
            for x in 0..w {
                let src = self.heat[y * w + x];
                let rnd = self.next() % 4;
                let dx = (x + 1).saturating_sub(rnd as usize).min(w - 1);
                let cool = (rnd & 1) as u8 + (self.next() % 3) as u8;
                self.heat[(y - 1) * w + dx] = src.saturating_sub(cool);
            }
        }
    }
}

impl Fire {
    /// Advances the fire one step and draws it rising from the bottom of `r` into the cells still blank.
    pub(super) fn draw(&self, f: &mut Frame, r: Rect) {
        if r.w < MIN_W || r.h < MIN_H {
            return;
        }
        let mut s = self.0.borrow_mut();
        let (w, h) = (r.w, r.h + 1);
        if s.w != w || s.h != h {
            *s = State { w, h, heat: vec![0; w * h], seed: 0x9e37_79b9_7f4a_7c15 };
            for _ in 0..h * 2 {
                s.step();
            }
        }
        s.step();
        for y in 0..r.h {
            for x in 0..w {
                let v = s.heat[y * w + x] as usize;
                let (ch, c) = RAMP[(v * RAMP.len() / (HOT as usize + 1)).min(RAMP.len() - 1)];
                if ch != ' ' && f.get(r.x + x, r.y + y) == BLANK {
                    f.set(r.x + x, r.y + y, ch, c, Black);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fire_burns_upward_and_only_in_blank_cells() {
        let fire = Fire::default();
        let mut f = Frame::new(20, 10);
        f.set(5, 9, 'x', White, Black);
        for _ in 0..3 {
            fire.draw(&mut f, Rect { x: 0, y: 0, w: 20, h: 10 });
        }
        assert_eq!(f.get(5, 9).ch, 'x');
        let lit = |y: usize| (0..20).filter(|&x| f.get(x, y).ch != ' ').count();
        assert!(lit(9) > 10);
        assert!(lit(0) < lit(9));
        let mut small = Frame::new(20, 10);
        fire.draw(&mut small, Rect { x: 0, y: 0, w: MIN_W - 1, h: 10 });
        assert!(small.cells.iter().all(|c| *c == BLANK));
    }
}
