//! Live arrivals: what reaches other devices and waiting agents the moment it
//! is stored.
//!
//! * `GET /api/events` is the event feed peers pull from, a long poll.
//! * `POST /api/events` is where peers push.
//! * `GET /api/stream` is the server-sent event feed of the TUI and `watch`.
//! * `GET /api/wait` holds an agent's request open until it has something to read.
//!
//! Each of them listens to [`AppState::arrivals`] and re-reads the store when
//! it rings. The channel is only a doorbell: a message says "something was
//! stored", never "this is what", and a receiver that falls behind (the channel
//! holds 1024 messages) loses nothing, because it looks at the store, which
//! has everything, from where it left off. Every long-lived handler also
//! ends when the node is asked to stop.

use std::collections::{HashSet, VecDeque};
use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::stream::{self, Stream};
use serde::Deserialize;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::broadcast::Receiver;
use tokio::time::{Instant, MissedTickBehavior};

use super::routes::{flag, page_limit, posts_json};
use super::{now_ms, ApiFailure, ApiJson, ApiQuery, ApiResult, AppState};
use crate::api::*;
use crate::blobs::Blobs;
use crate::error::Error;
use crate::model::{parse_vv, validate_event, validate_handle, EventKind, Presence, VersionVector};
use crate::store::{Applied, Store};

/// The header that names the node that answered `/api/events`.
const NODE_HEADER: HeaderName = HeaderName::from_static("x-beacon-node");
const INSTANCE_HEADER: HeaderName = HeaderName::from_static(crate::sync::INSTANCE_HEADER);
/// Events per page of `GET /api/events` when the request does not say, and
/// the most it may ask for.
const EVENTS_PAGE: usize = 500;
/// The longest a peer's pull is held open.
const EVENTS_WAIT_MAX: Duration = Duration::from_secs(25);
/// Largest body of `POST /api/events`: a batch of 500 events is up to 64 KiB
/// of text each.
const PUSH_BODY_MAX: usize = 64 * 1024 * 1024;
/// How long `GET /api/wait` waits when the request does not say, and the
/// longest it may be asked to.
const WAIT_DEFAULT: Duration = Duration::from_secs(60);
const WAIT_MAX: Duration = Duration::from_secs(3600);
/// How often a stream looks at the peer list for a reachability change.
const PEERS_POLL: Duration = Duration::from_secs(1);
/// Events a stream reads from the store at a time.
const CATCH_UP_PAGE: usize = 256;
/// How long a stream waits before it tries again after the store failed it.
const RETRY_PAUSE: Duration = Duration::from_secs(1);
/// A stream that fails this many times in a row to read the store ends, so
/// its client reconnects instead of waiting on a stream that is dead.
const MAX_STORE_FAILURES: u32 = 10;

pub fn router(state: &AppState) -> Router<AppState> {
    let events = Router::new()
        .route(
            "/api/events",
            get(pull_events)
                .post(push_events)
                // The route answers other methods itself, so the layer below
                // names the node on that answer too.
                .fallback(super::method_not_allowed)
                .layer(DefaultBodyLimit::max(PUSH_BODY_MAX)),
        )
        .layer(middleware::from_fn_with_state(state.clone(), name_the_node));
    Router::new()
        .route("/api/stream", get(stream_events))
        .route("/api/wait", get(wait_for_unread))
        .merge(events)
}

/// Says which node answered, and which run of it, on every `/api/events`
/// response: peers learn each other's names from it, including their own
/// address when they dial it, and tell that from another device that was
/// given the same name (a different run).
async fn name_the_node(State(app): State<AppState>, req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    if let Ok(node) = HeaderValue::from_str(&app.cfg.node) {
        response.headers_mut().insert(NODE_HEADER, node);
    }
    if let Ok(run) = HeaderValue::from_str(app.sync.instance()) {
        response.headers_mut().insert(INSTANCE_HEADER, run);
    }
    response
}

// -------------------------------------------------------------- the doorbell

/// Waits until the bell rings for an event `matters` to the caller, or the
/// receiver lagged (which says nothing about what was missed, so it counts).
/// Returns `false` if the channel is closed. A burst of rings is one: the
/// caller reads the store, which has all of it.
async fn ring(bell: &mut Receiver<Applied>, matters: impl Fn(&Applied) -> bool) -> bool {
    loop {
        match bell.recv().await {
            Ok(applied) if !matters(&applied) => continue,
            Ok(_) | Err(RecvError::Lagged(_)) => break,
            Err(RecvError::Closed) => return false,
        }
    }
    while !matches!(
        bell.try_recv(),
        Err(TryRecvError::Empty | TryRecvError::Closed)
    ) {}
    true
}

// ------------------------------------------------------ GET /api/events (pull)

#[derive(Deserialize)]
struct PullQuery {
    /// The caller's version vector: `origin:seq,origin:seq`.
    after: Option<String>,
    /// Seconds to hold the request open when nothing is new (at most 25).
    wait: Option<u64>,
    /// How many events at most (default and maximum 500).
    limit: Option<u32>,
    /// The `head` of the answer to the caller's last pull that applied
    /// nothing new (default 0). Not for paging through a backlog: see
    /// [`pull_events`].
    since: Option<i64>,
}

/// What a pull selected, read under one lock.
struct Page {
    events: Vec<Applied>,
    have: VersionVector,
    head: i64,
}

async fn select_page(app: &AppState, after: &VersionVector, limit: usize) -> Result<Page, Error> {
    let after = after.clone();
    app.with_store(move |s| {
        Ok(Page {
            events: s.events_after_applied(&after, limit)?,
            have: s.version_vector(),
            head: s.arrival_head(),
        })
    })
    .await
}

/// The feed a peer pulls: the events it lacks, from every origin.
///
/// The selection never depends on `since`; it is the store's, and a caller
/// holding a gap in its version vector is re-sent what lies above the gap
/// even though it has it. So a non-empty selection is not news. The request
/// waits, up to `wait`, unless some selected event arrived here after
/// `since`, and wakes when anything is stored. Without a `since` everything
/// stored is news, and so is everything for a `since` beyond our head.
///
/// `head` in the answer is the node's head *now*, so how a puller uses the
/// pair matters (R22): while its pages keep advancing its version vector (at
/// least one event newly applied) it follows `more` at once with `wait=0`;
/// when a page applied nothing new it long-polls with `since=<that page's
/// head>`. Passing `since=head` after a cut page would hold the request for
/// the whole `wait` with events still owed, because all of them have already
/// arrived by `head`.
async fn pull_events(
    State(app): State<AppState>,
    ApiQuery(q): ApiQuery<PullQuery>,
) -> ApiResult<EventsResponse> {
    let after = match q.after.as_deref() {
        Some(text) => parse_vv(text)?,
        None => VersionVector::new(),
    };
    let limit = match q.limit {
        None => EVENTS_PAGE,
        Some(0) => return Err(Error::Usage("limit must be at least 1".into()).into()),
        Some(n) => (n as usize).min(EVENTS_PAGE),
    };
    let wait = events_wait(q.wait);
    let since = q.since.unwrap_or(0);
    let deadline = Instant::now() + wait;
    // Listen before looking, so nothing stored in between goes unheard.
    let mut bell = (!wait.is_zero()).then(|| app.arrivals.subscribe());

    loop {
        let page = select_page(&app, &after, limit).await?;
        // A `since` past our head was given by some other database (this one
        // was wiped and started over): it says nothing about what we sent.
        let seen = if since > page.head { 0 } else { since };
        let news = page.events.iter().any(|a| a.arrival > seen);
        let rang = match bell.as_mut() {
            Some(bell) if !news => {
                tokio::select! {
                    rang = ring(bell, |_| true) => rang,
                    () = tokio::time::sleep_until(deadline) => false,
                    () = app.shutdown.triggered() => false,
                }
            }
            _ => false,
        };
        if !rang {
            return Ok(Json(EventsResponse {
                v: V,
                more: page.events.len() >= limit,
                events: page.events.into_iter().map(|a| a.event).collect(),
                have: page.have,
                head: page.head,
            }));
        }
    }
}

/// How long a pull may wait: what it asked for, up to 25 s.
fn events_wait(asked: Option<u64>) -> Duration {
    Duration::from_secs(asked.unwrap_or(0)).min(EVENTS_WAIT_MAX)
}

// ----------------------------------------------------- POST /api/events (push)

/// Events a peer pushes, stored in one transaction by
/// [`AppState::ingest_all`], which announces the valid ones and wakes sync
/// (so a post relays onward). An event of a kind this build does not know is
/// stored and relayed, never derived. One that cannot be stored at all (a
/// seq SQLite cannot hold, an origin that is not `{node}~{db_id}`) fails the
/// request with 400; the others in the batch stay applied, and sending the
/// batch again is harmless. Sync is told the sender holds them, so they are
/// not pushed straight back to it.
async fn push_events(
    State(app): State<AppState>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<PushRequest>,
) -> ApiResult<PushResponse> {
    if let Some(from) = super::sender(&headers) {
        app.sync.received_from(&from, &req.events);
    }
    let mut ingested = app.ingest_all(req.events).await?;
    if !ingested.refused.is_empty() {
        let (_, why) = ingested.refused.swap_remove(0);
        return Err(why.into());
    }
    let applied = u64::try_from(ingested.applied.len()).unwrap_or(u64::MAX);
    let have = app.with_store(|s| Ok(s.version_vector())).await?;
    Ok(Json(PushResponse {
        v: V,
        applied,
        have,
    }))
}

// ------------------------------------------------------------- GET /api/wait

#[derive(Deserialize)]
struct WaitQuery {
    handle: Option<String>,
    /// Seconds to wait (default 60, at most 3600).
    timeout: Option<u64>,
    limit: Option<u32>,
}

/// Holds the request until `handle` has relevant unread posts, then answers
/// with them (200, cursors untouched), or answers 204 when `timeout` runs out.
/// It looks first, so a post that arrived between the caller's last read and
/// this request is never missed.
async fn wait_for_unread(
    State(app): State<AppState>,
    ApiQuery(q): ApiQuery<WaitQuery>,
) -> Result<Response, ApiFailure> {
    let handle = q
        .handle
        .as_deref()
        .ok_or_else(|| Error::Usage("handle is required".into()))?;
    let handle = validate_handle(handle)?;
    let limit = page_limit(q.limit)?;
    let deadline = Instant::now() + wait_timeout(q.timeout);
    let mut bell = app.arrivals.subscribe();

    loop {
        let who = handle.clone();
        let page = app.with_store(move |s| s.unread(&who, limit)).await?;
        if !page.posts.is_empty() {
            let body = posts_json(&app, &page.posts, Some(page.remaining));
            return Ok(Json(body).into_response());
        }
        let rang = tokio::select! {
            rang = ring(&mut bell, may_add_unread) => rang,
            () = tokio::time::sleep_until(deadline) => false,
            () = app.shutdown.triggered() => false,
        };
        if !rang {
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
    }
}

/// How long a wait may last: what it asked for, up to an hour.
fn wait_timeout(asked: Option<u64>) -> Duration {
    asked
        .map_or(WAIT_DEFAULT, Duration::from_secs)
        .min(WAIT_MAX)
}

/// Whether an arrival can put a post in someone's unread list: a new post, or
/// a channel coming back with its posts. (Heartbeats, deletes and peer
/// announcements cannot, and they are most of the traffic.)
fn may_add_unread(applied: &Applied) -> bool {
    matches!(
        applied.event.kind,
        EventKind::PostCreated(_) | EventKind::ChannelCreated { .. }
    )
}

// ----------------------------------------------------------- GET /api/stream

#[derive(Deserialize)]
struct StreamQuery {
    handle: Option<String>,
    relevant: Option<String>,
}

/// The live feed: a `post`, `post_deleted`, `channel` or `presence` event for
/// everything stored from now on that changes what is shown, and `peers` when a
/// peer's reachability changes. With `relevant=1` it carries `post` events
/// and nothing else, and only the posts that matter to `handle` (see
/// `Store::is_relevant`, checked against its joins when the event is sent):
/// that is what `wait` and `watch` listen to.
async fn stream_events(
    State(app): State<AppState>,
    ApiQuery(q): ApiQuery<StreamQuery>,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ApiFailure> {
    let handle = q.handle.as_deref().map(validate_handle).transpose()?;
    let relevant_to = if flag("relevant", q.relevant.as_deref())? {
        let handle = handle.ok_or_else(|| Error::Usage("relevant=1 needs a handle".into()))?;
        Some(handle)
    } else {
        None
    };

    // Listen, then note where the store stands: whatever is stored after
    // that rings the bell, and what was stored before is not ours to send.
    let bell = app.arrivals.subscribe();
    let last = app.with_store(|s| Ok(s.arrival_head())).await?;
    let mut ticker = tokio::time::interval_at(Instant::now() + PEERS_POLL, PEERS_POLL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let keep_alive = app.cfg.sse_keep_alive;
    let feed = Feed {
        peers_seen: peers_signature(&app.sync.peers()),
        app,
        bell,
        last,
        stale: false,
        failures: 0,
        relevant_to,
        ticker,
        out: VecDeque::new(),
    };
    let events = stream::unfold(feed, Feed::next);
    Ok(Sse::new(events).keep_alive(KeepAlive::new().interval(keep_alive).text("keep-alive")))
}

/// One open stream.
struct Feed {
    app: AppState,
    bell: Receiver<Applied>,
    /// The newest arrival already looked at.
    last: i64,
    /// The store may hold events after `last` that were not read yet.
    stale: bool,
    /// How many times in a row reading the store has failed.
    failures: u32,
    /// `Some` on a relevant stream, which carries only that handle's posts.
    relevant_to: Option<String>,
    /// Paces the look at the peer list (a relevant stream does not look).
    ticker: tokio::time::Interval,
    peers_seen: Vec<(String, Option<String>, bool)>,
    /// Events ready to send, oldest first.
    out: VecDeque<StreamEvent>,
}

impl Feed {
    /// The next item for the response body, or `None` when the stream is over.
    async fn next(mut self) -> Option<(Result<SseEvent, Infallible>, Feed)> {
        loop {
            if let Some(event) = self.out.pop_front() {
                let frame = SseEvent::default()
                    .event(event.name())
                    .data(event.data().to_string());
                return Some((Ok(frame), self));
            }
            if !self.fill().await {
                return None;
            }
        }
    }

    /// Waits until there is something to send and puts it in `out`. `false`
    /// means the stream should end (the node is stopping).
    async fn fill(&mut self) -> bool {
        loop {
            if self.stale {
                match self.catch_up().await {
                    Ok(()) => self.failures = 0,
                    Err(e) => {
                        self.failures += 1;
                        if self.failures >= MAX_STORE_FAILURES {
                            tracing::error!(
                                "stream: ending after {} failures to read new events: {e}",
                                self.failures
                            );
                            return false;
                        }
                        tracing::warn!("stream: cannot read new events, trying again: {e}");
                        tokio::select! {
                            () = tokio::time::sleep(RETRY_PAUSE) => {}
                            () = self.app.shutdown.triggered() => return false,
                        }
                    }
                }
                if !self.out.is_empty() {
                    return true;
                }
                // Nothing to show, but a full page (or a failure) means there
                // may be more to read before it is time to wait.
                if self.stale {
                    continue;
                }
            }
            tokio::select! {
                rang = ring(&mut self.bell, |_| true) => {
                    if !rang {
                        return false;
                    }
                    self.stale = true;
                }
                _ = self.ticker.tick(), if self.relevant_to.is_none() => {
                    let peers = self.app.sync.peers();
                    if let Some(event) = self.peers_changed(peers) {
                        self.out.push_back(event);
                        return true;
                    }
                }
                () = self.app.shutdown.triggered() => return false,
            }
        }
    }

    /// Reads the next page of events stored after `last` and turns what they
    /// changed into stream events. Because it reads the store, not the bell's
    /// messages, it sees each event exactly once, in arrival order, whether the
    /// receiver kept up or lagged.
    async fn catch_up(&mut self) -> Result<(), Error> {
        let after = self.last;
        let relevant_to = self.relevant_to.clone();
        let blobs = self.app.blobs.clone();
        let (events, last, full) = self
            .app
            .with_store(move |s| {
                let page = s.events_since_arrival(after, CATCH_UP_PAGE)?;
                let last = page.last().map_or(after, |a| a.arrival);
                let events = describe(s, &page, relevant_to.as_deref(), &blobs, now_ms())?;
                Ok((events, last, page.len() == CATCH_UP_PAGE))
            })
            .await?;
        self.last = last;
        self.stale = full;
        self.out.extend(events);
        Ok(())
    }

    /// A `peers` event if `peers` differ from the last ones seen in who they
    /// are or whether they are reachable (how far behind they are changes all
    /// the time and is not worth an event).
    fn peers_changed(&mut self, peers: Vec<PeerJson>) -> Option<StreamEvent> {
        let signature = peers_signature(&peers);
        if signature == self.peers_seen {
            return None;
        }
        self.peers_seen = signature;
        Some(StreamEvent::Peers(peers))
    }
}

fn peers_signature(peers: &[PeerJson]) -> Vec<(String, Option<String>, bool)> {
    peers
        .iter()
        .map(|p| (p.url.clone(), p.node.clone(), p.reachable))
        .collect()
}

/// What stored `page` changed, as stream events, read from the board as it is
/// now. An event that broke the rules was stored without deriving anything
/// (and never announced), and one that changed nothing that is shown (a post
/// that was deleted first, a second post with the same id, a delete older
/// than what keeps a channel alive) says nothing. With `relevant_to` set, only
/// the posts that matter to that handle are described.
fn describe(
    store: &Store,
    page: &[Applied],
    relevant_to: Option<&str>,
    blobs: &Blobs,
    now: i64,
) -> Result<Vec<StreamEvent>, Error> {
    // Read when the first post needs them: the joins as they are at the
    // moment of sending, not as they were when the stream was opened.
    let mut joins: Option<Vec<String>> = None;
    let mut presence = None;
    let mut told: HashSet<&str> = HashSet::new();
    let mut out = Vec::new();
    for applied in page {
        let event = &applied.event;
        // A relevant stream carries posts only.
        if relevant_to.is_some() && !matches!(event.kind, EventKind::PostCreated(_)) {
            continue;
        }
        if validate_event(event).is_err() {
            continue;
        }
        match &event.kind {
            EventKind::PostCreated(post) => {
                let Some(row) = store.shown_post(&post.post_id)? else {
                    continue;
                };
                // The row is the first event with this post id; a later one
                // that claims it made no post.
                if row.arrival != applied.arrival {
                    continue;
                }
                if let Some(handle) = relevant_to {
                    let joins = match &joins {
                        Some(joins) => joins,
                        None => joins.insert(store.joins(handle)?),
                    };
                    if !store.is_relevant(&row, handle, joins)? {
                        continue;
                    }
                }
                out.push(StreamEvent::Post(PostJson::from_row(&row, blobs)));
            }
            EventKind::PostDeleted { post_id, .. } => {
                out.push(StreamEvent::PostDeleted {
                    id: post_id.clone(),
                });
            }
            EventKind::ChannelCreated { name, .. } => {
                if let Some(row) = store.channel(name)? {
                    out.push(StreamEvent::Channel {
                        channel: ChannelJson::from_row(&row, None, None),
                        deleted: false,
                    });
                }
            }
            EventKind::ChannelDeleted { name } => {
                if store.channel(name)?.is_none() {
                    out.push(StreamEvent::Channel {
                        channel: ChannelJson {
                            name: name.clone(),
                            topic: None,
                            unread: None,
                            read_to: None,
                        },
                        deleted: true,
                    });
                }
            }
            // One event per handle per page, from the newest state: `presence`
            // if it is listed now, `agent_forgotten` if a forget left it off.
            // A heartbeat older than a forget says nothing.
            EventKind::AgentSeen(Presence { handle, .. })
            | EventKind::AgentForgotten { handle, .. } => {
                if told.contains(handle.as_str()) {
                    continue;
                }
                let rows = match &presence {
                    Some(rows) => rows,
                    None => presence.insert(store.who(now)?),
                };
                let event = match (rows.iter().find(|r| &r.handle == handle), &event.kind) {
                    (Some(row), _) => StreamEvent::Presence(PresenceJson::from_row(row, now)),
                    (None, EventKind::AgentForgotten { .. }) => StreamEvent::AgentForgotten {
                        handle: handle.clone(),
                    },
                    (None, _) => continue,
                };
                told.insert(handle.as_str());
                out.push(event);
            }
            // An announcement shows nothing; an unknown kind is never valid,
            // so it does not get this far.
            EventKind::PeerAnnounced { .. } | EventKind::Unknown(_) => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pull_waits_at_most_25_seconds() {
        assert_eq!(events_wait(None), Duration::ZERO);
        assert_eq!(events_wait(Some(0)), Duration::ZERO);
        assert_eq!(events_wait(Some(7)), Duration::from_secs(7));
        assert_eq!(events_wait(Some(25)), Duration::from_secs(25));
        assert_eq!(events_wait(Some(26)), Duration::from_secs(25));
        assert_eq!(events_wait(Some(u64::MAX)), Duration::from_secs(25));
    }

    #[test]
    fn a_wait_is_capped_at_an_hour() {
        assert_eq!(wait_timeout(None), Duration::from_secs(60));
        assert_eq!(wait_timeout(Some(0)), Duration::ZERO);
        assert_eq!(wait_timeout(Some(600)), Duration::from_secs(600));
        assert_eq!(wait_timeout(Some(3600)), Duration::from_secs(3600));
        assert_eq!(wait_timeout(Some(3601)), Duration::from_secs(3600));
        assert_eq!(wait_timeout(Some(u64::MAX)), Duration::from_secs(3600));
    }

    fn peer(url: &str, reachable: bool, queued: u64) -> PeerJson {
        PeerJson {
            node: Some("mac".into()),
            url: url.into(),
            reachable,
            last_sync: None,
            queued,
        }
    }

    #[test]
    fn peers_event_only_when_who_or_reachability_changes() {
        let a = [peer("http://mac:7700", true, 0)];
        let seen = peers_signature(&a);
        // Falling behind or catching up is not a change.
        assert_eq!(seen, peers_signature(&[peer("http://mac:7700", true, 9)]));
        // Going offline, coming back, or another peer appearing is.
        assert_ne!(seen, peers_signature(&[peer("http://mac:7700", false, 0)]));
        assert_ne!(
            seen,
            peers_signature(&[a[0].clone(), peer("http://pc:7700", true, 0)])
        );
        assert_ne!(seen, peers_signature(&[]));
    }
}
