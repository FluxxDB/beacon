//! What each key does.
//!
//! [`App::update`] hands every key here. `Ctrl-C` always quits. Otherwise an
//! open dialog takes the key, then the `/` filter while it is being typed,
//! then the focused pane:
//!
//! * **Composer:** printable keys type (so `q` is just a letter here);
//!   `Enter` sends, `Shift-Enter`, `Alt-Enter` or `Ctrl-J` start a new line;
//!   `Esc` leaves for the posts.
//! * **Sidebar and posts:** `j`/`k` and the arrows move (in the sidebar that
//!   opens the channel), `n` `a` `s` `d` `/` `?` `q` are commands, and any
//!   other printable key jumps to the composer and types itself.
//! * Anywhere: `Tab`/`Shift-Tab` cycle the focus, `PgUp`/`PgDn` scroll the
//!   posts, `Esc` clears the filter.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

use crate::model::{normalize_channel, BODY_MAX, TOPIC_MAX};
use crate::timefmt::{clock_12h, day_label};
use crate::tui::state::{
    possessive, App, AttachPicker, Cmd, ConfirmDialog, Dialog, Focus, FormField, NewChannelForm,
    Selection, CHANNEL_NAME_MAX, FILTER_MAX, NAME_RULE,
};
use crate::tui::widgets::posts::{self, Span};

/// What every key in the help overlay does, in its order.
/// Each description fits the dialog at 80 columns.
pub const HELP: [(&str, &str); 14] = [
    ("tab  shift-tab", "channels, posts, composer"),
    ("j k  ↓ ↑", "pick a channel or a post"),
    ("enter", "send, or write in the channel"),
    ("shift-enter", "new line (or alt-enter, ctrl-j)"),
    ("pgup pgdn", "scroll the posts"),
    ("n", "new channel"),
    ("a", "attach a file from ./exchange"),
    ("s", "save the post's file"),
    ("d", "delete the post or channel"),
    ("/", "filter by tag or @handle"),
    ("esc", "clear filter, leave composer"),
    ("?", "this help"),
    ("q", "quit (not from the composer)"),
    ("ctrl-c", "quit from anywhere"),
];

/// What a `Select a post` hint says where.
const PICK_POST: &str = "Select a post first: j/k";
const PICK_POST_FROM_SIDEBAR: &str = "Select a post first: tab to the posts, then j/k";
const STILL_SENDING: &str = "Still sending the last post…";
const BODY_FULL: &str = "A post holds up to 64 KB of text";

impl App {
    pub(crate) fn on_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        if key.kind == KeyEventKind::Release {
            return Vec::new();
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c' | 'C')) {
            return vec![Cmd::Quit];
        }
        if self.dialog.is_some() {
            return self.dialog_key(&key);
        }
        if self.filter_editing {
            self.filter_key(&key);
            return Vec::new();
        }
        match key.code {
            KeyCode::BackTab => {
                self.cycle_focus(false);
                return Vec::new();
            }
            KeyCode::Tab => {
                self.cycle_focus(!key.modifiers.contains(KeyModifiers::SHIFT));
                return Vec::new();
            }
            KeyCode::PageUp => return self.page(true),
            KeyCode::PageDown => return self.page(false),
            KeyCode::Esc => {
                self.escape();
                return Vec::new();
            }
            _ => {}
        }
        match self.focus {
            Focus::Composer => self.composer_key(&key),
            Focus::Sidebar | Focus::Posts => self.pane_key(&key),
        }
    }

    /// Pasted text goes where typed text would, all of it at once: a paste
    /// with newlines never sends.
    pub(crate) fn on_paste(&mut self, text: &str) -> Vec<Cmd> {
        match &mut self.dialog {
            Some(Dialog::NewChannel(form)) => {
                for c in text.chars() {
                    form_char(form, c);
                }
            }
            Some(_) => {}
            None if self.filter_editing => {
                for c in text.chars() {
                    self.filter_char(c);
                }
            }
            None => {
                self.focus = Focus::Composer;
                self.insert_text(text);
            }
        }
        Vec::new()
    }

    // ---------------------------------------------------------- composer

    fn composer_key(&mut self, key: &KeyEvent) -> Vec<Cmd> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.insert_text("\n");
                Vec::new()
            }
            // What most terminals send for Ctrl-J (a line feed) in raw mode.
            KeyCode::Char('j') if ctrl => {
                self.insert_text("\n");
                Vec::new()
            }
            KeyCode::Enter => self.send(),
            KeyCode::Backspace => {
                pop_grapheme(&mut self.input);
                Vec::new()
            }
            _ => {
                if let Some(c) = printable(key) {
                    self.insert_text(c.encode_utf8(&mut [0; 4]));
                }
                Vec::new()
            }
        }
    }

    /// Sends the composer's text to the channel on screen.
    fn send(&mut self) -> Vec<Cmd> {
        if self.input.trim().is_empty() {
            return Vec::new();
        }
        let Selection::Channel(channel) = &self.selected else {
            self.toast("Pick a channel to post in: tab to the channels, then j/k");
            return Vec::new();
        };
        if self.sending.is_some() {
            self.toast(STILL_SENDING);
            return Vec::new();
        }
        let channel = channel.clone();
        self.sending = Some(self.input.clone());
        vec![Cmd::SendPost {
            channel,
            body: self.input.clone(),
        }]
    }

    /// Adds `text` to the composer: line breaks as `\n`, no other control
    /// characters but tabs, and no more than a post can hold.
    fn insert_text(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        for c in text.chars() {
            if c.is_control() && c != '\n' && c != '\t' {
                continue;
            }
            if self.input.len() + c.len_utf8() > BODY_MAX {
                self.error(BODY_FULL);
                break;
            }
            self.input.push(c);
        }
    }

    // ------------------------------------------------- sidebar and posts

    fn pane_key(&mut self, key: &KeyEvent) -> Vec<Cmd> {
        match key.code {
            KeyCode::Down => return self.move_by(1),
            KeyCode::Up => return self.move_by(-1),
            KeyCode::Enter => {
                if self.focus == Focus::Sidebar {
                    self.focus = Focus::Composer;
                }
                return Vec::new();
            }
            _ => {}
        }
        match printable(key) {
            Some('j') => self.move_by(1),
            Some('k') => self.move_by(-1),
            Some('n') => {
                self.dialog = Some(Dialog::NewChannel(NewChannelForm {
                    name: String::new(),
                    topic: String::new(),
                    field: FormField::Name,
                    error: None,
                }));
                Vec::new()
            }
            Some('a') => self.open_attach(),
            Some('s') => self.save_selected(),
            Some('d') => {
                self.ask_delete();
                Vec::new()
            }
            Some('/') => {
                self.filter_editing = true;
                self.filter.get_or_insert_with(String::new);
                Vec::new()
            }
            Some('?') => {
                self.dialog = Some(Dialog::Help);
                Vec::new()
            }
            Some('q') => vec![Cmd::Quit],
            Some(c) => {
                self.focus = Focus::Composer;
                self.insert_text(c.encode_utf8(&mut [0; 4]));
                Vec::new()
            }
            None => Vec::new(),
        }
    }

    /// `j`/`↓` (+1) and `k`/`↑` (-1): the next view in the sidebar, or the
    /// next newer / older post.
    fn move_by(&mut self, delta: i32) -> Vec<Cmd> {
        match self.focus {
            Focus::Sidebar => {
                let views: Vec<Selection> = std::iter::once(Selection::Announcements)
                    .chain(
                        self.channels
                            .iter()
                            .map(|c| Selection::Channel(c.name.clone())),
                    )
                    .collect();
                let at = views.iter().position(|v| v == &self.selected).unwrap_or(0);
                let next = if delta > 0 {
                    (at + 1).min(views.len() - 1)
                } else {
                    at.saturating_sub(1)
                };
                let view = views[next].clone();
                self.select(view)
            }
            Focus::Posts => self.move_post(delta),
            Focus::Composer => Vec::new(),
        }
    }

    fn move_post(&mut self, delta: i32) -> Vec<Cmd> {
        // Newest first.
        let spans = self.post_spans();
        if spans.is_empty() {
            return Vec::new();
        }
        let Some(at) = self.selected_index(&spans) else {
            self.pick_visible_post();
            return Vec::new();
        };
        let target = if delta > 0 {
            at.checked_sub(1)
        } else {
            Some(at + 1).filter(|&i| i < spans.len())
        };
        match target {
            Some(i) => {
                self.selected_post = Some(spans[i].id.clone());
                self.reveal(&spans[i]);
                Vec::new()
            }
            // Past the oldest loaded: fetch the page before it.
            None if delta < 0 => self.load_older(),
            None => {
                self.scroll = 0;
                Vec::new()
            }
        }
    }

    /// The selected post's index in `spans`, if it is shown.
    fn selected_index(&self, spans: &[Span]) -> Option<usize> {
        let id = self.selected_post.as_deref()?;
        spans.iter().position(|s| s.id == id)
    }

    /// Every shown post's place, newest first, at the terminal's width (or a
    /// typical one before the size is known: only the order matters then).
    fn post_spans(&self) -> Vec<Span> {
        let width = self.layout().map_or(80, |l| l.posts.width);
        posts::spans(self, width)
    }

    /// Selects the newest post that is on screen, unless the selection
    /// already is a shown post.
    fn pick_visible_post(&mut self) {
        let spans = self.post_spans();
        if self.selected_index(&spans).is_some() {
            return;
        }
        let pick = spans
            .iter()
            .find(|s| s.bottom + s.height > self.scroll)
            .or(spans.first())
            .cloned();
        if let Some(span) = pick {
            self.selected_post = Some(span.id.clone());
            self.reveal(&span);
        }
    }

    /// Scrolls just enough to show all of `span` (its top, if it is taller
    /// than the pane).
    fn reveal(&mut self, span: &Span) {
        let Some(view) = self.viewport() else {
            return;
        };
        let height = usize::from(view.height);
        if span.bottom < self.scroll {
            self.scroll = span.bottom;
        } else if span.bottom + span.height > self.scroll + height {
            self.scroll = (span.bottom + span.height).saturating_sub(height);
        }
    }

    /// Asks for the page of posts before the oldest loaded, if there may be one.
    fn load_older(&mut self) -> Vec<Cmd> {
        let Selection::Channel(channel) = &self.selected else {
            return Vec::new();
        };
        if !self.has_older || self.loading_older {
            return Vec::new();
        }
        // From where the pages end, not from the oldest post on screen.
        let Some(cursor) = self.oldest_page.clone() else {
            return Vec::new();
        };
        let cmd = Cmd::LoadPosts {
            channel: channel.clone(),
            before: Some(cursor),
        };
        self.loading_older = true;
        vec![cmd]
    }

    /// `PgUp` / `PgDn`: a pane's height at a time, less two rows of overlap.
    /// Reaching the top asks for older posts.
    fn page(&mut self, up: bool) -> Vec<Cmd> {
        let (Some(view), Some(max)) = (self.viewport(), self.max_scroll()) else {
            return Vec::new();
        };
        let step = usize::from(view.height.saturating_sub(2).max(1));
        if up {
            self.scroll = (self.scroll + step).min(max);
            if self.scroll >= max {
                return self.load_older();
            }
        } else {
            self.scroll = self.scroll.saturating_sub(step);
        }
        Vec::new()
    }

    /// `Tab` forward: sidebar → posts → composer → sidebar.
    fn cycle_focus(&mut self, forward: bool) {
        self.focus = match (self.focus, forward) {
            (Focus::Sidebar, true) | (Focus::Composer, false) => Focus::Posts,
            (Focus::Posts, true) | (Focus::Sidebar, false) => Focus::Composer,
            (Focus::Composer, true) | (Focus::Posts, false) => Focus::Sidebar,
        };
        if self.focus == Focus::Posts {
            self.pick_visible_post();
        }
    }

    fn escape(&mut self) {
        if self.filter.is_some() {
            self.set_filter(None);
        } else if self.focus == Focus::Composer {
            self.focus = Focus::Posts;
            self.pick_visible_post();
        }
    }

    // ---------------------------------------------------------- commands

    fn open_attach(&mut self) -> Vec<Cmd> {
        if !matches!(self.selected, Selection::Channel(_)) {
            self.toast("Pick a channel to attach a file to: tab to the channels, then j/k");
            return Vec::new();
        }
        if self.sending.is_some() {
            self.toast(STILL_SENDING);
            return Vec::new();
        }
        self.dialog = Some(Dialog::Attach(AttachPicker {
            files: None,
            selected: 0,
            message: self.input.clone(),
        }));
        vec![Cmd::ListFiles]
    }

    fn save_selected(&mut self) -> Vec<Cmd> {
        if self.focus != Focus::Posts {
            self.toast(PICK_POST_FROM_SIDEBAR);
            return Vec::new();
        }
        let Some(id) = self.selected_shown() else {
            self.toast(PICK_POST);
            return Vec::new();
        };
        let attachment = self
            .posts
            .iter()
            .find(|p| p.id == id)
            .and_then(|p| p.attachment.clone());
        let sha = self.extras.get(&id).and_then(|e| e.sha256.clone());
        let (Some(attachment), Some(sha)) = (attachment, sha) else {
            self.toast("This post has no file to save");
            return Vec::new();
        };
        // Whether the bytes are here yet is the node's to say: they arrive
        // after the post, and nothing here would hear of it. A node without
        // them answers 404, which comes back as "not synced yet".
        self.waiting(format!("saving {}…", attachment.name));
        vec![Cmd::Save {
            post_id: id,
            sha,
            size: attachment.size,
            name: attachment.name,
        }]
    }

    /// `d`: asks before deleting the selected post, or in the sidebar the
    /// channel on screen.
    fn ask_delete(&mut self) {
        let (dialog, cmd) = match self.focus {
            Focus::Sidebar => {
                let Selection::Channel(name) = &self.selected else {
                    self.toast("Announcements can't be deleted: pick a channel");
                    return;
                };
                (
                    ConfirmDialog {
                        title: format!("Delete #{name}?"),
                        message:
                            "Its posts are hidden on every device until someone posts there again."
                                .to_string(),
                        action: "delete".to_string(),
                    },
                    Cmd::DeleteChannel(name.clone()),
                )
            }
            _ => {
                // Only a post on screen: never one the filter hides.
                let Some(id) = self.selected_shown() else {
                    self.toast(PICK_POST);
                    return;
                };
                // Named, since the dialog may cover the post it is about.
                let whose = match self.posts.iter().find(|p| p.id == id) {
                    Some(post) => {
                        let owner = if post.author.eq_ignore_ascii_case(&self.me) {
                            "Your".to_string()
                        } else {
                            possessive(&post.author)
                        };
                        format!("{owner} post from {}", self.when(post.ts_ms))
                    }
                    None => "It".to_string(),
                };
                (
                    ConfirmDialog {
                        title: "Delete this post?".to_string(),
                        message: format!("{whose} disappears on every device."),
                        action: "delete".to_string(),
                    },
                    Cmd::DeletePost(id),
                )
            }
        };
        self.dialog = Some(Dialog::Confirm(dialog));
        self.confirm = Some(cmd);
    }

    /// `6:07 pm` today, `yesterday, 6:07 pm`, or `Tue Oct 20, 6:07 pm`.
    fn when(&self, ts_ms: i64) -> String {
        let clock = clock_12h(ts_ms, &self.tz);
        match day_label(ts_ms, self.now_ms, &self.tz).as_str() {
            "Today" => clock,
            "Yesterday" => format!("yesterday, {clock}"),
            day => format!("{day}, {clock}"),
        }
    }

    // ------------------------------------------------------------ filter

    fn filter_key(&mut self, key: &KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter_editing = false;
                self.set_filter(None);
            }
            KeyCode::Enter => {
                self.filter_editing = false;
                if self.filter.as_deref().is_some_and(str::is_empty) {
                    self.set_filter(None);
                }
            }
            KeyCode::Backspace => {
                let mut filter = self.filter.clone().unwrap_or_default();
                filter.pop();
                self.set_filter(Some(filter));
            }
            _ => {
                if let Some(c) = printable(key) {
                    self.filter_char(c);
                }
            }
        }
    }

    /// A tag or handle has no spaces; a leading `/` is the one already typed.
    fn filter_char(&mut self, c: char) {
        let mut filter = self.filter.clone().unwrap_or_default();
        if c.is_whitespace() || c.is_control() || (filter.is_empty() && c == '/') {
            return;
        }
        if filter.chars().count() >= FILTER_MAX {
            return;
        }
        filter.push(c);
        self.set_filter(Some(filter));
    }

    /// A new filter shows different posts: back to the newest.
    /// The selection stays only on a post the filter shows; in the posts
    /// pane it moves to the newest one shown.
    fn set_filter(&mut self, filter: Option<String>) {
        self.filter = filter;
        self.scroll = 0;
        if self.selected_shown().is_none() {
            self.selected_post = None;
            if self.focus == Focus::Posts {
                self.pick_visible_post();
            }
        }
    }

    /// The selected post's id, if the post is on screen (shown by the
    /// filter). `s` and `d` act on nothing else.
    fn selected_shown(&self) -> Option<String> {
        let id = self.selected_post.as_deref()?;
        posts::shown(self)
            .iter()
            .any(|p| p.id == id)
            .then(|| id.to_string())
    }

    // ----------------------------------------------------------- dialogs

    fn dialog_key(&mut self, key: &KeyEvent) -> Vec<Cmd> {
        let Some(dialog) = &mut self.dialog else {
            return Vec::new();
        };
        match dialog {
            // Any key closes the help.
            Dialog::Help => {
                self.dialog = None;
                Vec::new()
            }
            Dialog::Confirm(_) => match key.code {
                KeyCode::Enter => {
                    self.dialog = None;
                    self.confirm.take().into_iter().collect()
                }
                KeyCode::Esc => {
                    self.dialog = None;
                    self.confirm = None;
                    Vec::new()
                }
                _ => Vec::new(),
            },
            Dialog::NewChannel(form) => match form_key(form, key) {
                FormOutcome::Stay => Vec::new(),
                FormOutcome::Close => {
                    self.dialog = None;
                    Vec::new()
                }
                FormOutcome::Create { name, topic } => {
                    self.dialog = None;
                    vec![Cmd::CreateChannel { name, topic }]
                }
            },
            Dialog::Attach(picker) => {
                let count = match &picker.files {
                    Some(Ok(files)) => files.len(),
                    _ => 0,
                };
                match key.code {
                    KeyCode::Esc => {
                        self.dialog = None;
                        Vec::new()
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        picker.selected = (picker.selected + 1).min(count.saturating_sub(1));
                        Vec::new()
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        picker.selected = picker.selected.saturating_sub(1);
                        Vec::new()
                    }
                    KeyCode::Enter => {
                        let file = match &picker.files {
                            Some(Ok(files)) => files.get(picker.selected).cloned(),
                            _ => None,
                        };
                        let (Some(file), Selection::Channel(channel)) = (file, &self.selected)
                        else {
                            return Vec::new();
                        };
                        let channel = channel.clone();
                        self.dialog = None;
                        self.sending = Some(self.input.clone());
                        vec![Cmd::Upload {
                            channel,
                            path: file.path,
                            body: self.input.clone(),
                        }]
                    }
                    _ => Vec::new(),
                }
            }
        }
    }
}

/// What a key did to the new channel form.
enum FormOutcome {
    Stay,
    Close,
    Create { name: String, topic: Option<String> },
}

fn form_key(form: &mut NewChannelForm, key: &KeyEvent) -> FormOutcome {
    match key.code {
        KeyCode::Esc => FormOutcome::Close,
        KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
            form.field = match form.field {
                FormField::Name => FormField::Topic,
                FormField::Topic => FormField::Name,
            };
            FormOutcome::Stay
        }
        KeyCode::Enter => match normalize_channel(&form.name) {
            Ok(name) if !form.name.is_empty() => FormOutcome::Create {
                name,
                topic: Some(form.topic.trim().to_string()).filter(|t| !t.is_empty()),
            },
            _ => {
                form.error = Some(NAME_RULE.to_string());
                form.field = FormField::Name;
                FormOutcome::Stay
            }
        },
        KeyCode::Backspace => {
            match form.field {
                FormField::Name => pop_grapheme(&mut form.name),
                FormField::Topic => pop_grapheme(&mut form.topic),
            }
            check_name(form);
            FormOutcome::Stay
        }
        _ => {
            if let Some(c) = printable(key) {
                form_char(form, c);
            }
            FormOutcome::Stay
        }
    }
}

/// Types `c` into the form's focused field. Names are lowercased as they
/// are typed (the field already shows the `#`) and stop at 32 characters.
fn form_char(form: &mut NewChannelForm, c: char) {
    if c.is_control() {
        return;
    }
    match form.field {
        FormField::Name => {
            if (c == '#' && form.name.is_empty()) || form.name.chars().count() >= CHANNEL_NAME_MAX {
                return;
            }
            form.name.push(c.to_ascii_lowercase());
        }
        FormField::Topic => {
            if form.topic.len() + c.len_utf8() <= TOPIC_MAX {
                form.topic.push(c);
            }
        }
    }
    check_name(form);
}

/// The name is checked as it is typed; an empty one is not wrong yet.
fn check_name(form: &mut NewChannelForm) {
    form.error = (!form.name.is_empty() && normalize_channel(&form.name).is_err())
        .then(|| NAME_RULE.to_string());
}

/// The character a key types, if it types one: no Ctrl or Alt chords.
fn printable(key: &KeyEvent) -> Option<char> {
    let chord = KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER;
    match key.code {
        KeyCode::Char(c) if !key.modifiers.intersects(chord) && !c.is_control() => Some(c),
        _ => None,
    }
}

/// Removes the last grapheme (a whole emoji, not a byte of it).
fn pop_grapheme(s: &mut String) {
    if let Some((at, _)) = s.grapheme_indices(true).next_back() {
        s.truncate(at);
    }
}
