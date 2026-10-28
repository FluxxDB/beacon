//! Server answers ([`crate::api`]) turned into the view structs of
//! [`crate::tui::state`] (ruling R12), so rendering never sees a wire type.
//!
//! Text from the server is shown as written, except that single-line fields
//! lose here every character [`is_unsafe`] names: controls, and the bidi
//! overrides and isolates that reorder what is shown, as the CLI does. (The
//! canvas also refuses to draw controls, so an escape sequence in a name or
//! topic can never reach the terminal.) Bodies keep their newlines and tabs
//! for wrapping; the posts widget cleans the rest when it draws them.

use jiff::Timestamp;
use reqwest::Url;

use crate::api::{ChannelJson, PeerJson, PostJson, PresenceJson};
use crate::text::is_unsafe;
use crate::tui::state::{AgentItem, AttachmentItem, ChannelItem, PeerItem, PostExtra, PostItem};

/// `s` without control characters (newlines, tabs and escapes included) or
/// bidi overrides: without anything [`is_unsafe`].
pub fn clean_line(s: &str) -> String {
    s.chars().filter(|&c| !is_unsafe(c)).collect()
}

pub fn post_item(p: &PostJson) -> PostItem {
    PostItem {
        id: p.id.clone(),
        channel: clean_line(&p.channel),
        ts_ms: p.ts_ms,
        author: clean_line(&p.author),
        announcement: p.announcement,
        client: p.client.as_deref().map(clean_line),
        node: clean_line(&p.node),
        body: p.body.clone(),
        tags: p.tags.iter().map(|t| clean_line(t)).collect(),
        mentions: p.mentions.iter().map(|m| m.to_ascii_lowercase()).collect(),
        attachment: p.attachment.as_ref().map(|a| AttachmentItem {
            name: clean_line(&a.name),
            size: a.size,
        }),
    }
}

/// What [`PostItem`] leaves out: where the post arrived here, and how to
/// fetch its file.
pub fn post_extra(p: &PostJson) -> PostExtra {
    PostExtra {
        arrival: p.arrival,
        sha256: p.attachment.as_ref().map(|a| a.sha256.clone()),
        available: p.attachment.as_ref().is_some_and(|a| a.available),
    }
}

pub fn channel_item(c: &ChannelJson) -> ChannelItem {
    ChannelItem {
        name: clean_line(&c.name),
        topic: c
            .topic
            .as_deref()
            .map(clean_line)
            .filter(|t| !t.trim().is_empty()),
        unread: c.unread.unwrap_or(0),
        read_to: c.read_to.unwrap_or(0),
    }
}

pub fn agent_item(p: &PresenceJson) -> AgentItem {
    AgentItem {
        handle: clean_line(&p.handle),
        client: clean_line(&p.client),
        node: clean_line(&p.node),
        status: p.status.as_deref().map(clean_line),
        last_seen_ms: p.last_seen_ms,
        active: p.active,
    }
}

/// A peer by its node name, or by its address's host until the name is known.
pub fn peer_item(p: &PeerJson) -> PeerItem {
    let node = match &p.node {
        Some(node) => clean_line(node),
        None => Url::parse(&p.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| clean_line(&p.url)),
    };
    PeerItem {
        node,
        reachable: p.reachable,
        queued: p.queued,
    }
}

/// The most recent sync with any peer, in epoch ms.
pub fn last_sync_ms(peers: &[PeerJson]) -> Option<i64> {
    peers
        .iter()
        .filter_map(|p| p.last_sync.as_deref())
        .filter_map(|s| s.parse::<Timestamp>().ok())
        .map(|t| t.as_millisecond())
        .max()
}
