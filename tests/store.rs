//! The event-log store: idempotent, order-independent apply and the channels,
//! posts and presence derived from it. Most tests run two in-memory nodes,
//! `a` ("pc") and `b` ("mac"), and sync them with `sync`.

use std::collections::BTreeSet;

use beacon::error::Error;
use beacon::model::*;
use beacon::store::*;

/// A minute and two hours, in ms.
const MIN: i64 = 60_000;
const TWO_HOURS: i64 = 2 * 60 * MIN;

fn pair() -> (Store, Store) {
    (
        Store::open_in_memory("pc").unwrap(),
        Store::open_in_memory("mac").unwrap(),
    )
}

/// Events per `events_after` page in `sync`, small so every test pages.
const PAGE: usize = 3;

/// Pages to each store the events the other has and it lacks, `PAGE` at a
/// time, until a round brings nothing new.
fn sync(a: &mut Store, b: &mut Store) {
    for _ in 0..100 {
        let mut fresh = 0;
        for ev in a.events_after(&b.version_vector(), PAGE).unwrap() {
            fresh += usize::from(b.apply(&ev).unwrap().is_some());
        }
        for ev in b.events_after(&a.version_vector(), PAGE).unwrap() {
            fresh += usize::from(a.apply(&ev).unwrap().is_some());
        }
        if fresh == 0 {
            return;
        }
    }
    panic!("sync did not settle in 100 rounds");
}

fn local(s: &mut Store, kind: EventKind, ts: i64) -> Event {
    s.append_local(kind, ts).unwrap().event
}

fn create(s: &mut Store, name: &str, topic: Option<&str>, ts: i64) -> Event {
    let kind = EventKind::ChannelCreated {
        name: name.into(),
        topic: topic.map(String::from),
    };
    local(s, kind, ts)
}

fn delete_channel(s: &mut Store, name: &str, ts: i64) -> Event {
    local(s, EventKind::ChannelDeleted { name: name.into() }, ts)
}

/// An agent post with an empty `post_id`, which `append_local` fills in.
fn new_post(node: &str, channel: &str, body: &str) -> NewPost {
    NewPost {
        post_id: String::new(),
        channel: channel.into(),
        author: format!("{node}-agent"),
        author_kind: AuthorKind::Agent,
        client: Some("claude-code".into()),
        node: node.into(),
        body: body.into(),
        tags: vec![],
        mentions: parse_mentions(body),
        attachment: None,
    }
}

fn post(s: &mut Store, node: &str, channel: &str, body: &str, ts: i64) -> Event {
    local(s, EventKind::PostCreated(new_post(node, channel, body)), ts)
}

fn delete_post(s: &mut Store, ev: &Event, ts: i64) -> Event {
    let kind = EventKind::PostDeleted {
        post_id: post_id(ev),
        by: "pc-agent".into(),
    };
    local(s, kind, ts)
}

fn post_id(ev: &Event) -> String {
    match &ev.kind {
        EventKind::PostCreated(p) => p.post_id.clone(),
        other => panic!("not a post: {other:?}"),
    }
}

fn in_channel(name: &str) -> PostQuery {
    PostQuery {
        channel: Some(name.into()),
        ..Default::default()
    }
}

fn bodies(s: &Store, q: &PostQuery) -> Vec<String> {
    s.posts(q).unwrap().into_iter().map(|p| p.body).collect()
}

fn names(s: &Store) -> Vec<String> {
    s.channels().unwrap().into_iter().map(|c| c.name).collect()
}

/// Posts without the node-local arrival number, so two nodes can be compared.
fn portable(s: &Store, q: &PostQuery) -> Vec<PostRow> {
    s.posts(q)
        .unwrap()
        .into_iter()
        .map(|p| PostRow { arrival: 0, ..p })
        .collect()
}

fn heartbeat(handle: &str, status: Option<&str>) -> Presence {
    Presence {
        handle: handle.into(),
        kind: AuthorKind::Agent,
        client: "claude-code".into(),
        node: "pc".into(),
        status: status.map(String::from),
    }
}

/// Records a heartbeat from `handle` on `s`.
fn seen(s: &mut Store, handle: &str, status: Option<&str>, ts: i64) {
    local(s, EventKind::AgentSeen(heartbeat(handle, status)), ts);
}

#[test]
fn apply_twice_is_noop() {
    let (mut a, mut b) = pair();
    let ev = post(&mut a, "pc", "general", "hello", 100);
    assert_eq!(post_id(&ev), ev.id, "an empty post_id becomes the event id");

    let first = b.apply(&ev).unwrap().expect("new event is applied");
    assert_eq!(first.event, ev);
    assert!(b.apply(&ev).unwrap().is_none());
    assert_eq!(b.posts(&in_channel("general")).unwrap().len(), 1);

    // The origin itself also ignores its own event coming back from a peer.
    assert!(a.apply(&ev).unwrap().is_none());
    assert_eq!(a.posts(&in_channel("general")).unwrap().len(), 1);
}

#[test]
fn apply_is_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("beacon.db");
    let mut a = Store::open(&path, "pc").unwrap();
    let mut b = Store::open_in_memory("mac").unwrap();
    let ev = post(&mut b, "mac", "ghost", "boom", 10);

    // Make the post insert fail, after the event row and the channel mark
    // were written: nothing of the event may remain.
    let other = rusqlite::Connection::open(&path).unwrap();
    other
        .execute_batch(
            "CREATE TRIGGER fail_boom BEFORE INSERT ON posts WHEN NEW.body = 'boom'
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
        )
        .unwrap();
    let before = a.version_vector();
    assert!(a.apply(&ev).is_err());
    assert_eq!(a.version_vector(), before);
    assert!(a.channel("ghost").unwrap().is_none());
    assert_eq!(a.arrival_head(), 1);
    assert!(a.events_since_arrival(1, 10).unwrap().is_empty());

    // The same (origin, seq) can still be applied once the failure is gone.
    other.execute_batch("DROP TRIGGER fail_boom").unwrap();
    assert!(a.apply(&ev).unwrap().is_some());
    assert_eq!(bodies(&a, &in_channel("ghost")), vec!["boom"]);
}

#[test]
fn apply_refuses_a_malformed_origin() {
    let mut a = Store::open_in_memory("pc").unwrap();
    let mut b = Store::open_in_memory("mac").unwrap();
    let good = post(&mut b, "mac", "general", "hello", 10);
    let before = (a.version_vector(), a.arrival_head());
    // A `,` or `:` would break `origin:seq,origin:seq`; the rest is not the
    // `{node}~{8 base32}` a store makes.
    for origin in [
        "mac,x~abcd1234",
        "mac:1~abcd1234",
        "mac",
        "mac~ABCD1234",
        "",
    ] {
        let mut ev = good.clone();
        ev.origin = origin.into();
        match a.apply(&ev) {
            Err(Error::Usage(_)) => {}
            other => panic!("origin {origin:?}: expected a usage error, got {other:?}"),
        }
    }
    assert_eq!((a.version_vector(), a.arrival_head()), before);
    assert!(a.apply(&good).unwrap().is_some());
}

#[test]
fn order_independent() {
    let (mut a, mut b) = pair();
    create(&mut a, "builds", Some("ci"), 10);
    let first = post(&mut a, "pc", "builds", "first", 20);
    post(&mut a, "pc", "builds", "second @mac-agent", 30);
    delete_post(&mut a, &first, 40);
    create(&mut a, "tmp", None, 15);
    post(&mut a, "pc", "tmp", "gone with tmp", 22);
    delete_channel(&mut a, "tmp", 25);
    create(&mut a, "builds", Some("ci and releases"), 50);
    seen(&mut a, "pc-configs", Some("busy"), 60);
    seen(&mut a, "pc-configs", Some("done"), 65);
    seen(&mut a, "pc-configs", None, 70);
    for (url, ts) in [("http://old:7700", 80), ("http://soybean-wsl:7700", 85)] {
        let announce = EventKind::PeerAnnounced {
            node: "pc".into(),
            url: url.into(),
        };
        local(&mut a, announce, ts);
    }

    let events = a.events_after(&VersionVector::new(), 10_000).unwrap();
    for ev in events.iter().rev() {
        b.apply(ev).unwrap();
    }

    let all = PostQuery::default();
    assert_eq!(bodies(&a, &all), vec!["second @mac-agent"]);
    assert_eq!(portable(&a, &all), portable(&b, &all));
    assert_eq!(names(&a), vec!["general", "builds"]);
    assert_eq!(
        a.channel("builds").unwrap().unwrap().topic.as_deref(),
        Some("ci and releases")
    );
    assert_eq!(a.channels().unwrap(), b.channels().unwrap());
    let who = a.who(100).unwrap();
    assert_eq!(who.len(), 1);
    assert_eq!(who[0].status.as_deref(), Some("done"));
    assert_eq!(who[0].last_seen_ms, 70);
    assert_eq!(who, b.who(100).unwrap());
    let peers = vec![("pc".to_string(), "http://soybean-wsl:7700".to_string())];
    assert_eq!(a.announced_peers().unwrap(), peers);
    assert_eq!(b.announced_peers().unwrap(), peers);
}

#[test]
fn same_channel_created_offline_converges() {
    let (mut a, mut b) = pair();
    create(&mut a, "builds", Some("x"), 10);
    create(&mut b, "builds", Some("y"), 20);
    sync(&mut a, &mut b);

    for s in [&a, &b] {
        let builds: Vec<ChannelRow> = s
            .channels()
            .unwrap()
            .into_iter()
            .filter(|c| c.name == "builds")
            .collect();
        assert_eq!(builds.len(), 1);
        assert_eq!(builds[0].topic.as_deref(), Some("y"));
    }
}

#[test]
fn delete_hides_and_recreate_restores() {
    let (mut a, mut b) = pair();
    create(&mut a, "builds", None, 10);
    let kept = post(&mut a, "pc", "builds", "kept", 20);
    delete_channel(&mut a, "builds", 30);

    assert!(!names(&a).contains(&"builds".to_string()));
    assert!(a.channel("builds").unwrap().is_none());
    assert!(a.posts(&in_channel("builds")).unwrap().is_empty());
    assert!(a.posts(&PostQuery::default()).unwrap().is_empty());
    assert!(matches!(a.post(&post_id(&kept)), Err(Error::NotFound(_))));

    create(&mut a, "builds", None, 40);
    sync(&mut a, &mut b);
    for s in [&a, &b] {
        assert!(s.channel("builds").unwrap().is_some());
        assert_eq!(bodies(s, &in_channel("builds")), vec!["kept"]);
    }
}

#[test]
fn post_after_delete_revives_channel() {
    let (mut a, mut b) = pair();
    create(&mut a, "builds", None, 10);
    post(&mut a, "pc", "builds", "before", 20);
    sync(&mut a, &mut b);

    delete_channel(&mut a, "builds", 50);
    assert!(a.channel("builds").unwrap().is_none());
    // b has not heard about the delete and posts into the channel later.
    post(&mut b, "mac", "builds", "after", 60);
    sync(&mut a, &mut b);

    for s in [&a, &b] {
        assert!(names(s).contains(&"builds".to_string()));
        assert_eq!(bodies(s, &in_channel("builds")), vec!["before", "after"]);
    }
}

#[test]
fn tombstone_before_post() {
    let (mut a, mut b) = pair();
    let channel = create(&mut a, "builds", None, 10);
    let oops = post(&mut a, "pc", "builds", "oops", 20);
    let tombstone = delete_post(&mut a, &oops, 30);

    b.apply(&channel).unwrap();
    b.apply(&tombstone).unwrap();
    b.apply(&oops).unwrap();

    for s in [&a, &b] {
        assert!(s.channel("builds").unwrap().is_some());
        assert!(s.posts(&in_channel("builds")).unwrap().is_empty());
        assert!(matches!(s.post(&post_id(&oops)), Err(Error::NotFound(_))));
    }
}

#[test]
fn display_order_ts_then_id() {
    let (mut a, mut b) = pair();
    let from_pc = post(&mut a, "pc", "general", "from pc", 100);
    let from_mac = post(&mut b, "mac", "general", "from mac", 100);
    // Written later with an older clock: its id sorts after the ts-100 post
    // from the same node, but its ts puts it first.
    let earliest = post(&mut b, "mac", "general", "earliest", 50);
    assert!(earliest.id > from_mac.id);
    sync(&mut a, &mut b);

    let mut same_ts = vec![post_id(&from_pc), post_id(&from_mac)];
    same_ts.sort();
    let mut expected = vec![post_id(&earliest)];
    expected.extend(same_ts);
    for s in [&a, &b] {
        let ids: Vec<String> = s
            .posts(&in_channel("general"))
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(ids, expected);
    }
}

#[test]
fn post_lookup_by_suffix() {
    let (mut a, mut b) = pair();
    let id1 = "01JDB7K4TR0000000001QRSTVW";
    let id2 = "01JDB7K4TR0000000002QRSTVW";
    for (id, body, ts) in [(id1, "one", 10), (id2, "two", 20)] {
        let p = NewPost {
            post_id: id.into(),
            ..new_post("pc", "general", body)
        };
        local(&mut a, EventKind::PostCreated(p), ts);
    }
    let generated = post_id(&post(&mut a, "pc", "general", "three", 30));
    sync(&mut a, &mut b);

    for s in [&a, &b] {
        assert_eq!(s.post(short_id(id1)).unwrap().body, "one");
        assert_eq!(s.post(id2).unwrap().body, "two");
        assert_eq!(s.post("02qrstvw").unwrap().id, id2, "case-insensitive");
        assert_eq!(s.post(short_id(&generated)).unwrap().body, "three");
        match s.post("QRSTVW") {
            Err(Error::Ambiguous(ids)) => assert_eq!(ids, vec![id1, id2]),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
        assert!(matches!(s.post("ZZZZZZ"), Err(Error::NotFound(_))));
        assert!(matches!(s.post("STVW"), Err(Error::Usage(_))));
    }
}

#[test]
fn shown_post_is_an_exact_lookup_of_shown_posts() {
    let (mut a, _) = pair();
    create(&mut a, "beacon", None, 5);
    // Ids an event may carry that `post` would not look up: short, odd.
    for (id, body) in [
        ("short", "short id"),
        ("01JDB7K4TR0000000001QRSTVW", "ulid"),
    ] {
        let p = NewPost {
            post_id: id.into(),
            ..new_post("pc", "beacon", body)
        };
        local(&mut a, EventKind::PostCreated(p), 10);
    }
    assert_eq!(a.shown_post("short").unwrap().unwrap().body, "short id");
    let ulid = a.shown_post("01JDB7K4TR0000000001QRSTVW").unwrap().unwrap();
    assert_eq!(ulid.body, "ulid");
    assert!(ulid.arrival > 0);
    // Exact, not a suffix; and unknown ids are `None`, not an error.
    assert!(a.shown_post("QRSTVW").unwrap().is_none());
    assert!(a.shown_post("hort").unwrap().is_none());
    assert!(a.shown_post("").unwrap().is_none());

    // A deleted post, or one in a deleted channel, is not shown.
    let deleted = post(&mut a, "pc", "beacon", "going", 20);
    assert!(a.shown_post(&post_id(&deleted)).unwrap().is_some());
    delete_post(&mut a, &deleted, 21);
    assert!(a.shown_post(&post_id(&deleted)).unwrap().is_none());
    delete_channel(&mut a, "beacon", 30);
    assert!(a.shown_post("short").unwrap().is_none());
}

#[test]
fn presence_due_once_a_minute() {
    let (mut a, mut b) = pair();
    let t0 = 1_000 * MIN;
    let hb = heartbeat("pc-configs", None);

    assert!(a.presence_due(&hb, t0));
    local(&mut a, EventKind::AgentSeen(hb.clone()), t0);
    assert!(!a.presence_due(&hb, t0 + 30_000));
    let busy = heartbeat("pc-configs", Some("building vllm"));
    assert!(a.presence_due(&busy, t0 + 30_000));
    assert!(a.presence_due(&hb, t0 + 61_000));

    sync(&mut a, &mut b);
    assert!(!b.presence_due(&hb, t0 + 30_000));
}

#[test]
fn status_survives_plain_heartbeats() {
    let (mut a, _) = pair();
    let t0 = 1_000 * MIN;
    let busy = heartbeat("pc-configs", Some("building vllm"));
    local(&mut a, EventKind::AgentSeen(busy.clone()), t0);
    seen(&mut a, "pc-configs", None, t0 + MIN);

    let who = a.who(t0 + MIN).unwrap();
    assert_eq!(who.len(), 1);
    assert_eq!(who[0].status.as_deref(), Some("building vllm"));
    assert_eq!(who[0].last_seen_ms, t0 + MIN);
    // Repeating the same status is not a change.
    assert!(!a.presence_due(&busy, t0 + MIN + 30_000));
}

#[test]
fn who_lists_two_hours_newest_first() {
    let (mut a, _) = pair();
    let t0 = 1_000 * MIN;
    seen(&mut a, "older", None, t0);
    seen(&mut a, "newer", None, t0 + 10 * MIN);

    let handles =
        |now| -> Vec<String> { a.who(now).unwrap().into_iter().map(|p| p.handle).collect() };
    assert_eq!(handles(t0 + 10 * MIN), vec!["newer", "older"]);
    assert_eq!(handles(t0 + TWO_HOURS), vec!["newer", "older"]);
    assert_eq!(handles(t0 + TWO_HOURS + 1), vec!["newer"]);
}

#[test]
fn fresh_store_has_general() {
    let general = ChannelRow {
        name: "general".into(),
        topic: None,
        last_post_ts: None,
    };
    let (a, b) = pair();
    assert_eq!(a.channels().unwrap(), vec![general.clone()]);
    assert_eq!(b.channels().unwrap(), vec![general.clone()]);
    // Stamped ts 0, so it never outranks a channel event someone made.
    let created = a.events_since_arrival(0, 1).unwrap().remove(0).event;
    assert_eq!(created.ts, 0);
    assert!(matches!(created.kind, EventKind::ChannelCreated { .. }));

    // On disk, reopening keeps the origin and does not create it again.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("beacon.db");
    let origin = Store::open(&path, "pc").unwrap().origin().to_string();
    let s = Store::open(&path, "pc").unwrap();
    assert_eq!(s.origin(), origin);
    assert_eq!(s.version_vector(), [(origin, 1)].into());
    assert_eq!(s.channels().unwrap(), vec![general]);
}

#[test]
fn events_after_relays_all_origins() {
    let (mut a, mut b) = pair();
    post(&mut a, "pc", "general", "from pc", 10);
    post(&mut b, "mac", "general", "from mac", 20);
    sync(&mut a, &mut b);

    let have: VersionVector = [(a.origin().to_string(), 0)].into();
    let events = a.events_after(&have, 100).unwrap();
    let origins: BTreeSet<&str> = events.iter().map(|e| e.origin.as_str()).collect();
    assert_eq!(origins, BTreeSet::from([a.origin(), b.origin()]));
    assert_eq!(events.len(), 4, "both nodes' general and post");

    // Nothing is resent to a caller that has everything; a limit below the
    // number of origins still sends each origin's first event, by arrival.
    assert!(a.events_after(&a.version_vector(), 100).unwrap().is_empty());
    let firsts: Vec<(String, u64)> = a
        .events_after(&VersionVector::new(), 1)
        .unwrap()
        .into_iter()
        .map(|e| (e.origin, e.seq))
        .collect();
    let expected = vec![(a.origin().to_string(), 1), (b.origin().to_string(), 1)];
    assert_eq!(firsts, expected);
}

#[test]
fn version_vector_counts_only_the_gapless_prefix() {
    let (mut a, mut b) = pair();
    post(&mut b, "mac", "general", "two", 20);
    post(&mut b, "mac", "general", "three", 30);
    let mac = b.events_after(&VersionVector::new(), 10).unwrap();
    assert_eq!(mac.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2, 3]);

    a.apply(&mac[2]).unwrap();
    assert_eq!(a.version_vector()[b.origin()], 0);
    a.apply(&mac[0]).unwrap();
    assert_eq!(a.version_vector()[b.origin()], 1);
    // Whoever syncs from a's vector is sent seq 2 again, not skipped past it.
    let resend = b.events_after(&a.version_vector(), 10).unwrap();
    assert_eq!(resend.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![2, 3]);
    a.apply(&mac[1]).unwrap();
    assert_eq!(a.version_vector()[b.origin()], 3);
    assert_eq!(a.version_vector()[a.origin()], 1);
}

#[test]
fn same_ms_recreate_after_delete_is_visible() {
    let (mut a, _) = pair();
    create(&mut a, "builds", None, 10);
    delete_channel(&mut a, "builds", 20);
    create(&mut a, "builds", Some("again"), 20);
    let builds = a
        .channel("builds")
        .unwrap()
        .expect("later seq wins the tie");
    assert_eq!(builds.topic.as_deref(), Some("again"));
}

#[test]
fn channels_sorted_general_first() {
    let (mut a, _) = pair();
    create(&mut a, "zeta", None, 10);
    create(&mut a, "alpha", Some("first"), 20);
    create(&mut a, "beacon", None, 30);
    post(&mut a, "pc", "alpha", "hi", 40);
    let p = post(&mut a, "pc", "alpha", "deleted later", 50);
    delete_post(&mut a, &p, 60);

    assert_eq!(names(&a), vec!["general", "alpha", "beacon", "zeta"]);
    let alpha = a.channel("alpha").unwrap().unwrap();
    assert_eq!(alpha.topic.as_deref(), Some("first"));
    assert_eq!(alpha.last_post_ts, Some(40));
    assert!(a.channel("nope").unwrap().is_none());
}

#[test]
fn post_queries_filter_and_page() {
    let (mut a, _) = pair();
    create(&mut a, "builds", None, 1);
    let tagged = NewPost {
        tags: vec!["claim".into()],
        ..new_post("pc", "builds", "taking this")
    };
    local(&mut a, EventKind::PostCreated(tagged), 10);
    post(&mut a, "pc", "general", "ping @Mac-Juicebox", 20);
    post(&mut a, "pc", "builds", "heads up @all", 30);
    let human = NewPost {
        author: "Dang".into(),
        author_kind: AuthorKind::Human,
        client: None,
        ..new_post("pc", "general", "ship it")
    };
    local(&mut a, EventKind::PostCreated(human), 40);
    post(&mut a, "pc", "general", "unrelated", 50);

    let q = |f: fn(&mut PostQuery)| {
        let mut q = PostQuery::default();
        f(&mut q);
        bodies(&a, &q)
    };
    assert_eq!(q(|q| q.tag = Some("claim".into())), vec!["taking this"]);
    assert_eq!(
        q(|q| q.mentions_of = Some("mac-juicebox".into())),
        vec!["ping @Mac-Juicebox", "heads up @all"]
    );
    let announcements = q(|q| q.announcements_only = true);
    assert_eq!(announcements, vec!["ship it"]);
    // The newest `limit`, oldest first; `before` pages further back.
    assert_eq!(q(|q| q.limit = 2), vec!["ship it", "unrelated"]);
    let page = PostQuery {
        before: Some((40, String::new())),
        limit: 2,
        ..Default::default()
    };
    assert_eq!(
        bodies(&a, &page),
        vec!["ping @Mac-Juicebox", "heads up @all"]
    );

    let rows = a.posts(&PostQuery::default()).unwrap();
    assert!(rows
        .iter()
        .all(|p| p.announcement() == (p.author == "Dang")));
    let arrivals: Vec<i64> = rows.iter().map(|p| p.arrival).collect();
    assert!(arrivals.windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn attachment_round_trips() {
    let (mut a, _) = pair();
    let attachment = Attachment {
        sha256: "ab".repeat(32),
        name: "log.txt".into(),
        size: 1234,
    };
    let p = NewPost {
        attachment: Some(attachment.clone()),
        tags: vec!["done".into()],
        ..new_post("pc", "general", "see log")
    };
    let id = post_id(&local(&mut a, EventKind::PostCreated(p), 10));
    let row = a.post(&id).unwrap();
    assert_eq!(row.attachment, Some(attachment));
    assert_eq!(row.tags, vec!["done"]);
    assert_eq!(row.client.as_deref(), Some("claude-code"));
    assert_eq!(row.author_kind, AuthorKind::Agent);
    assert_eq!(row.ts, 10);
}

#[test]
fn arrival_feed() {
    let (mut a, mut b) = pair();
    let head = a.arrival_head();
    assert_eq!(head, 1, "general");
    let ev = post(&mut b, "mac", "general", "late", 5);
    let applied = a.apply(&ev).unwrap().unwrap();
    assert_eq!(applied.arrival, head + 1);
    assert_eq!(a.arrival_head(), head + 1);
    assert_eq!(a.events_since_arrival(head, 10).unwrap(), vec![applied]);
    assert!(a.events_since_arrival(head + 1, 10).unwrap().is_empty());
    let row = a.post(&ev.id).unwrap();
    assert_eq!(row.arrival, head + 1);
}

#[test]
fn announced_peers_latest_per_node() {
    let (mut a, mut b) = pair();
    let announce = |node: &str, url: &str| EventKind::PeerAnnounced {
        node: node.into(),
        url: url.into(),
    };
    local(&mut a, announce("pc", "http://old:7700"), 10);
    local(&mut b, announce("mac", "http://dangs-macbook-pro:7700"), 15);
    local(&mut a, announce("pc", "http://soybean-wsl:7700"), 20);
    sync(&mut a, &mut b);

    let expected = vec![
        (
            "mac".to_string(),
            "http://dangs-macbook-pro:7700".to_string(),
        ),
        ("pc".to_string(), "http://soybean-wsl:7700".to_string()),
    ];
    assert_eq!(a.announced_peers().unwrap(), expected);
    assert_eq!(b.announced_peers().unwrap(), expected);
}

#[test]
fn gap_filler_beyond_limit_converges() {
    // The reviewer's probe: the relay holds mac:2..12 and received mac:1 last;
    // the receiver holds mac:2..12 and lacks mac:1.
    let mut mac = Store::open_in_memory("mac").unwrap();
    for i in 0..11 {
        post(&mut mac, "mac", "general", &format!("post {i}"), 10 + i);
    }
    let events = mac.events_after(&VersionVector::new(), 100).unwrap();
    assert_eq!(events.iter().map(|e| e.seq).max(), Some(12));
    let mut relay = Store::open_in_memory("relay").unwrap();
    let mut receiver = Store::open_in_memory("receiver").unwrap();
    for ev in &events[1..] {
        relay.apply(ev).unwrap();
        receiver.apply(ev).unwrap();
    }
    relay.apply(&events[0]).unwrap();
    assert_eq!(receiver.version_vector()[mac.origin()], 0);

    // One page of 5 already carries mac:1 (and the relay's own first event),
    // though mac:1 is the relay's newest arrival.
    for ev in relay.events_after(&receiver.version_vector(), 5).unwrap() {
        receiver.apply(&ev).unwrap();
    }
    assert_eq!(receiver.version_vector()[mac.origin()], 12);
    assert_eq!(receiver.version_vector()[relay.origin()], 1);
}

#[test]
fn events_after_applied_carries_arrivals() {
    let (mut a, _) = pair();
    post(&mut a, "pc", "general", "one", 10);
    post(&mut a, "pc", "general", "two", 20);
    let applied = a.events_after_applied(&VersionVector::new(), 10).unwrap();
    assert_eq!(applied.len(), 3);
    assert_eq!(applied, a.events_since_arrival(0, 10).unwrap());
    let events: Vec<Event> = applied.into_iter().map(|x| x.event).collect();
    assert_eq!(a.events_after(&VersionVector::new(), 10).unwrap(), events);
}

#[test]
fn fresh_receiver_pages_through_origins_received_in_bulk() {
    // The reviewer's scenario: the relay received pc:1..10, then mac:1..10.
    let mut pc = Store::open_in_memory("pc").unwrap();
    let mut mac = Store::open_in_memory("mac").unwrap();
    for i in 0..9 {
        post(&mut pc, "pc", "general", &format!("pc {i}"), 10 + i);
        post(&mut mac, "mac", "general", &format!("mac {i}"), 10 + i);
    }
    let mut relay = Store::open_in_memory("relay").unwrap();
    for source in [&pc, &mac] {
        for ev in source.events_after(&VersionVector::new(), 100).unwrap() {
            relay.apply(&ev).unwrap();
        }
    }
    assert_eq!(relay.arrival_head(), 21);

    // 21 events, 4 a page, nothing re-sent: 6 pages.
    let mut receiver = Store::open_in_memory("receiver").unwrap();
    for _page in 0..6 {
        for ev in relay.events_after(&receiver.version_vector(), 4).unwrap() {
            assert!(receiver.apply(&ev).unwrap().is_some(), "re-sent {ev:?}");
        }
    }
    for origin in [pc.origin(), mac.origin(), relay.origin()] {
        assert_eq!(
            receiver.version_vector()[origin],
            relay.version_vector()[origin]
        );
    }
    assert_eq!(receiver.version_vector()[pc.origin()], 10);
    assert_eq!(receiver.version_vector()[mac.origin()], 10);
}

#[test]
fn limit_below_origin_count_still_reaches_every_origin() {
    // x:1 is missing on both relay and receiver, so x:2 is re-sent on every
    // page; with limit 1 it must not crowd out y, which arrived after it.
    let mut x = Store::open_in_memory("x").unwrap();
    post(&mut x, "x", "general", "x two", 10);
    let xs = x.events_after(&VersionVector::new(), 10).unwrap();
    let mut y = Store::open_in_memory("y").unwrap();
    post(&mut y, "y", "general", "y two", 20);
    post(&mut y, "y", "general", "y three", 30);

    let mut relay = Store::open_in_memory("relay").unwrap();
    let mut receiver = Store::open_in_memory("receiver").unwrap();
    relay.apply(&xs[1]).unwrap();
    receiver.apply(&xs[1]).unwrap();
    for ev in y.events_after(&VersionVector::new(), 10).unwrap() {
        relay.apply(&ev).unwrap();
    }

    for _page in 0..10 {
        let page = relay.events_after(&receiver.version_vector(), 1).unwrap();
        for ev in page {
            receiver.apply(&ev).unwrap();
        }
    }
    assert_eq!(receiver.version_vector()[y.origin()], 3);
    assert_eq!(receiver.version_vector()[relay.origin()], 1);
    assert_eq!(receiver.version_vector()[x.origin()], 0);
}

#[test]
fn fresh_node_does_not_reset_general() {
    let (mut a, mut b) = pair();
    create(&mut a, "general", Some("lobby"), 100);
    sync(&mut a, &mut b);
    let mut c = Store::open_in_memory("new-laptop").unwrap();
    sync(&mut c, &mut b);
    sync(&mut a, &mut b);
    for s in [&a, &b, &c] {
        let general = s.channel("general").unwrap().unwrap();
        assert_eq!(general.topic.as_deref(), Some("lobby"));
    }

    delete_channel(&mut a, "general", 200);
    let mut d = Store::open_in_memory("another").unwrap();
    sync(&mut d, &mut a);
    for s in [&a, &d] {
        assert!(s.channel("general").unwrap().is_none());
    }
}

#[test]
fn same_node_name_on_fresh_databases_gets_new_origins() {
    let mut before_wipe = Store::open_in_memory("pc").unwrap();
    let mut after_wipe = Store::open_in_memory("pc").unwrap();
    assert_eq!(before_wipe.node(), "pc");
    assert_ne!(before_wipe.origin(), after_wipe.origin());
    let (node, db_id) = before_wipe.origin().split_once('~').unwrap();
    assert_eq!(node, "pc");
    assert_eq!(db_id.len(), 8);
    assert!(db_id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));

    post(&mut before_wipe, "pc", "general", "before the wipe", 10);
    post(&mut after_wipe, "pc", "general", "after the wipe", 20);
    let mut mac = Store::open_in_memory("mac").unwrap();
    sync(&mut before_wipe, &mut mac);
    sync(&mut after_wipe, &mut mac);
    assert_eq!(
        bodies(&mac, &in_channel("general")),
        vec!["before the wipe", "after the wipe"]
    );
}

/// A peer's event with the given payload, as if `append_local` had not
/// checked it.
fn remote(seq: u64, kind: EventKind) -> Event {
    Event {
        origin: "evil~abcdefgh".into(),
        seq,
        id: format!("01JDB7K4TR00000000000000{seq:02}"),
        ts: 1_000,
        kind,
    }
}

#[test]
fn invalid_remote_event_is_stored_and_relayed_not_applied() {
    let (mut a, mut b) = pair();
    let bad = remote(
        1,
        EventKind::ChannelCreated {
            name: "Bad Name".into(),
            topic: None,
        },
    );
    assert!(a.apply(&bad).unwrap().is_some());
    assert_eq!(a.version_vector()["evil~abcdefgh"], 1);
    assert_eq!(names(&a), vec!["general"]);
    assert!(a.apply(&bad).unwrap().is_none());

    sync(&mut a, &mut b);
    assert_eq!(b.version_vector()["evil~abcdefgh"], 1);
    assert_eq!(names(&b), vec!["general"]);
    assert_eq!(b.events_after(&a.version_vector(), 10).unwrap(), vec![]);
}

#[test]
fn invalid_payloads_change_nothing() {
    let valid = || new_post("evil", "general", "hi @pc-agent");
    let attachment = |name: &str, size: u64| {
        Some(Attachment {
            sha256: "00".repeat(32),
            name: name.into(),
            size,
        })
    };
    let posts = [
        NewPost {
            channel: "#general".into(),
            ..valid()
        },
        NewPost {
            author: "-dash".into(),
            ..valid()
        },
        NewPost {
            tags: vec!["Done".into()],
            ..valid()
        },
        NewPost {
            mentions: vec!["PC-agent".into()],
            ..valid()
        },
        NewPost {
            mentions: vec!["pc agent".into()],
            ..valid()
        },
        NewPost {
            body: "x".repeat(64 * 1024 + 1),
            ..valid()
        },
        NewPost {
            attachment: attachment("", 1),
            ..valid()
        },
        NewPost {
            attachment: attachment(&"n".repeat(256), 1),
            ..valid()
        },
        NewPost {
            attachment: attachment("big", u64::MAX),
            ..valid()
        },
    ];
    let mut kinds: Vec<EventKind> = posts.into_iter().map(EventKind::PostCreated).collect();
    kinds.push(EventKind::ChannelDeleted {
        name: "general ".into(),
    });
    kinds.push(EventKind::PostDeleted {
        post_id: "x".into(),
        by: "".into(),
    });
    kinds.push(EventKind::AgentSeen(heartbeat("bad handle", None)));

    for kind in kinds {
        let (mut a, _) = pair();
        let label = format!("{kind:?}");
        let mut local_store = Store::open_in_memory("evil").unwrap();
        assert!(
            matches!(
                local_store.append_local(kind.clone(), 1_000),
                Err(Error::Usage(_))
            ),
            "append_local accepted {label}"
        );
        assert_eq!(local_store.arrival_head(), 1, "{label}");

        assert!(a.apply(&remote(1, kind)).unwrap().is_some(), "{label}");
        assert_eq!(a.version_vector()["evil~abcdefgh"], 1, "{label}");
        assert_eq!(names(&a), vec!["general"], "{label}");
        assert!(
            a.posts(&PostQuery::default()).unwrap().is_empty(),
            "{label}"
        );
        assert!(a.who(1_000).unwrap().is_empty(), "{label}");
    }

    // A valid post is applied, as a control.
    let (mut a, _) = pair();
    a.apply(&remote(1, EventKind::PostCreated(valid())))
        .unwrap();
    assert_eq!(bodies(&a, &PostQuery::default()), vec!["hi @pc-agent"]);
}

#[test]
fn seq_out_of_range() {
    let (mut a, _) = pair();
    let hello = || EventKind::PostCreated(new_post("evil", "general", "hello"));
    let huge = remote(u64::MAX, hello());
    assert!(matches!(a.apply(&huge), Err(Error::Usage(_))));
    assert_eq!(a.arrival_head(), 1);
    assert!(!a.version_vector().contains_key("evil~abcdefgh"));

    // seq 0 fits in SQLite, so it is stored but not applied.
    assert!(a.apply(&remote(0, hello())).unwrap().is_some());
    assert!(a.posts(&PostQuery::default()).unwrap().is_empty());
}

// ---- unread, relevance, cursors, joins ----

/// Posts `body` in `channel` as `author` (a human when `kind` says so), with
/// the mentions parsed from the body.
fn post_by(
    s: &mut Store,
    author: &str,
    kind: AuthorKind,
    channel: &str,
    body: &str,
    ts: i64,
) -> Event {
    let mut p = new_post("pc", channel, body);
    p.author = author.into();
    p.author_kind = kind;
    if kind == AuthorKind::Human {
        p.client = None;
    }
    local(s, EventKind::PostCreated(p), ts)
}

fn agent_post(s: &mut Store, author: &str, channel: &str, body: &str, ts: i64) -> Event {
    post_by(s, author, AuthorKind::Agent, channel, body, ts)
}

/// An announcement: Dang's post.
fn dang_post(s: &mut Store, channel: &str, body: &str, ts: i64) -> Event {
    post_by(s, "dang", AuthorKind::Human, channel, body, ts)
}

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

fn unread_bodies(s: &Store, handle: &str, limit: usize) -> Vec<String> {
    let unread = s.unread(handle, limit).unwrap();
    unread.posts.into_iter().map(|p| p.body).collect()
}

/// Every shown post, in display order.
fn every_post(s: &Store) -> Vec<PostRow> {
    s.posts(&PostQuery {
        limit: 1_000,
        ..Default::default()
    })
    .unwrap()
}

#[test]
fn unread_relevance() {
    let (mut a, _) = pair();
    let me = "pc-configs";
    a.set_joins(me, &strings(&["beacon"])).unwrap();
    agent_post(&mut a, "mac-agent", "beacon", "agent in beacon", 1_000);
    agent_post(&mut a, "mac-agent", "vllm", "agent in vllm", 2_000);
    dang_post(&mut a, "vllm", "dang in vllm", 3_000);
    agent_post(&mut a, "mac-agent", "vllm", "psst @pc-configs", 4_000);
    agent_post(&mut a, me, "beacon", "my own post", 5_000);
    agent_post(&mut a, "mac-agent", "configs", "@all heads up", 6_000);

    let unread = a.unread(me, 50).unwrap();
    let bodies: Vec<&str> = unread.posts.iter().map(|p| p.body.as_str()).collect();
    assert_eq!(
        bodies,
        [
            "dang in vllm",
            "agent in beacon",
            "psst @pc-configs",
            "@all heads up"
        ]
    );
    assert_eq!(unread.remaining, 0);

    // Another handle has its own joins: no `beacon`, no mention.
    assert_eq!(
        unread_bodies(&a, "someone", 50),
        ["dang in vllm", "@all heads up"]
    );
    // The announcement's author never gets his own post back.
    assert_eq!(unread_bodies(&a, "dang", 50), ["@all heads up"]);
}

#[test]
fn relevance_ignores_handle_case() {
    let (mut a, _) = pair();
    a.set_joins("PC-Configs", &strings(&["beacon"])).unwrap();
    agent_post(
        &mut a,
        "pc-configs",
        "beacon",
        "own post, other case",
        1_000,
    );
    agent_post(&mut a, "mac-agent", "beacon", "joined", 2_000);
    agent_post(&mut a, "mac-agent", "vllm", "hi @Pc-Configs", 3_000);
    for handle in ["pc-configs", "PC-CONFIGS", "Pc-Configs"] {
        assert_eq!(
            unread_bodies(&a, handle, 50),
            ["joined", "hi @Pc-Configs"],
            "{handle}"
        );
    }

    // One handle, one cursor, whichever way it is spelled.
    let shown = a.unread("pc-configs", 50).unwrap().posts;
    a.mark_read("pc-configs", &shown).unwrap();
    assert!(a.unread("PC-CONFIGS", 50).unwrap().posts.is_empty());
    assert_eq!(a.cursors("Pc-Configs").unwrap().len(), 2);
}

/// `unread` selects in SQL; `is_relevant` is the rule it must agree with.
#[test]
fn unread_agrees_with_is_relevant() {
    let (mut a, _) = pair();
    let mut ts = 0;
    for author in ["pc-configs", "PC-CONFIGS", "mac-agent"] {
        for channel in ["beacon", "vllm"] {
            for body in ["plain", "to @pc-configs", "to @all", "to @someone-else"] {
                ts += 1;
                agent_post(&mut a, author, channel, body, ts);
            }
        }
    }
    for channel in ["beacon", "vllm"] {
        for body in ["plain", "to @pc-configs", "to @all"] {
            ts += 1;
            dang_post(&mut a, channel, body, ts);
        }
    }
    let every = every_post(&a);

    let cases: [(&str, &[&str]); 5] = [
        ("pc-configs", &["beacon"]),
        ("dang", &["vllm"]),
        ("mac-agent", &["beacon", "vllm"]),
        ("PC-Configs", &[]),
        ("nobody", &["general"]),
    ];
    let mut wider = false;
    for (handle, joined) in cases {
        a.set_joins(handle, &strings(joined)).unwrap();
        let joins = a.joins(handle).unwrap();
        let want: Vec<&PostRow> = every
            .iter()
            .filter(|p| a.is_relevant(p, handle, &joins))
            .collect();
        assert!(
            !want.is_empty() && want.len() < every.len(),
            "{handle}: the cases should split the posts"
        );

        let mut want_ids: Vec<&str> = want.iter().map(|p| p.id.as_str()).collect();
        let unread = a.unread(handle, 1_000).unwrap();
        let mut got_ids: Vec<&str> = unread.posts.iter().map(|p| p.id.as_str()).collect();
        want_ids.sort_unstable();
        got_ids.sort_unstable();
        assert_eq!(got_ids, want_ids, "{handle}");

        // The badges count every new post by anyone else, relevant or not.
        let mut want_counts = std::collections::BTreeMap::new();
        for p in every
            .iter()
            .filter(|p| !p.author.eq_ignore_ascii_case(handle))
        {
            *want_counts.entry(p.channel.clone()).or_insert(0u32) += 1;
        }
        wider |= want_counts.values().sum::<u32>() as usize > want.len();
        assert_eq!(a.unread_counts(handle).unwrap(), want_counts, "{handle}");
    }
    assert!(
        wider,
        "some handle should have new posts that are not relevant"
    );
}

/// The sidebar badges: every shown post by someone else after the cursor.
#[test]
fn unread_counts_cover_every_new_post_by_others() {
    let (mut a, _) = pair();
    let me = "dang";
    // No joins, no mentions: none of these is relevant to `dang`.
    agent_post(&mut a, "mac-agent", "vllm", "plain in vllm", 1_000);
    agent_post(&mut a, "mac-agent", "beacon", "plain in beacon", 2_000);
    // His own posts, spelled any way, are not new to him.
    post_by(
        &mut a,
        "DANG",
        AuthorKind::Human,
        "beacon",
        "own announcement",
        3_000,
    );
    agent_post(&mut a, "Dang", "vllm", "own, as an agent", 4_000);
    // A deleted post and a deleted channel's posts are not shown.
    let gone = agent_post(&mut a, "mac-agent", "beacon", "deleted", 5_000);
    delete_post(&mut a, &gone, 6_000);
    agent_post(&mut a, "mac-agent", "old", "in a deleted channel", 7_000);
    delete_channel(&mut a, "old", 8_000);

    let counts = |s: &Store, h: &str| -> Vec<(String, u32)> {
        s.unread_counts(h).unwrap().into_iter().collect()
    };
    assert_eq!(counts(&a, me), [("beacon".into(), 1), ("vllm".into(), 1)]);
    assert_eq!(counts(&a, "DANG"), counts(&a, me));
    // `unread` lists only what is relevant; the badges are wider on purpose.
    assert!(a.unread(me, 50).unwrap().posts.is_empty());
    // Someone else sees the posts that were `dang`'s own.
    assert_eq!(
        counts(&a, "reader"),
        [("beacon".into(), 2), ("vllm".into(), 2)]
    );

    // Announcements by someone else count too.
    post_by(
        &mut a,
        "ann",
        AuthorKind::Human,
        "vllm",
        "from another human",
        9_000,
    );
    assert_eq!(counts(&a, me), [("beacon".into(), 1), ("vllm".into(), 2)]);

    // Cursors are per channel: reading vllm leaves beacon's badge, and a
    // newer post in vllm lights it again.
    let vllm = a.posts(&in_channel("vllm")).unwrap();
    let newest = vllm.iter().map(|p| p.arrival).max().unwrap();
    a.set_cursor(me, "vllm", newest).unwrap();
    assert_eq!(counts(&a, me), [("beacon".into(), 1)]);
    agent_post(&mut a, "mac-agent", "vllm", "one more", 10_000);
    assert_eq!(counts(&a, me), [("beacon".into(), 1), ("vllm".into(), 1)]);
    a.set_cursor(me, "beacon", i64::MAX).unwrap();
    assert_eq!(counts(&a, me), [("vllm".into(), 1)]);
}

#[test]
fn unread_announcements_counts_others_human_posts_across_channels() {
    let (mut a, _) = pair();
    let me = "reader";
    dang_post(&mut a, "vllm", "a1", 1_000);
    dang_post(&mut a, "beacon", "a2", 2_000);
    dang_post(&mut a, "beacon", "a3", 3_000);
    // Not announcements, not new to him, or not shown.
    agent_post(&mut a, "mac-agent", "beacon", "an agent", 4_000);
    post_by(
        &mut a,
        "READER",
        AuthorKind::Human,
        "beacon",
        "his own",
        5_000,
    );
    let gone = dang_post(&mut a, "beacon", "deleted", 6_000);
    delete_post(&mut a, &gone, 7_000);
    dang_post(&mut a, "old", "in a deleted channel", 8_000);
    delete_channel(&mut a, "old", 9_000);

    assert_eq!(a.unread_announcements(me).unwrap(), 3);
    assert_eq!(a.unread_announcements("READER").unwrap(), 3);
    // Dang's own announcements are not new to him; the human "reader" posted one.
    assert_eq!(a.unread_announcements("dang").unwrap(), 1);

    // Joins do not matter, and it agrees with what `unread` lists.
    let listed = |s: &Store| {
        s.unread(me, 1_000)
            .unwrap()
            .posts
            .iter()
            .filter(|p| p.announcement())
            .count()
    };
    assert_eq!(listed(&a), 3);

    // Cursors are per channel: reading beacon up to a2 leaves a1 (vllm) and a3.
    let beacon = a.posts(&in_channel("beacon")).unwrap();
    let a2 = beacon.iter().find(|p| p.body == "a2").unwrap();
    a.set_cursor(me, "beacon", a2.arrival).unwrap();
    assert_eq!(a.unread_announcements(me).unwrap(), 2, "a1 and a3 remain");
    assert_eq!(listed(&a), 2);

    let shown = a.unread(me, 50).unwrap().posts;
    a.mark_read(me, &shown).unwrap();
    assert_eq!(a.unread_announcements(me).unwrap(), 0);
}

#[test]
fn mark_read_advances_and_limits() {
    let (mut a, _) = pair();
    let me = "pc-configs";
    a.set_joins(me, &strings(&["beacon"])).unwrap();
    agent_post(&mut a, "mac-agent", "beacon", "b1", 1_000);
    agent_post(&mut a, "mac-agent", "vllm", "v1 @pc-configs", 2_000);
    agent_post(&mut a, "mac-agent", "beacon", "b2", 3_000);
    agent_post(&mut a, "mac-agent", "vllm", "v2 @pc-configs", 4_000);
    agent_post(&mut a, "mac-agent", "beacon", "b3", 5_000);
    let counts =
        |s: &Store| -> Vec<(String, u32)> { s.unread_counts(me).unwrap().into_iter().collect() };
    assert_eq!(counts(&a), [("beacon".into(), 3), ("vllm".into(), 2)]);

    let first = a.unread(me, 2).unwrap();
    let bodies: Vec<&str> = first.posts.iter().map(|p| p.body.as_str()).collect();
    assert_eq!(bodies, ["b1", "v1 @pc-configs"]);
    assert_eq!(first.remaining, 3);
    // Asking again without reading changes nothing.
    assert_eq!(a.unread(me, 2).unwrap(), first);

    a.mark_read(me, &first.posts).unwrap();
    let cursors = a.cursors(me).unwrap();
    assert_eq!(cursors["beacon"], first.posts[0].arrival);
    assert_eq!(cursors["vllm"], first.posts[1].arrival);
    assert_eq!(counts(&a), [("beacon".into(), 2), ("vllm".into(), 1)]);

    let rest = a.unread(me, 50).unwrap();
    let bodies: Vec<&str> = rest.posts.iter().map(|p| p.body.as_str()).collect();
    assert_eq!(bodies, ["b2", "v2 @pc-configs", "b3"]);
    assert_eq!(rest.remaining, 0);
    a.mark_read(me, &rest.posts).unwrap();
    assert!(a.unread(me, 50).unwrap().posts.is_empty());
    assert!(a.unread_counts(me).unwrap().is_empty());

    // Nothing shown, nothing to do.
    a.mark_read(me, &[]).unwrap();
}

#[test]
fn reading_one_channel_leaves_the_others_unread() {
    let (mut a, _) = pair();
    let me = "reader";
    a.set_joins(me, &strings(&["beacon", "vllm"])).unwrap();
    agent_post(&mut a, "mac-agent", "beacon", "in beacon", 1_000);
    agent_post(&mut a, "mac-agent", "vllm", "in vllm", 2_000);
    let both = a.unread(me, 50).unwrap().posts;

    a.mark_read(me, &both[..1]).unwrap();
    assert_eq!(
        a.cursors(me).unwrap().keys().collect::<Vec<_>>(),
        ["beacon"]
    );
    assert_eq!(unread_bodies(&a, me, 50), ["in vllm"]);
    // Other handles keep their own cursors.
    assert_eq!(unread_bodies(&a, "someone-else", 50), Vec::<String>::new());
    a.set_joins("someone-else", &strings(&["beacon"])).unwrap();
    assert_eq!(unread_bodies(&a, "someone-else", 50), ["in beacon"]);
}

#[test]
fn unread_limit_edges() {
    let (mut a, _) = pair();
    a.set_joins("h", &strings(&["beacon"])).unwrap();
    for n in 1..=3 {
        agent_post(&mut a, "mac-agent", "beacon", &format!("p{n}"), n * 1_000);
    }
    let shape = |limit| {
        let u = a.unread("h", limit).unwrap();
        (u.posts.len(), u.remaining)
    };
    assert_eq!(shape(0), (0, 3));
    assert_eq!(shape(1), (1, 2));
    assert_eq!(shape(2), (2, 1));
    assert_eq!(shape(3), (3, 0));
    assert_eq!(shape(4), (3, 0));
    assert_eq!(shape(usize::MAX), (3, 0));
}

#[test]
fn unread_takes_lowest_arrivals_then_orders_announcements_first() {
    let (mut a, _) = pair();
    let me = "reader";
    a.set_joins(me, &strings(&["beacon"])).unwrap();
    // Arrival order is p1 p2 p3 p4; ts order is p2 p3 p1 p4.
    agent_post(&mut a, "mac-agent", "beacon", "p1", 300);
    agent_post(&mut a, "mac-agent", "beacon", "p2", 100);
    dang_post(&mut a, "vllm", "announcement", 200);
    agent_post(&mut a, "mac-agent", "beacon", "p4", 400);

    // The two lowest arrivals are p1 and p2: shown by ts, and the
    // announcement (third to arrive) waits for the next page.
    let page = a.unread(me, 2).unwrap();
    let bodies: Vec<&str> = page.posts.iter().map(|p| p.body.as_str()).collect();
    assert_eq!(bodies, ["p2", "p1"]);
    assert_eq!(page.remaining, 2);

    // Three: announcements first, then the rest by ts.
    let page = a.unread(me, 3).unwrap();
    let bodies: Vec<&str> = page.posts.iter().map(|p| p.body.as_str()).collect();
    assert_eq!(bodies, ["announcement", "p2", "p1"]);
    assert_eq!(page.remaining, 1);

    assert_eq!(
        unread_bodies(&a, me, 50),
        ["announcement", "p2", "p1", "p4"]
    );
}

#[test]
fn unread_ties_on_ts_break_by_id() {
    let (mut a, _) = pair();
    a.set_joins("reader", &strings(&["beacon"])).unwrap();
    // Same ts and same origin; the higher id arrives first.
    for (seq, id) in [
        (1, "01JDB7K4TR0000000000000002"),
        (2, "01JDB7K4TR0000000000000001"),
    ] {
        let mut p = new_post("evil", "beacon", id);
        p.post_id = id.into();
        let ev = Event {
            origin: "evil~abcdefgh".into(),
            seq,
            id: id.into(),
            ts: 700,
            kind: EventKind::PostCreated(p),
        };
        assert!(a.apply(&ev).unwrap().is_some());
    }
    let posts = a.unread("reader", 50).unwrap().posts;
    assert!(
        posts[0].arrival > posts[1].arrival,
        "arrival order is the reverse"
    );
    let ids: Vec<&str> = posts.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(
        ids,
        ["01JDB7K4TR0000000000000001", "01JDB7K4TR0000000000000002"]
    );
}

#[test]
fn unread_includes_late_arriving_old_post() {
    let (mut a, mut b) = pair();
    let me = "reader";
    a.set_joins(me, &strings(&["beacon"])).unwrap();
    agent_post(&mut a, "pc-agent", "beacon", "first", 10_000);
    agent_post(&mut a, "pc-agent", "beacon", "second", 11_000);
    let seen = a.unread(me, 50).unwrap();
    assert_eq!(seen.posts.len(), 2);
    a.mark_read(me, &seen.posts).unwrap();
    assert!(a.unread(me, 50).unwrap().posts.is_empty());

    // The mac wrote this offline, hours before everything above, and syncs now.
    post(&mut b, "mac", "beacon", "written offline", 5_000);
    sync(&mut a, &mut b);

    let late = a.unread(me, 50).unwrap();
    let bodies: Vec<&str> = late.posts.iter().map(|p| p.body.as_str()).collect();
    assert_eq!(bodies, ["written offline"]);
    assert!(
        late.posts[0].ts < seen.posts[0].ts,
        "its ts is older than all"
    );
    assert_eq!(late.remaining, 0);
    assert_eq!(a.unread_counts(me).unwrap()["beacon"], 1);

    a.mark_read(me, &late.posts).unwrap();
    assert!(a.unread(me, 50).unwrap().posts.is_empty());
}

#[test]
fn unread_skips_deleted_posts_and_hidden_channels() {
    let (mut a, _) = pair();
    let me = "reader";
    a.set_joins(me, &strings(&["beacon", "vllm"])).unwrap();
    let gone = agent_post(&mut a, "mac-agent", "beacon", "deleted", 1_000);
    agent_post(&mut a, "mac-agent", "beacon", "kept", 2_000);
    agent_post(
        &mut a,
        "mac-agent",
        "vllm",
        "hidden with its channel",
        3_000,
    );
    dang_post(&mut a, "vllm", "so is the announcement", 4_000);
    delete_post(&mut a, &gone, 5_000);
    delete_channel(&mut a, "vllm", 6_000);

    assert_eq!(unread_bodies(&a, me, 50), ["kept"]);
    assert_eq!(a.unread(me, 50).unwrap().remaining, 0);
    assert_eq!(
        a.unread_counts(me).unwrap().into_iter().collect::<Vec<_>>(),
        [("beacon".to_string(), 1)]
    );
}

#[test]
fn cursor_never_moves_backwards() {
    let (mut a, _) = pair();
    let me = "reader";
    a.set_cursor(me, "beacon", 10).unwrap();
    a.set_cursor(me, "beacon", 4).unwrap();
    assert_eq!(a.cursors(me).unwrap()["beacon"], 10);
    a.set_cursor(me, "beacon", 10).unwrap();
    a.set_cursor(me, "beacon", 12).unwrap();
    assert_eq!(a.cursors(me).unwrap()["beacon"], 12);
    // A cursor is an arrival: 0 is the floor, the same as having none.
    a.set_cursor(me, "vllm", -3).unwrap();
    assert_eq!(a.cursors(me).unwrap()["vllm"], 0);
    // Channels and handles are independent; a `#` or capital is normalized.
    a.set_cursor(me, "#Beacon", 5).unwrap();
    a.set_cursor("other", "beacon", 1).unwrap();
    assert_eq!(
        a.cursors(me).unwrap(),
        [("beacon".to_string(), 12), ("vllm".to_string(), 0)].into()
    );
    assert_eq!(
        a.cursors("other").unwrap(),
        [("beacon".to_string(), 1)].into()
    );
    assert!(matches!(
        a.set_cursor(me, "not a channel", 1),
        Err(Error::Usage(_))
    ));

    // Reading older posts later does not bring newer ones back.
    a.set_joins("h", &strings(&["beacon"])).unwrap();
    agent_post(&mut a, "mac-agent", "beacon", "older", 1_000);
    agent_post(&mut a, "mac-agent", "beacon", "newer", 2_000);
    let both = a.unread("h", 50).unwrap().posts;
    a.mark_read("h", &both[1..]).unwrap();
    a.mark_read("h", &both[..1]).unwrap();
    assert_eq!(a.cursors("h").unwrap()["beacon"], both[1].arrival);
    assert!(a.unread("h", 50).unwrap().posts.is_empty());
    a.set_cursor("h", "beacon", 0).unwrap();
    assert!(a.unread("h", 50).unwrap().posts.is_empty());
}

#[test]
fn joins_roundtrip() {
    let (mut a, _) = pair();
    assert_eq!(a.joins("h").unwrap(), Vec::<String>::new());

    // A set, listed by name.
    a.set_joins("h", &strings(&["vllm", "beacon", "vllm"]))
        .unwrap();
    assert_eq!(a.joins("h").unwrap(), ["beacon", "vllm"]);
    // Per handle, and a handle's case does not matter.
    assert_eq!(a.joins("other").unwrap(), Vec::<String>::new());
    assert_eq!(a.joins("H").unwrap(), ["beacon", "vllm"]);

    // Replaces the whole list; names are normalized like any channel name.
    a.set_joins("h", &strings(&["#Configs"])).unwrap();
    assert_eq!(a.joins("h").unwrap(), ["configs"]);

    // A bad name is refused and changes nothing.
    assert!(matches!(
        a.set_joins("h", &strings(&["beacon", "not a channel"])),
        Err(Error::Usage(_))
    ));
    assert_eq!(a.joins("h").unwrap(), ["configs"]);

    // A channel can be joined before it exists.
    assert!(a.channel("configs").unwrap().is_none());

    a.set_joins("h", &[]).unwrap();
    assert_eq!(a.joins("h").unwrap(), Vec::<String>::new());
}

#[test]
fn cursors_and_joins_are_local_only() {
    let (mut a, mut b) = pair();
    agent_post(&mut b, "mac-agent", "beacon", "hello", 1_000);
    sync(&mut a, &mut b);
    let head = a.arrival_head();
    let have = a.version_vector();

    a.set_joins("h", &strings(&["beacon"])).unwrap();
    let shown = a.unread("h", 50).unwrap().posts;
    assert_eq!(shown.len(), 1);
    a.mark_read("h", &shown).unwrap();
    a.set_cursor("h", "vllm", 3).unwrap();

    // No event was made, so there is nothing to sync.
    assert_eq!(a.arrival_head(), head);
    assert_eq!(a.version_vector(), have);
    sync(&mut a, &mut b);
    assert!(b.joins("h").unwrap().is_empty());
    assert!(b.cursors("h").unwrap().is_empty());
}

#[test]
fn cursors_and_joins_persist_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("beacon.db");
    let mut a = Store::open(&path, "pc").unwrap();
    a.set_joins("h", &strings(&["beacon"])).unwrap();
    agent_post(&mut a, "mac-agent", "beacon", "one", 1_000);
    agent_post(&mut a, "mac-agent", "beacon", "two", 2_000);
    let first = a.unread("h", 1).unwrap();
    assert_eq!(first.remaining, 1);
    a.mark_read("h", &first.posts).unwrap();
    drop(a);

    let a = Store::open(&path, "pc").unwrap();
    assert_eq!(a.joins("h").unwrap(), ["beacon"]);
    assert_eq!(unread_bodies(&a, "h", 50), ["two"]);
}

#[test]
fn post_row_from_applied_is_the_row_the_store_derives() {
    let (mut a, _) = pair();
    let p = NewPost {
        attachment: Some(Attachment {
            sha256: "ab".repeat(32),
            name: "log.txt".into(),
            size: 1234,
        }),
        tags: vec!["done".into(), "log".into()],
        client: None,
        ..new_post("pc", "general", "see log @mac-agent")
    };
    let applied = a.append_local(EventKind::PostCreated(p), 10).unwrap();
    let built = PostRow::from_applied(&applied).expect("a post_created event");
    let stored = a.post(&built.id).unwrap();
    assert_eq!(built, stored, "the two ways of making a PostRow agree");
    assert_eq!(built.arrival, applied.arrival);
    assert_eq!(built.ts, 10);

    // Events that are not posts make no row.
    let channel = a
        .append_local(
            EventKind::ChannelCreated {
                name: "builds".into(),
                topic: None,
            },
            11,
        )
        .unwrap();
    assert_eq!(PostRow::from_applied(&channel), None);
}

#[test]
fn attachments_lists_each_file_of_a_live_post_once() {
    let mut s = Store::open_in_memory("pc").unwrap();
    let with = |sha: &str, size: u64| {
        let mut p = new_post("pc", "general", "see attached");
        p.attachment = Some(Attachment {
            sha256: sha.into(),
            name: "log.txt".into(),
            size,
        });
        EventKind::PostCreated(p)
    };
    let (one, two, gone) = ("a".repeat(64), "b".repeat(64), "c".repeat(64));
    assert!(s.attachments().unwrap().is_empty());
    post(&mut s, "pc", "general", "no file", 1);
    local(&mut s, with(&one, 10), 2);
    // The same file announced again, with a larger size: listed once, at it.
    local(&mut s, with(&one, 12), 3);
    local(&mut s, with(&two, 5), 4);
    // A deleted post's file is not wanted.
    let doomed = local(&mut s, with(&gone, 7), 5);
    delete_post(&mut s, &doomed, 6);
    assert_eq!(s.attachments().unwrap(), [(one, 12), (two, 5)]);
}
