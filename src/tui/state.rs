//! What the TUI shows, and how it changes.
//!
//! [`App`] is a plain view model that [`crate::tui::view::render`] reads and
//! never changes. [`App::update`] is the only thing that changes it: it takes
//! one [`Msg`] (a key, a tick, something from the server) and answers with
//! the [`Cmd`]s the event loop ([`crate::tui::live`]) should carry out, whose
//! results come back as more `Msg`s. `update` does no I/O, reads no clock and
//! never blocks, so all of the TUI's behaviour is tested without a terminal
//! or a server. Server types become view structs in [`crate::tui::convert`],
//! so rendering only depends on what it draws. How colors reach the terminal
//! is the [`crate::tui::theme::Theme`]'s business, not the model's.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crossterm::event::KeyEvent;
use jiff::tz::TimeZone;
use ratatui::layout::Rect;

use crate::api::{
    ChannelAck, ChannelsJson, PeerJson, PostCreated, PostJson, PostsResponse, StatusJson,
    StreamEvent, WhoJson, ACTIVE_WINDOW_MS,
};
use crate::error::Error;
use crate::model::AuthorKind;
use crate::tui::convert;
use crate::tui::mention::Mention;
use crate::tui::reply::Target;
use crate::tui::view::Layout;
use crate::tui::widgets::posts;

/// Posts asked for at a time in a channel (the newest, then older pages).
pub const PAGE: u32 = 100;
/// Announcements asked for (the newest; that view has no older pages).
pub const ANNOUNCEMENTS_LIMIT: u32 = 200;
/// Longest filter, in characters (a tag or `@handle` is far shorter).
pub const FILTER_MAX: usize = 64;
/// Shown under the new channel's name field, in coral when the name breaks it.
pub const NAME_RULE: &str = "lowercase letters, digits and dashes";
/// Longest channel name, as the server allows.
pub const CHANNEL_NAME_MAX: usize = 32;
/// How long an ordinary toast stays, and one that reports a failure.
/// The toast after a send whose connection dropped: it may have been stored.
pub const MAYBE_SENT: &str =
    "Not sent? The connection dropped — check the channel before resending";
const TOAST_MS: i64 = 5_000;
const ERROR_TOAST_MS: i64 = 10_000;
/// How often the TUI tells the node Dang is here, and asks how sync is going.
pub const HEARTBEAT_MS: i64 = 60_000;
pub const STATUS_MS: i64 = 5_000;
/// Agents not seen for this long leave the sidebar (as `who` drops them).
const AGENT_WINDOW_MS: i64 = 2 * 60 * 60 * 1000;

/// Everything one frame of the TUI is drawn from, plus the little the
/// keyboard and the live feed need to remember between frames.
#[derive(Debug, Clone)]
pub struct App {
    /// This device's node name, shown in the header.
    pub node: String,
    /// "Now" for ages and day labels, in epoch milliseconds.
    pub now_ms: i64,
    /// Zone for clock times and day separators.
    pub tz: TimeZone,
    /// Sidebar channels, in display order.
    pub channels: Vec<ChannelItem>,
    /// Unread announcements across every channel.
    pub announcements_unread: u32,
    /// The view in the main pane.
    pub selected: Selection,
    /// Posts of the selected view, oldest first.
    pub posts: Vec<PostItem>,
    /// How many lines the posts are scrolled up from the newest (0 = bottom).
    pub scroll: usize,
    /// Agents in the sidebar, in display order.
    pub agents: Vec<AgentItem>,
    /// The agent row the sidebar's cursor is on (`j` past the last channel),
    /// by handle; the view on screen stays. Only while the sidebar has the
    /// keys: `None` whenever it does not.
    pub selected_agent: Option<String>,
    /// `shift-e` or `space e` hid the sidebar: from 80 columns up the main
    /// pane takes its place. Below 80 there is no sidebar, and neither key
    /// touches this. For this session only.
    pub hide_sidebar: bool,
    /// Sync peers, for the header.
    pub peers: Vec<PeerItem>,
    /// When the last sync with the peers finished.
    pub last_sync_ms: Option<i64>,
    /// The human at the keyboard (`BEACON_HUMAN`).
    pub me: String,
    /// The composer's text.
    pub input: String,
    /// Where the composer's cursor is, as how many bytes of `input` come
    /// after it (0: at the end, where typing appends). Counted from the end
    /// so that text leaving the front (a post once sent) or a whole new
    /// `input` leaves it in place. Read and set it with [`App::cursor`] and
    /// [`App::set_cursor`].
    pub cursor_from_end: usize,
    /// The column `↑` `↓` aim for through rows of different lengths; any
    /// other key forgets it.
    pub cursor_goal: Option<u16>,
    /// The first input row the composer shows when there are more than fit:
    /// it scrolls only as far as it must to keep the cursor in view.
    pub composer_top: usize,
    /// One `Esc` in a composer with text arms a clear, and the next one
    /// empties it; any other key disarms it.
    pub clear_armed: bool,
    /// `space` in the posts or the sidebar started a chord (Dang, XNPVKNHP):
    /// the next key finishes it (`e` shows or hides the sidebar), `Esc`
    /// drops it, and any other key drops it and does what it always does.
    /// A paste, a resize or the focus moving drops it too.
    pub leader: bool,
    /// What the keys did to the `@handle` being typed (its ghost).
    pub mention: Option<Mention>,
    pub focus: Focus,
    pub dialog: Option<Dialog>,
    pub connection: Conn,
    /// Active post filter without its leading `/`: a tag (`claim`) or `@handle`.
    pub filter: Option<String>,
    /// Id of the post selected in the posts pane.
    pub selected_post: Option<String>,
    /// A short notice shown in the help row (`saved to ./exchange/x`).
    pub toast: Option<String>,
    /// The toast reports a failure (drawn in coral).
    pub toast_error: bool,

    // ------------------------------------------- remembered between frames
    /// When the toast goes away (epoch ms); `None` keeps it up. Pushed back
    /// while the leader's hint covers the toast.
    pub toast_until_ms: Option<i64>,
    /// The terminal's size, for paging and keeping the selected post in view.
    pub size: (u16, u16),
    /// The view's posts are being fetched: the pane stays blank rather than
    /// say the channel is empty.
    pub loading: bool,
    /// The view's newest posts could not be fetched: the pane says so
    /// rather than call the channel empty.
    pub load_failed: bool,
    /// The live feed from the local server is open.
    pub live: bool,
    /// A first view has been picked (the first channel list picks it).
    pub view_chosen: bool,
    /// `/` is being typed: keys go to the filter.
    pub filter_editing: bool,
    /// What the open confirmation does on `↵`.
    pub confirm: Option<Cmd>,
    /// The body of the post on its way. One at a time, so `↵` pressed
    /// twice never posts twice.
    pub sending: Option<String>,
    /// A full page came back, so there may be older posts above the oldest.
    pub has_older: bool,
    /// `(ts, id)` of the oldest post the pages brought: what the next older
    /// page is asked for, and the line above which live posts are left for
    /// paging to bring (a late-synced old post must not move it).
    pub oldest_page: Option<(i64, String)>,
    /// The node refused BEACON_SECRET: the down band says so.
    pub secret_rejected: bool,
    /// A page of older posts is on its way.
    pub loading_older: bool,
    /// What the view structs leave out of each loaded post, by id.
    pub extras: HashMap<String, PostExtra>,
    /// What arrived live while the view's newest page was on its way, so the
    /// page cannot drop it (or bring back what was deleted meanwhile).
    pub arrivals: Option<Arrivals>,
    /// Live posts older than the oldest page post that came while a page of
    /// older posts was on its way: the node may have read that page before
    /// they arrived, so they go in when it lands, where they fit.
    pub held: Vec<(PostItem, PostExtra)>,
    /// What the node said about the posts replies name, by the id a reply
    /// names (uppercase), kept across views; see [`crate::tui::reply`].
    pub replies: HashMap<String, Target>,
    pub last_heartbeat_ms: Option<i64>,
    pub last_status_ms: Option<i64>,
}

impl App {
    /// An empty board for `node`, typed at by `me`.
    pub fn new(node: impl Into<String>, me: impl Into<String>, tz: TimeZone) -> Self {
        App {
            node: node.into(),
            now_ms: 0,
            tz,
            channels: Vec::new(),
            announcements_unread: 0,
            selected: Selection::Announcements,
            posts: Vec::new(),
            scroll: 0,
            agents: Vec::new(),
            selected_agent: None,
            hide_sidebar: false,
            peers: Vec::new(),
            last_sync_ms: None,
            me: me.into(),
            input: String::new(),
            cursor_from_end: 0,
            cursor_goal: None,
            composer_top: 0,
            clear_armed: false,
            leader: false,
            mention: None,
            focus: Focus::Composer,
            dialog: None,
            connection: Conn::Up,
            filter: None,
            selected_post: None,
            toast: None,
            toast_error: false,
            toast_until_ms: None,
            size: (0, 0),
            loading: false,
            load_failed: false,
            live: false,
            view_chosen: false,
            filter_editing: false,
            confirm: None,
            sending: None,
            has_older: false,
            oldest_page: None,
            secret_rejected: false,
            loading_older: false,
            extras: HashMap::new(),
            arrivals: None,
            held: Vec::new(),
            replies: HashMap::new(),
            last_heartbeat_ms: None,
            last_status_ms: None,
        }
    }
}

/// What a loaded post carries that the view structs leave out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostExtra {
    /// Where the post arrived on this node: what a read cursor is set to.
    pub arrival: i64,
    /// The attachment's hash, to fetch its bytes by.
    pub sha256: Option<String>,
    /// Whether this node holds the attachment's bytes yet.
    pub available: bool,
}

/// Posts and deletions that came in live while a view's newest page was
/// being fetched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Arrivals {
    pub posts: Vec<(PostItem, PostExtra)>,
    pub deleted: HashSet<String>,
}

/// A file in the exchange folder, for the attach picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExchangeFile {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    /// Last modified, epoch ms.
    pub modified_ms: i64,
}

// ------------------------------------------------------------- messages

/// Something that happened: a key, a tick, an answer from the server.
#[derive(Debug, Clone)]
pub enum Msg {
    Key(KeyEvent),
    /// Text pasted into the terminal (bracketed paste), all at once.
    Paste(String),
    Resize(u16, u16),
    /// Once a second, with the time (epoch ms): ages, toasts, heartbeats.
    Tick(i64),
    /// An event from the live feed.
    Stream(StreamEvent),
    Loaded(Loaded),
    /// The live feed opened (`Up`) or broke (`Down`).
    Connection(Conn),
    /// A post (or an upload) went out, or did not.
    Sent(Result<PostCreated, SendFailed>),
    Created(Result<ChannelAck, String>),
    /// The post with this id was deleted, or was not.
    PostDeleted(Result<String, String>),
    /// The channel with this name was deleted, or was not.
    ChannelDeleted(Result<String, String>),
    /// The agent with this handle was forgotten, or was not.
    AgentForgotten(Result<String, String>),
    /// An attachment was saved to the exchange folder under this name.
    Saved(Result<String, String>),
    /// A problem worth a toast.
    Notice(String),
    /// The node answered 401: this device's BEACON_SECRET is not the node's.
    SecretRejected,
}

/// Why a post (or an upload) did not go out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendFailed {
    /// It was refused, or never reached the node: `Not sent: <why>`.
    NotSent(String),
    /// The connection dropped (or the node went quiet) mid-request, so the
    /// node may have stored it: sending again could post it twice.
    MaybeSent,
}

impl From<Error> for SendFailed {
    fn from(e: Error) -> Self {
        match e {
            Error::Interrupted(_) => SendFailed::MaybeSent,
            e => SendFailed::NotSent(e.to_string()),
        }
    }
}

/// Merges what one batch of messages asked for: one `MarkRead` per channel,
/// at the highest arrival, where the first one was; one `LoadChannels`.
/// Everything else is kept, in order. A burst of live posts then costs one
/// cursor update, not one per post.
pub fn coalesce(cmds: Vec<Cmd>) -> Vec<Cmd> {
    let mut out: Vec<Cmd> = Vec::with_capacity(cmds.len());
    for cmd in cmds {
        match cmd {
            Cmd::MarkRead { channel, arrival } => {
                let earlier = out.iter_mut().find_map(|c| match c {
                    Cmd::MarkRead {
                        channel: seen,
                        arrival: at,
                    } if *seen == channel => Some(at),
                    _ => None,
                });
                match earlier {
                    Some(at) => *at = (*at).max(arrival),
                    None => out.push(Cmd::MarkRead { channel, arrival }),
                }
            }
            Cmd::LoadChannels if out.contains(&Cmd::LoadChannels) => {}
            cmd => out.push(cmd),
        }
    }
    out
}

/// An answer to a `Load…` command.
#[derive(Debug, Clone)]
pub enum Loaded {
    Channels(ChannelsJson),
    /// A page of posts for `view`; `before` as it was asked for.
    Posts {
        view: Selection,
        before: Option<(i64, String)>,
        page: Result<PostsResponse, String>,
    },
    Agents(WhoJson),
    Status(StatusJson),
    /// The exchange folder, newest first.
    Files(Result<Vec<ExchangeFile>, String>),
    /// The post a reply names by `id`: `None` when the node has no such
    /// post, `Err` when it could not be asked.
    Post {
        id: String,
        post: Result<Option<Box<PostJson>>, String>,
    },
}

/// Something for the event loop to do. Each runs on its own task; its
/// result comes back as a [`Msg`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// The channels, with `me`'s unread counts.
    LoadChannels,
    /// A channel's newest page, or the page before `(ts_ms, id)`.
    LoadPosts {
        channel: String,
        before: Option<(i64, String)>,
    },
    /// Human posts from every channel.
    LoadAnnouncements,
    LoadAgents,
    LoadStatus,
    /// List the exchange folder for the attach picker.
    ListFiles,
    SendPost {
        channel: String,
        body: String,
    },
    /// Post the file at `path` with `body` as its message.
    Upload {
        channel: String,
        path: PathBuf,
        body: String,
    },
    /// Save a post's attachment into the exchange folder as `name` (or
    /// `name (1)` and so on if that is taken), if what arrives is the
    /// `size` bytes with hash `sha` the post describes.
    Save {
        post_id: String,
        sha: String,
        size: u64,
        name: String,
    },
    CreateChannel {
        name: String,
        topic: Option<String>,
    },
    DeleteChannel(String),
    DeletePost(String),
    /// Take this handle off the agents list on every device (as `me`).
    ForgetAgent(String),
    /// `me` has read `channel` up to the post that arrived as `arrival`.
    MarkRead {
        channel: String,
        arrival: i64,
    },
    /// Tell the node `me` is here (kind human, client `tui`).
    Heartbeat,
    /// The post a reply names, by its id or short id.
    LoadPost(String),
    Quit,
}

/// One channel row in the sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelItem {
    pub name: String,
    pub topic: Option<String>,
    pub unread: u32,
}

/// What the main pane shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Dang's posts from every channel.
    Announcements,
    /// One channel, by name.
    Channel(String),
}

/// One post, with only what rendering needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostItem {
    pub id: String,
    pub channel: String,
    pub ts_ms: i64,
    pub author: String,
    /// Written by a human: drawn on the announcement band.
    pub announcement: bool,
    pub client: Option<String>,
    pub node: String,
    pub body: String,
    pub tags: Vec<String>,
    /// Lowercase handles mentioned in the body.
    pub mentions: Vec<String>,
    pub attachment: Option<AttachmentItem>,
}

/// A post's attached file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentItem {
    pub name: String,
    /// Bytes.
    pub size: u64,
}

/// One agent row in the sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentItem {
    pub handle: String,
    pub client: String,
    pub node: String,
    pub status: Option<String>,
    pub last_seen_ms: i64,
    /// Seen recently enough to count as active (●) rather than idle (○).
    pub active: bool,
}

/// A sync peer, for the header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerItem {
    pub node: String,
    pub reachable: bool,
    /// Local events not yet delivered to this peer.
    pub queued: u64,
}

/// Which pane takes the keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Posts,
    Composer,
}

/// Whether the local server answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conn {
    Up,
    Down { since_ms: i64 },
}

/// A dialog drawn over the board, which dims behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialog {
    NewChannel(NewChannelForm),
    Confirm(ConfirmDialog),
    /// Pick a file from the exchange folder to post.
    Attach(AttachPicker),
    /// Every key, and what it does.
    Help,
}

/// The attach picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachPicker {
    /// The folder's files, newest first, once listed (or why they could not be).
    pub files: Option<Result<Vec<ExchangeFile>, String>>,
    /// Index of the highlighted file.
    pub selected: usize,
    /// The composer's text, which goes out as the file's message.
    pub message: String,
}

/// The "New channel" form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewChannelForm {
    pub name: String,
    pub topic: String,
    /// The field being typed into.
    pub field: FormField,
    /// Shown under the name field in place of its hint, in coral.
    pub error: Option<String>,
}

/// A field of [`NewChannelForm`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormField {
    Name,
    Topic,
}

/// A yes/no question before something destructive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmDialog {
    pub title: String,
    pub message: String,
    /// The primary action's verb (`delete`).
    pub action: String,
}

// ---------------------------------------------------------------- update

impl App {
    /// Applies `msg` and returns what the event loop should do next.
    pub fn update(&mut self, msg: Msg) -> Vec<Cmd> {
        let scrolled = self.scroll > 0;
        let filtered = !posts::shows_all(self);
        let agent_at = self.selected_agent_index();
        let focus = self.focus;
        let mut cmds = self.apply(msg);
        // A chord `space` started belongs to the pane it started in.
        if self.focus != focus {
            self.leader = false;
        }
        self.sync_mention();
        self.keep_cursor_in_view();
        self.keep_agent_cursor(agent_at);
        // Back down at the newest (by a key, a resize, a deletion or a sent
        // post), or the filter gone: what was out of sight is seen now.
        if self.scroll == 0 && (scrolled || (filtered && posts::shows_all(self))) {
            cmds.extend(self.caught_up());
        }
        // What the view's replies answer, asked for once per id.
        cmds.extend(self.ask_for_reply_targets());
        cmds
    }

    fn apply(&mut self, msg: Msg) -> Vec<Cmd> {
        match msg {
            Msg::Key(key) => self.on_key(key),
            Msg::Paste(text) => self.on_paste(&text),
            Msg::Resize(width, height) => {
                self.leader = false;
                // The posts wrap anew; a tall one being read keeps its place.
                self.rewrap(|app| app.size = (width, height));
                self.leave_hidden_sidebar();
                Vec::new()
            }
            Msg::Tick(now_ms) => self.on_tick(now_ms),
            Msg::Stream(event) => self.on_stream(event),
            Msg::Loaded(loaded) => self.on_loaded(loaded),
            Msg::Connection(conn) => self.on_connection(conn),
            Msg::Sent(result) => self.on_sent(result),
            Msg::Created(result) => self.on_created(result),
            Msg::PostDeleted(result) => {
                match result {
                    Ok(id) => {
                        self.remove_post(&id);
                        self.reply_target_deleted(&id);
                    }
                    Err(e) => self.error(format!("Not deleted: {e}")),
                }
                Vec::new()
            }
            Msg::ChannelDeleted(result) => match result {
                Ok(name) => {
                    self.channels.retain(|c| c.name != name);
                    self.reply_targets_lost_channel(&name);
                    let mut cmds = Vec::new();
                    if self.selected == Selection::Channel(name.clone()) {
                        cmds = self.channel_gone(&name);
                    }
                    self.toast(format!("deleted #{name}"));
                    cmds.push(Cmd::LoadChannels);
                    cmds
                }
                Err(e) => {
                    self.error(format!("Not deleted: {e}"));
                    Vec::new()
                }
            },
            Msg::AgentForgotten(result) => {
                match result {
                    Ok(handle) => {
                        self.agents.retain(|a| a.handle != handle);
                        self.toast(format!("forgot {handle}"));
                    }
                    Err(e) => self.error(format!("Not forgotten: {e}")),
                }
                Vec::new()
            }
            Msg::Saved(result) => {
                match result {
                    Ok(name) => self.toast(format!("saved to ./exchange/{name}")),
                    Err(e) => self.error(format!("Not saved: {e}")),
                }
                Vec::new()
            }
            Msg::Notice(text) => {
                self.error(text);
                Vec::new()
            }
            Msg::SecretRejected => {
                self.secret_rejected = true;
                Vec::new()
            }
        }
    }

    // ------------------------------------------------------------ toasts

    /// Shows `text` in the help row for a few seconds.
    pub(crate) fn toast(&mut self, text: impl Into<String>) {
        self.toast_for(text.into(), TOAST_MS, false);
    }

    /// Shows a failure in the help row, in coral, for longer.
    pub(crate) fn error(&mut self, text: impl Into<String>) {
        self.toast_for(text.into(), ERROR_TOAST_MS, true);
    }

    /// Shows what is being waited on (`saving x…`) as long as a failure
    /// stays; the answer replaces it.
    pub(crate) fn waiting(&mut self, text: impl Into<String>) {
        self.toast_for(text.into(), ERROR_TOAST_MS, false);
    }

    fn toast_for(&mut self, text: String, ms: i64, error: bool) {
        self.toast = Some(text);
        self.toast_error = error;
        self.toast_until_ms = Some(self.now_ms.saturating_add(ms));
    }

    // -------------------------------------------------------------- time

    fn on_tick(&mut self, now_ms: i64) -> Vec<Cmd> {
        let elapsed = now_ms.saturating_sub(self.now_ms);
        self.now_ms = now_ms;
        if self.leader {
            // The leader's hint covers the toast: its time doesn't run
            // until it shows again.
            if let Some(until) = &mut self.toast_until_ms {
                *until = until.saturating_add(elapsed);
            }
        } else if self.toast_until_ms.is_some_and(|until| now_ms >= until) {
            self.toast = None;
            self.toast_error = false;
            self.toast_until_ms = None;
        }
        // Presence ages with the clock: active for 5 minutes, then idle,
        // then gone after 2 hours.
        self.agents
            .retain(|a| now_ms.saturating_sub(a.last_seen_ms) <= AGENT_WINDOW_MS);
        for agent in &mut self.agents {
            agent.active = now_ms.saturating_sub(agent.last_seen_ms) <= ACTIVE_WINDOW_MS;
        }
        let mut cmds = Vec::new();
        if self.live {
            if due(self.last_heartbeat_ms, now_ms, HEARTBEAT_MS) {
                self.last_heartbeat_ms = Some(now_ms);
                cmds.push(Cmd::Heartbeat);
            }
            if due(self.last_status_ms, now_ms, STATUS_MS) {
                self.last_status_ms = Some(now_ms);
                cmds.push(Cmd::LoadStatus);
            }
        }
        cmds
    }

    // -------------------------------------------------------- connection

    fn on_connection(&mut self, conn: Conn) -> Vec<Cmd> {
        match conn {
            Conn::Down { since_ms } => {
                self.live = false;
                // Retries keep the time it first went down.
                if self.connection == Conn::Up {
                    self.connection = Conn::Down { since_ms };
                }
                Vec::new()
            }
            Conn::Up => {
                self.connection = Conn::Up;
                self.live = true;
                self.secret_rejected = false;
                self.retry_reply_targets();
                self.last_heartbeat_ms = Some(self.now_ms);
                self.last_status_ms = Some(self.now_ms);
                // Whatever happened while the feed was closed is read afresh.
                let mut cmds = vec![
                    Cmd::Heartbeat,
                    Cmd::LoadChannels,
                    Cmd::LoadAgents,
                    Cmd::LoadStatus,
                ];
                if self.view_chosen {
                    cmds.extend(self.load_view());
                }
                cmds
            }
        }
    }

    // ------------------------------------------------------------- views

    /// Shows `view` in the main pane and asks for its posts. A channel is
    /// marked read for `me` once they arrive.
    pub(crate) fn select(&mut self, view: Selection) -> Vec<Cmd> {
        if self.view_chosen && self.selected == view {
            return Vec::new();
        }
        self.view_chosen = true;
        self.selected = view;
        self.posts.clear();
        self.extras.clear();
        self.selected_post = None;
        self.scroll = 0;
        self.has_older = false;
        self.oldest_page = None;
        self.held.clear();
        self.retry_failed_reply_targets();
        self.load_view()
    }

    /// Asks for the current view's newest posts. What arrives live until
    /// they come is kept aside, so the page cannot lose it.
    fn load_view(&mut self) -> Vec<Cmd> {
        self.arrivals = Some(Arrivals::default());
        self.loading = self.posts.is_empty();
        self.load_failed = false;
        self.loading_older = false;
        vec![match &self.selected {
            Selection::Channel(name) => Cmd::LoadPosts {
                channel: name.clone(),
                before: None,
            },
            Selection::Announcements => Cmd::LoadAnnouncements,
        }]
    }

    /// Where to go when the channel on screen goes away.
    fn fallback_view(&self) -> Selection {
        self.channels.first().map_or(Selection::Announcements, |c| {
            Selection::Channel(c.name.clone())
        })
    }

    /// Whether `post` belongs in the view on screen.
    fn in_view(&self, post: &PostItem) -> bool {
        match &self.selected {
            Selection::Channel(name) => &post.channel == name,
            Selection::Announcements => post.announcement,
        }
    }

    /// Marks the channel on screen read up to the newest post loaded.
    fn mark_read(&mut self) -> Vec<Cmd> {
        let Selection::Channel(name) = &self.selected else {
            return Vec::new();
        };
        let name = name.clone();
        // What is on screen: the filter may hide some of the view.
        let newest = posts::shown(self)
            .iter()
            .filter_map(|p| self.extras.get(&p.id))
            .map(|e| e.arrival)
            .max();
        let Some(arrival) = newest else {
            return Vec::new();
        };
        if let Some(channel) = self.channels.iter_mut().find(|c| c.name == name) {
            channel.unread = 0;
        }
        vec![Cmd::MarkRead {
            channel: name,
            arrival,
        }]
    }

    /// The view is back at the newest post: if posts came in while it was
    /// scrolled up, they have now been seen.
    fn caught_up(&mut self) -> Vec<Cmd> {
        let Selection::Channel(name) = &self.selected else {
            return Vec::new();
        };
        if self
            .channels
            .iter()
            .any(|c| &c.name == name && c.unread > 0)
        {
            self.mark_read()
        } else {
            Vec::new()
        }
    }

    /// The channel on screen went away: says so and shows another view.
    fn channel_gone(&mut self, name: &str) -> Vec<Cmd> {
        self.toast(format!("#{name} was deleted"));
        let next = self.fallback_view();
        self.select(next)
    }

    // ------------------------------------------------------------ loaded

    fn on_loaded(&mut self, loaded: Loaded) -> Vec<Cmd> {
        match loaded {
            Loaded::Channels(list) => self.on_channels(&list),
            Loaded::Posts { view, before, page } => self.on_posts(view, before, page),
            Loaded::Agents(who) => {
                self.agents = who
                    .agents
                    .iter()
                    .filter(|p| p.kind == AuthorKind::Agent)
                    .map(convert::agent_item)
                    .collect();
                sort_agents(&mut self.agents);
                Vec::new()
            }
            Loaded::Status(status) => {
                self.node = convert::clean_line(&status.node);
                self.set_peers(&status.peers);
                Vec::new()
            }
            Loaded::Files(files) => {
                if let Some(Dialog::Attach(picker)) = &mut self.dialog {
                    picker.files = Some(files);
                    picker.selected = 0;
                }
                Vec::new()
            }
            Loaded::Post { id, post } => {
                self.on_reply_target(id, post);
                Vec::new()
            }
        }
    }

    fn on_channels(&mut self, list: &ChannelsJson) -> Vec<Cmd> {
        self.channels = list.channels.iter().map(convert::channel_item).collect();
        self.announcements_unread = list.announcements_unread.unwrap_or(0);
        if !self.view_chosen {
            // The board opens on its first channel (`general` comes first).
            let first = self.fallback_view();
            return self.select(first);
        }
        if let Selection::Channel(name) = &self.selected {
            if !self.channels.iter().any(|c| &c.name == name) {
                let name = name.clone();
                return self.channel_gone(&name);
            }
        }
        Vec::new()
    }

    fn on_posts(
        &mut self,
        view: Selection,
        before: Option<(i64, String)>,
        page: Result<PostsResponse, String>,
    ) -> Vec<Cmd> {
        if view != self.selected {
            return Vec::new();
        }
        let older = before.is_some();
        // An older page fits only right above the oldest page post; one
        // asked for before the view was reloaded would leave a gap.
        if older && before != self.oldest_page {
            return Vec::new();
        }
        let page = match page {
            Ok(page) => page,
            Err(e) => {
                if older {
                    self.loading_older = false;
                } else {
                    self.loading = false;
                    self.load_failed = true;
                    self.arrivals = None;
                }
                self.error(format!("Couldn't load {}: {e}", view_name(&view)));
                return Vec::new();
            }
        };
        let limit = match view {
            Selection::Channel(_) => PAGE,
            Selection::Announcements => ANNOUNCEMENTS_LIMIT,
        };
        let full = page.posts.len() >= limit as usize;
        for post in &page.posts {
            self.extras
                .insert(post.id.clone(), convert::post_extra(post));
        }
        let mut items: Vec<PostItem> = page.posts.iter().map(convert::post_item).collect();
        sort_posts(&mut items);
        // Where the next older page starts: set by pages only, never by a
        // live post, however old its ts.
        let page_oldest = items.first().map(|p| (p.ts_ms, p.id.clone()));

        if older {
            self.loading_older = false;
            self.has_older = full;
            if page_oldest.is_some() {
                self.oldest_page = page_oldest;
            }
            for item in items {
                if !self.posts.iter().any(|p| p.id == item.id) {
                    self.posts.push(item);
                }
            }
            // Held posts below the new cursor go in now; any above it come
            // with the next page, which is asked for after they arrived.
            for (item, extra) in std::mem::take(&mut self.held) {
                let above = full
                    && self
                        .oldest_page
                        .as_ref()
                        .is_some_and(|c| before_cursor(&item, c));
                if !above && !self.posts.iter().any(|p| p.id == item.id) {
                    self.extras.insert(item.id.clone(), extra);
                    self.posts.push(item);
                }
            }
            sort_posts(&mut self.posts);
            return Vec::new();
        }

        let has_older = full && matches!(self.selected, Selection::Channel(_));
        // An older page asked for while this one loaded (paging the posts
        // still on screen) fits only if this page starts where the last one
        // did. Otherwise it will be dropped, and paging must not wait for it.
        self.loading_older = self.loading_older && page_oldest == self.oldest_page;
        if !self.loading_older {
            self.held.clear();
        }
        let arrivals = self.arrivals.take().unwrap_or_default();
        let mut posts: Vec<PostItem> = items
            .into_iter()
            .filter(|p| !arrivals.deleted.contains(&p.id))
            .collect();
        for (post, extra) in arrivals.posts {
            if arrivals.deleted.contains(&post.id) || posts.iter().any(|p| p.id == post.id) {
                continue;
            }
            let above = has_older
                && page_oldest
                    .as_ref()
                    .is_some_and(|c| before_cursor(&post, c));
            if above {
                if self.loading_older {
                    self.hold(post, extra);
                }
                continue;
            }
            self.extras.insert(post.id.clone(), extra);
            posts.push(post);
        }
        sort_posts(&mut posts);
        self.oldest_page = page_oldest;
        self.posts = posts;
        let ids: HashSet<String> = self.posts.iter().map(|p| p.id.clone()).collect();
        self.extras.retain(|id, _| ids.contains(id));
        if self
            .selected_post
            .as_ref()
            .is_some_and(|id| !ids.contains(id))
        {
            self.selected_post = None;
        }
        self.loading = false;
        self.has_older = has_older;
        self.clamp_scroll();
        // A channel on screen is a channel read.
        self.mark_read()
    }

    // ------------------------------------------------------------ stream

    fn on_stream(&mut self, event: StreamEvent) -> Vec<Cmd> {
        match event {
            StreamEvent::Post(post) => self.on_live_post(&post),
            StreamEvent::PostDeleted { id } => {
                if let Some(arrivals) = &mut self.arrivals {
                    arrivals.posts.retain(|(p, _)| p.id != id);
                    arrivals.deleted.insert(id.clone());
                }
                self.held.retain(|(p, _)| p.id != id);
                self.remove_post(&id);
                self.reply_target_deleted(&id);
                // The unread counts may have included it.
                vec![Cmd::LoadChannels]
            }
            StreamEvent::Channel { channel, deleted } => {
                if deleted {
                    self.channels.retain(|c| c.name != channel.name);
                    self.reply_targets_lost_channel(&channel.name);
                    let mut cmds = Vec::new();
                    if self.selected == Selection::Channel(channel.name.clone()) {
                        cmds = self.channel_gone(&channel.name);
                    }
                    cmds.push(Cmd::LoadChannels);
                    return cmds;
                }
                let item = convert::channel_item(&channel);
                match self.channels.iter_mut().find(|c| c.name == item.name) {
                    Some(existing) => existing.topic = item.topic,
                    None => self.channels.push(item),
                }
                // The server knows where it goes and what is unread there.
                vec![Cmd::LoadChannels]
            }
            StreamEvent::Presence(presence) => {
                // The sidebar lists agents; Dang (and any other human) is not one.
                if presence.kind == AuthorKind::Agent {
                    let item = convert::agent_item(&presence);
                    match self
                        .agents
                        .iter_mut()
                        .find(|a| a.handle.eq_ignore_ascii_case(&item.handle))
                    {
                        Some(existing) => *existing = item,
                        None => self.agents.push(item),
                    }
                    sort_agents(&mut self.agents);
                }
                Vec::new()
            }
            // Forgotten on some device: gone until a presence event brings it back.
            StreamEvent::AgentForgotten { handle } => {
                let handle = convert::clean_line(&handle);
                self.agents.retain(|a| a.handle != handle);
                Vec::new()
            }
            StreamEvent::Peers(peers) => {
                self.set_peers(&peers);
                Vec::new()
            }
        }
    }

    fn on_live_post(&mut self, post: &PostJson) -> Vec<Cmd> {
        let item = convert::post_item(post);
        self.reply_target_arrived(&item);
        let mut cmds = Vec::new();
        if !self.channels.iter().any(|c| c.name == item.channel) {
            // A post can arrive before its channel does, or bring a deleted
            // channel back.
            self.channels.push(ChannelItem {
                name: item.channel.clone(),
                topic: None,
                unread: 0,
            });
            cmds.push(Cmd::LoadChannels);
        }
        let mine = item.author.eq_ignore_ascii_case(&self.me);
        let on_screen = self.selected == Selection::Channel(item.channel.clone());
        let announcement = item.announcement;
        let channel = item.channel.clone();
        let shown = posts::is_shown(self, &item);
        let placed = self.add_to_view(item, convert::post_extra(post));
        if mine {
            return cmds;
        }
        // Read only if it is where Dang is looking: the bottom of the view.
        if on_screen && placed && shown && self.scroll == 0 {
            cmds.push(Cmd::MarkRead {
                channel,
                arrival: post.arrival,
            });
        } else {
            if let Some(c) = self.channels.iter_mut().find(|c| c.name == channel) {
                c.unread = c.unread.saturating_add(1);
            }
            if announcement {
                self.announcements_unread = self.announcements_unread.saturating_add(1);
            }
        }
        cmds
    }

    fn set_peers(&mut self, peers: &[PeerJson]) {
        self.peers = peers.iter().map(convert::peer_item).collect();
        self.last_sync_ms = convert::last_sync_ms(peers);
    }

    // ----------------------------------------------------------- results

    fn on_sent(&mut self, result: Result<PostCreated, SendFailed>) -> Vec<Cmd> {
        let body = self.sending.take();
        let created = match result {
            Ok(created) => created,
            // The text stays in the composer, to send again.
            Err(SendFailed::NotSent(e)) => {
                self.error(format!("Not sent: {e}"));
                return Vec::new();
            }
            // If it was stored, the live feed brings it once it is back.
            Err(SendFailed::MaybeSent) => {
                self.error(MAYBE_SENT);
                return Vec::new();
            }
        };
        // Only what was sent leaves the composer; anything typed since stays.
        if let Some(body) = body {
            if self.input.starts_with(&body) {
                self.input.drain(..body.len());
            }
        }
        // An Esc pressed while it was on its way has nothing left to clear.
        if self.input.is_empty() {
            self.clear_armed = false;
        }
        let item = convert::post_item(&created.post);
        self.add_to_view(item, convert::post_extra(&created.post));
        // Down to the newest, to see it; what came in meanwhile is seen too
        // (`update` marks it read).
        self.scroll = 0;
        let queued: Vec<&str> = created
            .delivery
            .iter()
            .filter(|d| !d.delivered)
            .map(|d| d.node.as_str())
            .collect();
        if !queued.is_empty() {
            self.toast(format!("sent · queued for {} (offline)", queued.join(", ")));
        }
        Vec::new()
    }

    /// Puts a post that came outside a page (live, or just sent) into the
    /// view, if it belongs there. One older than the oldest page post, with
    /// more history above, is left for paging to bring in its place: shown
    /// now, it would become the oldest post and the next page would be asked
    /// for from it, skipping everything in between. Returns whether it is
    /// now in the view.
    fn add_to_view(&mut self, item: PostItem, extra: PostExtra) -> bool {
        if !self.in_view(&item) {
            return false;
        }
        // While the first page is on its way, the page decides (on_posts).
        if let Some(arrivals) = &mut self.arrivals {
            arrivals.posts.push((item.clone(), extra.clone()));
        }
        if self.has_older
            && self
                .oldest_page
                .as_ref()
                .is_some_and(|c| before_cursor(&item, c))
        {
            // Left for paging; a page already on its way may not have it.
            if self.loading_older {
                self.hold(item, extra);
            }
            return false;
        }
        self.extras.insert(item.id.clone(), extra);
        self.insert_post(item);
        true
    }

    /// Keeps `item` for the older page on its way (once).
    fn hold(&mut self, item: PostItem, extra: PostExtra) {
        if !self.held.iter().any(|(p, _)| p.id == item.id) {
            self.held.push((item, extra));
        }
    }

    fn on_created(&mut self, result: Result<ChannelAck, String>) -> Vec<Cmd> {
        let ack = match result {
            Ok(ack) => ack,
            Err(e) => {
                self.error(format!("Not created: {e}"));
                return Vec::new();
            }
        };
        let name = ack.channel.name.clone();
        if !self.channels.iter().any(|c| c.name == name) {
            self.channels.push(convert::channel_item(&ack.channel));
        }
        self.toast(if ack.created {
            format!("created #{name}")
        } else {
            format!("#{name} was already there")
        });
        self.focus = Focus::Composer;
        let mut cmds = self.select(Selection::Channel(name));
        cmds.push(Cmd::LoadChannels);
        cmds
    }

    // ------------------------------------------------------------- posts

    /// Adds `item` to the view in display order, once. When the view is
    /// scrolled up it stays put rather than jump.
    fn insert_post(&mut self, item: PostItem) {
        if self.posts.iter().any(|p| p.id == item.id) {
            return;
        }
        let at = self
            .posts
            .partition_point(|p| (p.ts_ms, p.id.as_str()) < (item.ts_ms, item.id.as_str()));
        // At the bottom nothing moves; measuring is only needed scrolled up.
        let width = match self.posts_width() {
            Some(width) if self.scroll > 0 => width,
            _ => {
                self.posts.insert(at, item);
                return;
            }
        };
        if at == self.posts.len() {
            // The usual case, a new newest post: it adds its own rows and
            // the gap above it, below the view.
            if posts::is_shown(self, &item) {
                let above = posts::shown(self).last().map(|p| (*p).clone());
                let gap = above.map_or(0, |p| posts::gap_rows(&p, &item, self));
                self.scroll += gap + posts::post_rows(&item, self, width);
            }
            self.posts.push(item);
            return;
        }
        // Somewhere in the middle (a late post): measure what moved.
        let before = posts::total_rows(self, &posts::spans(self, width));
        let id = item.id.clone();
        self.posts.insert(at, item);
        let spans = posts::spans(self, width);
        let below = spans.iter().find(|s| s.id == id).map(|s| s.bottom);
        if below.is_some_and(|b| b < self.scroll) {
            self.scroll += posts::total_rows(self, &spans).saturating_sub(before);
        }
    }

    /// Takes post `id` out of the view; a selection on it moves to a
    /// neighbour. Returns whether it was there.
    pub(crate) fn remove_post(&mut self, id: &str) -> bool {
        let Some(index) = self.posts.iter().position(|p| p.id == id) else {
            return false;
        };
        if self.selected_post.as_deref() == Some(id) {
            let shown: Vec<String> = posts::shown(self).iter().map(|p| p.id.clone()).collect();
            let at = shown.iter().position(|s| s == id);
            self.selected_post = at.and_then(|i| {
                shown
                    .get(i + 1)
                    .or_else(|| i.checked_sub(1).and_then(|j| shown.get(j)))
                    .cloned()
            });
        }
        self.posts.remove(index);
        self.extras.remove(id);
        self.clamp_scroll();
        true
    }

    // ------------------------------------------------------------ agents

    /// Where the agent row under the sidebar's cursor is in `agents`.
    pub(crate) fn selected_agent_index(&self) -> Option<usize> {
        let handle = self.selected_agent.as_deref()?;
        self.agents.iter().position(|a| a.handle == handle)
    }

    /// Keeps the agent cursor true after a message: only while the sidebar
    /// has the keys and is drawn (the switcher below 80 columns has no agent
    /// rows), and on a row that is there. A row that went away (forgotten,
    /// or quiet for 2 hours) passes the cursor to the row now in its place,
    /// `was_at`, or the one above; with no agents left it goes back to the
    /// view on screen.
    fn keep_agent_cursor(&mut self, was_at: Option<usize>) {
        let drawn = self.layout().is_some_and(|l| l.sidebar.is_some());
        if self.focus != Focus::Sidebar || !drawn {
            self.selected_agent = None;
        }
        if self.selected_agent.is_none() || self.selected_agent_index().is_some() {
            return;
        }
        let last = self.agents.len().checked_sub(1);
        self.selected_agent = was_at
            .zip(last)
            .map(|(at, last)| self.agents[at.min(last)].handle.clone());
    }

    // ------------------------------------------------------------ layout

    /// The board's layout at the terminal's size, if it is drawn at all.
    pub(crate) fn layout(&self) -> Option<Layout> {
        Layout::compute(Rect::new(0, 0, self.size.0, self.size.1), self)
    }

    /// The rows the posts are drawn in.
    pub(crate) fn viewport(&self) -> Option<Rect> {
        self.layout().map(|layout| posts::viewport(self, &layout))
    }

    pub(crate) fn posts_width(&self) -> Option<u16> {
        self.layout().map(|layout| layout.posts.width)
    }

    /// The highest useful scroll: the oldest loaded row at the top.
    pub(crate) fn max_scroll(&self) -> Option<usize> {
        let view = self.viewport()?;
        let total = posts::total_rows(self, &posts::spans(self, view.width));
        Some(total.saturating_sub(usize::from(view.height)))
    }

    pub(crate) fn clamp_scroll(&mut self) {
        if let Some(max) = self.max_scroll() {
            self.scroll = self.scroll.min(max);
        }
    }
}

/// Whether `post` sorts before the cursor `(ts, id)`.
fn before_cursor(post: &PostItem, cursor: &(i64, String)) -> bool {
    (post.ts_ms, post.id.as_str()) < (cursor.0, cursor.1.as_str())
}

/// Whether something done every `every` ms, last at `last`, is due at `now`.
fn due(last: Option<i64>, now: i64, every: i64) -> bool {
    last.is_none_or(|t| now.saturating_sub(t) >= every)
}

/// Display order: `(ts, id)`.
fn sort_posts(posts: &mut [PostItem]) {
    posts.sort_by(|a, b| (a.ts_ms, &a.id).cmp(&(b.ts_ms, &b.id)));
}

/// Most recently seen first.
fn sort_agents(agents: &mut [AgentItem]) {
    agents.sort_by(|a, b| {
        b.last_seen_ms
            .cmp(&a.last_seen_ms)
            .then_with(|| a.handle.cmp(&b.handle))
    });
}

/// `#beacon` or `announcements`, for messages.
pub(crate) fn view_name(view: &Selection) -> String {
    match view {
        Selection::Channel(name) => format!("#{name}"),
        Selection::Announcements => "announcements".to_string(),
    }
}

/// `Dang's`, but `pc-configs'`: a name ending in s takes only the apostrophe.
pub(crate) fn possessive(name: &str) -> String {
    if name.ends_with(['s', 'S']) {
        format!("{name}'")
    } else {
        format!("{name}'s")
    }
}
