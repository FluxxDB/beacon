//! Display-width wrapping and truncation. Widths are terminal cells
//! (unicode-width, via ratatui's [`CellWidth`] so they agree with the buffer),
//! and text is only ever cut between grapheme clusters.

use std::ops::Range;

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
    wrap_ranges(text, width)
        .into_iter()
        .map(|line| text[line].to_string())
        .collect()
}

/// The lines [`wrap`] makes, each as the bytes of `text` it shows. Between
/// one line and the next there is nothing but the spaces dropped at a soft
/// break, or a hard break's `\n` (and any indentation dropped after it). An
/// empty line at the end of a paragraph sits at the paragraph's end.
pub fn wrap_ranges(text: &str, width: u16) -> Vec<Range<usize>> {
    let width = usize::from(width.max(1));
    let mut lines = Vec::new();
    let mut start = 0;
    for paragraph in text.split('\n') {
        wrap_paragraph(paragraph, start, width, &mut lines);
        start += paragraph.len() + 1;
    }
    lines
}

/// Wraps one paragraph that starts at byte `base` of the text.
fn wrap_paragraph(paragraph: &str, base: usize, width: usize, lines: &mut Vec<Range<usize>>) {
    // The line being filled, as bytes of the paragraph; empty while nothing
    // is on it.
    let mut line = 0..0;
    let mut line_w = 0usize;
    // Whether `line` started at a soft break: spaces there are dropped.
    let mut after_break = false;
    // `split(' ')` yields an empty word for each extra space, which keeps
    // runs of spaces inside a line.
    let mut word_start = 0;
    for (i, word) in paragraph.split(' ').enumerate() {
        let start = word_start;
        let end = start + word.len();
        word_start = end + 1;
        let at_break = after_break && line.is_empty();
        if at_break && word.is_empty() {
            continue;
        }
        let word_w = cells(word);
        let gap = usize::from(i > 0 && !at_break);
        if line_w + gap + word_w <= width {
            // The gap is the space just before the word.
            if line.is_empty() {
                line.start = start - gap;
            }
            line.end = end;
            line_w += gap + word_w;
            continue;
        }
        // The word goes on a new line; a line of nothing but indentation is
        // dropped rather than left blank.
        if paragraph[line.clone()].trim_start_matches(' ').is_empty() {
            line = 0..0;
        } else {
            push_line(paragraph, base, lines, &mut line);
            after_break = true;
        }
        line_w = 0;
        if word_w <= width {
            line = start..end;
            line_w = word_w;
            continue;
        }
        // Longer than a whole line: break it between graphemes.
        for (offset, g) in word.grapheme_indices(true) {
            let g_w = usize::from(grapheme_width(g));
            if line_w + g_w > width && !line.is_empty() {
                push_line(paragraph, base, lines, &mut line);
                line_w = 0;
            }
            if line.is_empty() {
                line.start = start + offset;
            }
            line.end = start + offset + g.len();
            line_w += g_w;
        }
    }
    if line.is_empty() {
        line = paragraph.len()..paragraph.len();
    }
    lines.push(base + line.start..base + line.end);
}

/// Ends `line` (without its trailing spaces) and starts an empty one.
fn push_line(paragraph: &str, base: usize, lines: &mut Vec<Range<usize>>, line: &mut Range<usize>) {
    let kept = paragraph[line.clone()].trim_end_matches(' ').len();
    lines.push(base + line.start..base + line.start + kept);
    *line = 0..0;
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
