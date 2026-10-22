//! Dialogs over the dimmed board: the "New channel" form and confirmations.
//! A carbon box with a smoke border, centered on the posts; one primary
//! action in lime, `esc cancel` beside it.
//!
//! ```text
//! ╭──────────────────────────╮
//! │  New channel             │   title
//! │                          │
//! │  Name                    │   label
//! │   # gpu-jobs█            │   field (obsidian), caret in the focused one
//! │  lowercase letters, …    │   hint, or the error in coral
//! │  Topic                   │
//! │   long-running jobs…     │
//! │                          │
//! │   esc cancel   ↵ create  │   primary action in lime
//! ╰──────────────────────────╯
//! ```

use ratatui::layout::Rect;

use crate::timefmt::age_short;
use crate::tui::keys::HELP;
use crate::tui::state::{
    App, AttachPicker, ConfirmDialog, Dialog, FormField, NewChannelForm, NAME_RULE,
};
use crate::tui::theme::{palette, role, Ink};
use crate::tui::view::Layout;
use crate::tui::widgets::posts::human_size;
use crate::tui::widgets::{segs_width, Canvas, Seg};
use crate::tui::wrap::{display_width, truncate, truncate_start, wrap};

const WIDTH: u16 = 54;
/// Content starts this many cells in from the box's left edge.
const PAD: u16 = 3;
/// The buttons end this many cells before the fields do.
const BUTTONS_INSET: u16 = 4;
const NAME_HINT: &str = NAME_RULE;
/// Files the attach picker lists at once; more scroll.
const PICKER_ROWS: usize = 8;
/// Cells of the help's key column.
const HELP_KEYS: u16 = 15;

pub fn render(canvas: &mut Canvas, dialog: &Dialog, app: &App, layout: &Layout) {
    match dialog {
        Dialog::NewChannel(form) => new_channel(canvas, form, layout),
        Dialog::Confirm(confirm) => confirmation(canvas, confirm, layout),
        Dialog::Attach(picker) => attach(canvas, picker, app, layout),
        Dialog::Help => help(canvas, layout),
    }
}

/// The attach picker: the exchange folder, newest first, the highlighted
/// file lifted; what message goes with it; `esc cancel   ↵ attach`.
///
/// ```text
/// │  Attach a file                                     │
/// │                                                    │
/// │   test-report.txt                    12 KB    3m   │   highlighted: lifted
/// │  build.log                          3.4 MB    1h   │
/// │                                                    │
/// │  with "on my way, tagging after the PC…"           │
/// │                                                    │
/// │                        esc cancel   ↵ attach       │
/// ```
fn attach(canvas: &mut Canvas, picker: &AttachPicker, app: &App, layout: &Layout) {
    let files = match &picker.files {
        Some(Ok(files)) if !files.is_empty() => files,
        other => {
            let (text, fg) = match other {
                None => ("Reading ./exchange…".to_string(), role::META),
                Some(Err(e)) => (format!("Can't read ./exchange: {e}"), role::ERROR),
                Some(Ok(_)) => (
                    "No files in ./exchange yet. Copy one there, then press a again.".to_string(),
                    role::META,
                ),
            };
            let field_w = WIDTH
                .min(layout.main.width.saturating_sub(2))
                .saturating_sub(2 * PAD);
            let lines = wrap(&text, field_w.max(1));
            let height = 6 + lines.len() as u16;
            let Some(b) = Panel::open(canvas, layout, height, "Attach a file") else {
                return;
            };
            for (i, line) in lines.iter().enumerate() {
                b.text(canvas, 3 + i as u16, line, Ink::new(fg, role::DIALOG_BG));
            }
            b.close_only(canvas, height - 2);
            return;
        }
    };

    // Rows for files: as many as fit, up to PICKER_ROWS, scrolled to keep
    // the highlighted one in view.
    let fixed = 8u16;
    let room = usize::from(layout.area.height.saturating_sub(2 + fixed)).max(1);
    let rows = files.len().min(PICKER_ROWS).min(room);
    let first = picker
        .selected
        .saturating_sub(rows - 1)
        .min(files.len() - rows);
    let height = fixed + rows as u16;
    let Some(b) = Panel::open(canvas, layout, height, "Attach a file") else {
        return;
    };
    for (i, file) in files.iter().enumerate().skip(first).take(rows) {
        let row = 3 + (i - first) as u16;
        let highlighted = i == picker.selected;
        b.file_row(canvas, row, file, app.now_ms, highlighted);
    }
    let message = picker.message.split('\n').next().unwrap_or("").trim();
    let with = if message.is_empty() {
        "no message: the file goes on its own".to_string()
    } else {
        format!("with “{message}”")
    };
    let with_row = 4 + rows as u16;
    b.text(
        canvas,
        with_row,
        &with,
        Ink::new(role::META, role::DIALOG_BG),
    );
    b.buttons(canvas, height - 2, "attach");
}

/// Every key and what it does; any key closes it.
fn help(canvas: &mut Canvas, layout: &Layout) {
    let height = 6 + HELP.len() as u16;
    let Some(b) = Panel::open(canvas, layout, height, "Keys") else {
        return;
    };
    for (i, (keys, what)) in HELP.iter().enumerate() {
        b.pair(canvas, 3 + i as u16, keys, what);
    }
    b.close_only(canvas, height - 2);
}

fn new_channel(canvas: &mut Canvas, form: &NewChannelForm, layout: &Layout) {
    let Some(b) = Panel::open(canvas, layout, 11, "New channel") else {
        return;
    };
    let name_focused = form.field == FormField::Name;
    b.label(canvas, 3, "Name");
    b.field(
        canvas,
        4,
        Some(("#", Ink::new(role::QUIET, role::LIFTED))),
        &form.name,
        name_focused,
    );
    match &form.error {
        Some(error) => b.text(canvas, 5, error, Ink::new(role::ERROR, role::DIALOG_BG)),
        None => b.text(canvas, 5, NAME_HINT, Ink::new(role::QUIET, role::DIALOG_BG)),
    }
    b.label(canvas, 6, "Topic");
    b.field(canvas, 7, None, &form.topic, !name_focused);
    b.buttons(canvas, 9, "create");
}

fn confirmation(canvas: &mut Canvas, confirm: &ConfirmDialog, layout: &Layout) {
    let field_w = WIDTH
        .min(layout.main.width.saturating_sub(2))
        .saturating_sub(2 * PAD);
    let message = wrap(&confirm.message, field_w.max(1));
    let height = 6 + message.len() as u16;
    let Some(b) = Panel::open(canvas, layout, height, &confirm.title) else {
        return;
    };
    for (i, line) in message.iter().enumerate() {
        b.text(
            canvas,
            3 + i as u16,
            line,
            Ink::new(role::META, role::DIALOG_BG),
        );
    }
    b.buttons(canvas, height - 2, &confirm.action);
}

/// An open dialog box: where it is, and how to fill its rows.
struct Panel {
    rect: Rect,
}

impl Panel {
    /// Draws the empty box with its title, centered on the posts viewport.
    fn open(canvas: &mut Canvas, layout: &Layout, height: u16, title: &str) -> Option<Panel> {
        let main = layout.main;
        let area = layout.area;
        let width = WIDTH.min(main.width.saturating_sub(2));
        let height = height.min(area.height.saturating_sub(2));
        if width < 2 * PAD + 4 || height < 3 {
            return None;
        }
        let x = main.x + (main.width - width) / 2;
        let posts = layout.posts;
        // Centered on the posts; a taller box on the rows between the top
        // border and the help row's rule, and only past those if it must.
        let body_top = area.y + 1;
        let body_rows = layout.help_y.saturating_sub(1 + body_top);
        let y = if height <= posts.height {
            posts.y + (posts.height - height) / 2
        } else if height <= body_rows {
            body_top + (body_rows - height) / 2
        } else {
            area.y + (area.height - height) / 2
        };
        let rect = Rect::new(x, y, width, height);

        let border = Ink::new(role::DIM, role::DIALOG_BG);
        canvas.fill(rect, Ink::new(role::TEXT, role::DIALOG_BG));
        let (left, right) = (rect.x, rect.right() - 1);
        let (top, bottom) = (rect.y, rect.bottom() - 1);
        canvas.hline(left, top, width, "─", border);
        canvas.hline(left, bottom, width, "─", border);
        for y in top + 1..bottom {
            canvas.text(left, y, "│", border, right);
            canvas.text(right, y, "│", border, right + 1);
        }
        canvas.text(left, top, "╭", border, right);
        canvas.text(right, top, "╮", border, right + 1);
        canvas.text(left, bottom, "╰", border, right);
        canvas.text(right, bottom, "╯", border, right + 1);

        let b = Panel { rect };
        b.text(
            canvas,
            1,
            title,
            Ink::new(role::STRONG, role::DIALOG_BG).bold(),
        );
        Some(b)
    }

    fn field_width(&self) -> u16 {
        self.rect.width - 2 * PAD
    }

    fn content_x(&self) -> u16 {
        self.rect.x + PAD
    }

    /// Whether row `row` (from the top border) is inside the box.
    fn row(&self, row: u16) -> Option<u16> {
        (row + 1 < self.rect.height).then_some(self.rect.y + row)
    }

    fn text(&self, canvas: &mut Canvas, row: u16, text: &str, ink: Ink) {
        let Some(y) = self.row(row) else { return };
        let limit = self.content_x() + self.field_width();
        let text = truncate(text, self.field_width());
        canvas.text(self.content_x(), y, &text, ink, limit);
    }

    fn label(&self, canvas: &mut Canvas, row: u16, text: &str) {
        self.text(canvas, row, text, Ink::new(role::META, role::DIALOG_BG));
    }

    /// An input field: lifted, an optional prefix (`#`), the value, and the
    /// lime caret after it when focused. A focused value too long to fit
    /// shows its end; an unfocused one its start.
    fn field(
        &self,
        canvas: &mut Canvas,
        row: u16,
        prefix: Option<(&str, Ink)>,
        value: &str,
        focused: bool,
    ) {
        let Some(y) = self.row(row) else { return };
        let x0 = self.content_x();
        let end = x0 + self.field_width();
        canvas.fill(
            Rect::new(x0, y, self.field_width(), 1),
            Ink::new(role::TEXT, role::LIFTED),
        );
        let mut x = x0 + 1;
        if let Some((prefix, ink)) = prefix {
            x = canvas.text(x, y, prefix, ink, end) + 1;
        }
        // Room for the value, the caret, and one cell of padding.
        let room = end.saturating_sub(x + 2);
        let (shown, fg) = if focused {
            (truncate_start(value, room), role::TEXT)
        } else {
            (truncate(value, room), role::META)
        };
        let after = canvas.text(x, y, &shown, Ink::new(fg, role::LIFTED), end);
        if focused {
            canvas.text(after, y, " ", Ink::new(palette::VOID, role::ACCENT), end);
        }
    }

    /// `esc cancel   ↵ {action}`, ending a little short of the fields' edge.
    fn buttons(&self, canvas: &mut Canvas, row: u16, action: &str) {
        let Some(y) = self.row(row) else { return };
        let segs: Vec<Seg> = vec![
            (
                " esc cancel ".to_string(),
                Ink::new(role::META, role::DIALOG_BG),
            ),
            (" ".to_string(), Ink::new(role::TEXT, role::DIALOG_BG)),
            (
                format!(" ↵ {action} "),
                Ink::new(palette::VOID, role::ACCENT),
            ),
        ];
        let right = self.content_x() + self.field_width() - BUTTONS_INSET;
        if segs_width(&segs) <= right.saturating_sub(self.content_x()) {
            canvas.segs_right(right, y, &segs);
        }
    }

    /// `esc close` where the buttons go, for a dialog with nothing to do.
    fn close_only(&self, canvas: &mut Canvas, row: u16) {
        let Some(y) = self.row(row) else { return };
        let segs: Vec<Seg> = vec![(
            " esc close ".to_string(),
            Ink::new(role::META, role::DIALOG_BG),
        )];
        let right = self.content_x() + self.field_width() - BUTTONS_INSET;
        if segs_width(&segs) <= right.saturating_sub(self.content_x()) {
            canvas.segs_right(right, y, &segs);
        }
    }

    /// One line of the help: the keys in mist, what they do in fog.
    fn pair(&self, canvas: &mut Canvas, row: u16, keys: &str, what: &str) {
        let Some(y) = self.row(row) else { return };
        let x = self.content_x();
        let end = x + self.field_width();
        let key_w = HELP_KEYS.min(self.field_width() / 2);
        canvas.text(
            x,
            y,
            &truncate(keys, key_w.saturating_sub(1)),
            Ink::new(role::TEXT, role::DIALOG_BG),
            end,
        );
        let what = truncate(what, self.field_width().saturating_sub(key_w));
        canvas.text(
            x + key_w,
            y,
            &what,
            Ink::new(role::META, role::DIALOG_BG),
            end,
        );
    }

    /// A file in the picker: name, size and age; lifted when highlighted.
    fn file_row(
        &self,
        canvas: &mut Canvas,
        row: u16,
        file: &crate::tui::state::ExchangeFile,
        now_ms: i64,
        highlighted: bool,
    ) {
        let Some(y) = self.row(row) else { return };
        let x0 = self.content_x();
        let width = self.field_width();
        let end = x0 + width;
        let bg = if highlighted {
            role::LIFTED
        } else {
            role::DIALOG_BG
        };
        if highlighted {
            canvas.fill(Rect::new(x0, y, width, 1), Ink::new(role::TEXT, bg));
        }
        let size = human_size(file.size);
        let age = age_short(now_ms.saturating_sub(file.modified_ms));
        let right = format!("{size:>7}  {age:>4} ");
        let right_w = display_width(&right);
        let name_room = width.saturating_sub(right_w + 3);
        let name_fg = if highlighted {
            role::STRONG
        } else {
            role::META
        };
        canvas.text(
            x0 + 1,
            y,
            &truncate(&file.name, name_room),
            Ink::new(name_fg, bg),
            end,
        );
        canvas.text(
            end.saturating_sub(right_w),
            y,
            &right,
            Ink::new(role::QUIET, bg),
            end,
        );
    }
}
