//! The composer: a rounded box with the key hint in its top border, the accent
//! `›` prompt, the text being typed (wrapped, growing up to five lines, then
//! scrolling) and a block caret on the cell under the cursor.

use std::borrow::Cow;
use std::ops::Range;

use ratatui::layout::Rect;

use crate::tui::state::{App, Focus, Selection};
use crate::tui::theme::{palette, role, Ink};
use crate::tui::view::Layout;
use crate::tui::widgets::Canvas;
use crate::tui::wrap::{display_width, grapheme_width, wrap_ranges};
use unicode_segmentation::UnicodeSegmentation;

/// Most text lines shown; longer input scrolls to keep the caret in view.
pub const MAX_LINES: u16 = 5;
/// Columns from the box's left edge: the prompt, then the text.
const PROMPT: u16 = 2;
const TEXT: u16 = 4;
const HINT_WIDE: &str = " enter send · a attach a file ";
const HINT: &str = " enter send · a attach ";
/// The hint while an `@` ghost shows (one match; more add their position).
const GHOST_HINT: &str = " tab complete ";
/// The hint while one `Esc` has armed a clear.
const CLEAR_HINT: &str = " esc again to clear ";

/// Cells for text in a composer box `box_width` wide: border, space and
/// prompt and space on the left; a space and border on the right.
pub fn text_width(box_width: u16) -> u16 {
    box_width.saturating_sub(TEXT + 2)
}

/// Lines the composer needs for the current input, caret included.
pub fn line_count(app: &App, text_width: u16) -> u16 {
    u16::try_from(rows(&shown(&app.input), text_width).len()).unwrap_or(u16::MAX)
}

/// The input as the composer draws it: a tab is one blank cell, as in the
/// posts. It is byte for byte as long as the input (a tab and a space are
/// one byte each), so a cursor position in one is a position in the other.
/// [`rows`], [`caret`] and [`at_column`] take this text.
pub fn shown(input: &str) -> Cow<'_, str> {
    if input.contains('\t') {
        Cow::Owned(input.replace('\t', " "))
    } else {
        Cow::Borrowed(input)
    }
}

/// One row of the composer's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The first cursor position on this row. A line's first row also holds
    /// any indentation the wrap dropped before its text.
    pub from: usize,
    /// The bytes of the input it shows.
    pub text: Range<usize>,
}

/// The rows the input takes at `width` cells: its wrapped lines, plus an
/// empty row at the end when the last one is full, for the caret.
pub fn rows(input: &str, width: u16) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let mut end = 0;
    for text in wrap_ranges(input, width) {
        let from = match input[end..text.start].rfind('\n') {
            Some(newline) => end + newline + 1,
            None if rows.is_empty() => 0,
            None => text.start,
        };
        end = text.end;
        rows.push(Row { from, text });
    }
    if rows
        .last()
        .is_some_and(|r| display_width(&input[r.text.clone()]) >= width)
    {
        let len = input.len();
        rows.push(Row {
            from: len,
            text: len..len,
        });
    }
    rows
}

/// Which of `rows` the cursor at byte `at` is on, and its column there. At
/// the end of a full row that is the padding cell after the text (column
/// `width`); in the spaces a soft break dropped, past the row's end, as far
/// as that.
pub fn caret(input: &str, rows: &[Row], width: u16, at: usize) -> (usize, u16) {
    let index = rows.iter().rposition(|r| r.from <= at).unwrap_or(0);
    let Some(row) = rows.get(index) else {
        return (0, 0);
    };
    let col = if at <= row.text.start {
        0
    } else if at <= row.text.end {
        display_width(&input[row.text.start..at])
    } else {
        // Spaces: a cell each.
        let past = u16::try_from(at - row.text.end).unwrap_or(u16::MAX);
        display_width(&input[row.text.clone()])
            .saturating_add(past)
            .min(width)
    };
    (index, col)
}

/// The cursor position on `row` at column `col`, or before the grapheme
/// that covers it, or the row's end if it is shorter.
pub fn at_column(input: &str, row: &Row, col: u16) -> usize {
    let mut x = 0u16;
    let mut at = row.text.start;
    for (i, g) in input[row.text.clone()].grapheme_indices(true) {
        x = x.saturating_add(grapheme_width(g));
        if x > col {
            break;
        }
        at = row.text.start + i + g.len();
    }
    at
}

/// The first of `count` rows to show in a box `shown` rows tall: as near
/// `top` as keeps row `caret` in view.
pub fn first_row(top: usize, caret: usize, count: usize, shown: usize) -> usize {
    let lowest = (caret + 1).saturating_sub(shown.max(1));
    let highest = caret.min(count.saturating_sub(shown.max(1)));
    top.clamp(lowest, highest.max(lowest))
}

pub fn render(canvas: &mut Canvas, app: &App, layout: &Layout) {
    let rect = layout.composer;
    if rect.width < TEXT + 3 || rect.height < 3 {
        return;
    }
    let focused = app.focus == Focus::Composer;
    let border = Ink::new(if focused { role::DIM } else { role::FRAME }, role::MAIN_BG);
    let (left, right) = (rect.x, rect.right() - 1);
    let (top, bottom) = (rect.y, rect.bottom() - 1);
    canvas.hline(left, top, rect.width, "─", border);
    canvas.hline(left, bottom, rect.width, "─", border);
    for y in top + 1..bottom {
        canvas.text(left, y, "│", border, right);
        canvas.text(right, y, "│", border, right + 1);
    }
    canvas.text(left, top, "╭", border, right);
    canvas.text(right, top, "╮", border, right + 1);
    canvas.text(left, bottom, "╰", border, right);
    canvas.text(right, bottom, "╯", border, right + 1);

    // The hint ends two cells before the corner: `… a attach ──╮`. While
    // an `@` ghost shows, it says how to take it (and which match it is);
    // after one `Esc` in a draft, that another clears it.
    let ghost = app.ghost();
    let hint = match &ghost {
        Some(g) if g.count > 1 => format!(" tab complete · ↑↓ {}/{} ", g.index + 1, g.count),
        Some(_) => GHOST_HINT.to_string(),
        None if focused && app.clear_armed && !app.input.is_empty() => CLEAR_HINT.to_string(),
        None if layout.wide => HINT_WIDE.to_string(),
        None => HINT.to_string(),
    };
    let hint_end = right.saturating_sub(2);
    if display_width(&hint) + 2 <= hint_end.saturating_sub(left) {
        let x = hint_end - display_width(&hint);
        let quiet = Ink::new(role::QUIET, role::MAIN_BG);
        canvas.text(x, top, &hint, quiet, hint_end);
    }

    let text_x = left + TEXT;
    let limit = right - 1;
    let shown_rows = rect.height - 2;
    canvas.text(
        left + PROMPT,
        top + 1,
        "›",
        Ink::new(role::ACCENT, role::MAIN_BG),
        limit,
    );
    let block = Ink::new(palette::VOID, role::ACCENT);

    if app.input.is_empty() {
        // Fog, not ash: a placeholder still has to be read.
        let hint = placeholder(app);
        canvas.text(
            text_x,
            top + 1,
            &hint,
            Ink::new(role::META, role::MAIN_BG),
            limit,
        );
        if focused {
            // The block caret sits on the placeholder's first letter.
            let first = hint.graphemes(true).next().unwrap_or(" ");
            canvas.text(text_x, top + 1, first, block, limit);
        }
        return;
    }

    let width = limit - text_x;
    let drawn = shown(&app.input);
    let input = drawn.as_ref();
    let rows = rows(input, width);
    let at = app.cursor();
    let (caret_row, caret_col) = caret(input, &rows, width, at);
    let first = first_row(
        app.composer_top,
        caret_row,
        rows.len(),
        usize::from(shown_rows),
    );
    let text = Ink::new(role::TEXT, role::MAIN_BG);
    for (row, y) in rows[first..].iter().zip(top + 1..bottom) {
        canvas.text(text_x, y, &input[row.text.clone()], text, limit);
    }
    if !focused {
        return;
    }
    let Some(y) = caret_row
        .checked_sub(first)
        .and_then(|r| u16::try_from(r).ok())
        .map(|r| top + 1 + r)
        .filter(|&y| y < bottom)
    else {
        return;
    };
    // The caret may sit on the padding cell after a full row: that one
    // cell, never the border.
    let x = text_x + caret_col;
    let row = &rows[caret_row];
    let after = &input[at.clamp(row.text.start, row.text.end)..row.text.end];
    match ghost.map(|g| g.rest).filter(|rest| !rest.is_empty()) {
        // A ghost starts under the caret, as the placeholder does, and runs
        // on in fog; the rest of the caret's row makes way for it, and the
        // edge cuts them. Nothing before the cursor moves, no other row
        // moves, and nothing wraps.
        Some(rest) => {
            // Blank first: a wide character pushed past the edge leaves
            // nothing of what was drawn there.
            canvas.fill(Rect::new(x, y, limit.saturating_sub(x), 1), text);
            let mut letters = rest.graphemes(true);
            let lead = letters.next().unwrap_or(" ");
            let x = canvas.text(x, y, lead, block, limit + 1);
            let fog = Ink::new(role::META, role::MAIN_BG);
            let x = canvas.text(x, y, letters.as_str(), fog, limit);
            canvas.text(x, y, after, text, limit);
        }
        // On the character under the cursor (a whole wide one), or on a
        // space at the end of a line.
        None => {
            let under = after
                .graphemes(true)
                .find(|g| grapheme_width(g) > 0)
                .unwrap_or(" ");
            canvas.text(x, y, under, block, limit + 1);
        }
    }
}

/// What the empty composer says it is for.
fn placeholder(app: &App) -> String {
    match &app.selected {
        // Nothing is known yet (the server has not answered).
        _ if app.channels.is_empty() && app.loading => "waiting for the server…".to_string(),
        _ if app.channels.is_empty() => "press n to create a channel".to_string(),
        Selection::Channel(name) => format!("message #{name}"),
        Selection::Announcements => "pick a channel to post in".to_string(),
    }
}
