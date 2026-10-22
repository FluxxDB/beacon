//! Text from the board on its way to a terminal, for the CLI and the TUI alike.

/// Characters that must not reach a terminal as they are: controls (escape
/// sequences start with one), the line and paragraph separators, and the bidi
/// marks, overrides (U+202A to U+202E) and isolates (U+2066 to U+2069), which
/// reorder what is shown, so a name could pass for another or a body hide
/// text. The CLI shows each as U+FFFD; the TUI leaves it out.
pub fn is_unsafe(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{2028}' | '\u{2029}' | '\u{200e}' | '\u{200f}')
        || ('\u{202a}'..='\u{202e}').contains(&c)
        || ('\u{2066}'..='\u{2069}').contains(&c)
}
