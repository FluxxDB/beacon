//! The parts of the screen, one module each, all drawing through [`Canvas`]
//! so every cell gets an explicit foreground, background and weight.

pub mod composer;
pub mod dialog;
pub mod header;
pub mod helpbar;
pub mod posts;
pub mod sidebar;
pub mod title;

use ratatui::buffer::{Buffer, CellWidth};
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use unicode_segmentation::UnicodeSegmentation;

use crate::tui::theme::{role, Ink, Theme};
use crate::tui::wrap::{display_width, grapheme_width};

/// A run of text in one ink, for lines built from several styles.
pub type Seg = (String, Ink);

/// The frame buffer plus the theme that turns palette colors into terminal
/// colors. Writes outside the buffer are ignored.
pub struct Canvas<'a> {
    buf: &'a mut Buffer,
    theme: &'a Theme,
}

impl<'a> Canvas<'a> {
    pub fn new(buf: &'a mut Buffer, theme: &'a Theme) -> Self {
        Canvas { buf, theme }
    }

    pub fn theme(&self) -> &Theme {
        self.theme
    }

    pub fn area(&self) -> Rect {
        self.buf.area
    }

    /// Paints `area` blank in `ink`.
    pub fn fill(&mut self, area: Rect, ink: Ink) {
        let area = self.buf.area.intersection(area);
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                self.put(x, y, " ", ink);
            }
        }
    }

    /// Repeats a one-cell `symbol` over `width` cells from (x, y).
    pub fn hline(&mut self, x: u16, y: u16, width: u16, symbol: &str, ink: Ink) {
        for i in 0..width {
            self.put(x.saturating_add(i), y, symbol, ink);
        }
    }

    /// Writes `text` from (x, y), stopping before column `limit` (a grapheme
    /// that would cross it is left out). Returns the column after the text.
    pub fn text(&mut self, x: u16, y: u16, text: &str, ink: Ink, limit: u16) -> u16 {
        let limit = limit.min(self.buf.area.right());
        let mut x = x;
        for g in text.graphemes(true) {
            let w = grapheme_width(g);
            if w == 0 {
                continue;
            }
            if x.saturating_add(w) > limit {
                break;
            }
            self.put(x, y, g, ink);
            // Cells under the rest of a wide glyph keep its colors.
            for i in 1..w {
                self.put_raw(x + i, y, " ", ink);
            }
            x += w;
        }
        x
    }

    /// Writes runs of text one after another; returns the column after them.
    pub fn segs(&mut self, x: u16, y: u16, segs: &[Seg], limit: u16) -> u16 {
        segs.iter()
            .fold(x, |x, (text, ink)| self.text(x, y, text, *ink, limit))
    }

    /// Writes runs of text so that they end just before column `right`.
    /// Returns the column they start at.
    pub fn segs_right(&mut self, right: u16, y: u16, segs: &[Seg]) -> u16 {
        let width: u16 = segs.iter().map(|(t, _)| display_width(t)).sum();
        let x = right.saturating_sub(width);
        self.segs(x, y, segs, right);
        x
    }

    /// Fades everything drawn so far behind a dialog: text and marks turn
    /// smoke and lose weight; backgrounds and the frame stay.
    pub fn dim(&mut self) {
        let frame = self.theme.c(role::FRAME);
        let dim = self.theme.c(role::DIM);
        for cell in self.buf.content.iter_mut() {
            if cell.fg != frame {
                cell.fg = dim;
            }
            cell.modifier.remove(Modifier::BOLD);
        }
    }

    /// Sets one cell, first blanking any wide glyph to its left that would
    /// otherwise spill over it.
    fn put(&mut self, x: u16, y: u16, symbol: &str, ink: Ink) {
        if !self.buf.area.contains(Position { x, y }) {
            return;
        }
        for back in 1..=3u16 {
            let Some(left) = x.checked_sub(back) else {
                break;
            };
            if left < self.buf.area.left() {
                break;
            }
            let cell = &mut self.buf[(left, y)];
            if cell.symbol().cell_width() > back {
                cell.set_symbol(" ");
            }
        }
        self.put_raw(x, y, symbol, ink);
    }

    fn put_raw(&mut self, x: u16, y: u16, symbol: &str, ink: Ink) {
        if !self.buf.area.contains(Position { x, y }) {
            return;
        }
        let fg = self.theme.c(ink.fg);
        let bg = self.theme.c(ink.bg);
        let cell = &mut self.buf[(x, y)];
        cell.set_symbol(symbol);
        cell.fg = fg;
        cell.bg = bg;
        cell.modifier = if ink.bold {
            Modifier::BOLD
        } else {
            Modifier::empty()
        };
    }
}

/// Total display width of some runs.
pub fn segs_width(segs: &[Seg]) -> u16 {
    segs.iter()
        .map(|(t, _)| display_width(t))
        .fold(0u16, u16::saturating_add)
}
