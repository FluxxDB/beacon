//! Peer sync: every node keeps the whole board and converges with the peers
//! it knows, over plain HTTP (Tailscale encrypts the link).
//!
//! **Who the peers are.** `BEACON_PEERS`, plus every node's announced URL
//! (`peer_announced` events, which relay like any other), each normalized
//! ([`normalize_peer_url`]). This node's own `BEACON_URL` is never a peer, and
//! neither is any URL whose answers carry our own name in `X-Beacon-Node`:
//! that is learned on first contact and the URL is dropped for good. Several
//! URLs that turn out to reach the same node are one peer, which uses one of
//! them at a time and moves to the next when it fails.
//!
//! **How events move.** Each peer has two tasks. The *puller* holds a long
//! poll open on the peer (`GET /api/events?after=<our vv>&wait=25`) and stores
//! whatever comes back; it follows `more` at once only while its pages keep
//! bringing something new, and otherwise long-polls with `since` set to the
//! peer's last head, so a gap both nodes share never turns into a hot loop
//! (ruling R22). The *pusher* wakes on every write stored here (and every 5 s)
//! and sends the peer what its last known version vector lacks, in batches of
//! 500 (`POST /api/events`). A post also goes out at once to every peer from
//! the request that made it ([`SyncHandle::push_now`]), which is what reports
//! delivery; a peer we are waiting out (below) is reported undelivered without
//! being dialled. Everything is idempotent, so all of these may overlap.
//!
//! **Pages** are stored in one transaction each (one sync to disk per page,
//! not per event). The first pulls from each peer after this node starts
//! also ask again for our own events written since it started
//! ([`crate::store::Store::opened_at`]): if this database lost commits the
//! peer already had and wrote different events at those seqs, the peer's
//! copies show it, and the store settles the collision by moving to a new
//! origin and recording both sides again.
//!
//! **What a peer holds** is what its answers said, which only grows while it
//! keeps its database. An answer (to a pull or a push) that holds less than
//! our view of it when we asked means the peer lost events (its database was
//! replaced, even if its head does not show it): the view becomes that
//! answer, and the pusher sends the peer what it lost, whether or not
//! anything new is written here.
//!
//! **When a peer is away.** A failed request marks the peer unreachable, and
//! both of its tasks wait before the next try: 5 s, doubling, at most 2 min.
//! A request from the peer itself (it is back, and pulling) cuts the wait short.
//!
//! **Attachments.** A post arrives without its file. A node fetches the bytes
//! from a reachable peer (`GET /api/files/<sha256>`, checked against the hash
//! and never read past the announced size or our own limit), retrying with
//! backoff, and sweeps for missing files every 60 s.
//!
//! Every request goes through `BEACON_PROXY` when it is set (`socks5h://` on a
//! userspace-Tailscale device, so names resolve through tailscaled) and
//! carries the secret and our node name (see [`http`]). Every task here ends
//! when the node stops, and [`SyncHandle::stopped`] waits for them.

mod http;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use futures_util::future::join_all;
use reqwest::Url;
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{mpsc, watch, Mutex as AsyncMutex};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

pub use self::http::parse_proxy;
use self::http::PeerClient;
use crate::api::{Delivery, EventsResponse, PeerJson, PushResponse};
use crate::config::Config;
use crate::error::Error;
use crate::model::{format_vv, Event, EventKind, VersionVector};
use crate::server::{now_ms, run_on_store, AppState, Shutdown};
use crate::store::{Applied, Store};
use crate::timefmt::iso8601;

/// The first wait before trying a peer that stopped answering again. Each
/// failed retry doubles it, up to [`MAX_BACKOFF`]. (`Config::sync_base_backoff`
/// overrides it in tests.)
pub const BASE_BACKOFF: Duration = Duration::from_secs(5);
/// The longest wait between tries.
pub const MAX_BACKOFF: Duration = Duration::from_secs(120);
/// How long a peer holds our pull open when it has nothing new.
/// (`Config::long_poll_secs` overrides it in tests.)
pub const LONG_POLL_SECS: u64 = 25;
/// A pull's HTTP timeout is its long poll plus this: 35 s.
const PULL_SLACK: Duration = Duration::from_secs(10);
/// The HTTP timeout of a push from the pusher.
const PUSH_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a new post waits on each peer to confirm it has it.
const PUSH_NOW_TIMEOUT: Duration = Duration::from_secs(2);
/// The pusher looks for something to send this often even with no write.
const PUSH_TICK: Duration = Duration::from_secs(5);
/// Events per push, and per pull.
const BATCH: usize = 500;
/// A pull that waited for news but brought nothing new is followed by the
/// next one no sooner than this, whatever the peer does.
const IDLE_PULL_GAP: Duration = Duration::from_millis(100);
/// How often every post's attachment is checked for, and the longest wait
/// between two tries of one file.
const BLOB_SWEEP: Duration = Duration::from_secs(60);
/// How long [`SyncHandle::stopped`] waits for the tasks to end.
const STOP_WAIT: Duration = Duration::from_secs(5);
/// A peer's URL may answer as another node, and start over, at most this
/// often (or once per base backoff, if that is longer): two nodes answering
/// at one address must not turn into steady traffic.
const RENAME_GAP: Duration = Duration::from_secs(5);
/// The header that carries a node's name: on our requests (who is asking),
/// and on a peer's `/api/events` answers (who answered).
pub const NODE_HEADER: &str = "x-beacon-node";

// ------------------------------------------------------------------- urls

/// A peer URL in normal form, or `None` if it is not one sync can use. Trims
/// it, lowercases the scheme and host, drops a trailing `/` (and the default
/// port 80), and requires `http://host[:port]` with nothing after it: no
/// path, query, fragment or credentials, and no TLS (peers talk over
/// Tailscale, which encrypts the link).
pub fn normalize_peer_url(s: &str) -> Option<String> {
    let url = Url::parse(s.trim()).ok()?;
    let plain = url.scheme() == "http"
        && url.host_str().is_some_and(|host| !host.is_empty())
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none();
    plain.then(|| url.as_str().trim_end_matches('/').to_string())
}

/// How many events a peer lacks: per origin, how far our version vector is
/// ahead of its (`None`: nothing is known about it, so everything).
pub fn queued(ours: &VersionVector, theirs: Option<&VersionVector>) -> u64 {
    ours.iter()
        .map(|(origin, &seq)| {
            let held = theirs.and_then(|t| t.get(origin)).copied().unwrap_or(0);
            seq.saturating_sub(held)
        })
        .sum()
}

/// `a` raised to `b` wherever `b` is ahead.
fn merge_vv(a: &mut VersionVector, b: &VersionVector) {
    for (origin, &seq) in b {
        let held = a.entry(origin.clone()).or_insert(0);
        *held = (*held).max(seq);
    }
}

/// Whether `answer` holds less than `view` of some origin.
fn behind(answer: &VersionVector, view: &VersionVector) -> bool {
    view.iter()
        .any(|(origin, &seq)| answer.get(origin).copied().unwrap_or(0) < seq)
}

/// How many events `vv` covers, in all.
fn total(vv: &VersionVector) -> u64 {
    vv.values().fold(0, |sum, &seq| sum.saturating_add(seq))
}

/// Raises `have` (what a peer holds, gap-free per origin) by `events`, which
/// it is known to hold: each origin's seqs that continue it without a gap.
fn raise_by(have: &mut VersionVector, events: &[Event]) {
    let mut by_origin: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
    for ev in events {
        by_origin.entry(&ev.origin).or_default().push(ev.seq);
    }
    for (origin, mut seqs) in by_origin {
        seqs.sort_unstable();
        let start = have.get(origin).copied().unwrap_or(0);
        let mut held = start;
        for seq in seqs {
            if seq == held + 1 {
                held = seq;
            }
        }
        if held > start {
            have.insert(origin.to_string(), held);
        }
    }
}

/// Whether `have` holds event `seq` of `origin`.
fn covers(have: &VersionVector, origin: &str, seq: u64) -> bool {
    have.get(origin).is_some_and(|&held| held >= seq)
}

// ---------------------------------------------------------------- backoff

/// When a peer that stopped answering may be tried again.
///
/// A failure starts a wait (`base`, then double the last one, at most
/// [`MAX_BACKOFF`]). A failure while a wait is on (another request to the same
/// peer, sent before it ran) changes nothing, so a puller and a pusher that
/// fail together count once. A success ends the wait and starts over at `base`.
#[derive(Debug, Clone)]
struct Backoff {
    base: Duration,
    /// The wait the next failure starts.
    next: Duration,
    /// The current wait: when it started and when it ends.
    window: Option<(Instant, Instant)>,
}

impl Backoff {
    fn new(base: Duration) -> Backoff {
        Backoff {
            base,
            next: base,
            window: None,
        }
    }

    /// A request failed at `now`. Returns whether that started a new wait.
    fn failed(&mut self, now: Instant) -> bool {
        if let Some((_, until)) = self.window {
            if now < until {
                return false;
            }
        }
        self.window = Some((now, now + self.next));
        self.next = (self.next * 2).min(MAX_BACKOFF.max(self.base));
        true
    }

    /// A request succeeded: no wait, and the next failure waits `base`.
    fn succeeded(&mut self) {
        self.next = self.base;
        self.window = None;
    }

    /// How long until the peer may be tried (zero: now).
    fn wait(&self, now: Instant) -> Duration {
        self.window.map_or(Duration::ZERO, |(_, until)| {
            until.saturating_duration_since(now)
        })
    }

    /// Ends the current wait, keeping the doubling: there is reason to
    /// think the peer can be reached now.
    fn retry_now(&mut self) {
        self.window = None;
    }

    /// Ends the current wait now if it has lasted at least `base`, keeping
    /// the doubling. Returns whether it did.
    fn cut_short(&mut self, now: Instant) -> bool {
        match self.window {
            Some((start, until)) if now < until && now.duration_since(start) >= self.base => {
                self.window = Some((start, now));
                true
            }
            _ => false,
        }
    }
}

// ------------------------------------------------------------- peer table

type PeerId = u64;

/// One URL a peer may be reached at, and where it came from.
#[derive(Debug, Clone)]
struct Candidate {
    url: String,
    /// Listed in `BEACON_PEERS` (or added with [`SyncHandle::add_peer`]).
    configured: bool,
    /// The newest URL the peer announced for itself.
    announced: bool,
}

/// A node this one syncs with.
struct Peer {
    id: PeerId,
    /// Its name: from its answers once it has answered, else what its
    /// announcement claimed.
    node: Option<String>,
    /// Never empty; `at` is the one in use.
    urls: Vec<Candidate>,
    at: usize,
    reachable: bool,
    last_sync_ms: Option<i64>,
    /// Its version vector, as last heard (`None` before it ever answered).
    their_have: Option<VersionVector>,
    /// The `have` of the last pull answer that `their_have` was lowered to
    /// ([`Table::pulled`]). The same answer again lowers nothing, so a peer
    /// whose pulls say less than its pushes is sent things again once, not
    /// after every pull.
    lowered_to: Option<VersionVector>,
    /// When its URL last started over as another node ([`Table::learned`]).
    renamed_at: Option<Instant>,
    /// Its newest arrival, as last heard from a pull. Going down means its
    /// database was replaced.
    their_head: Option<i64>,
    /// `since` for the next long poll (R22): the head of its last answer.
    since: i64,
    /// Our arrival head and its version vector when the pusher last finished
    /// a round: a selection with nothing past the one and the other unchanged
    /// was sent already, so it is not sent again.
    push_mark: Option<(i64, VersionVector)>,
    backoff: Backoff,
    /// Ends the peer's tasks (it was dropped, or merged into another).
    cancel: CancellationToken,
    /// Rings when the peer may be tried at once (its wait was cut short).
    retry: watch::Sender<()>,
    /// One push to this peer at a time: the pusher's or [`SyncHandle::push_now`]'s.
    push_lock: Arc<AsyncMutex<()>>,
}

impl Peer {
    fn url(&self) -> &str {
        &self.urls[self.at].url
    }

    fn has_url(&self, url: &str) -> bool {
        self.urls.iter().any(|c| c.url == url)
    }

    /// How it is named in a delivery report: its node, or its URL until it
    /// has answered.
    fn label(&self) -> String {
        self.node.clone().unwrap_or_else(|| self.url().to_string())
    }

    /// Lets its tasks try it now.
    fn wake(&self) {
        self.retry.send_replace(());
    }

    fn json(&self, ours: &VersionVector) -> PeerJson {
        PeerJson {
            node: self.node.clone(),
            url: self.url().to_string(),
            reachable: self.reachable,
            last_sync: self.last_sync_ms.map(iso8601),
            queued: queued(ours, self.their_have.as_ref()),
        }
    }
}

/// What an answer's `X-Beacon-Node` meant for the peer it came from.
#[derive(Debug, PartialEq, Eq)]
enum Learned {
    /// The peer is who we thought: carry on.
    Same,
    /// Another node answered at the peer's URL, which changed hands moments
    /// ago (two nodes answering at one address): the answer says nothing
    /// about what the peer holds, and is not used for that.
    Other,
    /// That URL was this node; the peer has other URLs to try.
    Retry,
    /// The peer is gone (it was this node, or it joined the peer that already
    /// had that name): its tasks end.
    Gone,
}

/// Every peer and what is known about it.
struct Table {
    /// Our node name and normalized `BEACON_URL`.
    node: String,
    own_url: Option<String>,
    base: Duration,
    next_id: PeerId,
    /// In the order they were added (`BEACON_PEERS` first).
    peers: Vec<Peer>,
    /// URLs that turned out to reach this node.
    selves: BTreeSet<String>,
}

impl Table {
    fn index(&self, id: PeerId) -> Option<usize> {
        self.peers.iter().position(|p| p.id == id)
    }

    fn get(&self, id: PeerId) -> Option<&Peer> {
        self.peers.iter().find(|p| p.id == id)
    }

    fn get_mut(&mut self, id: PeerId) -> Option<&mut Peer> {
        self.peers.iter_mut().find(|p| p.id == id)
    }

    /// Whether `url` is this node.
    fn is_us(&self, url: &str) -> bool {
        self.own_url.as_deref() == Some(url) || self.selves.contains(url)
    }

    fn insert(&mut self, node: Option<String>, candidate: Candidate) -> PeerId {
        self.next_id += 1;
        let id = self.next_id;
        self.peers.push(Peer {
            id,
            node,
            urls: vec![candidate],
            at: 0,
            reachable: false,
            last_sync_ms: None,
            their_have: None,
            lowered_to: None,
            renamed_at: None,
            their_head: None,
            since: 0,
            push_mark: None,
            backoff: Backoff::new(self.base),
            cancel: CancellationToken::new(),
            retry: watch::channel(()).0,
            push_lock: Arc::new(AsyncMutex::new(())),
        });
        id
    }

    /// A URL from `BEACON_PEERS`. Returns the new peer to start, if it is one.
    fn add_configured(&mut self, url: &str) -> Option<PeerId> {
        let url = normalize_peer_url(url)?;
        if self.is_us(&url) {
            return None;
        }
        for peer in &mut self.peers {
            if let Some(c) = peer.urls.iter_mut().find(|c| c.url == url) {
                c.configured = true;
                return None;
            }
        }
        Some(self.insert(
            None,
            Candidate {
                url,
                configured: true,
                announced: false,
            },
        ))
    }

    /// `node` announced that it is at `url`. Returns the new peer to start,
    /// if it is one. The node's earlier announced URL, unless it is also
    /// configured, is dropped: the node says it moved.
    fn add_announced(&mut self, node: &str, url: &str) -> Option<PeerId> {
        if node == self.node {
            return None;
        }
        let url = normalize_peer_url(url)?;
        if self.is_us(&url) {
            return None;
        }
        // Forget what it announced before.
        for i in 0..self.peers.len() {
            if self.peers[i].node.as_deref() != Some(node) {
                continue;
            }
            let stale: Vec<String> = self.peers[i]
                .urls
                .iter()
                .filter(|c| c.announced && !c.configured && c.url != url)
                .map(|c| c.url.clone())
                .collect();
            for old in stale {
                if self.peers[i].urls.len() == 1 {
                    // Its only URL: replaced in place below.
                    break;
                }
                self.remove_url(i, &old);
            }
        }
        if let Some(peer) = self.peers.iter_mut().find(|p| p.has_url(&url)) {
            for c in &mut peer.urls {
                c.announced = c.url == url;
            }
            return None;
        }
        let candidate = Candidate {
            url,
            configured: false,
            announced: true,
        };
        if let Some(peer) = self
            .peers
            .iter_mut()
            .find(|p| p.node.as_deref() == Some(node))
        {
            if peer.urls.len() == 1 && peer.urls[0].announced && !peer.urls[0].configured {
                // The one URL it had is the old announcement: take the new
                // one, and try it now.
                peer.urls[0] = candidate;
                peer.at = 0;
                peer.backoff.retry_now();
                peer.wake();
            } else {
                peer.urls.push(candidate);
            }
            return None;
        }
        Some(self.insert(Some(node.to_string()), candidate))
    }

    /// Removes `url` from peer `i` (which keeps at least one). Moves `at` off
    /// it if it was in use.
    fn remove_url(&mut self, i: usize, url: &str) {
        let peer = &mut self.peers[i];
        let Some(gone) = peer.urls.iter().position(|c| c.url == url) else {
            return;
        };
        if peer.urls.len() == 1 {
            return;
        }
        peer.urls.remove(gone);
        if gone < peer.at {
            peer.at -= 1;
        }
        if peer.at >= peer.urls.len() {
            peer.at = 0;
        }
    }

    /// Removes peer `i` and ends its tasks.
    fn remove_peer(&mut self, i: usize) -> Peer {
        let peer = self.peers.remove(i);
        peer.cancel.cancel();
        peer
    }

    /// Peer `id` answered from `url`, and the answer was signed `node`.
    fn learned(&mut self, id: PeerId, url: &str, node: &str) -> Learned {
        let Some(i) = self.index(id) else {
            return Learned::Gone;
        };
        if node == self.node {
            tracing::info!(%url, "sync: {url} is this node; not syncing with it");
            self.selves.insert(url.to_string());
            if self.peers[i].urls.len() == 1 {
                self.remove_peer(i);
                return Learned::Gone;
            }
            self.remove_url(i, url);
            return Learned::Retry;
        }
        if self.peers[i].node.as_deref() == Some(node) {
            return Learned::Same;
        }
        let other = self
            .peers
            .iter()
            .position(|p| p.id != id && p.node.as_deref() == Some(node));
        let gap = self.base.max(RENAME_GAP);
        let Some(_) = other else {
            let peer = &mut self.peers[i];
            if let Some(old) = &peer.node {
                let now = Instant::now();
                if peer
                    .renamed_at
                    .is_some_and(|at| now.duration_since(at) < gap)
                {
                    // It changed hands moments ago: two nodes answering at
                    // one address. Until the gap is over the peer stays the
                    // current one, and the other's answers say nothing of
                    // what it holds, so starting over cannot become a loop.
                    return Learned::Other;
                }
                // Another node took over the address: what we knew of the
                // old one says nothing about this one.
                tracing::info!(%url, "sync: {url} answers as {node}, not {old}");
                peer.their_have = None;
                peer.lowered_to = None;
                peer.their_head = None;
                peer.since = 0;
                peer.push_mark = None;
                peer.renamed_at = Some(now);
                peer.wake();
            }
            peer.node = Some(node.to_string());
            return Learned::Same;
        };
        // Another peer already is that node: it takes these URLs, and uses
        // the one that just answered.
        let merged = self.remove_peer(i);
        let Some(keep) = self
            .peers
            .iter_mut()
            .find(|p| p.node.as_deref() == Some(node))
        else {
            return Learned::Gone;
        };
        for c in merged.urls {
            match keep.urls.iter_mut().find(|k| k.url == c.url) {
                Some(k) => {
                    k.configured |= c.configured;
                    k.announced |= c.announced;
                }
                None => keep.urls.push(c),
            }
        }
        if let Some(at) = keep.urls.iter().position(|c| c.url == url) {
            keep.at = at;
        }
        if !keep.reachable {
            // That URL just answered: no reason to wait.
            keep.backoff.retry_now();
            keep.wake();
        }
        Learned::Gone
    }

    /// A request to peer `id` at `url` failed. Returns whether the peer was
    /// reachable until now.
    fn failed(&mut self, id: PeerId, url: &str) -> bool {
        let Some(peer) = self.get_mut(id) else {
            return false;
        };
        let was = peer.reachable;
        peer.reachable = false;
        if peer.backoff.failed(Instant::now()) && peer.url() == url && peer.urls.len() > 1 {
            // Try the next URL when the wait is over.
            peer.at = (peer.at + 1) % peer.urls.len();
        }
        was
    }

    /// Peer `id` answered. Returns whether it was unreachable until now.
    fn answered(&mut self, id: PeerId) -> bool {
        let Some(peer) = self.get_mut(id) else {
            return false;
        };
        let back = !peer.reachable;
        let waiting = peer.backoff.window.is_some();
        peer.reachable = true;
        peer.last_sync_ms = Some(now_ms());
        peer.backoff.succeeded();
        if waiting {
            peer.wake();
        }
        back
    }

    /// A pull from peer `id`, sent while our view of it was `asked_with`,
    /// answered with its version vector `have` and its head `head`. Returns
    /// whether it was unreachable until now.
    ///
    /// A peer's version vector only grows while it keeps its database, and
    /// the view holds nothing it had not said or shown by then, so an answer
    /// that holds less of some origin than `asked_with` means the database
    /// was replaced, even if its head did not go back: like a head that went
    /// back, the view becomes the answer and the pusher is woken. (Comparing
    /// with the view as it is now could mistake an answer that was merely
    /// overtaken, by a push it acknowledged meanwhile, for a loss.)
    ///
    /// Otherwise the view is raised to `have` and never past it: the page's
    /// events say no more, because each one the peer holds without a gap is
    /// in `have` already and the others lie above a gap in it. Raising by
    /// them from a view that was ahead of the truth would push it further.
    fn pulled(
        &mut self,
        id: PeerId,
        asked_with: Option<&VersionVector>,
        have: VersionVector,
        head: i64,
    ) -> bool {
        let back = self.answered(id);
        let Some(peer) = self.get_mut(id) else {
            return back;
        };
        let went_back = peer.their_head.is_some_and(|before| head < before);
        let lost = asked_with.is_some_and(|view| behind(&have, view))
            && peer.lowered_to.as_ref() != Some(&have);
        if went_back || lost {
            // A new database, which holds only what it says, whatever it
            // held before.
            tracing::info!("sync: {} has a new database", peer.label());
            if lost {
                peer.lowered_to = Some(have.clone());
            }
            peer.their_have = Some(have);
            peer.push_mark = None;
            peer.wake();
        } else {
            match &mut peer.their_have {
                Some(known) => merge_vv(known, &have),
                slot => *slot = Some(have),
            }
        }
        peer.their_head = Some(head);
        peer.since = head;
        back
    }

    /// A push to peer `id`, of a batch chosen from our view `chosen_from`,
    /// answered with its version vector `have`. Returns whether it was
    /// unreachable until now.
    ///
    /// An answer that holds less of some origin than `chosen_from` means the
    /// peer lost what it had (see [`Table::pulled`]): the view becomes the
    /// answer, or it would never be sent those events again. Otherwise the
    /// view is raised to it.
    fn pushed(&mut self, id: PeerId, chosen_from: &VersionVector, have: &VersionVector) -> bool {
        let back = self.answered(id);
        if let Some(peer) = self.get_mut(id) {
            match &mut peer.their_have {
                Some(known) if !behind(have, chosen_from) => merge_vv(known, have),
                slot => *slot = Some(have.clone()),
            }
        }
        back
    }
}

// ---------------------------------------------------------------- handle

/// What the blob fetcher is asked to do.
#[derive(Debug)]
enum BlobJob {
    /// A post announced this file and it is not here.
    Want { sha: String, size: u64 },
    /// A peer came back: try every missing file again now.
    RetryAll,
    /// Look at every post for missing files.
    Sweep,
}

struct Inner {
    cfg: Arc<Config>,
    client: PeerClient,
    store: Arc<Mutex<Store>>,
    table: Mutex<Table>,
    /// Our version vector, kept current by the server on every write, so
    /// [`SyncHandle::peers`] never waits on the store.
    our_vv: Mutex<VersionVector>,
    /// [`Store::opened_at`], read once: where the replay of our own events
    /// starts on the first pulls from each peer.
    opened_at: (String, u64),
    /// Bumped on every write stored here; the pushers wait on it.
    written: watch::Sender<u64>,
    tasks: TaskTracker,
    /// Peers added from outside a sync task, for the supervisor to start.
    launch: mpsc::UnboundedSender<PeerId>,
    blob_jobs: mpsc::UnboundedSender<BlobJob>,
    /// The receiving ends, until [`spawn`] takes them.
    receivers: Mutex<Option<Receivers>>,
}

struct Receivers {
    launch: mpsc::UnboundedReceiver<PeerId>,
    blob_jobs: mpsc::UnboundedReceiver<BlobJob>,
}

/// The server's handle on the sync engine; cheap to clone.
///
/// It holds no [`AppState`] (the state holds it): the tasks [`spawn`] starts
/// do, and they end when the node stops.
#[derive(Clone)]
pub struct SyncHandle {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for SyncHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncHandle")
            .field("node", &self.inner.cfg.node)
            .finish_non_exhaustive()
    }
}

/// Lock a std mutex; a panic elsewhere leaves the data usable.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl SyncHandle {
    /// The engine for a node with config `cfg` and board `store`, not yet
    /// running (see [`spawn`]). Fails if the secret, node name or proxy cannot
    /// be used in a request.
    pub fn new(cfg: Arc<Config>, store: Arc<Mutex<Store>>) -> Result<SyncHandle, Error> {
        let client = PeerClient::new(&cfg)?;
        let (our_vv, opened_at) = {
            let store = lock(&store);
            (store.version_vector(), store.opened_at())
        };
        let (launch, launch_rx) = mpsc::unbounded_channel();
        let (blob_jobs, blob_rx) = mpsc::unbounded_channel();
        let table = Table {
            node: cfg.node.clone(),
            own_url: cfg.url.as_deref().and_then(normalize_peer_url),
            base: cfg.sync_base_backoff,
            next_id: 0,
            peers: Vec::new(),
            selves: BTreeSet::new(),
        };
        Ok(SyncHandle {
            inner: Arc::new(Inner {
                client,
                store,
                table: Mutex::new(table),
                our_vv: Mutex::new(our_vv),
                opened_at,
                written: watch::channel(0).0,
                tasks: TaskTracker::new(),
                launch,
                blob_jobs,
                receivers: Mutex::new(Some(Receivers {
                    launch: launch_rx,
                    blob_jobs: blob_rx,
                })),
                cfg,
            }),
        })
    }

    fn table(&self) -> MutexGuard<'_, Table> {
        lock(&self.inner.table)
    }

    fn with_peer<T>(&self, id: PeerId, f: impl FnOnce(&Peer) -> T) -> Option<T> {
        self.table().get(id).map(f)
    }

    fn our_vv(&self) -> VersionVector {
        lock(&self.inner.our_vv).clone()
    }

    /// A write was stored on this node: tell the pushers there is something to send.
    pub fn notify(&self) {
        self.inner.written.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// The store's version vector after a write; the server calls it under
    /// the store lock, so it is always the latest.
    pub fn record_version_vector(&self, vv: VersionVector) {
        *lock(&self.inner.our_vv) = vv;
    }

    /// The peers this node syncs with, one per node, in the order they were
    /// added.
    pub fn peers(&self) -> Vec<PeerJson> {
        let ours = self.our_vv();
        self.table().peers.iter().map(|p| p.json(&ours)).collect()
    }

    /// Adds a peer URL (normalized; ignored if it is not usable, is this
    /// node, or is known already) and starts syncing with it.
    pub fn add_peer(&self, url: &str) {
        let added = self.table().add_configured(url);
        if let Some(id) = added {
            let _ = self.inner.launch.send(id);
        }
    }

    /// A request arrived from node `node`'s sync: it is up. If we were
    /// waiting to retry it, and have waited at least the base backoff, try
    /// it now.
    pub fn heard_from(&self, node: &str) {
        let mut table = self.table();
        if let Some(peer) = table
            .peers
            .iter_mut()
            .find(|p| p.node.as_deref() == Some(node) && !p.reachable)
        {
            if peer.backoff.cut_short(Instant::now()) {
                peer.wake();
            }
        }
    }

    /// Node `node` pushed us `events`: it holds them. Where they continue its
    /// version vector without a gap, that is what it has now, so the pusher
    /// does not send them back.
    pub fn received_from(&self, node: &str, events: &[Event]) {
        let mut table = self.table();
        let known = table
            .peers
            .iter_mut()
            .find(|p| p.node.as_deref() == Some(node))
            .and_then(|p| p.their_have.as_mut());
        if let Some(have) = known {
            raise_by(have, events);
        }
    }

    /// Pushes the events each peer lacks, up to and including `ev` (just
    /// written here), to every peer at once, and reports who confirmed they
    /// hold `ev`. Each peer gets 2 s; one that fails, or does not answer at
    /// all in that time, is marked unreachable, and `ev` stays queued for it.
    /// A peer in a backoff wait is not dialled: undelivered at once (R25).
    pub async fn push_now(&self, ev: &Event) -> Vec<Delivery> {
        let targets: Vec<(PeerId, Arc<AsyncMutex<()>>)> = self
            .table()
            .peers
            .iter()
            .map(|p| (p.id, Arc::clone(&p.push_lock)))
            .collect();
        let pushes = targets
            .into_iter()
            .map(|(id, push_lock)| self.push_one(id, push_lock, ev));
        join_all(pushes).await.into_iter().flatten().collect()
    }

    /// [`SyncHandle::push_now`] for one peer. `None` if it turned out not to
    /// be a peer (this node, or another URL of a peer already reported).
    async fn push_one(
        &self,
        id: PeerId,
        push_lock: Arc<AsyncMutex<()>>,
        ev: &Event,
    ) -> Option<Delivery> {
        let report = |delivered: bool| {
            self.with_peer(id, |p| Delivery {
                node: p.label(),
                delivered,
            })
        };
        // A peer we are waiting out is not dialled (R25): the post would wait
        // its whole 2 s on a machine that is most likely off. Its pusher,
        // woken by this write, sends the post when the wait is over.
        let waiting = self.with_peer(id, |p| !p.backoff.wait(Instant::now()).is_zero())?;
        if waiting {
            return report(false);
        }
        let started_ms = now_ms();
        let deadline = tokio::time::Instant::now() + PUSH_NOW_TIMEOUT;
        // The pusher is sending to it: whatever it sends, we see after.
        let Ok(_turn) = tokio::time::timeout_at(deadline, push_lock.lock_owned()).await else {
            return report(false);
        };
        let url = self.with_peer(id, |p| p.url().to_string())?;
        match tokio::time::timeout_at(deadline, self.deliver(id, ev)).await {
            Ok(Ok(Some(delivered))) => report(delivered),
            Ok(Ok(None)) => None,
            Ok(Err((url, e))) => {
                self.failed(id, &url, &e);
                report(false)
            }
            Err(_) => {
                // Out of time. A peer that has answered since we started
                // (taking pages, or answering the pusher) is slow, not gone.
                let answered = self
                    .with_peer(id, |p| p.last_sync_ms.is_some_and(|ms| ms >= started_ms))
                    .unwrap_or(false);
                if !answered {
                    self.failed(id, &url, &anyhow!("no answer within {PUSH_NOW_TIMEOUT:?}"));
                }
                report(false)
            }
        }
    }

    /// Sends peer `id` what it lacks until it holds `ev`. `Some(true)` when it
    /// says it does, `Some(false)` when an answer showed no progress or came
    /// from another node ([`Learned::Other`]), `None` when it is not a peer
    /// after all.
    ///
    /// The first batch is chosen from what we know of the peer (R13); every
    /// later one from what the peer itself last answered. Our view of a peer
    /// can run ahead of the truth (a wiped database not noticed yet): a batch
    /// chosen from it may lack what the peer needs, and only the peer's own
    /// answer says what that is.
    async fn deliver(
        &self,
        id: PeerId,
        ev: &Event,
    ) -> Result<Option<bool>, (String, anyhow::Error)> {
        // The peer's own last answer, in this delivery.
        let mut told: Option<VersionVector> = None;
        loop {
            let Some((url, known)) =
                self.with_peer(id, |p| (p.url().to_string(), p.their_have.clone()))
            else {
                return Ok(None);
            };
            let have = match (&told, known) {
                (Some(told), _) => told.clone(),
                // The pusher may have delivered it already.
                (None, Some(known)) if covers(&known, &ev.origin, ev.seq) => {
                    return Ok(Some(true));
                }
                (None, Some(known)) => known,
                (None, None) => {
                    // Never heard from it: ask what it has before sending.
                    let (node, answer) = push(&self.inner.client, &url, &[], PUSH_NOW_TIMEOUT)
                        .await
                        .map_err(|e| (url.clone(), e))?;
                    match self.learned(id, &url, &node) {
                        Learned::Same => {
                            self.pushed(id, &VersionVector::new(), &answer.have);
                            told = Some(answer.have);
                        }
                        Learned::Other => return Ok(Some(false)),
                        Learned::Retry => {}
                        Learned::Gone => return Ok(None),
                    }
                    continue;
                }
            };
            if covers(&have, &ev.origin, ev.seq) {
                return Ok(Some(true));
            }
            let after = have.clone();
            let page =
                run_on_store(&self.inner.store, move |s| s.events_after(&after, BATCH)).await;
            let page = match page {
                Ok(page) if !page.is_empty() => page,
                Ok(_) => return Ok(Some(false)),
                Err(e) => {
                    tracing::error!("sync: cannot read events to push: {e}");
                    return Ok(Some(false));
                }
            };
            let (node, answer) = push(&self.inner.client, &url, &page, PUSH_NOW_TIMEOUT)
                .await
                .map_err(|e| (url.clone(), e))?;
            match self.learned(id, &url, &node) {
                Learned::Same => {}
                // Another node answered: that says nothing of whether the
                // peer holds `ev`.
                Learned::Other => return Ok(Some(false)),
                Learned::Retry => {
                    told = None;
                    continue;
                }
                Learned::Gone => return Ok(None),
            }
            self.pushed(id, &have, &answer.have);
            if covers(&answer.have, &ev.origin, ev.seq) {
                return Ok(Some(true));
            }
            // Stop when its own word shows no progress since its last answer.
            if told
                .as_ref()
                .is_some_and(|t| total(&answer.have) <= total(t))
            {
                return Ok(Some(false));
            }
            told = Some(answer.have);
        }
    }

    /// How many sync tasks are running. (For tests: 0 once the node stopped.)
    #[doc(hidden)]
    pub fn task_count(&self) -> usize {
        self.inner.tasks.len()
    }

    /// Waits (a few seconds at most) for every sync task to end. The node
    /// calls it while stopping, after signalling shutdown.
    pub async fn stopped(&self) {
        self.inner.tasks.close();
        if tokio::time::timeout(STOP_WAIT, self.inner.tasks.wait())
            .await
            .is_err()
        {
            tracing::warn!(
                "sync: {} tasks still running {STOP_WAIT:?} after shutdown",
                self.inner.tasks.len()
            );
        }
    }

    // ------------------------------------------------ table updates + logs

    fn learned(&self, id: PeerId, url: &str, node: &str) -> Learned {
        self.table().learned(id, url, node)
    }

    fn failed(&self, id: PeerId, url: &str, why: &anyhow::Error) {
        let (was, label) = {
            let mut table = self.table();
            let was = table.failed(id, url);
            (was, table.get(id).map(Peer::label))
        };
        match label {
            Some(label) if was => {
                tracing::warn!("sync: {label} unreachable at {url}: {why:#}");
            }
            _ => tracing::debug!("sync: {url} failed again: {why:#}"),
        }
    }

    /// A pull from peer `id`, sent while our view of it was `asked_with`,
    /// answered with its version vector `have` and its head `head` (see
    /// [`Table::pulled`]). Called before the page's events are stored, so
    /// the pusher, woken by each one stored, already knows the peer holds
    /// them and does not send them straight back.
    fn pulled(
        &self,
        id: PeerId,
        asked_with: Option<&VersionVector>,
        have: VersionVector,
        head: i64,
    ) {
        let back = self.table().pulled(id, asked_with, have, head);
        if back {
            self.came_back(id);
        }
    }

    /// A push to peer `id`, of a batch chosen from our view `chosen_from`,
    /// answered with its version vector `have` (see [`Table::pushed`]).
    fn pushed(&self, id: PeerId, chosen_from: &VersionVector, have: &VersionVector) {
        let back = self.table().pushed(id, chosen_from, have);
        if back {
            self.came_back(id);
        }
    }

    fn came_back(&self, id: PeerId) {
        if let Some(label) = self.with_peer(id, Peer::label) {
            tracing::info!("sync: {label} reachable");
        }
        // Files that no reachable peer had may be there now.
        let _ = self.inner.blob_jobs.send(BlobJob::RetryAll);
    }
}

// -------------------------------------------------------------- start up

/// Starts the engine in `state.sync`: announces this node's URL if it
/// changed, starts syncing with every peer known now and every one learned
/// later, and keeps attachments fetched. Every task ends when the node stops
/// (see [`SyncHandle::stopped`]). Returns the handle; starting it twice
/// does nothing the second time.
pub fn spawn(state: AppState) -> SyncHandle {
    let handle = state.sync.clone();
    let Some(receivers) = lock(&handle.inner.receivers).take() else {
        tracing::warn!("sync: already started");
        return handle;
    };
    let tasks = &handle.inner.tasks;
    tasks.spawn(supervise(state.clone(), receivers.launch));
    tasks.spawn(fetch_blobs(state, receivers.blob_jobs));
    handle
}

/// Starts the puller and the pusher of peer `id`.
fn launch(state: &AppState, id: PeerId) {
    let tasks = &state.sync.inner.tasks;
    tasks.spawn(pull_loop(state.clone(), id));
    tasks.spawn(push_loop(state.clone(), id));
}

/// Announces us, starts the peers, then follows arrivals: an announcement
/// from another node adds its URL, a post with a file we lack gets it fetched.
async fn supervise(state: AppState, mut launches: mpsc::UnboundedReceiver<PeerId>) {
    let sync = state.sync.clone();
    // Listen before reading the announcements, so none is missed in between.
    let mut arrivals = state.arrivals.subscribe();
    announce_self(&state).await;
    for url in &state.cfg.peers {
        let added = sync.table().add_configured(url);
        match added {
            Some(id) => launch(&state, id),
            None if normalize_peer_url(url).is_none() => {
                tracing::warn!("sync: ignoring peer {url:?}: not a plain http:// URL");
            }
            None => {}
        }
    }
    learn_announced(&state).await;

    loop {
        tokio::select! {
            got = arrivals.recv() => match got {
                Ok(applied) => on_arrival(&state, &applied).await,
                Err(RecvError::Lagged(_)) => {
                    // Missed some: look at the store instead.
                    learn_announced(&state).await;
                    let _ = sync.inner.blob_jobs.send(BlobJob::Sweep);
                }
                Err(RecvError::Closed) => return,
            },
            Some(id) = launches.recv() => launch(&state, id),
            () = state.shutdown.triggered() => return,
        }
    }
}

async fn on_arrival(state: &AppState, applied: &Applied) {
    match &applied.event.kind {
        EventKind::PeerAnnounced { node, .. } if *node != state.cfg.node => {
            learn_announced(state).await;
        }
        EventKind::PostCreated(post) => {
            if let Some(att) = &post.attachment {
                if !state.blobs.has(&att.sha256) {
                    let _ = state.sync.inner.blob_jobs.send(BlobJob::Want {
                        sha: att.sha256.clone(),
                        size: att.size,
                    });
                }
            }
        }
        _ => {}
    }
}

/// Adds every other node's newest announced URL (the store keeps the newest
/// per node, so an old announcement that arrives late changes nothing).
async fn learn_announced(state: &AppState) {
    let announced = match state.with_store(|s| s.announced_peers()).await {
        Ok(announced) => announced,
        Err(e) => {
            tracing::error!("sync: cannot read announced peers: {e}");
            return;
        }
    };
    for (node, url) in announced {
        let added = state.sync.table().add_announced(&node, &url);
        if let Some(id) = added {
            tracing::info!("sync: learned {node} at {url}");
            launch(state, id);
        }
    }
}

/// Commits `peer_announced {node, BEACON_URL}` unless that is already the
/// newest announcement for this node.
async fn announce_self(state: &AppState) {
    let Some(raw) = state.cfg.url.as_deref() else {
        return;
    };
    let Some(url) = normalize_peer_url(raw) else {
        tracing::warn!("sync: not announcing BEACON_URL {raw:?}: not a plain http:// URL");
        return;
    };
    let node = state.cfg.node.clone();
    let (ours, at) = (node.clone(), url.clone());
    let announced = state
        .commit_if(EventKind::PeerAnnounced { node, url }, move |s, _| {
            let current = s.announced_peers()?;
            Ok(!current.iter().any(|(n, u)| *n == ours && *u == at))
        })
        .await;
    match announced {
        Ok(Some(_)) => tracing::info!("sync: announced {raw}"),
        Ok(None) => {}
        Err(e) => tracing::error!("sync: cannot announce {raw}: {e}"),
    }
}

// ---------------------------------------------------------- peer tasks

/// What a peer's task needs from the table, taken once.
struct PeerTask {
    id: PeerId,
    cancel: CancellationToken,
    retry: watch::Receiver<()>,
    push_lock: Arc<AsyncMutex<()>>,
}

impl SyncHandle {
    fn peer_task(&self, id: PeerId) -> Option<PeerTask> {
        self.with_peer(id, |p| PeerTask {
            id,
            cancel: p.cancel.clone(),
            retry: p.retry.subscribe(),
            push_lock: Arc::clone(&p.push_lock),
        })
    }

    /// Waits until the peer may be tried (its backoff is over, or was cut
    /// short). `false` when the task should end instead.
    async fn wait_turn(&self, task: &mut PeerTask, shutdown: &Shutdown) -> bool {
        loop {
            task.retry.borrow_and_update();
            let Some(wait) = self.with_peer(task.id, |p| p.backoff.wait(Instant::now())) else {
                return false;
            };
            if task.cancel.is_cancelled() || shutdown.is_triggered() {
                return false;
            }
            if wait.is_zero() {
                return true;
            }
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                _ = task.retry.changed() => {}
                () = task.cancel.cancelled() => return false,
                () = shutdown.triggered() => return false,
            }
        }
    }
}

/// Sleeps for `wait`, unless the peer or the node goes first: `false` then.
async fn pause(task: &PeerTask, shutdown: &Shutdown, wait: Duration) -> bool {
    tokio::select! {
        () = tokio::time::sleep(wait) => true,
        () = task.cancel.cancelled() => false,
        () = shutdown.triggered() => false,
    }
}

/// The puller: long-polls the peer for what we lack and stores it.
async fn pull_loop(state: AppState, id: PeerId) {
    let sync = state.sync.clone();
    let shutdown = state.shutdown.clone();
    let Some(mut task) = sync.peer_task(id) else {
        return;
    };
    // Follow `more` at once (R22): only while pages bring something new.
    let mut follow = false;
    // Ours since this node started, as the peer holds them: the first pulls
    // ask for them again, a page at a time, so a seq this node wrote twice
    // (it lost commits the peer already had) shows up as a collision in the
    // store, which settles it. See `Store::opened_at`.
    let mut replay = Some(sync.inner.opened_at.clone());
    loop {
        if !sync.wait_turn(&mut task, &shutdown).await {
            return;
        }
        // `asked_with`: what we think it holds as we ask. An answer with less
        // means it lost events (see `Table::pulled`).
        let Some((url, since, asked_with)) =
            sync.with_peer(id, |p| (p.url().to_string(), p.since, p.their_have.clone()))
        else {
            return;
        };
        let wait = if follow { 0 } else { state.cfg.long_poll_secs };
        let mut after = sync.our_vv();
        if let Some((origin, from)) = &replay {
            if let Some(have) = after.get_mut(origin) {
                *have = (*have).min(*from);
            }
        }
        let started = Instant::now();
        let answer = tokio::select! {
            answer = pull(&sync.inner.client, &url, &after, wait, since) => answer,
            () = task.cancel.cancelled() => return,
            () = shutdown.triggered() => return,
        };
        let (node, page) = match answer {
            Ok(answer) => answer,
            Err(e) => {
                sync.failed(id, &url, &e);
                follow = false;
                continue;
            }
        };
        match sync.learned(id, &url, &node) {
            Learned::Same => sync.pulled(id, asked_with.as_ref(), page.have, page.head),
            // Another node's events are as good as any to store, but what it
            // holds says nothing of the peer.
            Learned::Other => {}
            Learned::Retry => {
                follow = false;
                continue;
            }
            Learned::Gone => return,
        }
        let replayed = replay.as_ref().and_then(|(origin, _)| {
            page.events
                .iter()
                .filter(|e| e.origin == *origin)
                .map(|e| e.seq)
                .max()
        });
        // The whole page in one transaction: one sync to disk, not one per
        // event, however far behind we are.
        let newly = match state.ingest_all(page.events).await {
            Ok(ingested) => {
                // An event no store can hold (a seq beyond i64): skipped.
                for (_, why) in &ingested.refused {
                    tracing::warn!("sync: skipping an event from {url}: {why}");
                }
                ingested.applied.len()
            }
            Err(e) => {
                // Our own store failed, not the peer: wait, then pull again.
                tracing::error!("sync: cannot store events from {url}: {e}");
                follow = false;
                if !pause(&task, &shutdown, state.cfg.sync_base_backoff).await {
                    return;
                }
                continue;
            }
        };
        if task.cancel.is_cancelled() || shutdown.is_triggered() {
            return;
        }
        // The replay goes on, at once, while a full page still brings ours.
        match (&mut replay, replayed) {
            (Some((_, from)), Some(top)) if page.more => *from = top,
            _ => replay = None,
        }
        follow = (page.more && newly > 0) || replay.is_some();
        if newly == 0 && wait > 0 {
            // A long poll that brought nothing new came back early (an echo
            // of what we already had): never let that turn into a hot loop.
            let left = IDLE_PULL_GAP.saturating_sub(started.elapsed());
            if !left.is_zero() && !pause(&task, &shutdown, left).await {
                return;
            }
        }
    }
}

/// How a pusher round ended.
enum Round {
    /// Nothing more to send now.
    Done,
    /// The URL was this node; try the peer's next one.
    Retry,
    /// The peer is gone.
    Gone,
}

/// The pusher: sends the peer what it lacks whenever something is written
/// here (and every 5 s), one round at a time.
async fn push_loop(state: AppState, id: PeerId) {
    let sync = state.sync.clone();
    let shutdown = state.shutdown.clone();
    let Some(mut task) = sync.peer_task(id) else {
        return;
    };
    let mut written = sync.inner.written.subscribe();
    loop {
        if !sync.wait_turn(&mut task, &shutdown).await {
            return;
        }
        // Whatever is written from here on wakes the next round.
        written.borrow_and_update();
        let push_lock = Arc::clone(&task.push_lock);
        let round = tokio::select! {
            round = async {
                let _turn = push_lock.lock_owned().await;
                push_round(&state, id).await
            } => round,
            () = task.cancel.cancelled() => return,
            () = shutdown.triggered() => return,
        };
        match round {
            Ok(Round::Done) => {}
            Ok(Round::Retry) => continue,
            Ok(Round::Gone) => return,
            Err((url, e)) => {
                sync.failed(id, &url, &e);
                continue;
            }
        }
        tokio::select! {
            _ = written.changed() => {}
            // What we knew of the peer changed (it was replaced, or came
            // back): look again now.
            _ = task.retry.changed() => {}
            () = tokio::time::sleep(PUSH_TICK) => {}
            () = task.cancel.cancelled() => return,
            () = shutdown.triggered() => return,
        }
    }
}

/// Sends peer `id` the events its version vector lacks, a batch at a time,
/// while it keeps taking them, and again from its answer when that holds
/// less than the view the batch was chosen from (it lost events we thought
/// it had). After the first push, another follows only an answer that holds
/// more, in all, than every earlier answer in the round, so the round ends
/// however the answers move, and a peer that takes nothing is not sent the
/// same batch again. A selection that holds nothing that arrived since the
/// last round, for a peer whose version vector has not changed, was sent
/// already (it lies above a gap the peer shares) and is skipped. An answer
/// from another node ([`Learned::Other`]) ends the round unmarked: the next
/// one tries again.
async fn push_round(state: &AppState, id: PeerId) -> Result<Round, (String, anyhow::Error)> {
    let sync = &state.sync;
    // The most the peer said it held, in all, in this round's answers.
    let mut most: Option<u64> = None;
    loop {
        let Some((url, have, mark)) = sync.with_peer(id, |p| {
            (
                p.url().to_string(),
                p.their_have.clone(),
                p.push_mark.clone(),
            )
        }) else {
            return Ok(Round::Gone);
        };
        let Some(have) = have else {
            // Never heard from it: ask what it has before sending anything.
            let (node, answer) = push(&sync.inner.client, &url, &[], PUSH_TIMEOUT)
                .await
                .map_err(|e| (url.clone(), e))?;
            match sync.learned(id, &url, &node) {
                Learned::Same => sync.pushed(id, &VersionVector::new(), &answer.have),
                Learned::Other => return Ok(Round::Done),
                Learned::Retry => return Ok(Round::Retry),
                Learned::Gone => return Ok(Round::Gone),
            }
            continue;
        };
        let after = have.clone();
        let selected = state
            .with_store(move |s| Ok((s.events_after_applied(&after, BATCH)?, s.arrival_head())))
            .await;
        let (page, head) = match selected {
            Ok(selected) => selected,
            Err(e) => {
                // Our store, not the peer: the next round tries again.
                tracing::error!("sync: cannot read events to push: {e}");
                return Ok(Round::Done);
            }
        };
        let news = match &mark {
            None => true,
            Some((seen, then)) => *then != have || page.iter().any(|a| a.arrival > *seen),
        };
        if page.is_empty() || !news {
            set_mark(sync, id, head, have);
            return Ok(Round::Done);
        }
        let full = page.len() >= BATCH;
        let events: Vec<Event> = page.into_iter().map(|a| a.event).collect();
        let (node, answer) = push(&sync.inner.client, &url, &events, PUSH_TIMEOUT)
            .await
            .map_err(|e| (url.clone(), e))?;
        match sync.learned(id, &url, &node) {
            Learned::Same => {}
            // Unmarked: what the peer took is not known.
            Learned::Other => return Ok(Round::Done),
            Learned::Retry => return Ok(Round::Retry),
            Learned::Gone => return Ok(Round::Gone),
        }
        sync.pushed(id, &have, &answer.have);
        let now = sync
            .with_peer(id, |p| p.their_have.clone().unwrap_or_default())
            .unwrap_or_default();
        // More to send: it took a full batch and there is more, or it holds
        // less than the view this batch came from (it lost events), and the
        // next is chosen from what it said.
        let more = (full || behind(&answer.have, &have)) && now != have;
        let held = total(&answer.have);
        let progress = most.is_none_or(|most| held > most);
        most = Some(most.map_or(held, |most| most.max(held)));
        if more && progress {
            continue;
        }
        set_mark(sync, id, head, now);
        return Ok(Round::Done);
    }
}

fn set_mark(sync: &SyncHandle, id: PeerId, head: i64, have: VersionVector) {
    if let Some(peer) = sync.table().get_mut(id) {
        peer.push_mark = Some((head, have));
    }
}

// ------------------------------------------------------------------ http

/// `GET /api/events` on the peer at `url`.
async fn pull(
    client: &PeerClient,
    url: &str,
    after: &VersionVector,
    wait: u64,
    since: i64,
) -> anyhow::Result<(String, EventsResponse)> {
    let mut query = Url::parse("http://query.invalid/")?;
    query
        .query_pairs_mut()
        .append_pair("after", &format_vv(after))
        .append_pair("wait", &wait.to_string())
        .append_pair("since", &since.to_string())
        .append_pair("limit", &BATCH.to_string());
    let path = format!("/api/events?{}", query.query().unwrap_or_default());
    let timeout = Duration::from_secs(wait) + PULL_SLACK;
    client.get_json(url, &path, timeout).await
}

/// `POST /api/events` to the peer at `url`.
async fn push(
    client: &PeerClient,
    url: &str,
    events: &[Event],
    timeout: Duration,
) -> anyhow::Result<(String, PushResponse)> {
    #[derive(Serialize)]
    struct Body<'a> {
        events: &'a [Event],
    }
    client
        .post_json(url, "/api/events", &Body { events }, timeout)
        .await
}

// ----------------------------------------------------------------- blobs

/// A file some post announced that is not here yet.
struct Wanted {
    /// The most bytes to accept: the announced size, or our limit if smaller.
    limit: u64,
    /// The wait after the next failed try.
    delay: Duration,
    due: tokio::time::Instant,
}

/// Fetches the files posts announced and this node lacks: as they are
/// announced, again with backoff (base, doubling, at most 60 s) while no
/// reachable peer has them, at once when a peer comes back, and from a sweep
/// of every post every 60 s.
async fn fetch_blobs(state: AppState, mut jobs: mpsc::UnboundedReceiver<BlobJob>) {
    let base = state.cfg.sync_base_backoff;
    let max = state.cfg.max_attachment_bytes();
    let shutdown = state.shutdown.clone();
    let mut wanted: BTreeMap<String, Wanted> = BTreeMap::new();
    // The first tick is at once: files missing from before a restart.
    let mut sweep = tokio::time::interval(BLOB_SWEEP);
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let want = |wanted: &mut BTreeMap<String, Wanted>, sha: String, size: u64| {
        if size > max {
            tracing::debug!("sync: not fetching {sha}: {size} bytes is over our limit");
            return;
        }
        if state.blobs.has(&sha) {
            return;
        }
        let entry = wanted.entry(sha).or_insert(Wanted {
            limit: size,
            delay: base,
            due: tokio::time::Instant::now(),
        });
        entry.limit = entry.limit.max(size);
    };

    loop {
        let now = tokio::time::Instant::now();
        let due: Vec<(String, u64)> = wanted
            .iter()
            .filter(|(_, w)| w.due <= now)
            .map(|(sha, w)| (sha.clone(), w.limit))
            .collect();
        for (sha, limit) in due {
            let fetched = tokio::select! {
                fetched = fetch_blob(&state, &sha, limit) => fetched,
                () = shutdown.triggered() => return,
            };
            if fetched {
                wanted.remove(&sha);
            } else if let Some(w) = wanted.get_mut(&sha) {
                w.due = tokio::time::Instant::now() + w.delay;
                w.delay = (w.delay * 2).min(BLOB_SWEEP);
            }
        }
        let next_due = wanted.values().map(|w| w.due).min();
        tokio::select! {
            job = jobs.recv() => match job {
                None => return,
                Some(BlobJob::Want { sha, size }) => want(&mut wanted, sha, size),
                Some(BlobJob::RetryAll) => {
                    let now = tokio::time::Instant::now();
                    for w in wanted.values_mut() {
                        w.due = now;
                        w.delay = base;
                    }
                }
                Some(BlobJob::Sweep) => sweep.reset_immediately(),
            },
            _ = sweep.tick() => match state.with_store(|s| s.attachments()).await {
                Ok(all) => {
                    for (sha, size) in all {
                        if !wanted.contains_key(&sha) {
                            want(&mut wanted, sha, size);
                        }
                    }
                }
                Err(e) => tracing::error!("sync: cannot list attachments: {e}"),
            },
            () = sleep_until_some(next_due) => {}
            () = shutdown.triggered() => return,
        }
    }
}

async fn sleep_until_some(when: Option<tokio::time::Instant>) {
    match when {
        Some(when) => tokio::time::sleep_until(when).await,
        None => std::future::pending().await,
    }
}

/// Tries every reachable peer for file `sha`, reading at most `limit` bytes.
async fn fetch_blob(state: &AppState, sha: &str, limit: u64) -> bool {
    if state.blobs.has(sha) {
        return true;
    }
    let sync = &state.sync;
    let urls: Vec<String> = sync
        .table()
        .peers
        .iter()
        .filter(|p| p.reachable)
        .map(|p| p.url().to_string())
        .collect();
    for url in urls {
        match download(state, &url, sha, limit).await {
            Ok(()) => {
                tracing::debug!("sync: fetched {sha} from {url}");
                return true;
            }
            Err(e) => tracing::debug!("sync: no {sha} from {url}: {e:#}"),
        }
    }
    false
}

/// Fetches file `sha` from the peer at `url` into the blob store, reading
/// at most `limit` bytes.
async fn download(state: &AppState, url: &str, sha: &str, limit: u64) -> anyhow::Result<()> {
    let path = format!("/api/files/{sha}");
    let client = &state.sync.inner.client;
    let chunks = client.download(url, &path, PUSH_TIMEOUT, limit).await?;
    state.blobs.put_verified(sha, chunks, limit).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_the_base_up_to_two_minutes() {
        let mut b = Backoff::new(BASE_BACKOFF);
        let mut now = Instant::now();
        let mut waits = Vec::new();
        for _ in 0..8 {
            assert!(b.failed(now));
            let wait = b.wait(now);
            waits.push(wait.as_secs());
            now += wait;
        }
        assert_eq!(waits, [5, 10, 20, 40, 80, 120, 120, 120]);
        // Success starts over.
        b.succeeded();
        assert_eq!(b.wait(now), Duration::ZERO);
        assert!(b.failed(now));
        assert_eq!(b.wait(now), Duration::from_secs(5));
    }

    #[test]
    fn failures_during_a_wait_count_once() {
        let mut b = Backoff::new(BASE_BACKOFF);
        let start = Instant::now();
        assert!(b.failed(start));
        // The other task's request, sent before, fails a moment later.
        assert!(!b.failed(start + Duration::from_millis(10)));
        assert_eq!(b.wait(start), Duration::from_secs(5));
        // The retry fails: 10 s, not 20.
        assert!(b.failed(start + Duration::from_secs(5)));
        assert_eq!(
            b.wait(start + Duration::from_secs(5)),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn a_wait_is_cut_short_only_after_the_base() {
        let mut b = Backoff::new(BASE_BACKOFF);
        let start = Instant::now();
        b.failed(start);
        b.failed(start + Duration::from_secs(5));
        // 1 s into a 10 s wait: too soon.
        assert!(!b.cut_short(start + Duration::from_secs(6)));
        // 5 s in: now, and the next failure still doubles.
        let later = start + Duration::from_secs(10);
        assert!(b.cut_short(later));
        assert_eq!(b.wait(later), Duration::ZERO);
        assert!(b.failed(later));
        assert_eq!(b.wait(later), Duration::from_secs(20));
        // Not waiting: nothing to cut.
        b.succeeded();
        assert!(!b.cut_short(later));
    }

    fn table(node: &str, own: Option<&str>) -> Table {
        Table {
            node: node.into(),
            own_url: own.map(String::from),
            base: BASE_BACKOFF,
            next_id: 0,
            peers: Vec::new(),
            selves: BTreeSet::new(),
        }
    }

    fn urls(t: &Table) -> Vec<(Option<String>, Vec<String>)> {
        t.peers
            .iter()
            .map(|p| {
                (
                    p.node.clone(),
                    p.urls.iter().map(|c| c.url.clone()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn configured_urls_are_normalized_deduped_and_never_ourselves() {
        let mut t = table("pc", Some("http://soybean-wsl:7700"));
        assert!(t.add_configured("http://Soybean-WSL:7700/").is_none());
        assert!(t.add_configured("http://mac:7700").is_some());
        assert!(t.add_configured("http://mac:7700/").is_none());
        assert!(t.add_configured(" HTTP://MAC:7700 ").is_none());
        assert!(t.add_configured("mac:7700").is_none());
        assert_eq!(urls(&t), [(None, vec!["http://mac:7700".to_string()])]);
    }

    #[test]
    fn a_url_that_answers_as_us_is_dropped_for_good() {
        let mut t = table("pc", None);
        let id = t.add_configured("http://localhost:7700").unwrap();
        assert_eq!(t.learned(id, "http://localhost:7700", "pc"), Learned::Gone);
        assert!(t.peers.is_empty());
        assert!(t.add_configured("http://localhost:7700").is_none());
        assert!(t.add_announced("mac", "http://localhost:7700").is_none());
    }

    #[test]
    fn urls_of_one_node_become_one_peer() {
        let mut t = table("pc", None);
        let first = t.add_configured("http://mac:7700").unwrap();
        let second = t.add_configured("http://100.64.0.2:7700").unwrap();
        assert_eq!(t.learned(first, "http://mac:7700", "mac"), Learned::Same);
        assert_eq!(
            t.learned(second, "http://100.64.0.2:7700", "mac"),
            Learned::Gone
        );
        assert_eq!(
            urls(&t),
            [(
                Some("mac".into()),
                vec!["http://mac:7700".into(), "http://100.64.0.2:7700".into()]
            )]
        );
        // It now uses the URL that just answered.
        assert_eq!(t.peers[0].url(), "http://100.64.0.2:7700");
        // A failure moves it to the other URL.
        t.failed(first, "http://100.64.0.2:7700");
        assert_eq!(t.peers[0].url(), "http://mac:7700");
        // An announcement of a URL it has adds nothing.
        assert!(t.add_announced("mac", "http://mac:7700").is_none());
        assert_eq!(t.peers.len(), 1);
    }

    #[test]
    fn a_new_announcement_replaces_the_old_one() {
        let mut t = table("pc", None);
        assert!(
            t.add_announced("pc", "http://pc:7700").is_none(),
            "ourselves"
        );
        let id = t.add_announced("mac", "http://old-mac:7700").unwrap();
        assert!(t.add_announced("mac", "http://new-mac:7700").is_none());
        assert_eq!(
            urls(&t),
            [(Some("mac".into()), vec!["http://new-mac:7700".into()])]
        );
        // A configured URL stays when the node announces another.
        t.add_configured("http://mac-configured:7700");
        t.learned(t.peers[1].id, "http://mac-configured:7700", "mac");
        assert!(t.add_announced("mac", "http://newest-mac:7700").is_none());
        let list = urls(&t);
        assert_eq!(list.len(), 1, "{list:?}");
        assert_eq!(
            list[0].1,
            ["http://mac-configured:7700", "http://newest-mac:7700"]
        );
        assert!(t.get(id).is_some());
    }

    #[test]
    fn a_url_answering_as_another_node_forgets_what_the_old_one_had() {
        let mut t = table("pc", None);
        let id = t.add_configured("http://mac:7700").unwrap();
        assert_eq!(t.learned(id, "http://mac:7700", "mac"), Learned::Same);
        {
            let peer = t.get_mut(id).unwrap();
            peer.their_have = Some([("pc~00000000".to_string(), 9)].into());
            peer.their_head = Some(40);
            peer.since = 40;
            peer.push_mark = Some((12, VersionVector::new()));
        }
        let mut woken = t.get(id).unwrap().retry.subscribe();
        woken.borrow_and_update();
        assert_eq!(t.learned(id, "http://mac:7700", "mac-mini"), Learned::Same);
        let peer = t.get(id).unwrap();
        assert_eq!(peer.node.as_deref(), Some("mac-mini"));
        assert_eq!(peer.their_have, None);
        assert_eq!(peer.their_head, None);
        assert_eq!(peer.since, 0);
        assert_eq!(peer.push_mark, None);
        // Its tasks hear of it, so the pusher starts over at once.
        assert!(woken.has_changed().unwrap());
    }

    #[test]
    fn peers_report_what_they_lack() {
        let mut t = table("pc", None);
        let id = t.add_configured("http://mac:7700").unwrap();
        let ours: VersionVector = [("pc~00000000".to_string(), 5)].into();
        let json = t.get(id).unwrap().json(&ours);
        assert_eq!(json.queued, 5);
        assert!(!json.reachable);
        assert_eq!(json.node, None);
        t.learned(id, "http://mac:7700", "mac");
        t.answered(id);
        t.get_mut(id).unwrap().their_have = Some([("pc~00000000".to_string(), 3)].into());
        let json = t.get(id).unwrap().json(&ours);
        assert_eq!(json.queued, 2);
        assert!(json.reachable);
        assert_eq!(json.node.as_deref(), Some("mac"));
        assert!(json.last_sync.is_some());
    }

    fn vv(entries: &[(&str, u64)]) -> VersionVector {
        entries.iter().map(|&(o, s)| (o.to_string(), s)).collect()
    }

    /// A peer `mac` whose view is `have`.
    fn known(have: VersionVector) -> (Table, PeerId) {
        let mut t = table("pc", None);
        let id = t.add_configured("http://mac:7700").unwrap();
        t.learned(id, "http://mac:7700", "mac");
        t.get_mut(id).unwrap().their_have = Some(have);
        (t, id)
    }

    #[test]
    fn a_push_answer_with_less_than_the_batch_was_chosen_from_is_believed() {
        let view = vv(&[("pc~00000000", 9), ("mac~00000000", 4)]);
        let (mut t, id) = known(view.clone());
        // It lost pc's events and the old mac's, and has some of a new db.
        let answer = vv(&[("pc~00000000", 2), ("mac~11111111", 3)]);
        t.pushed(id, &view, &answer);
        let peer = t.get(id).unwrap();
        assert_eq!(peer.their_have.as_ref(), Some(&answer));
        assert_eq!(peer.json(&view).queued, 7 + 4);

        // An answer that holds what the batch was chosen from is no loss,
        // even if the view went past it meanwhile (it heard more since).
        let (mut t, id) = known(vv(&[("pc~00000000", 9)]));
        t.pushed(id, &vv(&[("pc~00000000", 5)]), &vv(&[("pc~00000000", 6)]));
        let peer = t.get(id).unwrap();
        assert_eq!(peer.their_have, Some(vv(&[("pc~00000000", 9)])));
    }

    #[test]
    fn a_pull_answer_with_less_than_it_was_asked_with_is_believed_once() {
        let asked = vv(&[("pc~00000000", 4)]);
        let (mut t, id) = known(asked.clone());
        t.get_mut(id).unwrap().push_mark = Some((7, asked.clone()));
        let mut woken = t.get(id).unwrap().retry.subscribe();
        woken.borrow_and_update();
        // Its head did not go back, but it holds less than it did.
        t.pulled(id, Some(&asked), vv(&[("pc~00000000", 3)]), 10);
        let peer = t.get(id).unwrap();
        assert_eq!(peer.their_have, Some(vv(&[("pc~00000000", 3)])));
        assert_eq!(peer.push_mark, None);
        assert!(woken.has_changed().unwrap(), "the pusher hears of it");

        // The same answer again (its pushes said more meanwhile) lowers
        // nothing a second time.
        t.get_mut(id).unwrap().their_have = Some(asked.clone());
        woken.borrow_and_update();
        t.pulled(id, Some(&asked), vv(&[("pc~00000000", 3)]), 10);
        let peer = t.get(id).unwrap();
        assert_eq!(peer.their_have, Some(asked.clone()));
        assert!(!woken.has_changed().unwrap());
    }

    #[test]
    fn a_pull_answer_raises_the_view_to_it_and_no_further() {
        // Asked when it held 2; a push it took meanwhile raised the view to
        // 5; the answer, given before that push, says 3. Overtaken, not lost.
        let (mut t, id) = known(vv(&[("pc~00000000", 5)]));
        let asked = vv(&[("pc~00000000", 2)]);
        t.pulled(id, Some(&asked), vv(&[("pc~00000000", 3)]), 10);
        assert_eq!(
            t.get(id).unwrap().their_have,
            Some(vv(&[("pc~00000000", 5)]))
        );
        // An answer that holds more raises it to what it says.
        t.pulled(
            id,
            Some(&asked),
            vv(&[("pc~00000000", 6), ("mac~00000000", 2)]),
            11,
        );
        assert_eq!(
            t.get(id).unwrap().their_have,
            Some(vv(&[("pc~00000000", 6), ("mac~00000000", 2)]))
        );
    }

    #[test]
    fn a_url_that_changes_names_again_soon_does_not_start_over_again() {
        let (mut t, id) = known(vv(&[("pc~00000000", 9)]));
        assert_eq!(t.learned(id, "http://mac:7700", "mini"), Learned::Same);
        assert_eq!(t.get(id).unwrap().their_have, None, "started over");
        t.get_mut(id).unwrap().their_have = Some(vv(&[("pc~00000000", 9)]));
        let mut woken = t.get(id).unwrap().retry.subscribe();
        woken.borrow_and_update();
        // Back to the first name at once: two nodes at one address. That
        // answer is the other node's, not the peer's; the peer keeps its
        // name and what it knows, and its tasks are not woken.
        assert_eq!(t.learned(id, "http://mac:7700", "mac"), Learned::Other);
        assert_eq!(t.learned(id, "http://mac:7700", "mini"), Learned::Same);
        let peer = t.get(id).unwrap();
        assert_eq!(peer.node.as_deref(), Some("mini"));
        assert_eq!(peer.their_have, Some(vv(&[("pc~00000000", 9)])));
        assert!(!woken.has_changed().unwrap());
        // Once the gap is over, it may start over again.
        let long_ago = Instant::now().checked_sub(RENAME_GAP + Duration::from_secs(1));
        let Some(long_ago) = long_ago else {
            return; // the clock started too recently to go back that far
        };
        t.get_mut(id).unwrap().renamed_at = Some(long_ago);
        assert_eq!(t.learned(id, "http://mac:7700", "mac"), Learned::Same);
        let peer = t.get(id).unwrap();
        assert_eq!(peer.node.as_deref(), Some("mac"));
        assert_eq!(peer.their_have, None);
        assert!(woken.has_changed().unwrap());
    }
}
