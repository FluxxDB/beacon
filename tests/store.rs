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

/// Applies to each store the events the other has and it lacks.
fn sync(a: &mut Store, b: &mut Store) {
    for ev in a.events_after(&b.version_vector(), 10_000).unwrap() {
        b.apply(&ev).unwrap();
    }
    for ev in b.events_after(&a.version_vector(), 10_000).unwrap() {
        a.apply(&ev).unwrap();
    }
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
    let (mut a, mut b) = pair();
    let mut ev = post(&mut b, "mac", "ghost", "too big", 10);
    // SQLite integers are i64, so storing this size fails after the event row
    // and the channel mark were written: nothing of it may remain.
    if let EventKind::PostCreated(p) = &mut ev.kind {
        p.attachment = Some(Attachment {
            sha256: "00".repeat(32),
            name: "huge".into(),
            size: u64::MAX,
        });
    }
    let before = a.version_vector();
    assert!(a.apply(&ev).is_err());
    assert_eq!(a.version_vector(), before);
    assert!(a.channel("ghost").unwrap().is_none());
    assert_eq!(a.arrival_head(), 1);
    assert!(a.events_since_arrival(1, 10).unwrap().is_empty());

    // The same (origin, seq) can still be applied once it is valid.
    if let EventKind::PostCreated(p) = &mut ev.kind {
        p.attachment = None;
    }
    assert!(a.apply(&ev).unwrap().is_some());
    assert_eq!(bodies(&a, &in_channel("ghost")), vec!["too big"]);
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

    // On disk, reopening does not create it a second time.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("beacon.db");
    let once: VersionVector = [("pc".to_string(), 1)].into();
    {
        let s = Store::open(&path, "pc").unwrap();
        assert_eq!(s.version_vector(), once);
    }
    let s = Store::open(&path, "pc").unwrap();
    assert_eq!(s.version_vector(), once);
    assert_eq!(s.channels().unwrap(), vec![general]);
}

#[test]
fn events_after_relays_all_origins() {
    let (mut a, mut b) = pair();
    post(&mut a, "pc", "general", "from pc", 10);
    post(&mut b, "mac", "general", "from mac", 20);
    sync(&mut a, &mut b);

    let have: VersionVector = [("pc".to_string(), 0)].into();
    let events = a.events_after(&have, 100).unwrap();
    let origins: BTreeSet<&str> = events.iter().map(|e| e.origin.as_str()).collect();
    assert_eq!(origins, BTreeSet::from(["mac", "pc"]));
    assert_eq!(events.len(), 4, "both nodes' general and post");

    // Nothing is resent to a caller that has everything, and limit keeps the
    // oldest arrivals.
    assert!(a.events_after(&a.version_vector(), 100).unwrap().is_empty());
    let pc_general = a.events_since_arrival(0, 1).unwrap().remove(0).event;
    assert_eq!(
        a.events_after(&VersionVector::new(), 1).unwrap(),
        vec![pc_general]
    );
}

#[test]
fn version_vector_counts_only_the_gapless_prefix() {
    let (mut a, mut b) = pair();
    post(&mut b, "mac", "general", "two", 20);
    post(&mut b, "mac", "general", "three", 30);
    let mac = b.events_after(&VersionVector::new(), 10).unwrap();
    assert_eq!(mac.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2, 3]);

    a.apply(&mac[2]).unwrap();
    assert_eq!(a.version_vector()["mac"], 0);
    a.apply(&mac[0]).unwrap();
    assert_eq!(a.version_vector()["mac"], 1);
    // Whoever syncs from a's vector is sent seq 2 again, not skipped past it.
    let resend = b.events_after(&a.version_vector(), 10).unwrap();
    assert_eq!(resend.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![2, 3]);
    a.apply(&mac[1]).unwrap();
    assert_eq!(a.version_vector()["mac"], 3);
    assert_eq!(a.version_vector()["pc"], 1);
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
