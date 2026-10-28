//! Reading in the posts pane (Dang, MJN7KXH6: on a small screen, `k` from
//! the bottom of a long post jumped to its top and the middle was lost).
//! `j` and `k` read a post taller than the pane a pane at a time, two rows
//! of overlap and never past its far edge, before they move on; the post
//! they move on to, when it is tall too, starts at the end that keeps the
//! reading going. While the selected post is that tall, the pane's edges
//! say there is more of it above or below, quietly. Short posts move one
//! per press, as they always did. Going into the posts leaves a tall post
//! where it is, and a re-wrap (the sidebar shown or hidden, a resize) keeps
//! the place in it.

use std::collections::BTreeSet;

use beacon::api::{ChannelJson, ChannelsJson, PostJson, PostsResponse, StreamEvent};
use beacon::model::{parse_mentions, AuthorKind};
use beacon::tui::state::{App, Cmd, Conn, Focus, Loaded, Msg, Selection, PAGE};
use beacon::tui::theme::{palette, role, ColorMode, Rgb, Theme};
use beacon::tui::view::{self, Layout};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use jiff::tz::TimeZone;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;

/// 2026-10-21 22:00 UTC: every post below is from the same day.
const NOW: i64 = 1_792_620_000_000;
const MIN: i64 = 60_000;
const TRUECOLOR: Theme = Theme {
    mode: ColorMode::TrueColor,
};

/// At 80×24, measured by hand from the layout in `src/tui/view.rs`: the
/// title's rule on row 3, a blank row, the posts on rows 5 to 16, the notice
/// row 17, the composer from row 18, the footer's rule on 21.
const SMALL: (u16, u16) = (80, 24);
const ABOVE_Y: u16 = 4;
const TOP_Y: u16 = 5;
const BOTTOM_Y: u16 = 16;
const NOTICE_Y: u16 = 17;
const VIEW: usize = 12;

const MORE_ABOVE: &str = "↑ more of this post";
const MORE_BELOW: &str = "↓ more of this post";
const NEWER_BELOW: &str = "↓ newer posts below";

/// The tall post's author: its header row is the only row with this name.
const TALL_AUTHOR: &str = "pc-longform";

// ---------------------------------------------------------------- helpers

fn ulid(n: u32) -> String {
    format!("01JAN{n:021}")
}

/// A post as the node sends it; `n` gives its id and order (all today).
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

/// `count` body rows, `line 01` and on: none wraps at 80 columns.
fn lines(count: usize) -> String {
    (1..=count)
        .map(|i| format!("line {i:02}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A post of `rows` rows by [`TALL_AUTHOR`]: its header, then `line 01`...
fn tall(n: u32, rows: usize) -> PostJson {
    api_post(n, TALL_AUTHOR, &lines(rows - 1))
}

/// Dang's board at `size`, #general open on `posts` (a short page unless
/// there are `PAGE` of them), the composer focused.
fn board(size: (u16, u16), posts: Vec<PostJson>) -> App {
    let mut app = App::new("pc", "Dang", TimeZone::UTC);
    app.update(Msg::Tick(NOW));
    app.update(Msg::Resize(size.0, size.1));
    app.update(Msg::Connection(Conn::Up));
    app.update(Msg::Loaded(Loaded::Channels(ChannelsJson {
        v: 1,
        channels: vec![ChannelJson {
            name: "general".to_string(),
            topic: None,
            unread: Some(0),
            read_to: None,
        }],
        announcements_unread: Some(0),
    })));
    app.update(Msg::Loaded(Loaded::Posts {
        view: Selection::Channel("general".to_string()),
        before: None,
        page: Ok(PostsResponse {
            v: 1,
            posts,
            remaining: None,
        }),
    }));
    assert_eq!(app.focus, Focus::Composer);
    app
}

/// An older short post, a tall one of `rows` rows, a newer short post; the
/// posts focused (Tab), on the newest.
fn tall_between(size: (u16, u16), rows: usize) -> App {
    let mut app = board(
        size,
        vec![
            api_post(1, "pc-early", "an older post"),
            tall(2, rows),
            api_post(3, "pc-late", "a newer post"),
        ],
    );
    app.update(key(KeyCode::Tab));
    assert_eq!(app.focus, Focus::Posts);
    assert_eq!(selected(&app), Some(3));
    app
}

fn key(code: KeyCode) -> Msg {
    Msg::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ch(c: char) -> Msg {
    key(KeyCode::Char(c))
}

/// Which post is selected, by its `n`.
fn selected(app: &App) -> Option<u32> {
    let id = app.selected_post.as_deref()?;
    Some(id[5..].parse().unwrap())
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

fn screen_text(buf: &Buffer) -> String {
    (0..buf.area.height)
        .map(|y| row_text(buf, y))
        .collect::<Vec<_>>()
        .join("\n")
}

fn posts_pane(app: &App) -> Rect {
    Layout::compute(Rect::new(0, 0, app.size.0, app.size.1), app)
        .expect("layout")
        .posts
}

/// The tall post's rows on screen, top to bottom, counted from its first
/// row: a reply's line (when it has one), the header, then `line 01`...
fn tall_rows(app: &App) -> Vec<usize> {
    let buf = screen(app);
    let pane = posts_pane(app);
    let has_reply = app
        .posts
        .iter()
        .any(|p| p.author == TALL_AUTHOR && p.body.starts_with("re "));
    let header = usize::from(has_reply);
    let mut shown = Vec::new();
    for y in pane.top()..pane.bottom() {
        let text = row_text(&buf, y);
        if text.contains(TALL_AUTHOR) {
            shown.push(header);
        } else if let Some(at) = text.find("line ") {
            let n: usize = text[at + 5..at + 7].parse().unwrap();
            shown.push(header + n);
        } else if has_reply && text.contains("╭─") {
            shown.push(0);
        }
    }
    assert!(
        shown.windows(2).all(|w| w[1] == w[0] + 1),
        "the post's rows in order: {shown:?}\n{}",
        screen_text(&buf)
    );
    shown
}

fn range(rows: std::ops::RangeInclusive<usize>) -> Vec<usize> {
    rows.collect()
}

/// Every press of `c` while the tall post (`n` 2) stays selected, and the
/// rows each one showed (the first view is the one before any press).
fn read_with(app: &mut App, c: char, limit: usize) -> Vec<Vec<usize>> {
    let mut views = vec![tall_rows(app)];
    for _ in 0..limit {
        assert!(app.update(ch(c)).is_empty(), "nothing asked of the node");
        if selected(app) != Some(2) {
            return views;
        }
        views.push(tall_rows(app));
    }
    panic!("{c} never left the tall post in {limit} presses: {views:?}");
}

/// Checks that `views` cover every row of a post of `rows` rows, each step
/// overlapping the one before by `overlap` rows (the last, which stops at
/// the post's edge, by at least that).
fn assert_read_whole(views: &[Vec<usize>], rows: usize, overlap: usize) {
    let seen: BTreeSet<usize> = views.iter().flatten().copied().collect();
    assert_eq!(seen, (0..rows).collect(), "every row: {views:?}");
    for (i, pair) in views.windows(2).enumerate() {
        let a: BTreeSet<usize> = pair[0].iter().copied().collect();
        let common = pair[1].iter().filter(|r| a.contains(r)).count();
        if i + 2 < views.len() {
            assert_eq!(common, overlap, "step {i}: {views:?}");
        } else {
            assert!(common >= overlap, "the last step: {views:?}");
        }
    }
}

// ------------------------------------------------------------- the layout

#[test]
fn the_small_screen_is_as_measured() {
    let app = tall_between(SMALL, 3 * VIEW);
    let pane = posts_pane(&app);
    assert_eq!((pane.top(), pane.bottom() - 1), (TOP_Y, BOTTOM_Y));
    let layout = Layout::compute(Rect::new(0, 0, 80, 24), &app).unwrap();
    assert_eq!((layout.rule_y + 1, layout.notice_y), (ABOVE_Y, NOTICE_Y));
}

// ------------------------------------------------------- reading with k, j

#[test]
fn k_lands_on_a_tall_posts_bottom_and_reads_up_to_its_top() {
    // Three panes tall: its header and 35 lines.
    let mut app = tall_between(SMALL, 3 * VIEW);
    assert!(app.update(ch('k')).is_empty());
    assert_eq!(selected(&app), Some(2));
    // Its last row on the pane's bottom edge.
    assert!(row_text(&screen(&app), BOTTOM_Y).contains("line 35"));

    let views = read_with(&mut app, 'k', 10);
    assert_eq!(
        views,
        [
            range(24..=35),
            range(14..=25),
            range(4..=15),
            // Stops at its top, not past it.
            range(0..=11),
        ]
    );
    assert_read_whole(&views, 3 * VIEW, 2);
    // That last k moved on, to the older post, shown whole.
    assert_eq!(selected(&app), Some(1));
    let text = screen_text(&screen(&app));
    assert!(text.contains("an older post"), "{text}");
}

#[test]
fn the_top_row_lands_on_the_top_edge() {
    let mut app = tall_between(SMALL, 3 * VIEW);
    for _ in 0..4 {
        app.update(ch('k'));
    }
    assert_eq!(selected(&app), Some(2));
    let buf = screen(&app);
    assert!(row_text(&buf, TOP_Y).contains(TALL_AUTHOR));
    assert!(row_text(&buf, BOTTOM_Y).contains("line 11"));
}

#[test]
fn j_lands_on_a_tall_posts_top_and_reads_down_to_its_bottom() {
    let mut app = tall_between(SMALL, 3 * VIEW);
    // Up past it to the older post first.
    for _ in 0..5 {
        app.update(ch('k'));
    }
    assert_eq!(selected(&app), Some(1));

    assert!(app.update(ch('j')).is_empty());
    assert_eq!(selected(&app), Some(2));
    // Its header on the pane's top edge.
    assert!(row_text(&screen(&app), TOP_Y).contains(TALL_AUTHOR));
    let views = read_with(&mut app, 'j', 10);
    assert_eq!(
        views,
        [
            range(0..=11),
            range(10..=21),
            range(20..=31),
            // Stops at its bottom, its last row on the bottom edge.
            range(24..=35),
        ]
    );
    assert_read_whole(&views, 3 * VIEW, 2);
    assert_eq!(selected(&app), Some(3), "then on to the newer post");
    assert_eq!(app.scroll, 0, "which is the newest, at the bottom");
}

#[test]
fn entering_the_posts_at_the_bottom_leaves_a_tall_newest_post_where_it_is() {
    // Dang's case: at the bottom, the last rows of a long newest post on
    // screen. Tab, or Esc from an empty composer, selects it without
    // moving the view, and k reads up from there: nothing is skipped.
    for enter in [KeyCode::Tab, KeyCode::Esc] {
        let mut app = board(
            SMALL,
            vec![api_post(1, "pc-early", "an older post"), tall(2, 3 * VIEW)],
        );
        assert_eq!(tall_rows(&app), range(24..=35));
        app.update(key(enter));
        assert_eq!((app.focus, selected(&app)), (Focus::Posts, Some(2)));
        assert_eq!(app.scroll, 0, "{enter:?}: the view stays");
        let views = read_with(&mut app, 'k', 10);
        assert_eq!(
            views,
            [range(24..=35), range(14..=25), range(4..=15), range(0..=11)],
            "{enter:?}"
        );
        assert_read_whole(&views, 3 * VIEW, 2);
        assert_eq!(selected(&app), Some(1));
    }
}

#[test]
fn entering_the_posts_picks_anew_when_the_selected_post_is_out_of_sight() {
    // A post selected; then, while the composer has the keys, a long
    // answer arrives below it and pushes it out of sight.
    let mut app = board(
        SMALL,
        vec![
            api_post(0, "pc-early", "an older post"),
            api_post(1, "pc-late", "a question"),
        ],
    );
    app.update(key(KeyCode::Tab));
    assert_eq!(selected(&app), Some(1));
    app.update(key(KeyCode::Tab));
    assert_eq!(app.focus, Focus::Composer);
    app.update(Msg::Stream(StreamEvent::Post(tall(2, 3 * VIEW))));
    assert_eq!((app.scroll, tall_rows(&app)), (0, range(24..=35)));
    // Tab in: the answer on screen is picked, where it is, and read.
    app.update(key(KeyCode::Tab));
    assert_eq!((selected(&app), app.scroll), (Some(2), 0));
    let views = read_with(&mut app, 'k', 10);
    assert_read_whole(&views, 3 * VIEW, 2);
    assert_eq!(selected(&app), Some(1), "then the question");

    // A selected post still partly in sight stays selected.
    let mut app = tall_between(SMALL, 3 * VIEW);
    app.update(ch('k'));
    app.update(ch('k'));
    let scroll = app.scroll;
    app.update(key(KeyCode::Tab));
    app.update(key(KeyCode::Tab));
    assert_eq!((selected(&app), app.scroll), (Some(2), scroll));
}

#[test]
fn a_tall_newest_post_is_read_down_to_the_bottom_then_j_stays() {
    let mut app = board(
        SMALL,
        vec![api_post(1, "pc-early", "an older post"), tall(2, 3 * VIEW)],
    );
    app.update(key(KeyCode::Tab));
    assert_eq!(selected(&app), Some(2));
    // Up to its top first.
    for _ in 0..3 {
        app.update(ch('k'));
    }
    assert_eq!(tall_rows(&app), range(0..=11));
    for want in [range(10..=21), range(20..=31), range(24..=35)] {
        assert!(app.update(ch('j')).is_empty());
        assert_eq!(tall_rows(&app), want);
    }
    assert_eq!(app.scroll, 0);
    // Nothing left to read and nothing newer: as before, nothing moves.
    assert!(app.update(ch('j')).is_empty());
    assert_eq!((selected(&app), app.scroll), (Some(2), 0));
}

#[test]
fn at_the_oldest_post_k_asks_for_older_posts_only_once_its_top_is_read() {
    // A full page, so there may be older posts; the oldest is tall.
    let mut posts = vec![tall(2, 3 * VIEW)];
    posts.extend((3..PAGE + 2).map(|n| api_post(n, "pc-late", &format!("post {n}"))));
    let mut app = board(SMALL, posts);
    app.update(key(KeyCode::Tab));
    // Up to the post just newer than the tall one.
    while selected(&app) != Some(3) {
        assert!(app.update(ch('k')).is_empty());
    }
    assert!(app.update(ch('k')).is_empty());
    assert_eq!(tall_rows(&app), range(24..=35));
    for want in [range(14..=25), range(4..=15), range(0..=11)] {
        assert!(app.update(ch('k')).is_empty(), "still reading");
        assert_eq!(tall_rows(&app), want);
    }
    let oldest = app.posts[0].clone();
    assert_eq!(
        app.update(ch('k')),
        [Cmd::LoadPosts {
            channel: "general".to_string(),
            before: Some((oldest.ts_ms, oldest.id))
        }]
    );
    assert_eq!(selected(&app), Some(2));
}

// ------------------------------------------------------------ short posts

#[test]
fn short_posts_move_one_per_press_as_before() {
    // Six posts of two rows, a blank row apart: rows from the bottom 0-1
    // (post 6), 3-4, 6-7, 9-10, 12-13 and 15-16 (post 1); the pane shows 12.
    let posts = (1..=6)
        .map(|n| api_post(n, "pc-configs", &format!("post {n}")))
        .collect();
    let mut app = board(SMALL, posts);
    app.update(key(KeyCode::Tab));
    assert_eq!((selected(&app), app.scroll), (Some(6), 0));
    let mut seen = Vec::new();
    for _ in 0..5 {
        assert!(app.update(ch('k')).is_empty());
        seen.push((selected(&app).unwrap(), app.scroll));
    }
    // Each revealed whole and no more, as before.
    assert_eq!(seen, [(5, 0), (4, 0), (3, 0), (2, 2), (1, 5)]);
    let mut seen = Vec::new();
    for _ in 0..5 {
        assert!(app.update(ch('j')).is_empty());
        seen.push((selected(&app).unwrap(), app.scroll));
    }
    assert_eq!(seen, [(2, 5), (3, 5), (4, 5), (5, 3), (6, 0)]);

    // Cut at the top edge, a short post is not read on: k moves on.
    app.selected_post = Some(ulid(2));
    app.scroll = 1;
    app.update(ch('k'));
    assert_eq!((selected(&app), app.scroll), (Some(1), 5));
    // Nor cut at the bottom edge: j moves on.
    app.selected_post = Some(ulid(5));
    app.scroll = 4;
    app.update(ch('j'));
    assert_eq!((selected(&app), app.scroll), (Some(6), 0));
}

// ------------------------------------------------- space tab: the latest

/// Twenty short posts, the posts focused on the newest, the view scrolled up
/// two pages.
fn scrolled_up_in_twenty() -> App {
    let posts = (1..=20)
        .map(|n| api_post(n, "pc-configs", &format!("post {n}")))
        .collect();
    let mut app = board(SMALL, posts);
    app.update(key(KeyCode::Tab));
    assert_eq!(app.focus, Focus::Posts);
    app.update(key(KeyCode::PageUp));
    app.update(key(KeyCode::PageUp));
    assert!(app.scroll > 0);
    app
}

#[test]
fn space_tab_in_the_posts_jumps_to_the_latest_post() {
    let mut app = scrolled_up_in_twenty();
    app.selected_post = Some(ulid(3));
    assert!(app.update(ch(' ')).is_empty());
    assert!(app.leader);
    app.update(key(KeyCode::Tab));
    assert_eq!(app.scroll, 0);
    assert_eq!(selected(&app), Some(20));
    assert_eq!(app.focus, Focus::Posts, "Tab did not cycle the focus");
    assert!(!app.leader);
}

#[test]
fn space_tab_from_the_sidebar_still_goes_to_the_posts() {
    let mut app = board(SMALL, vec![api_post(1, "pc-configs", "post 1")]);
    app.update(key(KeyCode::Tab));
    app.update(ch('E'));
    assert_eq!(app.focus, Focus::Sidebar);
    app.update(ch(' '));
    app.update(key(KeyCode::Tab));
    assert_eq!(app.focus, Focus::Posts, "the chord cancels; Tab goes on");
    assert!(!app.leader);
}

#[test]
fn space_tab_at_the_bottom_marks_the_channel_read() {
    let mut app = scrolled_up_in_twenty();
    app.channels[0].unread = 3;
    assert!(app.update(ch(' ')).is_empty());
    let cmds = app.update(key(KeyCode::Tab));
    assert_eq!(
        cmds,
        [Cmd::MarkRead {
            channel: "general".to_string(),
            arrival: 20
        }]
    );
    assert_eq!(app.channels[0].unread, 0);
}

// --------------------------------------------------- a pane of 1 or 2 rows

#[test]
fn a_pane_of_one_or_two_rows_still_reads_on_and_never_loops() {
    // 80×13 and 80×14 leave the posts one and two rows; a pane less two
    // rows would be no step at all, so it steps one.
    for (height, want_up, want_down) in [
        (
            13u16,
            vec![vec![5], vec![4], vec![3], vec![2], vec![1], vec![0]],
            vec![vec![0], vec![1], vec![2], vec![3], vec![4], vec![5]],
        ),
        (
            14,
            vec![vec![4, 5], vec![3, 4], vec![2, 3], vec![1, 2], vec![0, 1]],
            vec![vec![0, 1], vec![1, 2], vec![2, 3], vec![3, 4], vec![4, 5]],
        ),
    ] {
        let mut app = tall_between((80, height), 6);
        assert_eq!(usize::from(posts_pane(&app).height), want_up[0].len());
        // On to the tall post.
        while selected(&app) != Some(2) {
            assert!(app.update(ch('k')).is_empty());
        }
        let up = read_with(&mut app, 'k', 20);
        assert_eq!(up, want_up, "k at {height} rows");
        assert_eq!(selected(&app), Some(1));
        while selected(&app) != Some(2) {
            assert!(app.update(ch('j')).is_empty());
        }
        let down = read_with(&mut app, 'j', 20);
        assert_eq!(down, want_down, "j at {height} rows");
        assert_eq!(selected(&app), Some(3));
    }
}

// --------------------------------------------------------- the reply line

#[test]
fn a_reply_line_is_read_as_the_top_row_of_a_tall_post() {
    let answered = api_post(1, "pc-early", "the plan for the build");
    let short = answered.short_id.clone();
    let reply = api_post(
        2,
        TALL_AUTHOR,
        &format!("re {short}: {}", lines(3 * VIEW - 2)),
    );
    let mut app = board(
        SMALL,
        vec![answered, reply, api_post(3, "pc-late", "a newer post")],
    );
    app.update(key(KeyCode::Tab));
    let views = {
        app.update(ch('k'));
        read_with(&mut app, 'k', 10)
    };
    // The reply's line, its header, 34 lines: 36 rows, read the same way.
    assert_eq!(
        views,
        [range(24..=35), range(14..=25), range(4..=15), range(0..=11)]
    );
    assert_read_whole(&views, 3 * VIEW, 2);

    // Back down from the older post: the reply's line on the top edge.
    app.update(ch('j'));
    assert_eq!(selected(&app), Some(2));
    let buf = screen(&app);
    let top = row_text(&buf, TOP_Y);
    assert!(top.contains("╭─ pc-early  the plan"), "{top}");
    assert!(row_text(&buf, TOP_Y + 1).contains(TALL_AUTHOR));
}

// --------------------------------------------------- a re-wrap mid-read

/// `count` numbered words, `w001 w002`...: a body that wraps to fewer rows
/// in a wider pane.
fn words(count: usize) -> String {
    (1..=count)
        .map(|i| format!("w{i:03}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A post of 400 words, `w001` to `w400` (51 rows beside the sidebar at
/// 80 columns, 35 across the full width), between two short ones, read up
/// to its middle.
fn mid_read() -> App {
    let mut app = board(
        SMALL,
        vec![
            api_post(1, "pc-early", "an older post"),
            api_post(2, TALL_AUTHOR, &words(400)),
            api_post(3, "pc-late", "a newer post"),
        ],
    );
    app.update(key(KeyCode::Tab));
    for _ in 0..3 {
        app.update(ch('k'));
    }
    assert_eq!(selected(&app), Some(2));
    assert_mid_read(&app, "before");
    app
}

/// The post's middle word is on screen, and neither of its ends.
fn assert_mid_read(app: &App, label: &str) {
    let text = screen_text(&screen(app));
    assert!(text.contains("w200"), "{label}: its middle\n{text}");
    assert!(
        !text.contains(TALL_AUTHOR) && !text.contains("w400"),
        "{label}: neither end\n{text}"
    );
}

#[test]
fn hiding_and_showing_the_sidebar_mid_read_keeps_the_place() {
    let mut app = mid_read();
    // space e from the posts: hidden, and the post wraps to fewer rows.
    app.update(ch(' '));
    app.update(ch('e'));
    assert_eq!((app.hide_sidebar, app.focus), (true, Focus::Posts));
    assert_mid_read(&app, "hidden");
    // space e again: shown, and focused.
    app.update(ch(' '));
    app.update(ch('e'));
    assert_eq!((app.hide_sidebar, app.focus), (false, Focus::Sidebar));
    assert_mid_read(&app, "shown");
}

#[test]
fn a_resize_mid_read_keeps_the_place() {
    // Wider, as when the Mac's font gets smaller; wider and taller; below
    // 80 columns; and back.
    for size in [(120u16, 24u16), (100, 33), (60, 24)] {
        let mut app = mid_read();
        app.update(Msg::Resize(size.0, size.1));
        assert_mid_read(&app, &format!("{size:?}"));
        app.update(Msg::Resize(SMALL.0, SMALL.1));
        assert_mid_read(&app, &format!("{size:?} and back"));
    }
}

/// At the bottom, the tall post selected and running off the top, with a
/// newer post under it: as after answering a long post.
fn at_the_bottom_under_a_tall_post() -> App {
    let mut app = board(
        SMALL,
        vec![api_post(1, "pc-early", "an older post"), tall(2, 3 * VIEW)],
    );
    app.update(key(KeyCode::Tab));
    assert_eq!((selected(&app), app.scroll), (Some(2), 0));
    app.update(Msg::Stream(StreamEvent::Post(api_post(
        3,
        "pc-late",
        "a newer post",
    ))));
    assert_eq!((selected(&app), app.scroll), (Some(2), 0));
    assert_at_the_bottom(&app, "before");
    app
}

/// Still at the bottom: the newer post on screen, nothing below it, and a
/// live post that comes now is read.
fn assert_at_the_bottom(app: &App, label: &str) {
    let text = screen_text(&screen(app));
    assert_eq!(app.scroll, 0, "{label}\n{text}");
    assert!(text.contains("a newer post"), "{label}\n{text}");
    assert!(!text.contains(NEWER_BELOW), "{label}\n{text}");
    let mut app = app.clone();
    let cmds = app.update(Msg::Stream(StreamEvent::Post(api_post(
        4,
        "pc-late",
        "and another",
    ))));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::MarkRead { arrival: 4, .. })),
        "{label}: the next live post is read: {cmds:?}"
    );
}

#[test]
fn a_resize_or_space_e_at_the_bottom_stays_at_the_bottom() {
    for size in [(100u16, 24u16), (120, 30), (60, 24), SMALL] {
        let mut app = at_the_bottom_under_a_tall_post();
        app.update(Msg::Resize(size.0, size.1));
        assert_at_the_bottom(&app, &format!("{size:?}"));
    }
    let mut app = at_the_bottom_under_a_tall_post();
    app.update(ch(' '));
    app.update(ch('e'));
    assert!(app.hide_sidebar);
    assert_at_the_bottom(&app, "hidden");
    app.update(ch(' '));
    app.update(ch('e'));
    assert!(!app.hide_sidebar);
    assert_at_the_bottom(&app, "shown again");
}

#[test]
fn a_same_size_resize_leaves_the_view_as_it_is() {
    // At the bottom, mid-read, and with the tall post's top showing.
    let mut cases = vec![at_the_bottom_under_a_tall_post(), mid_read()];
    let mut top = tall_between(SMALL, 3 * VIEW);
    for _ in 0..4 {
        top.update(ch('k'));
    }
    assert_eq!(tall_rows(&top), range(0..=11));
    top.update(key(KeyCode::PageUp));
    cases.push(top);
    for mut app in cases {
        let before = (app.scroll, screen_text(&screen(&app)));
        app.update(Msg::Resize(SMALL.0, SMALL.1));
        assert_eq!((app.scroll, screen_text(&screen(&app))), before);
    }
}

// --------------------------------------------------- the server down

#[test]
fn with_the_server_down_the_hint_sits_under_its_band_and_k_reads_on() {
    let mut app = tall_between(SMALL, 3 * VIEW);
    app.update(Msg::Connection(Conn::Down { since_ms: NOW }));
    assert!(app.update(ch('k')).is_empty());
    let buf = screen(&app);
    let band_end = (0..SMALL.1)
        .rfind(|&y| row_text(&buf, y).contains("Retrying"))
        .expect("the server-down band");
    assert!(row_text(&buf, TOP_Y).contains("Can't reach the beacon server"));
    // Right above the first row of posts, as without the band: under it,
    // a blank row apart.
    let first = (0..SMALL.1)
        .find(|&y| row_text(&buf, y).contains("line "))
        .expect("the post's rows");
    assert_eq!(first - 1, band_end + 2, "{}", screen_text(&buf));
    let above = hint_cells(&app, &buf, first - 1, MORE_ABOVE);
    assert_quiet(&buf, &above);
    let views = read_with(&mut app, 'k', 10);
    assert!(views[0].len() < VIEW, "the band takes rows: {views:?}");
    assert_eq!(views[0].last(), Some(&35), "landed on its bottom");
    assert_read_whole(&views, 3 * VIEW, 2);
}

// -------------------------------------------------------- PgUp and PgDn

#[test]
fn pgup_and_pgdn_still_scroll_by_a_page_and_leave_the_selection() {
    let mut app = tall_between(SMALL, 3 * VIEW);
    app.update(ch('k'));
    let scroll = app.scroll;
    app.update(key(KeyCode::PageUp));
    assert_eq!((selected(&app), app.scroll), (Some(2), scroll + VIEW - 2));
    app.update(key(KeyCode::PageDown));
    app.update(key(KeyCode::PageDown));
    assert_eq!((selected(&app), app.scroll), (Some(2), 0));
}

// ------------------------------------------------------ the position hint

/// The hint `text`'s cells on row `y`, right-aligned where the newer-posts
/// notice is (three cells in from the pane's right edge).
fn hint_cells(app: &App, buf: &Buffer, y: u16, text: &str) -> Vec<(u16, u16)> {
    let pane = posts_pane(app);
    let end = pane.right() - 3;
    let width = text.chars().count() as u16;
    let row: String = (end - width..end).map(|x| buf[(x, y)].symbol()).collect();
    assert_eq!(row, text, "row {y}: {}", row_text(buf, y));
    (end - width..end).map(|x| (x, y)).collect()
}

fn rgb(c: Rgb) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

/// The hint is in quiet ash on the pane's background, never bold, never
/// the accent, and every cell of the screen has its own colours.
fn assert_quiet(buf: &Buffer, cells: &[(u16, u16)]) {
    for &(x, y) in cells {
        let cell = &buf[(x, y)];
        assert_eq!(
            (cell.fg, cell.bg),
            (rgb(palette::ASH), rgb(role::MAIN_BG)),
            "({x},{y}) {:?}",
            cell.symbol()
        );
        assert!(!cell.modifier.contains(Modifier::BOLD), "({x},{y})");
    }
    for (i, cell) in buf.content().iter().enumerate() {
        let (x, y) = buf.pos_of(i);
        assert_ne!(cell.bg, Color::Reset, "default bg at ({x},{y})");
        assert_ne!(cell.fg, Color::Reset, "default fg at ({x},{y})");
    }
}

/// Every accent cell on screen: here, only the composer's prompt (its
/// caret is not drawn while the posts have the keys) and the active
/// channel's edge in the sidebar.
fn accent_cells(buf: &Buffer) -> Vec<(u16, u16)> {
    let accent = rgb(role::ACCENT);
    buf.content()
        .iter()
        .enumerate()
        .filter(|(_, c)| c.fg == accent || c.bg == accent)
        .map(|(i, _)| buf.pos_of(i))
        .collect()
}

#[test]
fn the_edges_say_there_is_more_of_a_tall_post_while_it_is_read() {
    let mut app = tall_between(SMALL, 3 * VIEW);
    let before = accent_cells(&screen(&app));

    // At its bottom: more above; below, the newer post, as ever.
    app.update(ch('k'));
    let buf = screen(&app);
    let above = hint_cells(&app, &buf, ABOVE_Y, MORE_ABOVE);
    hint_cells(&app, &buf, NOTICE_Y, NEWER_BELOW);
    assert!(!screen_text(&buf).contains(MORE_BELOW));
    assert_quiet(&buf, &above);
    assert_eq!(accent_cells(&buf), before, "the hint adds no accent");

    // In its middle: more both ways (the post's own rows below come before
    // the newer posts).
    app.update(ch('k'));
    let buf = screen(&app);
    let above = hint_cells(&app, &buf, ABOVE_Y, MORE_ABOVE);
    let below = hint_cells(&app, &buf, NOTICE_Y, MORE_BELOW);
    assert!(!screen_text(&buf).contains(NEWER_BELOW));
    assert_quiet(&buf, &[above, below].concat());
    assert_eq!(accent_cells(&buf), before);

    // At its top: more below only.
    app.update(ch('k'));
    app.update(ch('k'));
    let buf = screen(&app);
    let below = hint_cells(&app, &buf, NOTICE_Y, MORE_BELOW);
    assert!(!screen_text(&buf).contains(MORE_ABOVE));
    assert_quiet(&buf, &below);

    // On the older, short post: no hint, the newer posts below.
    app.update(ch('k'));
    let buf = screen(&app);
    let text = screen_text(&buf);
    assert!(
        !text.contains(MORE_ABOVE) && !text.contains(MORE_BELOW),
        "{text}"
    );
    assert!(text.contains(NEWER_BELOW), "{text}");
}

#[test]
fn no_hint_unless_the_selected_post_is_tall_and_the_posts_have_the_keys() {
    let hints = |app: &App| {
        let text = screen_text(&screen(app));
        (text.contains(MORE_ABOVE), text.contains(MORE_BELOW))
    };
    // A short post selected beside a tall one cut by both edges.
    let mut app = tall_between(SMALL, 3 * VIEW);
    app.update(ch('k'));
    app.update(ch('k'));
    assert_eq!(hints(&app), (true, true));
    app.selected_post = Some(ulid(3));
    assert_eq!(hints(&app), (false, false));
    // The tall one selected, but the keys in the composer or the sidebar.
    app.selected_post = Some(ulid(2));
    for focus in [Focus::Composer, Focus::Sidebar] {
        app.focus = focus;
        assert_eq!(hints(&app), (false, false), "{focus:?}");
    }
    // A post that fits the pane exactly is not tall.
    let mut app = tall_between(SMALL, VIEW);
    app.update(ch('k'));
    assert_eq!(selected(&app), Some(2));
    assert_eq!(hints(&app), (false, false));
}

#[test]
fn a_tall_post_scrolled_out_of_sight_has_no_hint_and_k_picks_from_the_screen() {
    // An older post, the tall one, then five short ones: 15 rows below it,
    // more than the pane shows.
    let mut posts = vec![api_post(1, "pc-early", "an older post"), tall(2, 3 * VIEW)];
    posts.extend((3..=7).map(|n| api_post(n, "pc-late", &format!("post {n}"))));
    let mut app = board(SMALL, posts);
    app.update(key(KeyCode::Tab));
    while selected(&app) != Some(2) {
        app.update(ch('k'));
    }
    assert!(screen_text(&screen(&app)).contains(MORE_ABOVE));
    // PgDn to the newest: it stays selected, all of it above the pane.
    app.update(key(KeyCode::PageDown));
    app.update(key(KeyCode::PageDown));
    assert_eq!((selected(&app), app.scroll), (Some(2), 0));
    let text = screen_text(&screen(&app));
    assert!(
        !text.contains(TALL_AUTHOR) && !text.contains("line "),
        "{text}"
    );
    assert!(
        !text.contains(MORE_ABOVE) && !text.contains(MORE_BELOW),
        "{text}"
    );
    // Nor do j and k read on through it out there, or jump back to it: k
    // picks the newest post on screen, as going into the posts does, and
    // the view stays.
    app.update(ch('k'));
    assert_eq!((selected(&app), app.scroll), (Some(7), 0));
}

#[test]
fn after_paging_away_from_the_selection_k_picks_from_the_screen() {
    // Thirty short posts, three rows apart: post n's last row is 3 × (30 -
    // n) rows up. The newest selected, then two pages up: scroll 20, rows
    // 20 to 31 on screen, and post 23 (rows 21-22) the newest there.
    let posts = (1..=30)
        .map(|n| api_post(n, "pc-configs", &format!("post {n}")))
        .collect();
    let mut app = board(SMALL, posts);
    app.update(key(KeyCode::Tab));
    assert_eq!((selected(&app), app.scroll), (Some(30), 0));
    app.update(key(KeyCode::PageUp));
    app.update(key(KeyCode::PageUp));
    assert_eq!((selected(&app), app.scroll), (Some(30), 20));
    assert!(app.update(ch('k')).is_empty());
    assert_eq!((selected(&app), app.scroll), (Some(23), 20), "no jump back");
    // From there, k moves on as ever.
    app.update(ch('k'));
    assert_eq!((selected(&app), app.scroll), (Some(22), 20));

    // The same with j, after paging back down past the selection.
    app.update(key(KeyCode::PageDown));
    app.update(key(KeyCode::PageDown));
    assert_eq!(app.scroll, 0);
    app.update(key(KeyCode::PageUp));
    app.update(key(KeyCode::PageUp));
    app.update(key(KeyCode::PageUp));
    // Scroll 30: rows 30 to 41, post 22 (rows 24-25) is below them.
    assert_eq!(app.scroll, 30);
    app.update(ch('j'));
    // Post 20 (rows 30-31) is the newest on screen.
    assert_eq!((selected(&app), app.scroll), (Some(20), 30));
}
