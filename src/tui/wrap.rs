//! Display-width wrapping and truncation. Widths are terminal cells
//! (unicode-width, via ratatui's [`CellWidth`] so they agree with the buffer),
//! and text is only ever cut between grapheme clusters.

use ratatui::buffer::CellWidth;
use unicode_segmentation::UnicodeSegmentation;

/// Cells taken by one grapheme cluster; control characters take none.
pub fn grapheme_width(g: &str) -> u16 {
    if g.chars().any(char::is_control) {
        0
    } else {
        g.cell_width()
    }
}

/// Cells taken by `s`, saturating at `u16::MAX` (a post body can be wider).
pub fn display_width(s: &str) -> u16 {
    u16::try_from(cells(s)).unwrap_or(u16::MAX)
}

/// Cells taken by `s`, without a ceiling. Widths are summed in `usize` here,
/// since one valid post body (64 KiB) is wider than `u16` can count.
fn cells(s: &str) -> usize {
    s.graphemes(true)
        .map(|g| usize::from(grapheme_width(g)))
        .sum()
}

/// Wraps `text` into lines at most `width` cells wide (a zero width counts as
/// one). `\n` always ends a line. Lines break at spaces, dropping the spaces
/// at a break; a word longer than a line starts on a fresh line and is broken
/// between graphemes. Nothing but those spaces is dropped.
pub fn wrap(text: &str, width: u16) -> Vec<String> {
    let width = usize::from(width.max(1));
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        wrap_paragraph(paragraph, width, &mut lines);
    }
    lines
}

fn wrap_paragraph(paragraph: &str, width: usize, lines: &mut Vec<String>) {
    let mut line = String::new();
    let mut line_w = 0usize;
    // Whether `line` started at a soft break: spaces there are dropped.
    let mut after_break = false;
    // `split(' ')` yields an empty word for each extra space, which keeps
    // runs of spaces inside a line.
    for (i, word) in paragraph.split(' ').enumerate() {
        let at_break = after_break && line.is_empty();
        if at_break && word.is_empty() {
            continue;
        }
        let word_w = cells(word);
        let gap = usize::from(i > 0 && !at_break);
        if line_w + gap + word_w <= width {
            if gap == 1 {
                line.push(' ');
            }
            line.push_str(word);
            line_w += gap + word_w;
            continue;
        }
        // The word goes on a new line; a line of nothing but indentation is
        // dropped rather than left blank.
        if line.trim_start_matches(' ').is_empty() {
            line.clear();
        } else {
            push_line(lines, &mut line);
            after_break = true;
        }
        line_w = 0;
        if word_w <= width {
            line.push_str(word);
            line_w = word_w;
            continue;
        }
        // Longer than a whole line: break it between graphemes.
        for g in word.graphemes(true) {
            let g_w = usize::from(grapheme_width(g));
            if line_w + g_w > width && !line.is_empty() {
                push_line(lines, &mut line);
                line_w = 0;
            }
            line.push_str(g);
            line_w += g_w;
        }
    }
    lines.push(line);
}

fn push_line(lines: &mut Vec<String>, line: &mut String) {
    let trimmed = line.trim_end_matches(' ').len();
    line.truncate(trimmed);
    lines.push(std::mem::take(line));
}

/// `s` cut to at most `width` cells, ending in `…` when anything was cut.
pub fn truncate(s: &str, width: u16) -> String {
    let width = usize::from(width);
    if cells(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for g in s.graphemes(true) {
        let g_w = usize::from(grapheme_width(g));
        if used + g_w > width - 1 {
            break;
        }
        out.push_str(g);
        used += g_w;
    }
    out.push('…');
    out
}

/// `s` cut from the front to at most `width` cells, starting with `…` when
/// anything was cut (keeps the end of text being typed in view).
pub fn truncate_start(s: &str, width: u16) -> String {
    let width = usize::from(width);
    if cells(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for g in s.graphemes(true).rev() {
        let g_w = usize::from(grapheme_width(g));
        if used + g_w > width - 1 {
            break;
        }
        kept.push(g);
        used += g_w;
    }
    kept.reverse();
    format!("…{}", kept.concat())
}
