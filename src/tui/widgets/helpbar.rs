//! The help row under the board: keys in mist, what they do in ash, and a
//! toast on the right when there is one.

use crate::tui::state::App;
use crate::tui::theme::{role, Ink};
use crate::tui::view::Layout;
use crate::tui::widgets::{segs_width, Canvas, Seg};
use crate::tui::wrap::{display_width, truncate};

const KEYS: [(&str, &str); 7] = [
    ("?", "help"),
    ("tab", "focus"),
    ("n", "new channel"),
    ("/", "filter"),
    ("s", "save"),
    ("d", "delete"),
    ("q", "quit"),
];
/// Leading cells, and the gap between two keys.
const PAD: &str = "  ";
const GAP: &str = "   ";
/// Shown while `/` is being typed.
pub const FILTER_HINT: &str = "a tag or @handle · enter keep · esc clear";

pub fn render(canvas: &mut Canvas, app: &App, layout: &Layout) {
    let area = layout.area;
    let y = layout.help_y;
    let (left, right) = (area.x + 1, area.right() - 1);
    let key = Ink::new(role::TEXT, role::MAIN_BG);
    let word = Ink::new(role::QUIET, role::MAIN_BG);

    // A toast takes the right end; keys that would run into it are left out.
    // While the filter is being typed, the keys for it take that place.
    let toast = if app.filter_editing {
        Some(FILTER_HINT)
    } else {
        app.toast.as_deref()
    };
    let mut keys_end = right;
    if let Some(toast) = toast.filter(|t| !t.is_empty()) {
        let end = right.saturating_sub(2);
        let room = end.saturating_sub(left + 2);
        let toast = truncate(toast, room);
        let start = end - display_width(&toast);
        canvas.text(start, y, &toast, key, end);
        keys_end = start.saturating_sub(2);
    }

    let mut x = left;
    for (i, (k, what)) in KEYS.iter().enumerate() {
        let lead = if i == 0 { PAD } else { GAP };
        let segs: Vec<Seg> = vec![(format!("{lead}{k} "), key), (what.to_string(), word)];
        if x + segs_width(&segs) > keys_end {
            break;
        }
        x = canvas.segs(x, y, &segs, keys_end);
    }
}
