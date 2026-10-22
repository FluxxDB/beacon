//! The composer: a rounded box with the key hint in its top border, the accent
//! `›` prompt, the text being typed (wrapped, growing up to five lines) and a
//! block caret after it.

use crate::tui::state::{App, Focus, Selection};
use crate::tui::theme::{palette, role, Ink};
use crate::tui::view::Layout;
use crate::tui::widgets::Canvas;
use crate::tui::wrap::{display_width, wrap};
use unicode_segmentation::UnicodeSegmentation;

/// Most text lines shown; longer input scrolls to keep the caret in view.
pub const MAX_LINES: u16 = 5;
/// Columns from the box's left edge: the prompt, then the text.
const PROMPT: u16 = 2;
const TEXT: u16 = 4;
const HINT_WIDE: &str = " enter send · a attach a file ";
const HINT: &str = " enter send · a attach ";

/// Cells for text in a composer box `box_width` wide: border, space and
/// prompt and space on the left; a space and border on the right.
pub fn text_width(box_width: u16) -> u16 {
    box_width.saturating_sub(TEXT + 2)
}

/// Lines the composer needs for the current input, caret included.
pub fn line_count(app: &App, text_width: u16) -> u16 {
    u16::try_from(input_lines(&app.input, text_width).len()).unwrap_or(u16::MAX)
}

/// The input wrapped to `width`, plus an empty last line when the caret
/// would not fit after the text.
fn input_lines(input: &str, width: u16) -> Vec<String> {
    let mut lines = wrap(input, width);
    if lines.last().is_some_and(|l| display_width(l) >= width) {
        lines.push(String::new());
    }
    lines
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

    // The hint ends two cells before the corner: `… a attach ──╮`.
    let hint = if layout.wide { HINT_WIDE } else { HINT };
    let hint_end = right.saturating_sub(2);
    if display_width(hint) + 2 <= hint_end.saturating_sub(left) {
        let x = hint_end - display_width(hint);
        canvas.text(x, top, hint, Ink::new(role::QUIET, role::MAIN_BG), hint_end);
    }

    let text_x = left + TEXT;
    let limit = right - 1;
    let rows = rect.height - 2;
    canvas.text(
        left + PROMPT,
        top + 1,
        "›",
        Ink::new(role::ACCENT, role::MAIN_BG),
        limit,
    );
    let caret = Ink::new(palette::VOID, role::ACCENT);

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
            canvas.text(text_x, top + 1, first, caret, limit);
        }
        return;
    }

    let lines = input_lines(&app.input, limit - text_x);
    let shown = &lines[lines.len().saturating_sub(usize::from(rows))..];
    let text = Ink::new(role::TEXT, role::MAIN_BG);
    let mut end = (text_x, top + 1);
    for (line, y) in shown.iter().zip(top + 1..bottom) {
        end = (canvas.text(text_x, y, line, text, limit), y);
    }
    if focused {
        canvas.text(end.0, end.1, " ", caret, limit);
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
