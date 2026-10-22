use beacon::error::Error;
use beacon::model::*;
use beacon::timefmt::*;
use jiff::tz::TimeZone;

#[test]
fn event_json_shape() {
    let e = Event {
        origin: "pc".into(),
        seq: 3,
        id: "01JDB7K4TR0000000000000000".into(),
        ts: 1_000,
        kind: EventKind::ChannelDeleted {
            name: "vllm".into(),
        },
    };
    let j = serde_json::to_value(&e).unwrap();
    assert_eq!(j["kind"], "channel_deleted");
    assert_eq!(j["data"]["name"], "vllm");
    assert_eq!(serde_json::from_value::<Event>(j).unwrap(), e);
}

#[test]
fn channel_names() {
    assert_eq!(normalize_channel("#Builds").unwrap(), "builds");
    assert!(normalize_channel("gpu jobs").is_err());
    assert!(normalize_channel(&"a".repeat(33)).is_err());
}

#[test]
fn mentions() {
    assert_eq!(
        parse_mentions("@pc-configs which toolchain? cc @Dang, @all."),
        vec!["pc-configs", "dang", "all"]
    );
    assert!(mentions_handle(&["all".into()], "anyone"));
    assert!(mentions_handle(&["dang".into()], "Dang"));
    assert!(parse_mentions("mail me@example.com").is_empty()); // '@' must start a word
}

#[test]
fn vv_roundtrip() {
    let v = parse_vv("mac:40,pc:12").unwrap();
    assert_eq!(v["pc"], 12);
    assert_eq!(format_vv(&v), "mac:40,pc:12");
    assert!(parse_vv("pc").is_err());
}

#[test]
fn twelve_hour() {
    let tz = TimeZone::get("America/New_York").unwrap();
    // 2026-10-21 22:07 UTC = 6:07 pm EDT
    assert_eq!(clock_12h(1792620420000, &tz), "6:07 pm");
    assert_eq!(clock_12h(1792555200000, &tz), "12:00 am");
    assert_eq!(age_short(59_000), "now");
    assert_eq!(age_short(14 * 60_000), "14m");
}

#[test]
fn exit_codes() {
    assert_eq!(Error::Timeout.exit_code(), 6);
    assert_eq!(Error::Auth.exit_code(), 4);
    assert_eq!(Error::Unreachable("x".into()).exit_code(), 3);
}

#[test]
fn day_labels() {
    let tz = TimeZone::get("America/New_York").unwrap();
    let at = |s: &str| -> i64 {
        s.parse::<jiff::Zoned>()
            .unwrap()
            .timestamp()
            .as_millisecond()
    };
    let now = at("2026-10-21T18:40:00-04:00[America/New_York]");
    assert_eq!(
        day_label(at("2026-10-21T08:00:00-04:00[America/New_York]"), now, &tz),
        "Today"
    );
    assert_eq!(
        day_label(at("2026-10-20T23:59:00-04:00[America/New_York]"), now, &tz),
        "Yesterday"
    );
    assert_eq!(
        day_label(at("2026-10-19T12:00:00-04:00[America/New_York]"), now, &tz),
        "Mon Oct 19"
    );
}

#[test]
fn exit_codes_cover_every_variant() {
    let codes = [
        Error::Usage("u".into()).exit_code(),
        Error::NotFound("n".into()).exit_code(),
        Error::Ambiguous(vec!["a".into(), "b".into()]).exit_code(),
        Error::Unreachable("x".into()).exit_code(),
        Error::Auth.exit_code(),
        Error::Conflict("c".into()).exit_code(),
        Error::Timeout.exit_code(),
        Error::Internal(anyhow::anyhow!("boom")).exit_code(),
        Error::Interrupted("i".into()).exit_code(),
    ];
    assert_eq!(codes, [1, 2, 5, 3, 4, 5, 6, 1, 3]);
    assert_eq!(
        Error::Ambiguous(vec!["aaaaaaaa".into(), "bbbbbbbb".into()]).to_string(),
        "ambiguous id, matches: aaaaaaaa, bbbbbbbb"
    );
}

#[test]
fn every_event_kind_roundtrips_through_json() {
    let post = NewPost {
        post_id: "01JDB7K4TRABCDEFGH12345678".into(),
        channel: "builds".into(),
        author: "pc-configs".into(),
        author_kind: AuthorKind::Agent,
        client: Some("claude-code".into()),
        node: "pc".into(),
        body: "done @dang".into(),
        tags: vec!["done".into()],
        mentions: vec!["dang".into()],
        attachment: Some(Attachment {
            sha256: "ab".repeat(32),
            name: "log.txt".into(),
            size: 12,
        }),
    };
    let kinds = vec![
        EventKind::ChannelCreated {
            name: "builds".into(),
            topic: Some("CI".into()),
        },
        EventKind::ChannelCreated {
            name: "builds".into(),
            topic: None,
        },
        EventKind::ChannelDeleted {
            name: "builds".into(),
        },
        EventKind::PostCreated(post),
        EventKind::PostDeleted {
            post_id: "p".into(),
            by: "dang".into(),
        },
        EventKind::AgentSeen(Presence {
            handle: "mac-juicebox".into(),
            kind: AuthorKind::Agent,
            client: "codex".into(),
            node: "mac".into(),
            status: None,
        }),
        EventKind::PeerAnnounced {
            node: "mac".into(),
            url: "http://mac:7700".into(),
        },
    ];
    for kind in kinds {
        let e = Event {
            origin: "pc".into(),
            seq: 1,
            id: "01JDB7K4TR0000000000000000".into(),
            ts: 5,
            kind,
        };
        let text = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&text).unwrap(), e, "{text}");
    }

    let e = Event {
        origin: "mac".into(),
        seq: 9,
        id: "01JDB7K4TR0000000000000000".into(),
        ts: 5,
        kind: EventKind::AgentSeen(Presence {
            handle: "mac-juicebox".into(),
            kind: AuthorKind::Human,
            client: "tui".into(),
            node: "mac".into(),
            status: Some("reviewing".into()),
        }),
    };
    let j = serde_json::to_value(&e).unwrap();
    assert_eq!(j["kind"], "agent_seen");
    assert_eq!(j["data"]["kind"], "human");
    assert_eq!(j["data"]["status"], "reviewing");
    assert_eq!(j["seq"], 9);
}

#[test]
fn channel_name_edges() {
    assert_eq!(normalize_channel("a-1").unwrap(), "a-1");
    assert_eq!(normalize_channel(&"a".repeat(32)).unwrap().len(), 32);
    assert!(normalize_channel("").is_err());
    assert!(normalize_channel("#").is_err());
    assert!(normalize_channel("##builds").is_err()); // only one '#' is stripped
    assert!(normalize_channel("under_score").is_err());
    assert!(normalize_channel("café").is_err());
    assert_eq!(normalize_channel("gpu jobs").unwrap_err().exit_code(), 1);
}

#[test]
fn handles_and_tags() {
    for ok in [
        "Dang",
        "pc-configs",
        "mac-juicebox",
        "a.b_c-d",
        "7",
        &"a".repeat(32),
    ] {
        assert_eq!(validate_handle(ok).unwrap(), ok);
    }
    for bad in [
        "",
        "-x",
        ".x",
        "has space",
        "emoji🚀",
        "@dang",
        &"a".repeat(33),
    ] {
        assert!(validate_handle(bad).is_err(), "{bad:?}");
    }
    for ok in ["claim", "fyi", "a-1", &"a".repeat(24)] {
        assert_eq!(validate_tag(ok).unwrap(), ok);
    }
    for bad in ["", "Done", "two words", "under_score", &"a".repeat(25)] {
        assert!(validate_tag(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn mention_edges() {
    assert_eq!(parse_mentions("@Dang and @dang again"), vec!["dang"]);
    assert_eq!(parse_mentions("(@dang) thanks"), vec!["dang"]);
    assert_eq!(parse_mentions("ping @dang!"), vec!["dang"]);
    assert_eq!(parse_mentions("ask @pc.configs..."), vec!["pc.configs"]);
    assert_eq!(parse_mentions("line one\n@a-b\t@c"), vec!["a-b", "c"]);
    assert_eq!(parse_mentions("@é @ @-x"), Vec::<String>::new());
    assert!(parse_mentions(&format!("@{}", "a".repeat(33))).is_empty());
    assert_eq!(parse_mentions(&format!("@{}", "a".repeat(32))).len(), 1);
    assert!(parse_mentions("").is_empty());
    assert!(!mentions_handle(&["dang".into()], "pc-configs"));
    assert!(!mentions_handle(&[], "dang"));
    assert!(mentions_handle(&["ALL".into()], "anyone"));
}

#[test]
fn short_ids() {
    assert_eq!(short_id("01JDB7K4TRABCDEFGH12345678"), "12345678");
    assert_eq!(short_id("abc"), "abc");
    assert_eq!(short_id("12345678"), "12345678");
    assert_eq!(short_id(""), "");
}

#[test]
fn version_vector_edges() {
    assert!(parse_vv("").unwrap().is_empty());
    assert_eq!(format_vv(&VersionVector::new()), "");
    // Output is sorted by origin whatever the input order.
    assert_eq!(
        format_vv(&parse_vv("pc:12,mac:40").unwrap()),
        "mac:40,pc:12"
    );
    for bad in [
        "pc",
        "pc:x",
        "pc:-1",
        ":1",
        "pc:1,pc:2",
        "pc:1,",
        "pc:1,,mac:2",
    ] {
        assert!(parse_vv(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn clock_edges() {
    let tz = TimeZone::get("America/New_York").unwrap();
    // 2026-10-21 16:00 UTC = 12:00 pm EDT; 12:30 am; 1:05 am; 11:59 pm.
    assert_eq!(clock_12h(1792598400000, &tz), "12:00 pm");
    assert_eq!(clock_12h(1792555200000 + 30 * 60_000, &tz), "12:30 am");
    assert_eq!(clock_12h(1792555200000 + 65 * 60_000, &tz), "1:05 am");
    assert_eq!(
        clock_12h(1792555200000 + (23 * 60 + 59) * 60_000, &tz),
        "11:59 pm"
    );
    // The same instant reads differently in another zone.
    assert_eq!(clock_12h(1792620420000, &TimeZone::UTC), "10:07 pm");
    // Out-of-range input clamps instead of panicking.
    assert!(!clock_12h(i64::MAX, &tz).is_empty());
    assert!(!clock_12h(i64::MIN, &tz).is_empty());
}

#[test]
fn age_buckets() {
    assert_eq!(age_short(-5_000), "now");
    assert_eq!(age_short(0), "now");
    assert_eq!(age_short(60_000), "1m");
    assert_eq!(age_short(3_599_999), "59m");
    assert_eq!(age_short(3_600_000), "1h");
    assert_eq!(age_short(3 * 3_600_000 + 1), "3h");
    assert_eq!(age_short(86_399_999), "23h");
    assert_eq!(age_short(2 * 86_400_000), "2d");
}

#[test]
fn iso8601_is_utc_with_millis() {
    assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(iso8601(1792620420123), "2026-10-21T22:07:00.123Z");
    assert_eq!(iso8601(-1), "1969-12-31T23:59:59.999Z");
}

#[test]
fn validate_event_enforces_free_text_limits() {
    let event = |kind: EventKind| Event {
        origin: "mac~abcd1234".into(),
        seq: 1,
        id: "01JDB7K4TR0000000000000000".into(),
        ts: 5,
        kind,
    };
    let post = |edit: &dyn Fn(&mut NewPost)| {
        let mut p = NewPost {
            post_id: "01JDB7K4TR0000000000000000".into(),
            channel: "builds".into(),
            author: "mac-juicebox".into(),
            author_kind: AuthorKind::Agent,
            client: Some("codex".into()),
            node: "mac".into(),
            body: "done".into(),
            tags: vec!["done".into()],
            mentions: vec![],
            attachment: Some(Attachment {
                sha256: "ab".repeat(32),
                name: "log.txt".into(),
                size: 12,
            }),
        };
        edit(&mut p);
        event(EventKind::PostCreated(p))
    };
    let seen = |edit: &dyn Fn(&mut Presence)| {
        let mut p = Presence {
            handle: "mac-juicebox".into(),
            kind: AuthorKind::Agent,
            client: "codex".into(),
            node: "mac".into(),
            status: Some("reviewing".into()),
        };
        edit(&mut p);
        event(EventKind::AgentSeen(p))
    };
    let topic = |t: &str| {
        event(EventKind::ChannelCreated {
            name: "builds".into(),
            topic: Some(t.into()),
        })
    };
    let accepts = |ev: Event, what: &str| {
        assert!(validate_event(&ev).is_ok(), "should accept {what}");
    };
    let refuses = |ev: Event, what: &str| match validate_event(&ev) {
        Err(Error::Usage(_)) => {}
        other => panic!("should refuse {what}, got {other:?}"),
    };

    accepts(post(&|_| {}), "a plain post");

    // client: 64 bytes, not characters.
    accepts(
        post(&|p| p.client = Some("c".repeat(64))),
        "a 64-byte client",
    );
    refuses(
        post(&|p| p.client = Some("c".repeat(65))),
        "a 65-byte client",
    );
    refuses(
        post(&|p| p.client = Some("é".repeat(33))),
        "66 bytes in 33 characters",
    );
    refuses(
        post(&|p| p.client = Some("co\u{1}dex".into())),
        "a control character in client",
    );
    accepts(
        seen(&|p| p.client = "c".repeat(64)),
        "a 64-byte presence client",
    );
    refuses(
        seen(&|p| p.client = "c".repeat(65)),
        "a 65-byte presence client",
    );
    refuses(
        seen(&|p| p.client = "tui\u{7f}".into()),
        "DEL in a presence client",
    );

    // presence status: 200 bytes.
    accepts(
        seen(&|p| p.status = Some("s".repeat(200))),
        "a 200-byte status",
    );
    refuses(
        seen(&|p| p.status = Some("s".repeat(201))),
        "a 201-byte status",
    );
    refuses(
        seen(&|p| p.status = Some("é".repeat(101))),
        "202 bytes in 101 characters",
    );
    refuses(
        seen(&|p| p.status = Some("busy\u{0}".into())),
        "NUL in a status",
    );
    accepts(seen(&|p| p.status = None), "no status");

    // channel topic: 200 bytes.
    accepts(topic(&"t".repeat(200)), "a 200-byte topic");
    refuses(topic(&"t".repeat(201)), "a 201-byte topic");
    refuses(topic("two\nlines"), "a newline in a topic");
    accepts(
        event(EventKind::ChannelCreated {
            name: "builds".into(),
            topic: None,
        }),
        "no topic",
    );

    // at most 8 tags.
    let tags = |n: usize| (1..=n).map(|i| format!("t{i}")).collect::<Vec<_>>();
    accepts(post(&|p| p.tags = tags(8)), "8 tags");
    refuses(post(&|p| p.tags = tags(9)), "9 tags");

    // bodies may hold \n and \t, and no other control character (Unicode Cc).
    accepts(post(&|p| p.body = "one\n\ttwo\n".into()), "newline and tab");
    accepts(
        post(&|p| p.body = "zero\u{200b}width \u{1f600}".into()),
        "invisible non-Cc",
    );
    for bad in [
        "\u{0}",
        "\u{1b}[31m",
        "\u{7}",
        "a\rb",
        "\u{7f}",
        "\u{85}",
        "\u{9f}",
    ] {
        refuses(
            post(&|p| p.body = format!("x{bad}y")),
            &format!("{bad:?} in a body"),
        );
    }

    // attachment names: no control characters (newline and tab included),
    // and the hash must be one a file can be named after.
    refuses(
        post(&|p| p.attachment.as_mut().unwrap().name = "a\nb".into()),
        "newline in a name",
    );
    refuses(
        post(&|p| p.attachment.as_mut().unwrap().name = "a\tb".into()),
        "tab in a name",
    );
    for sha in [
        "",
        "ab",
        "AB".repeat(32).as_str(),
        "../../etc/passwd",
        &"g".repeat(64),
    ] {
        refuses(
            post(&|p| p.attachment.as_mut().unwrap().sha256 = sha.to_string()),
            &format!("attachment hash {sha:?}"),
        );
    }
}
