//! Replies in the posts pane: a post whose body starts with `re <id>` gets
//! one dim line above its header, Discord style, naming the post it answers
//! (its author, its channel when that is another one, and the start of its
//! first line), and its body loses the `re <id>:` it no longer needs. The
//! id is looked up among the loaded posts first, then fetched from the node
//! once; misses are remembered too.

use beacon::api::{
    AttachmentJson, ChannelJson, ChannelsJson, PostJson, PostsResponse, StreamEvent,
};
use beacon::model::{parse_mentions, AuthorKind};
use beacon::tui::convert;
use beacon::tui::reply::{self, Quote};
use beacon::tui::state::{App, Cmd, Conn, Focus, Loaded, Msg, Selection};
use beacon::tui::theme::{palette, role, ColorMode, Rgb, Theme};
use beacon::tui::view::{self, Layout};
use beacon::tui::widgets::posts;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use jiff::tz::TimeZone;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;

const W: u16 = 104;
const H: u16 = 33;
/// 2026-10-21 22:00 UTC: every post below is from the same day.
const NOW: i64 = 1_792_620_000_000;
const MIN: i64 = 60_000;
const TRUECOLOR: Theme = Theme {
    mode: ColorMode::TrueColor,
};
const MISSING: &str = "╭─ reply to a post that isn't here";

// ---------------------------------------------------------------- helpers

/// A full post id ending in `tail`.
fn ulid(tail: &str) -> String {
    format!("01JAN{tail:0>21}")
}

/// A post as the node sends it, by an agent on `pc`, `minutes_ago` old.
fn api_post(tail: &str, minutes_ago: i64, channel: &str, author: &str, body: &str) -> PostJson {
    let id = ulid(tail);
    PostJson {
        v: 1,
        short_id: id[id.len() - 8..].to_string(),
        id,
        channel: channel.to_string(),
        ts: String::new(),
        ts_ms: NOW - minutes_ago * MIN,
        author: author.to_string(),
        author_kind: AuthorKind::Agent,
        client: Some("claude-code".to_string()),
        node: "pc".to_string(),
        body: body.to_string(),
        tags: Vec::new(),
        mentions: parse_mentions(body),
        announcement: false,
        attachment: None,
        arrival: 10_000 - minutes_ago,
    }
}

/// The same, written by Dang in the TUI: an announcement.
fn dang_post(tail: &str, minutes_ago: i64, channel: &str, body: &str) -> PostJson {
    let mut p = api_post(tail, minutes_ago, channel, "Dang", body);
    p.author_kind = AuthorKind::Human;
    p.client = Some("tui".to_string());
    p.announcement = true;
    p
}

fn channels() -> ChannelsJson {
    ChannelsJson {
        v: 1,
        channels: ["general", "beacon"]
            .iter()
            .map(|name| ChannelJson {
                name: name.to_string(),
                topic: None,
                unread: Some(0),
                read_to: None,
            })
            .collect(),
        announcements_unread: Some(0),
    }
}

fn page(view: Selection, posts: Vec<PostJson>) -> Msg {
    Msg::Loaded(Loaded::Posts {
        view,
        before: None,
        page: Ok(PostsResponse {
            v: 1,
            posts,
            remaining: None,
        }),
    })
}

fn general() -> Selection {
    Selection::Channel("general".to_string())
}

/// Dang's board, connected, with #general open on `posts`. Returns the
/// commands the page asked for.
fn board(posts: Vec<PostJson>) -> (App, Vec<Cmd>) {
    let mut app = App::new("pc", "Dang", TimeZone::UTC);
    app.update(Msg::Tick(NOW));
    app.update(Msg::Resize(W, H));
    app.update(Msg::Connection(Conn::Up));
    app.update(Msg::Loaded(Loaded::Channels(channels())));
    let cmds = app.update(page(general(), posts));
    (app, cmds)
}

/// The ids the commands ask the node for, as replies name them.
fn fetches(cmds: &[Cmd]) -> Vec<String> {
    cmds.iter()
        .filter_map(|c| match c {
            Cmd::LoadPost(id) => Some(id.clone()),
            _ => None,
        })
        .collect()
}

/// The node's answer about `id`: the post, none, or a failure to ask.
fn answer(id: &str, post: Result<Option<PostJson>, String>) -> Msg {
    Msg::Loaded(Loaded::Post {
        id: id.to_string(),
        post: post.map(|p| p.map(Box::new)),
    })
}

fn key(code: KeyCode) -> Msg {
    Msg::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn screen(app: &App) -> Buffer {
    let (w, h) = app.size;
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    let frame = terminal
        .draw(|f| view::render(f, app, &TRUECOLOR))
        .expect("draw");
    frame.buffer.clone()
}

fn region(buf: &Buffer, y: u16, x0: u16, x1: u16) -> String {
    (x0..x1).map(|x| buf[(x, y)].symbol()).collect()
}

fn row_text(buf: &Buffer, y: u16) -> String {
    region(buf, y, 0, buf.area.width)
}

fn screen_text(buf: &Buffer) -> String {
    (0..buf.area.height)
        .map(|y| row_text(buf, y))
        .collect::<Vec<_>>()
        .join("\n")
}

fn row_of(buf: &Buffer, needle: &str) -> u16 {
    (0..buf.area.height)
        .find(|&y| row_text(buf, y).contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} is not on screen:\n{}", screen_text(buf)))
}

/// The column `needle` starts at on row `y` (these screens have no wide glyphs).
fn column_of(buf: &Buffer, y: u16, needle: &str) -> u16 {
    let row = row_text(buf, y);
    let byte = row
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} is not on row {y}: {row}"));
    row[..byte].chars().count() as u16
}

fn posts_pane(app: &App) -> Rect {
    Layout::compute(Rect::new(0, 0, app.size.0, app.size.1), app)
        .expect("layout")
        .posts
}

/// Where the post by `author` is drawn: its header row and its body column
/// (where the author's name starts). Every post here is by an agent on the
/// claude-code client, so the header reads `author  claude-code · pc`.
fn header_of(buf: &Buffer, author: &str) -> (u16, u16) {
    let needle = format!("{author}  claude-code · pc");
    let y = row_of(buf, &needle);
    (y, column_of(buf, y, &needle))
}

/// The row above `author`'s header, from the body column to the pane's
/// right edge, with what is left of the body column checked to be blank.
fn line_above(app: &App, buf: &Buffer, author: &str) -> String {
    let (header, body_x) = header_of(buf, author);
    let pane = posts_pane(app);
    let gutter = region(buf, header - 1, pane.x + 1, body_x);
    assert!(
        gutter.trim().is_empty(),
        "left of the body column: {gutter:?}"
    );
    region(buf, header - 1, body_x, pane.right())
        .trim_end()
        .to_string()
}

/// The body rows under `author`'s header, from the body column, joined.
fn body_of(app: &App, buf: &Buffer, author: &str) -> String {
    let (header, body_x) = header_of(buf, author);
    let pane = posts_pane(app);
    let mut lines = Vec::new();
    for y in header + 1..pane.bottom() {
        let line = region(buf, y, body_x, pane.right());
        if line.trim().is_empty() {
            break;
        }
        lines.push(line.trim_end().to_string());
    }
    lines.join("\n")
}

fn rgb(c: Rgb) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

// -------------------------------------------------------------- detection

#[test]
fn a_reply_starts_with_re_and_a_post_id() {
    let replies = [
        // From the board.
        (
            "re SFRR8WM4: done. tagged v0.1",
            "SFRR8WM4",
            "done. tagged v0.1",
        ),
        (
            "re 00NHQEG3: @pc-beacon on it",
            "00NHQEG3",
            "@pc-beacon on it",
        ),
        // Any case, of the id and of `re`; ids are kept uppercase.
        ("re sfrr8wm4: lowercase", "SFRR8WM4", "lowercase"),
        ("Re SFRR8WM4: capital", "SFRR8WM4", "capital"),
        // `:` or `,` or nothing after the id, then whitespace or the end.
        ("re SFRR8WM4, comma", "SFRR8WM4", "comma"),
        ("re SFRR8WM4 space", "SFRR8WM4", "space"),
        ("re SFRR8WM4", "SFRR8WM4", ""),
        ("re SFRR8WM4:", "SFRR8WM4", ""),
        ("re SFRR8WM4:\nthe next line", "SFRR8WM4", "the next line"),
        ("re  SFRR8WM4:  extra spaces", "SFRR8WM4", "extra spaces"),
        // A full ULID, any case.
        (
            "re 01KF3V8ZQ4M2N7P9R5T6W8X0Y1: full id",
            "01KF3V8ZQ4M2N7P9R5T6W8X0Y1",
            "full id",
        ),
        (
            "re 01kf3v8zq4m2n7p9r5t6w8x0y1 lowercase full id",
            "01KF3V8ZQ4M2N7P9R5T6W8X0Y1",
            "lowercase full id",
        ),
        // Only the first id counts.
        ("re SFRR8WM4, 00NHQEG3: both", "SFRR8WM4", "00NHQEG3: both"),
    ];
    for (body, id, rest) in replies {
        let got = reply::parse(body).map(|r| (r.id, r.rest));
        assert_eq!(got, Some((id.to_string(), rest)), "{body:?}");
    }

    let not_replies = [
        // Not at the start.
        "done. re SFRR8WM4: mid-body",
        "are SFRR8WM4: x",
        // `re` with nothing, or nothing like an id, after it.
        "re",
        "re ",
        "re: SFRR8WM4 x",
        "reSFRR8WM4 x",
        "re\nSFRR8WM4: x",
        "re ally good",
        "re the spec: looks right",
        // Seven or nine characters, or 25: not a short id or a ULID.
        "re SFRR8WM: seven",
        "re SFRR8WM45: nine",
        "re 01KF3V8ZQ4M2N7P9R5T6W8X0Y: twenty-five",
        // I, L, O and U are not Crockford base32.
        "re SFRRIWM4: x",
        "re SFRRLWM4: x",
        "re SFRROWM4: x",
        "re SFRRUWM4: x",
        // Above the largest ULID (its first character is at most 7).
        "re 81KF3V8ZQ4M2N7P9R5T6W8X0Y1: x",
        // Something other than `:`/`,` and whitespace right after the id.
        "re SFRR8WM4.done",
        "re SFRR8WM4:done",
        "re SFRR8WM4,done",
        "re SFRR8WM4-x",
        "re SFRR8WM4é",
    ];
    for body in not_replies {
        assert!(reply::parse(body).is_none(), "{body:?} is not a reply");
    }
}

// --------------------------------------------------------------- the line

#[test]
fn a_reply_to_a_loaded_post_names_it_above_the_header_in_dim_ink() {
    let (app, cmds) = board(vec![
        api_post(
            "A1B2C3D4",
            30,
            "general",
            "pc-configs",
            "on it. scaffolding the rust crate now\nsecond line, not shown",
        ),
        api_post(
            "E5F6G7H8",
            20,
            "general",
            "mac-juicebox",
            "re a1b2c3d4: which toolchain are you pinning?",
        ),
    ]);
    assert_eq!(fetches(&cmds), Vec::<String>::new(), "it is loaded already");

    let buf = screen(&app);
    assert_eq!(
        line_above(&app, &buf, "mac-juicebox"),
        "╭─ pc-configs  on it. scaffolding the rust crate now"
    );
    // The body no longer starts with what the reply line now says.
    assert_eq!(
        body_of(&app, &buf, "mac-juicebox"),
        "which toolchain are you pinning?"
    );
    assert!(!screen_text(&buf).contains("a1b2c3d4"));
    // The post it answers has no reply line: a blank row above its header.
    assert_eq!(line_above(&app, &buf, "pc-configs"), "");

    // Ash for the elbow and the quote, fog for the name; never bold, never
    // the accent, on the pane's own background.
    let (header, x) = header_of(&buf, "mac-juicebox");
    let y = header - 1;
    let ink = |from: u16, len: u16| -> Vec<(Color, Color, bool)> {
        (from..from + len)
            .map(|x| {
                let cell = &buf[(x, y)];
                (cell.fg, cell.bg, cell.modifier.contains(Modifier::BOLD))
            })
            .collect()
    };
    let ash = (rgb(palette::ASH), rgb(role::MAIN_BG), false);
    let fog = (rgb(palette::FOG), rgb(role::MAIN_BG), false);
    assert_eq!(ink(x, 2), vec![ash; 2], "the elbow");
    assert_eq!(ink(x + 3, 10), vec![fog; 10], "the name");
    assert_eq!(ink(x + 15, 6), vec![ash; 6], "the quote");
    let accent = rgb(role::ACCENT);
    for cx in posts_pane(&app).x..posts_pane(&app).right() {
        assert_ne!(buf[(cx, y)].fg, accent, "({cx},{y})");
        assert_ne!(buf[(cx, y)].bg, accent, "({cx},{y})");
    }
}

#[test]
fn a_body_that_only_looks_like_a_reply_gets_no_line() {
    let (app, cmds) = board(vec![
        api_post("A1B2C3D4", 30, "general", "pc-configs", "re ally good news"),
        api_post(
            "E5F6G7H8",
            20,
            "general",
            "mac-juicebox",
            "see re SFRR8WM4: x",
        ),
    ]);
    assert_eq!(fetches(&cmds), Vec::<String>::new());
    let buf = screen(&app);
    let pane = posts_pane(&app);
    let text: String = (pane.y..pane.bottom())
        .map(|y| region(&buf, y, pane.x, pane.right()))
        .collect();
    assert!(!text.contains('╭'), "{}", screen_text(&buf));
    assert_eq!(body_of(&app, &buf, "pc-configs"), "re ally good news");
}

#[test]
fn a_reply_to_a_post_elsewhere_is_fetched_once_and_names_its_channel() {
    let (mut app, cmds) = board(vec![
        api_post(
            "E5F6G7H8",
            20,
            "general",
            "pc-beacon",
            "re 7K4TR2MF: claiming the arm64 build",
        ),
        api_post(
            "E5F6G7H9",
            10,
            "general",
            "mac-beacon",
            "re 7k4tr2mf, me too",
        ),
    ]);
    // Both name the same post: one fetch.
    assert_eq!(fetches(&cmds), ["7K4TR2MF"]);

    // While it is on its way: an ellipsis, and the bodies as written.
    let buf = screen(&app);
    assert_eq!(line_above(&app, &buf, "pc-beacon"), "╭─ …");
    assert_eq!(line_above(&app, &buf, "mac-beacon"), "╭─ …");
    assert_eq!(
        body_of(&app, &buf, "pc-beacon"),
        "re 7K4TR2MF: claiming the arm64 build"
    );

    let target = dang_post(
        "AB7K4TR2MF",
        60,
        "beacon",
        "Starting beacon today. Whoever is free: get it building on both machines.",
    );
    let cmds = app.update(answer("7K4TR2MF", Ok(Some(target))));
    assert_eq!(fetches(&cmds), Vec::<String>::new());

    let buf = screen(&app);
    // Cut with an ellipsis where the body text would end.
    assert_eq!(
        line_above(&app, &buf, "pc-beacon"),
        "╭─ Dang in #beacon  Starting beacon today. Whoever is free: ge…"
    );
    let pane = posts_pane(&app);
    let (header, _) = header_of(&buf, "pc-beacon");
    let end = column_of(&buf, header - 1, "…") + 1;
    assert_eq!(end, pane.right() - 4, "ends where body text does");
    assert_eq!(body_of(&app, &buf, "pc-beacon"), "claiming the arm64 build");
    assert_eq!(body_of(&app, &buf, "mac-beacon"), "me too");

    // Nothing asks for it again: not the clock, not keys, not a new reply.
    let mut later = Vec::new();
    for msg in [
        Msg::Tick(NOW + 1_000),
        key(KeyCode::Tab),
        key(KeyCode::Tab),
        key(KeyCode::Char('k')),
        Msg::Stream(StreamEvent::Post(api_post(
            "E5F6G7HA",
            1,
            "general",
            "pc-vllm",
            "re 7K4TR2MF: and me",
        ))),
    ] {
        later.extend(app.update(msg));
    }
    assert_eq!(fetches(&later), Vec::<String>::new());
    let buf = screen(&app);
    assert_eq!(
        line_above(&app, &buf, "pc-vllm"),
        "╭─ Dang in #beacon  Starting beacon today. Whoever is free: ge…"
    );
}

#[test]
fn a_reply_to_a_post_that_is_not_here_says_so_and_keeps_its_body() {
    let (mut app, cmds) = board(vec![api_post(
        "E5F6G7H8",
        20,
        "general",
        "pc-beacon",
        "re 7K4TR2MF: claiming the arm64 build",
    )]);
    assert_eq!(fetches(&cmds), ["7K4TR2MF"]);
    let cmds = app.update(answer("7K4TR2MF", Ok(None)));
    assert_eq!(fetches(&cmds), Vec::<String>::new());

    let buf = screen(&app);
    assert_eq!(line_above(&app, &buf, "pc-beacon"), MISSING);
    assert_eq!(
        body_of(&app, &buf, "pc-beacon"),
        "re 7K4TR2MF: claiming the arm64 build"
    );
    // The miss is remembered: a later reply to it asks nothing.
    let cmds = app.update(Msg::Stream(StreamEvent::Post(api_post(
        "E5F6G7H9",
        1,
        "general",
        "mac-beacon",
        "re 7K4TR2MF: same here",
    ))));
    assert_eq!(fetches(&cmds), Vec::<String>::new());
    assert_eq!(line_above(&app, &screen(&app), "mac-beacon"), MISSING);
}

#[test]
fn a_deleted_target_turns_the_line_into_the_missing_copy() {
    let (mut app, cmds) = board(vec![
        api_post("A1B2C3D4", 30, "general", "pc-configs", "on it"),
        api_post(
            "E5F6G7H8",
            20,
            "general",
            "mac-juicebox",
            "re A1B2C3D4: thanks",
        ),
        api_post("E5F6G7H9", 10, "general", "pc-vllm", "re 7K4TR2MF: nice"),
    ]);
    assert_eq!(fetches(&cmds), ["7K4TR2MF"]);
    app.update(answer(
        "7K4TR2MF",
        Ok(Some(api_post(
            "AB7K4TR2MF",
            90,
            "beacon",
            "nas-backup",
            "nightly done",
        ))),
    ));
    let buf = screen(&app);
    assert_eq!(
        line_above(&app, &buf, "mac-juicebox"),
        "╭─ pc-configs  on it"
    );
    assert_eq!(
        line_above(&app, &buf, "pc-vllm"),
        "╭─ nas-backup in #beacon  nightly done"
    );

    // Both go, live: no fetch to learn what is already known.
    let mut cmds = app.update(Msg::Stream(StreamEvent::PostDeleted {
        id: ulid("A1B2C3D4"),
    }));
    cmds.extend(app.update(Msg::Stream(StreamEvent::PostDeleted {
        id: ulid("AB7K4TR2MF"),
    })));
    assert_eq!(fetches(&cmds), Vec::<String>::new());
    let buf = screen(&app);
    assert_eq!(line_above(&app, &buf, "mac-juicebox"), MISSING);
    assert_eq!(body_of(&app, &buf, "mac-juicebox"), "re A1B2C3D4: thanks");
    assert_eq!(line_above(&app, &buf, "pc-vllm"), MISSING);
    assert_eq!(body_of(&app, &buf, "pc-vllm"), "re 7K4TR2MF: nice");
}

#[test]
fn a_target_in_a_deleted_channel_is_asked_for_again() {
    let (mut app, _) = board(vec![api_post(
        "E5F6G7H9",
        10,
        "general",
        "pc-vllm",
        "re 7K4TR2MF: nice",
    )]);
    app.update(answer(
        "7K4TR2MF",
        Ok(Some(api_post(
            "AB7K4TR2MF",
            90,
            "beacon",
            "nas-backup",
            "nightly done",
        ))),
    ));
    // The channel's posts are no longer shown anywhere: the node decides.
    let cmds = app.update(Msg::Stream(StreamEvent::Channel {
        channel: ChannelJson {
            name: "beacon".to_string(),
            topic: None,
            unread: None,
            read_to: None,
        },
        deleted: true,
    }));
    assert_eq!(fetches(&cmds), ["7K4TR2MF"]);
    app.update(answer("7K4TR2MF", Ok(None)));
    assert_eq!(line_above(&app, &screen(&app), "pc-vllm"), MISSING);
}

#[test]
fn a_target_that_arrives_live_resolves_without_waiting_for_the_fetch() {
    let (mut app, cmds) = board(vec![api_post(
        "E5F6G7H9",
        10,
        "general",
        "pc-vllm",
        "re 7K4TR2MF: nice",
    )]);
    assert_eq!(fetches(&cmds), ["7K4TR2MF"]);
    // It syncs in, in another channel, before the node answered.
    app.update(Msg::Stream(StreamEvent::Post(api_post(
        "AB7K4TR2MF",
        90,
        "beacon",
        "nas-backup",
        "nightly done",
    ))));
    let resolved = "╭─ nas-backup in #beacon  nightly done";
    assert_eq!(line_above(&app, &screen(&app), "pc-vllm"), resolved);
    // An answer from before it arrived does not take it back.
    app.update(answer("7K4TR2MF", Ok(None)));
    assert_eq!(line_above(&app, &screen(&app), "pc-vllm"), resolved);
}

#[test]
fn a_failed_fetch_waits_for_the_reconnect_and_is_asked_once_more() {
    let (mut app, cmds) = board(vec![
        api_post("E5F6G7H8", 20, "general", "pc-beacon", "re SFRR8WM4: done"),
        api_post("E5F6G7H9", 10, "general", "pc-vllm", "re 7K4TR2MF: nice"),
        api_post("E5F6G7HA", 5, "general", "mac-beacon", "re 00NHQEG3: ok"),
    ]);
    assert_eq!(fetches(&cmds), ["SFRR8WM4", "7K4TR2MF", "00NHQEG3"]);
    app.update(answer(
        "SFRR8WM4",
        Ok(Some(api_post(
            "ABSFRR8WM4",
            90,
            "general",
            "Dang",
            "tag v0.1",
        ))),
    ));
    app.update(answer("7K4TR2MF", Ok(None)));
    app.update(answer("00NHQEG3", Err("connection refused".to_string())));
    // Not a claim that it is gone: still on its way, as far as the line says.
    let buf = screen(&app);
    assert_eq!(line_above(&app, &buf, "mac-beacon"), "╭─ …");
    assert_eq!(body_of(&app, &buf, "mac-beacon"), "re 00NHQEG3: ok");

    // No retry loop while the node is away.
    let mut cmds = Vec::new();
    for t in 1..5 {
        cmds.extend(app.update(Msg::Tick(NOW + t * 1_000)));
    }
    cmds.extend(app.update(Msg::Connection(Conn::Down { since_ms: NOW })));
    assert_eq!(fetches(&cmds), Vec::<String>::new());

    // Back: the board is read afresh, and what was not found is asked again;
    // what was found is kept.
    let cmds = app.update(Msg::Connection(Conn::Up));
    let mut asked = fetches(&cmds);
    asked.sort();
    assert_eq!(asked, ["00NHQEG3", "7K4TR2MF"]);
}

#[test]
fn switching_views_keeps_what_was_learnt() {
    let (mut app, cmds) = board(vec![api_post(
        "E5F6G7H9",
        10,
        "general",
        "pc-vllm",
        "re 7K4TR2MF: nice",
    )]);
    assert_eq!(fetches(&cmds), ["7K4TR2MF"]);
    app.update(answer("7K4TR2MF", Ok(None)));

    app.focus = Focus::Sidebar;
    let mut cmds = app.update(key(KeyCode::Char('j')));
    cmds.extend(app.update(page(Selection::Channel("beacon".to_string()), vec![])));
    cmds.extend(app.update(key(KeyCode::Char('k'))));
    cmds.extend(app.update(page(
        general(),
        vec![api_post(
            "E5F6G7H9",
            10,
            "general",
            "pc-vllm",
            "re 7K4TR2MF: nice",
        )],
    )));
    assert_eq!(app.selected, general());
    assert_eq!(fetches(&cmds), Vec::<String>::new());
    assert_eq!(line_above(&app, &screen(&app), "pc-vllm"), MISSING);
}

// ------------------------------------------------------- views and widths

#[test]
fn works_in_announcements_on_the_band() {
    let mut app = App::new("pc", "Dang", TimeZone::UTC);
    app.update(Msg::Tick(NOW));
    app.update(Msg::Resize(W, H));
    app.update(Msg::Connection(Conn::Up));
    app.update(Msg::Loaded(Loaded::Channels(channels())));
    app.focus = Focus::Sidebar;
    app.update(key(KeyCode::Char('k')));
    assert_eq!(app.selected, Selection::Announcements);
    // Announcements holds Dang's posts only: the agent post he answers is
    // not loaded there.
    let cmds = app.update(page(
        Selection::Announcements,
        vec![dang_post("E5F6G7H8", 20, "beacon", "re 7K4TR2MF: ship it")],
    ));
    assert_eq!(fetches(&cmds), ["7K4TR2MF"]);
    app.update(answer(
        "7K4TR2MF",
        Ok(Some(api_post(
            "AB7K4TR2MF",
            60,
            "beacon",
            "mac-juicebox",
            "arm64 image builds in 2m14s",
        ))),
    ));

    let buf = screen(&app);
    let header = row_of(&buf, " announcement ");
    let pane = posts_pane(&app);
    let body_x = column_of(&buf, header, "Dang");
    let y = header - 1;
    assert_eq!(
        region(&buf, y, body_x, pane.right()).trim_end(),
        "╭─ mac-juicebox  arm64 image builds in 2m14s"
    );
    assert_eq!(
        region(&buf, header + 1, body_x, pane.right()).trim_end(),
        "ship it"
    );
    // The reply line is a row of the announcement: on its band, under its
    // edge rule; its words stay dim.
    let edge = &buf[(pane.x, y)];
    assert_eq!(
        (edge.symbol(), edge.fg, edge.bg),
        ("▍", rgb(role::ACCENT), rgb(palette::OBSIDIAN))
    );
    for x in body_x..body_x + 2 {
        assert_eq!(
            (buf[(x, y)].fg, buf[(x, y)].bg),
            (rgb(palette::ASH), rgb(palette::OBSIDIAN))
        );
    }
}

#[test]
fn resolves_against_posts_the_filter_hides() {
    let (mut app, _) = board(vec![
        api_post("A1B2C3D4", 30, "general", "pc-configs", "on it"),
        api_post(
            "E5F6G7H8",
            20,
            "general",
            "mac-juicebox",
            "re A1B2C3D4: thanks",
        ),
    ]);
    app.focus = Focus::Posts;
    let mut cmds = app.update(key(KeyCode::Char('/')));
    for c in "@mac-juicebox".chars() {
        cmds.extend(app.update(key(KeyCode::Char(c))));
    }
    cmds.extend(app.update(key(KeyCode::Enter)));
    assert_eq!(fetches(&cmds), Vec::<String>::new());
    let buf = screen(&app);
    assert!(!screen_text(&buf).contains("pc-configs  claude-code"));
    assert_eq!(
        line_above(&app, &buf, "mac-juicebox"),
        "╭─ pc-configs  on it"
    );
    assert_eq!(body_of(&app, &buf, "mac-juicebox"), "thanks");
}

#[test]
fn narrow_terminals_cut_the_line_and_wrap_the_body_without_its_prefix() {
    let (mut app, _) = board(vec![
        api_post(
            "A1B2C3D4",
            30,
            "general",
            "pc-configs",
            "on it. scaffolding the rust crate now. @mac-juicebox can you take the arm64 image",
        ),
        api_post(
            "E5F6G7H8",
            20,
            "general",
            "mac-juicebox",
            "re A1B2C3D4: sure, once main is pushed I will build the arm64 image and run the e2e test on both machines",
        ),
    ]);
    app.update(Msg::Resize(60, 24));
    let buf = screen(&app);
    let pane = posts_pane(&app);
    assert_eq!(pane.x, 1, "no sidebar below 80 columns");
    assert_eq!(
        line_above(&app, &buf, "mac-juicebox"),
        "╭─ pc-configs  on it. scaffolding the rust…"
    );
    let (header, _) = header_of(&buf, "mac-juicebox");
    assert_eq!(column_of(&buf, header - 1, "…") + 1, pane.right() - 4);
    assert_eq!(
        body_of(&app, &buf, "mac-juicebox"),
        "sure, once main is pushed I will build the\narm64 image and run the e2e test on both\nmachines"
    );
}

// ------------------------------------------------------- ids without digits

/// An 8-letter word can be a short id (about 1 in 20 has no digit), or just
/// a word: without a digit, the line shows only once the node finds the post.
#[test]
fn a_word_that_could_be_an_id_draws_nothing_unless_it_resolves() {
    let body = "re branches: merged both into main";
    let (mut app, cmds) = board(vec![api_post("E5F6G7H8", 20, "general", "pc-beacon", body)]);
    assert_eq!(fetches(&cmds), ["BRANCHES"], "asked for all the same");
    let unchanged = |app: &App, when: &str| {
        let buf = screen(app);
        assert_eq!(line_above(app, &buf, "pc-beacon"), "", "{when}");
        assert_eq!(body_of(app, &buf, "pc-beacon"), body, "{when}");
        assert_eq!(rows_of(app, "E5F6G7H8"), 2, "{when}");
    };
    unchanged(&app, "while it is asked for");

    let mut cmds = app.update(answer("BRANCHES", Ok(None)));
    unchanged(&app, "once the node has no such post");
    for t in 1..4 {
        cmds.extend(app.update(Msg::Tick(NOW + t * 1_000)));
    }
    assert_eq!(fetches(&cmds), Vec::<String>::new());
}

#[test]
fn an_id_without_a_digit_that_resolves_draws_its_line() {
    let (mut app, cmds) = board(vec![
        api_post("ABKQZXTWMN", 30, "general", "pc-configs", "on it"),
        api_post(
            "E5F6G7H8",
            20,
            "general",
            "mac-juicebox",
            "re kqzxtwmn: thanks",
        ),
        api_post("E5F6G7H9", 10, "general", "pc-vllm", "re HJKMNPQR: nice"),
    ]);
    assert_eq!(
        fetches(&cmds),
        ["HJKMNPQR"],
        "the loaded one is not asked for"
    );
    let buf = screen(&app);
    assert_eq!(
        line_above(&app, &buf, "mac-juicebox"),
        "╭─ pc-configs  on it"
    );
    assert_eq!(body_of(&app, &buf, "mac-juicebox"), "thanks");
    assert_eq!(line_above(&app, &buf, "pc-vllm"), "", "not known yet");

    app.update(answer(
        "HJKMNPQR",
        Ok(Some(api_post(
            "ABHJKMNPQR",
            90,
            "beacon",
            "nas-backup",
            "nightly done",
        ))),
    ));
    let buf = screen(&app);
    assert_eq!(
        line_above(&app, &buf, "pc-vllm"),
        "╭─ nas-backup in #beacon  nightly done"
    );
    assert_eq!(body_of(&app, &buf, "pc-vllm"), "nice");
}

/// Agents often post one huge line: the cache keeps only its start, far
/// more than any pane shows.
#[test]
fn a_huge_first_line_is_kept_short() {
    let body = "build log ".repeat(6_000);
    assert_eq!(body.chars().count(), 60_000);
    let huge = || api_post("AB7K4TR2MF", 60, "beacon", "pc-configs", &body);

    // The line is cut to the pane as it always was: 36 characters fit
    // before the ellipsis.
    let (mut app, cmds) = board(vec![api_post(
        "E5F6G7H9",
        10,
        "general",
        "pc-vllm",
        "re 7K4TR2MF: seen it",
    )]);
    assert_eq!(fetches(&cmds), ["7K4TR2MF"]);
    app.update(answer("7K4TR2MF", Ok(Some(huge()))));
    assert_eq!(
        line_above(&app, &screen(&app), "pc-vllm"),
        "╭─ pc-configs in #beacon  build log build log build log build …"
    );

    let quote = Quote::of(&convert::post_item(&huge()));
    let kept = quote.text.chars().count();
    assert!((100..=1_000).contains(&kept), "kept {kept} characters");
    assert!(body.starts_with(&quote.text), "the start of the line");
}

#[test]
fn the_quote_leaves_out_the_answered_posts_own_reply_prefix() {
    let mut file_only = api_post("A1B2C3D6", 45, "general", "pc-files", "");
    file_only.attachment = Some(AttachmentJson {
        name: "notes.txt".to_string(),
        size: 12,
        sha256: "0".repeat(64),
        available: true,
    });
    let (app, _) = board(vec![
        file_only,
        api_post(
            "A1B2C3D4",
            40,
            "general",
            "pc-configs",
            "re SFRR8WM4: done, tagged v0.1",
        ),
        api_post("A1B2C3D5", 35, "general", "pc-vllm", "re branches: merged"),
        api_post(
            "E5F6G7H7",
            30,
            "general",
            "nas-files",
            "re A1B2C3D6: got it",
        ),
        api_post(
            "E5F6G7H8",
            20,
            "general",
            "mac-juicebox",
            "re A1B2C3D4: thanks",
        ),
        api_post("E5F6G7H9", 10, "general", "nas-backup", "re A1B2C3D5: nice"),
    ]);
    let buf = screen(&app);
    // A prefix with a digit is a reply's: left out. A word without one may
    // be just a word: kept. A post with only a file quotes the file.
    assert_eq!(
        line_above(&app, &buf, "mac-juicebox"),
        "╭─ pc-configs  done, tagged v0.1"
    );
    assert_eq!(
        line_above(&app, &buf, "nas-backup"),
        "╭─ pc-vllm  re branches: merged"
    );
    assert_eq!(
        line_above(&app, &buf, "nas-files"),
        "╭─ pc-files  ≡ notes.txt"
    );
}

#[test]
fn a_failed_fetch_is_asked_again_after_switching_away_and_back() {
    let reply = || api_post("E5F6G7H9", 10, "general", "pc-vllm", "re 7K4TR2MF: nice");
    let (mut app, cmds) = board(vec![reply()]);
    assert_eq!(fetches(&cmds), ["7K4TR2MF"]);
    app.update(answer("7K4TR2MF", Err("connection refused".to_string())));

    app.focus = Focus::Sidebar;
    let mut cmds = app.update(key(KeyCode::Char('j')));
    cmds.extend(app.update(page(Selection::Channel("beacon".to_string()), vec![])));
    cmds.extend(app.update(key(KeyCode::Char('k'))));
    cmds.extend(app.update(page(general(), vec![reply()])));
    assert_eq!(app.selected, general());
    assert_eq!(fetches(&cmds), ["7K4TR2MF"], "once");
}

#[test]
fn the_selected_posts_edge_covers_its_reply_line() {
    let (mut app, _) = board(vec![
        api_post("A1B2C3D4", 30, "general", "pc-configs", "on it"),
        api_post(
            "E5F6G7H8",
            20,
            "general",
            "mac-juicebox",
            "re A1B2C3D4: thanks",
        ),
    ]);
    app.focus = Focus::Posts;
    app.update(key(KeyCode::Char('k')));
    assert_eq!(app.selected_post, Some(ulid("E5F6G7H8")));
    let buf = screen(&app);
    let pane = posts_pane(&app);
    let (header, _) = header_of(&buf, "mac-juicebox");
    for y in [header - 1, header, header + 1] {
        let cell = &buf[(pane.x, y)];
        assert_eq!((cell.symbol(), cell.fg), ("▍", rgb(role::META)), "row {y}");
    }
    assert_eq!(
        buf[(pane.x, header - 2)].symbol(),
        " ",
        "the gap is not the post's"
    );
}

#[test]
fn reply_lines_render_at_every_size() {
    let (mut app, _) = board(vec![
        api_post(
            "A1B2C3D4",
            50,
            "general",
            "pc-configs",
            "on it, a first line\nand a second",
        ),
        api_post(
            "E5F6G7H5",
            40,
            "general",
            "mac-juicebox",
            "re A1B2C3D4: found, loaded",
        ),
        api_post(
            "E5F6G7H6",
            30,
            "general",
            "pc-vllm",
            "re 7K4TR2MF: found, fetched from another channel",
        ),
        api_post(
            "E5F6G7H7",
            20,
            "general",
            "nas-backup",
            "re 00NHQEG3: missing",
        ),
        api_post(
            "E5F6G7H8",
            10,
            "general",
            "pc-beacon",
            "re SFRR8WM4: still on its way",
        ),
        api_post(
            "E5F6G7H9",
            5,
            "general",
            "mac-vllm",
            "re branches: a word, not an id",
        ),
        // A name long enough to fill the line at the narrowest widths.
        api_post(
            "E5F6G7HA",
            3,
            "general",
            "mac-juicebox-nightly-builder",
            "a long name",
        ),
        api_post(
            "E5F6G7HB",
            1,
            "general",
            "pc-vllm",
            "re E5F6G7HA: and the quote?",
        ),
    ]);
    app.update(answer("7K4TR2MF", Ok(Some(kickoff()))));
    app.update(answer("00NHQEG3", Ok(None)));
    app.focus = Focus::Posts;
    app.update(key(KeyCode::Char('k')));
    for width in 0..=80u16 {
        for height in [0u16, 1, 5, 13, 14, 15, 20, 24, 33] {
            app.update(Msg::Resize(width, height));
            let buf = screen(&app);
            for (i, cell) in buf.content().iter().enumerate() {
                let (x, y) = buf.pos_of(i);
                assert_ne!(cell.bg, Color::Reset, "{width}x{height}: bg at ({x},{y})");
                assert_ne!(cell.fg, Color::Reset, "{width}x{height}: fg at ({x},{y})");
            }
        }
    }
}

// ---------------------------------------------------- height and scrolling

/// The rows the post ending in `tail` takes in the pane.
fn rows_of(app: &App, tail: &str) -> usize {
    posts::spans(app, posts_pane(app).width)
        .into_iter()
        .find(|s| s.id == ulid(tail))
        .unwrap_or_else(|| panic!("{tail} is not shown"))
        .height
}

#[test]
fn the_reply_line_counts_in_a_posts_height() {
    let (mut app, _) = board(vec![
        api_post("A1B2C3D4", 30, "general", "pc-configs", "on it"),
        api_post(
            "E5F6G7H8",
            20,
            "general",
            "mac-juicebox",
            "re A1B2C3D4: thanks",
        ),
        api_post("E5F6G7H9", 10, "general", "pc-vllm", "re 7K4TR2MF: nice"),
    ]);
    // Header and one body row; a reply has its line on top.
    assert_eq!(rows_of(&app, "A1B2C3D4"), 2);
    assert_eq!(rows_of(&app, "E5F6G7H8"), 3);
    assert_eq!(rows_of(&app, "E5F6G7H9"), 3, "on its way");
    app.update(answer("7K4TR2MF", Ok(None)));
    assert_eq!(rows_of(&app, "E5F6G7H9"), 3, "not here");
}

/// `n` posts in #general, a minute apart, the oldest a reply to a post the
/// node will have to find.
fn long_board(n: i64) -> (App, Vec<Cmd>) {
    let mut posts = vec![api_post(
        "0000OLD1",
        n + 1,
        "general",
        "pc-oldest",
        "re 7K4TR2MF: the first post here",
    )];
    for i in 0..n {
        posts.push(api_post(
            &format!("{i:08}"),
            n - i,
            "general",
            "pc-configs",
            &format!("post number {i}"),
        ));
    }
    board(posts)
}

#[test]
fn scrolled_to_the_top_the_oldest_replys_line_shows() {
    let (mut app, _) = long_board(15);
    app.update(answer(
        "7K4TR2MF",
        Ok(Some(api_post(
            "AB7K4TR2MF",
            600,
            "beacon",
            "Dang",
            "kickoff",
        ))),
    ));
    for _ in 0..5 {
        app.update(key(KeyCode::PageUp));
    }
    let buf = screen(&app);
    let pane = posts_pane(&app);
    let top = region(&buf, pane.y, pane.x, pane.right());
    assert_eq!(
        top.trim(),
        "╭─ Dang in #beacon  kickoff",
        "{}",
        screen_text(&buf)
    );
    assert!(row_text(&buf, pane.y + 1).contains("pc-oldest  claude-code · pc"));
}

/// Fifteen posts in #general and, newest, pc-vllm's reply (`E5F6G7H9`)
/// saying `body`, with the view at the bottom.
fn reply_at_the_bottom(body: &str) -> App {
    let mut posts: Vec<PostJson> = (0..15)
        .map(|i| {
            api_post(
                &format!("{i:08}"),
                30 - i,
                "general",
                "pc-configs",
                &format!("post number {i}"),
            )
        })
        .collect();
    posts.push(api_post("E5F6G7H9", 1, "general", "pc-vllm", body));
    board(posts).0
}

/// The posts pane's rows as drawn.
fn pane_rows(app: &App) -> Vec<String> {
    let buf = screen(app);
    let pane = posts_pane(app);
    (pane.y..pane.bottom())
        .map(|y| region(&buf, y, pane.x, pane.right()))
        .collect()
}

/// `PgUp` once, which leaves the reply below the view. Returns the rows on
/// screen then.
fn page_up_past_the_reply(app: &mut App) -> Vec<String> {
    app.update(key(KeyCode::PageUp));
    let rows = pane_rows(app);
    assert!(app.scroll > 0);
    assert!(
        !rows.iter().any(|r| r.contains("pc-vllm")),
        "the reply is below the view:\n{}",
        rows.join("\n")
    );
    rows
}

/// Two body rows while its `re 7K4TR2MF: ` shows, one without it.
const LONG_REPLY: &str =
    "re 7K4TR2MF: the arm64 image builds in 2m14s and every test passes on both";

/// The post `7K4TR2MF` names, in #beacon.
fn kickoff() -> PostJson {
    api_post("AB7K4TR2MF", 600, "beacon", "Dang", "kickoff")
}

#[test]
fn a_reply_resolving_below_a_scrolled_view_does_not_move_it() {
    let mut app = reply_at_the_bottom(LONG_REPLY);
    assert_eq!(rows_of(&app, "E5F6G7H9"), 4);
    let before = page_up_past_the_reply(&mut app);
    app.update(answer("7K4TR2MF", Ok(Some(kickoff()))));
    assert_eq!(rows_of(&app, "E5F6G7H9"), 3, "one body row now");
    assert_eq!(pane_rows(&app), before, "the view stays where it was");
}

#[test]
fn a_digit_free_reply_resolving_below_a_scrolled_view_does_not_move_it() {
    let mut app = reply_at_the_bottom("re HJKMNPQR: kickoff done");
    assert_eq!(rows_of(&app, "E5F6G7H9"), 2, "no line while unknown");
    let before = page_up_past_the_reply(&mut app);
    app.update(answer(
        "HJKMNPQR",
        Ok(Some(api_post(
            "ABHJKMNPQR",
            600,
            "beacon",
            "Dang",
            "kickoff",
        ))),
    ));
    assert_eq!(rows_of(&app, "E5F6G7H9"), 3, "its line now");
    assert_eq!(pane_rows(&app), before, "the view stays where it was");
}

#[test]
fn a_target_arriving_live_below_a_scrolled_view_does_not_move_it() {
    let mut app = reply_at_the_bottom(LONG_REPLY);
    let before = page_up_past_the_reply(&mut app);
    app.update(Msg::Stream(StreamEvent::Post(kickoff())));
    assert_eq!(rows_of(&app, "E5F6G7H9"), 3, "one body row now");
    assert_eq!(pane_rows(&app), before, "the view stays where it was");
}

#[test]
fn a_target_deleted_below_a_scrolled_view_does_not_move_it() {
    let mut app = reply_at_the_bottom(LONG_REPLY);
    app.update(answer("7K4TR2MF", Ok(Some(kickoff()))));
    assert_eq!(rows_of(&app, "E5F6G7H9"), 3);
    let before = page_up_past_the_reply(&mut app);
    app.update(Msg::Stream(StreamEvent::PostDeleted {
        id: ulid("AB7K4TR2MF"),
    }));
    assert_eq!(rows_of(&app, "E5F6G7H9"), 4, "its prefix is back");
    assert_eq!(pane_rows(&app), before, "the view stays where it was");
}

#[test]
fn a_target_whose_channel_is_deleted_below_a_scrolled_view_does_not_move_it() {
    let mut app = reply_at_the_bottom(LONG_REPLY);
    app.update(answer("7K4TR2MF", Ok(Some(kickoff()))));
    let before = page_up_past_the_reply(&mut app);
    let cmds = app.update(Msg::Stream(StreamEvent::Channel {
        channel: ChannelJson {
            name: "beacon".to_string(),
            topic: None,
            unread: None,
            read_to: None,
        },
        deleted: true,
    }));
    assert_eq!(fetches(&cmds), ["7K4TR2MF"]);
    assert_eq!(rows_of(&app, "E5F6G7H9"), 4, "on its way again");
    assert_eq!(pane_rows(&app), before, "the view stays where it was");
}
