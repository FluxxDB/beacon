//! The CLI's human output, as pure functions over the API types.
//!
//! The layout is the approved mockup's agent screen (`tests/fixtures/mockup/cli.txt`):
//!
//! ```text
//! ◆ announcements  none new   # beacon  2 new
//!  6:31 pm  mac-juicebox  agent · codex · mac  [done]  Q8ZK7C2M
//!           arm64 image builds in 2m14s, all 41 tests pass
//!           attachment: test-report.txt (12 KB)  →  ./beacon get Q8ZK7C2M -o test-report.txt
//! ```
//!
//! Functions return text without a final newline. Nothing here reads the
//! clock or the environment except [`Clock::system`], so every format is
//! testable with a fixed time. Text that came from other agents (names,
//! bodies, tags) is cleaned of control characters, so a post cannot move the
//! reader's cursor or retitle the terminal; `--json` is the lossless form.

use std::collections::{BTreeMap, BTreeSet};

use jiff::tz::TimeZone;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::api::{
    ChannelAck, ChannelsJson, HealthJson, PeerJson, PeersJson, PostCreated, PostJson,
    PostsResponse, PresenceJson, StatusJson,
};
use crate::model::AuthorKind;
use crate::text::is_unsafe;
use crate::timefmt::{age_short, clock_12h, day_label};

/// Columns a post's body is indented by: the 8-wide time and two spaces.
pub const INDENT: usize = 10;
/// Lines (indent included) are wrapped at this many columns.
pub const WRAP_COLUMNS: usize = 100;

/// The time zone times are shown in and the moment "today" is judged by.
#[derive(Debug, Clone)]
pub struct Clock {
    pub tz: TimeZone,
    pub now_ms: i64,
}

impl Clock {
    pub fn new(tz: TimeZone, now_ms: i64) -> Clock {
        Clock { tz, now_ms }
    }

    /// The machine's zone (`$TZ`, else the system's, else UTC) and now.
    pub fn system() -> Clock {
        Clock::new(TimeZone::system(), crate::server::now_ms())
    }
}

/// How many terminal columns `s` takes.
pub fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

// ------------------------------------------------------------ small pieces

/// `12 KB`, `1.4 MB`: 1024-based, whole kilobytes, one decimal from 1 MB up.
pub fn format_size(bytes: u64) -> String {
    const KB: u128 = 1024;
    const MB: u128 = KB * KB;
    const GB: u128 = MB * KB;
    let bytes = u128::from(bytes);
    if bytes < KB {
        return format!("{bytes} B");
    }
    let kb = (bytes + KB / 2) / KB;
    if kb < 1024 {
        return format!("{kb} KB");
    }
    let tenths = |unit: u128| (bytes * 10 + unit / 2) / unit;
    let mb = tenths(MB);
    if mb < 10_240 {
        format!("{}.{} MB", mb / 10, mb % 10)
    } else {
        let gb = tenths(GB);
        format!("{}.{} GB", gb / 10, gb % 10)
    }
}

/// How long ago, for `delta_ms` milliseconds: `just now`, `12m ago`, `4h ago`.
pub fn ago(delta_ms: i64) -> String {
    match age_short(delta_ms).as_str() {
        "now" => "just now".to_string(),
        age => format!("{age} ago"),
    }
}

/// `none new` or `3 new`.
fn new_count(n: u32) -> String {
    match n {
        0 => "none new".to_string(),
        n => format!("{n} new"),
    }
}

/// A one-line field (a name, a tag) with unsafe characters replaced by U+FFFD.
pub fn clean_inline(s: &str) -> String {
    s.chars()
        .map(|c| if is_unsafe(c) { '\u{fffd}' } else { c })
        .collect()
}

/// Post text with `\n` as the only line break, tabs as four spaces and
/// unsafe characters replaced by U+FFFD.
fn clean_body(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                // CRLF is one break; a lone CR is a break too.
                if chars.peek() != Some(&'\n') {
                    out.push('\n');
                }
            }
            '\t' => out.push_str("    "),
            '\n' => out.push('\n'),
            c if is_unsafe(c) => out.push('\u{fffd}'),
            c => out.push(c),
        }
    }
    out
}

/// Pads `s` with spaces to `width` columns (it is never cut).
fn pad(s: &str, width: usize) -> String {
    let have = display_width(s);
    format!("{s}{}", " ".repeat(width.saturating_sub(have)))
}

/// `s` as one word for a shell: left alone when it is plainly safe,
/// otherwise single-quoted.
fn shell_word(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-/+=:,@%".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// What the attachment `name`, which another machine chose, is called here:
/// its last path component, whether the separator is `/` or `\`. A name that
/// is nothing, `.` or `..` is `attachment`; control characters become `_`.
/// `get -o DIR` saves under this name and suggestions name it, so a peer
/// cannot point a download at a path of its choosing.
pub fn safe_file_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or(name).trim();
    if matches!(last, "" | "." | "..") {
        return "attachment".to_string();
    }
    last.chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .collect()
}

/// `name` as the value of `-o` in a command to copy and paste: quoted if the
/// shell would split or expand it, and kept from being read as an option if
/// it starts with a dash.
fn output_word(name: &str) -> String {
    let word = shell_word(name);
    if name.starts_with('-') {
        format!("./{word}")
    } else {
        word
    }
}

// ---------------------------------------------------------------- wrapping

/// Breaks `text` into lines of at most `width` columns.
///
/// Line breaks, blank lines and a line's indentation are kept. Lines break
/// at spaces; the spaces at a break are dropped. A word wider than a line
/// starts on a line of its own and is cut there, between graphemes, never
/// inside one. A grapheme wider than `width` gets a line to itself.
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    clean_body(text)
        .split('\n')
        .flat_map(|line| wrap_line(line, width))
        .collect()
}

fn wrap_line(line: &str, width: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_width = 0;
    // Spaces seen since the last word.
    let mut gap = 0;
    // Whether `cur` is a continuation line, which starts flush left.
    let mut continued = false;
    for (is_space, token) in runs(line) {
        if is_space {
            gap = token.len();
            continue;
        }
        let word_width = display_width(token);
        let gap_width = if cur.is_empty() && continued { 0 } else { gap };
        if cur_width + gap_width + word_width <= width {
            cur.push_str(&" ".repeat(gap_width));
            cur.push_str(token);
            cur_width += gap_width + word_width;
        } else {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                continued = true;
            }
            let mut pieces = break_word(token, width);
            let (last, last_width) = pieces.pop().expect("a word has a piece");
            if !pieces.is_empty() {
                continued = true;
            }
            out.extend(pieces.into_iter().map(|(piece, _)| piece));
            cur = last;
            cur_width = last_width;
        }
        gap = 0;
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

/// Alternating runs of spaces and of anything else, as `(is_space, text)`.
fn runs(line: &str) -> Vec<(bool, &str)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut current: Option<bool> = None;
    for (i, c) in line.char_indices() {
        let space = c == ' ';
        if let Some(was) = current {
            if was != space {
                out.push((was, &line[start..i]));
                start = i;
            }
        }
        current = Some(space);
    }
    if let Some(was) = current {
        out.push((was, &line[start..]));
    }
    out
}

/// Cuts `word` into pieces of at most `width` columns, each with its width.
fn break_word(word: &str, width: usize) -> Vec<(String, usize)> {
    let mut pieces = Vec::new();
    let mut cur = String::new();
    let mut cur_width = 0;
    for cluster in clusters(word) {
        let w = display_width(cluster);
        if cur_width + w > width && !cur.is_empty() {
            pieces.push((std::mem::take(&mut cur), cur_width));
            cur_width = 0;
        }
        cur.push_str(cluster);
        cur_width += w;
    }
    if !cur.is_empty() {
        pieces.push((cur, cur_width));
    }
    pieces
}

fn is_regional_indicator(c: char) -> bool {
    ('\u{1f1e6}'..='\u{1f1ff}').contains(&c)
}

/// `s` as clusters that are shown as one: a character with the combining
/// marks, variation selectors and skin-tone modifiers after it, ZWJ
/// sequences, and flags (pairs of regional indicators).
fn clusters(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut prev: Option<char> = None;
    let mut indicators = 0;
    for (i, c) in s.char_indices() {
        let joins = prev.is_some_and(|p| {
            p == '\u{200d}'
                || c == '\u{200d}'
                || UnicodeWidthChar::width(c) == Some(0)
                || ('\u{1f3fb}'..='\u{1f3ff}').contains(&c)
                || (is_regional_indicator(c) && indicators % 2 == 1)
        });
        if !joins && i > 0 {
            out.push(&s[start..i]);
            start = i;
            indicators = 0;
        }
        if is_regional_indicator(c) {
            indicators += 1;
        }
        prev = Some(c);
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

// ------------------------------------------------------------------- posts

/// One post: its header, its body and its attachment, one string per line.
///
/// The header is `{time:>8}  {author}  {kind} · {client} · {node}
/// [{tags}]  {short_id}`; Dang's posts read `announcement · {node}`. The
/// body is indented to line up under the author and wrapped at 100 columns.
pub fn post_lines(p: &PostJson, clock: &Clock) -> Vec<String> {
    let node = clean_inline(&p.node);
    let kind = match p.author_kind {
        AuthorKind::Human => format!("announcement · {node}"),
        AuthorKind::Agent => match p.client.as_deref().map(str::trim) {
            Some(client) if !client.is_empty() => {
                format!("agent · {} · {node}", clean_inline(client))
            }
            _ => format!("agent · {node}"),
        },
    };
    let mut header = format!(
        "{:>8}  {}  {kind}",
        clock_12h(p.ts_ms, &clock.tz),
        clean_inline(&p.author)
    );
    if !p.tags.is_empty() {
        let tags: Vec<String> = p.tags.iter().map(|t| clean_inline(t)).collect();
        header.push_str(&format!("  [{}]", tags.join(" ")));
    }
    header.push_str(&format!("  {}", clean_inline(&p.short_id)));

    let indent = " ".repeat(INDENT);
    let mut lines = vec![header];
    let body = p.body.trim_end();
    if !body.is_empty() {
        for line in wrap_text(body, WRAP_COLUMNS - INDENT) {
            lines.push(if line.is_empty() {
                line
            } else {
                format!("{indent}{line}")
            });
        }
    }
    if let Some(a) = &p.attachment {
        let name = clean_inline(&safe_file_name(&a.name));
        lines.push(format!(
            "{indent}attachment: {name} ({})  →  ./beacon get {} -o {}",
            format_size(a.size),
            clean_inline(&p.short_id),
            output_word(&name)
        ));
    }
    lines
}

/// Posts one after the other, in the order given.
///
/// A line `── {day} ──` precedes the first post of each new day; "Today" is
/// taken as the starting point, so a first post from an earlier day is
/// labelled too. With `channel_headings`, a line `# {channel}` precedes each
/// run of posts from one channel (after the day line, if both apply): for
/// reads that mix channels.
pub fn posts_text(posts: &[PostJson], clock: &Clock, channel_headings: bool) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut day = "Today".to_string();
    let mut channel: Option<&str> = None;
    for p in posts {
        let label = day_label(p.ts_ms, clock.now_ms, &clock.tz);
        if label != day {
            lines.push(format!("── {label} ──"));
            day = label;
        }
        if channel_headings && channel != Some(p.channel.as_str()) {
            lines.push(format!("# {}", clean_inline(&p.channel)));
        }
        channel = Some(p.channel.as_str());
        lines.extend(post_lines(p, clock));
    }
    lines.join("\n")
}

/// The line that opens `read --unread`: how many announcements and how many
/// posts per channel are about to be shown.
pub fn unread_summary(posts: &[PostJson]) -> String {
    let announcements = posts.iter().filter(|p| p.announcement).count();
    let mut per_channel: BTreeMap<&str, usize> = BTreeMap::new();
    for p in posts {
        *per_channel.entry(&p.channel).or_default() += 1;
    }
    let mut parts = vec![format!(
        "◆ announcements  {}",
        new_count(u32::try_from(announcements).unwrap_or(u32::MAX))
    )];
    if per_channel.is_empty() {
        parts.push("no new posts".to_string());
    }
    for (channel, n) in per_channel {
        parts.push(format!("# {}  {n} new", clean_inline(channel)));
    }
    parts.join("   ")
}

/// The whole of `read --unread`: the summary, the posts (naming each
/// channel when they come from several) and, if a page was cut, how many
/// more are waiting.
pub fn unread_text(resp: &PostsResponse, clock: &Clock) -> String {
    let mut parts = vec![unread_summary(&resp.posts)];
    if !resp.posts.is_empty() {
        let channels: BTreeSet<&str> = resp.posts.iter().map(|p| p.channel.as_str()).collect();
        parts.push(posts_text(&resp.posts, clock, channels.len() > 1));
    }
    if let Some(n) = resp.remaining.filter(|n| *n > 0) {
        parts.push(format!("{n} more unread, run again"));
    }
    parts.join("\n")
}

/// `posted Q8ZK7C2M to #beacon · delivered to mac · pc-gpu unreachable, queued`.
pub fn posted_text(created: &PostCreated) -> String {
    let mut line = format!(
        "posted {} to #{}",
        clean_inline(&created.post.short_id),
        clean_inline(&created.post.channel)
    );
    if let Some(a) = &created.post.attachment {
        line.push_str(&format!(
            " · attached {} ({})",
            clean_inline(&safe_file_name(&a.name)),
            format_size(a.size)
        ));
    }
    if created.delivery.is_empty() {
        line.push_str(" · no peers yet");
    }
    for d in &created.delivery {
        let node = clean_inline(&d.node);
        if d.delivered {
            line.push_str(&format!(" · delivered to {node}"));
        } else {
            line.push_str(&format!(" · {node} unreachable, queued"));
        }
    }
    line
}

// ---------------------------------------------- channels, peers and status

/// The channel list: names, topics and, when asked for a handle, how many
/// posts are new. An announcements line comes first in that case.
pub fn channels_text(list: &ChannelsJson) -> String {
    let mut lines = Vec::new();
    if let Some(n) = list.announcements_unread {
        lines.push(format!("◆ announcements  {}", new_count(n)));
    }
    if list.channels.is_empty() {
        lines.push("no channels yet: create one with ./beacon channel new NAME".to_string());
    }
    let names: Vec<String> = list
        .channels
        .iter()
        .map(|c| clean_inline(&c.name))
        .collect();
    let topics: Vec<String> = list
        .channels
        .iter()
        .map(|c| clean_inline(c.topic.as_deref().unwrap_or("")))
        .collect();
    let name_width = names.iter().map(|n| display_width(n)).max().unwrap_or(0);
    let topic_width = topics.iter().map(|t| display_width(t)).max().unwrap_or(0);
    for ((channel, name), topic) in list.channels.iter().zip(&names).zip(&topics) {
        let mut line = format!("# {}", pad(name, name_width));
        if topic_width > 0 {
            line.push_str(&format!("  {}", pad(topic, topic_width)));
        }
        if let Some(n) = channel.unread.filter(|n| *n > 0) {
            line.push_str(&format!("  {n} new"));
        }
        lines.push(line.trim_end().to_string());
    }
    lines.join("\n")
}

/// The peers, one per line: `● mac  http://mac:7700  synced 12m ago`.
pub fn peers_text(peers: &[PeerJson], now_ms: i64) -> String {
    if peers.is_empty() {
        return "no peers yet".to_string();
    }
    let names: Vec<String> = peers
        .iter()
        .map(|p| clean_inline(p.node.as_deref().unwrap_or("?")))
        .collect();
    let urls: Vec<String> = peers.iter().map(|p| clean_inline(&p.url)).collect();
    let name_width = names.iter().map(|n| display_width(n)).max().unwrap_or(0);
    let url_width = urls.iter().map(|u| display_width(u)).max().unwrap_or(0);
    let lines: Vec<String> = peers
        .iter()
        .zip(&names)
        .zip(&urls)
        .map(|((peer, name), url)| {
            let dot = if peer.reachable { '●' } else { '○' };
            let line = format!(
                "{dot} {}  {}  {}",
                pad(name, name_width),
                pad(url, url_width),
                peer_state(peer, now_ms)
            );
            line.trim_end().to_string()
        })
        .collect();
    lines.join("\n")
}

/// [`peers_text`] for `beacon peers`, then a line for each URL that answered
/// with this device's name (see [`PeersJson::same_name`]).
pub fn peers_list_text(list: &PeersJson, now_ms: i64) -> String {
    format!(
        "{}{}",
        peers_text(&list.peers, now_ms),
        same_name_text(&list.same_name)
    )
}

/// `\n! <url> is another device with this one's name; …` per URL.
fn same_name_text(urls: &[String]) -> String {
    urls.iter()
        .map(|url| {
            format!(
                "\n! {} is another device with this one's name; give each device its own BEACON_NODE",
                clean_inline(url)
            )
        })
        .collect()
}

/// How sync with a peer is going.
fn peer_state(peer: &PeerJson, now_ms: i64) -> String {
    let synced_ago = peer
        .last_sync
        .as_deref()
        .and_then(|iso| iso.parse::<jiff::Timestamp>().ok())
        .map(|t| ago(now_ms.saturating_sub(t.as_millisecond())));
    let mut state = match (peer.reachable, synced_ago) {
        (true, Some(ago)) => format!("synced {ago}"),
        (true, None) => "connected".to_string(),
        (false, Some(ago)) => format!("unreachable, last synced {ago}"),
        (false, None) => "unreachable".to_string(),
    };
    if peer.queued > 0 {
        state.push_str(&format!(" · {} queued", peer.queued));
    }
    state
}

/// `pc · beacon 0.1.0 · 2 peers`, then the peers.
pub fn status_text(status: &StatusJson, now_ms: i64) -> String {
    let peers = match status.peers.len() {
        0 => "no peers yet".to_string(),
        1 => "1 peer".to_string(),
        n => format!("{n} peers"),
    };
    let head = format!(
        "{} · beacon {} · {peers}",
        clean_inline(&status.node),
        clean_inline(&status.version)
    );
    let same_name = same_name_text(&status.same_name);
    if status.peers.is_empty() {
        format!("{head}{same_name}")
    } else {
        format!("{head}\n{}{same_name}", peers_text(&status.peers, now_ms))
    }
}

// ------------------------------------------------- presence, joins, watching

/// Who has been seen lately, one per line:
/// `{●|○} {handle:<16} {client} · {node:<6} {age:>4}   {status}`, in the order
/// given. `●` is active, `○` idle. The columns line up across lines: the
/// handle and the `client · node` are padded to the widest.
pub fn who_text(agents: &[PresenceJson], now_ms: i64) -> String {
    if agents.is_empty() {
        return "nobody has been seen lately".to_string();
    }
    let handles: Vec<String> = agents.iter().map(|a| clean_inline(&a.handle)).collect();
    let whos: Vec<String> = agents
        .iter()
        .map(|a| {
            format!(
                "{} · {}",
                clean_inline(&a.client),
                pad(&clean_inline(&a.node), 6)
            )
        })
        .collect();
    let handle_width = handles
        .iter()
        .map(|h| display_width(h))
        .fold(16, usize::max);
    let who_width = whos.iter().map(|w| display_width(w)).max().unwrap_or(0);
    let lines: Vec<String> = agents
        .iter()
        .zip(&handles)
        .zip(&whos)
        .map(|((agent, handle), who)| {
            let dot = if agent.active { '●' } else { '○' };
            let age = age_short(now_ms.saturating_sub(agent.last_seen_ms));
            let status = clean_inline(agent.status.as_deref().unwrap_or(""));
            let line = format!(
                "{dot} {} {} {age:>4}   {status}",
                pad(handle, handle_width),
                pad(who, who_width)
            );
            line.trim_end().to_string()
        })
        .collect();
    lines.join("\n")
}

/// `joined #beacon · wait and watch will wake you for posts here`.
pub fn joined_text(channel: &str) -> String {
    format!(
        "joined #{} · wait and watch will wake you for posts here",
        clean_inline(channel)
    )
}

/// What `join --all` says, given the channels it joined just now (`added`,
/// sorted) and every channel there is (`channels`):
/// `joined #beacon #general · wait and watch will wake you for posts there`,
/// `already in all 3 channels` (`already in #beacon, the only channel`), or
/// `no channels yet`.
pub fn joined_all_text(added: &[String], channels: &[String]) -> String {
    if !added.is_empty() {
        let names: Vec<String> = added
            .iter()
            .map(|c| format!("#{}", clean_inline(c)))
            .collect();
        return format!(
            "joined {} · wait and watch will wake you for posts there",
            names.join(" ")
        );
    }
    match channels {
        [] => "no channels yet".to_string(),
        [only] => format!("already in #{}, the only channel", clean_inline(only)),
        all => format!("already in all {} channels", all.len()),
    }
}

/// What `wait` and `read --unread` add on stderr for a handle that joined no
/// channel, after `beacon: `.
pub fn no_joins_hint(handle: &str) -> String {
    format!(
        "{} hasn't joined any channel, so only @mentions and announcements reach it; \
         join your channels (./beacon join CHANNEL, or join --all)",
        clean_inline(handle)
    )
}

/// `left #beacon · mentions and announcements still wake you`.
pub fn left_text(channel: &str) -> String {
    format!(
        "left #{} · mentions and announcements still wake you",
        clean_inline(channel)
    )
}

/// The longest first line of a post `watch` shows, in characters.
pub const WATCH_BODY_MAX: usize = 200;

/// One line for `watch`: `6:31 pm #beacon mac-juicebox: first line of the
/// body`, cut to 200 characters (the last of them `…` when it was cut). A post
/// without text shows its attachment.
pub fn watch_line(p: &PostJson, clock: &Clock) -> String {
    let body = p.body.trim_start();
    let first = body
        .lines()
        .next()
        .unwrap_or("")
        .trim_end()
        .replace('\t', " ");
    let mut text = clean_inline(&first);
    if text.is_empty() {
        if let Some(a) = &p.attachment {
            text = format!(
                "attachment: {} ({})",
                clean_inline(&safe_file_name(&a.name)),
                format_size(a.size)
            );
        }
    }
    let graphemes: Vec<&str> = text.graphemes(true).collect();
    if graphemes.len() > WATCH_BODY_MAX {
        text = format!("{}…", graphemes[..WATCH_BODY_MAX - 1].concat());
    }
    format!(
        "{} #{} {}: {text}",
        clock_12h(p.ts_ms, &clock.tz),
        clean_inline(&p.channel),
        clean_inline(&p.author)
    )
}

// --------------------------------------------------------- small answers

/// `ok · pc`.
pub fn health_text(health: &HealthJson) -> String {
    format!("ok · {}", clean_inline(&health.node))
}

/// `created #beacon · topic: the agent message board`, or `#beacon already exists`.
pub fn channel_ack_text(ack: &ChannelAck) -> String {
    let name = clean_inline(&ack.channel.name);
    let mut line = if ack.created {
        format!("created #{name}")
    } else {
        format!("#{name} already exists")
    };
    if let Some(topic) = &ack.channel.topic {
        line.push_str(&format!(" · topic: {}", clean_inline(topic)));
    }
    line
}
