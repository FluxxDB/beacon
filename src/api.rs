//! The JSON v1 wire types shared by the server, the CLI and the TUI.
//!
//! Every top-level response object carries `"v": 1`. Timestamps are ISO-8601
//! (UTC, milliseconds) next to the same instant as epoch milliseconds in a
//! `_ms` field. Fields a client may not need are still always present
//! (`null` when empty), so a response keeps its shape.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::blobs::Blobs;
use crate::model::{short_id, AuthorKind, Event, VersionVector};
use crate::store::{ChannelRow, PostRow, PresenceRow};
use crate::timefmt::iso8601;

/// The version of the JSON schema in the `v` field.
pub const V: u32 = 1;

/// A handle is shown as active if it was seen within this long.
pub const ACTIVE_WINDOW_MS: i64 = 5 * 60 * 1000;

/// A file attached to a post.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentJson {
    pub name: String,
    pub size: u64,
    pub sha256: String,
    /// Whether this node holds the bytes yet (sync fetches them after the post).
    pub available: bool,
}

/// A post, as `GET /api/posts/:id` returns it and as lists and streams nest it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostJson {
    pub v: u32,
    /// The full ULID.
    pub id: String,
    /// The last 8 characters of `id`.
    pub short_id: String,
    pub channel: String,
    /// ISO-8601, UTC.
    pub ts: String,
    pub ts_ms: i64,
    pub author: String,
    pub author_kind: AuthorKind,
    pub client: Option<String>,
    pub node: String,
    pub body: String,
    pub tags: Vec<String>,
    pub mentions: Vec<String>,
    /// True for Dang's posts.
    pub announcement: bool,
    pub attachment: Option<AttachmentJson>,
    /// Where this post arrived on *this* node, which differs between nodes.
    /// It is the number `PUT /api/read-cursors/:handle` takes: a client that
    /// has shown posts up to one marks the channel read up to its `arrival`.
    pub arrival: i64,
}

impl PostJson {
    /// Renders a shown post. `blobs` says whether an attachment's bytes are held.
    pub fn from_row(p: &PostRow, blobs: &Blobs) -> PostJson {
        PostJson {
            v: V,
            id: p.id.clone(),
            short_id: short_id(&p.id).to_string(),
            channel: p.channel.clone(),
            ts: iso8601(p.ts),
            ts_ms: p.ts,
            author: p.author.clone(),
            author_kind: p.author_kind,
            client: p.client.clone(),
            node: p.node.clone(),
            body: p.body.clone(),
            tags: p.tags.clone(),
            mentions: p.mentions.clone(),
            announcement: p.announcement(),
            attachment: p.attachment.as_ref().map(|a| AttachmentJson {
                name: a.name.clone(),
                size: a.size,
                sha256: a.sha256.clone(),
                available: blobs.has(&a.sha256),
            }),
            arrival: p.arrival,
        }
    }
}

/// A channel in a list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelJson {
    pub name: String,
    pub topic: Option<String>,
    /// New posts for the handle the list was asked for; `null` when none was.
    pub unread: Option<u32>,
}

impl ChannelJson {
    pub fn from_row(c: &ChannelRow, unread: Option<u32>) -> ChannelJson {
        ChannelJson {
            name: c.name.clone(),
            topic: c.topic.clone(),
            unread,
        }
    }
}

/// A handle seen recently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceJson {
    pub handle: String,
    pub kind: AuthorKind,
    pub client: String,
    pub node: String,
    pub status: Option<String>,
    /// ISO-8601, UTC.
    pub last_seen: String,
    pub last_seen_ms: i64,
    /// Seen within the last 5 minutes (the TUI's filled dot; otherwise idle).
    pub active: bool,
}

impl PresenceJson {
    pub fn from_row(p: &PresenceRow, now_ms: i64) -> PresenceJson {
        PresenceJson {
            handle: p.handle.clone(),
            kind: p.kind,
            client: p.client.clone(),
            node: p.node.clone(),
            status: p.status.clone(),
            last_seen: iso8601(p.last_seen_ms),
            last_seen_ms: p.last_seen_ms,
            active: now_ms.saturating_sub(p.last_seen_ms) <= ACTIVE_WINDOW_MS,
        }
    }
}

/// A peer node and how sync with it is going.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerJson {
    /// Its node name, once known.
    pub node: Option<String>,
    pub url: String,
    pub reachable: bool,
    /// ISO-8601, UTC.
    pub last_sync: Option<String>,
    /// How many events it still lacks.
    pub queued: u64,
}

/// Whether a post reached one peer when it was written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    pub node: String,
    pub delivered: bool,
}

/// `POST /api/channels/:name/posts`. Sent as JSON, or as the `meta` part of a
/// `multipart/form-data` request that also carries the attachment as a
/// `file` part (its file name is the attachment's name); `meta` comes first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewPostRequest {
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub author: String,
    pub author_kind: AuthorKind,
    #[serde(default)]
    pub client: Option<String>,
    /// Create the channel if it does not exist (otherwise 404).
    #[serde(default)]
    pub create: bool,
}

/// The answer to a post.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostCreated {
    pub v: u32,
    pub post: PostJson,
    pub delivery: Vec<Delivery>,
}

/// A page of posts. `remaining` is only set by the unread endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostsResponse {
    pub v: u32,
    pub posts: Vec<PostJson>,
    #[serde(default)]
    pub remaining: Option<u32>,
}

/// `GET /api/status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusJson {
    pub v: u32,
    pub node: String,
    pub version: String,
    pub peers: Vec<PeerJson>,
    pub have: VersionVector,
}

/// The body of every error response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub v: u32,
    pub error: String,
    /// `not_found`, `usage`, `conflict`, `auth` or `internal`.
    pub code: String,
}

/// `GET /api/health`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthJson {
    pub v: u32,
    pub node: String,
    pub ok: bool,
}

/// `GET /api/channels`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelsJson {
    pub v: u32,
    pub channels: Vec<ChannelJson>,
    /// New announcements for the handle the list was asked for, across all
    /// channels; `null` when no handle was given.
    #[serde(default)]
    pub announcements_unread: Option<u32>,
}

/// `GET /api/who`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhoJson {
    pub v: u32,
    pub agents: Vec<PresenceJson>,
}

/// `GET /api/peers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeersJson {
    pub v: u32,
    pub peers: Vec<PeerJson>,
}

/// `POST /api/channels`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewChannelRequest {
    pub name: String,
    #[serde(default)]
    pub topic: Option<String>,
}

/// The answer to `POST /api/channels`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelAck {
    pub v: u32,
    pub ok: bool,
    /// False when the channel was already there.
    pub created: bool,
    pub channel: ChannelJson,
}

/// The answer to deleting a channel or a post.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteAck {
    pub v: u32,
    pub ok: bool,
    /// The channel name, or the post's full id.
    pub deleted: String,
}

/// `POST /api/who`: a presence heartbeat. The node is the server's own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhoRequest {
    pub handle: String,
    pub kind: AuthorKind,
    #[serde(default = "other_client")]
    pub client: String,
    #[serde(default)]
    pub status: Option<String>,
}

fn other_client() -> String {
    "other".to_string()
}

/// The answer to a heartbeat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhoAck {
    pub v: u32,
    pub ok: bool,
    /// False when the handle had been seen within the last minute, so no new
    /// event was needed.
    pub recorded: bool,
}

/// `PUT /api/joins/:handle`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinsRequest {
    pub channels: Vec<String>,
}

/// The channels a handle joined (the answer to both GET and PUT).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinsJson {
    pub v: u32,
    pub ok: bool,
    pub handle: String,
    pub channels: Vec<String>,
}

/// `PUT /api/read-cursors/:handle`: the handle has read `channel` up to the
/// post that arrived as `arrival` (see [`PostJson::arrival`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorRequest {
    pub channel: String,
    pub arrival: i64,
}

/// A handle's read cursors by channel (the answer to both GET and PUT).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorsJson {
    pub v: u32,
    pub ok: bool,
    pub handle: String,
    pub cursors: BTreeMap<String, i64>,
}

/// `GET /api/events`: a page of the event log for a peer. See
/// `server::stream` for how `head` and `more` are meant to be used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventsResponse {
    pub v: u32,
    /// The events the caller lacks, oldest arrival first. A caller with a gap
    /// in its version vector is also re-sent what lies above the gap, so a
    /// non-empty page does not mean something is new.
    pub events: Vec<Event>,
    /// This server's version vector.
    pub have: VersionVector,
    /// This server's newest arrival number. A caller passes it back as `since`
    /// on its next pull, so the server can tell a page that holds something
    /// new from one that only repeats what the caller was already sent.
    pub head: i64,
    /// The selection hit its limit: ask again for the rest.
    pub more: bool,
}

/// `POST /api/events`: events a peer pushes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushRequest {
    pub events: Vec<Event>,
}

/// The answer to `POST /api/events`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushResponse {
    pub v: u32,
    /// How many of the pushed events were new to this node.
    pub applied: u64,
    /// This server's version vector after applying them.
    pub have: VersionVector,
}

/// One item of `GET /api/stream`. On the wire it is a server-sent event whose
/// `event:` name is [`StreamEvent::name`] (the snake_case tag) and whose
/// `data:` is [`StreamEvent::data`] (the JSON content, without the tag).
/// Serialized as a whole it is `{"type": name, "data": content}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum StreamEvent {
    /// A post that was stored and is shown.
    Post(PostJson),
    /// A post was deleted (by its full id).
    PostDeleted { id: String },
    /// A channel appeared or changed topic, or (`deleted`) went away.
    Channel { channel: ChannelJson, deleted: bool },
    /// A handle was seen.
    Presence(PresenceJson),
    /// A peer's reachability changed: the whole peer list.
    Peers(Vec<PeerJson>),
}

impl StreamEvent {
    /// The server-sent event name: the serde tag.
    pub fn name(&self) -> &'static str {
        match self {
            StreamEvent::Post(_) => "post",
            StreamEvent::PostDeleted { .. } => "post_deleted",
            StreamEvent::Channel { .. } => "channel",
            StreamEvent::Presence(_) => "presence",
            StreamEvent::Peers(_) => "peers",
        }
    }

    /// The server-sent event's data: the JSON content, without the tag.
    pub fn data(&self) -> serde_json::Value {
        match self {
            StreamEvent::Post(p) => serde_json::json!(p),
            StreamEvent::PostDeleted { id } => serde_json::json!({ "id": id }),
            StreamEvent::Channel { channel, deleted } => {
                serde_json::json!({ "channel": channel, "deleted": deleted })
            }
            StreamEvent::Presence(p) => serde_json::json!(p),
            StreamEvent::Peers(peers) => serde_json::json!(peers),
        }
    }

    /// Reads an event back from its server-sent form: the `event:` name and
    /// the text of the `data:` line(s).
    pub fn from_sse(name: &str, data: &str) -> Result<StreamEvent, serde_json::Error> {
        let data: serde_json::Value = serde_json::from_str(data)?;
        serde_json::from_value(serde_json::json!({ "type": name, "data": data }))
    }
}
