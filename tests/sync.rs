//! The sync engine end to end: real nodes on loopback ports, syncing over
//! HTTP. Every wait is bounded; a test that cannot converge fails instead of
//! hanging.

mod common;

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use beacon::api::{Delivery, PeerJson, PostCreated, StatusJson, StreamEvent};
use beacon::config::Config;
use beacon::model::{AuthorKind, EventKind, NewPost, VersionVector};
use beacon::sync::normalize_peer_url;
use common::*;
use reqwest::multipart::{Form, Part};
use reqwest::Response;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// ------------------------------------------------------------------ helpers

/// Retries after 100 ms instead of 5 s and long-polls for 2 s instead of 25,
/// so a test sees a peer come and go quickly.
fn quick(cfg: &mut Config) {
    cfg.sync_base_backoff = Duration::from_millis(100);
    cfg.long_poll_secs = 2;
}

/// [`quick`], announcing `url` as the node's own (`BEACON_URL`).
fn quick_at(url: &str) -> impl FnOnce(&mut Config) + '_ {
    move |cfg| {
        quick(cfg);
        cfg.url = Some(url.to_string());
    }
}

/// Polls `check` every 25 ms until it holds; fails the test after `secs`.
async fn eventually<F, Fut>(secs: u64, what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if check().await {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "gave up after {secs}s waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Bounds something that must finish.
async fn within<T>(secs: u64, what: &str, fut: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(secs), fut)
        .await
        .unwrap_or_else(|_| panic!("timed out after {secs}s waiting for {what}"))
}

async fn ok_json(resp: Response) -> Value {
    let status = resp.status();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "{body}");
    body
}

/// Posts `body` to `channel` (created if missing) as `<node>-agent`.
async fn post(node: &TestNode, channel: &str, body: &str) -> PostCreated {
    let resp = node
        .post(&format!("/api/channels/{channel}/posts"))
        .json(&json!({
            "body": body,
            "author": format!("{}-agent", node.name),
            "author_kind": "agent",
            "client": "claude-code",
            "create": true,
        }))
        .send()
        .await
        .unwrap();
    serde_json::from_value(ok_json(resp).await).unwrap()
}

/// Posts `body` with a file attached, as the multipart form.
async fn upload(node: &TestNode, channel: &str, body: &str, bytes: Vec<u8>) -> PostCreated {
    let meta = json!({
        "body": body,
        "author": format!("{}-agent", node.name),
        "author_kind": "agent",
        "create": true,
    });
    let form = Form::new()
        .part(
            "meta",
            Part::text(meta.to_string())
                .mime_str("application/json")
                .unwrap(),
        )
        .part("file", Part::bytes(bytes).file_name("build.log"));
    let resp = node
        .post(&format!("/api/channels/{channel}/posts"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    serde_json::from_value(ok_json(resp).await).unwrap()
}

/// What a node shows in `channel`, as (id, body) pairs; empty if the
/// channel is not there (yet).
async fn shown(node: &TestNode, channel: &str) -> Vec<(String, String)> {
    let resp = node
        .get(&format!("/api/channels/{channel}/posts?limit=500"))
        .send()
        .await
        .unwrap();
    if resp.status() == 404 {
        return Vec::new();
    }
    let body = ok_json(resp).await;
    body["posts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["id"].as_str().unwrap().to_string(),
                p["body"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

async fn version_vector(node: &TestNode) -> VersionVector {
    let status: StatusJson =
        serde_json::from_value(ok_json(node.get("/api/status").send().await.unwrap()).await)
            .unwrap();
    status.have
}

async fn peers(node: &TestNode) -> Vec<PeerJson> {
    let body = ok_json(node.get("/api/peers").send().await.unwrap()).await;
    assert_eq!(body["v"], 1);
    serde_json::from_value(body["peers"].clone()).unwrap()
}

/// The peer that `node` knows by `name`, if any.
async fn peer(node: &TestNode, name: &str) -> Option<PeerJson> {
    peers(node)
        .await
        .into_iter()
        .find(|p| p.node.as_deref() == Some(name))
}

async fn reaches(node: &TestNode, name: &str) -> bool {
    peer(node, name).await.is_some_and(|p| p.reachable)
}

/// Requests `node` received from the sync engine of `from`.
fn requests_from(node: &TestNode, from: &str) -> u64 {
    node.state().peer_requests.from(from)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// The bytes `node` serves for `sha`, or `None` while it does not have them.
async fn file(node: &TestNode, sha: &str) -> Option<Vec<u8>> {
    let resp = node.get(&format!("/api/files/{sha}")).send().await.unwrap();
    if resp.status() != 200 {
        return None;
    }
    Some(resp.bytes().await.unwrap().to_vec())
}

/// Three nodes in a line: A ↔ B ↔ C. A and C are not peered, and no node
/// announces itself, so they never learn about each other.
async fn line() -> (TestNode, TestNode, TestNode) {
    let (ra, rb, rc) = (reserve(), reserve(), reserve());
    let (ua, ub, uc) = (ra.url.clone(), rb.url.clone(), rc.url.clone());
    let a = ra.start("a", &[ub.as_str()], quick).await;
    let b = rb.start("b", &[ua.as_str(), uc.as_str()], quick).await;
    let c = rc.start("c", &[ub.as_str()], quick).await;
    eventually(5, "the line to connect", || async {
        reaches(&a, "b").await
            && reaches(&b, "a").await
            && reaches(&b, "c").await
            && reaches(&c, "b").await
    })
    .await;
    (a, b, c)
}

/// Two nodes peered with each other.
async fn pair() -> (TestNode, TestNode) {
    let (ra, rb) = (reserve(), reserve());
    let (ua, ub) = (ra.url.clone(), rb.url.clone());
    let a = ra.start("a", &[ub.as_str()], quick).await;
    let b = rb.start("b", &[ua.as_str()], quick).await;
    eventually(5, "the pair to connect", || async {
        reaches(&a, "b").await && reaches(&b, "a").await
    })
    .await;
    (a, b)
}

/// A reader for `GET /api/stream`.
struct Sse {
    resp: Response,
    buf: Vec<u8>,
}

impl Sse {
    async fn open(node: &TestNode, query: &str) -> Sse {
        let resp = within(
            5,
            "the stream",
            node.get(&format!("/api/stream{query}")).send(),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), 200);
        Sse {
            resp,
            buf: Vec::new(),
        }
    }

    /// The next event (keep-alive comments skipped), or `None` if nothing
    /// came by `deadline`.
    async fn next_before(&mut self, deadline: Instant) -> Option<StreamEvent> {
        loop {
            if let Some(end) = self.buf.windows(2).position(|w| w == b"\n\n") {
                let frame = String::from_utf8(self.buf[..end].to_vec()).unwrap();
                self.buf.drain(..end + 2);
                if frame.lines().all(|l| l.starts_with(':')) {
                    continue;
                }
                let name = frame
                    .lines()
                    .find_map(|l| l.strip_prefix("event:"))
                    .unwrap()
                    .trim()
                    .to_string();
                let data: Vec<&str> = frame
                    .lines()
                    .filter_map(|l| l.strip_prefix("data:"))
                    .map(|l| l.strip_prefix(' ').unwrap_or(l))
                    .collect();
                return Some(StreamEvent::from_sse(&name, &data.join("\n")).unwrap());
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            match tokio::time::timeout(left, self.resp.chunk()).await {
                Ok(Ok(Some(bytes))) => self.buf.extend_from_slice(&bytes),
                _ => return None,
            }
        }
    }

    /// Reads events until one satisfies `want`; fails after `secs`.
    async fn until(&mut self, secs: u64, what: &str, want: impl Fn(&StreamEvent) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while let Some(event) = self.next_before(deadline).await {
            if want(&event) {
                return;
            }
        }
        panic!("no {what} on the stream within {secs}s");
    }
}

// ------------------------------------------------------------ convergence

#[tokio::test(flavor = "multi_thread")]
async fn converges_in_a_line() {
    let (a, b, c) = line().await;
    // A and C only know B.
    let names =
        |list: Vec<PeerJson>| -> Vec<Option<String>> { list.into_iter().map(|p| p.node).collect() };
    assert_eq!(names(peers(&a).await), [Some("b".to_string())]);
    assert_eq!(names(peers(&c).await), [Some("b".to_string())]);

    post(&a, "beacon", "from a").await;
    post(&c, "beacon", "from c").await;
    let started = Instant::now();
    eventually(3, "all three to show the same board", || async {
        let (sa, sb, sc) = (
            shown(&a, "beacon").await,
            shown(&b, "beacon").await,
            shown(&c, "beacon").await,
        );
        let (va, vb, vc) = (
            version_vector(&a).await,
            version_vector(&b).await,
            version_vector(&c).await,
        );
        sa.len() == 2 && sa == sb && sb == sc && va == vb && vb == vc
    })
    .await;
    assert!(started.elapsed() < Duration::from_secs(3));
    let bodies: Vec<String> = shown(&b, "beacon").await.into_iter().map(|p| p.1).collect();
    assert!(bodies.contains(&"from a".to_string()), "{bodies:?}");
    assert!(bodies.contains(&"from c".to_string()), "{bodies:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn push_delivers_under_a_second() {
    let (a, b) = pair().await;
    let mut stream = Sse::open(&b, "").await;

    let started = Instant::now();
    let created = post(&a, "beacon", "ping").await;
    assert_eq!(
        created.delivery,
        [Delivery {
            node: "b".into(),
            delivered: true
        }]
    );
    stream
        .until(
            1,
            "the post",
            |e| matches!(e, StreamEvent::Post(p) if p.body == "ping"),
        )
        .await;
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_peer_queues_and_catches_up() {
    let (a, b, c) = line().await;
    let away = c.stop().await;

    let mut ids = Vec::new();
    for i in 0..3 {
        let created = post(&b, "beacon", &format!("while c is away {i}")).await;
        ids.push(created.post.id.clone());
        let mut delivery = created.delivery.clone();
        delivery.sort_by(|x, y| x.node.cmp(&y.node));
        assert_eq!(
            delivery,
            [
                Delivery {
                    node: "a".into(),
                    delivered: true
                },
                Delivery {
                    node: "c".into(),
                    delivered: false
                },
            ],
            "post {i}"
        );
    }
    let gone = peer(&b, "c").await.expect("b still lists c");
    assert!(!gone.reachable, "{gone:?}");
    assert!(gone.queued >= 3, "{gone:?}");
    assert!(gone.last_sync.is_some(), "{gone:?}");
    // A got them all along.
    assert_eq!(shown(&a, "beacon").await.len(), 3);

    let c = away.restart().await;
    eventually(10, "c to catch up and b to see it back", || async {
        let have: Vec<String> = shown(&c, "beacon").await.into_iter().map(|p| p.0).collect();
        let back = peer(&b, "c").await.unwrap();
        ids.iter().all(|id| have.contains(id)) && back.reachable && back.queued == 0
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn attachments_relay_through_middle() {
    let (a, _b, c) = line().await;
    let bytes = pseudo_random(300_000, 7);
    let sha = sha256_hex(&bytes);
    let created = upload(&a, "beacon", "the log", bytes.clone()).await;
    assert_eq!(created.post.attachment.as_ref().unwrap().sha256, sha);

    eventually(5, "c to serve the file", || async {
        file(&c, &sha).await.as_deref() == Some(&bytes[..])
    })
    .await;
    // And C's post now says the bytes are there.
    let row = ok_json(
        c.get(&format!("/api/posts/{}", created.post.id))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(row["attachment"]["available"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn attachments_over_the_limit_are_not_fetched() {
    let (ra, rb) = (reserve(), reserve());
    let (ua, ub) = (ra.url.clone(), rb.url.clone());
    let a = ra.start("a", &[ub.as_str()], quick).await;
    let b = rb
        .start("b", &[ua.as_str()], |cfg| {
            quick(cfg);
            cfg.max_attachment_mb = 1;
        })
        .await;
    eventually(5, "the pair to connect", || async {
        reaches(&a, "b").await && reaches(&b, "a").await
    })
    .await;

    let big = pseudo_random(1024 * 1024 + 1, 3);
    let small = pseudo_random(1000, 4);
    let big_post = upload(&a, "beacon", "too big for b", big.clone()).await;
    upload(&a, "beacon", "fits", small.clone()).await;
    // The small file arrives; the big one, sent after it, never does.
    eventually(5, "b to fetch the small file", || async {
        file(&b, &sha256_hex(&small)).await.is_some()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(file(&b, &sha256_hex(&big)).await, None);
    let row = ok_json(
        b.get(&format!("/api/posts/{}", big_post.post.id))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(row["attachment"]["available"], false);
    assert_eq!(row["body"], "too big for b");
}

// --------------------------------------------------------------- discovery

#[tokio::test(flavor = "multi_thread")]
async fn discovery_via_announcement() {
    let (ra, rb, rd) = (reserve(), reserve(), reserve());
    let (ua, ub, ud) = (ra.url.clone(), rb.url.clone(), rd.url.clone());
    let a = ra.start("a", &[ub.as_str()], quick_at(&ua)).await;
    let b = rb.start("b", &[ua.as_str()], quick_at(&ub)).await;
    let d = rd.start("d", &[ub.as_str()], quick_at(&ud)).await;

    eventually(5, "d to learn a from its announcement", || async {
        let list = peers(&d).await;
        list.iter()
            .any(|p| p.url == ua && p.node.as_deref() == Some("a") && p.reachable)
    })
    .await;
    // And a learns d the same way; nobody lists anyone twice or themselves.
    eventually(5, "a to learn d", || async {
        peers(&a).await.iter().any(|p| p.url == ud && p.reachable)
    })
    .await;
    for (node, others) in [(&a, ["b", "d"]), (&b, ["a", "d"]), (&d, ["a", "b"])] {
        eventually(5, "every node to list the other two once", || async {
            let mut names: Vec<String> = peers(node)
                .await
                .into_iter()
                .filter_map(|p| p.node)
                .collect();
            names.sort();
            names == others
        })
        .await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_announces_its_url_once_and_again_when_it_changes() {
    let rn = reserve();
    let un = rn.url.clone();
    // Written loosely: announced in normal form.
    let n = rn
        .start("n", &[], |cfg| {
            quick(cfg);
            cfg.url = Some(format!(" {}/ ", un.replace("http", "HTTP")));
        })
        .await;
    let announced = |events: &Value| -> Vec<String> {
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["kind"] == "peer_announced")
            .map(|e| {
                assert_eq!(e["data"]["node"], "n");
                e["data"]["url"].as_str().unwrap().to_string()
            })
            .collect()
    };
    let all = |node: &TestNode| {
        let get = node.get("/api/events");
        async move { ok_json(get.send().await.unwrap()).await }
    };
    assert_eq!(announced(&all(&n).await), std::slice::from_ref(&un));

    // The same URL after a restart: nothing new.
    let stopped = n.stop().await;
    let n = stopped.restart().await;
    assert_eq!(announced(&all(&n).await), std::slice::from_ref(&un));

    // A new one: announced again.
    let mut stopped = n.stop().await;
    stopped.cfg.url = Some("http://n.example:7700".into());
    let n = stopped.restart().await;
    assert_eq!(
        announced(&all(&n).await),
        [un.clone(), "http://n.example:7700".to_string()]
    );
}

// ------------------------------------------------------ self and duplicates

#[tokio::test(flavor = "multi_thread")]
async fn peer_list_dedupes_and_skips_self() {
    let (rn, rb) = (reserve(), reserve());
    let (un, ub) = (rn.url.clone(), rb.url.clone());
    let b = rb.start("b", &[], quick).await;
    let listed = [format!("{un}/"), ub.clone(), format!("{ub}/")];
    let listed: Vec<&str> = listed.iter().map(String::as_str).collect();
    let n = rn.start("n", &listed, quick_at(&un)).await;

    eventually(5, "n to reach b", || async { reaches(&n, "b").await }).await;
    let list = peers(&n).await;
    assert_eq!(list.len(), 1, "{list:?}");
    assert_eq!(list[0].url, ub);
    assert_eq!(list[0].node.as_deref(), Some("b"));

    // Long enough for a puller and a pusher to have run several rounds.
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(requests_from(&n, "n"), 0, "n must never call itself");
    assert!(requests_from(&b, "n") > 0, "n does sync with b");
    assert_eq!(peers(&n).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn own_address_under_another_name_is_dropped_after_first_contact() {
    let (rn, rb) = (reserve(), reserve());
    let ub = rb.url.clone();
    // `localhost` is this very node, but nothing in the config says so.
    let alias = rn.url.replace("127.0.0.1", "localhost");
    let b = rb.start("b", &[], quick).await;
    let n = rn.start("n", &[alias.as_str(), ub.as_str()], quick).await;

    eventually(5, "n to drop itself and keep b", || async {
        let list = peers(&n).await;
        list.len() == 1 && list[0].node.as_deref() == Some("b") && list[0].reachable
    })
    .await;
    let first = requests_from(&n, "n");
    assert!((1..=2).contains(&first), "{first} requests to itself");
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(requests_from(&n, "n"), first, "no more requests to itself");
    assert!(requests_from(&b, "n") > 0);
    // A post reports b only.
    let created = post(&n, "beacon", "hello").await;
    assert_eq!(
        created.delivery,
        [Delivery {
            node: "b".into(),
            delivered: true
        }]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn two_urls_for_one_node_are_one_peer() {
    let (rn, rb) = (reserve(), reserve());
    let ub = rb.url.clone();
    let alias = ub.replace("127.0.0.1", "localhost");
    let b = rb.start("b", &[], quick).await;
    let n = rn.start("n", &[ub.as_str(), alias.as_str()], quick).await;

    eventually(5, "n to see b once", || async {
        let list = peers(&n).await;
        list.len() == 1 && list[0].node.as_deref() == Some("b") && list[0].reachable
    })
    .await;
    let created = post(&n, "beacon", "once").await;
    assert_eq!(
        created.delivery,
        [Delivery {
            node: "b".into(),
            delivered: true
        }]
    );
    eventually(2, "b to have the post", || async {
        shown(&b, "beacon").await.len() == 1
    })
    .await;
}

// ------------------------------------------------------------- reachability

#[tokio::test(flavor = "multi_thread")]
async fn stream_reports_peer_reachability_changes() {
    let (a, b) = pair().await;
    let mut stream = Sse::open(&a, "").await;
    let b_is = |reachable: bool| {
        move |e: &StreamEvent| match e {
            StreamEvent::Peers(list) => list
                .iter()
                .any(|p| p.node.as_deref() == Some("b") && p.reachable == reachable),
            _ => false,
        }
    };

    let away = b.stop().await;
    stream.until(5, "b going offline", b_is(false)).await;
    let _b = away.restart().await;
    stream.until(10, "b coming back", b_is(true)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn peers_endpoint_lists_unreachable_peers_with_what_they_lack() {
    // Nothing listens on port 1.
    let n = spawn_node_with("n", &["http://127.0.0.1:1/"], quick).await;
    post(&n, "beacon", "nobody hears this").await;
    eventually(5, "the peer to be marked unreachable", || async {
        let list = peers(&n).await;
        list.len() == 1 && !list[0].reachable
    })
    .await;
    let body = ok_json(n.get("/api/peers").send().await.unwrap()).await;
    assert_eq!(body["v"], 1);
    let peer = &body["peers"][0];
    assert_eq!(peer["url"], "http://127.0.0.1:1");
    assert_eq!(peer["node"], Value::Null);
    assert_eq!(peer["reachable"], false);
    assert_eq!(peer["last_sync"], Value::Null);
    // `general`, `beacon` and the post: three events it has never been sent.
    assert_eq!(peer["queued"], 3);
    // Status carries the same list.
    let status = ok_json(n.get("/api/status").send().await.unwrap()).await;
    assert_eq!(status["peers"], body["peers"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn delivery_names_an_unknown_peer_by_its_url() {
    let n = spawn_node_with("n", &["http://127.0.0.1:1"], quick).await;
    let created = post(&n, "beacon", "into the void").await;
    assert_eq!(
        created.delivery,
        [Delivery {
            node: "http://127.0.0.1:1".into(),
            delivered: false
        }]
    );
}

// ------------------------------------------------------------- no busy loops

/// An event from an origin no node here owns.
fn foreign(seq: u64, body: &str) -> beacon::model::Event {
    beacon::model::Event {
        origin: "zed~gap00001".into(),
        seq,
        id: ulid::Ulid::generate().to_string(),
        ts: 1_000 + seq as i64,
        kind: EventKind::PostCreated(NewPost {
            post_id: ulid::Ulid::generate().to_string(),
            channel: "general".into(),
            author: "zed-agent".into(),
            author_kind: AuthorKind::Agent,
            client: None,
            node: "zed".into(),
            body: body.into(),
            tags: vec![],
            mentions: vec![],
            attachment: None,
        }),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_peers_and_a_shared_gap_do_not_spin() {
    let (a, b) = pair().await;
    // Both hold seq 1 and 3 of an origin, and neither has seq 2: each one's
    // version vector stops at 1, so each is offered seq 3 by the other again
    // and again for as long as the gap lasts.
    for node in [&a, &b] {
        for seq in [1, 3] {
            node.state()
                .ingest(foreign(seq, &format!("zed {seq}")))
                .await
                .unwrap();
        }
    }
    // Let the pushes that the new events caused settle.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let before = (requests_from(&a, "b"), requests_from(&b, "a"));
    tokio::time::sleep(Duration::from_secs(3)).await;
    let after = (requests_from(&a, "b"), requests_from(&b, "a"));
    // Long polls of 2 s: two or three pulls each way in 3 s, and no pushes.
    assert!(
        after.0 - before.0 <= 4,
        "b called a {} times",
        after.0 - before.0
    );
    assert!(
        after.1 - before.1 <= 4,
        "a called b {} times",
        after.1 - before.1
    );

    // The gap fills: both converge past it.
    b.state().ingest(foreign(2, "zed 2")).await.unwrap();
    eventually(3, "a to get seq 2 and move past the gap", || async {
        version_vector(&a).await.get("zed~gap00001") == Some(&3)
    })
    .await;
}

// ----------------------------------------------------------------- shutdown

#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_node_stops_its_sync() {
    let (a, b) = pair().await;
    let sync = a.state().sync.clone();
    assert!(sync.task_count() > 0);
    within(10, "a to stop", a.stop()).await;
    assert_eq!(sync.task_count(), 0, "sync tasks outlived the node");
    // A request already on the wire when a stopped may still be counted.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let calls = requests_from(&b, "a");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(requests_from(&b, "a"), calls, "a stopped but still calls b");
}

// -------------------------------------------------------------------- proxy

/// A SOCKS5 proxy that sends every connection to 127.0.0.1 on the asked-for
/// port, and records what it was asked for (the host as the client gave it).
async fn fake_socks5() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("socks5h://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((client, _)) = listener.accept().await else {
                return;
            };
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let _ = relay(client, log).await;
            });
        }
    });
    (url, seen)
}

async fn relay(mut client: TcpStream, log: Arc<Mutex<Vec<String>>>) -> std::io::Result<()> {
    let mut head = [0u8; 2];
    client.read_exact(&mut head).await?;
    let mut methods = vec![0u8; usize::from(head[1])];
    client.read_exact(&mut methods).await?;
    client.write_all(&[5, 0]).await?;
    let mut req = [0u8; 4];
    client.read_exact(&mut req).await?;
    let host = match req[3] {
        3 => {
            let mut len = [0u8; 1];
            client.read_exact(&mut len).await?;
            let mut name = vec![0u8; usize::from(len[0])];
            client.read_exact(&mut name).await?;
            format!("name:{}", String::from_utf8_lossy(&name))
        }
        1 => {
            let mut ip = [0u8; 4];
            client.read_exact(&mut ip).await?;
            format!("ip:{}", std::net::Ipv4Addr::from(ip))
        }
        _ => return Ok(()),
    };
    let mut port = [0u8; 2];
    client.read_exact(&mut port).await?;
    let port = u16::from_be_bytes(port);
    log.lock().unwrap().push(format!("{host}:{port}"));
    let mut upstream = TcpStream::connect(("127.0.0.1", port)).await?;
    client.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0]).await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn every_peer_request_goes_through_the_proxy() {
    let (proxy, seen) = fake_socks5().await;
    let rb = reserve();
    let port = rb.url.rsplit(':').next().unwrap().to_string();
    let b = rb.start("b", &[], quick).await;
    // A name that never resolves (.invalid): only the proxy can reach it, and
    // only if it is handed the name (socks5h), not an address.
    let far = format!("http://beacon-b.invalid:{port}");
    let a = spawn_node_with("a", &[far.as_str()], |cfg| {
        quick(cfg);
        cfg.proxy = Some(proxy.clone());
    })
    .await;

    eventually(5, "a to reach b through the proxy", || async {
        reaches(&a, "b").await
    })
    .await;
    // push_now
    let created = post(&a, "beacon", "via socks").await;
    assert_eq!(
        created.delivery,
        [Delivery {
            node: "b".into(),
            delivered: true
        }]
    );
    // pull, then the attachment fetch
    let bytes = pseudo_random(50_000, 11);
    let sha = sha256_hex(&bytes);
    upload(&b, "beacon", "from b", bytes.clone()).await;
    eventually(5, "a to fetch b's file through the proxy", || async {
        file(&a, &sha).await.as_deref() == Some(&bytes[..])
    })
    .await;

    let seen = seen.lock().unwrap().clone();
    assert!(!seen.is_empty());
    let want = format!("name:beacon-b.invalid:{port}");
    assert!(seen.iter().all(|s| *s == want), "{seen:?}");
}

// ------------------------------------------------------------------- urls

#[test]
fn peer_urls_are_normalized() {
    let ok = |s: &str| normalize_peer_url(s).unwrap_or_else(|| panic!("{s:?} should be fine"));
    assert_eq!(ok("http://soybean-wsl:7700"), "http://soybean-wsl:7700");
    assert_eq!(ok(" http://Soybean-WSL:7700/ "), "http://soybean-wsl:7700");
    assert_eq!(ok("HTTP://MAC:7700"), "http://mac:7700");
    assert_eq!(ok("http://100.64.0.7:7700"), "http://100.64.0.7:7700");
    assert_eq!(ok("http://[fd7a::1]:7700/"), "http://[fd7a::1]:7700");
    // The default port is implied.
    assert_eq!(ok("http://mac:80"), "http://mac");
    for bad in [
        "",
        "   ",
        "mac:7700",
        "soybean-wsl",
        "https://mac:7700",
        "socks5h://mac:7700",
        "http://",
        "http://mac:7700/beacon",
        "http://mac:7700/?x=1",
        "http://mac:7700/#top",
        "http://user:pw@mac:7700",
        "http://mac:notaport",
    ] {
        assert_eq!(normalize_peer_url(bad), None, "{bad:?}");
    }
}

#[test]
fn queued_counts_what_a_peer_lacks_per_origin() {
    use beacon::sync::queued;
    let vv = |pairs: &[(&str, u64)]| -> VersionVector {
        pairs.iter().map(|(o, s)| (o.to_string(), *s)).collect()
    };
    let ours = vv(&[("a~00000000", 10), ("b~00000000", 4)]);
    assert_eq!(queued(&ours, None), 14);
    assert_eq!(queued(&ours, Some(&vv(&[]))), 14);
    assert_eq!(queued(&ours, Some(&vv(&[("a~00000000", 7)]))), 7);
    // Ahead of us on one origin counts as nothing owed there, not less.
    let theirs = vv(&[("a~00000000", 12), ("b~00000000", 1), ("c~00000000", 9)]);
    assert_eq!(queued(&ours, Some(&theirs)), 3);
}
