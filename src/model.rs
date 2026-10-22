//! The event model every node stores and syncs, plus the validation and
//! parsing rules for the names that appear in it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::blobs::is_sha256;
use crate::error::Error;
use crate::sync::normalize_peer_url;

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
    ChannelCreated {
        name: String,
        topic: Option<String>,
    },
    ChannelDeleted {
        name: String,
    },
    PostCreated(NewPost),
    PostDeleted {
        post_id: String,
        by: String,
    },
    AgentSeen(Presence),
    /// `by` (Dang, usually) took `handle` off the presence list: it stays
    /// off until an `agent_seen` newer than this event checks it in again.
    AgentForgotten {
        handle: String,
        by: String,
    },
    PeerAnnounced {
        node: String,
        url: String,
    },
    /// A kind this build does not know (a newer peer made it), or a known
    /// kind whose data does not match it. Kept as it came, so it is stored
    /// and relayed like any event, but [`validate_event`] refuses it, so it
    /// never changes the board. Must stay the last variant.
    #[serde(untagged)]
    Unknown(UnknownKind),
}

/// The kind and data of an event this build cannot read, as they came.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnknownKind {
    pub kind: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub data: serde_json::Value,
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
/// Longest client name (`claude-code`, `tui`, ...), in bytes.
pub const CLIENT_MAX: usize = 64;
/// Longest presence status, in bytes.
pub const STATUS_MAX: usize = 200;
/// Longest channel topic, in bytes.
pub const TOPIC_MAX: usize = 200;
/// Most tags on one post.
pub const TAGS_MAX: usize = 8;
/// Longest announced peer URL, in bytes.
pub const PEER_URL_MAX: usize = 512;
/// Length of the database id at the end of an origin.
const DB_ID_LEN: usize = 8;

pub(crate) fn is_handle_char(c: char) -> bool {
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

/// Checks free text: at most `max` bytes (not characters), and no control
/// character (Unicode category Cc) other than the ones in `allowed`.
fn check_text(what: &str, text: &str, max: usize, allowed: &[char]) -> Result<(), Error> {
    if text.len() > max {
        return Err(Error::Usage(format!(
            "{what} is {} bytes, over the {max}-byte limit",
            text.len()
        )));
    }
    if let Some(c) = text
        .chars()
        .find(|c| c.is_control() && !allowed.contains(c))
    {
        return Err(Error::Usage(format!(
            "{what} contains a control character (U+{:04X})",
            u32::from(c)
        )));
    }
    Ok(())
}

/// Checks a client name: at most [`CLIENT_MAX`] bytes, no control characters.
pub fn validate_client(s: &str) -> Result<(), Error> {
    check_text("client", s, CLIENT_MAX, &[])
}

/// Checks a presence status: at most [`STATUS_MAX`] bytes, no control characters.
pub fn validate_status(s: &str) -> Result<(), Error> {
    check_text("status", s, STATUS_MAX, &[])
}

/// Checks a channel topic: at most [`TOPIC_MAX`] bytes, no control characters.
pub fn validate_topic(s: &str) -> Result<(), Error> {
    check_text("topic", s, TOPIC_MAX, &[])
}

/// Checks how many tags a post has: at most [`TAGS_MAX`].
pub fn validate_tag_count(count: usize) -> Result<(), Error> {
    if count > TAGS_MAX {
        return Err(Error::Usage(format!(
            "a post can have at most {TAGS_MAX} tags, not {count}"
        )));
    }
    Ok(())
}

/// Checks a post body's characters: no control character but `\n` and `\t`.
/// (The size limit, [`BODY_MAX`], is checked apart, because an API answers it
/// differently.)
pub fn validate_body_text(s: &str) -> Result<(), Error> {
    check_text("post body", s, usize::MAX, &['\n', '\t'])
}

/// Checks an attachment name: 1 to [`ATTACHMENT_NAME_MAX`] bytes, no control
/// characters.
pub fn validate_attachment_name(s: &str) -> Result<(), Error> {
    if s.is_empty() || s.len() > ATTACHMENT_NAME_MAX {
        return Err(Error::Usage(format!(
            "attachment name must be 1-{ATTACHMENT_NAME_MAX} bytes"
        )));
    }
    check_text("attachment name", s, ATTACHMENT_NAME_MAX, &[])
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

/// Length of a short id, and of a whole ULID.
const SHORT_ID_LEN: usize = 8;
const ULID_LEN: usize = 26;

/// What a reply's body says it answers. Agents answer a post by starting
/// theirs with `re <id>:` (there are no threads); see [`parse_reply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply<'a> {
    /// The id it names, uppercase: 8 characters, or a whole ULID.
    pub id: String,
    /// The body after `re <id>:` and the whitespace that follows it.
    pub rest: &'a str,
}

/// The reply `body` is, if it is one: it starts with `re` (any case), one or
/// more spaces and a post id, a short id (the last 8 characters of a ULID)
/// or a whole 26-character ULID, in Crockford base32 of any case; then maybe
/// `:` or `,`, then whitespace or the end of the body. Only that first id
/// counts. The one parser for the TUI, which draws a line naming the post a
/// reply answers, and the store, which makes a reply relevant to that post's
/// author. An 8-character id may be a word (`re branches:`), so neither
/// takes it on its own: the store needs a post with that id, and the TUI
/// asks for a digit too.
pub fn parse_reply(body: &str) -> Option<Reply<'_>> {
    let after_re = body
        .get(..2)
        .filter(|re| re.eq_ignore_ascii_case("re"))
        .map(|_| &body[2..])?;
    let at_id = after_re.trim_start_matches(' ');
    if at_id.len() == after_re.len() {
        return None;
    }
    let id_len = at_id
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(at_id.len());
    let (id, after_id) = at_id.split_at(id_len);
    if !is_post_id(id) {
        return None;
    }
    let after_mark = after_id.strip_prefix([':', ',']).unwrap_or(after_id);
    let rest = after_mark.trim_start();
    if !after_mark.is_empty() && rest.len() == after_mark.len() {
        return None;
    }
    Some(Reply {
        id: id.to_ascii_uppercase(),
        rest,
    })
}

/// A short id, or a whole ULID (whose first character is at most `7`).
fn is_post_id(id: &str) -> bool {
    let crockford = id.chars().all(|c| {
        matches!(
            c.to_ascii_uppercase(),
            '0'..='9' | 'A'..='H' | 'J' | 'K' | 'M' | 'N' | 'P'..='T' | 'V'..='Z'
        )
    });
    crockford
        && match id.len() {
            SHORT_ID_LEN => true,
            ULID_LEN => id.starts_with(|c: char| ('0'..='7').contains(&c)),
            _ => false,
        }
}

/// Whether `reference`, the id a reply names, names the post whose id is
/// `id`: the whole id, or its last 8 characters, in any case.
pub fn names_post(reference: &str, id: &str) -> bool {
    reference.eq_ignore_ascii_case(id)
        || (reference.len() == SHORT_ID_LEN && short_id(id).eq_ignore_ascii_case(reference))
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

/// Checks an event against the rules local input is held to: a kind this
/// build knows ([`EventKind::Unknown`] never passes); `seq` from 1 to
/// `i64::MAX`; an origin shaped like the ones stores make (see
/// [`validate_origin`]); channel names already normalized; handles (authors,
/// node names, tags, lowercase mentions) valid; at most [`TAGS_MAX`] tags; a
/// body of at most [`BODY_MAX`] bytes; an attachment name of 1 to
/// [`ATTACHMENT_NAME_MAX`] bytes, a size SQLite can store and a hash that is 64
/// lowercase hex digits; an announced peer URL that sync can use (see
/// [`normalize_peer_url`]); and free text within its limit ([`CLIENT_MAX`],
/// [`STATUS_MAX`], [`TOPIC_MAX`], [`PEER_URL_MAX`] bytes) with no control
/// characters (a post body may hold `\n` and `\t`).
///
/// Peers send events made elsewhere, so every name in one is checked here,
/// not only the ones local input could get wrong.
pub fn validate_event(ev: &Event) -> Result<(), Error> {
    if ev.seq == 0 || i64::try_from(ev.seq).is_err() {
        return Err(Error::Usage(format!(
            "event seq {} is outside 1..={}",
            ev.seq,
            i64::MAX
        )));
    }
    validate_origin(&ev.origin)?;
    match &ev.kind {
        EventKind::ChannelCreated { name, topic } => {
            check_channel(name)?;
            topic.as_deref().map_or(Ok(()), validate_topic)
        }
        EventKind::ChannelDeleted { name } => check_channel(name),
        EventKind::PostCreated(post) => check_post(post),
        EventKind::PostDeleted { by, .. } => validate_handle(by).map(drop),
        EventKind::AgentSeen(p) => {
            validate_handle(&p.handle)?;
            validate_handle(&p.node)?;
            validate_client(&p.client)?;
            p.status.as_deref().map_or(Ok(()), validate_status)
        }
        EventKind::AgentForgotten { handle, by } => {
            validate_handle(handle)?;
            validate_handle(by).map(drop)
        }
        EventKind::PeerAnnounced { node, url } => {
            validate_handle(node)?;
            check_text("peer URL", url, PEER_URL_MAX, &[])?;
            match normalize_peer_url(url) {
                Some(_) => Ok(()),
                None => Err(Error::Usage(format!(
                    "announced URL {url:?} is not a plain http:// URL"
                ))),
            }
        }
        EventKind::Unknown(u) => Err(Error::Usage(format!(
            "event kind {:?} is not one this build knows, or its data does not fit it",
            u.kind
        ))),
    }
}

/// Checks an event origin: `{node}~{db_id}`, a handle, then the 8
/// characters a store takes from the end of a ULID, lowercased (Crockford
/// base32: digits and lowercase letters except `i`, `l`, `o` and `u`). Nothing
/// else may be stored: an origin holding `,` or `:` would break the
/// `origin:seq,origin:seq` version vectors every pull sends.
pub fn validate_origin(origin: &str) -> Result<(), Error> {
    let bad = || {
        Error::Usage(format!(
            "event origin {origin:?} is not {{node}}~{{{DB_ID_LEN}-character database id}}"
        ))
    };
    let (node, db_id) = origin.split_once('~').ok_or_else(bad)?;
    validate_handle(node).map_err(|_| bad())?;
    let crockford = |b: u8| matches!(b, b'0'..=b'9' | b'a'..=b'h' | b'j' | b'k' | b'm' | b'n' | b'p'..=b't' | b'v'..=b'z');
    if db_id.len() != DB_ID_LEN || !db_id.bytes().all(crockford) {
        return Err(bad());
    }
    Ok(())
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
    validate_handle(&post.node)?;
    if let Some(client) = &post.client {
        validate_client(client)?;
    }
    validate_tag_count(post.tags.len())?;
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
    validate_body_text(&post.body)?;
    if let Some(att) = &post.attachment {
        validate_attachment_name(&att.name)?;
        // The hash becomes a file name, so it must not be anything else.
        if !is_sha256(&att.sha256) {
            return Err(Error::Usage(format!(
                "attachment hash {:?} is not 64 lowercase hex digits",
                att.sha256
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
