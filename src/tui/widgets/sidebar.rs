//! The sidebar: Announcements, the channels with unread counts, `+ new
//! channel`, then the agents with presence and age.

use std::slice;

use ratatui::layout::Rect;

use crate::timefmt::age_short;
use crate::tui::state::{App, Focus, Selection};
use crate::tui::theme::{palette, role, Ink, Rgb};
use crate::tui::widgets::{Canvas, Seg};
use crate::tui::wrap::truncate;

/// Columns from the sidebar's left edge: the glyph (`◆`, `#`, `●`) and the
/// name after it. Counts and ages end `RIGHT_PAD` cells before the right edge.
const GLYPH: u16 = 2;
const NAME: u16 = 4;
const RIGHT_PAD: u16 = 2;

/// One sidebar line.
struct Row {
    /// Lifted, with the accent edge rule: the view on screen.
    selected: bool,
    /// Lifted, with a quiet edge rule: the agent row under the cursor (as a
    /// selected post is marked; the accent stays on the view on screen).
    cursor: bool,
    /// Drawn at the glyph column, the name then starting at the name column.
    glyph: Option<Seg>,
    name: String,
    name_ink: Ink,
    /// A count or age, right-aligned.
    right: Option<Seg>,
}

pub fn render(canvas: &mut Canvas, app: &App, rect: Rect) {
    let mut rows: Vec<Option<Row>> = vec![None];
    rows.push(Some(announcements(app)));
    rows.push(None);
    // While the sidebar has the keys, its section label brightens and says
    // which keys move in it (as `n` sits beside `+ new channel`).
    let mut channels = label("CHANNELS", None);
    if app.focus == Focus::Sidebar {
        channels.name_ink = panel(role::TEXT);
        channels.right = Some(("j k".to_string(), panel(role::META)));
    }
    rows.push(Some(channels));
    rows.push(None);
    rows.extend(app.channels.iter().map(|ch| {
        let selected = app.selected == Selection::Channel(ch.name.clone());
        let count = (ch.unread > 0).then(|| ch.unread.to_string());
        Some(if selected {
            let bright = Ink::new(role::STRONG, role::LIFTED);
            Row {
                selected,
                cursor: false,
                glyph: None,
                name: format!("# {}", ch.name),
                name_ink: bright,
                right: count.map(|c| (c, bright)),
            }
        } else {
            let fg = if ch.unread > 0 {
                role::TEXT
            } else {
                role::META
            };
            Row {
                selected,
                cursor: false,
                glyph: Some(("# ".to_string(), panel(role::QUIET))),
                name: ch.name.clone(),
                name_ink: panel(fg),
                right: count.map(|c| (c, panel(role::TEXT))),
            }
        })
    }));
    rows.push(None);
    rows.push(Some(label(
        "+ new channel",
        Some(("n".to_string(), panel(role::META))),
    )));
    rows.extend([None, None]);
    rows.push(Some(label("AGENTS", None)));
    rows.push(None);
    if app.agents.is_empty() {
        let mut none = label("none seen lately", None);
        none.name_ink = panel(role::META);
        rows.push(Some(none));
    }
    let under_cursor = match app.focus {
        Focus::Sidebar => app.selected_agent.as_deref(),
        _ => None,
    };
    rows.extend(app.agents.iter().map(|agent| {
        let (dot, dot_fg, name_fg) = if agent.active {
            ("●", palette::GREEN, role::TEXT)
        } else {
            ("○", role::QUIET, role::META)
        };
        let cursor = under_cursor == Some(agent.handle.as_str());
        let (bg, name_fg) = if cursor {
            (role::LIFTED, role::STRONG)
        } else {
            (role::SIDEBAR_BG, name_fg)
        };
        Some(Row {
            selected: false,
            cursor,
            glyph: Some((dot.to_string(), Ink::new(dot_fg, bg))),
            name: agent.handle.clone(),
            name_ink: Ink::new(name_fg, bg),
            right: Some((
                age_short(app.now_ms - agent.last_seen_ms),
                Ink::new(role::QUIET, bg),
            )),
        })
    }));

    // Whatever doesn't fit is cut off at the bottom.
    for (row, y) in rows.iter().zip(rect.top()..rect.bottom()) {
        if let Some(row) = row {
            draw(canvas, rect, y, row);
        }
    }
}

fn panel(fg: Rgb) -> Ink {
    Ink::new(fg, role::SIDEBAR_BG)
}

fn label(text: &str, right: Option<Seg>) -> Row {
    Row {
        selected: false,
        cursor: false,
        glyph: None,
        name: text.to_string(),
        name_ink: panel(role::QUIET),
        right,
    }
}

fn announcements(app: &App) -> Row {
    let selected = app.selected == Selection::Announcements;
    let unread = app.announcements_unread;
    let fg = if unread > 0 || selected {
        role::STRONG
    } else {
        role::META
    };
    let bg = if selected {
        role::LIFTED
    } else {
        role::SIDEBAR_BG
    };
    let ink = Ink::new(fg, bg);
    Row {
        selected,
        cursor: false,
        glyph: Some(("◆".to_string(), ink)),
        name: "Announcements".to_string(),
        name_ink: ink,
        right: (unread > 0).then(|| (unread.to_string(), ink)),
    }
}

fn draw(canvas: &mut Canvas, rect: Rect, y: u16, row: &Row) {
    if row.selected {
        canvas.fill(
            Rect::new(rect.x, y, rect.width, 1),
            Ink::new(role::TEXT, role::LIFTED),
        );
        canvas.text(
            rect.x,
            y,
            "▍",
            Ink::new(role::ACCENT, role::LIFTED),
            rect.right(),
        );
    }
    if row.cursor {
        canvas.fill(
            Rect::new(rect.x, y, rect.width, 1),
            Ink::new(role::TEXT, role::LIFTED),
        );
        canvas.text(
            rect.x,
            y,
            "▍",
            Ink::new(role::META, role::LIFTED),
            rect.right(),
        );
    }
    let count_right = rect.right().saturating_sub(RIGHT_PAD);
    // The name stops one cell short of the count, truncating with `…`.
    let name_limit = match &row.right {
        Some(seg) => canvas
            .segs_right(count_right, y, slice::from_ref(seg))
            .saturating_sub(1),
        None => count_right,
    };
    let x = match &row.glyph {
        Some(glyph) => {
            canvas.segs(rect.x + GLYPH, y, slice::from_ref(glyph), rect.x + NAME);
            rect.x + NAME
        }
        None => rect.x + GLYPH,
    };
    let name = truncate(&row.name, name_limit.saturating_sub(x));
    canvas.text(x, y, &name, row.name_ink, name_limit);
}
