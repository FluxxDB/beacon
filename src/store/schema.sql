-- Beacon's SQLite schema. `events` is the log every node keeps a full copy of;
-- every other table except the local-only `cursors` and `joins` (at the end) is
-- derived from it by `Store::apply`, so it holds the same rows on every node
-- that has the same events, whatever order they arrived in.
--
-- "Newest" always means the greatest (ts, origin, seq): the spec's (ts, origin),
-- with the origin's own seq breaking a tie between two of its events in one ms.

-- Facts about this database. `db_id` (8 random lowercase base32 characters,
-- made when the database is created) makes this node's origin `{node}~{db_id}`.
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;

-- Every event, numbered by local arrival (`arrival`, never reused). Read
-- cursors follow arrival, not `ts`, so a late post with an old ts is still new.
CREATE TABLE IF NOT EXISTS events (
    arrival INTEGER PRIMARY KEY AUTOINCREMENT,
    origin  TEXT    NOT NULL,
    seq     INTEGER NOT NULL,
    id      TEXT    NOT NULL,
    ts      INTEGER NOT NULL,
    json    TEXT    NOT NULL,
    UNIQUE (origin, seq)
);

-- The newest mark of each kind per channel name:
--   created  the newest channel_created (its topic is the channel's topic)
--   alive    the newest channel_created or post_created in the channel
--   deleted  the newest channel_deleted
CREATE TABLE IF NOT EXISTS channel_marks (
    name   TEXT    NOT NULL,
    kind   TEXT    NOT NULL CHECK (kind IN ('created', 'alive', 'deleted')),
    ts     INTEGER NOT NULL,
    origin TEXT    NOT NULL,
    seq    INTEGER NOT NULL,
    topic  TEXT,
    PRIMARY KEY (name, kind)
) WITHOUT ROWID;

-- A channel is visible iff its alive mark is newer than its deleted mark.
CREATE VIEW IF NOT EXISTS visible_channels AS
SELECT alive.name AS name, created.topic AS topic
FROM channel_marks AS alive
LEFT JOIN channel_marks AS deleted
    ON deleted.name = alive.name AND deleted.kind = 'deleted'
LEFT JOIN channel_marks AS created
    ON created.name = alive.name AND created.kind = 'created'
WHERE alive.kind = 'alive'
  AND (deleted.name IS NULL
       OR (alive.ts, alive.origin, alive.seq) > (deleted.ts, deleted.origin, deleted.seq));

-- Posts, immutable. `deleted` is set by a tombstone; a post is shown iff it is
-- not deleted and its channel is visible. `arrival` is its event's arrival.
CREATE TABLE IF NOT EXISTS posts (
    id            TEXT PRIMARY KEY,
    channel       TEXT    NOT NULL,
    ts            INTEGER NOT NULL,
    author        TEXT    NOT NULL,
    author_kind   TEXT    NOT NULL,
    client        TEXT,
    node          TEXT    NOT NULL,
    body          TEXT    NOT NULL,
    tags_json     TEXT    NOT NULL,
    mentions_json TEXT    NOT NULL,
    att_sha       TEXT,
    att_name      TEXT,
    att_size      INTEGER,
    arrival       INTEGER NOT NULL,
    deleted       INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS posts_by_channel ON posts (channel, ts, id);
CREATE INDEX IF NOT EXISTS posts_by_time ON posts (ts, id);
-- What is new to a handle: per channel, the posts past its cursor.
CREATE INDEX IF NOT EXISTS posts_by_arrival ON posts (channel, arrival);

-- Every deleted post id, so a delete that arrives before its post still wins.
CREATE TABLE IF NOT EXISTS tombstones (
    post_id TEXT PRIMARY KEY
) WITHOUT ROWID;

-- The newest agent_seen per handle.
CREATE TABLE IF NOT EXISTS presence (
    handle       TEXT PRIMARY KEY,
    kind         TEXT    NOT NULL,
    client       TEXT    NOT NULL,
    node         TEXT    NOT NULL,
    last_seen_ms INTEGER NOT NULL,
    origin       TEXT    NOT NULL,
    seq          INTEGER NOT NULL
);

-- The newest agent_seen per handle that carried a status. A heartbeat without
-- one keeps the status set by `who --here`.
CREATE TABLE IF NOT EXISTS presence_status (
    handle TEXT PRIMARY KEY,
    status TEXT    NOT NULL,
    ts     INTEGER NOT NULL,
    origin TEXT    NOT NULL,
    seq    INTEGER NOT NULL
);

-- The newest peer_announced per node.
CREATE TABLE IF NOT EXISTS peers_announced (
    node   TEXT PRIMARY KEY,
    url    TEXT    NOT NULL,
    ts     INTEGER NOT NULL,
    origin TEXT    NOT NULL,
    seq    INTEGER NOT NULL
);

-- Local-only state: never events, never synced, never derived. What each handle
-- has read and joined on this node. Handles compare case-insensitively, like
-- authors and mentions do, so `PC-Configs` and `pc-configs` share one row.

-- Per handle and channel, the newest `arrival` the handle has read. Unread is
-- every relevant post with a greater arrival; a channel with no row has
-- cursor 0. Arrival, not ts, so a late post with an old ts is still unread.
CREATE TABLE IF NOT EXISTS cursors (
    handle  TEXT    NOT NULL COLLATE NOCASE,
    channel TEXT    NOT NULL,
    arrival INTEGER NOT NULL,
    PRIMARY KEY (handle, channel)
) WITHOUT ROWID;

-- The channels a handle joined: they make every post there relevant to it.
CREATE TABLE IF NOT EXISTS joins (
    handle  TEXT NOT NULL COLLATE NOCASE,
    channel TEXT NOT NULL,
    PRIMARY KEY (handle, channel)
) WITHOUT ROWID;
