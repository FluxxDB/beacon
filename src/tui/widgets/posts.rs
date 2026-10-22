//! The posts viewport: posts bottom-anchored above the composer, one blank
//! row apart, a centered day separator between days (and above the oldest
//! post when it is not from today), the announcement and mention bands, tag
//! badges, attachments, and the empty and server-down states.
//!
//! Columns of a post, from the pane's left edge:
//!
//! ```text
//!            ╭─ pc-beacon in #general  start of the post it answers…
//! ▍ 6:07 pm  Dang                                   announcement
//! 0 1      9 11 author  client · node               badge ends at width - 3
//!            body, wrapped to end by width - 4
//! ```
//!
//! Only a reply has the first row ([`crate::tui::reply`]): what it answers,
//! in dim ink, cut where body text ends.

use jiff::Timestamp;
use ratatui::layout::Rect;

use crate::model::{mentions_handle, validate_handle};
use crate::text::is_unsafe;
use crate::timefmt::{clock_12h, day_label};
use crate::tui::reply::{self, Resolved};
use crate::tui::state::{view_name, App, Conn, Focus, PostItem, Selection};
use crate::tui::theme::{palette, role, Ink, Rgb};
use crate::tui::view::Layout;
use crate::tui::widgets::{segs_width, Canvas, Seg};
use crate::tui::wrap::{display_width, truncate, wrap};

/// The edge rule's column (`▍` on announcements).
const EDGE: u16 = 0;
/// The clock time, right-aligned in eight cells.
const TIME: u16 = 1;
const TIME_W: u16 = 8;
/// Author, body and attachment column.
const INDENT: u16 = 11;
/// Badges end this many cells before the pane's right edge; body text one more.
const BADGE_MARGIN: u16 = 3;
const BODY_MARGIN: u16 = 4;

/// Starts a reply's line, over the author's name.
const ELBOW: &str = "╭─";
/// A reply's line when the post it names is not on this node.
const NOT_HERE: &str = "reply to a post that isn't here";

const SERVER_DOWN: &str =
    "Can't reach the beacon server. Start it with docker compose up -d. Retrying…";
const SECRET_REJECTED: &str = "The server rejected BEACON_SECRET. Check .env on this device.";

/// One row of the viewport: an optional band color across the pane (all but
/// its last cell) and text runs at pane offsets.
#[derive(Default)]
struct Line {
    band: Option<Rgb>,
    segs: Vec<(u16, Seg)>,
}

impl Line {
    fn blank() -> Self {
        Line::default()
    }
}

pub fn render(canvas: &mut Canvas, app: &App, layout: &Layout) {
    let full = layout.posts;
    if full.width <= INDENT + BODY_MARGIN || full.height == 0 {
        return;
    }

    let area = viewport(app, layout);
    if let Conn::Down { .. } = app.connection {
        let band = server_down(app, full.width);
        draw_lines(
            canvas,
            Rect {
                height: full.height - area.height,
                ..full
            },
            &band,
        );
    }
    if area.height == 0 {
        return;
    }

    let filter = active_filter(app);
    let posts = shown(app);
    // While the view's posts are on their way, say nothing rather than
    // claim the channel is empty.
    if app.loading && posts.is_empty() {
        return;
    }

    if let Some(message) = empty_message(app, posts.is_empty(), filter) {
        let lines: Vec<Line> = wrap(&message, area.width - BODY_MARGIN)
            .into_iter()
            .map(|text| {
                let offset = (area.width - display_width(&text)) / 2;
                Line {
                    band: None,
                    segs: vec![(offset, (text, Ink::new(role::META, role::MAIN_BG)))],
                }
            })
            .collect();
        let top = area.height.saturating_sub(lines.len() as u16) / 2;
        draw_lines(
            canvas,
            Rect {
                y: area.y + top,
                height: area.height - top,
                ..area
            },
            &lines,
        );
        return;
    }

    // Lay out from the newest post up, only as far as the screen needs.
    let height = usize::from(area.height);
    let wanted = height.saturating_add(app.scroll);
    let mut lines_rev: Vec<Line> = Vec::new();
    for (i, post) in posts.iter().enumerate().rev() {
        if let Some(next) = posts.get(i + 1) {
            lines_rev.push(Line::blank());
            if !same_day(post.ts_ms, next.ts_ms, app) {
                lines_rev.push(day_separator(next.ts_ms, app, area.width));
                lines_rev.push(Line::blank());
            }
        }
        lines_rev.extend(post_lines(post, app, area.width).into_iter().rev());
        // The oldest post says its day when that is not today.
        if i == 0 && lead_rows(app) > 0 {
            lines_rev.push(Line::blank());
            lines_rev.push(day_separator(post.ts_ms, app, area.width));
        }
        if lines_rev.len() >= wanted {
            break;
        }
    }
    let scroll = app.scroll.min(lines_rev.len().saturating_sub(height));
    // Bottom-anchored: the newest line sits just above the composer.
    let mut shown: Vec<Line> = lines_rev.into_iter().skip(scroll).take(height).collect();
    shown.reverse();
    let top = height - shown.len();
    draw_lines(
        canvas,
        Rect {
            y: area.y + top as u16,
            height: shown.len() as u16,
            ..area
        },
        &shown,
    );

    if scroll > 0 {
        let notice = [(
            "↓ newer posts below".to_string(),
            Ink::new(role::QUIET, role::MAIN_BG),
        )];
        let right = layout.posts.right().saturating_sub(BADGE_MARGIN);
        canvas.segs_right(right, layout.notice_y, &notice);
    }
}

fn draw_lines(canvas: &mut Canvas, area: Rect, lines: &[Line]) {
    for (line, y) in lines.iter().zip(area.top()..area.bottom()) {
        if let Some(band) = line.band {
            canvas.fill(
                Rect::new(area.x, y, area.width - 1, 1),
                Ink::new(role::TEXT, band),
            );
        }
        for (offset, (text, ink)) in &line.segs {
            canvas.text(area.x + offset, y, text, *ink, area.right());
        }
    }
}

/// The rows of the posts pane left for posts: all of them, less the
/// server-down band while the server is unreachable.
pub fn viewport(app: &App, layout: &Layout) -> Rect {
    let area = layout.posts;
    match app.connection {
        Conn::Down { .. } if area.width > INDENT + BODY_MARGIN => {
            let used = (server_down(app, area.width).len() as u16 + 1).min(area.height);
            Rect {
                y: area.y + used,
                height: area.height - used,
                ..area
            }
        }
        _ => area,
    }
}

/// The filter, if one is set and not empty.
fn active_filter(app: &App) -> Option<&str> {
    app.filter.as_deref().filter(|f| !f.is_empty())
}

/// The posts the pane shows: the view's posts that pass the filter, oldest first.
pub fn shown(app: &App) -> Vec<&PostItem> {
    app.posts.iter().filter(|p| is_shown(app, p)).collect()
}

/// Whether `post` passes the filter (all do when there is none).
pub fn is_shown(app: &App, post: &PostItem) -> bool {
    active_filter(app).is_none_or(|f| matches_filter(post, f))
}

/// Whether every post of the view is shown: no filter, or an empty one.
pub fn shows_all(app: &App) -> bool {
    active_filter(app).is_none()
}

/// Rows above the oldest shown post: its day's label and a blank row, when
/// that day is not today (later days are labelled between posts).
pub fn lead_rows(app: &App) -> usize {
    match app.posts.iter().find(|p| is_shown(app, p)) {
        Some(first) if !same_day(first.ts_ms, app.now_ms, app) => 2,
        _ => 0,
    }
}

/// Rows everything in the pane takes: the posts laid out in `spans` (from
/// [`spans`] for the same app), and the oldest one's day label.
pub fn total_rows(app: &App, spans: &[Span]) -> usize {
    spans
        .last()
        .map_or(0, |s| s.bottom + s.height + lead_rows(app))
}

/// Rows `post` takes in a pane `width` cells wide: header, body, attachment.
pub fn post_rows(post: &PostItem, app: &App, width: u16) -> usize {
    post_lines(post, app, width).len()
}

/// Rows between a post and the next newer one: a blank row, plus a day
/// separator and another blank row when the day changes.
pub fn gap_rows(older: &PostItem, newer: &PostItem, app: &App) -> usize {
    if same_day(older.ts_ms, newer.ts_ms, app) {
        1
    } else {
        3
    }
}

/// Where one shown post sits in the laid-out pane, in rows counted up from
/// the bottom (row 0 is the newest post's last row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub id: String,
    /// Rows below the post's last row.
    pub bottom: usize,
    /// The post's own rows: header, body and attachment.
    pub height: usize,
}

/// The rows each shown post takes in a pane `width` cells wide, newest
/// first, laid out as [`render`] lays them out (a blank row, and a day
/// separator when the day changes, between posts). [`total_rows`] adds the
/// oldest post's day label to the last span's top.
pub fn spans(app: &App, width: u16) -> Vec<Span> {
    let posts = shown(app);
    let mut out = Vec::with_capacity(posts.len());
    let mut row = 0usize;
    for (i, post) in posts.iter().enumerate().rev() {
        if let Some(next) = posts.get(i + 1) {
            row += gap_rows(post, next, app);
        }
        let height = post_lines(post, app, width).len();
        out.push(Span {
            id: post.id.clone(),
            bottom: row,
            height,
        });
        row += height;
    }
    out
}

/// The copy for a pane with nothing to show, if that is the case.
fn empty_message(app: &App, no_posts: bool, filter: Option<&str>) -> Option<String> {
    if app.channels.is_empty() {
        return Some("No channels yet. Press n to create one.".to_string());
    }
    if !no_posts {
        return None;
    }
    if let Some(f) = filter.filter(|_| !app.posts.is_empty()) {
        return Some(format!("No posts match /{f}. Esc clears the filter."));
    }
    if app.load_failed {
        let other = match app.selected {
            Selection::Channel(_) => "another channel",
            Selection::Announcements => "a channel",
        };
        return Some(format!(
            "Couldn't load {}. Switch to {other} and back to try again.",
            view_name(&app.selected)
        ));
    }
    Some(match &app.selected {
        Selection::Channel(name) => format!("Nothing in #{name} yet. Say hi below."),
        Selection::Announcements => format!(
            "Nothing from {} yet. What you post in any channel shows up here.",
            app.me
        ),
    })
}

/// `claim` keeps posts tagged claim; `@pc-vllm` keeps posts by pc-vllm.
pub fn matches_filter(post: &PostItem, filter: &str) -> bool {
    match filter.strip_prefix('@') {
        Some(handle) => post.author.eq_ignore_ascii_case(handle),
        None => post.tags.iter().any(|t| t.eq_ignore_ascii_case(filter)),
    }
}

fn same_day(a_ms: i64, b_ms: i64, app: &App) -> bool {
    let date = |ms: i64| {
        Timestamp::from_millisecond(ms)
            .ok()
            .map(|t| t.to_zoned(app.tz.clone()).date())
    };
    date(a_ms) == date(b_ms)
}

/// `── Today ──`, centered.
fn day_separator(ts_ms: i64, app: &App, width: u16) -> Line {
    let text = format!("── {} ──", day_label(ts_ms, app.now_ms, &app.tz));
    let offset = width.saturating_sub(display_width(&text)) / 2;
    Line {
        band: None,
        segs: vec![(offset, (text, Ink::new(role::QUIET, role::MAIN_BG)))],
    }
}

/// The band across the top while the local server is unreachable, or
/// refuses this device's secret (starting it again would not help then).
fn server_down(app: &App, width: u16) -> Vec<Line> {
    let band = role::LIFTED;
    let text_w = width.saturating_sub(BODY_MARGIN + 2);
    let copy = if app.secret_rejected {
        SECRET_REJECTED
    } else {
        SERVER_DOWN
    };
    let mut lines: Vec<Line> = wrap(copy, text_w)
        .into_iter()
        .map(|text| Line {
            band: Some(band),
            segs: vec![
                (EDGE, ("▍".to_string(), Ink::new(role::ERROR, band))),
                (2, (text, Ink::new(role::TEXT, band))),
            ],
        })
        .collect();
    lines.push(Line::blank());
    lines
}

/// A post's rows: a reply's line, the header, the wrapped body, then its
/// attachment.
fn post_lines(post: &PostItem, app: &App, width: u16) -> Vec<Line> {
    let mention = !post.announcement && mentions_handle(&post.mentions, &app.me);
    let band = (post.announcement || mention).then_some(role::LIFTED);
    let bg = band.unwrap_or(role::MAIN_BG);
    let ink = |fg| Ink::new(fg, bg);
    let selected = app.focus == Focus::Posts && app.selected_post.as_deref() == Some(&post.id);
    let edge: Option<Seg> = if post.announcement {
        Some(("▍".to_string(), ink(role::ACCENT)))
    } else if selected {
        Some(("▍".to_string(), ink(role::META)))
    } else {
        None
    };
    // Every row of the post shares its band and edge rule.
    let row = |mut segs: Vec<(u16, Seg)>| {
        if let Some(edge) = &edge {
            segs.insert(0, (EDGE, edge.clone()));
        }
        Line { band, segs }
    };

    // An id without a digit may be just a word (`re branches:`): its line
    // waits until the post is found.
    let reply = reply::parse(&post.body)
        .map(|r| {
            let target = app.reply_target(&r.id);
            (r, target)
        })
        .filter(|(r, target)| r.has_digit() || matches!(target, Resolved::Found(_)));
    let mut lines = Vec::new();
    if let Some((_, target)) = &reply {
        lines.push(row(reply_line(target, post, width, bg)));
    }
    lines.push(row(header(post, app, width, bg, selected)));

    let body_ink = ink(if post.announcement {
        role::STRONG
    } else {
        role::TEXT
    });
    let body_w = width.saturating_sub(INDENT + BODY_MARGIN);
    // Once the line above says what it answers, `re <id>:` is left out.
    let written = match &reply {
        Some((reply, Resolved::Found(_))) => reply.rest,
        _ => &post.body,
    };
    // Trailing whitespace is never shown: a final newline, or a space that
    // would land just past a full line, must not add a blank row.
    let body = clean(written);
    let body = body.trim_end();
    if !body.is_empty() {
        for text in wrap(body, body_w) {
            lines.push(row(body_segs(&text, body_ink, ink(role::STRONG))));
        }
    }

    if let Some(att) = &post.attachment {
        let size = human_size(att.size);
        let action = "s to save";
        let fixed = display_width(&size) + display_width(action) + 8;
        let name = truncate(&att.name, body_w.saturating_sub(fixed));
        let chip = format!(" ≡ {name}   {size} ");
        let chip_w = display_width(&chip);
        lines.push(row(vec![
            (INDENT, (chip, Ink::new(role::TEXT, role::LIFTED))),
            (INDENT + chip_w + 1, (action.to_string(), ink(role::QUIET))),
        ]));
    }
    lines
}

/// `╭─ {author} in #{channel}  {start of its first line}` from the body
/// column, over the reply's own name: the author in fog, the rest in ash,
/// cut where body text ends. ` in #channel` only when the post it answers
/// is in another channel; `…` while that post is on its way.
fn reply_line(target: &Resolved, post: &PostItem, width: u16, bg: Rgb) -> Vec<(u16, Seg)> {
    let quiet = Ink::new(role::QUIET, bg);
    let end = width.saturating_sub(BODY_MARGIN);
    let mut segs: Vec<(u16, Seg)> = Vec::new();
    let mut put = |x: &mut u16, text: &str, ink: Ink| {
        let text = truncate(text, end.saturating_sub(*x));
        if !text.is_empty() {
            let w = display_width(&text);
            segs.push((*x, (text, ink)));
            *x += w;
        }
    };
    let mut x = INDENT;
    put(&mut x, ELBOW, quiet);
    x += 1;
    match target {
        Resolved::Pending => put(&mut x, "…", quiet),
        Resolved::Missing => put(&mut x, NOT_HERE, quiet),
        Resolved::Found(quote) => {
            put(&mut x, &quote.author, Ink::new(role::META, bg));
            if quote.channel != post.channel {
                put(&mut x, &format!(" in #{}", quote.channel), quiet);
            }
            x += 2;
            put(&mut x, &quote.text, quiet);
        }
    }
    segs
}

/// `{time}  {author}  {client} · {node}`, with the badges right-aligned.
fn header(post: &PostItem, app: &App, width: u16, bg: Rgb, selected: bool) -> Vec<(u16, Seg)> {
    let ink = |fg| Ink::new(fg, bg);
    let mut segs: Vec<(u16, Seg)> = Vec::new();

    let time = clock_12h(post.ts_ms, &app.tz);
    let time_fg = if selected { role::TEXT } else { role::QUIET };
    let time_x = TIME + TIME_W.saturating_sub(display_width(&time));
    segs.push((time_x, (time, ink(time_fg))));

    // Badges: the announcement label, then each tag.
    let mut badges: Vec<Seg> = Vec::new();
    if post.announcement {
        badges.push((
            " announcement ".to_string(),
            Ink::new(role::STRONG, palette::GRAPHITE),
        ));
    }
    for tag in &post.tags {
        badges.push((format!(" {tag} "), Ink::new(role::tag(tag), role::LIFTED)));
    }
    let badge_right = width.saturating_sub(BADGE_MARGIN);
    let gaps = u16::try_from(badges.len().saturating_sub(1)).unwrap_or(u16::MAX);
    let badges_w = segs_width(&badges).saturating_add(gaps);
    let mut left_limit = width.saturating_sub(BODY_MARGIN);
    if !badges.is_empty() && badges_w.saturating_add(2 + INDENT) < badge_right {
        let mut x = badge_right - badges_w;
        left_limit = x - 2;
        for badge in badges {
            let w = display_width(&badge.0);
            segs.push((x, badge));
            x += w + 1;
        }
    }

    let author = truncate(&post.author, left_limit.saturating_sub(INDENT));
    let author_end = INDENT + display_width(&author);
    segs.push((INDENT, (author, ink(role::STRONG).bold())));

    let meta = match (&app.selected, post.announcement) {
        (Selection::Announcements, _) => Some(format!("#{}", post.channel)),
        (_, true) => None,
        (_, false) => Some(match &post.client {
            Some(client) => format!("{client} · {}", post.node),
            None => post.node.clone(),
        }),
    };
    if let Some(meta) = meta {
        let start = author_end + 2;
        let meta = truncate(&meta, left_limit.saturating_sub(start));
        if !meta.is_empty() {
            segs.push((start, (meta, ink(role::META))));
        }
    }
    segs
}

/// A body line's words, `@mentions` bright; spaces are left to the row's fill.
fn body_segs(text: &str, ink: Ink, mention: Ink) -> Vec<(u16, Seg)> {
    let mut segs = Vec::new();
    let mut x = INDENT;
    for (i, word) in text.split(' ').enumerate() {
        if i > 0 {
            x += 1;
        }
        if word.is_empty() {
            continue;
        }
        let handle_len = mention_len(word);
        if handle_len > 0 {
            let (handle, rest) = word.split_at(handle_len);
            segs.push((x, (handle.to_string(), mention)));
            if !rest.is_empty() {
                segs.push((x + display_width(handle), (rest.to_string(), ink)));
            }
        } else {
            segs.push((x, (word.to_string(), ink)));
        }
        x += display_width(word);
    }
    segs
}

/// Bytes of `@handle` at the start of `word` (trailing sentence punctuation
/// excluded, as when mentions are parsed), or 0 when it isn't a mention.
fn mention_len(word: &str) -> usize {
    let Some(rest) = word.strip_prefix('@') else {
        return 0;
    };
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
        .unwrap_or(rest.len());
    let handle = rest[..end].trim_end_matches('.');
    if validate_handle(handle).is_ok() {
        1 + handle.len()
    } else {
        0
    }
}

/// The body as it may reach the terminal: tabs as spaces, no other control
/// characters (an escape sequence in a post must never be drawn) and no bidi
/// overrides (see [`is_unsafe`]).
pub(crate) fn clean(body: &str) -> String {
    body.chars()
        .filter_map(|c| match c {
            '\t' => Some(' '),
            '\n' => Some('\n'),
            c if is_unsafe(c) => None,
            c => Some(c),
        })
        .collect()
}

/// `512 B`, `12 KB`, `3.4 MB`, `1.2 GB` (powers of 1024).
pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.0} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.1} GB", b / (KB * KB * KB))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(12 * 1024), "12 KB");
        assert_eq!(human_size(3_565_158), "3.4 MB");
    }

    #[test]
    fn mentions_stop_at_punctuation() {
        assert_eq!(mention_len("@Dang"), 5);
        assert_eq!(mention_len("@pc-configs,"), 11);
        assert_eq!(mention_len("@mac-juicebox."), 13);
        assert_eq!(mention_len("me@example.com"), 0);
        assert_eq!(mention_len("@"), 0);
    }

    #[test]
    fn control_characters_never_reach_the_screen() {
        assert_eq!(clean("a\tb\x1b[31mc\r\nd"), "a b[31mc\nd");
        assert_eq!(clean("x\u{202e}gpj.exe\u{2066}y\u{2069}"), "xgpj.exey");
    }
}
