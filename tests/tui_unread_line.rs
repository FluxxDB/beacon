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

// ------------------------------------------------- opening at the line

/// At 80×24 the posts take rows 5 to 16 (see `tests/tui_reading.rs`): 30
/// two-row posts overflow them.
const SMALL: (u16, u16) = (80, 24);
const TOP_Y: u16 = 5;

/// A full page of posts, as the node sends one.
const PAGE: u32 = beacon::tui::state::PAGE;

/// Post `n` of `rows` rows by `pc-longform`: its header, then `line 01`...
fn tall(n: u32, rows: usize) -> PostJson {
    let body = (1..rows)
        .map(|i| format!("line {i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    api_post(n, "pc-longform", &body)
}

/// Older posts' page, fitting right above `before`.
fn older_page(before: (i64, String), posts: Vec<PostJson>) -> Msg {
    Msg::Loaded(Loaded::Posts {
        view: general(),
        before: Some(before),
        page: Ok(PostsResponse {
            v: 1,
            posts,
            remaining: None,
        }),
    })
}

/// Where the page `posts` starts: what the next older page is asked from.
fn oldest(posts: &[PostJson]) -> (i64, String) {
    posts
        .iter()
        .map(|p| (p.ts_ms, p.id.clone()))
        .min()
        .expect("posts")
}

/// Dang's board at [`SMALL`], #general just selected with his read point at
/// `read_to` and `unread` posts new; returns what the first page, `posts`,
/// asked for. The composer has the keys.
fn open_small(read_to: i64, unread: u32, posts: Vec<PostJson>) -> (App, Vec<Cmd>) {
    let mut app = App::new("pc", "Dang", TimeZone::UTC);
    app.update(Msg::Tick(NOW));
    app.update(Msg::Resize(SMALL.0, SMALL.1));
    app.update(Msg::Connection(Conn::Up));
    app.update(channels(read_to, unread));
    let cmds = app.update(page(general(), posts));
    (app, cmds)
}

fn marks_read(cmds: &[Cmd]) -> bool {
    cmds.iter().any(|c| matches!(c, Cmd::MarkRead { .. }))
}

fn loads_older(cmds: &[Cmd]) -> Option<(i64, String)> {
    cmds.iter().find_map(|c| match c {
        Cmd::LoadPosts {
            before: Some(before),
            ..
        } => Some(before.clone()),
        _ => None,
    })
}

#[test]
fn a_channel_with_new_posts_opens_at_the_line() {
    let (app, cmds) = open_small(10, 20, posts(1..=30));
    let buf = screen(&app);
    assert_eq!(line_rows(&buf, &app), [TOP_Y]);
    assert_eq!(header_row(&buf, &app, 11), TOP_Y + 1);
    assert!(!marks_read(&cmds), "{cmds:?}");
    assert_eq!(app.channels[0].unread, 20);
    assert!(app.scroll > 0);
}

#[test]
fn reaching_the_bottom_marks_it_read_and_keeps_the_line() {
    let (mut app, _) = open_small(10, 20, posts(1..=30));
    let opened_at = app.scroll;
    app.update(key(KeyCode::Tab));
    assert_eq!(app.focus, Focus::Posts);
    let mut cmds = app.update(ch(' '));
    cmds.extend(app.update(key(KeyCode::Tab)));
    assert_eq!(app.scroll, 0);
    assert!(cmds.contains(&mark_read(30)), "{cmds:?}");
    assert_eq!(app.channels[0].unread, 0);

    // Back up to where Dang started: the line is still there.
    while app.scroll < opened_at {
        let before = app.scroll;
        app.update(key(KeyCode::PageUp));
        assert!(app.scroll > before, "PgUp moves up");
        if !line_rows(&screen(&app), &app).is_empty() {
            break;
        }
    }
    assert_eq!(app.line_point, Some(10));
    assert_line_above(&app, 11);
}

#[test]
fn few_new_posts_open_at_the_bottom_and_mark_read() {
    let (app, cmds) = open_small(28, 2, posts(1..=30));
    assert_eq!(app.scroll, 0);
    assert!(cmds.contains(&mark_read(30)), "{cmds:?}");
    assert_line_above(&app, 29);
}

#[test]
fn a_tall_first_new_post_opens_with_its_top_under_the_line() {
    let mut page = posts(1..=4);
    page.push(tall(5, 30));
    let (app, cmds) = open_small(4, 1, page);
    let buf = screen(&app);
    assert_eq!(line_rows(&buf, &app), [TOP_Y]);
    assert!(row_text(&buf, TOP_Y + 1).contains("pc-longform"));
    assert!(!marks_read(&cmds), "{cmds:?}");
}

#[test]
fn more_than_a_page_new_loads_older_pages_until_a_read_post() {
    let first = posts(PAGE + 1..=2 * PAGE);
    let cursor = oldest(&first);
    let (mut app, cmds) = open_small(50, 150, first);
    assert_eq!(loads_older(&cmds), Some(cursor.clone()), "{cmds:?}");
    assert!(!marks_read(&cmds), "{cmds:?}");

    let cmds = app.update(older_page(cursor, posts(1..=PAGE)));
    assert!(!marks_read(&cmds), "{cmds:?}");
    assert_eq!(loads_older(&cmds), None, "{cmds:?}");
    let buf = screen(&app);
    assert_eq!(line_rows(&buf, &app), [TOP_Y]);
    assert_eq!(header_row(&buf, &app, 51), TOP_Y + 1);
}

/// Post `n`, with hundreds of posts after it still on the same day.
fn early(n: u32) -> PostJson {
    let mut p = post(n);
    p.ts_ms = NOW - (1_000 - i64::from(n)) * MIN;
    p
}

#[test]
fn past_five_pages_it_opens_at_the_bottom_with_no_line() {
    let newest = 6 * PAGE;
    let page_of = |k: u32| -> Vec<PostJson> {
        (newest - (k + 1) * PAGE + 1..=newest - k * PAGE)
            .map(early)
            .collect()
    };
    let first = page_of(0);
    let mut cursor = oldest(&first);
    let (mut app, mut cmds) = open_small(10, newest - 10, first);
    for k in 1..5 {
        assert_eq!(loads_older(&cmds), Some(cursor.clone()), "page {k}");
        assert!(!marks_read(&cmds), "page {k}: {cmds:?}");
        let older = page_of(k);
        let next = oldest(&older);
        cmds = app.update(older_page(cursor, older));
        cursor = next;
    }
    assert_eq!(loads_older(&cmds), None, "no sixth page: {cmds:?}");
    assert_eq!(app.scroll, 0);
    assert!(cmds.contains(&mark_read(i64::from(newest))), "{cmds:?}");
    assert_no_line(&app, "no read post in five pages");
}

#[test]
fn a_never_read_channel_loads_no_older_pages() {
    let (app, cmds) = open_small(0, PAGE, posts(1..=PAGE));
    assert_eq!(loads_older(&cmds), None, "{cmds:?}");
    assert!(cmds.contains(&mark_read(i64::from(PAGE))), "{cmds:?}");
    assert_eq!(app.scroll, 0);
}

#[test]
fn a_reload_while_scrolled_up_marks_nothing_and_does_not_rescroll() {
    let (mut app, _) = open_small(10, 20, posts(1..=30));
    let s = app.scroll;
    assert!(s > 0);
    let cmds = app.update(Msg::Connection(Conn::Up));
    assert!(!marks_read(&cmds), "{cmds:?}");
    let cmds = app.update(page(general(), posts(1..=30)));
    assert_eq!(app.scroll, s);
    assert!(!marks_read(&cmds), "{cmds:?}");
    assert_eq!(line_rows(&screen(&app), &app), [TOP_Y]);
}

#[test]
fn a_never_read_channel_gets_a_line_for_posts_that_arrive_while_scrolled_up() {
    let (mut app, cmds) = open_small(0, 30, posts(1..=30));
    assert_eq!(app.scroll, 0);
    assert!(cmds.contains(&mark_read(30)), "{cmds:?}");
    assert_eq!(app.line_point, Some(30));

    app.update(key(KeyCode::Tab));
    assert_eq!(app.focus, Focus::Posts);
    app.update(key(KeyCode::PageUp));
    assert!(app.scroll > 0);
    let cmds = app.update(Msg::Stream(StreamEvent::Post(post(31))));
    assert!(!marks_read(&cmds), "scrolled up: {cmds:?}");

    app.update(ch(' '));
    let cmds = app.update(key(KeyCode::Tab));
    assert_eq!(app.scroll, 0);
    assert!(cmds.contains(&mark_read(31)), "{cmds:?}");
    assert_line_above(&app, 31);
}

#[test]
fn a_post_arriving_while_older_pages_load_is_not_read() {
    let first = posts(PAGE + 1..=2 * PAGE);
    let cursor = oldest(&first);
    let (mut app, _) = open_small(50, 150, first);
    let cmds = app.update(Msg::Stream(StreamEvent::Post(post(2 * PAGE + 1))));
    assert!(!marks_read(&cmds), "still opening: {cmds:?}");
    assert_eq!(app.line_point, Some(50));

    app.update(older_page(cursor, posts(1..=PAGE)));
    assert_eq!(line_rows(&screen(&app), &app), [TOP_Y]);
    assert_eq!(app.channels[0].unread, 151);
}

#[test]
fn an_older_page_that_fails_leaves_the_channel_opened_at_the_bottom() {
    let first = posts(PAGE + 1..=2 * PAGE);
    let cursor = oldest(&first);
    let (mut app, _) = open_small(50, 150, first);
    let cmds = app.update(Msg::Loaded(Loaded::Posts {
        view: general(),
        before: Some(cursor),
        page: Err("timed out".to_string()),
    }));
    assert_eq!(app.opening, None);
    assert_eq!(app.scroll, 0);
    assert!(cmds.contains(&mark_read(i64::from(2 * PAGE))), "{cmds:?}");
    // What arrives at the bottom from now on is read.
    let cmds = app.update(Msg::Stream(StreamEvent::Post(post(2 * PAGE + 1))));
    assert!(
        cmds.contains(&mark_read(i64::from(2 * PAGE + 1))),
        "{cmds:?}"
    );
}
