//! One frame of the TUI: the layout (measured from the approved mockup), the
//! outer frame, each widget, then any dialog over the dimmed board.
//!
//! ```text
//! ╭─ ◆ beacon ───┬──────────────── pc · 2 peers · ● synced 0.4s ago ─╮  top border + status
//! │              │                                                    │  blank (≥ 80 cols) or channel switcher (< 80)
//! │  sidebar     │ # beacon   topic                                   │  title
//! │              │ ────────────────────────────────────────────────── │  rule
//! │              │                                                    │  padding
//! │              │  posts, bottom-anchored                            │  posts viewport
//! │              │                                                    │  notice row (scroll hint)
//! │              │╭─────────────────────────── enter send · a attach ─╮│  composer
//! │              ││ › text                                           ││
//! │              │╰──────────────────────────────────────────────────╯│
//! ├──────────────┴────────────────────────────────────────────────────┤  rule (right under the composer)
//! │  ? help   tab focus   …                                           │  help row
//! ╰───────────────────────────────────────────────────────────────────╯
//! ```

use ratatui::layout::Rect;
use ratatui::Frame;

use crate::tui::state::App;
use crate::tui::theme::{role, Ink, Theme};
use crate::tui::widgets::{composer, dialog, header, helpbar, posts, sidebar, title, Canvas};
use crate::tui::wrap::{display_width, wrap};

/// Sidebar inner width at 100 columns and wider.
pub const SIDEBAR_WIDE: u16 = 23;
/// Sidebar inner width from 80 to 99 columns; below 80 there is no sidebar.
pub const SIDEBAR_NARROW: u16 = 21;
/// Narrowest terminal the board is drawn in at all.
pub const MIN_WIDTH: u16 = 40;
/// Rows above the posts viewport: top border, blank/switcher, title, rule, padding.
const ROWS_ABOVE_POSTS: u16 = 5;
/// Rows below the composer: separator, help, bottom border. The composer
/// sits right on the separator; the posts have the rest (Dang, 2026-10-22).
const ROWS_BELOW_COMPOSER: u16 = 3;
/// Shortest terminal the board is drawn in at all: one row of posts, the
/// notice row and a one-line composer between the fixed rows.
pub const MIN_HEIGHT: u16 = ROWS_ABOVE_POSTS + 1 + 1 + 3 + ROWS_BELOW_COMPOSER;

/// Where everything goes, for one terminal size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub area: Rect,
    /// The sidebar's inner columns over the body rows (none below 80 columns).
    pub sidebar: Option<Rect>,
    /// The main pane's columns over the body rows.
    pub main: Rect,
    /// The one-line channel switcher that replaces the sidebar below 80 columns.
    pub switcher_y: Option<u16>,
    pub title_y: u16,
    pub rule_y: u16,
    pub posts: Rect,
    /// The row between the posts and the composer.
    pub notice_y: u16,
    /// The composer box, borders included.
    pub composer: Rect,
    pub help_y: u16,
    /// At least 100 columns: room for the longer hints.
    pub wide: bool,
}

impl Layout {
    /// The layout for `area`, or `None` when it is too small to draw the board.
    pub fn compute(area: Rect, app: &App) -> Option<Layout> {
        let (w, h) = (area.width, area.height);
        if w < MIN_WIDTH || h < MIN_HEIGHT {
            return None;
        }
        let sidebar_w = match w {
            100.. => Some(SIDEBAR_WIDE),
            80.. => Some(SIDEBAR_NARROW),
            _ => None,
        };
        let main_x = area.x + sidebar_w.map_or(1, |s| s + 2);
        let main_w = area.right() - 1 - main_x;
        let body_rows = h.checked_sub(4)?;

        let composer_w = main_w - 1;
        let wanted = composer::line_count(app, composer::text_width(composer_w));
        // Give the posts at least one row; the composer shrinks first.
        let fixed = ROWS_ABOVE_POSTS + 1 + 2 + ROWS_BELOW_COMPOSER;
        let room = h.checked_sub(fixed + 1)?;
        let composer_lines = wanted.clamp(1, composer::MAX_LINES).min(room);
        if composer_lines == 0 {
            return None;
        }
        let composer_h = composer_lines + 2;
        let composer_y = area.y + h - ROWS_BELOW_COMPOSER - composer_h;
        let notice_y = composer_y - 1;
        let posts_y = area.y + ROWS_ABOVE_POSTS;

        Some(Layout {
            area,
            sidebar: sidebar_w.map(|sw| Rect::new(area.x + 1, area.y + 1, sw, body_rows)),
            main: Rect::new(main_x, area.y + 1, main_w, body_rows),
            switcher_y: sidebar_w.is_none().then_some(area.y + 1),
            title_y: area.y + 2,
            rule_y: area.y + 3,
            posts: Rect::new(main_x, posts_y, main_w, notice_y - posts_y),
            notice_y,
            composer: Rect::new(main_x, composer_y, composer_w, composer_h),
            help_y: area.y + h - 2,
            wide: w >= 100,
        })
    }

    /// The column between the sidebar and the main pane.
    pub fn separator_x(&self) -> Option<u16> {
        self.sidebar.map(|s| s.right())
    }
}

/// Draws the whole board for `app`.
pub fn render(f: &mut Frame, app: &App, theme: &Theme) {
    let area = f.area();
    let mut canvas = Canvas::new(f.buffer_mut(), theme);
    canvas.fill(area, Ink::new(role::TEXT, role::MAIN_BG));
    let Some(layout) = Layout::compute(area, app) else {
        too_small(&mut canvas, area);
        return;
    };
    frame(&mut canvas, &layout);
    header::render(&mut canvas, app, &layout);
    match layout.sidebar {
        Some(rect) => sidebar::render(&mut canvas, app, rect),
        None => title::switcher(&mut canvas, app, &layout),
    }
    title::render(&mut canvas, app, &layout);
    posts::render(&mut canvas, app, &layout);
    composer::render(&mut canvas, app, &layout);
    helpbar::render(&mut canvas, app, &layout);
    if let Some(d) = &app.dialog {
        canvas.dim();
        dialog::render(&mut canvas, d, app, &layout);
    }
}

/// The rounded outer frame, the sidebar's panel and the rule above the help row.
fn frame(canvas: &mut Canvas, layout: &Layout) {
    let area = layout.area;
    let (left, right) = (area.x, area.right() - 1);
    let (top, bottom) = (area.y, area.bottom() - 1);
    let separator_y = layout.help_y - 1;
    let main = Ink::new(role::FRAME, role::MAIN_BG);
    // The sidebar's panel color reaches the frame on its side.
    let side = match layout.sidebar {
        Some(_) => main.on(role::SIDEBAR_BG),
        None => main,
    };
    let split = layout.separator_x();
    let panel_end = split.unwrap_or(left + 1);

    if let Some(sidebar) = layout.sidebar {
        canvas.fill(sidebar, Ink::new(role::TEXT, role::SIDEBAR_BG));
    }
    for y in [top, separator_y, bottom] {
        canvas.hline(left, y, panel_end - left, "─", side);
        canvas.hline(panel_end, y, right - panel_end, "─", main);
    }
    for y in top + 1..bottom {
        if y != separator_y {
            canvas.text(left, y, "│", side, right);
            canvas.text(right, y, "│", main, right + 1);
        }
    }
    canvas.text(left, top, "╭", side, right);
    canvas.text(right, top, "╮", main, right + 1);
    canvas.text(left, separator_y, "├", side, right);
    canvas.text(right, separator_y, "┤", main, right + 1);
    canvas.text(left, bottom, "╰", side, right);
    canvas.text(right, bottom, "╯", main, right + 1);
    if let Some(x) = split {
        canvas.text(x, top, "┬", main, right);
        for y in layout.main.top()..layout.main.bottom() {
            canvas.text(x, y, "│", main, right);
        }
        canvas.text(x, separator_y, "┴", main, right);
    }
}

/// Shown instead of the board when the terminal is too small for it: how
/// big it is, and how big it has to be.
fn too_small(canvas: &mut Canvas, area: Rect) {
    // Two short lines, each whole in anything narrower than the minimum.
    let message = format!(
        "Too small for beacon: {}×{}.\nResize to at least {MIN_WIDTH}×{MIN_HEIGHT}.",
        area.width, area.height
    );
    let lines = wrap(&message, area.width.saturating_sub(2));
    let top = area.height.saturating_sub(lines.len() as u16) / 2;
    let ink = Ink::new(role::META, role::MAIN_BG);
    for (line, y) in lines.iter().zip(area.y + top..area.bottom()) {
        let x = area.x + area.width.saturating_sub(display_width(line)) / 2;
        canvas.text(x, y, line, ink, area.right());
    }
}
