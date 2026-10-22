//! The TUI's event loop, headless: `tui::drive` on a `TestBackend` against a
//! real node, with keys sent in and the screen read back after every frame.
//! Every test runs under a deadline, and each wait for the screen has its own.

mod common;

use std::path::Path;
use std::time::Duration;

use beacon::client::Client;
use beacon::error::Error;
use beacon::tui::state::Msg;
use beacon::tui::theme::{ColorMode, Theme};
use beacon::tui::{drive, TuiConfig};
use common::{spawn_node, TestNode, SECRET};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use futures_util::stream;
use jiff::tz::TimeZone;
use ratatui::backend::TestBackend;
use ratatui::buffer::{Buffer, CellWidth};
use ratatui::Terminal;
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// Longest any one test may take.
const DEADLINE: Duration = Duration::from_secs(60);

/// A TUI running on its own task.
struct Tui {
    task: JoinHandle<Result<(), Error>>,
    keys: mpsc::UnboundedSender<Msg>,
    screen: watch::Receiver<String>,
}

impl Tui {
    /// Starts the TUI against `url` as Dang, at 104×33, with `exchange` as
    /// its exchange folder.
    fn start(url: &str, exchange: &Path) -> Tui {
        let client = Client::new(url, Some(SECRET)).unwrap();
        let cfg = TuiConfig {
            human: "Dang".to_string(),
            exchange: exchange.to_path_buf(),
            tz: TimeZone::UTC,
            theme: Theme::new(ColorMode::TrueColor),
        };
        let (keys, keys_rx) = mpsc::unbounded_channel::<Msg>();
        let (screen_tx, screen) = watch::channel(String::new());
        let task = tokio::spawn(async move {
            let mut terminal = Terminal::new(TestBackend::new(104, 33)).unwrap();
            let input = Box::pin(stream::unfold(keys_rx, |mut rx| async move {
                rx.recv().await.map(|msg| (msg, rx))
            }));
            drive(&mut terminal, client, cfg, input, |buf| {
                let _ = screen_tx.send(screen_text(buf));
            })
            .await
        });
        Tui { task, keys, screen }
    }

    fn press(&self, code: KeyCode) {
        self.keys
            .send(Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)))
            .unwrap();
    }

    fn type_text(&self, text: &str) {
        for c in text.chars() {
            self.press(KeyCode::Char(c));
        }
    }

    /// Waits up to `within` for a frame that satisfies `ok`; returns it.
    async fn until(&mut self, within: Duration, what: &str, ok: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + within;
        loop {
            {
                let screen = self.screen.borrow_and_update();
                if ok(&screen) {
                    return screen.clone();
                }
            }
            if tokio::time::timeout_at(deadline, self.screen.changed())
                .await
                .is_err()
            {
                panic!(
                    "{what}: not on screen within {within:?}. Last frame:\n{}",
                    self.screen.borrow().as_str()
                );
            }
        }
    }

    /// Ctrl-C, and the loop ends cleanly.
    async fn quit(self) {
        self.keys
            .send(Msg::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            )))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .expect("the TUI quits on Ctrl-C")
            .unwrap()
            .unwrap();
    }
}

/// The screen as a terminal shows it: one line per row.
fn screen_text(buf: &Buffer) -> String {
    let mut text = String::new();
    for y in 0..buf.area.height {
        let mut hidden = 0u16;
        for x in 0..buf.area.width {
            let cell = &buf[(x, y)];
            if hidden == 0 {
                text.push_str(cell.symbol());
            }
            hidden = hidden.max(cell.symbol().cell_width()).saturating_sub(1);
        }
        text.push('\n');
    }
    text
}

async fn post_as(node: &TestNode, channel: &str, author: &str, body: &str) -> Value {
    let response = node
        .post(&format!("/api/channels/{channel}/posts"))
        .json(&json!({
            "body": body,
            "author": author,
            "author_kind": "agent",
            "client": "codex",
        }))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    response.json().await.unwrap()
}

async fn get_json(node: &TestNode, path: &str) -> Value {
    node.get(path).send().await.unwrap().json().await.unwrap()
}

/// Polls `path` until `ok` holds for its answer, for up to `within`.
async fn until_api(
    node: &TestNode,
    path: &str,
    within: Duration,
    what: &str,
    ok: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + within;
    loop {
        let answer = get_json(node, path).await;
        if ok(&answer) {
            return answer;
        }
        assert!(Instant::now() < deadline, "{what}: last answer {answer:#}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn general_unread(channels: &Value) -> u64 {
    channels["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "general")
        .unwrap()["unread"]
        .as_u64()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_reconnects_after_server_restart() {
    tokio::time::timeout(DEADLINE, async {
        let node = spawn_node("pc", &[]).await;
        let exchange = tempfile::tempdir().unwrap();
        post_as(&node, "general", "pc-configs", "posted before the restart").await;
        let mut tui = Tui::start(&node.url, exchange.path());
        tui.until(Duration::from_secs(5), "the board", |s| {
            s.contains("# general") && s.contains("posted before the restart")
        })
        .await;

        let stopped = node.stop().await;
        tui.until(Duration::from_secs(3), "the server-down band", |s| {
            s.contains("Can't reach the beacon server")
        })
        .await;

        let node = stopped.restart().await;
        let screen = tui
            .until(Duration::from_secs(5), "the band gone", |s| {
                !s.contains("Can't reach the beacon server")
            })
            .await;
        assert!(screen.contains("posted before the restart"));
        post_as(&node, "general", "mac-juicebox", "posted after the restart").await;
        tui.until(Duration::from_secs(5), "the new post, live", |s| {
            s.contains("posted after the restart")
        })
        .await;
        tui.quit().await;
    })
    .await
    .expect("test timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_unreachable_at_start_shows_the_band_and_quits_cleanly() {
    tokio::time::timeout(DEADLINE, async {
        // A port nothing listens on: a privileged one, which no test node
        // can take in the meantime (a freed ephemeral port could be).
        let url = "http://127.0.0.1:1";
        let exchange = tempfile::tempdir().unwrap();
        let mut tui = Tui::start(url, exchange.path());
        let screen = tui
            .until(Duration::from_secs(3), "the server-down band", |s| {
                s.contains("Can't reach the beacon server")
            })
            .await;
        // Nothing claims the board is empty while nothing is known.
        assert!(!screen.contains("No channels yet"), "{screen}");
        // `q` from the composer types; Ctrl-C quits.
        tui.type_text("q");
        tui.until(Duration::from_secs(2), "q typed", |s| s.contains("› q"))
            .await;
        tui.quit().await;
    })
    .await
    .expect("test timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn posts_as_dang_and_marks_read() {
    tokio::time::timeout(DEADLINE, async {
        let node = spawn_node("pc", &[]).await;
        let exchange = tempfile::tempdir().unwrap();
        post_as(&node, "general", "pc-configs", "@Dang can you look?").await;
        let before = get_json(&node, "/api/channels?handle=Dang").await;
        assert_eq!(general_unread(&before), 1);

        let mut tui = Tui::start(&node.url, exchange.path());
        tui.until(Duration::from_secs(5), "the post", |s| {
            s.contains("can you look?")
        })
        .await;
        // Opening the channel marked it read for Dang.
        until_api(
            &node,
            "/api/channels?handle=Dang",
            Duration::from_secs(5),
            "general read",
            |c| general_unread(c) == 0,
        )
        .await;
        // And the TUI said Dang is here, as a human on the tui.
        let who = until_api(
            &node,
            "/api/who",
            Duration::from_secs(5),
            "Dang seen",
            |w| {
                w["agents"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|a| a["handle"] == "Dang")
            },
        )
        .await;
        let dang = who["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["handle"] == "Dang")
            .unwrap()
            .clone();
        assert_eq!(
            (dang["kind"].as_str(), dang["client"].as_str()),
            (Some("human"), Some("tui"))
        );

        tui.type_text("on it, \"quoted\" and $HOME");
        tui.press(KeyCode::Enter);
        tui.until(Duration::from_secs(5), "the sent post", |s| {
            s.contains("on it, \"quoted\" and $HOME") && s.contains("message #general")
        })
        .await;
        let posts = get_json(&node, "/api/channels/general/posts").await;
        let mine = posts["posts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["author"] == "Dang")
            .expect("Dang's post on the server")
            .clone();
        assert_eq!(mine["body"], "on it, \"quoted\" and $HOME");
        assert_eq!(mine["author_kind"], "human");
        assert_eq!(mine["client"], "tui");
        assert_eq!(mine["announcement"], true);
        tui.quit().await;
    })
    .await
    .expect("test timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_save_and_delete_through_the_keys() {
    tokio::time::timeout(DEADLINE, async {
        let node = spawn_node("pc", &[]).await;
        let exchange = tempfile::tempdir().unwrap();
        std::fs::write(exchange.path().join("notes.txt"), b"line one\nline two\n").unwrap();
        let mut tui = Tui::start(&node.url, exchange.path());
        tui.until(Duration::from_secs(5), "the board", |s| {
            s.contains("# general")
        })
        .await;

        // Attach: the composer's text goes with the file.
        tui.type_text("my notes");
        tui.press(KeyCode::Esc);
        tui.press(KeyCode::Char('a'));
        tui.until(Duration::from_secs(5), "the picker", |s| {
            s.contains("Attach a file") && s.contains("notes.txt")
        })
        .await;
        tui.press(KeyCode::Enter);
        tui.until(Duration::from_secs(5), "the post with its file", |s| {
            s.contains("my notes") && s.contains("≡ notes.txt") && !s.contains("Attach a file")
        })
        .await;
        let posts = get_json(&node, "/api/channels/general/posts").await;
        let post = posts["posts"][0].clone();
        assert_eq!(post["body"], "my notes");
        assert_eq!(post["author"], "Dang");
        assert_eq!(post["attachment"]["name"], "notes.txt");
        let id = post["id"].as_str().unwrap().to_string();

        // Save: the name is taken, so it gets the next free one.
        tui.press(KeyCode::Tab);
        tui.press(KeyCode::Tab);
        tui.press(KeyCode::Tab);
        tui.press(KeyCode::Char('s'));
        tui.until(Duration::from_secs(5), "the saved toast", |s| {
            s.contains("saved to ./exchange/notes (1).txt")
        })
        .await;
        assert_eq!(
            std::fs::read(exchange.path().join("notes (1).txt")).unwrap(),
            b"line one\nline two\n"
        );

        // Delete: nothing until confirmed.
        tui.press(KeyCode::Char('d'));
        tui.until(Duration::from_secs(2), "the confirmation", |s| {
            s.contains("Delete this post?") && s.contains("It disappears on every device.")
        })
        .await;
        tui.press(KeyCode::Esc);
        tui.until(Duration::from_secs(2), "the dialog gone", |s| {
            !s.contains("Delete this post?")
        })
        .await;
        assert_eq!(
            get_json(&node, "/api/channels/general/posts").await["posts"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        tui.press(KeyCode::Char('d'));
        tui.press(KeyCode::Enter);
        tui.until(Duration::from_secs(5), "the post gone", |s| {
            !s.contains("my notes") && s.contains("Nothing in #general yet")
        })
        .await;
        let gone = node
            .get(&format!("/api/posts/{id}"))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(gone, 404);
        tui.quit().await;
    })
    .await
    .expect("test timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_channel_from_the_keys() {
    tokio::time::timeout(DEADLINE, async {
        let node = spawn_node("pc", &[]).await;
        let exchange = tempfile::tempdir().unwrap();
        let mut tui = Tui::start(&node.url, exchange.path());
        tui.until(Duration::from_secs(5), "the board", |s| {
            s.contains("# general")
        })
        .await;
        tui.press(KeyCode::Tab);
        tui.press(KeyCode::Char('n'));
        tui.type_text("GPU-jobs");
        tui.press(KeyCode::Tab);
        tui.type_text("long-running jobs");
        tui.press(KeyCode::Enter);
        tui.until(Duration::from_secs(5), "the new channel, open", |s| {
            s.contains("# gpu-jobs   long-running jobs") && s.contains("message #gpu-jobs")
        })
        .await;
        let channels = get_json(&node, "/api/channels").await;
        assert!(channels["channels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "gpu-jobs" && c["topic"] == "long-running jobs"));

        // Posted from elsewhere into another channel: counted in the sidebar.
        post_as(&node, "general", "pc-configs", "over here").await;
        tui.until(Duration::from_secs(5), "general's unread count", |s| {
            s.lines()
                .any(|l| l.contains("# general") && l.contains(" 1  │"))
        })
        .await;
        tui.quit().await;
    })
    .await
    .expect("test timed out");
}
