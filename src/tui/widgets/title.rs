//! The main pane's title row (`# beacon   topic`) and its rule, and the
//! one-line channel switcher that stands in for the sidebar below 80 columns.

use crate::tui::state::{App, Selection};
use crate::tui::theme::{role, Ink};
use crate::tui::view::Layout;
use crate::tui::widgets::{segs_width, Canvas, Seg};
use crate::tui::wrap::{display_width, truncate};

pub fn render(canvas: &mut Canvas, app: &App, layout: &Layout) {
    let main = layout.main;
    let ink = |fg| Ink::new(fg, role::MAIN_BG);
    let left = main.x + 1;
    let mut right = main.right() - 1;
    canvas.hline(
        left,
        layout.rule_y,
        main.width.saturating_sub(2),
        "─",
        ink(role::FRAME),
    );

    // An active filter, right-aligned where the post badges end.
    if let Some(filter) = app.filter.as_deref().filter(|f| !f.is_empty()) {
        let segs: Vec<Seg> = vec![
            ("filter ".to_string(), ink(role::QUIET)),
            (format!("/{filter}"), ink(role::TEXT)),
        ];
        let end = main.right().saturating_sub(3);
        if segs_width(&segs) + 2 < end.saturating_sub(left) {
            right = canvas.segs_right(end, layout.title_y, &segs) - 2;
        }
    }

    let Some((name, topic)) = heading(app) else {
        return;
    };
    let x = canvas.text(
        left,
        layout.title_y,
        &truncate(&name, right.saturating_sub(left)),
        ink(role::STRONG).bold(),
        right,
    );
    if let Some(topic) = topic.filter(|t| !t.is_empty()) {
        // `# name`, a plain space, then the topic two spaces further on.
        let start = x + 1;
        let topic = truncate(&format!("  {topic}"), right.saturating_sub(start));
        canvas.text(start, layout.title_y, &topic, ink(role::META), right);
    }
}

/// The bold name and the topic beside it.
fn heading(app: &App) -> Option<(String, Option<String>)> {
    match &app.selected {
        Selection::Announcements => Some((
            "◆ Announcements".to_string(),
            Some(format!("{}'s posts from every channel", app.me)),
        )),
        Selection::Channel(_) if app.channels.is_empty() => None,
        Selection::Channel(name) => {
            let topic = app
                .channels
                .iter()
                .find(|c| &c.name == name)
                .and_then(|c| c.topic.clone());
            Some((format!("# {name}"), topic))
        }
    }
}

/// `◆ 1  ‹ # beacon ›  3 unread elsewhere`: announcements, the current view,
/// and how much is waiting in the other channels.
pub fn switcher(canvas: &mut Canvas, app: &App, layout: &Layout) {
    let Some(y) = layout.switcher_y else { return };
    let ink = |fg| Ink::new(fg, role::MAIN_BG);
    let unread = app.announcements_unread;
    let mut segs: Vec<Seg> = Vec::new();
    if unread > 0 {
        segs.push(("◆".to_string(), ink(role::STRONG)));
        segs.push((format!(" {unread}"), ink(role::STRONG)));
    } else {
        segs.push(("◆".to_string(), ink(role::META)));
    }
    let current = match &app.selected {
        Selection::Announcements => "◆ Announcements".to_string(),
        Selection::Channel(name) => format!("# {name}"),
    };
    segs.push(("  ‹ ".to_string(), ink(role::QUIET)));
    segs.push((current, ink(role::STRONG)));
    segs.push((" ›".to_string(), ink(role::QUIET)));
    let main = layout.main;
    let room = main.width.saturating_sub(2);
    let elsewhere: u32 = app
        .channels
        .iter()
        .filter(|c| app.selected != Selection::Channel(c.name.clone()))
        .map(|c| c.unread)
        .sum();
    // The count says less as room runs out, rather than being cut mid-word.
    if elsewhere > 0 {
        let fits = [
            format!("  {elsewhere} unread elsewhere"),
            format!("  {elsewhere} unread"),
        ]
        .into_iter()
        .find(|text| segs_width(&segs) + display_width(text) <= room);
        if let Some(text) = fits {
            segs.push((text, ink(role::META)));
        }
    }
    canvas.segs(main.x + 1, y, &segs, main.right() - 1);
}
