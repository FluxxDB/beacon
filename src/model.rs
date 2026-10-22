//! The event model every node stores and syncs, plus the validation and
//! parsing rules for the names that appear in it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// One immutable change to the board, keyed by `(origin, seq)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// The database that created the event: `{node}~{db_id}`, where `db_id`
    /// is made once per database, so a node whose database is wiped starts a
    /// new origin instead of reusing seqs its peers already hold.
    pub origin: String,
    /// Position in the origin's own log; increases by one per event.
    pub seq: u64,
    /// ULID, unique across nodes.
    pub id: String,
    /// Milliseconds since the Unix epoch, at the origin.
    pub ts: i64,
    #[serde(flatten)]
    pub kind: EventKind,
}

/// What an event does. Serializes as `{"kind": "...", "data": {...}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum EventKind {
    ChannelCreated { name: String, topic: Option<String> },
    ChannelDeleted { name: String },
    PostCreated(NewPost),
    PostDeleted { post_id: String, by: String },
    AgentSeen(Presence),
    PeerAnnounced { node: String, url: String },
}

/// The payload of a `post_created` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewPost {
    pub post_id: String,
    pub channel: String,
    pub author: String,
    pub author_kind: AuthorKind,
    pub client: Option<String>,
    pub node: String,
    pub body: String,
    pub tags: Vec<String>,
    pub mentions: Vec<String>,
    pub attachment: Option<Attachment>,
}

/// Who wrote a post: Dang at the TUI, or an agent through the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthorKind {
    Human,
    Agent,
}

/// A file attached to a post; the bytes live in the blob store under `sha256`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub sha256: String,
    pub name: String,
    pub size: u64,
}

/// The payload of an `agent_seen` event (a presence heartbeat).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presence {
    pub handle: String,
    pub kind: AuthorKind,
    pub client: String,
    pub node: String,
    pub status: Option<String>,
}

const CHANNEL_MAX: usize = 32;
const HANDLE_MAX: usize = 32;
const TAG_MAX: usize = 24;
/// Longest post body, in bytes.
pub const BODY_MAX: usize = 64 * 1024;
/// Longest attachment name, in bytes.
pub const ATTACHMENT_NAME_MAX: usize = 255;

fn is_handle_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// Strips one leading `#`, lowercases, and checks the channel slug `[a-z0-9-]{1,32}`.
pub fn normalize_channel(s: &str) -> Result<String, Error> {
    let name = s.strip_prefix('#').unwrap_or(s).to_ascii_lowercase();
    let valid = (1..=CHANNEL_MAX).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if valid {
        Ok(name)
    } else {
        Err(Error::Usage(format!(
            "invalid channel name {s:?}: use 1-{CHANNEL_MAX} characters from a-z, 0-9 and '-'"
        )))
    }
}

/// Checks an agent or human handle: `[A-Za-z0-9][A-Za-z0-9._-]{0,31}`.
pub fn validate_handle(s: &str) -> Result<String, Error> {
    let valid = (1..=HANDLE_MAX).contains(&s.len())
        && s.starts_with(|c: char| c.is_ascii_alphanumeric())
        && s.chars().all(is_handle_char);
    if valid {
        Ok(s.to_string())
    } else {
        Err(Error::Usage(format!(
            "invalid handle {s:?}: start with a letter or digit, then letters, digits, '.', '_' or '-' (max {HANDLE_MAX})"
        )))
    }
}

/// Checks a post tag: `[a-z0-9-]{1,24}`.
pub fn validate_tag(s: &str) -> Result<String, Error> {
    let valid = (1..=TAG_MAX).contains(&s.len())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if valid {
        Ok(s.to_string())
    } else {
        Err(Error::Usage(format!(
            "invalid tag {s:?}: use 1-{TAG_MAX} characters from a-z, 0-9 and '-'"
        )))
    }
}

/// Finds the `@handle` mentions in a post body: lowercased, deduplicated, in
/// order of first appearance. An `@` only starts a mention at the start of the
/// text or after a character that cannot be part of a handle, so
/// `me@example.com` is not one. `all` is kept like any other handle.
pub fn parse_mentions(body: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut prev: Option<char> = None;
    for (i, c) in body.char_indices() {
        let starts_word = prev.is_none_or(|p| !is_handle_char(p));
        prev = Some(c);
        if c != '@' || !starts_word {
            continue;
        }
        let rest = &body[i + 1..];
        let end = rest
            .find(|c: char| !is_handle_char(c))
            .unwrap_or(rest.len());
        // Sentence punctuation ('.' is the only one that is also a handle
        // character) belongs to the sentence, not the handle.
        let word = rest[..end].trim_end_matches(['.', ',', ':', ';', '!', '?', ')']);
        if word.is_empty() || validate_handle(word).is_err() {
            continue;
        }
        let handle = word.to_ascii_lowercase();
        if !found.contains(&handle) {
            found.push(handle);
        }
    }
    found
}

/// Whether a post with these `mentions` is addressed to `handle`
/// (case-insensitive; `all` addresses everyone).
pub fn mentions_handle(mentions: &[String], handle: &str) -> bool {
    mentions
        .iter()
        .any(|m| m.eq_ignore_ascii_case("all") || m.eq_ignore_ascii_case(handle))
}

/// The last 8 characters of an id (all of it when shorter).
pub fn short_id(id: &str) -> &str {
    match id.char_indices().rev().nth(7) {
        Some((i, _)) => &id[i..],
        None => id,
    }
}

/// Per origin, the highest `seq` up to which every event of that origin is
/// held: an event above a missing one does not count until the gap is filled.
pub type VersionVector = BTreeMap<String, u64>;

/// Parses `origin:seq,origin:seq`; the empty string is the empty vector.
pub fn parse_vv(s: &str) -> Result<VersionVector, Error> {
    let mut vv = VersionVector::new();
    if s.trim().is_empty() {
        return Ok(vv);
    }
    for part in s.split(',') {
        let bad = || {
            Error::Usage(format!(
                "invalid version vector entry {part:?}: expected origin:seq"
            ))
        };
        let (origin, seq) = part.trim().rsplit_once(':').ok_or_else(bad)?;
        let seq: u64 = seq.parse().map_err(|_| bad())?;
        if origin.is_empty() || vv.insert(origin.to_string(), seq).is_some() {
            return Err(bad());
        }
    }
    Ok(vv)
}

/// Formats a version vector as `origin:seq,origin:seq`, sorted by origin.
pub fn format_vv(vv: &VersionVector) -> String {
    vv.iter()
        .map(|(origin, seq)| format!("{origin}:{seq}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Checks an event against the rules local input is held to: `seq` from 1 to
/// `i64::MAX`; channel names already normalized; handles, tags and lowercase
/// mentions valid; a body of at most [`BODY_MAX`] bytes; an attachment name of
/// 1 to [`ATTACHMENT_NAME_MAX`] bytes and a size SQLite can store.
pub fn validate_event(ev: &Event) -> Result<(), Error> {
    if ev.seq == 0 || i64::try_from(ev.seq).is_err() {
        return Err(Error::Usage(format!(
            "event seq {} is outside 1..={}",
            ev.seq,
            i64::MAX
        )));
    }
    match &ev.kind {
        EventKind::ChannelCreated { name, .. } | EventKind::ChannelDeleted { name } => {
            check_channel(name)
        }
        EventKind::PostCreated(post) => check_post(post),
        EventKind::PostDeleted { by, .. } => validate_handle(by).map(drop),
        EventKind::AgentSeen(p) => validate_handle(&p.handle).map(drop),
        EventKind::PeerAnnounced { .. } => Ok(()),
    }
}

/// A channel name in an event must already be in normal form.
fn check_channel(name: &str) -> Result<(), Error> {
    if normalize_channel(name)? == name {
        Ok(())
    } else {
        Err(Error::Usage(format!(
            "channel name {name:?} is not normalized"
        )))
    }
}

fn check_post(post: &NewPost) -> Result<(), Error> {
    check_channel(&post.channel)?;
    validate_handle(&post.author)?;
    for tag in &post.tags {
        validate_tag(tag)?;
    }
    for mention in &post.mentions {
        validate_handle(mention)?;
        if mention.chars().any(|c| c.is_ascii_uppercase()) {
            return Err(Error::Usage(format!(
                "mention {mention:?} is not lowercase"
            )));
        }
    }
    if post.body.len() > BODY_MAX {
        return Err(Error::Usage(format!(
            "post body is {} bytes, over the {BODY_MAX}-byte limit",
            post.body.len()
        )));
    }
    if let Some(att) = &post.attachment {
        if att.name.is_empty() || att.name.len() > ATTACHMENT_NAME_MAX {
            return Err(Error::Usage(format!(
                "attachment name must be 1-{ATTACHMENT_NAME_MAX} bytes"
            )));
        }
        if i64::try_from(att.size).is_err() {
            return Err(Error::Usage(format!(
                "attachment size {} is too large",
                att.size
            )));
        }
    }
    Ok(())
}
