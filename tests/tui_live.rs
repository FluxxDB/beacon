//! The TUI's event loop, headless: `tui::drive` on a `TestBackend` against a
//! real node, with keys sent in and the screen read back after every frame.
//! Every test runs under a deadline, and each wait for the screen has its own.

mod common;

use std::path::Path;
use std::time::Duration;

use beacon::client::Client;
use beacon::error::Error;
use beacon::model::{Attachment, AuthorKind, Event, EventKind, NewPost};
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
use sha2::{Digest, Sha256};
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
        Tui::start_with_secret(url, SECRET, exchange)
    }

    fn start_with_secret(url: &str, secret: &str, exchange: &Path) -> Tui {
        let client = Client::new(url, Some(secret)).unwrap();
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

        // Attach: the composer's text goes with the file. Shift-Tab leaves
        // for the posts with it kept (Esc would arm a clear).
        tui.type_text("my notes");
        tui.press(KeyCode::BackTab);
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
            s.contains("Delete this post?") && s.contains("Your post from")
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

/// `s` checks what it fetched against the post, size and sha256, like `get`
/// does: a file that does not match never shows up in the exchange folder
/// (not even for a moment, under its name), and the toast says why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn save_refuses_a_file_that_does_not_match_its_post() {
    tokio::time::timeout(DEADLINE, async {
        let node = spawn_node("pc", &[]).await;
        // The node holds a file; a peer's post claims it is 1 byte long.
        let bytes = b"the bytes the node holds\n";
        let sha: String = Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        std::fs::write(node.dir.path().join("files").join(&sha), bytes).unwrap();
        let id = ulid::Ulid::generate().to_string();
        let claim = Event {
            origin: "mac~abcd1234".to_string(),
            seq: 1,
            id: id.clone(),
            ts: beacon::server::now_ms(),
            kind: EventKind::PostCreated(NewPost {
                post_id: id,
                channel: "general".to_string(),
                author: "mac-agent".to_string(),
                author_kind: AuthorKind::Agent,
                client: Some("codex".to_string()),
                node: "mac".to_string(),
                body: "here is the log".to_string(),
                tags: Vec::new(),
                mentions: Vec::new(),
                attachment: Some(Attachment {
                    sha256: sha,
                    name: "claimed.txt".to_string(),
                    size: 1,
                }),
            }),
        };
        node.state().ingest(claim).await.unwrap().unwrap();

        let exchange = tempfile::tempdir().unwrap();
        let mut tui = Tui::start(&node.url, exchange.path());
        tui.until(Duration::from_secs(5), "the post", |s| {
            s.contains("here is the log") && s.contains("claimed.txt")
        })
        .await;
        // From the composer: the sidebar, then the posts.
        tui.press(KeyCode::Tab);
        tui.press(KeyCode::Tab);
        tui.press(KeyCode::Char('s'));
        tui.until(Duration::from_secs(5), "the refusal", |s| {
            s.contains("Not saved: attachment doesn't match what its post describes")
        })
        .await;
        let left: Vec<_> = std::fs::read_dir(exchange.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(
            left.is_empty(),
            "nothing saved, nothing left over: {left:?}"
        );
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

/// The handles the node lists in `who`.
async fn who(node: &TestNode) -> Vec<String> {
    get_json(node, "/api/who").await["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["handle"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forget_a_stale_agent_from_the_sidebar() {
    tokio::time::timeout(DEADLINE, async {
        let node = spawn_node("pc", &[]).await;
        let resp = node
            .post("/api/who")
            .json(&json!({ "handle": "pc-test", "kind": "agent", "client": "claude-code" }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
        let exchange = tempfile::tempdir().unwrap();
        let mut tui = Tui::start(&node.url, exchange.path());
        tui.until(Duration::from_secs(5), "the agent in the sidebar", |s| {
            s.contains("# general") && s.contains("● pc-test")
        })
        .await;

        // Sidebar, past #general onto the agent, `d`, then ↵ in the dialog.
        tui.press(KeyCode::Tab);
        tui.press(KeyCode::Char('j'));
        tui.until(Duration::from_secs(5), "the cursor on the agent", |s| {
            s.contains("▍ ● pc-test")
        })
        .await;
        tui.press(KeyCode::Char('d'));
        tui.until(Duration::from_secs(5), "the forget dialog", |s| {
            s.contains("Forget pc-test?") && s.contains("↵ forget")
        })
        .await;
        tui.press(KeyCode::Enter);
        tui.until(Duration::from_secs(5), "the row gone", |s| {
            !s.contains("● pc-test") && s.contains("forgot pc-test")
        })
        .await;
        assert!(who(&node).await.iter().all(|h| h != "pc-test"));

        // It checks in again: back in the sidebar, live.
        let resp = node
            .post("/api/who")
            .json(&json!({ "handle": "pc-test", "kind": "agent", "client": "claude-code" }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
        tui.until(Duration::from_secs(5), "the agent back", |s| {
            s.contains("● pc-test")
        })
        .await;
        tui.quit().await;
    })
    .await
    .expect("test timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_secret_shows_the_rejected_band() {
    tokio::time::timeout(DEADLINE, async {
        let node = spawn_node("pc", &[]).await;
        let exchange = tempfile::tempdir().unwrap();
        let mut tui = Tui::start_with_secret(&node.url, "not-the-secret", exchange.path());
        let screen = tui
            .until(Duration::from_secs(3), "the rejected band", |s| {
                s.contains("The server rejected BEACON_SECRET.")
            })
            .await;
        assert!(screen.contains("Check .env on this device."), "{screen}");
        assert!(!screen.contains("docker compose"), "{screen}");
        tui.quit().await;
    })
    .await
    .expect("test timed out");
}

// ------------------------------------------------- the real binary on a pty

mod pty {
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// A pty pair, 104×33.
    fn open_pty() -> (OwnedFd, OwnedFd) {
        let (mut master, mut slave) = (0, 0);
        let size = libc::winsize {
            ws_row: 33,
            ws_col: 104,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty writes two valid fds on success; the null pointers
        // mean "default name and termios".
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size,
            )
        };
        assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
        // SAFETY: both fds are fresh and owned by nobody else.
        unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
    }

    /// The terminal's local modes: canonical input and echo, as a shell has them.
    fn cooked(fd: &OwnedFd) -> (bool, bool) {
        // SAFETY: termios is plain data and tcgetattr fills it in.
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::tcgetattr(fd.as_raw_fd(), &mut t) };
        assert_eq!(rc, 0, "tcgetattr: {}", std::io::Error::last_os_error());
        (t.c_lflag & libc::ICANON != 0, t.c_lflag & libc::ECHO != 0)
    }

    /// Everything the program wrote, as it comes.
    struct Output {
        rx: mpsc::Receiver<Vec<u8>>,
        seen: Vec<u8>,
    }

    impl Output {
        /// Waits until the output (from `from` on) holds `needle`.
        fn wait_for(&mut self, from: usize, needle: &[u8], within: Duration) {
            let deadline = Instant::now() + within;
            while !contains(&self.seen[from.min(self.seen.len())..], needle) {
                let left = deadline.saturating_duration_since(Instant::now());
                match self.rx.recv_timeout(left) {
                    Ok(chunk) => self.seen.extend(chunk),
                    Err(_) => panic!(
                        "{:?} not written within {within:?}; got {:?}",
                        String::from_utf8_lossy(needle),
                        String::from_utf8_lossy(&self.seen)
                    ),
                }
            }
        }

        fn drain(&mut self, for_: Duration) {
            let deadline = Instant::now() + for_;
            while let Ok(chunk) = self
                .rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                self.seen.extend(chunk);
            }
        }
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    /// The child, killed if a failing test leaves it running.
    struct Reaped(std::process::Child);

    impl Drop for Reaped {
        fn drop(&mut self) {
            if matches!(self.0.try_wait(), Ok(None)) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    /// `beacon tui` on its own pty (its controlling terminal), with the
    /// server unreachable; `signal` sent once it is running. The terminal
    /// must come back: off the alternate screen, cursor shown, bracketed
    /// paste off, canonical input and echo on.
    fn signal_restores_the_terminal(signal: libc::c_int) {
        let (master, slave) = open_pty();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_beacon"));
        cmd.arg("tui")
            .env("BEACON_SERVER", "http://127.0.0.1:1")
            .env("BEACON_SECRET", "x")
            .env("COLORTERM", "truecolor")
            .env_remove("BEACON_HUMAN")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                // A session of its own, with the pty as its terminal, so
                // nothing reaches the terminal running the tests.
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = Reaped(cmd.spawn().unwrap());
        drop(cmd);

        let mut writer = File::from(master.try_clone().unwrap());
        let mut reader = File::from(master);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        let mut out = Output {
            rx,
            seen: Vec::new(),
        };

        // In the alternate screen, it asks the terminal two things, as a
        // real one would be asked: its keyboard (answering only the device
        // query means "nothing special") and where the cursor is.
        out.wait_for(0, b"\x1b[?1049h", Duration::from_secs(10));
        out.wait_for(0, b"\x1b[c", Duration::from_secs(5));
        writer.write_all(b"\x1b[?62c").unwrap();
        out.wait_for(0, b"\x1b[6n", Duration::from_secs(5));
        writer.write_all(b"\x1b[1;1R").unwrap();
        out.wait_for(0, b"Can't reach the beacon server", Duration::from_secs(10));
        assert_eq!(cooked(&slave), (false, false), "raw mode while running");

        let mark = out.seen.len();
        // SAFETY: plain kill(2) on our own child.
        assert_eq!(
            unsafe { libc::kill(child.0.id() as libc::pid_t, signal) },
            0
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "still running after signal {signal}"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(
            status.code(),
            Some(0),
            "signal {signal}: it exits, not dies"
        );
        out.drain(Duration::from_millis(200));
        let after = &out.seen[mark..];
        for (what, seq) in [
            ("leaves the alternate screen", &b"\x1b[?1049l"[..]),
            ("shows the cursor", b"\x1b[?25h"),
            ("turns bracketed paste off", b"\x1b[?2004l"),
        ] {
            assert!(
                contains(after, seq),
                "signal {signal}: {what}; wrote {:?}",
                String::from_utf8_lossy(after)
            );
        }
        assert_eq!(
            cooked(&slave),
            (true, true),
            "signal {signal}: cooked again"
        );
    }

    #[test]
    fn sigterm_restores_the_terminal() {
        signal_restores_the_terminal(libc::SIGTERM);
    }

    #[test]
    fn sighup_restores_the_terminal() {
        signal_restores_the_terminal(libc::SIGHUP);
    }
}
