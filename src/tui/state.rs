//! What the TUI shows: a plain view model that [`crate::tui::view::render`]
//! reads and never changes. Server types are converted into these structs by
//! the event loop, so rendering only depends on what it draws. How colors
//! reach the terminal is the [`crate::tui::theme::Theme`]'s business, not
//! the model's.

use jiff::tz::TimeZone;

/// Everything one frame of the TUI is drawn from.
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
    /// Sync peers, for the header.
    pub peers: Vec<PeerItem>,
    /// When the last sync with the peers finished.
    pub last_sync_ms: Option<i64>,
    /// The human at the keyboard (`BEACON_HUMAN`).
    pub me: String,
    /// The composer's text.
    pub input: String,
    pub focus: Focus,
    pub dialog: Option<Dialog>,
    pub connection: Conn,
    /// Active post filter without its leading `/`: a tag (`claim`) or `@handle`.
    pub filter: Option<String>,
    /// Id of the post selected in the posts pane.
    pub selected_post: Option<String>,
    /// A short notice shown in the help row (`saved to ./exchange/x`).
    pub toast: Option<String>,
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
            peers: Vec::new(),
            last_sync_ms: None,
            me: me.into(),
            input: String::new(),
            focus: Focus::Composer,
            dialog: None,
            connection: Conn::Up,
            filter: None,
            selected_post: None,
            toast: None,
        }
    }
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
