//! The top border: `◆ beacon` on the left, node, peers and sync on the right.

use crate::timefmt::age_short;
use crate::tui::state::App;
use crate::tui::theme::{palette, role, ColorMode, Ink};
use crate::tui::view::Layout;
use crate::tui::widgets::{segs_width, Canvas, Seg};

pub fn render(canvas: &mut Canvas, app: &App, layout: &Layout) {
    let area = layout.area;
    let title_bg = if layout.sidebar.is_some() {
        role::SIDEBAR_BG
    } else {
        role::MAIN_BG
    };
    let title_end = canvas.text(
        area.x + 2,
        area.y,
        " ◆ beacon ",
        Ink::new(role::STRONG, title_bg).bold(),
        area.right() - 2,
    );

    // The status sits on the main pane's stretch of the border, ending two
    // cells before the corner (`… ago ─╮`). The fullest version that fits wins.
    let right = area.right() - 2;
    let left = match layout.separator_x() {
        Some(x) => x + 2,
        None => title_end + 1,
    };
    let room = right.saturating_sub(left);
    let ansi = canvas.theme().mode == ColorMode::Ansi256;
    if let Some(status) = status_variants(app, ansi)
        .into_iter()
        .find(|segs| segs_width(segs) <= room)
    {
        canvas.segs_right(right, area.y, &status);
    }
}

/// The header status, fullest first: everything, then without the sync
/// detail, then the node alone.
fn status_variants(app: &App, ansi: bool) -> Vec<Vec<Seg>> {
    let ink = |fg| Ink::new(fg, role::MAIN_BG);
    let node: Seg = (format!(" {}", app.node), ink(role::TEXT));
    let mut full = vec![node.clone()];
    let mut short = vec![node.clone()];

    let peers = app.peers.len();
    let offline = app.peers.iter().filter(|p| !p.reachable).count();
    let queued: u64 = app.peers.iter().map(|p| p.queued).sum();
    let peers_text = match peers {
        0 => " · no peers".to_string(),
        1 => " · 1 peer".to_string(),
        n => format!(" · {n} peers"),
    };
    if peers == 0 {
        full.push((format!("{peers_text} "), ink(role::META)));
    } else {
        full.push((format!("{peers_text} · "), ink(role::META)));
        if offline > 0 {
            full.push(("○".to_string(), ink(role::QUIET)));
            full.push((
                format!(" {offline} offline · {queued} queued "),
                ink(role::META),
            ));
        } else if let Some(last) = app.last_sync_ms {
            full.push(("●".to_string(), ink(palette::GREEN)));
            full.push((
                format!(" synced {} ago ", sync_age(app.now_ms - last)),
                ink(role::META),
            ));
        } else {
            full.push(("○".to_string(), ink(role::QUIET)));
            full.push((" not synced yet ".to_string(), ink(role::META)));
        }
    }
    short.push((format!("{peers_text} "), ink(role::META)));
    if ansi {
        for segs in [&mut full, &mut short] {
            if let Some(last) = segs.last_mut() {
                last.0.push_str("· 256 colors ");
            }
        }
    }
    let mut node_only = vec![node];
    node_only[0].0.push(' ');
    vec![full, short, node_only]
}

/// `0.4s` under ten seconds, then `12s`, then minutes, hours, days.
fn sync_age(delta_ms: i64) -> String {
    let ms = delta_ms.max(0);
    match ms {
        0..=9_999 => format!("{}.{}s", ms / 1000, ms % 1000 / 100),
        10_000..=59_999 => format!("{}s", ms / 1000),
        _ => age_short(ms),
    }
}

#[cfg(test)]
mod tests {
    use super::sync_age;

    #[test]
    fn sync_ages() {
        assert_eq!(sync_age(400), "0.4s");
        assert_eq!(sync_age(-50), "0.0s");
        assert_eq!(sync_age(9_999), "9.9s");
        assert_eq!(sync_age(12_000), "12s");
        assert_eq!(sync_age(3 * 60_000), "3m");
    }
}
