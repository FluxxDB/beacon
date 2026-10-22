//! The event log every node keeps, and the board state derived from it.
//!
//! [`Store::apply`] (one event) and [`Store::apply_all`] (a page of them, in
//! one transaction) store each event once per `(origin, seq)` and update the
//! derived tables (channels, posts, presence, announced peers) in the same
//! transaction. An event whose payload breaks [`validate_event`] is stored
//! and relayed but changes no derived state. The derived state depends only
//! on the set of events stored, never on the order they arrived in, so nodes
//! that hold the same events show the same board; a build whose rules derive
//! differently bumps [`DERIVE_VERSION`], and opening the database then derives
//! the board again from the log. See `schema.sql` for the tables and the
//! visibility rule. What each handle has read and joined lives beside the
//! log, local to the node; see [`Unread`].
//!
//! Every commit is synced to disk before it returns (`synchronous=FULL`): a
//! peer may hold an event a moment later, and a node that lost it in a power
//! cut would write a different event at the same seq. Should that happen
//! anyway (a restored backup, a disk that lied), [`Store::apply`] notices
//! when a peer offers the other event and moves this database to a new
//! origin; see `Store::on_collision`.

/// SQL for "post `p` mentions the handle bound to `$param` (or `@all`)",
/// mirroring [`crate::model::mentions_handle`]. One definition for
/// [`Store::posts`] and the unread queries.
macro_rules! mentions_sql {
    ($param:literal) => {
        concat!(
            "EXISTS (SELECT 1 FROM json_each(p.mentions_json) \
             WHERE lower(value) IN ('all', lower(",
            $param,
            ")))"
        )
    };
}

mod unread;

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, Type, Value, ValueRef};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row, ToSql};
use serde::de::DeserializeOwned;
use ulid::{Generator, Ulid};

pub use unread::Unread;

use crate::error::Error;
use crate::model::{
    validate_event, validate_origin, Attachment, AuthorKind, Event, EventKind, NewPost, Presence,
    VersionVector,
};

type Result<T, E = Error> = std::result::Result<T, E>;

const SCHEMA: &str = include_str!("schema.sql");

/// The rules the derived tables were built by. Bump it whenever a change to
/// [`validate_event`] or to what an event derives would build a different
/// board from the same events (a rule loosened, a new kind learned): opening
/// a database built under another version derives the board again from its
/// events. Stored in `meta` as `derive_version`.
pub const DERIVE_VERSION: u32 = 1;
/// The `meta` key prefix of a database id this database wrote under before
/// it moved to its current one (`Store::move_origin`).
const FORMER_DB_ID: &str = "former_db_id:";

/// The channel a fresh board starts with; listed first.
const GENERAL: &str = "general";
/// `who` lists handles seen within this long.
const WHO_WINDOW_MS: i64 = 2 * 60 * 60 * 1000;
/// A handle needs a new heartbeat event after this long (or a new status).
const PRESENCE_INTERVAL_MS: i64 = 60 * 1000;
/// Shortest id suffix [`Store::post`] accepts.
const MIN_ID_SUFFIX: usize = 6;

/// The columns [`post_row`] reads, from `posts AS p`.
const POST_COLUMNS: &str = "p.id, p.channel, p.ts, p.author, p.author_kind, p.client, p.node, \
     p.body, p.tags_json, p.mentions_json, p.att_sha, p.att_name, p.att_size, p.arrival";
/// The posts that are shown: not tombstoned, in a visible channel. Ends in a
/// `WHERE` clause, so callers append `AND …`.
const SHOWN_POSTS: &str =
    "posts AS p JOIN visible_channels AS v ON v.name = p.channel WHERE p.deleted = 0";
/// A visible channel with the ts of its newest shown post, from `visible_channels AS v`.
const CHANNEL_SELECT: &str = "SELECT v.name, v.topic, \
     (SELECT MAX(p.ts) FROM posts AS p WHERE p.channel = v.name AND p.deleted = 0) \
     FROM visible_channels AS v";

/// One node's copy of the board: the event log plus the state derived from it.
///
/// Not internally synchronized; the server keeps it behind a mutex. The
/// fields are private to `store` and its child modules (such as `unread`).
pub struct Store {
    conn: Connection,
    /// This node's name (`BEACON_NODE`), as it appears in payloads.
    node: String,
    /// `{node}~{db_id}`: the origin of every event this database appends.
    origin: String,
    /// Every database id this database has written under: the one in
    /// `origin` and any it moved off. An origin ending in one is ours.
    own_db_ids: BTreeSet<String>,
    ids: Generator,
    /// Per origin, the highest seq up to which every event is stored.
    vv: VersionVector,
    /// The highest arrival stored (0 when empty).
    head: i64,
    /// Our origin when the store was opened, and how far its events went
    /// then; see [`Store::opened_at`].
    opened_at: (String, u64),
}

/// The part of a [`Store`] a transaction may change besides the database,
/// put back if the transaction rolls back.
struct Saved {
    origin: String,
    own_db_ids: BTreeSet<String>,
    vv: VersionVector,
    head: i64,
}

/// What [`Store::apply_all`] did with a page of events.
#[derive(Debug, Default)]
pub struct Batch {
    /// The events of the page that were new here, in the order given (an
    /// invalid one too: it is stored, though it derives nothing).
    pub applied: Vec<Applied>,
    /// Events this node wrote along the way: the two sides of a collision on
    /// one of our origins, recorded again under our current origin (see
    /// `Store::on_collision`). Announce them like the rest.
    pub reissued: Vec<Applied>,
    /// The events no store can hold (see [`Store::apply`]), by their index
    /// in the page, with why. Nothing of them was stored.
    pub refused: Vec<(usize, Error)>,
}

/// An event this node has just stored, with its local arrival number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub event: Event,
    pub arrival: i64,
}

/// A visible channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelRow {
    pub name: String,
    /// The topic of its newest `channel_created`.
    pub topic: Option<String>,
    /// The ts of its newest shown post.
    pub last_post_ts: Option<i64>,
}

/// Which posts [`Store::posts`] returns: the newest `limit` shown posts that
/// match every filter given, oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostQuery {
    /// Only this channel (all channels when `None`).
    pub channel: Option<String>,
    /// Only posts before this `(ts, id)`, for paging back.
    pub before: Option<(i64, String)>,
    pub limit: usize,
    /// Only posts carrying this tag.
    pub tag: Option<String>,
    /// Only posts mentioning this handle or `@all` (case-insensitive).
    pub mentions_of: Option<String>,
    /// Only Dang's posts.
    pub announcements_only: bool,
}

impl Default for PostQuery {
    /// Every shown post, newest 50.
    fn default() -> Self {
        PostQuery {
            channel: None,
            before: None,
            limit: 50,
            tag: None,
            mentions_of: None,
            announcements_only: false,
        }
    }
}

/// A shown post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostRow {
    pub id: String,
    pub channel: String,
    /// The post's event ts (ms at its origin).
    pub ts: i64,
    pub author: String,
    pub author_kind: AuthorKind,
    pub client: Option<String>,
    pub node: String,
    pub body: String,
    pub tags: Vec<String>,
    pub mentions: Vec<String>,
    pub attachment: Option<Attachment>,
    /// Local arrival of its `post_created` event; differs between nodes.
    pub arrival: i64,
}

impl PostRow {
    /// Whether this is one of Dang's posts.
    pub fn announcement(&self) -> bool {
        self.author_kind == AuthorKind::Human
    }
}

/// A handle's presence, from its newest heartbeat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresenceRow {
    pub handle: String,
    pub kind: AuthorKind,
    pub client: String,
    pub node: String,
    /// The newest status it set; heartbeats without a status keep it.
    pub status: Option<String>,
    pub last_seen_ms: i64,
}

impl Store {
    /// Opens (or creates) the database at `path` in WAL mode, for node `node`.
    pub fn open(path: &Path, node: &str) -> Result<Store> {
        let conn = Connection::open(path)?;
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))?;
        // Each commit reaches the disk before it returns: peers may hold an
        // event as soon as it is committed (see the module docs).
        conn.pragma_update(None, "synchronous", "FULL")?;
        // The image has no /tmp and a container may have a read-only root:
        // temporary tables and sorts must not need a file.
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        Store::init(conn, node)
    }

    /// A store that lives only in memory, for tests.
    pub fn open_in_memory(node: &str) -> Result<Store> {
        Store::init(Connection::open_in_memory()?, node)
    }

    /// Creates the tables and this database's id, derives the board again if
    /// it was derived under another [`DERIVE_VERSION`], loads the version
    /// vector and, on a board with no channel event yet, creates `general`.
    fn init(mut conn: Connection, node: &str) -> Result<Store> {
        conn.execute_batch(SCHEMA)?;
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('db_id', ?1) ON CONFLICT (key) DO NOTHING",
            [new_db_id()],
        )?;
        rederive_if_stale(&mut conn)?;
        let (db_id, own_db_ids) = load_db_ids(&conn)?;
        let vv = load_version_vector(&conn)?;
        let head = conn.query_row("SELECT COALESCE(MAX(arrival), 0) FROM events", [], |r| {
            r.get(0)
        })?;
        let has_channel_event: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM channel_marks WHERE kind IN ('created', 'deleted'))",
            [],
            |r| r.get(0),
        )?;
        let origin = format!("{node}~{db_id}");
        let opened_at = (origin.clone(), vv.get(&origin).copied().unwrap_or(0));
        let mut store = Store {
            conn,
            node: node.to_string(),
            origin,
            own_db_ids,
            ids: Generator::new(),
            vv,
            head,
            opened_at,
        };
        if !has_channel_event {
            let general = EventKind::ChannelCreated {
                name: GENERAL.into(),
                topic: None,
            };
            // ts 0, so every new device's `general` is older than any channel
            // event a person made (a topic, a delete); the id keeps real time.
            store.append(general, 0, wall_clock_ms())?;
        }
        Ok(store)
    }

    /// This node's name, as given to `open`.
    pub fn node(&self) -> &str {
        &self.node
    }

    /// The origin of the events this database appends: `{node}~{db_id}`.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// Our origin when this store was opened, and the seq its events went up
    /// to then. Ours above that seq were written since. If the database had
    /// lost commits a peer already held (a restored backup, a disk that lied
    /// about a flush), those are the seqs that may have been written twice,
    /// and only the peer's copies show it, so sync asks each peer for them
    /// once (see [`Store::apply`] for what a collision does).
    pub fn opened_at(&self) -> (String, u64) {
        self.opened_at.clone()
    }

    /// Records a change made on this node: the next seq of our own origin, a
    /// new ULID, ts `now_ms`, then [`Store::apply`]. A `post_created` with an
    /// empty `post_id` gets the event's id as its post id. A payload that
    /// breaks [`validate_event`] is [`Error::Usage`] and is not stored.
    pub fn append_local(&mut self, kind: EventKind, now_ms: i64) -> Result<Applied> {
        self.append(kind, now_ms, now_ms)
    }

    /// [`Store::append_local`] with ts `ts` and an id made for time `id_ms`.
    fn append(&mut self, kind: EventKind, ts: i64, id_ms: i64) -> Result<Applied> {
        self.in_tx(|s| s.append_in_tx(kind, ts, id_ms, &mut Vec::new()))
    }

    /// [`Store::append`] inside the caller's transaction; a collision it
    /// causes (it cannot: the seq is new) would add to `reissued`.
    fn append_in_tx(
        &mut self,
        mut kind: EventKind,
        ts: i64,
        id_ms: i64,
        reissued: &mut Vec<Applied>,
    ) -> Result<Applied> {
        let seq: u64 = self.conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE origin = ?1",
            [&self.origin],
            |r| r.get(0),
        )?;
        let id = self.next_id(id_ms).to_string();
        if let EventKind::PostCreated(post) = &mut kind {
            if post.post_id.is_empty() {
                post.post_id = id.clone();
            }
        }
        let ev = Event {
            origin: self.origin.clone(),
            seq,
            id,
            ts,
            kind,
        };
        validate_event(&ev)?;
        self.store_event(&ev, reissued)?.ok_or_else(|| {
            Error::Internal(anyhow::anyhow!(
                "local event {}:{} is already stored",
                ev.origin,
                ev.seq
            ))
        })
    }

    /// Stores `ev` and updates the derived state, all in one transaction.
    /// Returns `None`, changing nothing, if `(origin, seq)` is already stored
    /// (a different event there is a collision: see `Store::on_collision`;
    /// what it records again is stored and syncs, and [`Store::apply_all`]
    /// reports it).
    ///
    /// An event whose payload breaks [`validate_event`] is still stored (so
    /// version vectors stay gap-free and it relays) and returned, but changes
    /// no derived state; code that renders events should read derived state or
    /// check it too. Two things make an event impossible to store at all, and
    /// are [`Error::Usage`] with nothing stored: a seq above `i64::MAX`, and an
    /// origin that is not `{node}~{db_id}` ([`validate_origin`]), which would
    /// break every version vector it appeared in.
    pub fn apply(&mut self, ev: &Event) -> Result<Option<Applied>> {
        storable(ev)?;
        self.in_tx(|s| s.store_event(ev, &mut Vec::new()))
    }

    /// [`Store::apply`] for each of `events`, in order, in one transaction
    /// (one sync to disk for a whole page from a peer). Each event keeps its
    /// own rules: an invalid one is stored without deriving anything, and one
    /// no store can hold is left out and listed in [`Batch::refused`] while
    /// the rest go in. Any other failure stores nothing of the page.
    pub fn apply_all(&mut self, events: &[Event]) -> Result<Batch> {
        if events.is_empty() {
            return Ok(Batch::default());
        }
        self.in_tx(|s| {
            let mut batch = Batch::default();
            for (i, ev) in events.iter().enumerate() {
                if let Err(why) = storable(ev) {
                    batch.refused.push((i, why));
                    continue;
                }
                if let Some(applied) = s.store_event(ev, &mut batch.reissued)? {
                    batch.applied.push(applied);
                }
            }
            Ok(batch)
        })
    }

    /// Runs `f` in one transaction: committed if it returns `Ok`; rolled
    /// back, with the in-memory state as it was, if it fails.
    fn in_tx<T>(&mut self, f: impl FnOnce(&mut Store) -> Result<T>) -> Result<T> {
        if !self.conn.is_autocommit() {
            // A panic inside an earlier transaction left it open: drop what
            // it did, and read the state back from what is committed.
            self.conn.execute_batch("ROLLBACK")?;
            self.reload()?;
        }
        let saved = Saved {
            origin: self.origin.clone(),
            own_db_ids: self.own_db_ids.clone(),
            vv: self.vv.clone(),
            head: self.head,
        };
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let done = f(self).and_then(|value| {
            self.conn.execute_batch("COMMIT")?;
            Ok(value)
        });
        if done.is_err() {
            if !self.conn.is_autocommit() {
                if let Err(e) = self.conn.execute_batch("ROLLBACK") {
                    tracing::error!("cannot roll back a failed write: {e}");
                }
            }
            self.origin = saved.origin;
            self.own_db_ids = saved.own_db_ids;
            self.vv = saved.vv;
            self.head = saved.head;
        }
        done
    }

    /// Reads the in-memory state back from the database.
    fn reload(&mut self) -> Result<()> {
        let (db_id, own_db_ids) = load_db_ids(&self.conn)?;
        self.origin = format!("{}~{db_id}", self.node);
        self.own_db_ids = own_db_ids;
        self.vv = load_version_vector(&self.conn)?;
        self.head =
            self.conn
                .query_row("SELECT COALESCE(MAX(arrival), 0) FROM events", [], |r| {
                    r.get(0)
                })?;
        Ok(())
    }

    /// [`Store::apply`] for an event that is [`storable`], inside the
    /// caller's transaction. The in-memory state moves with it; the caller
    /// puts it back if the transaction fails.
    fn store_event(&mut self, ev: &Event, reissued: &mut Vec<Applied>) -> Result<Option<Applied>> {
        let invalid = validate_event(ev).err();
        let json = serde_json::to_string(ev).map_err(anyhow::Error::from)?;
        let inserted = self
            .conn
            .prepare_cached(
                "INSERT INTO events (origin, seq, id, ts, json) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (origin, seq) DO NOTHING",
            )?
            .execute(params![ev.origin, ev.seq, ev.id, ev.ts, json])?;
        if inserted == 0 {
            self.on_collision(ev, reissued)?;
            return Ok(None);
        }
        let arrival = self.conn.last_insert_rowid();
        match &invalid {
            None => derive(&self.conn, ev, arrival)?,
            Some(why) => tracing::warn!(
                origin = %ev.origin,
                seq = ev.seq,
                "stored an invalid event without applying it: {why}"
            ),
        }
        let have = self.vv.get(&ev.origin).copied().unwrap_or(0);
        let have = gapless_from(&self.conn, &ev.origin, have)?;
        self.vv.insert(ev.origin.clone(), have);
        self.head = arrival;
        Ok(Some(Applied {
            event: ev.clone(),
            arrival,
        }))
    }

    /// `ev` claims an `(origin, seq)` that is already stored. The same event
    /// (by id) again is nothing new. A different one means that origin wrote
    /// two events at one seq: its database lost commits a peer already had
    /// (a power cut, a restored backup) and reused their seqs. Nodes that
    /// hold one of the two never take the other at that place, so:
    ///
    /// - the stored event is kept, never overwritten, and the collision is
    ///   logged as an error;
    /// - if the origin is ours (now or before we moved), this node records
    ///   both events' changes again, with their own ts, as new events of our
    ///   current origin, moving to a new origin first if the collision is on
    ///   the one we write under, so no later write of ours reuses a seq a
    ///   peer holds. Every node then gets both changes; a post keeps its id,
    ///   so the copy of a post a node already shows changes nothing there.
    ///
    /// Each collision is settled once (`settled_collisions`): another peer
    /// offering the same event later changes nothing. A collision on another
    /// node's origin is left to that node, which sees it when a peer offers
    /// it the other event (sync asks for them; see [`Store::opened_at`]).
    fn on_collision(&mut self, ev: &Event, reissued: &mut Vec<Applied>) -> Result<()> {
        let (stored_id, stored_json): (String, String) = self
            .conn
            .prepare_cached("SELECT id, json FROM events WHERE origin = ?1 AND seq = ?2")?
            .query_row(params![ev.origin, ev.seq], |r| Ok((r.get(0)?, r.get(1)?)))?;
        if stored_id == ev.id {
            return Ok(());
        }
        let ours = ev
            .origin
            .split_once('~')
            .is_some_and(|(_, db_id)| self.own_db_ids.contains(db_id));
        tracing::error!(
            origin = %ev.origin,
            seq = ev.seq,
            stored = %stored_id,
            offered = %ev.id,
            "two different events claim one origin and seq; keeping the stored one{}",
            if ours { " and recording both again (the origin is ours)" } else { "" }
        );
        if !ours {
            return Ok(());
        }
        let first_time = self
            .conn
            .prepare_cached(
                "INSERT INTO settled_collisions (origin, seq, id) VALUES (?1, ?2, ?3)
                 ON CONFLICT DO NOTHING",
            )?
            .execute(params![ev.origin, ev.seq, ev.id])?;
        if first_time == 0 {
            return Ok(());
        }
        if ev.origin == self.origin {
            self.move_origin()?;
        }
        let stored = parse_event(&stored_json)?;
        for lost in [ev, &stored] {
            if let Err(why) = validate_event(lost) {
                tracing::warn!(
                    origin = %lost.origin,
                    seq = lost.seq,
                    "cannot record an event again: {why}"
                );
                continue;
            }
            let again = self.append_in_tx(lost.kind.clone(), lost.ts, wall_clock_ms(), reissued)?;
            reissued.push(again);
        }
        Ok(())
    }

    /// Gives this database a new id, so it writes under a new origin from
    /// now on; the old id stays one of ours.
    fn move_origin(&mut self) -> Result<()> {
        let new = loop {
            let id = new_db_id();
            if !self.own_db_ids.contains(&id) {
                break id;
            }
        };
        let old = self.origin.clone();
        let old_db_id = old.rsplit_once('~').map_or("", |(_, d)| d);
        self.conn
            .execute("UPDATE meta SET value = ?1 WHERE key = 'db_id'", [&new])?;
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO NOTHING",
            [format!("{FORMER_DB_ID}{old_db_id}"), new.clone()],
        )?;
        self.own_db_ids.insert(new.clone());
        self.origin = format!("{}~{new}", self.node);
        tracing::error!(
            from = %old,
            to = %self.origin,
            "this node reused a seq a peer already holds (it lost recent commits); writing under a new origin from now on"
        );
        Ok(())
    }

    /// For every origin stored, the highest seq up to which this node has
    /// every event (so a gap is asked for again, never skipped).
    ///
    /// Because it stops at a gap, a peer syncing from this vector is sent the
    /// events above the gap again, which this node already holds. So a
    /// non-empty [`Store::events_after`] does not mean the caller lacks
    /// anything: two nodes missing the same event re-send each other what lies
    /// above it for as long as the gap lasts. A server deciding whether
    /// anything is new (say, whether a long-poll should wait) compares the
    /// arrivals of the selected events with the caller's last-seen head; it
    /// never filters what it sends by that head.
    pub fn version_vector(&self) -> VersionVector {
        self.vv.clone()
    }

    /// Events of every origin with `seq > have[origin]` (0 when absent),
    /// ordered by arrival: each origin's lowest seqs first, up to `limit`
    /// events, but never fewer than one per origin that has any, so every
    /// origin's next missing event is in each page and paging always makes
    /// progress (the result can exceed `limit` when more origins than that
    /// have events to send). A caller with a gap may be re-sent events it
    /// already holds; see [`Store::version_vector`].
    pub fn events_after(&self, have: &VersionVector, limit: usize) -> Result<Vec<Event>> {
        let applied = self.events_after_applied(have, limit)?;
        Ok(applied.into_iter().map(|a| a.event).collect())
    }

    /// [`Store::events_after`] with each event's local arrival.
    pub fn events_after_applied(&self, have: &VersionVector, limit: usize) -> Result<Vec<Applied>> {
        // Pick by (origin, seq) on the index first; read the JSON only for
        // the events kept.
        let per_origin = sql_limit(limit.max(1));
        let mut select = self.conn.prepare_cached(
            "SELECT arrival FROM events WHERE origin = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
        )?;
        // (rank within its origin by seq, arrival)
        let mut picked: Vec<(usize, i64)> = Vec::new();
        let mut origins = 0;
        for origin in self.vv.keys() {
            let after = have.get(origin).copied().unwrap_or(0);
            let after = i64::try_from(after).unwrap_or(i64::MAX);
            let before = picked.len();
            let rows = select.query_map(params![origin, after, per_origin], |r| r.get(0))?;
            for (rank, arrival) in rows.enumerate() {
                picked.push((rank, arrival?));
            }
            origins += usize::from(picked.len() > before);
        }
        let keep = limit.max(origins);
        if picked.len() > keep {
            // Cut by rank, not arrival: an origin's lowest missing seq may
            // have arrived here after many later ones.
            picked.sort_unstable_by_key(|&(rank, arrival)| (rank, arrival));
            picked.truncate(keep);
        }
        picked.sort_unstable_by_key(|&(_, arrival)| arrival);

        let mut fetch = self
            .conn
            .prepare_cached("SELECT json FROM events WHERE arrival = ?1")?;
        picked
            .into_iter()
            .map(|(_, arrival)| {
                let json: String = fetch.query_row([arrival], |r| r.get(0))?;
                Ok(Applied {
                    event: parse_event(&json)?,
                    arrival,
                })
            })
            .collect()
    }

    /// The highest arrival stored (0 when empty).
    pub fn arrival_head(&self) -> i64 {
        self.head
    }

    /// Events that arrived after `after`, the oldest `limit`.
    pub fn events_since_arrival(&self, after: i64, limit: usize) -> Result<Vec<Applied>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT arrival, json FROM events WHERE arrival > ?1 ORDER BY arrival LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![after, sql_limit(limit)], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut applied = Vec::new();
        for row in rows {
            let (arrival, json) = row?;
            applied.push(Applied {
                event: parse_event(&json)?,
                arrival,
            });
        }
        Ok(applied)
    }

    /// Visible channels, `general` first, then by name.
    pub fn channels(&self) -> Result<Vec<ChannelRow>> {
        let sql = format!("{CHANNEL_SELECT} ORDER BY v.name <> '{GENERAL}', v.name");
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map([], channel_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The channel `name` if it is visible.
    pub fn channel(&self, name: &str) -> Result<Option<ChannelRow>> {
        let sql = format!("{CHANNEL_SELECT} WHERE v.name = ?1");
        let mut stmt = self.conn.prepare_cached(&sql)?;
        Ok(stmt.query_row([name], channel_row).optional()?)
    }

    /// The newest `q.limit` shown posts matching `q`, oldest first, in
    /// display order `(ts, id)`.
    pub fn posts(&self, q: &PostQuery) -> Result<Vec<PostRow>> {
        let mut sql = format!("SELECT {POST_COLUMNS} FROM {SHOWN_POSTS}");
        let mut args: Vec<Value> = Vec::new();
        if let Some(channel) = &q.channel {
            sql.push_str(" AND p.channel = ?");
            args.push(channel.clone().into());
        }
        if let Some((ts, id)) = &q.before {
            sql.push_str(" AND (p.ts, p.id) < (?, ?)");
            args.push((*ts).into());
            args.push(id.clone().into());
        }
        if let Some(tag) = &q.tag {
            sql.push_str(" AND EXISTS (SELECT 1 FROM json_each(p.tags_json) WHERE value = ?)");
            args.push(tag.clone().into());
        }
        if let Some(handle) = &q.mentions_of {
            sql.push_str(concat!(" AND ", mentions_sql!("?")));
            args.push(handle.clone().into());
        }
        if q.announcements_only {
            sql.push_str(" AND p.author_kind = 'human'");
        }
        sql.push_str(" ORDER BY p.ts DESC, p.id DESC LIMIT ?");
        args.push(sql_limit(q.limit).into());

        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params_from_iter(args), post_row)?;
        let mut posts = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        posts.reverse();
        Ok(posts)
    }

    /// The shown post whose id is `id_or_suffix` or ends with it
    /// (case-insensitive, at least 6 characters). Several matches are
    /// [`Error::Ambiguous`], none [`Error::NotFound`].
    pub fn post(&self, id_or_suffix: &str) -> Result<PostRow> {
        let needle = id_or_suffix.trim().to_ascii_uppercase();
        let len = needle.chars().count();
        if len < MIN_ID_SUFFIX {
            return Err(Error::Usage(format!(
                "post id {id_or_suffix:?} is too short: give the full id or at least its last {MIN_ID_SUFFIX} characters"
            )));
        }
        let sql = format!(
            "SELECT {POST_COLUMNS} FROM {SHOWN_POSTS} AND upper(substr(p.id, -?1)) = ?2 ORDER BY p.id"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![sql_limit(len), needle], post_row)?;
        let mut found = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        match found.len() {
            0 => Err(Error::NotFound(format!("no post with id {id_or_suffix}"))),
            1 => Ok(found.remove(0)),
            _ => Err(Error::Ambiguous(found.into_iter().map(|p| p.id).collect())),
        }
    }

    /// The shown post whose id is exactly `id`, if there is one: unlike
    /// [`Store::post`] it takes no suffix, so it works for any id an event
    /// carries and is a primary-key lookup.
    pub fn shown_post(&self, id: &str) -> Result<Option<PostRow>> {
        let sql = format!("SELECT {POST_COLUMNS} FROM {SHOWN_POSTS} AND p.id = ?1");
        let mut stmt = self.conn.prepare_cached(&sql)?;
        Ok(stmt.query_row([id], post_row).optional()?)
    }

    /// Handles seen within the last 2 hours, most recently seen first.
    pub fn who(&self, now_ms: i64) -> Result<Vec<PresenceRow>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT p.handle, p.kind, p.client, p.node, s.status, p.last_seen_ms
             FROM presence AS p LEFT JOIN presence_status AS s ON s.handle = p.handle
             WHERE p.last_seen_ms >= ?1
             ORDER BY p.last_seen_ms DESC, p.handle",
        )?;
        let rows = stmt.query_map([now_ms.saturating_sub(WHO_WINDOW_MS)], |r| {
            Ok(PresenceRow {
                handle: r.get(0)?,
                kind: r.get(1)?,
                client: r.get(2)?,
                node: r.get(3)?,
                status: r.get(4)?,
                last_seen_ms: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Whether heartbeat `p` should be recorded as an `agent_seen` event: the
    /// handle has none in the last 60 s, or `p` sets a different status.
    pub fn presence_due(&self, p: &Presence, now_ms: i64) -> bool {
        let last = self
            .conn
            .query_row(
                "SELECT p.last_seen_ms, s.status
                 FROM presence AS p LEFT JOIN presence_status AS s ON s.handle = p.handle
                 WHERE p.handle = ?1",
                [&p.handle],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .optional();
        match last {
            Ok(Some((seen_ms, status))) => {
                now_ms.saturating_sub(seen_ms) >= PRESENCE_INTERVAL_MS
                    || (p.status.is_some() && p.status != status)
            }
            // Never seen, or the store could not tell: one extra heartbeat is harmless.
            Ok(None) | Err(_) => true,
        }
    }

    /// The newest URL each node announced, by node.
    pub fn announced_peers(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT node, url FROM peers_announced ORDER BY node")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The attachments of posts that are not deleted, each hash once, with
    /// the largest size any post claims for it: what sync makes sure this
    /// node holds the bytes of.
    pub fn attachments(&self) -> Result<Vec<(String, u64)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT att_sha, MAX(att_size) FROM posts
             WHERE att_sha IS NOT NULL AND deleted = 0 GROUP BY att_sha",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A new ULID for ts `now_ms`, greater than every id this store made before.
    fn next_id(&mut self, now_ms: i64) -> Ulid {
        let at = UNIX_EPOCH + Duration::from_millis(u64::try_from(now_ms).unwrap_or(0));
        match self.ids.generate_from_datetime(at) {
            Ok(id) => id,
            Err(overflow) => overflow.commit_overflow_increment(),
        }
    }
}

/// Whether any store can hold `ev` (see [`Store::apply`]).
fn storable(ev: &Event) -> Result<()> {
    if i64::try_from(ev.seq).is_err() {
        return Err(Error::Usage(format!(
            "event {}:{} has a seq too large to store",
            ev.origin, ev.seq
        )));
    }
    validate_origin(&ev.origin)
}

/// If the board was derived under another [`DERIVE_VERSION`] (or before
/// versions were recorded), derives it again from every event, in arrival
/// order and under today's rules, in one transaction: an event that breaks
/// [`validate_event`] still derives nothing, one an older build refused may
/// now derive, and each post keeps its event's arrival. Cursors and joins
/// are local state, not derived, and stay as they are.
fn rederive_if_stale(conn: &mut Connection) -> Result<()> {
    let current = DERIVE_VERSION.to_string();
    let stored: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'derive_version'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if stored.as_deref() == Some(current.as_str()) {
        return Ok(());
    }
    let tx = conn.transaction()?;
    tx.execute_batch(
        "DELETE FROM channel_marks; DELETE FROM posts; DELETE FROM tombstones;
         DELETE FROM presence; DELETE FROM presence_status; DELETE FROM peers_announced;",
    )?;
    let (mut derived, mut skipped) = (0u64, 0u64);
    {
        let mut stmt = tx.prepare("SELECT arrival, json FROM events ORDER BY arrival")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let arrival: i64 = row.get(0)?;
            let json: String = row.get(1)?;
            match parse_event(&json) {
                Ok(ev) if validate_event(&ev).is_ok() => {
                    derive(&tx, &ev, arrival)?;
                    derived += 1;
                }
                Ok(_) => skipped += 1,
                Err(e) => {
                    tracing::warn!(arrival, "cannot read a stored event: {e}");
                    skipped += 1;
                }
            }
        }
    }
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('derive_version', ?1)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [&current],
    )?;
    tx.commit()?;
    if derived + skipped > 0 {
        tracing::info!(
            from = stored.as_deref().unwrap_or("none"),
            to = %current,
            derived,
            skipped,
            "derived the board again from the event log"
        );
    }
    Ok(())
}

/// This database's current id, and every id it has written under.
fn load_db_ids(conn: &Connection) -> Result<(String, BTreeSet<String>)> {
    let db_id: String = conn.query_row("SELECT value FROM meta WHERE key = 'db_id'", [], |r| {
        r.get(0)
    })?;
    let mut stmt = conn.prepare("SELECT substr(key, ?1) FROM meta WHERE key LIKE ?2")?;
    let former = stmt.query_map(
        params![FORMER_DB_ID.len() + 1, format!("{FORMER_DB_ID}%")],
        |r| r.get::<_, String>(0),
    )?;
    let mut own = former.collect::<rusqlite::Result<BTreeSet<_>>>()?;
    own.insert(db_id.clone());
    Ok((db_id, own))
}

/// Updates the derived tables for a newly stored event.
fn derive(tx: &Connection, ev: &Event, arrival: i64) -> Result<()> {
    match &ev.kind {
        EventKind::ChannelCreated { name, topic } => {
            mark_channel(tx, name, "created", ev, topic.as_deref())?;
            mark_channel(tx, name, "alive", ev, None)?;
        }
        EventKind::ChannelDeleted { name } => mark_channel(tx, name, "deleted", ev, None)?,
        EventKind::PostCreated(post) => {
            // A post keeps (or brings back) its channel, even if deleted since.
            mark_channel(tx, &post.channel, "alive", ev, None)?;
            insert_post(tx, post, ev.ts, arrival)?;
        }
        EventKind::PostDeleted { post_id, .. } => {
            tx.execute(
                "INSERT INTO tombstones (post_id) VALUES (?1) ON CONFLICT DO NOTHING",
                [post_id],
            )?;
            tx.execute("UPDATE posts SET deleted = 1 WHERE id = ?1", [post_id])?;
        }
        EventKind::AgentSeen(p) => {
            tx.execute(
                "INSERT INTO presence (handle, kind, client, node, last_seen_ms, origin, seq)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (handle) DO UPDATE SET
                     kind = excluded.kind, client = excluded.client, node = excluded.node,
                     last_seen_ms = excluded.last_seen_ms,
                     origin = excluded.origin, seq = excluded.seq
                 WHERE (excluded.last_seen_ms, excluded.origin, excluded.seq)
                     > (presence.last_seen_ms, presence.origin, presence.seq)",
                params![p.handle, p.kind, p.client, p.node, ev.ts, ev.origin, ev.seq],
            )?;
            if let Some(status) = &p.status {
                tx.execute(
                    "INSERT INTO presence_status (handle, status, ts, origin, seq)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (handle) DO UPDATE SET
                         status = excluded.status,
                         ts = excluded.ts, origin = excluded.origin, seq = excluded.seq
                     WHERE (excluded.ts, excluded.origin, excluded.seq)
                         > (presence_status.ts, presence_status.origin, presence_status.seq)",
                    params![p.handle, status, ev.ts, ev.origin, ev.seq],
                )?;
            }
        }
        // Never valid, so never derived.
        EventKind::Unknown(_) => {}
        EventKind::PeerAnnounced { node, url } => {
            tx.execute(
                "INSERT INTO peers_announced (node, url, ts, origin, seq)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (node) DO UPDATE SET
                     url = excluded.url,
                     ts = excluded.ts, origin = excluded.origin, seq = excluded.seq
                 WHERE (excluded.ts, excluded.origin, excluded.seq)
                     > (peers_announced.ts, peers_announced.origin, peers_announced.seq)",
                params![node, url, ev.ts, ev.origin, ev.seq],
            )?;
        }
    }
    Ok(())
}

/// Records `ev` as the channel's `kind` mark if it is newer than the current one.
fn mark_channel(
    tx: &Connection,
    name: &str,
    kind: &str,
    ev: &Event,
    topic: Option<&str>,
) -> Result<()> {
    tx.execute(
        "INSERT INTO channel_marks (name, kind, ts, origin, seq, topic)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (name, kind) DO UPDATE SET
             ts = excluded.ts, origin = excluded.origin, seq = excluded.seq,
             topic = excluded.topic
         WHERE (excluded.ts, excluded.origin, excluded.seq)
             > (channel_marks.ts, channel_marks.origin, channel_marks.seq)",
        params![name, kind, ev.ts, ev.origin, ev.seq, topic],
    )?;
    Ok(())
}

/// Inserts a post, already deleted if its tombstone arrived first.
fn insert_post(tx: &Connection, post: &NewPost, ts: i64, arrival: i64) -> Result<()> {
    let tags = serde_json::to_string(&post.tags).map_err(anyhow::Error::from)?;
    let mentions = serde_json::to_string(&post.mentions).map_err(anyhow::Error::from)?;
    let att = post.attachment.as_ref();
    tx.execute(
        "INSERT INTO posts (id, channel, ts, author, author_kind, client, node, body,
                            tags_json, mentions_json, att_sha, att_name, att_size, arrival, deleted)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                 EXISTS (SELECT 1 FROM tombstones WHERE post_id = ?1))
         ON CONFLICT (id) DO NOTHING",
        params![
            post.post_id,
            post.channel,
            ts,
            post.author,
            post.author_kind,
            post.client,
            post.node,
            post.body,
            tags,
            mentions,
            att.map(|a| &a.sha256),
            att.map(|a| &a.name),
            att.map(|a| a.size),
            arrival,
        ],
    )?;
    Ok(())
}

impl PostRow {
    /// The row [`insert_post`] makes for `applied`, built from the event
    /// instead of read back, so a caller that just stored a post can show it
    /// without another query. `None` if the event is not a `post_created`.
    /// Kept beside `insert_post` so the two list the same fields.
    pub fn from_applied(applied: &Applied) -> Option<PostRow> {
        let EventKind::PostCreated(p) = &applied.event.kind else {
            return None;
        };
        Some(PostRow {
            id: p.post_id.clone(),
            channel: p.channel.clone(),
            ts: applied.event.ts,
            author: p.author.clone(),
            author_kind: p.author_kind,
            client: p.client.clone(),
            node: p.node.clone(),
            body: p.body.clone(),
            tags: p.tags.clone(),
            mentions: p.mentions.clone(),
            attachment: p.attachment.clone(),
            arrival: applied.arrival,
        })
    }
}

/// Starting from `have`, the highest seq of `origin` with no gap below it.
fn gapless_from(tx: &Connection, origin: &str, mut have: u64) -> Result<u64> {
    let mut stmt =
        tx.prepare_cached("SELECT EXISTS (SELECT 1 FROM events WHERE origin = ?1 AND seq = ?2)")?;
    while stmt.query_row(params![origin, have + 1], |r| r.get::<_, bool>(0))? {
        have += 1;
    }
    Ok(have)
}

/// The version vector of everything stored, as [`Store::version_vector`] defines it.
fn load_version_vector(conn: &Connection) -> Result<VersionVector> {
    let mut vv = VersionVector::new();
    let mut stmt = conn.prepare("SELECT origin, seq FROM events ORDER BY origin, seq")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let have = vv.entry(row.get::<_, String>(0)?).or_insert(0);
        if row.get::<_, u64>(1)? == *have + 1 {
            *have += 1;
        }
    }
    Ok(vv)
}

fn channel_row(r: &Row) -> rusqlite::Result<ChannelRow> {
    Ok(ChannelRow {
        name: r.get(0)?,
        topic: r.get(1)?,
        last_post_ts: r.get(2)?,
    })
}

/// Reads a [`PostRow`] from a row selected with [`POST_COLUMNS`].
fn post_row(r: &Row) -> rusqlite::Result<PostRow> {
    let attachment = match r.get::<_, Option<String>>(10)? {
        Some(sha256) => Some(Attachment {
            sha256,
            name: r.get(11)?,
            size: r.get(12)?,
        }),
        None => None,
    };
    Ok(PostRow {
        id: r.get(0)?,
        channel: r.get(1)?,
        ts: r.get(2)?,
        author: r.get(3)?,
        author_kind: r.get(4)?,
        client: r.get(5)?,
        node: r.get(6)?,
        body: r.get(7)?,
        tags: json_column(r, 8)?,
        mentions: json_column(r, 9)?,
        attachment,
        arrival: r.get(13)?,
    })
}

fn json_column<T: DeserializeOwned>(r: &Row, idx: usize) -> rusqlite::Result<T> {
    let text: String = r.get(idx)?;
    serde_json::from_str(&text)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(idx, Type::Text, Box::new(e)))
}

fn parse_event(json: &str) -> Result<Event> {
    Ok(serde_json::from_str(json).map_err(anyhow::Error::from)?)
}

/// `limit` as an SQL `LIMIT` (which is an i64).
fn sql_limit(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

/// 8 random lowercase base32 characters (the random tail of a ULID).
fn new_db_id() -> String {
    let ulid = Ulid::generate().to_string();
    ulid[ulid.len() - 8..].to_ascii_lowercase()
}

fn wall_clock_ms() -> i64 {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX)
}

impl ToSql for AuthorKind {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(match self {
            AuthorKind::Human => "human",
            AuthorKind::Agent => "agent",
        }))
    }
}

impl FromSql for AuthorKind {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "human" => Ok(AuthorKind::Human),
            "agent" => Ok(AuthorKind::Agent),
            other => Err(FromSqlError::Other(
                format!("unknown author kind {other:?}").into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A commit that a peer may already hold must survive a power loss or a
    /// killed VM: in WAL mode, `NORMAL` can lose the last commits, and a node
    /// that lost them would reuse their seqs for different events.
    #[test]
    fn open_syncs_every_commit_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("beacon.db"), "pc").unwrap();
        let mode: i64 = store
            .conn
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, 2, "synchronous should be FULL (2)");
    }

    /// The image is `FROM scratch`: it has no /tmp, and a container may run
    /// with a read-only root. A query that needs a temporary table or a big
    /// sort (`read --unread --limit 500`) then fails with SQLite's "disk I/O
    /// error 6410" unless temporary data stays in memory.
    #[test]
    fn open_keeps_temporary_data_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("beacon.db"), "pc").unwrap();
        let mode: i64 = store
            .conn
            .query_row("PRAGMA temp_store", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, 2, "temp_store should be MEMORY (2)");
    }
}
