//! What each handle has read and joined on this node, and from those, what is
//! new for it.
//!
//! Read cursors follow local *arrival* order, never `ts`: a post written on a
//! device that was offline and synced hours later has an old `ts` but a fresh
//! arrival, so it is still unread. Cursors and joins are local-only state in
//! their own tables (see `schema.sql`); they are never events and never sync.
//! A handle is the same handle whatever its case, as it is for authors and
//! mentions.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{params, Connection};

use super::{post_row, sql_limit, PostRow, Result, Store, POST_COLUMNS, SHOWN_POSTS};
use crate::model::{mentions_handle, normalize_channel};

/// Conditions after [`SHOWN_POSTS`] for the posts that are new to handle `?1`:
/// not its own (case-insensitive) and arrived after its cursor for the channel.
const NEW_FROM_OTHERS: &str = "
    AND lower(p.author) <> lower(?1)
    AND p.arrival > COALESCE(
        (SELECT c.arrival FROM cursors AS c WHERE c.handle = ?1 AND c.channel = p.channel), 0)";

/// Further conditions for the ones relevant to handle `?1`: the SQL twin of the
/// rule in [`Store::is_relevant`] (its first clause, not own, is in
/// [`NEW_FROM_OTHERS`]); a test keeps the two agreeing.
const RELEVANT: &str = concat!(
    "
    AND (p.author_kind = 'human'
         OR ",
    mentions_sql!("?1"),
    "
         OR p.channel IN (SELECT j.channel FROM joins AS j WHERE j.handle = ?1))"
);

/// A page of what is new for a handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unread {
    /// Announcements first, then the rest; each group in display order `(ts, id)`.
    pub posts: Vec<PostRow>,
    /// How many more unread posts there are beyond this page.
    pub remaining: u32,
}

impl Store {
    /// The channels `handle` joined, by name.
    pub fn joins(&self, handle: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT channel FROM joins WHERE handle = ?1 ORDER BY channel")?;
        let rows = stmt.query_map([handle], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Replaces the channels `handle` joined with `channels` (a set; names are
    /// normalized like any channel name, and may not exist yet). A name that
    /// is not a channel name is [`crate::error::Error::Usage`] and changes
    /// nothing.
    pub fn set_joins(&mut self, handle: &str, channels: &[String]) -> Result<()> {
        let channels = channels
            .iter()
            .map(|c| normalize_channel(c))
            .collect::<Result<BTreeSet<_>>>()?;
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM joins WHERE handle = ?1", [handle])?;
        {
            let mut insert =
                tx.prepare_cached("INSERT INTO joins (handle, channel) VALUES (?1, ?2)")?;
            for channel in &channels {
                insert.execute(params![handle, channel])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// For each channel `handle` has read, the arrival it has read up to.
    pub fn cursors(&self, handle: &str) -> Result<BTreeMap<String, i64>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT channel, arrival FROM cursors WHERE handle = ?1")?;
        let rows = stmt.query_map([handle], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Records that `handle` has read `channel` up to `arrival`. A cursor never
    /// moves backwards: an `arrival` at or below the current one changes
    /// nothing, and none is the same as 0. The channel name is normalized.
    pub fn set_cursor(&mut self, handle: &str, channel: &str, arrival: i64) -> Result<()> {
        let channel = normalize_channel(channel)?;
        upsert_cursor(&self.conn, handle, &channel, arrival)
    }

    /// Whether `p` is relevant to `handle`, who joined `joins`: not its own
    /// post (case-insensitive), and an announcement, a mention of it (or of
    /// `@all`), or a post in a channel it joined.
    pub fn is_relevant(&self, p: &PostRow, handle: &str, joins: &[String]) -> bool {
        !p.author.eq_ignore_ascii_case(handle)
            && (p.announcement()
                || mentions_handle(&p.mentions, handle)
                || joins.contains(&p.channel))
    }

    /// The first `limit` unread posts for `handle`: shown posts that are
    /// relevant to it and arrived after its cursor for their channel. The
    /// `limit` lowest arrivals are taken, so a page never skips a post that
    /// arrived earlier; those are then ordered announcements first, each
    /// group by `(ts, id)`. `remaining` counts the unread posts beyond them.
    /// Nothing is marked read; see [`Store::mark_read`].
    pub fn unread(&self, handle: &str, limit: usize) -> Result<Unread> {
        let sql = format!(
            "SELECT {POST_COLUMNS} FROM {SHOWN_POSTS}{NEW_FROM_OTHERS}{RELEVANT} \
             ORDER BY p.arrival LIMIT ?2"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![handle, sql_limit(limit)], post_row)?;
        let mut posts = rows.collect::<rusqlite::Result<Vec<_>>>()?;

        // A short page is everything there is.
        let remaining = if posts.len() < limit {
            0
        } else {
            let total = self.count_unread(handle)?;
            u32::try_from(total.saturating_sub(posts.len())).unwrap_or(u32::MAX)
        };
        posts.sort_by(|a, b| {
            b.announcement()
                .cmp(&a.announcement())
                .then(a.ts.cmp(&b.ts))
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(Unread { posts, remaining })
    }

    /// Marks `shown` as read by `handle`: in each channel, the cursor moves up
    /// to the newest arrival among the posts shown (never backwards). So pass
    /// what the reader was actually shown, such as a page from
    /// [`Store::unread`]: every post in those channels that arrived earlier
    /// counts as read too, shown or not.
    pub fn mark_read(&mut self, handle: &str, shown: &[PostRow]) -> Result<()> {
        let mut newest: BTreeMap<&str, i64> = BTreeMap::new();
        for p in shown {
            let at = newest.entry(&p.channel).or_insert(p.arrival);
            *at = (*at).max(p.arrival);
        }
        let tx = self.conn.transaction()?;
        for (channel, arrival) in newest {
            upsert_cursor(&tx, handle, channel, arrival)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// For the sidebar badges: how many new posts `handle` has in each
    /// channel, relevant or not. That is every shown post not by `handle`
    /// (case-insensitive) that arrived after its cursor for the channel, not
    /// one page and not only what [`Store::unread`] would list: an agent's
    /// post in a channel `handle` has not joined counts here. Channels with
    /// none are left out.
    pub fn unread_counts(&self, handle: &str) -> Result<BTreeMap<String, u32>> {
        let sql = format!(
            "SELECT p.channel, COUNT(*) FROM {SHOWN_POSTS}{NEW_FROM_OTHERS} GROUP BY p.channel"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map([handle], |r| Ok((r.get(0)?, r.get::<_, i64>(1)?)))?;
        let mut counts = BTreeMap::new();
        for row in rows {
            let (channel, n) = row?;
            counts.insert(channel, u32::try_from(n).unwrap_or(u32::MAX));
        }
        Ok(counts)
    }

    /// For the Announcements badge: how many announcements (human-authored
    /// shown posts) not by `handle` arrived after its cursor for their
    /// channel, across all channels.
    pub fn unread_announcements(&self, handle: &str) -> Result<u32> {
        let sql = format!(
            "SELECT COUNT(*) FROM {SHOWN_POSTS}{NEW_FROM_OTHERS} AND p.author_kind = 'human'"
        );
        let n: i64 = self
            .conn
            .prepare_cached(&sql)?
            .query_row([handle], |r| r.get(0))?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    fn count_unread(&self, handle: &str) -> Result<usize> {
        let sql = format!("SELECT COUNT(*) FROM {SHOWN_POSTS}{NEW_FROM_OTHERS}{RELEVANT}");
        let n: i64 = self
            .conn
            .prepare_cached(&sql)?
            .query_row([handle], |r| r.get(0))?;
        Ok(usize::try_from(n).unwrap_or(usize::MAX))
    }
}

/// Raises the cursor to `arrival` (at least 0) if that is higher than it is.
fn upsert_cursor(conn: &Connection, handle: &str, channel: &str, arrival: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO cursors (handle, channel, arrival) VALUES (?1, ?2, ?3)
         ON CONFLICT (handle, channel) DO UPDATE SET arrival = excluded.arrival
         WHERE excluded.arrival > cursors.arrival",
        params![handle, channel, arrival.max(0)],
    )?;
    Ok(())
}
