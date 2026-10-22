//! TUI rendering: the approved mockup screens (`docs/design/tui-mockup.html`,
//! extracted to `tests/fixtures/mockup/`) must come out of `view::render`
//! glyph for glyph and color for color, and the rules the mockup can't show
//! (lime only where allowed, every cell painted, 256-color fallback, narrow
//! layout, wrapping, empty states) hold too.

use std::collections::BTreeSet;
use std::ops::Range;

use beacon::tui::state::{
    AgentItem, App, AttachmentItem, ChannelItem, Conn, Dialog, Focus, FormField, NewChannelForm,
    PeerItem, PostItem, Selection,
};
use beacon::tui::theme::{nearest_xterm256, palette, ColorMode, Rgb, Theme};
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

/// Checks every cell of `rows` against the per-row style runs
/// `[col, fg, bg, bold]` of `{name}.colors.json`.
fn assert_colors_match_fixture(buf: &Buffer, name: &str, rows: Range<usize>) {
    let path = format!("{FIXTURES}/{name}.colors.json");
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let fixture_rows = json.as_array().unwrap();
    assert_eq!(fixture_rows.len(), buf.area.height as usize, "{name}: rows");
    let mut wrong = Vec::new();
    for (y, runs) in fixture_rows.iter().enumerate() {
        if !rows.contains(&y) {
            continue;
        }
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
    assert_colors_match_fixture(&board, "board", 0..33);
    let modal = render(&modal_app(), TRUECOLOR, 104, 33);
    assert_colors_match_fixture(&modal, "modal", 0..33);
    // The Mac screen's bottom border (row 29) carries a stray obsidian
    // background left over by the mockup's generator; every other row counts.
    let mac = render(&mockup_app(), TRUECOLOR, 94, 30);
    assert_colors_match_fixture(&mac, "mac", 0..29);
}

// ---------------------------------------------------------------------------
// Rules the mockup implies everywhere
// ---------------------------------------------------------------------------

type Cells = BTreeSet<(u16, u16)>;

/// The cells allowed to carry lime on a screen, each found by the role of
/// the element it belongs to.
#[derive(Debug, Default)]
struct LimeSites {
    /// `▍` edge rules: the active channel's sidebar row, each announcement row.
    edges: Cells,
    /// The composer's `›`.
    prompt: Cells,
    /// Block carets: after the composer's text, after a focused field's value.
    carets: Cells,
    /// The dialog's primary action button.
    primary: Cells,
}

/// The column where `needle` starts in row `y` (screens without wide glyphs).
fn column_of(buf: &Buffer, y: u16, needle: &str) -> Option<u16> {
    let row = row_text(buf, y);
    row.find(needle)
        .map(|byte| row[..byte].chars().count() as u16)
}

fn lime_sites(buf: &Buffer, app: &App) -> LimeSites {
    let (width, height) = (buf.area.width, buf.area.height);
    let has_sidebar = width >= 80;
    let (x0, pane_w) = if has_sidebar {
        main_pane(width)
    } else {
        (1, width - 2)
    };
    let mut sites = LimeSites::default();

    // The active channel: the sidebar's first column on its row.
    if let (true, Selection::Channel(name)) = (has_sidebar, &app.selected) {
        let label = format!("# {name}");
        let y = (1..height)
            .find(|&y| region_text(buf, y, 2, x0 - 1).trim() == label)
            .expect("active channel row");
        sites.edges.insert((1, y));
    }
    // An announcement: the pane's edge column on its header row (the one
    // with the `announcement` label) and on each body row under it.
    for y in 1..height {
        if region_text(buf, y, x0, x0 + pane_w).contains(" announcement ") {
            let rows = y..height;
            for row in
                rows.take_while(|&r| !region_text(buf, r, x0 + 1, x0 + pane_w).trim().is_empty())
            {
                sites.edges.insert((x0, row));
            }
        }
    }
    // The composer's input row: `│ › text█`.
    let y = (1..height)
        .find(|&y| buf[(x0, y)].symbol() == "│" && buf[(x0 + 2, y)].symbol() == "›")
        .expect("composer input row");
    sites.prompt.insert((x0 + 2, y));
    sites
        .carets
        .insert((x0 + 4 + wrap::display_width(&app.input), y));

    if let Some(Dialog::NewChannel(form)) = &app.dialog {
        let value = match form.field {
            FormField::Name => &form.name,
            FormField::Topic => &form.topic,
        };
        let y = (1..height)
            .find(|&y| row_text(buf, y).contains(value.as_str()))
            .expect("focused field row");
        let x = column_of(buf, y, value).unwrap() + wrap::display_width(value);
        sites.carets.insert((x, y));
        let button = " ↵ create ";
        let y = (1..height)
            .find(|&y| row_text(buf, y).contains(button))
            .expect("primary action row");
        let x = column_of(buf, y, button).unwrap();
        sites
            .primary
            .extend((x..x + wrap::display_width(button)).map(|x| (x, y)));
    }
    sites
}

fn lime_cells(buf: &Buffer) -> Cells {
    let lime = rgb(palette::LIME);
    buf.content()
        .iter()
        .enumerate()
        .filter(|(_, c)| c.fg == lime || c.bg == lime)
        .map(|(i, _)| buf.pos_of(i))
        .collect()
}

#[test]
fn lime_only_where_allowed() {
    let lime = rgb(palette::LIME);
    // Nothing dimmed: exactly the edges, the prompt and the caret are lime.
    for (label, width, height) in [("board", 104, 33), ("mac", 94, 30), ("narrow", 79, 24)] {
        let app = mockup_app();
        let buf = render(&app, TRUECOLOR, width, height);
        let sites = lime_sites(&buf, &app);
        let allowed: Cells = sites
            .edges
            .iter()
            .chain(&sites.prompt)
            .chain(&sites.carets)
            .copied()
            .collect();
        assert_eq!(lime_cells(&buf), allowed, "{label}: {sites:?}");
        for &(x, y) in &sites.edges {
            let cell = &buf[(x, y)];
            assert_eq!(
                (cell.symbol(), cell.fg, cell.bg),
                ("▍", lime, rgb(palette::OBSIDIAN)),
                "{label}: edge ({x},{y})"
            );
        }
        for &(x, y) in &sites.prompt {
            assert_eq!(
                (buf[(x, y)].symbol(), buf[(x, y)].fg),
                ("›", lime),
                "{label}: prompt"
            );
        }
        for &(x, y) in &sites.carets {
            assert_eq!(buf[(x, y)].bg, lime, "{label}: caret ({x},{y})");
        }
        if label == "board" {
            // The active channel, three announcement rows, prompt and caret.
            assert_eq!((sites.edges.len(), allowed.len()), (4, 6));
        }
    }

    // Behind a dialog the marks dim: lime stays on the two carets (composer
    // and the focused field) and the dialog's primary action.
    let app = modal_app();
    let buf = render(&app, TRUECOLOR, 104, 33);
    let sites = lime_sites(&buf, &app);
    let allowed: Cells = sites.carets.union(&sites.primary).copied().collect();
    assert_eq!(sites.carets.len(), 2);
    assert_eq!(sites.primary.len(), 10);
    assert_eq!(lime_cells(&buf), allowed, "modal: {sites:?}");
    for &(x, y) in &sites.primary {
        assert_eq!(
            (buf[(x, y)].fg, buf[(x, y)].bg),
            (rgb(palette::VOID), lime),
            "primary ({x},{y})"
        );
    }
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
fn ansi256_marker_survives_a_long_node_name() {
    let theme = Theme {
        mode: ColorMode::Ansi256,
    };
    let mut app = mockup_app();
    for node in [
        "workstation-in-the-basement-rack-01".to_string(),
        "n".repeat(70),
    ] {
        app.node = node;
        for (width, height) in [(80, 24), (79, 24), (104, 33)] {
            let header = row_text(&render(&app, theme, width, height), 0);
            assert!(
                header.contains("· 256 colors ─╮"),
                "{width}x{height}: {header}"
            );
        }
    }
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
    // Obsidian is exactly between greys 233 and 234; the tie goes to the
    // lighter one, so a lifted row still stands off the carbon panel.
    assert_eq!(theme.c(palette::CARBON), Color::Indexed(233));
    assert_eq!(theme.c(palette::OBSIDIAN), Color::Indexed(234));
    // Ash (#787d85, lifted from #62666d for contrast) takes its own nearest
    // index: grey 244 (#808080), not the old ash's 242, and apart from fog.
    assert_eq!(nearest_xterm256(Rgb(0x78, 0x7d, 0x85)), 244);
    assert_eq!(theme.c(palette::ASH), Color::Indexed(244));
    assert_ne!(theme.c(palette::ASH), theme.c(palette::FOG));
    // Every surface keeps its own step.
    let surfaces: BTreeSet<String> = [
        palette::VOID,
        palette::CARBON,
        palette::OBSIDIAN,
        palette::GRAPHITE,
    ]
    .iter()
    .map(|&c| format!("{:?}", theme.c(c)))
    .collect();
    assert_eq!(surfaces.len(), 4, "{surfaces:?}");
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
    assert_eq!(
        wrap::truncate(&"x".repeat(70_000), 5),
        "xxxx…",
        "wider than u16 counts"
    );
    assert_eq!(wrap::truncate("pc-vllm", 10), "pc-vllm");
    assert_eq!(wrap::truncate("日本語のテキスト", 5), "日本…");
}

#[test]
fn wraps_a_body_wider_than_u16() {
    // The server takes bodies up to 64 KiB, so one token can be 65 534 cells.
    let body = format!("a {}", "x".repeat(65_534));
    let lines = wrap::wrap(&body, 63);
    for line in &lines {
        assert!(line.as_str().cell_width() <= 63, "{} wide", line.len());
    }
    assert_eq!(lines[0], "a");
    assert_eq!(lines.concat(), format!("a{}", "x".repeat(65_534)));
    assert_eq!(lines.len(), 1 + 65_534_usize.div_ceil(63));

    // And on screen: the newest rows are the token's tail, hard-broken.
    let mut app = mockup_app();
    app.posts = vec![post(
        "01JAN0000000000000000000C1",
        at(18, 9),
        "pc-configs",
        Some("claude-code"),
        "pc",
        &body,
        &[],
        &[],
    )];
    let buf = render(&app, TRUECOLOR, 104, 33);
    let (x0, pane_w) = main_pane(104);
    let last = region_text(&buf, 24, x0, x0 + pane_w);
    let tail = 65_534 % 63;
    assert_eq!(last.trim(), "x".repeat(tail), "|{last}|");
    let full = region_text(&buf, 23, x0, x0 + pane_w);
    assert_eq!(full.trim(), "x".repeat(63), "|{full}|");
}

#[test]
fn trailing_whitespace_adds_no_row() {
    let mut app = mockup_app();
    let mut first = app.posts[1].clone();
    first.body = "hello\n".to_string();
    first.tags.clear();
    let mut second = app.posts[2].clone();
    // Exactly the body width (63 at 104 cols), then a space and a newline.
    second.body = format!("{} \n", "x".repeat(63));
    second.tags.clear();
    second.mentions.clear();
    app.posts = vec![first, second];
    let buf = render(&app, TRUECOLOR, 104, 33);
    let (x0, pane_w) = main_pane(104);
    let pane = |y: u16| region_text(&buf, y, x0, x0 + pane_w).trim().to_string();
    // Bottom-anchored on row 24, with nothing but a blank between the posts.
    assert_eq!(pane(24), "x".repeat(63));
    assert!(
        pane(23).starts_with("6:12 pm  mac-juicebox"),
        "{}",
        pane(23)
    );
    assert_eq!(pane(22), "");
    assert_eq!(pane(21), "hello");
    assert!(pane(20).starts_with("6:09 pm  pc-configs"), "{}", pane(20));
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
    // Yesterday's post says its day too: its label sits above it.
    let label = lines
        .iter()
        .position(|l| l.contains("── Yesterday ──"))
        .expect("a label above the first post");
    assert_eq!(label + 2, first_post, "{}", lines.join("\n"));
}

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

#[test]
fn a_first_post_from_another_day_is_labelled() {
    // A channel whose posts are all from yesterday: the times alone would
    // read as today's.
    let mut app = mockup_app();
    app.posts.truncate(3);
    for post in &mut app.posts {
        post.ts_ms -= DAY_MS;
    }
    let buf = render(&app, TRUECOLOR, 104, 33);
    let lines = rows(&buf);
    let labels: Vec<usize> = (0..lines.len())
        .filter(|&y| lines[y].contains("──") && lines[y].contains("day ──"))
        .collect();
    let first = lines.iter().position(|l| l.contains("6:07 pm")).unwrap();
    assert_eq!(labels.len(), 1, "{}", lines.join("\n"));
    assert!(lines[labels[0]].contains("── Yesterday ──"));
    assert_eq!(labels[0] + 2, first, "the label, a blank row, the post");

    // Older than yesterday: the date.
    for post in &mut app.posts {
        post.ts_ms -= 2 * DAY_MS;
    }
    let lines = rows(&render(&app, TRUECOLOR, 104, 33));
    assert!(
        lines.iter().any(|l| l.contains("── Sun Oct 18 ──")),
        "{}",
        lines.join("\n")
    );

    // Scrolling to the top of a full channel reaches the label: it counts
    // as a row of the history.
    let mut app = mockup_app();
    for post in &mut app.posts {
        post.ts_ms -= DAY_MS;
    }
    app.update(beacon::tui::state::Msg::Resize(104, 33));
    for _ in 0..10 {
        app.update(beacon::tui::state::Msg::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::PageUp,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
    }
    let buf = render(&app, TRUECOLOR, 104, 33);
    // Row 5 is the first row of the posts viewport.
    assert!(
        row_text(&buf, 5).contains("── Yesterday ──"),
        "{}",
        rows(&buf).join("\n")
    );
}

#[test]
fn too_small_says_the_real_minimum() {
    let app = mockup_app();
    // The board is drawn down to its minimum …
    let buf = render(&app, TRUECOLOR, view::MIN_WIDTH, view::MIN_HEIGHT);
    assert!(
        row_text(&buf, 0).contains("◆ beacon"),
        "{}",
        rows(&buf).join("\n")
    );
    // … and below it the message gives that minimum, and the size it got.
    for (width, height) in [(view::MIN_WIDTH - 1, 30), (100, view::MIN_HEIGHT - 1)] {
        let buf = render(&app, TRUECOLOR, width, height);
        let text = rows(&buf)
            .iter()
            .map(|r| r.trim())
            .filter(|r| !r.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            text.contains(&format!(
                "at least {}×{}",
                view::MIN_WIDTH,
                view::MIN_HEIGHT
            )),
            "{width}x{height}: {text}"
        );
        assert!(
            text.contains(&format!("{width}×{height}")),
            "{width}x{height}: {text}"
        );
    }
}

#[test]
fn the_switcher_cuts_a_long_channel_name_with_an_ellipsis() {
    let mut app = mockup_app();
    let long = "a-very-long-channel-name-for-gpu";
    app.channels.push(channel(long, None, 0));
    app.selected = Selection::Channel(long.to_string());
    app.posts.clear();
    for width in [40u16, 44] {
        let buf = render(&app, TRUECOLOR, width, 24);
        let row = row_text(&buf, 1);
        assert!(row.contains("# a-very-long") && row.contains('…'), "{row}");
        assert!(row.contains("… ›"), "the closing arrow stays: {row}");
    }
    let row = row_text(&render(&app, TRUECOLOR, 60, 24), 1);
    assert!(row.contains(&format!("‹ # {long} ›")), "{row}");
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

// ---------------------------------------------------------------------------
// Behaviour: App::update, keys, dialogs and the live feed (Task 11)
// ---------------------------------------------------------------------------

mod behaviour {
    use super::*;

    use beacon::api::{
        AttachmentJson, ChannelAck, ChannelJson, ChannelsJson, Delivery, PeerJson, PostCreated,
        PostJson, PostsResponse, PresenceJson, StreamEvent, WhoJson,
    };
    use beacon::model::{parse_mentions, AuthorKind};
    use beacon::tui::convert;
    use beacon::tui::keys::HELP;
    use beacon::tui::state::{Cmd, ExchangeFile, Loaded, Msg, FILTER_MAX, NAME_RULE, PAGE};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    const W: u16 = 104;
    const H: u16 = 33;

    fn key(code: KeyCode) -> Msg {
        Msg::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn chord(code: KeyCode, modifiers: KeyModifiers) -> Msg {
        Msg::Key(KeyEvent::new(code, modifiers))
    }

    fn ch(c: char) -> Msg {
        key(KeyCode::Char(c))
    }

    /// Feeds every message, returning all the commands they asked for.
    fn feed(app: &mut App, msgs: impl IntoIterator<Item = Msg>) -> Vec<Cmd> {
        msgs.into_iter().flat_map(|m| app.update(m)).collect()
    }

    fn type_text(app: &mut App, text: &str) -> Vec<Cmd> {
        feed(app, text.chars().map(ch))
    }

    fn ulid(n: u32) -> String {
        format!("01JAN{n:021}")
    }

    /// A post as the server sends it: `n` gives its id, arrival and minute.
    fn api_post(n: u32, channel: &str, author: &str, body: &str) -> PostJson {
        let id = ulid(n);
        PostJson {
            v: 1,
            short_id: id[id.len() - 8..].to_string(),
            id,
            channel: channel.to_string(),
            ts: String::new(),
            ts_ms: at(17, 0) + i64::from(n) * 60_000,
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

    fn announcement(n: u32, channel: &str, author: &str, body: &str) -> PostJson {
        let mut p = api_post(n, channel, author, body);
        p.author_kind = AuthorKind::Human;
        p.announcement = true;
        p.client = Some("tui".to_string());
        p
    }

    fn channels_json(list: &[(&str, u32)], announcements_unread: u32) -> ChannelsJson {
        ChannelsJson {
            v: 1,
            channels: list
                .iter()
                .map(|(name, unread)| ChannelJson {
                    name: name.to_string(),
                    topic: Some(format!("about {name}")),
                    unread: Some(*unread),
                })
                .collect(),
            announcements_unread: Some(announcements_unread),
        }
    }

    fn page(posts: Vec<PostJson>) -> Loaded {
        let view = Selection::Channel(posts[0].channel.clone());
        Loaded::Posts {
            view,
            before: None,
            page: Ok(PostsResponse {
                v: 1,
                posts,
                remaining: None,
            }),
        }
    }

    /// Dang's board after it connected: three channels, `general` open with
    /// two posts, the composer focused.
    fn live_app() -> App {
        let mut app = App::new("pc", "Dang", new_york());
        feed(
            &mut app,
            [
                Msg::Tick(at(18, 40)),
                Msg::Resize(W, H),
                Msg::Connection(Conn::Up),
            ],
        );
        let cmds = app.update(Msg::Loaded(Loaded::Channels(channels_json(
            &[("general", 0), ("beacon", 2), ("vllm", 3)],
            1,
        ))));
        assert_eq!(
            cmds,
            [Cmd::LoadPosts {
                channel: "general".to_string(),
                before: None
            }],
            "the board opens on its first channel"
        );
        app.update(Msg::Loaded(page(vec![
            api_post(1, "general", "pc-configs", "scaffolding the crate"),
            api_post(2, "general", "mac-juicebox", "arm64 builds"),
        ])));
        app
    }

    fn screen(app: &App) -> Buffer {
        render(app, TRUECOLOR, W, H)
    }

    fn screen_text(app: &App) -> String {
        rows(&screen(app)).join("\n")
    }

    fn created(post: PostJson) -> PostCreated {
        PostCreated {
            v: 1,
            post,
            delivery: vec![Delivery {
                node: "mac".to_string(),
                delivered: true,
            }],
        }
    }

    fn row_of(buf: &Buffer, needle: &str) -> u16 {
        (0..buf.area.height)
            .find(|&y| row_text(buf, y).contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} is not on screen:\n{}", rows(buf).join("\n")))
    }

    // ----------------------------------------------------------- composer

    #[test]
    fn enter_sends_and_clears_input() {
        let mut app = live_app();
        assert_eq!(app.focus, Focus::Composer);
        assert!(app.update(key(KeyCode::Enter)).is_empty(), "empty: nothing");
        type_text(&mut app, "   ");
        assert!(app.update(key(KeyCode::Enter)).is_empty(), "blank: nothing");
        app.input.clear();

        type_text(&mut app, "on my way");
        let cmds = app.update(key(KeyCode::Enter));
        assert_eq!(
            cmds,
            [Cmd::SendPost {
                channel: "general".to_string(),
                body: "on my way".to_string()
            }]
        );
        // Enter again while it is on its way does not post it twice.
        assert!(app.update(key(KeyCode::Enter)).is_empty());
        let mut mine = api_post(3, "general", "Dang", "on my way");
        mine.author_kind = AuthorKind::Human;
        mine.announcement = true;
        app.update(Msg::Sent(Ok(created(mine))));
        assert_eq!(app.input, "");
        assert!(app.posts.iter().any(|p| p.body == "on my way"));
        assert!(
            screen_text(&app).contains("message #general"),
            "placeholder back"
        );

        // A failure keeps the text and says why.
        type_text(&mut app, "again");
        assert_eq!(app.update(key(KeyCode::Enter)).len(), 1);
        app.update(Msg::Sent(Err(
            "cannot reach http://127.0.0.1:7700 (connection refused)".to_string(),
        )));
        assert_eq!(app.input, "again");
        assert_eq!(
            app.toast.as_deref(),
            Some("Not sent: cannot reach http://127.0.0.1:7700 (connection refused)")
        );
        assert!(screen_text(&app).contains("Not sent: cannot reach"));
    }

    #[test]
    fn text_typed_while_sending_stays() {
        let mut app = live_app();
        type_text(&mut app, "first");
        app.update(key(KeyCode::Enter));
        type_text(&mut app, " and more");
        app.update(Msg::Sent(Ok(created(api_post(
            3, "general", "Dang", "first",
        )))));
        assert_eq!(app.input, " and more");
    }

    #[test]
    fn shift_enter_alt_enter_and_ctrl_j_add_a_line() {
        let mut app = live_app();
        type_text(&mut app, "one");
        for newline in [
            chord(KeyCode::Enter, KeyModifiers::SHIFT),
            chord(KeyCode::Enter, KeyModifiers::ALT),
            chord(KeyCode::Char('j'), KeyModifiers::CONTROL),
        ] {
            assert!(app.update(newline).is_empty());
        }
        type_text(&mut app, "two");
        assert_eq!(app.input, "one\n\n\ntwo");
        app.update(key(KeyCode::Backspace));
        assert_eq!(app.input, "one\n\n\ntw");
    }

    #[test]
    fn paste_with_newlines_never_sends() {
        let mut app = live_app();
        let cmds = app.update(Msg::Paste("line one\r\nline two\n\x1b[31mred".to_string()));
        assert!(cmds.is_empty());
        assert_eq!(app.input, "line one\nline two\n[31mred");
    }

    #[test]
    fn q_in_composer_types_q() {
        let mut app = live_app();
        assert!(app.update(ch('q')).is_empty());
        assert_eq!(app.input, "q");
        // Outside the composer it quits.
        app.update(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Sidebar);
        assert_eq!(app.update(ch('q')), [Cmd::Quit]);
        // Ctrl-C quits from anywhere, dialogs included.
        let ctrl_c = || chord(KeyCode::Char('c'), KeyModifiers::CONTROL);
        app.focus = Focus::Composer;
        assert_eq!(app.update(ctrl_c()), [Cmd::Quit]);
        app.focus = Focus::Sidebar;
        app.update(ch('n'));
        assert!(app.dialog.is_some());
        assert!(app.update(ch('q')).is_empty(), "q is typed into the name");
        assert_eq!(app.update(ctrl_c()), [Cmd::Quit]);
    }

    #[test]
    fn printable_keys_outside_the_composer_jump_to_it() {
        let mut app = live_app();
        app.update(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Sidebar);
        app.update(ch('H'));
        assert_eq!(app.focus, Focus::Composer);
        type_text(&mut app, "i there");
        assert_eq!(app.input, "Hi there");
    }

    #[test]
    fn tab_cycles_focus_and_shows_it() {
        let mut app = live_app();
        let order: Vec<Focus> = (0..3)
            .map(|_| {
                app.update(key(KeyCode::Tab));
                app.focus
            })
            .collect();
        assert_eq!(order, [Focus::Sidebar, Focus::Posts, Focus::Composer]);
        app.update(key(KeyCode::BackTab));
        assert_eq!(app.focus, Focus::Posts);
        // Posts focused: the newest post is selected and drawn with its edge.
        assert_eq!(app.selected_post.as_deref(), Some(ulid(2).as_str()));
        let buf = screen(&app);
        let y = row_of(&buf, "mac-juicebox");
        let (x0, _) = main_pane(W);
        assert_eq!(buf[(x0, y)].symbol(), "▍");
        // Sidebar focused: its section label brightens.
        app.update(key(KeyCode::BackTab));
        let buf = screen(&app);
        let y = row_of(&buf, "CHANNELS");
        let x = column_of(&buf, y, "CHANNELS").unwrap();
        assert_eq!(buf[(x, y)].fg, rgb(palette::MIST));
        // Esc leaves the composer for the posts.
        app.focus = Focus::Composer;
        app.update(key(KeyCode::Esc));
        assert_eq!(app.focus, Focus::Posts);
    }

    // ------------------------------------------------------------ sidebar

    #[test]
    fn selecting_channel_loads_and_marks_read() {
        let mut app = live_app();
        app.update(key(KeyCode::Tab));
        let cmds = app.update(ch('j'));
        assert_eq!(
            cmds,
            [Cmd::LoadPosts {
                channel: "beacon".to_string(),
                before: None
            }]
        );
        assert_eq!(app.selected, Selection::Channel("beacon".to_string()));
        assert!(app.posts.is_empty() && app.loading);
        // While it loads the pane says nothing, rather than "Nothing in #beacon".
        assert!(!screen_text(&app).contains("Nothing in"));

        // A page for a view no longer on screen is dropped.
        let stale = app.update(Msg::Loaded(page(vec![api_post(7, "general", "x", "old")])));
        assert!(stale.is_empty() && app.posts.is_empty());

        let cmds = app.update(Msg::Loaded(page(vec![
            api_post(9, "beacon", "pc-configs", "nine"),
            api_post(5, "beacon", "pc-vllm", "five"),
        ])));
        assert_eq!(
            cmds,
            [Cmd::MarkRead {
                channel: "beacon".to_string(),
                arrival: 9
            }]
        );
        let beacon = app.channels.iter().find(|c| c.name == "beacon").unwrap();
        assert_eq!(beacon.unread, 0);
        assert_eq!(app.posts[0].body, "five", "oldest first");
        let buf = screen(&app);
        assert!(rows(&buf).iter().any(|r| r.contains("▍ # beacon")));

        // ↓ and ↑ work too, and the top of the list is Announcements.
        assert_eq!(
            app.update(key(KeyCode::Down)),
            [Cmd::LoadPosts {
                channel: "vllm".to_string(),
                before: None
            }]
        );
        assert!(app.update(ch('j')).is_empty(), "the last channel stays");
        feed(&mut app, [ch('k'), ch('k')]);
        assert_eq!(app.update(key(KeyCode::Up)), [Cmd::LoadAnnouncements]);
        assert_eq!(app.selected, Selection::Announcements);
        // Enter from the sidebar goes to the composer.
        app.update(key(KeyCode::Enter));
        assert_eq!(app.focus, Focus::Composer);
    }

    // ------------------------------------------------------------ dialogs

    #[test]
    fn new_channel_dialog_validates() {
        let mut app = live_app();
        app.update(key(KeyCode::Tab));
        app.update(ch('n'));
        assert!(matches!(app.dialog, Some(Dialog::NewChannel(_))));
        let form = |app: &App| match &app.dialog {
            Some(Dialog::NewChannel(form)) => form.clone(),
            other => panic!("not the form: {other:?}"),
        };

        // `#` is already drawn; capitals are lowercased as typed.
        type_text(&mut app, "#GPU-Jobs");
        assert_eq!(form(&app).name, "gpu-jobs");
        assert_eq!(form(&app).error, None);

        // A character a channel name can't have: the rule turns coral.
        app.update(ch('_'));
        assert_eq!(form(&app).error.as_deref(), Some(NAME_RULE));
        let buf = screen(&app);
        let y = row_of(&buf, NAME_RULE);
        let x = column_of(&buf, y, NAME_RULE).unwrap();
        assert_eq!(buf[(x, y)].fg, rgb(palette::CORAL));
        assert!(app.update(key(KeyCode::Enter)).is_empty(), "not sent");

        app.update(key(KeyCode::Backspace));
        assert_eq!(form(&app).error, None);
        let buf = screen(&app);
        let y = row_of(&buf, NAME_RULE);
        let x = column_of(&buf, y, NAME_RULE).unwrap();
        assert_eq!(buf[(x, y)].fg, rgb(palette::ASH), "the hint again");

        // No longer than 32 characters.
        type_text(&mut app, &"x".repeat(40));
        assert_eq!(form(&app).name.len(), 32);
        for _ in 0..24 {
            app.update(key(KeyCode::Backspace));
        }
        assert_eq!(form(&app).name, "gpu-jobs");

        app.update(key(KeyCode::Tab));
        type_text(&mut app, "long jobs on the 7900 GRE");
        assert_eq!(form(&app).field, FormField::Topic);
        let cmds = app.update(key(KeyCode::Enter));
        assert_eq!(
            cmds,
            [Cmd::CreateChannel {
                name: "gpu-jobs".to_string(),
                topic: Some("long jobs on the 7900 GRE".to_string())
            }]
        );
        assert!(app.dialog.is_none());

        // An empty name is not sent either; Esc closes.
        app.update(ch('n'));
        assert!(app.update(key(KeyCode::Enter)).is_empty());
        assert_eq!(form(&app).error.as_deref(), Some(NAME_RULE));
        app.update(key(KeyCode::Esc));
        assert!(app.dialog.is_none());

        // Once created, the new channel is opened.
        let cmds = app.update(Msg::Created(Ok(ChannelAck {
            v: 1,
            ok: true,
            created: true,
            channel: ChannelJson {
                name: "gpu-jobs".to_string(),
                topic: Some("long jobs".to_string()),
                unread: None,
            },
        })));
        assert_eq!(app.selected, Selection::Channel("gpu-jobs".to_string()));
        assert_eq!(app.focus, Focus::Composer);
        assert!(cmds.contains(&Cmd::LoadPosts {
            channel: "gpu-jobs".to_string(),
            before: None
        }));
        assert_eq!(app.toast.as_deref(), Some("created #gpu-jobs"));
    }

    #[test]
    fn delete_requires_confirm() {
        let mut app = live_app();
        feed(&mut app, [key(KeyCode::Tab), key(KeyCode::Tab)]);
        assert_eq!(app.focus, Focus::Posts);
        let id = app.selected_post.clone().unwrap();

        assert!(app.update(ch('d')).is_empty());
        let buf = screen(&app);
        let text = rows(&buf).join("\n");
        assert!(text.contains("Delete this post?"), "{text}");
        assert!(text.contains("mac-juicebox's post from 5:02 pm"), "{text}");
        assert!(text.contains("esc cancel   ↵ delete"));
        let y = row_of(&buf, "↵ delete");
        let x = column_of(&buf, y, "↵ delete").unwrap();
        assert_eq!(buf[(x, y)].bg, rgb(palette::LIME), "the one primary action");

        assert!(app.update(key(KeyCode::Esc)).is_empty());
        assert!(app.dialog.is_none() && app.confirm.is_none());
        assert_eq!(app.focus, Focus::Posts);
        assert!(app.posts.iter().any(|p| p.id == id), "nothing deleted");

        app.update(ch('d'));
        assert_eq!(
            app.update(key(KeyCode::Enter)),
            [Cmd::DeletePost(id.clone())]
        );
        assert!(app.dialog.is_none());
        app.update(Msg::PostDeleted(Ok(id.clone())));
        assert!(app.posts.iter().all(|p| p.id != id));
        assert_eq!(
            app.selected_post.as_deref(),
            Some(ulid(1).as_str()),
            "the selection moves to a neighbour"
        );

        // In the sidebar, `d` deletes the channel on screen.
        app.update(key(KeyCode::BackTab));
        assert_eq!(app.focus, Focus::Sidebar);
        app.update(ch('d'));
        let text = screen_text(&app);
        assert!(text.contains("Delete #general?"), "{text}");
        // The message wraps inside the dialog.
        for part in [
            "Its posts are hidden on every device until",
            "someone posts there again.",
        ] {
            assert!(text.contains(part), "{part:?} missing:\n{text}");
        }
        assert_eq!(
            app.update(key(KeyCode::Enter)),
            [Cmd::DeleteChannel("general".to_string())]
        );
        let cmds = app.update(Msg::ChannelDeleted(Ok("general".to_string())));
        assert_eq!(app.selected, Selection::Channel("beacon".to_string()));
        assert!(cmds.contains(&Cmd::LoadPosts {
            channel: "beacon".to_string(),
            before: None
        }));
        assert_eq!(app.toast.as_deref(), Some("deleted #general"));
    }

    #[test]
    fn attach_picker_uploads_with_the_composer_text() {
        let mut app = live_app();
        type_text(&mut app, "the log");
        app.update(key(KeyCode::Esc));
        assert_eq!(app.update(ch('a')), [Cmd::ListFiles]);
        assert!(screen_text(&app).contains("Reading ./exchange…"));
        let file = |name: &str, minutes_ago: i64| ExchangeFile {
            name: name.to_string(),
            path: format!("/exchange/{name}").into(),
            size: 12 * 1024,
            modified_ms: at(18, 40) - minutes_ago * 60_000,
        };
        app.update(Msg::Loaded(Loaded::Files(Ok(vec![
            file("newest.txt", 1),
            file("build.log", 30),
        ]))));
        let buf = screen(&app);
        let text = rows(&buf).join("\n");
        assert!(text.contains("Attach a file"));
        assert!(text.contains("newest.txt") && text.contains("build.log"));
        assert!(text.contains("with “the log”"), "{text}");
        let lime: Vec<_> = lime_cells(&buf).into_iter().collect();
        let y = row_of(&buf, "↵ attach");
        assert!(lime
            .iter()
            .all(|&(_, ly)| ly == y || ly == row_of(&buf, "›")));

        app.update(ch('j'));
        let cmds = app.update(key(KeyCode::Enter));
        assert_eq!(
            cmds,
            [Cmd::Upload {
                channel: "general".to_string(),
                path: "/exchange/build.log".into(),
                body: "the log".to_string()
            }]
        );
        assert!(app.dialog.is_none());

        // An empty folder says what to do; Announcements can't take a file.
        app.update(Msg::Sent(Err("boom".to_string())));
        app.update(ch('a'));
        app.update(Msg::Loaded(Loaded::Files(Ok(Vec::new()))));
        assert!(pane_text(&screen(&app)).contains("No files in ./exchange yet."));
        assert!(app.update(key(KeyCode::Enter)).is_empty());
        app.update(key(KeyCode::Esc));
        app.focus = Focus::Sidebar;
        app.update(ch('k'));
        assert_eq!(app.selected, Selection::Announcements);
        assert!(app.update(ch('a')).is_empty());
        assert!(app.dialog.is_none());
    }

    #[test]
    fn save_writes_the_selected_attachment() {
        let mut app = live_app();
        let mut with_file = api_post(3, "general", "mac-juicebox", "report attached");
        with_file.attachment = Some(AttachmentJson {
            name: "test-report.txt".to_string(),
            size: 12 * 1024,
            sha256: "ab".repeat(32),
            available: true,
        });
        app.update(Msg::Stream(StreamEvent::Post(with_file)));
        // `s` saves the post selected in the posts pane.
        app.focus = Focus::Sidebar;
        assert!(app.update(ch('s')).is_empty());
        assert!(app
            .toast
            .as_deref()
            .unwrap()
            .starts_with("Select a post first"));

        app.update(key(KeyCode::Tab));
        assert_eq!(app.selected_post.as_deref(), Some(ulid(3).as_str()));
        assert_eq!(
            app.update(ch('s')),
            [Cmd::Save {
                post_id: ulid(3),
                sha: "ab".repeat(32),
                name: "test-report.txt".to_string()
            }]
        );
        app.update(Msg::Saved(Ok("test-report (1).txt".to_string())));
        assert_eq!(
            app.toast.as_deref(),
            Some("saved to ./exchange/test-report (1).txt")
        );
        assert!(screen_text(&app).contains("saved to ./exchange/test-report (1).txt"));

        // A post without a file has nothing to save.
        app.update(ch('k'));
        assert!(app.update(ch('s')).is_empty());
        assert_eq!(app.toast.as_deref(), Some("This post has no file to save"));
    }

    #[test]
    fn help_overlay_lists_every_key() {
        let mut app = live_app();
        app.update(key(KeyCode::Tab));
        app.update(ch('?'));
        assert_eq!(app.dialog, Some(Dialog::Help));
        let text = screen_text(&app);
        for (keys, what) in HELP {
            assert!(text.contains(keys), "{keys} missing:\n{text}");
            assert!(text.contains(what), "{what} missing:\n{text}");
        }
        // The longest keys keep two spaces before what they do.
        assert!(
            text.contains("tab  shift-tab  channels, posts, composer"),
            "{text}"
        );
        assert!(lime_cells(&screen(&app)).len() <= 1, "no primary action");
        // At 80×24 it still fits.
        let buf = render(&app, TRUECOLOR, 80, 24);
        let small = rows(&buf).join("\n");
        assert!(
            small.contains("ctrl-c") && small.contains("esc close"),
            "{small}"
        );
        app.update(ch('x'));
        assert_eq!(app.dialog, None, "any key closes it");
        assert_eq!(app.input, "", "and does nothing else");
    }

    // ------------------------------------------------------------- filter

    #[test]
    fn filter_by_tag_or_handle() {
        let mut app = live_app();
        app.update(key(KeyCode::Tab));
        app.update(ch('/'));
        assert!(app.filter_editing);
        type_text(&mut app, "@pc-configs");
        let text = screen_text(&app);
        assert!(text.contains("filter /@pc-configs"), "{text}");
        assert!(
            text.contains("enter keep"),
            "the keys for it in the help row"
        );
        let pane = pane_text(&screen(&app));
        assert!(pane.contains("scaffolding") && !pane.contains("arm64"));
        app.update(key(KeyCode::Enter));
        assert!(!app.filter_editing);
        assert_eq!(app.filter.as_deref(), Some("@pc-configs"));
        // j/k keys are commands again, not filter text.
        app.update(ch('j'));
        assert_eq!(app.filter.as_deref(), Some("@pc-configs"));
        app.update(key(KeyCode::Esc));
        assert_eq!(app.filter, None);

        // A huge paste is cut to the cap and still draws.
        app.update(ch('/'));
        app.update(Msg::Paste("x".repeat(70_000)));
        assert_eq!(app.filter.as_ref().unwrap().chars().count(), FILTER_MAX);
        for (w, h) in [(W, H), (80, 24), (60, 20)] {
            render(&app, TRUECOLOR, w, h);
        }
        app.update(key(KeyCode::Esc));
        assert_eq!(app.filter, None);
        assert!(!app.filter_editing);
    }

    // -------------------------------------------------------------- live

    #[test]
    fn stream_post_appends_and_counts_unread() {
        let mut app = live_app();
        // In the channel on screen: shown, and read at once.
        let cmds = app.update(Msg::Stream(StreamEvent::Post(api_post(
            10,
            "general",
            "mac-juicebox",
            "@Dang e2e passed across both machines",
        ))));
        assert_eq!(
            cmds,
            [Cmd::MarkRead {
                channel: "general".to_string(),
                arrival: 10
            }]
        );
        assert_eq!(app.posts.last().unwrap().id, ulid(10));
        // A mention of Dang is drawn on the lifted band.
        let buf = screen(&app);
        let y = row_of(&buf, "e2e passed");
        let (x0, w) = main_pane(W);
        assert_eq!(buf[(x0 + 2, y)].bg, rgb(palette::OBSIDIAN));
        assert_eq!(buf[(x0 + w - 2, y)].bg, rgb(palette::OBSIDIAN));

        // Elsewhere: counted.
        let cmds = app.update(Msg::Stream(StreamEvent::Post(api_post(
            11, "vllm", "pc-vllm", "waiting",
        ))));
        assert!(cmds.is_empty());
        let unread =
            |app: &App, name: &str| app.channels.iter().find(|c| c.name == name).unwrap().unread;
        assert_eq!(unread(&app, "vllm"), 4);
        assert!(app.posts.iter().all(|p| p.id != ulid(11)));

        // An announcement from another human counts in its channel and in
        // Announcements.
        app.update(Msg::Stream(StreamEvent::Post(announcement(
            12,
            "beacon",
            "Kim",
            "freeze at 6",
        ))));
        assert_eq!(app.announcements_unread, 2);
        assert_eq!(unread(&app, "beacon"), 3);
        assert!(rows(&screen(&app))
            .iter()
            .any(|r| r.contains("◆ Announcements   2")));

        // Dang's own posts are never unread.
        app.update(Msg::Stream(StreamEvent::Post(announcement(
            13,
            "vllm",
            "dang",
            "from the mac",
        ))));
        assert_eq!(unread(&app, "vllm"), 4);
        assert_eq!(app.announcements_unread, 2);

        // The same post twice is shown once.
        app.update(Msg::Stream(StreamEvent::Post(api_post(
            10,
            "general",
            "mac-juicebox",
            "@Dang e2e passed across both machines",
        ))));
        assert_eq!(app.posts.iter().filter(|p| p.id == ulid(10)).count(), 1);

        // A post in a channel the TUI doesn't know (revived) reloads the list.
        let cmds = app.update(Msg::Stream(StreamEvent::Post(api_post(
            14,
            "revived",
            "pc-configs",
            "back",
        ))));
        assert!(cmds.contains(&Cmd::LoadChannels));
        assert_eq!(unread(&app, "revived"), 1);

        // Deleted elsewhere: gone from the screen.
        app.update(Msg::Stream(StreamEvent::PostDeleted { id: ulid(10) }));
        assert!(app.posts.iter().all(|p| p.id != ulid(10)));
        assert!(!screen_text(&app).contains("e2e passed"));
    }

    #[test]
    fn a_page_keeps_what_arrived_while_it_loaded() {
        let mut app = live_app();
        app.focus = Focus::Sidebar;
        app.update(ch('j'));
        // While #beacon's page is on its way, a post arrives and one goes.
        app.update(Msg::Stream(StreamEvent::Post(api_post(
            20, "beacon", "pc-vllm", "live one",
        ))));
        app.update(Msg::Stream(StreamEvent::PostDeleted { id: ulid(5) }));
        app.update(Msg::Loaded(page(vec![
            api_post(5, "beacon", "pc-configs", "deleted meanwhile"),
            api_post(6, "beacon", "pc-configs", "kept"),
        ])));
        let bodies: Vec<&str> = app.posts.iter().map(|p| p.body.as_str()).collect();
        assert_eq!(bodies, ["kept", "live one"]);
    }

    #[test]
    fn channel_events_update_the_sidebar() {
        let mut app = live_app();
        let cmds = app.update(Msg::Stream(StreamEvent::Channel {
            channel: ChannelJson {
                name: "gpu-jobs".to_string(),
                topic: Some("long jobs".to_string()),
                unread: None,
            },
            deleted: false,
        }));
        assert_eq!(cmds, [Cmd::LoadChannels]);
        assert!(app.channels.iter().any(|c| c.name == "gpu-jobs"));
        // The channel on screen deleted elsewhere: says so and moves on.
        let cmds = app.update(Msg::Stream(StreamEvent::Channel {
            channel: ChannelJson {
                name: "general".to_string(),
                topic: None,
                unread: None,
            },
            deleted: true,
        }));
        assert_eq!(app.selected, Selection::Channel("beacon".to_string()));
        assert!(cmds.contains(&Cmd::LoadChannels));
        assert_eq!(app.toast.as_deref(), Some("#general was deleted"));
    }

    #[test]
    fn presence_and_peers_reach_the_sidebar_and_header() {
        let mut app = live_app();
        let seen = |handle: &str, kind: AuthorKind, ms: i64| PresenceJson {
            handle: handle.to_string(),
            kind,
            client: "claude-code".to_string(),
            node: "pc".to_string(),
            status: None,
            last_seen: String::new(),
            last_seen_ms: ms,
            active: true,
        };
        app.update(Msg::Loaded(Loaded::Agents(WhoJson {
            v: 1,
            agents: vec![
                seen("pc-configs", AuthorKind::Agent, at(18, 39)),
                seen("Dang", AuthorKind::Human, at(18, 40)),
            ],
        })));
        assert_eq!(app.agents.len(), 1, "Dang is not an agent");
        app.update(Msg::Stream(StreamEvent::Presence(seen(
            "mac-juicebox",
            AuthorKind::Agent,
            at(18, 40),
        ))));
        let handles: Vec<&str> = app.agents.iter().map(|a| a.handle.as_str()).collect();
        assert_eq!(handles, ["mac-juicebox", "pc-configs"]);
        // Ten minutes later both are idle, and three hours later gone.
        app.update(Msg::Tick(at(18, 50)));
        assert!(app.agents.iter().all(|a| !a.active));
        app.update(Msg::Tick(at(21, 50)));
        assert!(app.agents.is_empty());

        app.update(Msg::Stream(StreamEvent::Peers(vec![PeerJson {
            node: Some("mac".to_string()),
            url: "http://mac:7700".to_string(),
            reachable: false,
            last_sync: Some("2026-10-21T22:39:00.000Z".to_string()),
            queued: 3,
        }])));
        assert!(screen_text(&app).contains("1 offline · 3 queued"));
    }

    #[test]
    fn server_down_then_back_reloads() {
        let mut app = live_app();
        let cmds = app.update(Msg::Connection(Conn::Down {
            since_ms: at(18, 41),
        }));
        assert!(cmds.is_empty());
        assert!(pane_text(&screen(&app)).contains("Can't reach the beacon server"));
        // Retries failing keep the first time.
        app.update(Msg::Connection(Conn::Down {
            since_ms: at(18, 45),
        }));
        assert_eq!(
            app.connection,
            Conn::Down {
                since_ms: at(18, 41)
            }
        );
        // No heartbeats while it is down.
        assert!(app.update(Msg::Tick(at(18, 50))).is_empty());

        let cmds = app.update(Msg::Connection(Conn::Up));
        for want in [
            Cmd::Heartbeat,
            Cmd::LoadChannels,
            Cmd::LoadAgents,
            Cmd::LoadStatus,
            Cmd::LoadPosts {
                channel: "general".to_string(),
                before: None,
            },
        ] {
            assert!(cmds.contains(&want), "{want:?} not in {cmds:?}");
        }
        assert!(!pane_text(&screen(&app)).contains("Can't reach"));
        assert!(!app.loading, "the old posts stay up while they reload");
    }

    #[test]
    fn heartbeat_every_minute_while_connected() {
        let mut app = live_app();
        let beats = |app: &mut App, at_ms: i64| {
            app.update(Msg::Tick(at_ms))
                .into_iter()
                .filter(|c| *c == Cmd::Heartbeat)
                .count()
        };
        let start = at(18, 40);
        assert_eq!(beats(&mut app, start + 30_000), 0);
        assert_eq!(beats(&mut app, start + 60_000), 1);
        assert_eq!(beats(&mut app, start + 61_000), 0);
        assert_eq!(beats(&mut app, start + 120_000), 1);
        // The toast goes away after a while.
        app.toast = Some("x".to_string());
        app.toast_until_ms = Some(start + 125_000);
        app.update(Msg::Tick(start + 126_000));
        assert_eq!(app.toast, None);
    }

    // ------------------------------------------------------------ scrolling

    fn full_page_app() -> App {
        let mut app = live_app();
        app.focus = Focus::Sidebar;
        app.update(ch('j'));
        let posts: Vec<PostJson> = (100..100 + PAGE)
            .map(|n| api_post(n, "beacon", "pc-configs", &format!("post {n}")))
            .collect();
        app.update(Msg::Loaded(page(posts)));
        app.focus = Focus::Posts;
        app
    }

    #[test]
    fn pgup_scrolls_and_loads_older_pages() {
        let mut app = full_page_app();
        assert!(app.has_older);
        assert!(app.update(key(KeyCode::PageUp)).is_empty());
        let one_page = app.scroll;
        assert!(one_page > 0);
        assert!(screen_text(&app).contains("↓ newer posts below"));
        app.update(key(KeyCode::PageDown));
        assert_eq!(app.scroll, 0);

        // Reaching the top asks for the page before the oldest post, once.
        let mut asked = Vec::new();
        for _ in 0..200 {
            asked.extend(app.update(key(KeyCode::PageUp)));
        }
        let oldest = app.posts[0].clone();
        assert_eq!(
            asked,
            [Cmd::LoadPosts {
                channel: "beacon".to_string(),
                before: Some((oldest.ts_ms, oldest.id.clone()))
            }]
        );
        let scroll = app.scroll;
        let older: Vec<PostJson> = (50..60)
            .map(|n| api_post(n, "beacon", "pc-configs", &format!("older {n}")))
            .collect();
        app.update(Msg::Loaded(Loaded::Posts {
            view: Selection::Channel("beacon".to_string()),
            before: Some((oldest.ts_ms, oldest.id)),
            page: Ok(PostsResponse {
                v: 1,
                posts: older,
                remaining: None,
            }),
        }));
        assert_eq!(app.posts.len(), PAGE as usize + 10);
        assert_eq!(app.posts[0].body, "older 50");
        assert!(!app.has_older, "a short page is the last");
        assert_eq!(app.scroll, scroll, "the view stays where it was");
        assert!(app.update(key(KeyCode::PageUp)).is_empty());
    }

    #[test]
    fn selection_scrolls_into_view_and_k_past_the_top_loads_more() {
        let mut app = full_page_app();
        app.update(key(KeyCode::Tab));
        app.update(key(KeyCode::BackTab));
        assert_eq!(app.selected_post.as_deref(), Some(ulid(199).as_str()));
        let mut asked = Vec::new();
        for _ in 0..PAGE {
            asked.extend(app.update(ch('k')));
        }
        assert_eq!(app.selected_post.as_deref(), Some(ulid(100).as_str()));
        let buf = screen(&app);
        assert!(rows(&buf).iter().any(|r| r.contains("post 100")));
        assert_eq!(asked.len(), 1, "one older page asked for: {asked:?}");
        // Back down to the newest returns to the bottom.
        for _ in 0..PAGE {
            app.update(ch('j'));
        }
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn a_live_post_does_not_move_a_scrolled_view() {
        let mut app = full_page_app();
        app.update(key(KeyCode::PageUp));
        let before = pane_text(&screen(&app));
        app.update(Msg::Stream(StreamEvent::Post(api_post(
            500,
            "beacon",
            "pc-vllm",
            "new at the bottom",
        ))));
        assert_eq!(pane_text(&screen(&app)), before);
        app.update(key(KeyCode::PageDown));
        app.update(key(KeyCode::PageDown));
        assert!(pane_text(&screen(&app)).contains("new at the bottom"));
    }

    // ---------------------------------------------------------- conversion

    #[test]
    fn api_types_become_view_items() {
        let mut p = api_post(1, "beacon", "pc-\x1b[31mconfigs", "body\n\tkept");
        p.client = Some("claude\ncode".to_string());
        p.mentions = vec!["Dang".to_string()];
        p.attachment = Some(AttachmentJson {
            name: "re\x07port.txt".to_string(),
            size: 9,
            sha256: "cd".repeat(32),
            available: false,
        });
        let item = convert::post_item(&p);
        assert_eq!(item.author, "pc-[31mconfigs", "no control characters");
        assert_eq!(item.client.as_deref(), Some("claudecode"));
        assert_eq!(item.body, "body\n\tkept", "bodies keep lines for wrapping");
        assert_eq!(item.mentions, ["dang"]);
        assert_eq!(item.attachment.as_ref().unwrap().name, "report.txt");
        let extra = convert::post_extra(&p);
        assert_eq!(extra.arrival, 1);
        assert_eq!(extra.sha256.as_deref(), Some("cd".repeat(32).as_str()));
        assert!(!extra.available);

        let ch = convert::channel_item(&ChannelJson {
            name: "beacon".to_string(),
            topic: Some("  ".to_string()),
            unread: None,
        });
        assert_eq!((ch.topic, ch.unread), (None, 0));

        let peers = [
            PeerJson {
                node: None,
                url: "http://soybean-wsl:7700/".to_string(),
                reachable: true,
                last_sync: Some("2026-10-21T22:39:59.600Z".to_string()),
                queued: 0,
            },
            PeerJson {
                node: Some("mac".to_string()),
                url: "http://mac:7700".to_string(),
                reachable: true,
                last_sync: Some("2026-10-21T22:30:00.000Z".to_string()),
                queued: 0,
            },
        ];
        assert_eq!(convert::peer_item(&peers[0]).node, "soybean-wsl");
        assert_eq!(convert::last_sync_ms(&peers), Some(at(18, 40) - 400));
    }

    #[test]
    fn behaviour_states_render_at_every_size() {
        let mut apps = Vec::new();
        let mut app = live_app();
        app.update(ch('?'));
        apps.push(app.clone());
        app.dialog = None;
        app.focus = Focus::Posts;
        app.update(ch('a'));
        apps.push(app.clone());
        app.update(Msg::Loaded(Loaded::Files(Ok((0..40)
            .map(|n| ExchangeFile {
                name: format!("{}-{n}.log", "very-long-file-name".repeat(4)),
                path: "/exchange/x".into(),
                size: n * 1_000_000,
                modified_ms: 0,
            })
            .collect()))));
        apps.push(app.clone());
        app.update(Msg::Loaded(Loaded::Files(Err(
            "permission denied".to_string()
        ))));
        apps.push(app.clone());
        app.dialog = None;
        app.update(ch('/'));
        apps.push(app);
        for app in &apps {
            for width in [0u16, 10, 40, 60, 79, 80, 104, 160] {
                for height in [0u16, 5, 12, 14, 24, 33] {
                    let buf = render(app, TRUECOLOR, width, height);
                    for cell in buf.content() {
                        assert_ne!(cell.bg, Color::Reset, "{width}x{height}");
                    }
                }
            }
        }
    }

    // ------------------------------------------------------ fix round 1

    fn beacon_view() -> Selection {
        Selection::Channel("beacon".to_string())
    }

    fn older_page(before: (i64, String), range: std::ops::Range<u32>) -> Msg {
        Msg::Loaded(Loaded::Posts {
            view: beacon_view(),
            before: Some(before),
            page: Ok(PostsResponse {
                v: 1,
                posts: range
                    .map(|n| api_post(n, "beacon", "pc-configs", &format!("post {n}")))
                    .collect(),
                remaining: None,
            }),
        })
    }

    /// Pages up until the view asks for older posts; returns what it asked.
    fn page_to_top(app: &mut App) -> Vec<Cmd> {
        (0..300)
            .flat_map(|_| app.update(key(KeyCode::PageUp)))
            .collect()
    }

    fn ts_of(n: u32) -> i64 {
        at(17, 0) + i64::from(n) * 60_000
    }

    fn unread_of(app: &App, name: &str) -> u32 {
        app.channels.iter().find(|c| c.name == name).unwrap().unread
    }

    fn mark_reads(cmds: &[Cmd]) -> Vec<Cmd> {
        cmds.iter()
            .filter(|c| matches!(c, Cmd::MarkRead { .. }))
            .cloned()
            .collect()
    }

    #[test]
    fn a_late_synced_post_does_not_skip_history() {
        let mut app = full_page_app();
        // Written offline long ago, synced now: an old ts and a new arrival.
        let mut late = api_post(500, "beacon", "pc-vllm", "written offline");
        late.ts_ms = ts_of(10);
        app.update(Msg::Stream(StreamEvent::Post(late.clone())));
        // It belongs above the loaded page, which paging brings in order.
        assert!(app.posts.iter().all(|p| p.id != ulid(500)));
        assert_eq!(
            page_to_top(&mut app),
            [Cmd::LoadPosts {
                channel: "beacon".to_string(),
                before: Some((ts_of(100), ulid(100)))
            }],
            "the page before the oldest *page* post, not before the late one"
        );

        // The same while a fresh view's first page is on its way.
        app.focus = Focus::Sidebar;
        app.update(ch('j'));
        let mut late_vllm = api_post(600, "vllm", "pc-vllm", "also offline");
        late_vllm.ts_ms = ts_of(10);
        app.update(Msg::Stream(StreamEvent::Post(late_vllm)));
        let posts: Vec<PostJson> = (100..100 + PAGE)
            .map(|n| api_post(n, "vllm", "pc-configs", &format!("v {n}")))
            .collect();
        app.update(Msg::Loaded(page(posts)));
        assert!(app.posts.iter().all(|p| p.id != ulid(600)));
        assert_eq!(app.posts.len(), PAGE as usize);

        // Where the whole history is loaded, a late post goes in its place.
        let mut app = live_app();
        let mut late = api_post(700, "general", "pc-vllm", "late but complete");
        late.ts_ms = ts_of(0) - 60_000;
        app.update(Msg::Stream(StreamEvent::Post(late)));
        assert_eq!(app.posts[0].id, ulid(700));
    }

    #[test]
    fn an_older_page_from_before_a_switch_is_dropped() {
        let mut app = full_page_app();
        let asked = page_to_top(&mut app);
        let Some(Cmd::LoadPosts {
            before: Some(stale),
            ..
        }) = asked.first().cloned()
        else {
            panic!("asked {asked:?}")
        };
        // Away and back before that page comes: #beacon is loaded afresh,
        // newer posts having arrived meanwhile.
        app.focus = Focus::Sidebar;
        app.update(ch('j'));
        app.update(ch('k'));
        assert_eq!(app.selected, beacon_view());
        let fresh: Vec<PostJson> = (150..150 + PAGE)
            .map(|n| api_post(n, "beacon", "pc-configs", &format!("post {n}")))
            .collect();
        app.update(Msg::Loaded(page(fresh)));
        // The page asked for before the switch arrives now: it is not next to
        // this view's oldest post, so it is dropped.
        app.update(older_page(stale, 0..PAGE));
        assert_eq!(app.posts.len(), PAGE as usize);
        assert_eq!(app.posts[0].body, "post 150");
        app.focus = Focus::Posts;
        assert_eq!(
            page_to_top(&mut app),
            [Cmd::LoadPosts {
                channel: "beacon".to_string(),
                before: Some((ts_of(150), ulid(150)))
            }]
        );
        // The page that is asked for is taken.
        app.update(older_page((ts_of(150), ulid(150)), 50..150));
        assert_eq!(app.posts.len(), 2 * PAGE as usize);
        assert_eq!(app.posts[0].body, "post 50");
    }

    #[test]
    fn d_and_s_never_act_on_a_post_the_filter_hides() {
        let mut app = live_app();
        let mut with_file = api_post(3, "general", "mac-juicebox", "report attached");
        with_file.attachment = Some(AttachmentJson {
            name: "r.txt".to_string(),
            size: 3,
            sha256: "ab".repeat(32),
            available: true,
        });
        app.update(Msg::Stream(StreamEvent::Post(with_file)));
        feed(&mut app, [key(KeyCode::Tab), key(KeyCode::Tab)]);
        assert_eq!(app.selected_post.as_deref(), Some(ulid(3).as_str()));
        // Filtering to pc-configs hides it: the selection moves to a shown post.
        app.update(ch('/'));
        type_text(&mut app, "@pc-configs");
        app.update(key(KeyCode::Enter));
        assert_eq!(app.selected_post.as_deref(), Some(ulid(1).as_str()));
        app.update(ch('d'));
        assert_eq!(app.update(key(KeyCode::Enter)), [Cmd::DeletePost(ulid(1))]);
        // A selection left on a hidden post is never acted on.
        app.selected_post = Some(ulid(3));
        assert!(app.update(ch('d')).is_empty());
        assert!(app.dialog.is_none());
        assert!(app.update(ch('s')).is_empty());
        assert!(app
            .toast
            .as_deref()
            .unwrap()
            .starts_with("Select a post first"));
    }

    #[test]
    fn s_asks_the_node_even_if_the_file_had_not_synced_at_load() {
        let mut app = live_app();
        let mut with_file = api_post(3, "general", "mac-juicebox", "report attached");
        with_file.attachment = Some(AttachmentJson {
            name: "r.txt".to_string(),
            size: 3,
            sha256: "ab".repeat(32),
            available: false,
        });
        app.update(Msg::Stream(StreamEvent::Post(with_file)));
        feed(&mut app, [key(KeyCode::Tab), key(KeyCode::Tab)]);
        // The bytes may have come since: only the node knows.
        assert_eq!(
            app.update(ch('s')),
            [Cmd::Save {
                post_id: ulid(3),
                sha: "ab".repeat(32),
                name: "r.txt".to_string()
            }]
        );
        app.update(Msg::Saved(Err(
            "attachment not synced yet; try again when a peer that has it is online".to_string(),
        )));
        assert_eq!(
            app.toast.as_deref(),
            Some(
                "Not saved: attachment not synced yet; try again when a peer that has it is online"
            )
        );
    }

    #[test]
    fn a_burst_of_live_posts_is_cheap_and_marks_read_once() {
        let mut app = full_page_app();
        app.focus = Focus::Composer;
        let started = std::time::Instant::now();
        let mut cmds = Vec::new();
        for n in 1000..1500 {
            cmds.extend(app.update(Msg::Stream(StreamEvent::Post(api_post(
                n, "beacon", "pc-vllm", "burst",
            )))));
        }
        let cmds = beacon::tui::state::coalesce(cmds);
        assert_eq!(
            mark_reads(&cmds),
            [Cmd::MarkRead {
                channel: "beacon".to_string(),
                arrival: 1499
            }],
            "one cursor update for the whole batch"
        );
        assert_eq!(app.posts.len(), PAGE as usize + 500);
        // Scrolled up, a burst keeps the view still, and stays cheap too.
        app.update(key(KeyCode::PageUp));
        let before = pane_text(&screen(&app));
        for n in 1500..2000 {
            app.update(Msg::Stream(StreamEvent::Post(api_post(
                n, "beacon", "pc-vllm", "burst",
            ))));
        }
        assert_eq!(pane_text(&screen(&app)), before);
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "1000 live posts took {elapsed:?}"
        );
    }

    #[test]
    fn coalesce_keeps_one_cursor_per_channel_and_one_list() {
        let mark = |channel: &str, arrival| Cmd::MarkRead {
            channel: channel.to_string(),
            arrival,
        };
        let cmds = vec![
            mark("a", 3),
            Cmd::LoadChannels,
            mark("b", 9),
            mark("a", 7),
            Cmd::LoadChannels,
            Cmd::Heartbeat,
            mark("a", 5),
        ];
        assert_eq!(
            beacon::tui::state::coalesce(cmds),
            [
                mark("a", 7),
                Cmd::LoadChannels,
                mark("b", 9),
                Cmd::Heartbeat
            ]
        );
    }

    #[test]
    fn a_rejected_secret_gets_its_own_band() {
        let mut app = live_app();
        app.update(Msg::SecretRejected);
        app.update(Msg::Connection(Conn::Down {
            since_ms: at(18, 41),
        }));
        let text = pane_text(&screen(&app));
        assert!(
            text.contains("The server rejected BEACON_SECRET. Check .env on this device."),
            "{text}"
        );
        assert!(!text.contains("docker compose"), "{text}");
        // The edge rule is coral, as for the server-down band.
        let buf = screen(&app);
        let y = row_of(&buf, "The server rejected");
        let (x0, _) = main_pane(W);
        assert_eq!(buf[(x0, y)].fg, rgb(palette::CORAL));
        // Once the feed opens again, it goes.
        app.update(Msg::Connection(Conn::Up));
        assert!(!pane_text(&screen(&app)).contains("rejected"));
        // A plain outage after that is the ordinary band again.
        app.update(Msg::Connection(Conn::Down {
            since_ms: at(18, 50),
        }));
        assert!(pane_text(&screen(&app)).contains("docker compose up -d"));
    }

    #[test]
    fn live_posts_are_read_only_at_the_bottom() {
        let mut app = full_page_app();
        app.update(key(KeyCode::PageUp));
        assert!(app.scroll > 0);
        let cmds = app.update(Msg::Stream(StreamEvent::Post(api_post(
            300,
            "beacon",
            "pc-vllm",
            "while scrolled up",
        ))));
        assert!(mark_reads(&cmds).is_empty(), "{cmds:?}");
        assert_eq!(unread_of(&app, "beacon"), 1, "unread until seen");
        // Back down to the newest: now it has been seen.
        let cmds = feed(&mut app, (0..5).map(|_| key(KeyCode::PageDown)));
        assert_eq!(app.scroll, 0);
        assert_eq!(
            mark_reads(&cmds),
            [Cmd::MarkRead {
                channel: "beacon".to_string(),
                arrival: 300
            }]
        );
        assert_eq!(unread_of(&app, "beacon"), 0);
        // At the bottom, a live post is read as it arrives.
        let cmds = app.update(Msg::Stream(StreamEvent::Post(api_post(
            301,
            "beacon",
            "pc-vllm",
            "at the bottom",
        ))));
        assert_eq!(
            mark_reads(&cmds),
            [Cmd::MarkRead {
                channel: "beacon".to_string(),
                arrival: 301
            }]
        );
        // j down to the newest post counts as reaching the bottom too.
        app.update(key(KeyCode::PageUp));
        app.update(Msg::Stream(StreamEvent::Post(api_post(
            302, "beacon", "pc-vllm", "again",
        ))));
        let cmds = feed(&mut app, (0..300).map(|_| ch('j')));
        assert_eq!(
            mark_reads(&cmds),
            [Cmd::MarkRead {
                channel: "beacon".to_string(),
                arrival: 302
            }]
        );
    }

    // ------------------------------------------------------ polish (Task 14)

    fn beacon_posts(range: std::ops::Range<u32>) -> Vec<PostJson> {
        range
            .map(|n| api_post(n, "beacon", "pc-configs", &format!("post {n}")))
            .collect()
    }

    fn ask_before(n: u32) -> Cmd {
        Cmd::LoadPosts {
            channel: "beacon".to_string(),
            before: Some((ts_of(n), ulid(n))),
        }
    }

    fn reconnect(app: &mut App) {
        app.update(Msg::Connection(Conn::Down {
            since_ms: at(18, 41),
        }));
        app.update(Msg::Connection(Conn::Up));
    }

    #[test]
    fn a_reconnect_reload_never_leaves_paging_stuck() {
        let mut app = full_page_app();
        reconnect(&mut app);
        // Paging up before the fresh page lands asks from the old cursor.
        assert_eq!(page_to_top(&mut app), [ask_before(100)]);
        // The fresh page starts later (posts came in while the feed was down),
        // so the page from the old cursor no longer fits, and is dropped.
        app.update(Msg::Loaded(page(beacon_posts(150..150 + PAGE))));
        app.update(older_page((ts_of(100), ulid(100)), 0..PAGE));
        assert_eq!(app.posts.len(), PAGE as usize);
        assert_eq!(app.posts[0].body, "post 150");
        // Paging still works, from the fresh page's oldest post.
        assert_eq!(page_to_top(&mut app), [ask_before(150)]);
    }

    #[test]
    fn an_older_page_asked_for_during_a_reload_lands_if_it_still_fits() {
        let mut app = full_page_app();
        reconnect(&mut app);
        assert_eq!(page_to_top(&mut app), [ask_before(100)]);
        // Nothing new came in: the fresh page starts where the old one did,
        // so the older page on its way still fits and is not asked twice.
        app.update(Msg::Loaded(page(beacon_posts(100..100 + PAGE))));
        assert!(page_to_top(&mut app).is_empty());
        app.update(older_page((ts_of(100), ulid(100)), 0..PAGE));
        assert_eq!(app.posts.len(), 2 * PAGE as usize);
        assert_eq!(page_to_top(&mut app), [ask_before(0)]);
    }

    #[test]
    fn a_late_post_while_an_older_page_loads_shows_when_it_lands() {
        let mut app = full_page_app();
        assert_eq!(page_to_top(&mut app), [ask_before(100)]);
        // Written offline, synced now: it belongs among the posts that page
        // brings, but the node may have read that page before it arrived.
        let mut late = api_post(500, "beacon", "pc-vllm", "written offline");
        late.ts_ms = ts_of(60) + 30_000;
        app.update(Msg::Stream(StreamEvent::Post(late)));
        let mut gone = api_post(501, "beacon", "pc-vllm", "deleted meanwhile");
        gone.ts_ms = ts_of(70) + 30_000;
        app.update(Msg::Stream(StreamEvent::Post(gone)));
        app.update(Msg::Stream(StreamEvent::PostDeleted { id: ulid(501) }));
        assert!(
            app.posts.iter().all(|p| p.id != ulid(500)),
            "not before its page"
        );

        // The page lands without it (the rest of the channel, 50 to 99).
        app.update(older_page((ts_of(100), ulid(100)), 50..100));
        let at = app
            .posts
            .iter()
            .position(|p| p.id == ulid(500))
            .expect("shown once its page is in");
        assert_eq!(app.posts[at - 1].id, ulid(60));
        assert_eq!(app.posts[at + 1].id, ulid(61));
        assert!(
            app.posts.iter().all(|p| p.id != ulid(501)),
            "deleted stays gone"
        );
        assert_eq!(
            app.extras.get(&ulid(500)).map(|e| e.arrival),
            Some(500),
            "with its arrival, to mark read by"
        );

        // One older than the page brings waits for the next page, which is
        // asked for from the page's own oldest post.
        let mut app = full_page_app();
        page_to_top(&mut app);
        let mut early = api_post(502, "beacon", "pc-vllm", "older still");
        early.ts_ms = ts_of(0) - 60_000;
        app.update(Msg::Stream(StreamEvent::Post(early)));
        app.update(older_page((ts_of(100), ulid(100)), 0..PAGE));
        assert!(app.posts.iter().all(|p| p.id != ulid(502)));
        assert_eq!(page_to_top(&mut app), [ask_before(0)]);
    }

    #[test]
    fn clearing_the_filter_marks_what_it_hid_as_read() {
        for keep in [true, false] {
            let mut app = live_app();
            app.focus = Focus::Posts;
            app.update(ch('/'));
            type_text(&mut app, "@pc-configs");
            if keep {
                app.update(key(KeyCode::Enter));
            }
            // A post the filter hides has not been seen.
            let cmds = app.update(Msg::Stream(StreamEvent::Post(api_post(
                3,
                "general",
                "mac-juicebox",
                "hidden by the filter",
            ))));
            assert!(mark_reads(&cmds).is_empty(), "{cmds:?}");
            assert_eq!(unread_of(&app, "general"), 1);
            // Esc clears the filter: at the bottom, the post is on screen now.
            let cmds = app.update(key(KeyCode::Esc));
            assert_eq!(app.filter, None);
            assert_eq!(
                mark_reads(&cmds),
                [Cmd::MarkRead {
                    channel: "general".to_string(),
                    arrival: 3
                }],
                "keep: {keep}"
            );
            assert_eq!(unread_of(&app, "general"), 0);
        }
    }

    #[test]
    fn back_at_the_bottom_by_resize_or_delete_marks_read() {
        let general = |arrival| Cmd::MarkRead {
            channel: "general".to_string(),
            arrival,
        };
        let mut app = live_app();
        // Three rows of posts: the two posts need five.
        app.update(Msg::Resize(W, 16));
        app.update(key(KeyCode::PageUp));
        assert!(app.scroll > 0);
        let cmds = app.update(Msg::Stream(StreamEvent::Post(api_post(
            3,
            "general",
            "pc-vllm",
            "while scrolled up",
        ))));
        assert!(mark_reads(&cmds).is_empty());
        assert_eq!(unread_of(&app, "general"), 1);
        // A taller terminal fits every post: back at the bottom, it is seen.
        let cmds = app.update(Msg::Resize(W, H));
        assert_eq!(app.scroll, 0);
        assert_eq!(mark_reads(&cmds), [general(3)]);
        assert_eq!(unread_of(&app, "general"), 0);

        // The same when deletions bring the view back down.
        app.update(Msg::Resize(W, 16));
        app.update(key(KeyCode::PageUp));
        app.update(Msg::Stream(StreamEvent::Post(api_post(
            4, "general", "pc-vllm", "again",
        ))));
        assert_eq!(unread_of(&app, "general"), 1);
        let cmds = feed(
            &mut app,
            [1, 2, 3].map(|n| Msg::Stream(StreamEvent::PostDeleted { id: ulid(n) })),
        );
        assert_eq!(app.scroll, 0);
        assert_eq!(mark_reads(&cmds), [general(4)]);
        assert_eq!(unread_of(&app, "general"), 0);
    }

    #[test]
    fn failures_are_coral_and_notices_are_not() {
        let toast_ink = |app: &App, needle: &str| {
            let buf = screen(app);
            let y = row_of(&buf, needle);
            let x = column_of(&buf, y, needle).unwrap();
            buf[(x, y)].fg
        };
        let mut app = live_app();
        app.update(Msg::Saved(Ok("report.txt".to_string())));
        assert_eq!(
            toast_ink(&app, "saved to ./exchange/report.txt"),
            rgb(palette::MIST)
        );
        app.update(Msg::Saved(Err("not synced to this device yet".to_string())));
        assert_eq!(
            toast_ink(&app, "Not saved: not synced"),
            rgb(palette::CORAL)
        );
        app.update(Msg::Sent(Err("cannot reach the node".to_string())));
        assert_eq!(
            toast_ink(&app, "Not sent: cannot reach"),
            rgb(palette::CORAL)
        );
        // Waiting on the node is no failure, however long the notice stays.
        let mut app = live_app();
        let mut with_file = api_post(3, "general", "pc-vllm", "the report");
        with_file.attachment = Some(AttachmentJson {
            name: "report.txt".to_string(),
            size: 9,
            sha256: "ab".repeat(32),
            available: true,
        });
        app.update(Msg::Stream(StreamEvent::Post(with_file)));
        feed(&mut app, [key(KeyCode::Tab), key(KeyCode::Tab)]);
        assert_eq!(app.selected_post.as_deref(), Some(ulid(3).as_str()));
        assert_eq!(app.update(ch('s')).len(), 1);
        assert_eq!(toast_ink(&app, "saving report.txt…"), rgb(palette::MIST));
    }

    #[test]
    fn the_delete_confirmation_names_the_post() {
        let mut app = live_app();
        feed(&mut app, [key(KeyCode::Tab), key(KeyCode::Tab)]);
        app.update(ch('d'));
        let Some(Dialog::Confirm(confirm)) = &app.dialog else {
            panic!("{:?}", app.dialog)
        };
        assert_eq!(confirm.title, "Delete this post?");
        assert_eq!(
            confirm.message,
            "mac-juicebox's post from 5:02 pm disappears on every device."
        );
        assert!(screen_text(&app).contains("mac-juicebox's post from 5:02 pm"));

        // Dang's own, from another day.
        let mut app = live_app();
        let mut mine = api_post(3, "general", "Dang", "an old note");
        mine.ts_ms -= 24 * 60 * 60 * 1000;
        app.update(Msg::Stream(StreamEvent::Post(mine)));
        app.focus = Focus::Posts;
        app.selected_post = Some(ulid(1));
        app.update(ch('k'));
        assert_eq!(app.selected_post.as_deref(), Some(ulid(3).as_str()));
        app.update(ch('d'));
        let Some(Dialog::Confirm(confirm)) = &app.dialog else {
            panic!("{:?}", app.dialog)
        };
        assert_eq!(
            confirm.message,
            "Your post from yesterday, 5:03 pm disappears on every device."
        );
    }

    #[test]
    fn the_focused_sidebar_says_which_keys_move_in_it() {
        let mut app = live_app();
        let channels_row = |app: &App| {
            let buf = screen(app);
            let y = row_of(&buf, "CHANNELS");
            region_text(&buf, y, 0, 26)
        };
        assert!(
            !channels_row(&app).contains("j k"),
            "{}",
            channels_row(&app)
        );
        app.update(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Sidebar);
        let row = channels_row(&app);
        assert!(row.contains("CHANNELS") && row.contains("j k"), "{row}");
        let buf = screen(&app);
        let y = row_of(&buf, "CHANNELS");
        let x = column_of(&buf, y, "j k").unwrap();
        assert_eq!(
            buf[(x, y)].fg,
            rgb(palette::FOG),
            "like the n of + new channel"
        );
    }

    #[test]
    fn attaching_from_announcements_says_how_to_pick_a_channel() {
        let mut app = live_app();
        app.focus = Focus::Sidebar;
        app.update(ch('k'));
        assert_eq!(app.selected, Selection::Announcements);
        assert!(app.update(ch('a')).is_empty());
        assert_eq!(
            app.toast.as_deref(),
            Some("Pick a channel to attach a file to: tab to the channels, then j/k")
        );
    }

    #[test]
    fn a_page_that_failed_to_load_is_not_an_empty_channel() {
        let mut app = live_app();
        app.focus = Focus::Sidebar;
        app.update(ch('j'));
        app.update(Msg::Loaded(Loaded::Posts {
            view: beacon_view(),
            before: None,
            page: Err("lost the connection to http://127.0.0.1:7700 (timed out)".to_string()),
        }));
        let text = pane_text(&screen(&app));
        assert!(!text.contains("Nothing in #beacon yet"), "{text}");
        assert!(
            text.contains(
                "Couldn't load #beacon. Switch to another channel and back to try again."
            ),
            "{text}"
        );
        assert!(app.toast_error);
        // Back again: it loads.
        app.update(ch('k'));
        app.update(ch('j'));
        app.update(Msg::Loaded(page(beacon_posts(1..3))));
        let text = pane_text(&screen(&app));
        assert!(!text.contains("Couldn't load"), "{text}");
        assert!(text.contains("post 2"), "{text}");
    }
}
