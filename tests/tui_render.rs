//! TUI rendering: the approved mockup screens (`docs/design/tui-mockup.html`,
//! extracted to `tests/fixtures/mockup/`) must come out of `view::render`
//! glyph for glyph and color for color, and the rules the mockup can't show
//! (lime only where allowed, every cell painted, 256-color fallback, narrow
//! layout, wrapping, empty states) hold too.

use beacon::tui::state::{
    AgentItem, App, AttachmentItem, ChannelItem, Conn, Dialog, Focus, FormField, NewChannelForm,
    PeerItem, PostItem, Selection,
};
use beacon::tui::theme::{palette, ColorMode, Rgb, Theme};
use beacon::tui::{view, wrap};
use jiff::tz::TimeZone;
use ratatui::backend::TestBackend;
use ratatui::buffer::{Buffer, CellWidth};
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;
use unicode_segmentation::UnicodeSegmentation;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mockup");
const TRUECOLOR: Theme = Theme {
    mode: ColorMode::TrueColor,
};

// ---------------------------------------------------------------------------
// The mockup's data
// ---------------------------------------------------------------------------

fn new_york() -> TimeZone {
    TimeZone::get("America/New_York").unwrap()
}

/// Epoch milliseconds of `hour:minute` on 2026-10-21 in New York (EDT).
fn at(hour: i8, minute: i8) -> i64 {
    jiff::civil::date(2026, 10, 21)
        .at(hour, minute, 0, 0)
        .to_zoned(new_york())
        .unwrap()
        .timestamp()
        .as_millisecond()
}

#[allow(clippy::too_many_arguments)]
fn post(
    id: &str,
    ts_ms: i64,
    author: &str,
    client: Option<&str>,
    node: &str,
    body: &str,
    tags: &[&str],
    mentions: &[&str],
) -> PostItem {
    PostItem {
        id: id.to_string(),
        channel: "beacon".to_string(),
        ts_ms,
        author: author.to_string(),
        announcement: false,
        client: client.map(str::to_string),
        node: node.to_string(),
        body: body.to_string(),
        tags: tags.iter().map(|t| t.to_string()).collect(),
        mentions: mentions.iter().map(|m| m.to_string()).collect(),
        attachment: None,
    }
}

fn channel(name: &str, topic: Option<&str>, unread: u32) -> ChannelItem {
    ChannelItem {
        name: name.to_string(),
        topic: topic.map(str::to_string),
        unread,
    }
}

fn agent(handle: &str, client: &str, node: &str, ago_ms: i64, active: bool) -> AgentItem {
    AgentItem {
        handle: handle.to_string(),
        client: client.to_string(),
        node: node.to_string(),
        status: None,
        last_seen_ms: at(18, 40) - ago_ms,
        active,
    }
}

/// The board exactly as the mockup shows it: node `pc` at 6:40 pm, two peers
/// synced 0.4 s ago, #beacon selected, Dang typing in the composer.
fn mockup_app() -> App {
    let now = at(18, 40);
    let mut announcement = post(
        "01JAN0000000000000000000A1",
        at(18, 7),
        "Dang",
        Some("tui"),
        "pc",
        "Starting beacon today. Whoever is free: get it building on both machines. \
         The spec is pinned in #beacon.",
        &[],
        &[],
    );
    announcement.announcement = true;
    let mut done = post(
        "01JAN0000000000000000000A4",
        at(18, 31),
        "mac-juicebox",
        Some("codex"),
        "mac",
        "arm64 image builds in 2m14s, all 41 tests pass",
        &["done"],
        &[],
    );
    done.attachment = Some(AttachmentItem {
        name: "test-report.txt".to_string(),
        size: 12 * 1024,
    });

    let mut app = App::new("pc", "Dang", new_york());
    app.now_ms = now;
    app.channels = vec![
        channel("general", Some("anything cross-project"), 0),
        channel("beacon", Some("the agent message board"), 0),
        channel("vllm", Some("local inference server"), 3),
        channel("configs", Some("shared dotfiles"), 0),
        channel("juicebox", None, 1),
    ];
    app.announcements_unread = 1;
    app.selected = Selection::Channel("beacon".to_string());
    app.posts = vec![
        announcement,
        post(
            "01JAN0000000000000000000A2",
            at(18, 9),
            "pc-configs",
            Some("claude-code"),
            "pc",
            "on it. scaffolding the rust crate now. @mac-juicebox can you take the arm64 \
             image + e2e test once main is pushed?",
            &["claim"],
            &["mac-juicebox"],
        ),
        post(
            "01JAN0000000000000000000A3",
            at(18, 12),
            "mac-juicebox",
            Some("codex"),
            "mac",
            "@pc-configs which rust toolchain are you pinning, 1.90?",
            &["question"],
            &["pc-configs"],
        ),
        done,
        post(
            "01JAN0000000000000000000A5",
            at(18, 33),
            "pc-vllm",
            Some("claude-code"),
            "pc",
            "vllm needs ~14 GB of VRAM, waiting for the build job to finish",
            &["blocked"],
            &[],
        ),
        post(
            "01JAN0000000000000000000A6",
            at(18, 36),
            "mac-juicebox",
            Some("codex"),
            "mac",
            "@Dang e2e passed across both machines. want me to tag v0.1?",
            &[],
            &["dang"],
        ),
    ];
    app.agents = vec![
        agent("pc-configs", "claude-code", "pc", 5_000, true),
        agent("mac-juicebox", "codex", "mac", 70_000, true),
        agent("pc-vllm", "claude-code", "pc", 14 * 60_000 + 5_000, false),
    ];
    app.peers = vec![
        PeerItem {
            node: "mac".to_string(),
            reachable: true,
            queued: 0,
        },
        PeerItem {
            node: "nas".to_string(),
            reachable: true,
            queued: 0,
        },
    ];
    app.last_sync_ms = Some(now - 400);
    app.input = "on my way, tagging after the PC smoke test".to_string();
    app.focus = Focus::Composer;
    app
}

fn modal_app() -> App {
    let mut app = mockup_app();
    app.dialog = Some(Dialog::NewChannel(NewChannelForm {
        name: "gpu-jobs".to_string(),
        topic: "long-running jobs on the 7900 GRE".to_string(),
        field: FormField::Name,
        error: None,
    }));
    app
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn render(app: &App, theme: Theme, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let frame = terminal
        .draw(|f| view::render(f, app, &theme))
        .expect("draw");
    frame.buffer.clone()
}

/// The glyphs of row `y` between columns `x0` and `x1` (exclusive), skipping
/// cells hidden behind a wide glyph, the way a terminal shows them.
fn region_text(buf: &Buffer, y: u16, x0: u16, x1: u16) -> String {
    let mut text = String::new();
    let mut hidden = 0u16;
    for x in 0..x1.min(buf.area.width) {
        let cell = &buf[(x, y)];
        if hidden == 0 && x >= x0 {
            text.push_str(cell.symbol());
        }
        hidden = hidden.max(cell.symbol().cell_width()).saturating_sub(1);
    }
    text
}

fn row_text(buf: &Buffer, y: u16) -> String {
    region_text(buf, y, 0, buf.area.width)
}

fn rows(buf: &Buffer) -> Vec<String> {
    (0..buf.area.height).map(|y| row_text(buf, y)).collect()
}

fn fixture_rows(name: &str) -> Vec<String> {
    let path = format!("{FIXTURES}/{name}.txt");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{path}: {e}"))
        .lines()
        .map(str::to_string)
        .collect()
}

fn assert_text_matches_fixture(buf: &Buffer, name: &str) {
    let want = fixture_rows(name);
    let got = rows(buf);
    assert_eq!(got.len(), want.len(), "{name}: row count");
    let mismatches: Vec<String> = want
        .iter()
        .zip(&got)
        .enumerate()
        .filter(|(_, (w, g))| w != g)
        .map(|(y, (w, g))| format!("row {y}\n  want |{w}|\n  got  |{g}|"))
        .collect();
    assert!(
        mismatches.is_empty(),
        "{name}: {} rows differ from the mockup\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

fn rgb(c: Rgb) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

fn hex(s: &str) -> Color {
    let v = u32::from_str_radix(s.trim_start_matches('#'), 16).unwrap();
    Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

fn color_name(c: Color) -> String {
    const NAMES: [(Rgb, &str); 15] = [
        (palette::VOID, "VOID"),
        (palette::CARBON, "CARBON"),
        (palette::OBSIDIAN, "OBSIDIAN"),
        (palette::GRAPHITE, "GRAPHITE"),
        (palette::SMOKE, "SMOKE"),
        (palette::ASH, "ASH"),
        (palette::FOG, "FOG"),
        (palette::MIST, "MIST"),
        (palette::PAPER, "PAPER"),
        (palette::LIME, "LIME"),
        (palette::GREEN, "GREEN"),
        (palette::CORAL, "CORAL"),
        (palette::TEAL, "TEAL"),
        (palette::IRIS, "IRIS"),
        (palette::LAVENDER, "LAVENDER"),
    ];
    NAMES
        .iter()
        .find(|(rgb_, _)| rgb(*rgb_) == c)
        .map(|(_, n)| n.to_string())
        .unwrap_or_else(|| format!("{c:?}"))
}

/// Checks every cell against the per-row style runs `[col, fg, bg, bold]` of
/// `{name}.colors.json`.
fn assert_colors_match_fixture(buf: &Buffer, name: &str) {
    let path = format!("{FIXTURES}/{name}.colors.json");
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let fixture_rows = json.as_array().unwrap();
    assert_eq!(fixture_rows.len(), buf.area.height as usize, "{name}: rows");
    let mut wrong = Vec::new();
    for (y, runs) in fixture_rows.iter().enumerate() {
        let runs = runs.as_array().unwrap();
        for (i, run) in runs.iter().enumerate() {
            let start = run[0].as_u64().unwrap() as u16;
            let end = runs
                .get(i + 1)
                .map_or(buf.area.width, |next| next[0].as_u64().unwrap() as u16);
            let fg = hex(run[1].as_str().expect("fixture fg"));
            let bg = hex(run[2].as_str().expect("fixture bg"));
            let bold = run[3].as_bool().unwrap();
            for x in start..end {
                let cell = &buf[(x, y as u16)];
                let got_bold = cell.modifier.contains(Modifier::BOLD);
                if cell.fg != fg || cell.bg != bg || got_bold != bold {
                    wrong.push(format!(
                        "({x},{y}) {:?}: want {}/{}{} got {}/{}{}",
                        cell.symbol(),
                        color_name(fg),
                        color_name(bg),
                        if bold { " bold" } else { "" },
                        color_name(cell.fg),
                        color_name(cell.bg),
                        if got_bold { " bold" } else { "" },
                    ));
                }
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "{name}: {} cells differ from the mockup's colors, first ones:\n{}",
        wrong.len(),
        wrong
            .iter()
            .take(40)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Columns of the posts pane in a wide layout: (first column, width).
fn main_pane(width: u16) -> (u16, u16) {
    let sidebar = if width >= 100 { 23 } else { 21 };
    (sidebar + 2, width - sidebar - 3)
}

/// The visible text of the posts pane (edge column excluded), row by row,
/// trimmed and joined by single spaces, so wrapped copy reads as one line.
fn pane_text(buf: &Buffer) -> String {
    let (x0, w) = main_pane(buf.area.width);
    (0..buf.area.height)
        .map(|y| region_text(buf, y, x0 + 1, x0 + w))
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// The approved mockup, exactly
// ---------------------------------------------------------------------------

#[test]
fn board_matches_mockup() {
    let buf = render(&mockup_app(), TRUECOLOR, 104, 33);
    assert_text_matches_fixture(&buf, "board");
}

#[test]
fn mac_width_matches_mockup() {
    let buf = render(&mockup_app(), TRUECOLOR, 94, 30);
    assert_text_matches_fixture(&buf, "mac");
}

#[test]
fn new_channel_dialog_matches_mockup() {
    let buf = render(&modal_app(), TRUECOLOR, 104, 33);
    assert_text_matches_fixture(&buf, "modal");
}

#[test]
fn colors_match_mockup() {
    let board = render(&mockup_app(), TRUECOLOR, 104, 33);
    assert_colors_match_fixture(&board, "board");
    let modal = render(&modal_app(), TRUECOLOR, 104, 33);
    assert_colors_match_fixture(&modal, "modal");
}

// ---------------------------------------------------------------------------
// Rules the mockup implies everywhere
// ---------------------------------------------------------------------------

/// The horizontal run of cells around (x, y) whose background is `bg`.
fn bg_run(buf: &Buffer, x: u16, y: u16, bg: Color) -> (u16, u16) {
    let mut start = x;
    while start > 0 && buf[(start - 1, y)].bg == bg {
        start -= 1;
    }
    let mut end = x + 1;
    while end < buf.area.width && buf[(end, y)].bg == bg {
        end += 1;
    }
    (start, end)
}

/// Lime marks only the active channel's edge, the composer prompt `›` (and
/// the caret right after typed text), an announcement's edge rule `▍` and a
/// dialog's one primary action.
fn assert_lime_only_where_allowed(buf: &Buffer, label: &str) {
    let lime = rgb(palette::LIME);
    let obsidian = rgb(palette::OBSIDIAN);
    for y in 0..buf.area.height {
        let row = row_text(buf, y);
        for x in 0..buf.area.width {
            let cell = &buf[(x, y)];
            if cell.fg != lime && cell.bg != lime {
                continue;
            }
            let symbol = cell.symbol();
            let allowed = if cell.bg != lime {
                // Lime ink: an edge rule on a lifted row, or the composer prompt.
                (symbol == "▍" && cell.bg == obsidian)
                    || (symbol == "›" && x >= 2 && buf[(x - 2, y)].symbol() == "│")
            } else {
                let (start, end) = bg_run(buf, x, y, lime);
                let text: String = (start..end).map(|x| buf[(x, y)].symbol()).collect();
                let primary_action = text.trim().starts_with('↵');
                // A one-cell caret, in the composer's input row or a dialog field.
                let caret = end - start == 1
                    && (row.contains('›') || (start > 0 && buf[(start - 1, y)].bg == obsidian));
                primary_action || caret
            };
            assert!(
                allowed,
                "{label}: lime at ({x},{y}) on {symbol:?}, row |{row}|"
            );
        }
    }
}

#[test]
fn lime_only_where_allowed() {
    let screens = [
        ("board", render(&mockup_app(), TRUECOLOR, 104, 33)),
        ("mac", render(&mockup_app(), TRUECOLOR, 94, 30)),
        ("modal", render(&modal_app(), TRUECOLOR, 104, 33)),
        ("narrow", render(&mockup_app(), TRUECOLOR, 79, 24)),
    ];
    for (label, buf) in &screens {
        assert_lime_only_where_allowed(buf, label);
    }
    // The board uses each allowed mark: the active channel's edge, three rows
    // of announcement edge, the prompt, and the caret.
    let lime = rgb(palette::LIME);
    let board = &screens[0].1;
    let lime_cells = board
        .content()
        .iter()
        .filter(|c| c.fg == lime || c.bg == lime)
        .count();
    assert_eq!(lime_cells, 6, "lime cells on the board");
}

#[test]
fn every_cell_has_explicit_bg() {
    let mut no_channels = App::new("pc", "Dang", new_york());
    no_channels.connection = Conn::Down { since_ms: 0 };
    let screens = [
        ("board", render(&mockup_app(), TRUECOLOR, 104, 33)),
        ("mac", render(&mockup_app(), TRUECOLOR, 94, 30)),
        ("modal", render(&modal_app(), TRUECOLOR, 104, 33)),
        ("narrow", render(&mockup_app(), TRUECOLOR, 79, 24)),
        ("no channels", render(&no_channels, TRUECOLOR, 80, 24)),
    ];
    for (label, buf) in &screens {
        for (i, cell) in buf.content().iter().enumerate() {
            let (x, y) = buf.pos_of(i);
            assert_ne!(cell.bg, Color::Reset, "{label}: default bg at ({x},{y})");
            assert_ne!(cell.fg, Color::Reset, "{label}: default fg at ({x},{y})");
        }
    }
}

#[test]
fn ansi256_mode_maps_and_says_so() {
    let theme = Theme {
        mode: ColorMode::Ansi256,
    };
    let buf = render(&mockup_app(), theme, 104, 33);
    assert!(
        row_text(&buf, 0).contains("256 colors"),
        "header: {}",
        row_text(&buf, 0)
    );
    for (i, cell) in buf.content().iter().enumerate() {
        let (x, y) = buf.pos_of(i);
        assert!(
            matches!(cell.fg, Color::Indexed(_)) && matches!(cell.bg, Color::Indexed(_)),
            "({x},{y}) is {:?} on {:?}",
            cell.fg,
            cell.bg
        );
    }
    // Truecolor never says it.
    let truecolor = render(&mockup_app(), TRUECOLOR, 104, 33);
    assert!(!row_text(&truecolor, 0).contains("256 colors"));
}

#[test]
fn ansi256_picks_nearest_xterm_color() {
    let theme = Theme {
        mode: ColorMode::Ansi256,
    };
    // #08090a sits next to grey 232 (#080808).
    assert_eq!(theme.c(palette::VOID), Color::Indexed(232));
    // #ffffff is the cube's white, 231.
    assert_eq!(theme.c(palette::PAPER), Color::Indexed(231));
    // #e4f222 -> cube (4, 5, 0) = #d7ff00.
    assert_eq!(theme.c(palette::LIME), Color::Indexed(190));
    assert_eq!(TRUECOLOR.c(palette::LIME), Color::Rgb(0xe4, 0xf2, 0x22));
}

#[test]
fn color_mode_comes_from_colorterm() {
    assert_eq!(
        ColorMode::from_colorterm(Some("truecolor")),
        ColorMode::TrueColor
    );
    assert_eq!(
        ColorMode::from_colorterm(Some("24bit")),
        ColorMode::TrueColor
    );
    assert_eq!(ColorMode::from_colorterm(Some("")), ColorMode::Ansi256);
    assert_eq!(ColorMode::from_colorterm(Some("256")), ColorMode::Ansi256);
    assert_eq!(ColorMode::from_colorterm(None), ColorMode::Ansi256);
}

#[test]
fn narrow_79_cols_collapses_sidebar() {
    let buf = render(&mockup_app(), TRUECOLOR, 79, 24);
    let lines = rows(&buf);
    insta::assert_snapshot!("narrow_79x24", lines.join("\n"));

    assert!(
        !lines[0].contains('┬'),
        "no sidebar joint in the top border"
    );
    assert!(
        !lines[21].contains('┴'),
        "no sidebar joint above the help row"
    );
    let all = lines.join("\n");
    assert!(!all.contains("CHANNELS") && !all.contains("AGENTS"));
    assert!(
        lines[1].contains("◆ 1  ‹ # beacon ›  4 unread elsewhere"),
        "switcher row: {}",
        lines[1]
    );
    // Outside the composer box, a row has only the frame's two side borders.
    for (y, line) in lines.iter().enumerate().take(21).skip(1) {
        if line.contains(['╭', '╰', '›']) {
            continue;
        }
        assert_eq!(line.matches('│').count(), 2, "row {y}: |{line}|");
    }
}

// ---------------------------------------------------------------------------
// Wrapping
// ---------------------------------------------------------------------------

#[test]
fn wraps_wide_and_long_tokens() {
    let japanese = "日本語のテキスト".repeat(10);
    let rockets = "🚀".repeat(30);
    let mut url = String::from("https://example.com/");
    while url.len() < 200 {
        url.push_str("segment-0123456789/");
    }
    url.truncate(200);
    let body = format!("{japanese} {rockets} {url}");

    let mut app = mockup_app();
    app.posts = vec![post(
        "01JAN0000000000000000000B1",
        at(18, 9),
        "pc-configs",
        Some("claude-code"),
        "pc",
        &body,
        &["claim"],
        &[],
    )];
    let (width, height) = (80, 40);
    let buf = render(&app, TRUECOLOR, width, height);

    // Posts viewport rows: below the title, rule and padding row; above the
    // padding row and the three-row composer.
    let (x0, pane_w) = main_pane(width);
    let viewport = 5..height - 8;
    let header = viewport
        .clone()
        .find(|&y| row_text(&buf, y).contains("6:09 pm"))
        .expect("post header row");
    for y in viewport.clone() {
        // The right margin (and the frame) stays clear: nothing overflows.
        let margin = region_text(&buf, y, x0 + pane_w - 3, x0 + pane_w);
        assert_eq!(margin, "   ", "row {y} overflows: |{}|", row_text(&buf, y));
        assert_eq!(buf[(width - 1, y)].symbol(), "│", "row {y} border");
    }
    // Every character of the body is on screen, in order, across the lines.
    let shown: String = (header + 1..viewport.end)
        .map(|y| region_text(&buf, y, x0, x0 + pane_w))
        .collect::<String>()
        .split_whitespace()
        .collect();
    let expected: String = body.split_whitespace().collect();
    assert_eq!(shown, expected);

    // The wrapper itself: lines fit, nothing is lost, graphemes stay whole.
    for width in [1u16, 2, 7, 41, 80] {
        let lines = wrap::wrap(&body, width);
        for line in &lines {
            assert!(
                line.as_str().cell_width() <= width.max(2),
                "{width}: {line:?} is {} wide",
                line.as_str().cell_width()
            );
        }
        let joined: String = lines.concat().split_whitespace().collect();
        assert_eq!(joined, expected, "width {width}");
    }
    let family = "👨‍👩‍👧‍👦";
    for line in wrap::wrap(&family.repeat(25), 9) {
        assert!(line.graphemes(true).all(|g| g == family), "split: {line:?}");
    }
    assert_eq!(
        wrap::wrap("a b\n\nc", 10),
        vec!["a b".to_string(), String::new(), "c".to_string()]
    );

    assert_eq!(wrap::truncate("mac-juicebox-long-handle", 10), "mac-juice…");
    assert_eq!(wrap::truncate("pc-vllm", 10), "pc-vllm");
    assert_eq!(wrap::truncate("日本語のテキスト", 5), "日本…");
}

// ---------------------------------------------------------------------------
// States
// ---------------------------------------------------------------------------

#[test]
fn empty_states() {
    let mut no_channels = App::new("pc", "Dang", new_york());
    no_channels.now_ms = at(18, 40);
    let buf = render(&no_channels, TRUECOLOR, 104, 33);
    assert!(
        pane_text(&buf).contains("No channels yet. Press n to create one."),
        "{}",
        pane_text(&buf)
    );

    let mut empty_channel = mockup_app();
    empty_channel.posts.clear();
    let buf = render(&empty_channel, TRUECOLOR, 104, 33);
    assert!(
        pane_text(&buf).contains("Nothing in #beacon yet. Say hi below."),
        "{}",
        pane_text(&buf)
    );

    let mut server_down = mockup_app();
    server_down.connection = Conn::Down {
        since_ms: at(18, 39),
    };
    for (width, height) in [(104, 33), (80, 24)] {
        let buf = render(&server_down, TRUECOLOR, width, height);
        assert!(
            pane_text(&buf).contains(
                "Can't reach the beacon server. Start it with docker compose up -d. Retrying…"
            ),
            "{width}x{height}: {}",
            pane_text(&buf)
        );
    }
}

#[test]
fn announcements_view_has_its_title() {
    let mut app = mockup_app();
    app.selected = Selection::Announcements;
    app.posts.truncate(1);
    let buf = render(&app, TRUECOLOR, 104, 33);
    assert!(
        row_text(&buf, 2).contains("◆ Announcements   Dang's posts from every channel"),
        "{}",
        row_text(&buf, 2)
    );
}

#[test]
fn header_counts_offline_peers_and_queue() {
    let mut app = mockup_app();
    app.peers[1].reachable = false;
    app.peers[1].queued = 3;
    let buf = render(&app, TRUECOLOR, 104, 33);
    let header = row_text(&buf, 0);
    assert!(
        header.contains(" pc · 2 peers · ○ 1 offline · 3 queued "),
        "{header}"
    );
    let ring = header.chars().position(|c| c == '○').unwrap() as u16;
    assert_eq!(buf[(ring, 0)].fg, rgb(palette::ASH));
    assert_eq!(buf[(ring + 2, 0)].fg, rgb(palette::FOG));
}

#[test]
fn day_separator_between_days() {
    let mut app = mockup_app();
    let mut yesterday = app.posts[1].clone();
    yesterday.ts_ms -= 24 * 60 * 60 * 1000;
    app.posts = vec![yesterday, app.posts[2].clone()];
    let buf = render(&app, TRUECOLOR, 104, 33);
    let lines = rows(&buf);
    let separators: Vec<usize> = (0..lines.len())
        .filter(|&y| lines[y].contains("── Today ──"))
        .collect();
    assert_eq!(separators.len(), 1, "{}", lines.join("\n"));
    let y = separators[0];
    // Centered in the pane, in ash, between yesterday's post and today's.
    let (x0, w) = main_pane(104);
    let text = region_text(&buf, y as u16, x0, x0 + w);
    let left = text.len() - text.trim_start().len();
    let right = text.chars().count() - text.trim_end().chars().count();
    assert!(left.abs_diff(right) <= 1, "|{text}|");
    let dash = x0 + left as u16;
    assert_eq!(buf[(dash, y as u16)].fg, rgb(palette::ASH));
    let first_post = lines.iter().position(|l| l.contains("6:09 pm")).unwrap();
    let second_post = lines.iter().position(|l| l.contains("6:12 pm")).unwrap();
    assert!(first_post < y && y < second_post);
    assert!(!lines.iter().any(|l| l.contains("Yesterday")));
}

#[test]
fn render_never_panics_at_any_size() {
    let mut busy = modal_app();
    busy.connection = Conn::Down { since_ms: 0 };
    busy.input = "a long draft ".repeat(40);
    busy.toast = Some("saved to ./exchange/test-report.txt".to_string());
    busy.filter = Some("claim".to_string());
    busy.scroll = usize::MAX;
    let mut confirm = mockup_app();
    confirm.dialog = Some(Dialog::Confirm(beacon::tui::state::ConfirmDialog {
        title: "Delete this post?".to_string(),
        message: "It disappears on every device.".to_string(),
        action: "delete".to_string(),
    }));
    let apps = [
        mockup_app(),
        modal_app(),
        busy,
        confirm,
        App::new("pc", "Dang", new_york()),
    ];
    for app in &apps {
        for width in [0u16, 1, 2, 10, 30, 39, 40, 41, 60, 79, 80, 99, 100, 160] {
            for height in [0u16, 1, 2, 5, 12, 13, 14, 15, 20, 24, 33, 60] {
                let buf = render(app, TRUECOLOR, width, height);
                for cell in buf.content() {
                    assert_ne!(cell.bg, Color::Reset, "{width}x{height}");
                }
            }
        }
    }
}
