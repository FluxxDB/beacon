//! The read/unread line (Dang, 2026-10-28): a thin blue `─` run in the
//! blank row between the last post Dang had read and the first one he
//! hasn't, from the clock column to where badges end. Posts don't move for
//! it. Dang's own posts count as read, a late-synced old post doesn't pull
//! it up, it follows the filter and deletions, and it never shows in
//! Announcements. Reaching the bottom leaves it; leaving the channel or
//! posting there takes it away.

use beacon::api::{
    ChannelJson, ChannelsJson, Delivery, PostCreated, PostJson, PostsResponse, StreamEvent,
};
use beacon::model::{parse_mentions, AuthorKind};
use beacon::tui::state::{App, Cmd, Conn, Focus, Loaded, Msg, Selection};
use beacon::tui::theme::{palette, role, ColorMode, Rgb, Theme};
use beacon::tui::view::{self, Layout};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use jiff::tz::TimeZone;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::Terminal;

/// 2026-10-21 22:00 UTC: every post below is from the same day.
const NOW: i64 = 1_792_620_000_000;
const MIN: i64 = 60_000;
const TRUECOLOR: Theme = Theme {
    mode: ColorMode::TrueColor,
};

/// Tall enough that seven two-row posts fit: the view is at the bottom.
const SIZE: (u16, u16) = (104, 33);

// ---------------------------------------------------------------- helpers

fn ulid(n: u32) -> String {
    format!("01JAN{n:021}")
}

/// A post as the node sends it; `n` gives its id, order and arrival (all
/// today).
fn api_post(n: u32, author: &str, body: &str) -> PostJson {
    let id = ulid(n);
    PostJson {
        v: 1,
        short_id: id[id.len() - 8..].to_string(),
        id,
        channel: "general".to_string(),
        ts: String::new(),
        ts_ms: NOW - (300 - i64::from(n)) * MIN,
        author: author.to_string(),
        author_kind: AuthorKind::Agent,
        client: Some("codex".to_string()),
        node: "mac".to_string(),
        body: body.to_string(),
        tags: Vec::new(),
        mentions: parse_mentions(body),
        announcement: false,
        attachment: None,
        arrival: i64::from(n),
    }
}

/// Post `n` by an agent, its one body row saying which it is.
fn post(n: u32) -> PostJson {
    api_post(n, &format!("pc-agent{n}"), &body(n))
}

/// Dang's post `n`.
fn dangs(n: u32) -> PostJson {
    let mut p = api_post(n, "Dang", &body(n));
    p.author_kind = AuthorKind::Human;
    p.announcement = true;
    p
}

fn body(n: u32) -> String {
    format!("the body of post {n}")
}

fn posts(range: std::ops::RangeInclusive<u32>) -> Vec<PostJson> {
    range.map(post).collect()
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

fn channels(read_to: i64, unread: u32) -> Msg {
    let channel = |name: &str, read_to, unread| ChannelJson {
        name: name.to_string(),
        topic: None,
        unread: Some(unread),
        read_to: Some(read_to),
    };
    Msg::Loaded(Loaded::Channels(ChannelsJson {
        v: 1,
        channels: vec![channel("general", read_to, unread), channel("random", 0, 0)],
        announcements_unread: Some(0),
    }))
}

/// Dang's board at [`SIZE`], #general open on `posts` with his read point
/// at `read_to` and `unread` posts new (#random below it), the composer
/// focused.
fn board(read_to: i64, unread: u32, posts: Vec<PostJson>) -> App {
    let mut app = App::new("pc", "Dang", TimeZone::UTC);
    app.update(Msg::Tick(NOW));
    app.update(Msg::Resize(SIZE.0, SIZE.1));
    app.update(Msg::Connection(Conn::Up));
    app.update(channels(read_to, unread));
    app.update(page(general(), posts));
    assert_eq!(app.focus, Focus::Composer);
    assert_eq!(app.scroll, 0);
    app
}

fn key(code: KeyCode) -> Msg {
    Msg::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ch(c: char) -> Msg {
    key(KeyCode::Char(c))
}

fn screen(app: &App) -> Buffer {
    let (w, h) = app.size;
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    let frame = terminal
        .draw(|f| view::render(f, app, &TRUECOLOR))
        .expect("draw");
    frame.buffer.clone()
}

fn row_text(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
}

fn posts_pane(app: &App) -> Rect {
    Layout::compute(Rect::new(0, 0, app.size.0, app.size.1), app)
        .expect("layout")
        .posts
}

fn rgb(c: Rgb) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

/// Rows of the posts pane whose clock column holds an azure `─`.
fn line_rows(buf: &Buffer, app: &App) -> Vec<u16> {
    let pane = posts_pane(app);
    (pane.top()..pane.bottom())
        .filter(|&y| {
            let cell = &buf[(pane.x + 1, y)];
            cell.symbol() == "─" && cell.fg == rgb(palette::AZURE)
        })
        .collect()
}

/// Post `n`'s header row: the row above its body.
fn header_row(buf: &Buffer, app: &App, n: u32) -> u16 {
    let pane = posts_pane(app);
    let needle = body(n);
    (pane.top()..pane.bottom())
        .find(|&y| row_text(buf, y).contains(&needle))
        .unwrap_or_else(|| panic!("post {n} is not on screen"))
        - 1
}

/// The screen has exactly one line, directly above post `n`'s header, and
/// it runs from the clock column to where badges end, azure on the pane.
fn assert_line_above(app: &App, n: u32) -> u16 {
    let buf = screen(app);
    let y = header_row(&buf, app, n) - 1;
    assert_eq!(line_rows(&buf, app), [y], "one line, above post {n}");
    let pane = posts_pane(app);
    for col in 1..=pane.width - 4 {
        let cell = &buf[(pane.x + col, y)];
        assert_eq!(
            (cell.symbol(), cell.fg, cell.bg),
            ("─", rgb(palette::AZURE), rgb(role::MAIN_BG)),
            "column {col}"
        );
    }
    for col in [0, pane.width - 3, pane.width - 2, pane.width - 1] {
        assert_eq!(buf[(pane.x + col, y)].symbol(), " ", "column {col}");
    }
    y
}

fn assert_no_line(app: &App, label: &str) {
    let buf = screen(app);
    assert!(line_rows(&buf, app).is_empty(), "{label}");
}

// ------------------------------------------------------------------ tests

#[test]
fn the_line_sits_between_the_last_read_and_the_first_new_post() {
    let app = board(4, 2, posts(1..=6));
    let y = assert_line_above(&app, 5);
    let buf = screen(&app);
    // Post 4's header and one body row, then the line in the gap row.
    assert_eq!(header_row(&buf, &app, 4) + 2, y);

    // Without a line, post 5 is on the same row: nothing moved for it.
    let read = board(6, 0, posts(1..=6));
    assert_eq!(header_row(&screen(&read), &read, 5), y + 1);
}

#[test]
fn nothing_new_draws_no_line() {
    let app = board(6, 0, posts(1..=6));
    assert_no_line(&app, "all read");
}

#[test]
fn a_channel_never_read_draws_no_line() {
    let app = board(0, 6, posts(1..=6));
    assert_no_line(&app, "no cursor");
}

#[test]
fn dangs_own_posts_are_never_below_the_line_start() {
    let mut page = posts(1..=6);
    page[4] = dangs(5);
    let app = board(4, 1, page);
    assert_line_above(&app, 6);
}

#[test]
fn a_late_synced_old_post_does_not_pull_the_line_up() {
    let mut late = post(7);
    late.ts_ms = post(2).ts_ms + 30_000;
    let mut page = posts(1..=6);
    page.push(late);
    let app = board(5, 2, page);
    let y = assert_line_above(&app, 6);
    let buf = screen(&app);
    assert_eq!(header_row(&buf, &app, 5) + 2, y, "directly below post 5");
    // Post 7 sorts up among the read posts, with no line above it.
    let late_y = header_row(&buf, &app, 7);
    assert!(late_y < header_row(&buf, &app, 3));
    assert!(!line_rows(&buf, &app).contains(&(late_y - 1)));
}

#[test]
fn the_line_follows_the_filter() {
    let mut page = posts(1..=6);
    for n in [2, 3, 6] {
        page[n - 1].tags.push("x".to_string());
    }
    for n in [5, 6] {
        page[n - 1].tags.push("y".to_string());
    }
    let mut app = board(4, 2, page);
    let filter = |app: &mut App, text: &str| {
        app.update(ch('/'));
        for c in text.chars() {
            app.update(ch(c));
        }
        app.update(key(KeyCode::Enter));
        assert_eq!(app.filter.as_deref(), Some(text));
    };
    app.update(key(KeyCode::Tab));
    assert_eq!(app.focus, Focus::Posts);

    // 2, 3 and 6 shown: read up to 3, the line above 6.
    filter(&mut app, "x");
    assert_line_above(&app, 6);

    // 5 and 6 shown, neither read: no line.
    app.update(key(KeyCode::Esc));
    assert_eq!(app.filter, None);
    filter(&mut app, "y");
    assert_no_line(&app, "nothing shown is read");
}

#[test]
fn deleting_the_post_next_to_the_line_moves_it() {
    let mut app = board(4, 2, posts(1..=6));
    assert_line_above(&app, 5);

    app.update(Msg::Stream(StreamEvent::PostDeleted { id: ulid(4) }));
    let y = assert_line_above(&app, 5);
    let buf = screen(&app);
    assert_eq!(header_row(&buf, &app, 3) + 2, y, "directly below post 3");

    app.update(Msg::Stream(StreamEvent::PostDeleted { id: ulid(5) }));
    assert_line_above(&app, 6);
}

#[test]
fn announcements_never_draw_a_line() {
    let mut app = board(4, 2, posts(1..=6));
    app.update(key(KeyCode::Tab));
    assert_eq!(app.update(key(KeyCode::Up)), [Cmd::LoadAnnouncements]);
    assert_eq!(app.selected, Selection::Announcements);
    let human = |n: u32| {
        let mut p = api_post(n, "Sam", &body(n));
        p.author_kind = AuthorKind::Human;
        p.announcement = true;
        p
    };
    app.update(page(Selection::Announcements, (1..=6).map(human).collect()));
    assert_eq!(app.line_point, None);
    assert_no_line(&app, "announcements");
}

#[test]
fn leaving_and_coming_back_with_nothing_new_draws_no_line() {
    let mut app = board(4, 2, posts(1..=6));
    assert_line_above(&app, 5);
    // At the bottom on open, so it was marked read.
    assert_eq!(app.channels[0].read_to, 6);

    app.update(key(KeyCode::Tab));
    app.update(key(KeyCode::Down));
    assert_eq!(app.selected, Selection::Channel("random".to_string()));
    app.update(ch('k'));
    assert_eq!(app.selected, general());
    app.update(page(general(), posts(1..=6)));
    assert_no_line(&app, "came back with nothing new");
}

#[test]
fn the_line_stays_when_read_and_goes_when_dang_posts() {
    let mut app = board(4, 2, posts(1..=6));
    assert_eq!((app.channels[0].read_to, app.channels[0].unread), (6, 0));
    assert_line_above(&app, 5);

    app.update(Msg::Sent(Ok(PostCreated {
        v: 1,
        post: dangs(7),
        delivery: vec![Delivery {
            node: "mac".to_string(),
            delivered: true,
        }],
    })));
    assert_no_line(&app, "Dang posted");
}

#[test]
fn the_line_stays_put_after_the_channel_is_marked_read() {
    let mut app = board(4, 2, posts(1..=6));
    assert_line_above(&app, 5);

    // At the bottom: seen, and marked read, but the line stays.
    let cmds = app.update(Msg::Stream(StreamEvent::Post(post(7))));
    assert!(cmds.contains(&Cmd::MarkRead {
        channel: "general".to_string(),
        arrival: 7
    }));
    assert_eq!(app.channels[0].read_to, 7);
    assert_line_above(&app, 5);
}

#[test]
fn a_stale_channel_list_never_lowers_the_read_point() {
    let mut app = board(4, 2, posts(1..=6));
    assert_eq!(app.channels[0].read_to, 6);
    // A reload the server answered before it stored that read point.
    app.update(channels(4, 0));
    assert_eq!(app.channels[0].read_to, 6);
}

// ------------------------------------------- what triggers a read (review)

/// A `MarkRead` for #general up to `arrival`.
fn mark_read(arrival: i64) -> Cmd {
    Cmd::MarkRead {
        channel: "general".to_string(),
        arrival,
    }
}

#[test]
fn a_live_post_seen_at_the_bottom_draws_no_line() {
    let mut app = board(6, 0, posts(1..=6));
    assert_no_line(&app, "nothing new");
    let cmds = app.update(Msg::Stream(StreamEvent::Post(post(7))));
    assert!(cmds.contains(&mark_read(7)), "{cmds:?}");
    assert_no_line(&app, "seen as it arrived");
    assert_eq!(app.line_point, Some(7));
}

#[test]
fn a_reloaded_page_seen_at_the_bottom_draws_no_line() {
    let mut app = board(6, 0, posts(1..=6));
    // The feed drops and comes back; the reload brings post 7.
    app.update(Msg::Connection(Conn::Up));
    let cmds = app.update(page(general(), posts(1..=7)));
    assert!(cmds.contains(&mark_read(7)), "{cmds:?}");
    assert_no_line(&app, "seen as it was reloaded");
    assert_eq!(app.line_point, Some(7));
}

#[test]
fn a_filter_hiding_the_read_posts_does_not_move_a_showing_line() {
    let mut page = posts(1..=6);
    for p in &mut page[4..] {
        p.tags.push("y".to_string());
    }
    let mut app = board(4, 2, page);
    assert_line_above(&app, 5);

    // Only new posts shown: no line drawn among them.
    app.update(key(KeyCode::Tab));
    for c in "/y".chars() {
        app.update(ch(c));
    }
    app.update(key(KeyCode::Enter));
    assert_eq!(app.filter.as_deref(), Some("y"));
    assert_no_line(&app, "nothing shown is read");

    // A post it shows arrives at the bottom and is read.
    let mut seven = post(7);
    seven.tags.push("y".to_string());
    let cmds = app.update(Msg::Stream(StreamEvent::Post(seven)));
    assert!(cmds.contains(&mark_read(7)), "{cmds:?}");

    // The line Dang opened with is still where it was.
    app.update(key(KeyCode::Esc));
    assert_eq!(app.filter, None);
    assert_line_above(&app, 5);
}

#[test]
fn posting_in_another_channel_keeps_this_channels_line() {
    let mut app = board(4, 2, posts(1..=6));
    assert_line_above(&app, 5);
    let mut elsewhere = dangs(7);
    elsewhere.channel = "random".to_string();
    app.update(Msg::Sent(Ok(PostCreated {
        v: 1,
        post: elsewhere,
        delivery: Vec::new(),
    })));
    assert_line_above(&app, 5);
}
