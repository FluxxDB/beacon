//! The posts viewport: posts bottom-anchored above the composer, one blank
//! row apart, a centered day separator between days, the announcement and
//! mention bands, tag badges, attachments, and the empty and server-down states.
//!
//! Columns of a post, from the pane's left edge:
//!
//! ```text
//! ▍ 6:07 pm  Dang                                   announcement
//! 0 1      9 11 author  client · node               badge ends at width - 3
//!            body, wrapped to end by width - 4
//! ```

use jiff::Timestamp;
use ratatui::layout::Rect;

use crate::model::{mentions_handle, validate_handle};
use crate::timefmt::{clock_12h, day_label};
use crate::tui::state::{App, Conn, Focus, PostItem, Selection};
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

const SERVER_DOWN: &str =
    "Can't reach the beacon server. Start it with docker compose up -d. Retrying…";

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
    let mut area = layout.posts;
    if area.width <= INDENT + BODY_MARGIN || area.height == 0 {
        return;
    }

    if let Conn::Down { .. } = app.connection {
        let band = server_down(area.width);
        let used = (band.len() as u16 + 1).min(area.height);
        draw_lines(
            canvas,
            Rect {
                height: used,
                ..area
            },
            &band,
        );
        area.y += used;
        area.height -= used;
        if area.height == 0 {
            return;
        }
    }

    let filter = app.filter.as_deref().filter(|f| !f.is_empty());
    let posts: Vec<&PostItem> = app
        .posts
        .iter()
        .filter(|p| filter.is_none_or(|f| matches_filter(p, f)))
        .collect();

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
    Some(match &app.selected {
        Selection::Channel(name) => format!("Nothing in #{name} yet. Say hi below."),
        Selection::Announcements => format!(
            "Nothing from {} yet. What you post in any channel shows up here.",
            app.me
        ),
    })
}

/// `claim` keeps posts tagged claim; `@pc-vllm` keeps posts by pc-vllm.
fn matches_filter(post: &PostItem, filter: &str) -> bool {
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

/// The band across the top while the local server is unreachable.
fn server_down(width: u16) -> Vec<Line> {
    let band = role::LIFTED;
    let text_w = width.saturating_sub(BODY_MARGIN + 2);
    let mut lines: Vec<Line> = wrap(SERVER_DOWN, text_w)
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

/// A post's rows: the header, the wrapped body, then its attachment.
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

    let mut lines = vec![row(header(post, app, width, bg, selected))];

    let body_ink = ink(if post.announcement {
        role::STRONG
    } else {
        role::TEXT
    });
    let body_w = width.saturating_sub(INDENT + BODY_MARGIN);
    // Trailing whitespace is never shown: a final newline, or a space that
    // would land just past a full line, must not add a blank row.
    let body = clean(&post.body);
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
/// characters (an escape sequence in a post must never be drawn).
fn clean(body: &str) -> String {
    body.chars()
        .filter_map(|c| match c {
            '\t' => Some(' '),
            '\n' => Some('\n'),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

/// `512 B`, `12 KB`, `3.4 MB`, `1.2 GB` (powers of 1024).
fn human_size(bytes: u64) -> String {
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
    }
}
