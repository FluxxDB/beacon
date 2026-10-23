//! The composer's cursor: where it is in the input, how the keys move it,
//! and editing at it.
//!
//! The cursor is a byte offset into [`App::input`] that always falls between
//! grapheme clusters, so a wide character, an emoji sequence or a letter with
//! combining marks moves and deletes as one. `←` `→` move a grapheme;
//! with `Ctrl` or `Alt` (or `Alt-b` `Alt-f`) a word; `Home` `End` and
//! `Ctrl-A` `Ctrl-E` go to the ends of the line (between `\n`s); `↑` `↓` go
//! to the row above or below as the composer shows it, wrapped or not,
//! keeping the column, and from the first row `↑` goes to the start of the
//! text, from the last `↓` to its end. Which key does what is in
//! [`crate::tui::keys`].

use unicode_segmentation::{GraphemeCursor, UnicodeSegmentation};

use crate::tui::state::App;
use crate::tui::widgets::composer;

/// Where a key moves the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    WordLeft,
    WordRight,
    LineStart,
    LineEnd,
    Up,
    Down,
}

impl App {
    /// The cursor's byte offset in `input`, between graphemes (a place
    /// inside one counts as just after it).
    pub fn cursor(&self) -> usize {
        let len = self.input.len();
        boundary_at_or_after(&self.input, len - self.cursor_from_end.min(len))
    }

    /// Puts the cursor at byte `at` (just after the grapheme it falls in, if
    /// it falls in one) and forgets the column `↑` `↓` aimed for.
    pub fn set_cursor(&mut self, at: usize) {
        let len = self.input.len();
        let at = boundary_at_or_after(&self.input, at.min(len));
        self.cursor_from_end = len - at;
        self.cursor_goal = None;
    }

    pub(crate) fn move_cursor(&mut self, motion: Motion) {
        let at = self.cursor();
        let input = &self.input;
        let to = match motion {
            Motion::Left => prev_boundary(input, at),
            Motion::Right => next_boundary(input, at),
            Motion::WordLeft => word_start(input, at),
            Motion::WordRight => word_end(input, at),
            Motion::LineStart => input[..at].rfind('\n').map_or(0, |i| i + 1),
            Motion::LineEnd => input[at..].find('\n').map_or(input.len(), |i| at + i),
            Motion::Up => return self.move_row(false),
            Motion::Down => return self.move_row(true),
        };
        self.set_cursor(to);
    }

    /// `↑` (up) and `↓`: the same column on the row above or below, or as
    /// near it as that row goes; a run of them keeps aiming for the column
    /// they started from. With no row there, to the end of the text (`↓`)
    /// or its start (`↑`).
    fn move_row(&mut self, down: bool) {
        let width = self.composer_text_width();
        let text = composer::shown(&self.input);
        let rows = composer::rows(&text, width);
        let (row, col) = composer::caret(&text, &rows, width, self.cursor());
        let target = if down {
            row.checked_add(1)
        } else {
            row.checked_sub(1)
        };
        let Some(target) = target.and_then(|r| rows.get(r)) else {
            // Past the last row, the end of the text; past the first, its
            // start, as in a macOS text box (Dang, R79KEKG2). The jump
            // forgets the column.
            self.set_cursor(if down { self.input.len() } else { 0 });
            return;
        };
        let goal = self.cursor_goal.unwrap_or(col);
        let to = composer::at_column(&text, target, goal);
        self.set_cursor(to);
        self.cursor_goal = Some(goal);
    }

    /// Puts `text` in at the cursor, which ends up after it.
    pub(crate) fn insert_at_cursor(&mut self, text: &str) {
        let at = self.cursor();
        self.input.insert_str(at, text);
        self.set_cursor(at + text.len());
    }

    /// `Backspace`: the grapheme before the cursor goes.
    pub(crate) fn delete_before_cursor(&mut self) {
        let at = self.cursor();
        let from = prev_boundary(&self.input, at);
        self.input.replace_range(from..at, "");
        self.set_cursor(from);
    }

    /// `Delete`: the grapheme after the cursor goes.
    pub(crate) fn delete_after_cursor(&mut self) {
        let at = self.cursor();
        let to = next_boundary(&self.input, at);
        self.input.replace_range(at..to, "");
        self.set_cursor(at);
    }

    /// `Esc` `Esc`: an empty composer.
    pub(crate) fn clear_input(&mut self) {
        self.input.clear();
        self.set_cursor(0);
        self.composer_top = 0;
    }

    /// After every message: the composer scrolls only as far as it must to
    /// show the cursor's row (the render keeps it in view regardless).
    pub(crate) fn keep_cursor_in_view(&mut self) {
        let Some(layout) = self.layout() else {
            return;
        };
        let width = composer::text_width(layout.composer.width);
        let text = composer::shown(&self.input);
        let rows = composer::rows(&text, width);
        let (row, _) = composer::caret(&text, &rows, width, self.cursor());
        let shown = usize::from(layout.composer.height.saturating_sub(2));
        self.composer_top = composer::first_row(self.composer_top, row, rows.len(), shown);
    }

    /// Cells for text in the composer as laid out now. Before the terminal's
    /// size is known, every line is one row.
    fn composer_text_width(&self) -> u16 {
        self.layout()
            .map_or(u16::MAX, |l| composer::text_width(l.composer.width))
    }
}

/// The first grapheme boundary at or after byte `at` (`at` need not be a
/// character boundary).
fn boundary_at_or_after(s: &str, at: usize) -> usize {
    s.grapheme_indices(true)
        .map(|(i, _)| i)
        .find(|&i| i >= at)
        .unwrap_or(s.len())
}

/// The boundary before the grapheme that ends at `at` (a boundary).
fn prev_boundary(s: &str, at: usize) -> usize {
    let mut cursor = GraphemeCursor::new(at, s.len(), true);
    cursor.prev_boundary(s, 0).ok().flatten().unwrap_or(0)
}

/// The boundary after the grapheme that starts at `at` (a boundary).
fn next_boundary(s: &str, at: usize) -> usize {
    let mut cursor = GraphemeCursor::new(at, s.len(), true);
    cursor.next_boundary(s, 0).ok().flatten().unwrap_or(s.len())
}

/// Words are runs of letters, digits and `_`; anything else is between them.
fn is_word(g: &str) -> bool {
    g.chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
}

/// Where the word before `at` starts (the one `at` is in, if it is in one).
fn word_start(s: &str, at: usize) -> usize {
    let mut start = at;
    let mut in_word = false;
    for (i, g) in s[..at].grapheme_indices(true).rev() {
        if is_word(g) {
            in_word = true;
        } else if in_word {
            break;
        }
        start = i;
    }
    start
}

/// Where the word after `at` ends (the one `at` is in, if it is in one).
fn word_end(s: &str, at: usize) -> usize {
    let mut in_word = false;
    for (i, g) in s[at..].grapheme_indices(true) {
        if is_word(g) {
            in_word = true;
        } else if in_word {
            return at + i;
        }
    }
    s.len()
}
