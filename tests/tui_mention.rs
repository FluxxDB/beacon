//! The composer's `@` ghost autocomplete: an `@` that starts a word, and the
//! start of a handle after it, show the rest of the best match after the
//! caret in a dim ink. `Tab` takes it (with a space), `↑` `↓` pick another
//! match, `Esc` puts it away for that word. Without a ghost every key does
//! what it always did.

use beacon::tui::state::{AgentItem, App, ChannelItem, Cmd, Focus, Msg, PostItem, Selection};
use beacon::tui::theme::{role, ColorMode, Rgb, Theme};
use beacon::tui::view::{self, Layout};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use jiff::tz::TimeZone;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::Terminal;

const W: u16 = 104;
const H: u16 = 33;
/// 2026-10-22, a little after 6 pm in New York.
const NOW: i64 = 1_792_620_000_000;
const MIN: i64 = 60_000;
const TRUECOLOR: Theme = Theme {
    mode: ColorMode::TrueColor,
};

// ---------------------------------------------------------------- helpers

fn agent(handle: &str, ago_ms: i64, active: bool) -> AgentItem {
    AgentItem {
        handle: handle.to_string(),
        client: "claude-code".to_string(),
        node: "pc".to_string(),
        status: None,
        last_seen_ms: NOW - ago_ms,
        active,
    }
}

fn post(n: u32, author: &str, ago_ms: i64) -> PostItem {
    PostItem {
        id: format!("01JAN{n:021}"),
        channel: "beacon".to_string(),
        ts_ms: NOW - ago_ms,
        author: author.to_string(),
        announcement: author == "Dang",
        client: None,
        node: "pc".to_string(),
        body: format!("post {n}"),
        tags: Vec::new(),
        mentions: Vec::new(),
        attachment: None,
    }
}

/// Dang typing in #beacon. The sidebar's agents are out of order on purpose
/// (the ghost sorts them itself), and two of the posts' authors are not in
/// the sidebar.
fn typing() -> App {
    let mut app = App::new("pc", "Dang", TimeZone::UTC);
    app.now_ms = NOW;
    app.size = (W, H);
    app.channels = vec![ChannelItem {
        name: "beacon".to_string(),
        topic: None,
        unread: 0,
    }];
    app.selected = Selection::Channel("beacon".to_string());
    app.view_chosen = true;
    app.agents = vec![
        agent("pc-vllm", 14 * MIN, false),
        agent("pc-configs", 5_000, true),
        agent("mac-juicebox", 70_000, true),
    ];
    // Oldest first, as the posts pane keeps them.
    app.posts = vec![
        post(1, "mac-old", 50 * MIN),
        post(2, "Dang", 40 * MIN),
        post(3, "pc-configs", 30 * MIN),
        post(4, "nas-backup", 20 * MIN),
    ];
    app.focus = Focus::Composer;
    app
}

fn key(code: KeyCode) -> Msg {
    Msg::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        app.update(key(KeyCode::Char(c)));
    }
}

/// The handle the ghost offers, if one shows.
fn offered(app: &App) -> Option<String> {
    app.ghost().map(|g| g.handle)
}

fn screen(app: &App) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(W, H)).unwrap();
    let frame = terminal
        .draw(|f| view::render(f, app, &TRUECOLOR))
        .expect("draw");
    frame.buffer.clone()
}

/// The composer box, borders included.
fn composer(app: &App) -> Rect {
    Layout::compute(Rect::new(0, 0, W, H), app)
        .expect("layout")
        .composer
}

fn row(buf: &Buffer, y: u16, x0: u16, x1: u16) -> String {
    (x0..x1).map(|x| buf[(x, y)].symbol()).collect()
}

fn color(c: Rgb) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

// ------------------------------------------------------------------ trigger

#[test]
fn an_at_that_starts_a_word_shows_a_ghost() {
    for typed in ["@", "@p", "hi @p", "(@p", "two words\n@p", "tab\t@p"] {
        let mut app = typing();
        app.update(Msg::Paste(typed.to_string()));
        assert!(app.ghost().is_some(), "{typed:?} shows a ghost");
    }
    // Mid-word (an address, `a@b`), after punctuation, or once the handle
    // has ended: no ghost.
    for typed in [
        "",
        "a@b",
        "mail@pc",
        "x:@pc",
        "@@pc",
        "@pc ",
        "@pc,",
        "@pc-configs!",
    ] {
        let mut app = typing();
        app.update(Msg::Paste(typed.to_string()));
        assert_eq!(app.ghost(), None, "{typed:?} shows no ghost");
    }
}

#[test]
fn candidates_are_agents_then_authors_then_all_but_never_me() {
    let mut app = typing();
    type_text(&mut app, "@");
    // A bare `@` offers the first candidate whole.
    let ghost = app.ghost().expect("a ghost");
    assert_eq!(
        (ghost.handle.as_str(), ghost.rest.as_str(), ghost.index),
        ("pc-configs", "pc-configs", 0)
    );
    let mut order = Vec::new();
    for _ in 0..ghost.count {
        order.push(offered(&app).unwrap());
        app.update(key(KeyCode::Down));
    }
    // Active agents, then idle ones, each most recently seen first; then
    // the posts' other authors, newest post first; then everyone. Dang
    // (BEACON_HUMAN) is not offered to himself, and pc-configs only once.
    assert_eq!(
        order,
        [
            "pc-configs",
            "mac-juicebox",
            "pc-vllm",
            "nas-backup",
            "mac-old",
            "all"
        ]
    );
    assert_eq!(offered(&app).as_deref(), Some("pc-configs"), "round again");
}

#[test]
fn matching_ignores_case() {
    let mut app = typing();
    type_text(&mut app, "@PC-V");
    let ghost = app.ghost().expect("a ghost");
    assert_eq!(
        (ghost.handle.as_str(), ghost.rest.as_str(), ghost.count),
        ("pc-vllm", "llm", 1)
    );

    let mut app = typing();
    app.agents.push(agent("Mac-Studio", 2 * MIN, true));
    type_text(&mut app, "@mac-s");
    let ghost = app.ghost().expect("a ghost");
    assert_eq!(
        (ghost.handle.as_str(), ghost.rest.as_str()),
        ("Mac-Studio", "tudio")
    );
}

// --------------------------------------------------------------------- keys

#[test]
fn tab_takes_the_ghost_and_a_space() {
    let mut app = typing();
    type_text(&mut app, "hey @mac");
    assert!(app.update(key(KeyCode::Tab)).is_empty());
    assert_eq!(app.input, "hey @mac-juicebox ");
    assert_eq!(app.focus, Focus::Composer, "tab did not move the focus");
    assert_eq!(app.ghost(), None);
    type_text(&mut app, "ping");
    assert_eq!(app.input, "hey @mac-juicebox ping");

    // A handle typed out whole still takes its space.
    let mut app = typing();
    type_text(&mut app, "@all");
    assert_eq!(app.ghost().map(|g| g.rest), Some(String::new()));
    app.update(key(KeyCode::Tab));
    assert_eq!((app.input.as_str(), app.focus), ("@all ", Focus::Composer));
}

#[test]
fn up_and_down_pick_another_match() {
    let mut app = typing();
    type_text(&mut app, "@pc");
    let pick = |app: &App| {
        let g = app.ghost().expect("a ghost");
        (g.handle, g.index, g.count)
    };
    assert_eq!(pick(&app), ("pc-configs".to_string(), 0, 2));
    app.update(key(KeyCode::Down));
    assert_eq!(pick(&app), ("pc-vllm".to_string(), 1, 2));
    app.update(key(KeyCode::Down));
    assert_eq!(pick(&app), ("pc-configs".to_string(), 0, 2), "wraps");
    app.update(key(KeyCode::Up));
    assert_eq!(pick(&app), ("pc-vllm".to_string(), 1, 2), "wraps back");
    assert_eq!(
        (app.focus, app.selected_post.as_ref()),
        (Focus::Composer, None),
        "the arrows moved nothing else"
    );
    app.update(key(KeyCode::Tab));
    assert_eq!(app.input, "@pc-vllm ");

    // Typing more starts again from the best match.
    let mut app = typing();
    type_text(&mut app, "@");
    app.update(key(KeyCode::Down));
    assert_eq!(offered(&app).as_deref(), Some("mac-juicebox"));
    type_text(&mut app, "p");
    assert_eq!(pick(&app), ("pc-configs".to_string(), 0, 2));
}

#[test]
fn esc_puts_the_ghost_away_for_this_word() {
    let mut app = typing();
    type_text(&mut app, "hey @pc");
    assert!(app.update(key(KeyCode::Esc)).is_empty());
    assert_eq!(app.ghost(), None);
    assert_eq!(app.focus, Focus::Composer, "esc did not leave the composer");
    type_text(&mut app, "-");
    assert_eq!(app.ghost(), None, "still the same word");
    // With no ghost, Tab moves the focus again.
    app.update(key(KeyCode::Tab));
    assert_eq!((app.focus, app.input.as_str()), (Focus::Posts, "hey @pc-"));

    // The next `@` brings it back.
    app.focus = Focus::Composer;
    type_text(&mut app, " and @");
    assert_eq!(offered(&app).as_deref(), Some("pc-configs"));

    // So does the same `@`, deleted and typed again.
    let mut app = typing();
    type_text(&mut app, "@pc");
    app.update(key(KeyCode::Esc));
    for _ in 0..3 {
        app.update(key(KeyCode::Backspace));
    }
    type_text(&mut app, "@");
    assert_eq!(offered(&app).as_deref(), Some("pc-configs"));

    // Esc without a ghost is the composer's own again: with text it arms a
    // clear, then clears (tests/tui_composer.rs), and leaves an empty one.
    app.update(key(KeyCode::Esc));
    assert_eq!(app.focus, Focus::Composer);
    app.update(key(KeyCode::Esc));
    assert_eq!((app.focus, app.input.as_str()), (Focus::Composer, "@"));
    app.update(key(KeyCode::Esc));
    assert_eq!((app.focus, app.input.as_str()), (Focus::Composer, ""));
    app.update(key(KeyCode::Esc));
    assert_eq!(app.focus, Focus::Posts);
}

#[test]
fn no_match_means_no_ghost_and_the_keys_do_what_they_did() {
    let mut app = typing();
    type_text(&mut app, "@zz");
    assert_eq!(app.ghost(), None);
    let buf = screen(&app);
    let c = composer(&app);
    let hint = row(&buf, c.y, c.x, c.right());
    assert!(
        hint.contains("enter send") && !hint.contains("tab complete"),
        "{hint}"
    );
    app.update(key(KeyCode::Down));
    app.update(key(KeyCode::Up));
    assert_eq!(app.input, "@zz");
    app.update(key(KeyCode::Tab));
    assert_eq!((app.focus, app.input.as_str()), (Focus::Posts, "@zz"));

    // Only Dang's own name starts with a d: nothing to offer.
    let mut app = typing();
    type_text(&mut app, "@D");
    assert_eq!(app.ghost(), None);

    // Nobody known at all: still `all`; but nothing else.
    let mut app = typing();
    app.agents.clear();
    app.posts.clear();
    type_text(&mut app, "@");
    let ghost = app.ghost().expect("a ghost");
    assert_eq!((ghost.handle.as_str(), ghost.count), ("all", 1));

    // Outside the composer there is no ghost: Tab moves on.
    let mut app = typing();
    type_text(&mut app, "@pc");
    app.focus = Focus::Posts;
    assert_eq!(app.ghost(), None);
    app.update(key(KeyCode::Tab));
    assert_eq!((app.focus, app.input.as_str()), (Focus::Composer, "@pc"));
}

#[test]
fn enter_sends_only_what_was_typed() {
    let mut app = typing();
    type_text(&mut app, "ping @mac");
    assert!(app.ghost().is_some());
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        [Cmd::SendPost {
            channel: "beacon".to_string(),
            body: "ping @mac".to_string(),
        }]
    );
}

// ------------------------------------------------------------------- render

#[test]
fn the_ghost_is_dim_after_the_caret_and_the_hint_says_tab() {
    let mut app = typing();
    let typed = "hey @pc-c";
    type_text(&mut app, typed);
    let buf = screen(&app);
    let c = composer(&app);
    let y = c.y + 1;
    let caret = c.x + 4 + typed.len() as u16;
    assert_eq!(row(&buf, y, c.x + 4, caret), typed);
    // The caret stays where the typing ends, on the ghost's first letter.
    assert_eq!(buf[(caret, y)].symbol(), "o");
    assert_eq!(buf[(caret, y)].bg, color(role::ACCENT));
    // The rest is fog on the panel, never the accent.
    assert_eq!(row(&buf, y, caret + 1, caret + 7), "nfigs ");
    for x in caret + 1..caret + 6 {
        let cell = &buf[(x, y)];
        assert_eq!(
            (cell.fg, cell.bg),
            (color(role::META), color(role::MAIN_BG)),
            "ghost cell {x}"
        );
        assert_ne!(cell.fg, color(role::ACCENT));
    }
    // The hint names the key; one match needs no position.
    let hint = row(&buf, c.y, c.x, c.right());
    assert!(hint.contains(" tab complete "), "{hint}");
    assert!(
        !hint.contains("enter send") && !hint.contains("1/"),
        "{hint}"
    );

    // Two matches: where this one is among them, and how to move.
    app.update(key(KeyCode::Backspace));
    app.update(key(KeyCode::Backspace));
    let hint = row(&screen(&app), c.y, c.x, c.right());
    assert!(hint.contains(" tab complete · ↑↓ 1/2 "), "{hint}");
    app.update(key(KeyCode::Down));
    let hint = row(&screen(&app), c.y, c.x, c.right());
    assert!(hint.contains(" tab complete · ↑↓ 2/2 "), "{hint}");
}

#[test]
fn the_ghost_never_moves_the_text_or_the_caret() {
    // The typing ends three cells before the edge, so `onfigs` cannot fit.
    let c = composer(&typing());
    let text_cells = usize::from(c.width - 6);
    let typed = format!("{} @pc-c", "x".repeat(text_cells - 3 - 6));
    let mut with = typing();
    with.update(Msg::Paste(typed.clone()));
    let mut without = with.clone();
    without.update(key(KeyCode::Esc));
    assert!(with.ghost().is_some() && without.ghost().is_none());
    assert_eq!(composer(&with), composer(&without), "same box");

    let (a, b) = (screen(&with), screen(&without));
    let y = c.y + 1;
    let text_x = c.x + 4;
    let caret = text_x + typed.len() as u16;
    for row_y in (0..H).filter(|&r| r != c.y && r != y) {
        for x in 0..W {
            assert_eq!(a[(x, row_y)], b[(x, row_y)], "({x},{row_y})");
        }
    }
    for x in 0..caret {
        assert_eq!(a[(x, y)], b[(x, y)], "typed text at ({x},{y})");
    }
    assert_eq!(a[(caret, y)].bg, color(role::ACCENT), "the caret stays");
    assert_eq!(b[(caret, y)].bg, color(role::ACCENT));
    // Cut at the edge: the padding and the border are untouched.
    let edge = text_x + c.width - 6;
    assert_eq!(row(&a, y, caret, edge), "onf");
    assert_eq!(row(&a, y, edge, edge + 2), " │");
    assert_eq!(a[(edge, y)], b[(edge, y)]);
}
