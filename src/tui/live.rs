//! The event loop: the terminal, keys, the live feed, ticks and commands.
//!
//! [`run`] owns the real terminal: raw mode, the alternate screen, no
//! hardware cursor (the render paints its own caret), bracketed paste, and
//! unambiguous key codes where the terminal offers them (so `Shift-Enter`
//! differs from `Enter`). It puts all of that back on exit and on a panic.
//!
//! [`drive`] is the loop itself, over any ratatui backend and any stream of
//! input, so tests run it headless against a real node. It redraws after
//! every batch of messages and never waits on the network: each [`Cmd`] runs
//! on its own tokio task and comes back as a [`Msg`]. Commands that change
//! or read the channel list run one after another on one task, so an older
//! list can never land after a newer change.
//!
//! The live feed (`GET /api/stream`, every post rather than only Dang's
//! relevant ones) reconnects by itself: 1 s after it breaks, then 2, 4, 8…
//! up to 30 s between tries, with [`Conn::Down`] on screen meanwhile. Once
//! it is back the board is read afresh ([`App::update`] asks for that).

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;
use std::time::Duration;

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute};
use futures_util::{Stream, StreamExt};
use jiff::tz::TimeZone;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::Buffer;
use ratatui::Terminal;
use reqwest::Method;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use tokio::io::AsyncWriteExt;

use crate::api::{NewPostRequest, PostCreated, PostsResponse, WhoRequest};
use crate::blobs::hex;
use crate::client::{Client, Query};
use crate::error::Error;
use crate::model::{validate_handle, AuthorKind};
use crate::server::now_ms;
use crate::tui::state::{
    coalesce, App, Cmd, Conn, ExchangeFile, Loaded, Msg, Selection, SendFailed,
    ANNOUNCEMENTS_LIMIT, PAGE,
};
use crate::tui::theme::{ColorMode, Theme};
use crate::tui::view;

/// Who is at the keyboard when `BEACON_HUMAN` does not say.
const DEFAULT_HUMAN: &str = "Dang";
/// Where `./exchange` is mounted in the container.
const CONTAINER_EXCHANGE: &str = "/exchange";
/// The first wait before reconnecting the live feed, and the longest.
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);
/// How often the clock ticks (ages, toasts, heartbeats).
const TICK: Duration = Duration::from_secs(1);
/// Messages applied before the next redraw at most, so a flood of them can
/// never keep the screen from updating.
const BATCH: usize = 256;
/// What the TUI posts as.
const CLIENT_NAME: &str = "tui";

/// What the TUI needs besides the client.
#[derive(Debug, Clone)]
pub struct TuiConfig {
    /// Who is at the keyboard (`BEACON_HUMAN`, default `Dang`): posts go out
    /// as this human, and reads are marked for this handle.
    pub human: String,
    /// The exchange folder: `a` lists it, `s` saves into it.
    pub exchange: PathBuf,
    pub tz: TimeZone,
    pub theme: Theme,
}

impl TuiConfig {
    /// From the environment: `BEACON_HUMAN`, `BEACON_EXCHANGE` (else
    /// `/exchange` in the container, `./exchange` outside it), the system
    /// time zone and `COLORTERM`.
    pub fn from_env() -> Result<TuiConfig, Error> {
        let human = match std::env::var("BEACON_HUMAN")
            .ok()
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty())
        {
            Some(h) => validate_handle(&h)
                .map_err(|e| Error::Usage(format!("BEACON_HUMAN is invalid: {e}")))?,
            None => DEFAULT_HUMAN.to_string(),
        };
        let exchange = match std::env::var_os("BEACON_EXCHANGE").filter(|v| !v.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None if Path::new(CONTAINER_EXCHANGE).is_dir() => PathBuf::from(CONTAINER_EXCHANGE),
            None => PathBuf::from("exchange"),
        };
        Ok(TuiConfig {
            human,
            exchange,
            tz: TimeZone::system(),
            theme: Theme::new(ColorMode::detect()),
        })
    }
}

// ----------------------------------------------------------- the terminal

/// The terminal is in the TUI's modes.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Unambiguous key codes were switched on (and must be switched off).
static ENHANCED: AtomicBool = AtomicBool::new(false);
/// Something panicked: the terminal is restored, and the loop must stop.
static PANICKED: AtomicBool = AtomicBool::new(false);

/// Runs the TUI on the real terminal until Dang quits.
pub async fn run(client: Client, cfg: TuiConfig) -> Result<(), Error> {
    install_panic_hook();
    // Listening before the terminal changes: a signal that comes at any
    // point after that ends the TUI the way Ctrl-C does.
    let stop = stop_signal()?;
    let result = run_on_terminal(client, cfg, stop).await;
    restore_terminal();
    result
}

/// Resolves when the process is asked to stop: SIGTERM (`kill`, `docker
/// stop`), SIGHUP (the terminal went away) or SIGINT (`kill -INT`; Ctrl-C
/// itself arrives as a key in raw mode). Handling them replaces the default
/// of dying on the spot, which would leave the terminal raw.
///
/// Inside Docker none of them comes when the `docker exec -it` client dies
/// (measured with the client killed four ways: Docker neither signals the
/// program nor closes its pty), so that TUI runs on until `q` or a container
/// restart; the README's troubleshooting table says so.
#[cfg(unix)]
fn stop_signal() -> Result<impl std::future::Future<Output = ()>, Error> {
    use tokio::signal::unix::{signal, SignalKind};
    let listen = |kind| {
        signal(kind).map_err(|e| Error::Internal(anyhow::anyhow!("cannot listen for signals: {e}")))
    };
    let mut term = listen(SignalKind::terminate())?;
    let mut hangup = listen(SignalKind::hangup())?;
    let mut interrupt = listen(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = hangup.recv() => {}
            _ = interrupt.recv() => {}
        }
    })
}

#[cfg(not(unix))]
fn stop_signal() -> Result<impl std::future::Future<Output = ()>, Error> {
    Ok(std::future::pending())
}

async fn run_on_terminal(
    client: Client,
    cfg: TuiConfig,
    stop: impl std::future::Future<Output = ()>,
) -> Result<(), Error> {
    // Asking the terminal what it supports can wait on its answer.
    tokio::task::spawn_blocking(enter_terminal)
        .await
        .map_err(terminal_error)?
        .map_err(terminal_error)?;
    let mut terminal =
        Terminal::new(CrosstermBackend::new(io::stdout())).map_err(terminal_error)?;
    terminal.hide_cursor().map_err(terminal_error)?;
    terminal.clear().map_err(terminal_error)?;
    // A terminal that can no longer be read ends the TUI, and so does a
    // stop signal: the input ends, and the loop with it.
    let input = EventStream::new()
        .take_while(|event| std::future::ready(event.is_ok()))
        .filter_map(|event| std::future::ready(event.ok().and_then(input_msg)))
        .take_until(stop);
    drive(&mut terminal, client, cfg, Box::pin(input), |_| {}).await
}

/// The message for a terminal event, if the TUI cares about it.
fn input_msg(event: Event) -> Option<Msg> {
    match event {
        Event::Key(key) => Some(Msg::Key(key)),
        Event::Paste(text) => Some(Msg::Paste(text)),
        Event::Resize(width, height) => Some(Msg::Resize(width, height)),
        _ => None,
    }
}

fn enter_terminal() -> io::Result<()> {
    terminal::enable_raw_mode()?;
    ACTIVE.store(true, Ordering::SeqCst);
    let mut out = io::stdout();
    execute!(
        out,
        EnterAlternateScreen,
        EnableBracketedPaste,
        cursor::Hide
    )?;
    // Only where the terminal says it understands them (kitty protocol).
    if matches!(terminal::supports_keyboard_enhancement(), Ok(true)) {
        execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
        ENHANCED.store(true, Ordering::SeqCst);
    }
    Ok(())
}

/// Puts the terminal back as it was found. Safe to call more than once (a
/// panic restores it, and so does the way out after it).
pub fn restore_terminal() {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let mut out = io::stdout();
    if ENHANCED.swap(false, Ordering::SeqCst) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        out,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        cursor::Show
    );
    let _ = terminal::disable_raw_mode();
    let _ = out.flush();
}

/// On any panic, on any thread: the terminal comes back first, so the
/// message is readable, and the loop stops at its next turn.
fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            PANICKED.store(true, Ordering::SeqCst);
            restore_terminal();
            previous(info);
        }));
    });
}

fn terminal_error(e: impl std::fmt::Display) -> Error {
    Error::Internal(anyhow::anyhow!("terminal: {e}"))
}

// ---------------------------------------------------------------- the loop

/// Aborts the background tasks when the loop ends, however it ends.
struct Tasks(Vec<JoinHandle<()>>);

impl Drop for Tasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

/// The TUI's loop on `terminal`, with keys (and pastes and resizes) from
/// `input`, until `Ctrl-C`/`q` or the end of `input`. `on_frame` sees each
/// frame as drawn (tests read the screen through it).
pub async fn drive<B, I, F>(
    terminal: &mut Terminal<B>,
    client: Client,
    cfg: TuiConfig,
    mut input: I,
    mut on_frame: F,
) -> Result<(), Error>
where
    B: Backend,
    B::Error: std::fmt::Display,
    I: Stream<Item = Msg> + Unpin,
    F: FnMut(&Buffer),
{
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let (board_tx, board_rx) = mpsc::unbounded_channel::<Cmd>();
    let worker = Worker {
        client: client.clone(),
        me: cfg.human.clone(),
        exchange: cfg.exchange.clone(),
        tx: tx.clone(),
    };
    let _tasks = Tasks(vec![
        tokio::spawn(live_feed(client, cfg.human.clone(), tx.clone())),
        tokio::spawn(ticks(tx.clone())),
        tokio::spawn(worker.clone().board(board_rx)),
    ]);

    let mut app = App::new("", cfg.human.clone(), cfg.tz.clone());
    // Nothing is known until the feed opens: show nothing rather than "no
    // channels".
    app.loading = true;
    let size = terminal.size().map_err(terminal_error)?;
    app.update(Msg::Resize(size.width, size.height));
    app.update(Msg::Tick(now_ms()));

    loop {
        if PANICKED.load(Ordering::SeqCst) {
            return Err(Error::Internal(anyhow::anyhow!(
                "the TUI stopped after an internal error (see above)"
            )));
        }
        let frame = terminal
            .draw(|f| view::render(f, &app, &cfg.theme))
            .map_err(terminal_error)?;
        on_frame(frame.buffer);

        let msg = tokio::select! {
            // Keys first, so the board stays responsive under a flood.
            biased;
            msg = input.next() => match msg {
                Some(msg) => msg,
                None => return Ok(()),
            },
            Some(msg) = rx.recv() => msg,
        };
        let mut cmds = app.update(msg);
        for _ in 0..BATCH {
            match rx.try_recv() {
                Ok(msg) => cmds.extend(app.update(msg)),
                Err(_) => break,
            }
        }
        // A burst of live posts asks for one cursor update, not one each.
        for cmd in coalesce(cmds) {
            match cmd {
                Cmd::Quit => return Ok(()),
                Cmd::LoadChannels
                | Cmd::CreateChannel { .. }
                | Cmd::DeleteChannel(_)
                | Cmd::MarkRead { .. } => {
                    let _ = board_tx.send(cmd);
                }
                cmd => {
                    let worker = worker.clone();
                    tokio::spawn(async move { worker.run(cmd).await });
                }
            }
        }
    }
}

/// `Tick` once a second.
async fn ticks(tx: UnboundedSender<Msg>) {
    let mut every = tokio::time::interval(TICK);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        every.tick().await;
        if tx.send(Msg::Tick(now_ms())).is_err() {
            return;
        }
    }
}

// ------------------------------------------------------------ the live feed

/// Keeps the live feed open: [`Conn::Up`] when it opens, its events as they
/// come, [`Conn::Down`] when it breaks, then tries again after a growing
/// pause.
async fn live_feed(client: Client, me: String, tx: UnboundedSender<Msg>) {
    let mut pause = RETRY_FIRST;
    let mut down_since: Option<i64> = None;
    loop {
        // Every post, not only those relevant to Dang: the TUI shows them all.
        match client.open_stream(Some(&me), false).await {
            Ok(mut events) => {
                pause = RETRY_FIRST;
                down_since = None;
                if tx.send(Msg::Connection(Conn::Up)).is_err() {
                    return;
                }
                // A feed only ends by breaking (Error::Interrupted).
                while let Some(Ok(event)) = events.next().await {
                    if tx.send(Msg::Stream(event)).is_err() {
                        return;
                    }
                }
            }
            // The node is there but refuses the secret: the band says so.
            Err(Error::Auth) => {
                if tx.send(Msg::SecretRejected).is_err() {
                    return;
                }
            }
            // Unreachable (no connection) or Interrupted (it broke): down.
            Err(_) => {}
        }
        let since = *down_since.get_or_insert_with(now_ms);
        if tx
            .send(Msg::Connection(Conn::Down { since_ms: since }))
            .is_err()
        {
            return;
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(RETRY_MAX);
    }
}

// ---------------------------------------------------------------- commands

/// Runs commands against the node and the exchange folder.
#[derive(Clone)]
struct Worker {
    client: Client,
    me: String,
    exchange: PathBuf,
    tx: UnboundedSender<Msg>,
}

impl Worker {
    /// Runs channel-list commands in order, one at a time. A list asked for
    /// several times in a row is fetched once.
    async fn board(self, mut rx: UnboundedReceiver<Cmd>) {
        let mut queue: VecDeque<Cmd> = VecDeque::new();
        while let Some(cmd) = rx.recv().await {
            queue.push_back(cmd);
            while let Ok(cmd) = rx.try_recv() {
                queue.push_back(cmd);
            }
            while let Some(cmd) = queue.pop_front() {
                if cmd == Cmd::LoadChannels && queue.contains(&Cmd::LoadChannels) {
                    continue;
                }
                self.run(cmd).await;
            }
        }
    }

    /// Carries out `cmd` and sends back what came of it.
    async fn run(&self, cmd: Cmd) {
        if let Some(msg) = self.execute(cmd).await {
            let _ = self.tx.send(msg);
        }
    }

    async fn execute(&self, cmd: Cmd) -> Option<Msg> {
        let client = &self.client;
        let me = self.me.as_str();
        match cmd {
            Cmd::LoadChannels => self.channels().await,
            Cmd::LoadPosts { channel, before } => {
                let mut query = Query::default().set("limit", Some(PAGE));
                if let Some((ts, id)) = &before {
                    query = query.set("before", Some(format!("{ts}:{id}")));
                }
                let request = client.request_with(
                    Method::GET,
                    &["api", "channels", &channel, "posts"],
                    query,
                );
                let page = client.json::<PostsResponse>(request).await;
                Some(Msg::Loaded(Loaded::Posts {
                    view: Selection::Channel(channel),
                    before,
                    page: page.map_err(|e| e.to_string()),
                }))
            }
            Cmd::LoadAnnouncements => {
                let page = client
                    .cross_posts(None, true, None, Some(ANNOUNCEMENTS_LIMIT))
                    .await;
                Some(Msg::Loaded(Loaded::Posts {
                    view: Selection::Announcements,
                    before: None,
                    page: page.map_err(|e| e.to_string()),
                }))
            }
            Cmd::LoadAgents => client
                .who()
                .await
                .ok()
                .map(|who| Msg::Loaded(Loaded::Agents(who))),
            Cmd::LoadStatus => client
                .status()
                .await
                .ok()
                .map(|status| Msg::Loaded(Loaded::Status(status))),
            Cmd::ListFiles => {
                let dir = self.exchange.clone();
                let files = tokio::task::spawn_blocking(move || list_exchange(&dir))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()));
                Some(Msg::Loaded(Loaded::Files(files)))
            }
            Cmd::SendPost { channel, body } => {
                let sent = client.post(&channel, &self.post_request(body)).await;
                Some(Msg::Sent(sent.map_err(SendFailed::from)))
            }
            Cmd::Upload {
                channel,
                path,
                body,
            } => Some(Msg::Sent(
                self.upload(&channel, &path, body)
                    .await
                    .map_err(SendFailed::from),
            )),
            Cmd::Save {
                sha, size, name, ..
            } => Some(Msg::Saved(
                self.save(&sha, size, &name)
                    .await
                    .map_err(|e| e.to_string()),
            )),
            Cmd::CreateChannel { name, topic } => {
                let created = client.create_channel(&name, topic.as_deref()).await;
                Some(Msg::Created(created.map_err(|e| e.to_string())))
            }
            Cmd::DeleteChannel(name) => {
                let deleted = client.delete_channel(&name).await;
                Some(Msg::ChannelDeleted(
                    deleted.map(|ack| ack.deleted).map_err(|e| e.to_string()),
                ))
            }
            Cmd::DeletePost(id) => {
                let deleted = client.delete_post(&id, Some(me)).await;
                Some(Msg::PostDeleted(
                    deleted.map(|ack| ack.deleted).map_err(|e| e.to_string()),
                ))
            }
            Cmd::ForgetAgent(handle) => {
                let forgot = client.forget(&handle, Some(me)).await;
                Some(Msg::AgentForgotten(
                    forgot.map(|ack| ack.forgot).map_err(|e| e.to_string()),
                ))
            }
            Cmd::MarkRead { channel, arrival } => {
                // Then the counts as they now stand.
                let _ = client.set_cursor(me, &channel, arrival).await;
                self.channels().await
            }
            Cmd::Heartbeat => {
                let beat = WhoRequest {
                    handle: me.to_string(),
                    kind: AuthorKind::Human,
                    client: CLIENT_NAME.to_string(),
                    status: None,
                };
                match client.heartbeat(&beat).await {
                    Err(Error::Auth) => Some(Msg::SecretRejected),
                    _ => None,
                }
            }
            Cmd::LoadPost(id) => {
                let post = match client.post_by_id(&id).await {
                    Ok(post) => Ok(Some(Box::new(post))),
                    // The node's answer: no post by that id, or several.
                    Err(Error::NotFound(_) | Error::Ambiguous(_) | Error::Usage(_)) => Ok(None),
                    Err(e) => Err(e.to_string()),
                };
                Some(Msg::Loaded(Loaded::Post { id, post }))
            }
            Cmd::Quit => None,
        }
    }

    /// A post by Dang: `author_kind` human, client `tui`, never creating a
    /// channel by accident.
    fn post_request(&self, body: String) -> NewPostRequest {
        NewPostRequest {
            body,
            tags: Vec::new(),
            author: self.me.clone(),
            author_kind: AuthorKind::Human,
            client: Some(CLIENT_NAME.to_string()),
            create: false,
        }
    }

    /// Posts the file at `path` (streamed, not read into memory) with `body`.
    async fn upload(&self, channel: &str, path: &Path, body: String) -> Result<PostCreated, Error> {
        let unreadable = |e: io::Error| Error::Usage(format!("can't read {}: {e}", path.display()));
        let file = tokio::fs::File::open(path).await.map_err(unreadable)?;
        let len = file.metadata().await.map_err(unreadable)?.len();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "attachment".to_string());
        self.client
            .upload(channel, &self.post_request(body), &name, file, Some(len))
            .await
    }

    /// Fetches the attachment `sha`, which its post says is `size` bytes,
    /// and puts it in the exchange folder as `name` (or the next free
    /// `name (n)`), the way `get -o` does: the bytes go into a hidden file
    /// there first, which is checked against the post (size and sha256) and
    /// only then renamed into place, so a file that did not arrive whole and
    /// right never shows up under its name. Returns the name used.
    async fn save(&self, sha: &str, size: u64, name: &str) -> Result<String, Error> {
        let mut download = self.client.file(sha).await.map_err(|e| match e {
            Error::NotFound(_) => Error::NotFound(
                "attachment not synced yet; try again when a peer that has it is online"
                    .to_string(),
            ),
            e => e,
        })?;
        let dir = self.exchange.clone();
        let (temp, file) = tokio::task::spawn_blocking(move || create_hidden(&dir))
            .await
            .map_err(|e| Error::Internal(anyhow::anyhow!("{e}")))?
            .map_err(|e| Error::Usage(format!("can't write to ./exchange: {e}")))?;
        let mut file = tokio::fs::File::from_std(file);
        let cannot = |e: io::Error| Error::Usage(format!("can't write to ./exchange: {e}"));
        let checked = async {
            let mut hasher = Sha256::new();
            let mut got = 0u64;
            while let Some(chunk) = download.chunk().await? {
                got += chunk.len() as u64;
                // Never more than the post said, whatever is sent.
                if got > size {
                    return Err(doesnt_match());
                }
                hasher.update(&chunk);
                file.write_all(&chunk).await.map_err(cannot)?;
            }
            file.sync_all().await.map_err(cannot)?;
            if got != size || hex(&hasher.finalize()) != sha {
                return Err(doesnt_match());
            }
            Ok(())
        }
        .await;
        drop(file);
        if let Err(e) = checked {
            let _ = tokio::fs::remove_file(&temp).await;
            return Err(e);
        }
        let dir = self.exchange.clone();
        let wanted = name.to_string();
        let checked_file = temp.clone();
        let placed = tokio::task::spawn_blocking(move || place(&dir, &wanted, &checked_file))
            .await
            .map_err(|e| Error::Internal(anyhow::anyhow!("{e}")))?;
        placed.map_err(|e| {
            let _ = std::fs::remove_file(&temp);
            cannot(e)
        })
    }

    async fn channels(&self) -> Option<Msg> {
        // The badges count every new post by someone else (R15), not only
        // what is relevant to Dang: he reads by opening a channel.
        match self.client.channels(Some(&self.me), false).await {
            Ok(list) => Some(Msg::Loaded(Loaded::Channels(list))),
            Err(Error::Auth) => Some(Msg::SecretRejected),
            // The feed shows the server as down; it reloads when it is back.
            Err(_) => None,
        }
    }
}

// ------------------------------------------------------------- the folder

/// The exchange folder's files, newest first. Hidden files and folders are
/// left out; a folder that is not there is an empty one.
pub fn list_exchange(dir: &Path) -> Result<Vec<ExchangeFile>, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        // Follows links: a linked file is attachable.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let modified_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        files.push(ExchangeFile {
            name,
            path,
            size: meta.len(),
            modified_ms,
        });
    }
    files.sort_by(|a, b| {
        b.modified_ms
            .cmp(&a.modified_ms)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(files)
}

/// What `save` says when the bytes are not the file the post describes (the
/// words `get` uses).
fn doesnt_match() -> Error {
    Error::Internal(anyhow::anyhow!(
        "attachment doesn't match what its post describes (size or hash)"
    ))
}

/// A new hidden file in `dir` (made if missing) for a download to go into
/// until it is checked; hidden, so the attach picker never lists it.
fn create_hidden(dir: &Path) -> io::Result<(PathBuf, std::fs::File)> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!(
        ".beacon-save.{}.{}",
        std::process::id(),
        ulid::Ulid::generate()
    ));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    Ok((path, file))
}

/// Moves the checked file `from` into `dir` as `name`, or the next free
/// `name (n)` ([`create_unique`] takes the name, then the rename fills it),
/// never over another file. Returns the name used.
fn place(dir: &Path, name: &str, from: &Path) -> io::Result<String> {
    let (saved, path, file) = create_unique(dir, name)?;
    drop(file);
    if let Err(e) = std::fs::rename(from, &path) {
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    Ok(saved)
}

/// Creates a new file in `dir` named `name`, or `name (1)`, `name (2)`… if
/// that is taken: never over an existing file. Returns the name used, its
/// path and the open file. The name comes from another device, so only its
/// last path part is used.
pub fn create_unique(dir: &Path, name: &str) -> io::Result<(String, PathBuf, std::fs::File)> {
    std::fs::create_dir_all(dir)?;
    let name = safe_file_name(name);
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), format!(".{ext}")),
        _ => (name.clone(), String::new()),
    };
    for n in 0..10_000u32 {
        let candidate = if n == 0 {
            name.clone()
        } else {
            format!("{stem} ({n}){ext}")
        };
        let path = dir.join(&candidate);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((candidate, path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("too many files named like {name}"),
    ))
}

/// `name` as a plain file name: its last path part, no control characters,
/// never `.` or `..` or empty.
fn safe_file_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or("");
    let clean: String = last.chars().filter(|c| !c.is_control()).collect();
    match clean.trim() {
        "" | "." | ".." => "attachment".to_string(),
        _ => clean,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_files_never_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let save = |name: &str, bytes: &[u8]| {
            let (saved, path, mut file) = create_unique(dir.path(), name).unwrap();
            file.write_all(bytes).unwrap();
            assert_eq!(path, dir.path().join(&saved));
            saved
        };
        assert_eq!(save("report.txt", b"a"), "report.txt");
        assert_eq!(save("report.txt", b"b"), "report (1).txt");
        assert_eq!(save("report.txt", b"c"), "report (2).txt");
        assert_eq!(save("Makefile", b"d"), "Makefile");
        assert_eq!(save("Makefile", b"e"), "Makefile (1)");
        assert_eq!(
            std::fs::read(dir.path().join("report.txt")).unwrap(),
            b"a",
            "the first is untouched"
        );
        // A folder that is not there yet is made.
        let nested = dir.path().join("exchange");
        assert_eq!(create_unique(&nested, "x").unwrap().0, "x");
    }

    #[test]
    fn saved_names_stay_in_the_folder() {
        assert_eq!(safe_file_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_file_name("..\\..\\boot.ini"), "boot.ini");
        assert_eq!(safe_file_name(".."), "attachment");
        assert_eq!(safe_file_name("dir/"), "attachment");
        assert_eq!(safe_file_name("re\x1b[2Jport.txt"), "re[2Jport.txt");
    }

    #[test]
    fn exchange_lists_files_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.txt");
        std::fs::write(&old, b"1").unwrap();
        let past = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(past)
            .unwrap();
        std::fs::write(dir.path().join("new.log"), b"22").unwrap();
        std::fs::write(dir.path().join(".hidden"), b"x").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let files = list_exchange(dir.path()).unwrap();
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["new.log", "old.txt"]);
        assert_eq!(files[0].size, 2);
        assert!(list_exchange(&dir.path().join("missing"))
            .unwrap()
            .is_empty());
    }
}
