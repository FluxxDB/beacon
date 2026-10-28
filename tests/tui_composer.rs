//! The composer's cursor (Dang, PV0QMR51: "type out "()" then hit left arrow
//! key to get into the parenthesis"): keys move it a grapheme, a word, to a
//! line's ends and between rows; typing, paste, Backspace and Delete work
//! where it is; the block caret is drawn on the cell under it and the box
//! scrolls to keep it in view; the `@` ghost reads the text before it.
//! And Esc twice clears it (Dang, KW1WZ5B5).

use beacon::tui::state::{AgentItem, App, ChannelItem, Cmd, Focus, Msg, Selection};
use beacon::tui::theme::{palette, role, ColorMode, Rgb, Theme};
use beacon::tui::view::{self, Layout};
use beacon::tui::wrap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use jiff::tz::TimeZone;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::Terminal;

const W: u16 = 104;
const H: u16 = 33;
const TRUECOLOR: Theme = Theme {
    mode: ColorMode::TrueColor,
};

// ------------------------------------------------------------------ helpers

/// Dang in the composer of #beacon, with pc-configs about for the ghost.
fn composing() -> App {
    let mut app = App::new("pc", "Dang", TimeZone::UTC);
    app.update(Msg::Resize(W, H));
    app.channels = vec![ChannelItem {
        name: "beacon".to_string(),
        topic: None,
        unread: 0,
        read_to: 0,
    }];
    app.selected = Selection::Channel("beacon".to_string());
    app.view_chosen = true;
    app.agents = vec![AgentItem {
        handle: "pc-configs".to_string(),
        client: "claude-code".to_string(),
        node: "pc".to_string(),
        status: None,
        last_seen_ms: 0,
        active: true,
    }];
    app.focus = Focus::Composer;
    app
}

fn chord(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Vec<Cmd> {
    app.update(Msg::Key(KeyEvent::new(code, modifiers)))
}

fn press(app: &mut App, code: KeyCode) -> Vec<Cmd> {
    chord(app, code, KeyModifiers::NONE)
}

fn presses(app: &mut App, code: KeyCode, times: usize) {
    for _ in 0..times {
        press(app, code);
    }
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c));
    }
}

fn paste(app: &mut App, text: &str) {
    app.update(Msg::Paste(text.to_string()));
}

/// Where the cursor stops after each of `times` presses of `code`.
fn stops(app: &mut App, code: KeyCode, modifiers: KeyModifiers, times: usize) -> Vec<usize> {
    (0..times)
        .map(|_| {
            chord(app, code, modifiers);
            app.cursor()
        })
        .collect()
}

fn screen(app: &App) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(W, H)).unwrap();
    let frame = terminal
        .draw(|f| view::render(f, app, &TRUECOLOR))
        .expect("draw");
    frame.buffer.clone()
}

/// The composer box, borders included: `│ › text… │`, so its text starts
/// four cells in and ends two cells before its right edge.
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

/// The node's answer to Dang's post of `body` in #beacon: stored.
fn sent(body: &str) -> Msg {
    let created: beacon::api::PostCreated = serde_json::from_value(serde_json::json!({
        "v": 1,
        "post": {
            "v": 1, "id": "01JAN000000000000000000001", "short_id": "00000001",
            "channel": "beacon", "ts": "", "ts_ms": 0, "author": "Dang",
            "author_kind": "human", "client": "tui", "node": "pc", "body": body,
            "tags": [], "mentions": [], "announcement": true, "attachment": null,
            "arrival": 1
        },
        "delivery": []
    }))
    .unwrap();
    Msg::Sent(Ok(created))
}

/// Every cell inside the composer box drawn as the caret (on the accent).
fn caret_cells(app: &App, buf: &Buffer) -> Vec<(u16, u16)> {
    let c = composer(app);
    (c.y + 1..c.bottom() - 1)
        .flat_map(|y| (c.x + 1..c.right() - 1).map(move |x| (x, y)))
        .filter(|&(x, y)| buf[(x, y)].bg == color(role::ACCENT))
        .collect()
}

// ------------------------------------------------------------------ wrapping

/// The rows the composer draws, and where the cursor is on them, come from
/// knowing which bytes of the input each wrapped line shows.
#[test]
fn wrap_ranges_say_which_bytes_each_line_shows() {
    let cases: [(&str, u16, Vec<std::ops::Range<usize>>); 7] = [
        ("hello world", 5, vec![0..5, 6..11]),
        // The spaces at a soft break are left out, however many.
        ("aaa   bbb", 4, vec![0..3, 6..9]),
        // An empty paragraph is an empty line where it is.
        ("a b\n\nc", 10, vec![0..3, 4..4, 5..6]),
        // An empty last line sits at its paragraph's end.
        ("aaaa ", 4, vec![0..4, 5..5]),
        // Indentation before a word too long for the line is dropped.
        ("   bbbbbbbbbb", 4, vec![3..7, 7..11, 11..13]),
        // Indentation that fits stays.
        ("  hi there", 8, vec![0..4, 5..10]),
        ("日本語", 4, vec![0..6, 6..9]),
    ];
    for (text, width, want) in cases {
        assert_eq!(wrap::wrap_ranges(text, width), want, "{text:?} at {width}");
    }

    // Nothing between the lines but the spaces of a soft break, or the `\n`
    // of a hard one (and any indentation dropped after it). (What `wrap`
    // itself makes is pinned in tests/tui_render.rs.)
    let texts = [
        "on my way, tagging after the PC smoke test",
        "one\n\n  two  three   four\nfive ",
        "a  b   c    d     e",
        "x 日本語のテキスト 🚀🚀🚀 👨‍👩‍👧‍👦👨‍👩‍👧‍👦 e\u{301}e\u{301}",
        "https://example.com/segment-0123456789/segment-0123456789",
        " \n \n",
        "",
    ];
    for text in texts {
        for width in [1u16, 2, 3, 5, 8, 13, 80] {
            let ranges = wrap::wrap_ranges(text, width);
            let mut end = 0;
            for range in &ranges {
                // Within the width (a lone wide grapheme may take two).
                let line = &text[range.clone()];
                assert!(
                    wrap::display_width(line) <= width.max(2),
                    "{text:?} at {width}: {line:?}"
                );
                let gap = &text[end..range.start];
                let soft = gap.chars().all(|c| c == ' ');
                let hard = gap
                    .strip_prefix('\n')
                    .is_some_and(|rest| rest.chars().all(|c| c == ' '));
                assert!(soft || hard, "{text:?} at {width}: gap {gap:?}");
                end = range.end;
            }
            assert!(text[end..].chars().all(|c| c == ' '), "{text:?} at {width}");
        }
    }
}

// ---------------------------------------------------------------- moving

#[test]
fn left_steps_into_the_parentheses() {
    let mut app = composing();
    type_text(&mut app, "()");
    press(&mut app, KeyCode::Left);
    type_text(&mut app, "x");
    assert_eq!((app.input.as_str(), app.cursor()), ("(x)", 2));
    assert_eq!(
        stops(&mut app, KeyCode::Right, KeyModifiers::NONE, 2),
        [3, 3]
    );
    assert_eq!(
        stops(&mut app, KeyCode::Left, KeyModifiers::NONE, 4),
        [2, 1, 0, 0],
        "not before the start"
    );
    // Shift-arrows (there is no selection) move all the same.
    assert_eq!(stops(&mut app, KeyCode::Right, KeyModifiers::SHIFT, 1), [1]);
    assert_eq!(app.input, "(x)", "moving types nothing");
}

#[test]
fn left_and_right_step_over_whole_graphemes() {
    // `a`, a family emoji (four people joined by three ZWJs: 25 bytes), an
    // `e` with a combining acute (3), `日` (3, two cells), `b`.
    let text = "a\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}e\u{301}日b";
    assert_eq!(text.len(), 33);
    let mut app = composing();
    paste(&mut app, text);
    assert_eq!(app.cursor(), 33, "a paste leaves the cursor after it");
    assert_eq!(
        stops(&mut app, KeyCode::Left, KeyModifiers::NONE, 6),
        [32, 29, 26, 1, 0, 0]
    );
    assert_eq!(
        stops(&mut app, KeyCode::Right, KeyModifiers::NONE, 6),
        [1, 26, 29, 32, 33, 33]
    );
    // Never inside one: a place in the middle of the family is after it.
    for inside in [2, 5, 25] {
        app.set_cursor(inside);
        assert_eq!(app.cursor(), 26, "{inside}");
    }
}

#[test]
fn home_end_ctrl_a_and_ctrl_e_go_to_the_ends_of_the_line() {
    // "first line" 0..10, `\n`, "second line" 11..22, `\n`, "third" 23..28.
    let text = "first line\nsecond line\nthird";
    let mut app = composing();
    paste(&mut app, text);
    let keys = [
        (KeyCode::Home, KeyCode::End, KeyModifiers::NONE),
        (
            KeyCode::Char('a'),
            KeyCode::Char('e'),
            KeyModifiers::CONTROL,
        ),
    ];
    for (start, end, mods) in keys {
        app.set_cursor(15);
        assert_eq!(stops(&mut app, start, mods, 2), [11, 11], "{start:?}");
        assert_eq!(stops(&mut app, end, mods, 2), [22, 22], "{end:?}");
        app.set_cursor(3);
        assert_eq!(stops(&mut app, end, mods, 1), [10]);
        assert_eq!(stops(&mut app, start, mods, 1), [0]);
        app.set_cursor(25);
        assert_eq!(stops(&mut app, end, mods, 1), [28]);
        assert_eq!(stops(&mut app, start, mods, 1), [23]);
    }
    assert_eq!(app.input, text, "ctrl-a and ctrl-e type nothing");
}

#[test]
fn ctrl_and_alt_arrows_jump_by_word() {
    // Words: hey 0..3, pc 6..8, configs 9..16, done 19..23.
    let text = "hey (@pc-configs), done";
    let mut app = composing();
    paste(&mut app, text);
    let jumps = [
        (KeyCode::Left, KeyCode::Right, KeyModifiers::CONTROL),
        (KeyCode::Left, KeyCode::Right, KeyModifiers::ALT),
        // What macOS terminals send for Option-arrows (readline's M-b M-f).
        (KeyCode::Char('b'), KeyCode::Char('f'), KeyModifiers::ALT),
    ];
    for (back, forward, mods) in jumps {
        app.set_cursor(23);
        assert_eq!(
            stops(&mut app, back, mods, 5),
            [19, 9, 6, 0, 0],
            "{mods:?} {back:?}"
        );
        assert_eq!(
            stops(&mut app, forward, mods, 5),
            [3, 8, 16, 23, 23],
            "{mods:?} {forward:?}"
        );
    }
    assert_eq!(app.input, text);
}

#[test]
fn up_and_down_move_between_lines_keeping_the_column() {
    // Lines: "abcdefgh" 0..8, "ab" 9..11, "abcdefgh" 12..20.
    let mut app = composing();
    paste(&mut app, "abcdefgh\nab\nabcdefgh");
    app.set_cursor(6);
    assert_eq!(
        stops(&mut app, KeyCode::Down, KeyModifiers::NONE, 2),
        // The short line's end, then back out to column 6.
        [11, 18]
    );
    assert_eq!(
        stops(&mut app, KeyCode::Up, KeyModifiers::NONE, 2),
        [11, 6],
        "and back"
    );
    // Any other key forgets the column.
    app.set_cursor(18);
    press(&mut app, KeyCode::Up);
    press(&mut app, KeyCode::Left);
    assert_eq!(app.cursor(), 10);
    assert_eq!(stops(&mut app, KeyCode::Down, KeyModifiers::NONE, 1), [13]);
}

#[test]
fn up_and_down_move_between_wrapped_rows() {
    let mut app = composing();
    let width = usize::from(composer(&app).width - 6);
    // Row 0: width-1 a's (the space after them is dropped at the break);
    // row 1: thirty b's.
    let text = format!("{} {}", "a".repeat(width - 1), "b".repeat(30));
    paste(&mut app, &text);
    assert_eq!(app.cursor(), width + 30);
    assert_eq!(stops(&mut app, KeyCode::Up, KeyModifiers::NONE, 1), [30]);
    assert_eq!(
        stops(&mut app, KeyCode::Down, KeyModifiers::NONE, 1),
        [width + 30]
    );

    // A column inside a wide character lands before it.
    let mut app = composing();
    // "日本語" 0..9 in cells 0-1, 2-3, 4-5; "abcdef" 10..16.
    paste(&mut app, "日本語\nabcdef");
    app.set_cursor(13);
    assert_eq!(stops(&mut app, KeyCode::Up, KeyModifiers::NONE, 1), [3]);
    assert_eq!(stops(&mut app, KeyCode::Down, KeyModifiers::NONE, 1), [13]);
}

#[test]
fn down_on_the_last_row_goes_to_the_end_and_up_on_the_first_to_the_start() {
    // Dang (R79KEKG2), as a macOS text box does it. A one-row draft too:
    // the keys stay in the composer.
    let mut app = composing();
    paste(&mut app, "hello");
    app.set_cursor(2);
    assert!(press(&mut app, KeyCode::Down).is_empty());
    assert_eq!(
        (app.input.as_str(), app.cursor(), app.focus),
        ("hello", 5, Focus::Composer)
    );
    app.set_cursor(2);
    assert!(press(&mut app, KeyCode::Up).is_empty());
    assert_eq!(
        (app.input.as_str(), app.cursor(), app.focus),
        ("hello", 0, Focus::Composer)
    );

    // Lines: "abcdefgh" 0..8, "ab" 9..11, "abcdefgh" 12..20.
    let mut app = composing();
    paste(&mut app, "abcdefgh\nab\nabcdefgh");
    app.set_cursor(14);
    assert_eq!(
        stops(&mut app, KeyCode::Down, KeyModifiers::NONE, 2),
        [20, 20]
    );
    app.set_cursor(2);
    assert_eq!(stops(&mut app, KeyCode::Up, KeyModifiers::NONE, 2), [0, 0]);

    // The jump forgets the column: from the end (column 8) up, the rows
    // aim for column 8, not the 6 the run started from.
    app.set_cursor(6);
    assert_eq!(
        stops(&mut app, KeyCode::Down, KeyModifiers::NONE, 3),
        [11, 18, 20]
    );
    assert_eq!(stops(&mut app, KeyCode::Up, KeyModifiers::NONE, 2), [11, 8]);

    // Wrapped rows count as rows: the last one's ↓ goes to the end.
    let mut app = composing();
    let width = usize::from(composer(&app).width - 6);
    // Row 0: width-1 a's (the space after them is dropped at the break);
    // row 1: thirty b's.
    let text = format!("{} {}", "a".repeat(width - 1), "b".repeat(30));
    paste(&mut app, &text);
    app.set_cursor(width + 5);
    assert_eq!(
        stops(&mut app, KeyCode::Down, KeyModifiers::NONE, 1),
        [width + 30]
    );
    app.set_cursor(5);
    assert_eq!(stops(&mut app, KeyCode::Up, KeyModifiers::NONE, 1), [0]);
}

// ----------------------------------------------------------------- editing

#[test]
fn typing_paste_backspace_and_delete_work_at_the_cursor() {
    let mut app = composing();
    type_text(&mut app, "()");
    press(&mut app, KeyCode::Left);
    type_text(&mut app, "x");
    paste(&mut app, "yz");
    assert_eq!((app.input.as_str(), app.cursor()), ("(xyz)", 4));
    press(&mut app, KeyCode::Backspace);
    assert_eq!((app.input.as_str(), app.cursor()), ("(xy)", 3));
    press(&mut app, KeyCode::Left);
    press(&mut app, KeyCode::Delete);
    assert_eq!((app.input.as_str(), app.cursor()), ("(x)", 2));
    // New lines go in at the cursor too.
    chord(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
    chord(&mut app, KeyCode::Enter, KeyModifiers::ALT);
    chord(&mut app, KeyCode::Char('j'), KeyModifiers::CONTROL);
    assert_eq!((app.input.as_str(), app.cursor()), ("(x\n\n\n)", 5));
    // Nothing to delete past either end.
    app.set_cursor(app.input.len());
    press(&mut app, KeyCode::Delete);
    app.set_cursor(0);
    press(&mut app, KeyCode::Backspace);
    assert_eq!((app.input.as_str(), app.cursor()), ("(x\n\n\n)", 0));

    // Whole graphemes go.
    let mut app = composing();
    paste(
        &mut app,
        "a\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}e\u{301}b",
    );
    app.set_cursor(1);
    press(&mut app, KeyCode::Delete);
    assert_eq!((app.input.as_str(), app.cursor()), ("ae\u{301}b", 1));
    app.set_cursor(4);
    press(&mut app, KeyCode::Backspace);
    assert_eq!((app.input.as_str(), app.cursor()), ("ab", 1));
}

#[test]
fn the_cursor_stays_put_when_the_sent_text_leaves_the_front() {
    let mut app = composing();
    type_text(&mut app, "first");
    assert_eq!(press(&mut app, KeyCode::Enter).len(), 1);
    type_text(&mut app, " and more");
    presses(&mut app, KeyCode::Left, 3);
    app.update(sent("first"));
    // Still before `ore`.
    assert_eq!((app.input.as_str(), app.cursor()), (" and more", 6));
}

#[test]
fn moving_and_drawing_never_panic_wherever_the_cursor_is() {
    let texts = [
        "a\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466} e\u{301}日本語\ttab",
        "   indented and a-very-long-word-that-will-not-fit-on-any-narrow-row",
        "@pc ends\n\n\nfull",
        "x".repeat(120).as_str(),
        "a b  c   d    e     f      g       h        i",
    ]
    .map(str::to_string);
    let carets = |app: &App, width: u16, height: u16| {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let frame = terminal
            .draw(|f| view::render(f, app, &TRUECOLOR))
            .expect("draw");
        frame
            .buffer
            .content()
            .iter()
            .filter(|c| c.bg == color(role::ACCENT))
            .count()
    };
    let keys = [
        (KeyCode::Left, KeyModifiers::NONE),
        (KeyCode::Right, KeyModifiers::NONE),
        (KeyCode::Up, KeyModifiers::NONE),
        (KeyCode::Down, KeyModifiers::NONE),
        (KeyCode::Left, KeyModifiers::CONTROL),
        (KeyCode::Right, KeyModifiers::ALT),
        (KeyCode::Home, KeyModifiers::NONE),
        (KeyCode::End, KeyModifiers::NONE),
    ];
    for text in &texts {
        for (width, height) in [(40u16, 13u16), (61, 20), (104, 33)] {
            let mut base = composing();
            base.update(Msg::Resize(width, height));
            paste(&mut base, text);
            for at in 0..=text.len() {
                let mut app = base.clone();
                app.set_cursor(at);
                let label = format!("{text:?} at {at} in {width}x{height}");
                // One caret, a wide one at most two cells.
                assert!((1..=2).contains(&carets(&app, width, height)), "{label}");
                for &(code, mods) in &keys {
                    chord(&mut app, code, mods);
                    let cursor = app.cursor();
                    assert!(app.input.is_char_boundary(cursor), "{label} {code:?}");
                }
                assert!((1..=2).contains(&carets(&app, width, height)), "{label}");
            }
        }
    }
}

#[test]
fn enter_sends_the_whole_input_wherever_the_cursor_is() {
    let mut app = composing();
    type_text(&mut app, "hello world");
    presses(&mut app, KeyCode::Left, 6);
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        [Cmd::SendPost {
            channel: "beacon".to_string(),
            body: "hello world".to_string(),
        }]
    );
}

// ------------------------------------------------------------------ render

#[test]
fn the_caret_is_drawn_on_the_cell_under_the_cursor() {
    let mut app = composing();
    type_text(&mut app, "(x)");
    presses(&mut app, KeyCode::Left, 2);
    let c = composer(&app);
    let (x, y) = (c.x + 4, c.y + 1);
    let buf = screen(&app);
    assert_eq!(row(&buf, y, x, x + 4), "(x) ");
    assert_eq!(caret_cells(&app, &buf), [(x + 1, y)]);
    let caret = &buf[(x + 1, y)];
    assert_eq!(
        (caret.symbol(), caret.fg, caret.bg),
        ("x", color(palette::VOID), color(role::ACCENT))
    );
    for x in [x, x + 2] {
        let cell = &buf[(x, y)];
        assert_eq!(
            (cell.fg, cell.bg),
            (color(role::TEXT), color(role::MAIN_BG))
        );
    }

    // At the end: on a space after the last character.
    press(&mut app, KeyCode::End);
    let buf = screen(&app);
    assert_eq!(caret_cells(&app, &buf), [(x + 3, y)]);
    assert_eq!(buf[(x + 3, y)].symbol(), " ");

    // A wide character under the cursor: the caret covers both its cells.
    let mut app = composing();
    paste(&mut app, "日本");
    press(&mut app, KeyCode::Left);
    let buf = screen(&app);
    assert_eq!(caret_cells(&app, &buf), [(x + 2, y), (x + 3, y)]);
    assert_eq!(buf[(x + 2, y)].symbol(), "本");

    // Another line: the caret is on its row, at its column; at the end of
    // a line, on a space after it.
    let mut app = composing();
    paste(&mut app, "ab\ncd");
    let c = composer(&app);
    let (x, y) = (c.x + 4, c.y + 1);
    app.set_cursor(4);
    let buf = screen(&app);
    assert_eq!(caret_cells(&app, &buf), [(x + 1, y + 1)]);
    assert_eq!(buf[(x + 1, y + 1)].symbol(), "d");
    app.set_cursor(2);
    let buf = screen(&app);
    assert_eq!(caret_cells(&app, &buf), [(x + 2, y)]);
    assert_eq!(row(&buf, y, x, x + 3), "ab ");

    // Out of the composer, no caret.
    app.focus = Focus::Posts;
    assert_eq!(caret_cells(&app, &screen(&app)), []);
}

#[test]
fn the_box_scrolls_only_as_far_as_it_must_to_keep_the_caret_in_view() {
    let mut app = composing();
    paste(&mut app, "1\n2\n3\n4\n5\n6\n7\n8");
    let c = composer(&app);
    assert_eq!(c.height, 7, "five rows of text at most");
    // The rows the box shows, and the one the caret is on (by the line's
    // only character).
    let view = |app: &App| {
        let buf = screen(app);
        let shown: String = (c.y + 1..c.bottom() - 1)
            .map(|y| row(&buf, y, c.x + 4, c.x + 5))
            .collect();
        let carets = caret_cells(app, &buf);
        assert_eq!(carets.len(), 1, "one caret, in view: {shown}");
        let (_, y) = carets[0];
        (shown, row(&buf, y, c.x + 4, c.x + 5))
    };
    let seen = |shown: &str, caret: &str| (shown.to_string(), caret.to_string());
    assert_eq!(view(&app), seen("45678", "8"));
    presses(&mut app, KeyCode::Up, 4);
    assert_eq!(view(&app), seen("45678", "4"), "no scrolling yet");
    press(&mut app, KeyCode::Up);
    assert_eq!(view(&app), seen("34567", "3"));
    // Back down, the caret moves within the box before the text scrolls.
    press(&mut app, KeyCode::Down);
    assert_eq!(view(&app), seen("34567", "4"));
    presses(&mut app, KeyCode::Down, 4);
    assert_eq!(view(&app), seen("45678", "8"));
    presses(&mut app, KeyCode::Up, 7);
    assert_eq!(view(&app), seen("12345", "1"));
}

// ------------------------------------------------------------------- ghost

#[test]
fn the_ghost_reads_the_text_before_the_cursor() {
    let mut app = composing();
    paste(&mut app, "hey @pc and more");
    assert_eq!(app.ghost(), None, "the input does not end in a mention");
    // Back to just after `@pc`, where a space follows: the ghost shows.
    presses(&mut app, KeyCode::Left, 9);
    assert_eq!(app.ghost().map(|g| g.handle).as_deref(), Some("pc-configs"));
    // Inside the word, no ghost.
    press(&mut app, KeyCode::Left);
    assert_eq!(app.ghost(), None);
    press(&mut app, KeyCode::Right);
    assert!(app.ghost().is_some());
    // Nor before anything but whitespace.
    let mut app = composing();
    paste(&mut app, "hey @pc, more");
    presses(&mut app, KeyCode::Left, 6);
    assert_eq!(app.cursor(), 7);
    assert_eq!(app.ghost(), None);
    // A line break counts as whitespace.
    let mut app = composing();
    paste(&mut app, "hey @pc\nmore");
    presses(&mut app, KeyCode::Left, 5);
    assert!(app.ghost().is_some());
}

#[test]
fn with_a_ghost_up_and_down_pick_a_match_and_leave_the_cursor() {
    // On a one-row draft ↑ and ↓ would jump to its ends; with a ghost they
    // are still the ghost's.
    let mut app = composing();
    let mut vllm = app.agents[0].clone();
    vllm.handle = "pc-vllm".to_string();
    app.agents.push(vllm);
    paste(&mut app, "hey @pc and more");
    presses(&mut app, KeyCode::Left, 9);
    assert_eq!(app.cursor(), 7);
    let index = |app: &App| app.ghost().expect("a ghost").index;
    assert_eq!((index(&app), app.ghost().unwrap().count), (0, 2));
    press(&mut app, KeyCode::Down);
    assert_eq!((index(&app), app.cursor()), (1, 7));
    press(&mut app, KeyCode::Up);
    assert_eq!((index(&app), app.cursor()), (0, 7));
    assert_eq!(app.input, "hey @pc and more");
}

#[test]
fn tab_puts_the_ghost_in_at_the_cursor() {
    let mut app = composing();
    paste(&mut app, "hey @pc and more");
    presses(&mut app, KeyCode::Left, 9);
    assert!(press(&mut app, KeyCode::Tab).is_empty());
    // The space is already there: none added, the cursor after the handle.
    assert_eq!(
        (app.input.as_str(), app.cursor(), app.focus),
        ("hey @pc-configs and more", 15, Focus::Composer)
    );
    type_text(&mut app, ",");
    assert_eq!(app.input, "hey @pc-configs, and more");

    // Nor before a line break.
    let mut app = composing();
    paste(&mut app, "hey @pc\nmore");
    app.set_cursor(7);
    press(&mut app, KeyCode::Tab);
    assert_eq!(
        (app.input.as_str(), app.cursor()),
        ("hey @pc-configs\nmore", 15)
    );

    // At the end of the input it still brings its space.
    let mut app = composing();
    type_text(&mut app, "x @pc");
    press(&mut app, KeyCode::Tab);
    assert_eq!((app.input.as_str(), app.cursor()), ("x @pc-configs ", 14));
}

#[test]
fn a_ghost_before_more_text_pushes_the_rest_of_its_row_along() {
    let mut app = composing();
    paste(&mut app, "hey @pc and more\nnext line");
    app.set_cursor(7);
    assert!(app.ghost().is_some());
    let c = composer(&app);
    let (x, y) = (c.x + 4, c.y + 1);
    let buf = screen(&app);
    assert_eq!(row(&buf, y, x, x + 25), "hey @pc-configs and more ");
    // The caret stays at the cursor, on the ghost's first letter; the rest
    // of the ghost is fog; the text after the cursor keeps its ink.
    assert_eq!(caret_cells(&app, &buf), [(x + 7, y)]);
    assert_eq!(buf[(x + 7, y)].symbol(), "-");
    for gx in x + 8..x + 15 {
        let cell = &buf[(gx, y)];
        assert_eq!(
            (cell.fg, cell.bg),
            (color(role::META), color(role::MAIN_BG)),
            "{gx}"
        );
    }
    for tx in (x..x + 7).chain(x + 16..x + 24) {
        assert_eq!(buf[(tx, y)].fg, color(role::TEXT), "{tx}");
    }
    // Other rows do not move.
    assert_eq!(row(&buf, y + 1, x, x + 10), "next line ");
}

// ----------------------------------------------------------------- esc esc

#[test]
fn esc_twice_clears_the_composer() {
    let mut app = composing();
    type_text(&mut app, "draft (x)");
    press(&mut app, KeyCode::Left);
    assert!(press(&mut app, KeyCode::Esc).is_empty());
    assert_eq!(
        (app.input.as_str(), app.cursor(), app.focus),
        ("draft (x)", 8, Focus::Composer),
        "the first esc only arms the clear"
    );
    assert!(press(&mut app, KeyCode::Esc).is_empty());
    assert_eq!(
        (app.input.as_str(), app.cursor(), app.focus),
        ("", 0, Focus::Composer)
    );
    // Empty: esc leaves for the posts, as it always did.
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.focus, Focus::Posts);
}

#[test]
fn any_other_key_disarms_the_clear_and_does_its_own_thing() {
    let mut app = composing();
    type_text(&mut app, "draft");
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Left);
    press(&mut app, KeyCode::Esc);
    assert_eq!(
        (app.input.as_str(), app.cursor()),
        ("draft", 4),
        "left moved, and the esc after it only armed again"
    );
    type_text(&mut app, "x");
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.input, "drafxt");
    paste(&mut app, "y");
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.input, "drafxyt", "a paste disarms too");

    // Tab leaves (for whichever pane comes next) with the text kept and the
    // clear (armed by that last Esc) disarmed.
    assert!(app.clear_armed);
    press(&mut app, KeyCode::Tab);
    assert_ne!(app.focus, Focus::Composer);
    assert_eq!(app.input, "drafxyt");
    assert!(!app.clear_armed);
    app.focus = Focus::Composer;
    press(&mut app, KeyCode::Esc);
    assert_eq!(
        app.input, "drafxyt",
        "back in the composer, one esc only arms"
    );
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.input, "");
}

#[test]
fn esc_on_a_ghost_puts_it_away_and_does_not_count() {
    let mut app = composing();
    type_text(&mut app, "hey @pc");
    assert!(app.ghost().is_some());
    press(&mut app, KeyCode::Esc);
    assert_eq!((app.ghost(), app.input.as_str()), (None, "hey @pc"));
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.input, "hey @pc", "the first esc that counts only arms");
    press(&mut app, KeyCode::Esc);
    assert_eq!((app.input.as_str(), app.focus), ("", Focus::Composer));
}

#[test]
fn an_armed_clear_says_so_in_the_hint() {
    let mut app = composing();
    type_text(&mut app, "draft");
    let c = composer(&app);
    let top = |app: &App| row(&screen(app), c.y, c.x, c.right());
    assert!(top(&app).contains(" enter send "), "{}", top(&app));
    press(&mut app, KeyCode::Esc);
    let buf = screen(&app);
    let hint = row(&buf, c.y, c.x, c.right());
    // In the hint's place, two cells before the corner.
    assert!(hint.ends_with(" esc again to clear ──╮"), "{hint}");
    assert!(!hint.contains("enter send"), "{hint}");
    // In the hint's quiet ink, never the accent.
    let end = c.right() - 3;
    let start = end - " esc again to clear ".chars().count() as u16;
    for x in start..end {
        let cell = &buf[(x, c.y)];
        assert_eq!(
            (cell.fg, cell.bg),
            (color(role::QUIET), color(role::MAIN_BG)),
            "({x},{})",
            c.y
        );
    }
    // Disarmed, the usual hint is back.
    press(&mut app, KeyCode::Left);
    assert!(top(&app).contains(" enter send "), "{}", top(&app));
}

#[test]
fn a_send_that_empties_the_composer_disarms_the_clear() {
    let mut app = composing();
    type_text(&mut app, "hello");
    assert_eq!(press(&mut app, KeyCode::Enter).len(), 1);
    // Esc while the post is on its way: the text is still there, so it arms.
    press(&mut app, KeyCode::Esc);
    assert!(app.clear_armed);
    app.update(sent("hello"));
    assert_eq!((app.input.as_str(), app.clear_armed), ("", false));
    let c = composer(&app);
    let hint = row(&screen(&app), c.y, c.x, c.right());
    assert!(
        hint.contains(" enter send ") && !hint.contains("esc again"),
        "{hint}"
    );
    // Empty, Esc leaves as always.
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.focus, Focus::Posts);
}

#[test]
fn an_empty_composer_never_says_esc_again_to_clear() {
    let mut app = composing();
    // However it came about.
    app.clear_armed = true;
    let c = composer(&app);
    let hint = row(&screen(&app), c.y, c.x, c.right());
    assert!(
        hint.contains(" enter send ") && !hint.contains("esc again"),
        "{hint}"
    );
}

// ------------------------------------------------------- review follow-ups

#[test]
fn a_ghost_never_leaves_a_stale_cell_where_a_wide_character_is_cut() {
    let mut app = composing();
    // One full row: `@pc `, 58 a's, then `日bbbbbbZ` on its last nine cells.
    paste(&mut app, &format!("@pc {}日bbbbbbZ more", "a".repeat(58)));
    app.set_cursor(3);
    assert!(app.ghost().is_some());
    let c = composer(&app);
    assert_eq!(c.width - 6, 71, "text cells at 104 columns");
    let (x, y) = (c.x + 4, c.y + 1);
    let buf = screen(&app);
    // `-configs` pushes the row along: the a's end on cell 69 and `日` no
    // longer fits, so cell 70 is blank, not the `Z` that was there.
    let want = format!("@pc-configs {} ", "a".repeat(58));
    assert_eq!(row(&buf, y, x, x + 71), want);
    let last = &buf[(x + 70, y)];
    assert_eq!(
        (last.fg, last.bg),
        (color(role::TEXT), color(role::MAIN_BG))
    );
    assert_eq!(row(&buf, y, x + 71, x + 73), " │", "padding and border");
}

#[test]
fn inserting_mid_text_stops_at_the_post_limit_and_keeps_the_rest() {
    let max = beacon::model::BODY_MAX;
    let mut app = composing();
    // Six bytes of room, the cursor before `TAIL`.
    paste(&mut app, &format!("{}TAIL", "a".repeat(max - 10)));
    presses(&mut app, KeyCode::Left, 4);
    paste(&mut app, "0123456789");
    assert_eq!(app.input.len(), max);
    assert!(
        app.input.ends_with("a012345TAIL"),
        "the text after it stays"
    );
    assert_eq!(app.cursor(), max - 4, "after what went in");
    assert_eq!(
        (app.toast.as_deref(), app.toast_error),
        (Some("A post holds up to 64 KB of text"), true)
    );
    // Full: typing changes nothing, and the cursor stays good.
    type_text(&mut app, "é");
    assert_eq!(app.input.len(), max);
    assert_eq!(app.cursor(), max - 4);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.cursor(), max - 3);
}

#[test]
fn the_caret_after_a_full_row_sits_on_the_padding_cell_not_the_border() {
    let width = usize::from(composer(&composing()).width - 6);
    let full = "a".repeat(width);
    // A full row before a line break, and before a soft break.
    for text in [format!("{full}\nnext"), format!("{full} next")] {
        let mut app = composing();
        paste(&mut app, &text);
        app.set_cursor(width);
        let c = composer(&app);
        let y = c.y + 1;
        let buf = screen(&app);
        assert_eq!(caret_cells(&app, &buf), [(c.right() - 2, y)], "{text:?}");
        let border = &buf[(c.right() - 1, y)];
        assert_eq!(
            (border.symbol(), border.fg, border.bg),
            ("│", color(role::DIM), color(role::MAIN_BG)),
            "{text:?}: the border keeps its ink"
        );
    }
}

#[test]
fn a_tab_is_one_blank_cell_that_the_cursor_steps_over() {
    let mut app = composing();
    paste(&mut app, "a\tb");
    let c = composer(&app);
    let (x, y) = (c.x + 4, c.y + 1);
    assert_eq!(row(&screen(&app), y, x, x + 4), "a b ");
    let mut carets = Vec::new();
    for _ in 0..4 {
        carets.push(caret_cells(&app, &screen(&app)));
        press(&mut app, KeyCode::Left);
    }
    // After `b`, on `b`, on the tab's cell, on `a`.
    assert_eq!(
        carets,
        [
            vec![(x + 3, y)],
            vec![(x + 2, y)],
            vec![(x + 1, y)],
            vec![(x, y)]
        ]
    );
    assert_eq!(app.input, "a\tb", "the text keeps its tab");
}
