//! The palette (Dang's Linear-style tokens), the color roles measured from
//! the approved mockup, and the xterm-256 fallback for terminals without
//! 24-bit color.

use std::cmp::Reverse;

use ratatui::style::{Color, Modifier, Style};

/// A 24-bit color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// The only colors the TUI paints with.
pub mod palette {
    use super::Rgb;

    pub const VOID: Rgb = Rgb(0x08, 0x09, 0x0a);
    pub const CARBON: Rgb = Rgb(0x0f, 0x10, 0x11);
    pub const OBSIDIAN: Rgb = Rgb(0x16, 0x17, 0x18);
    pub const GRAPHITE: Rgb = Rgb(0x23, 0x25, 0x2a);
    pub const SMOKE: Rgb = Rgb(0x38, 0x3b, 0x3f);
    // ash, lifted to #787d85 for contrast (Dang, 2026-10-22)
    pub const ASH: Rgb = Rgb(0x78, 0x7d, 0x85);
    pub const FOG: Rgb = Rgb(0x8a, 0x8f, 0x98);
    pub const MIST: Rgb = Rgb(0xd0, 0xd6, 0xe0);
    pub const PAPER: Rgb = Rgb(0xff, 0xff, 0xff);
    /// The one accent. Only on: the active channel's edge, the composer
    /// prompt and caret, an announcement's edge rule, a dialog's primary action,
    /// the unread line (Dang, 2026-10-28).
    /// A blue standing in for the mockup's acid lime #e4f222 (Dang, 2026-10-22):
    /// 6.2:1 on obsidian, 6.85:1 under void text, its own xterm-256 index (69),
    /// and clear of iris, lavender and teal.
    pub const AZURE: Rgb = Rgb(0x40, 0x99, 0xff);
    pub const GREEN: Rgb = Rgb(0x27, 0xa6, 0x44);
    pub const CORAL: Rgb = Rgb(0xeb, 0x57, 0x57);
    pub const TEAL: Rgb = Rgb(0x02, 0xb8, 0xcc);
    pub const IRIS: Rgb = Rgb(0x63, 0x66, 0xf1);
    pub const LAVENDER: Rgb = Rgb(0x8b, 0x5c, 0xf6);
}

/// What each part of the screen is painted with, as measured from the mockup.
pub mod role {
    use super::palette::*;
    use super::Rgb;

    /// Outer frame, pane separators and the title rule.
    pub const FRAME: Rgb = GRAPHITE;
    pub const SIDEBAR_BG: Rgb = CARBON;
    pub const MAIN_BG: Rgb = VOID;
    /// Plain text, and the ink of blank cells.
    pub const TEXT: Rgb = MIST;
    /// Names, `◆ beacon`, titles, mentions and announcement bodies.
    pub const STRONG: Rgb = PAPER;
    /// Metadata: client · device, topics, inactive channels, field labels.
    pub const META: Rgb = FOG;
    /// Quiet labels: section names, times, ages, hints, help words.
    pub const QUIET: Rgb = ASH;
    /// A lifted row: the announcement and mention bands, the active channel,
    /// tag badges, dialog fields and the attachment chip.
    pub const LIFTED: Rgb = OBSIDIAN;
    /// The composer's border, and everything behind an open dialog.
    pub const DIM: Rgb = SMOKE;
    pub const ACCENT: Rgb = AZURE;
    pub const DIALOG_BG: Rgb = CARBON;
    pub const ACTIVE: Rgb = GREEN;
    pub const ERROR: Rgb = CORAL;

    /// The ink of a tag badge.
    pub fn tag(tag: &str) -> Rgb {
        match tag {
            "claim" => IRIS,
            "done" => GREEN,
            "blocked" => CORAL,
            "question" => TEAL,
            "handoff" => LAVENDER,
            _ => FOG,
        }
    }
}

/// Foreground, background and weight of a run of cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ink {
    pub fg: Rgb,
    pub bg: Rgb,
    pub bold: bool,
}

impl Ink {
    pub const fn new(fg: Rgb, bg: Rgb) -> Self {
        Ink {
            fg,
            bg,
            bold: false,
        }
    }

    /// The same ink in bold (names and titles only).
    pub const fn bold(self) -> Self {
        Ink { bold: true, ..self }
    }

    /// The same ink over another background.
    pub const fn on(self, bg: Rgb) -> Self {
        Ink { bg, ..self }
    }
}

/// How many colors the terminal takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    TrueColor,
    /// No `COLORTERM=truecolor|24bit`: every color maps to the nearest xterm-256 index.
    Ansi256,
}

impl ColorMode {
    /// Reads `COLORTERM`.
    pub fn detect() -> Self {
        Self::from_colorterm(std::env::var("COLORTERM").ok().as_deref())
    }

    /// `truecolor` or `24bit` (any case) means 24-bit color; anything else, 256.
    pub fn from_colorterm(value: Option<&str>) -> Self {
        match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("truecolor" | "24bit") => ColorMode::TrueColor,
            _ => ColorMode::Ansi256,
        }
    }
}

/// Turns palette colors into terminal colors for the detected [`ColorMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub mode: ColorMode,
}

impl Theme {
    pub fn new(mode: ColorMode) -> Self {
        Theme { mode }
    }

    /// The terminal color for `rgb`.
    pub fn c(&self, rgb: Rgb) -> Color {
        match self.mode {
            ColorMode::TrueColor => Color::Rgb(rgb.0, rgb.1, rgb.2),
            ColorMode::Ansi256 => Color::Indexed(nearest_xterm256(rgb)),
        }
    }

    /// A ratatui style that sets fg, bg and weight outright.
    pub fn style(&self, ink: Ink) -> Style {
        let style = Style::new().fg(self.c(ink.fg)).bg(self.c(ink.bg));
        if ink.bold {
            style.add_modifier(Modifier::BOLD)
        } else {
            style.remove_modifier(Modifier::BOLD)
        }
    }
}

/// The xterm-256 index nearest to `rgb` by squared RGB distance, over the
/// 6×6×6 color cube (16–231) and the 24 greys (232–255). The first 16 are
/// left out: terminals redefine them. Ties go to the lighter color, so a
/// lifted surface never collapses onto the panel below it (obsidian sits
/// exactly between greys 233 and 234; carbon takes 233).
pub fn nearest_xterm256(rgb: Rgb) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let distance = |c: (u8, u8, u8)| -> u32 {
        let d = |a: u8, b: u8| (i32::from(a) - i32::from(b)).pow(2) as u32;
        d(rgb.0, c.0) + d(rgb.1, c.1) + d(rgb.2, c.2)
    };
    let nearest_level = |v: u8| -> usize {
        (0..LEVELS.len())
            .min_by_key(|&i| ((i32::from(LEVELS[i]) - i32::from(v)).abs(), Reverse(i)))
            .unwrap_or(0)
    };
    let (r, g, b) = (
        nearest_level(rgb.0),
        nearest_level(rgb.1),
        nearest_level(rgb.2),
    );
    let cube_index = 16 + 36 * r + 6 * g + b;
    let cube_distance = distance((LEVELS[r], LEVELS[g], LEVELS[b]));

    let (grey_index, grey_distance) = (0..24u8)
        .map(|i| {
            let level = 8 + 10 * i;
            (232 + usize::from(i), distance((level, level, level)))
        })
        .min_by_key(|&(i, d)| (d, Reverse(i)))
        .unwrap_or((232, u32::MAX));

    let cube_light = u32::from(LEVELS[r]) + u32::from(LEVELS[g]) + u32::from(LEVELS[b]);
    let grey_light = 3 * (8 + 10 * (grey_index as u32 - 232));
    let index = if (grey_distance, Reverse(grey_light)) < (cube_distance, Reverse(cube_light)) {
        grey_index
    } else {
        cube_index
    };
    index as u8
}
