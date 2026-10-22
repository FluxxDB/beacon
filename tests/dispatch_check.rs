//! Tests for `scripts/dispatch-check.sh`, the one cheap call a beacon dispatcher
//! makes on every `/loop` tick: join every channel, peek at the board, check
//! in, and say whether there is anything to do. The project and its fake
//! `beacon` are in `fake_beacon`.

mod common;
mod fake_beacon;

use fake_beacon::{Project, Run, ENV_SPELLINGS_OF_PC, TWO_UNREAD};
use serde_json::{json, Value};

const CHECK: &str = "dispatch-check.sh";

const JOIN_ALL: [&str; 7] = [
    "join",
    "--all",
    "--as",
    "pc-dispatch",
    "--client",
    "claude-code",
    "--json",
];

const PEEK: [&str; 8] = [
    "wait",
    "--timeout",
    "0",
    "--as",
    "pc-dispatch",
    "--client",
    "claude-code",
    "--json",
];

fn check(p: &Project, args: &[&str], env: &[(&str, &str)]) -> Run {
    p.run(CHECK, args, "", env)
}

/// The status a `who --here` call set, after checking the rest of the call.
fn status_set(call: &[String], handle: &str) -> String {
    assert_eq!(call.len(), 7, "{call:?}");
    assert_eq!(call[..2], ["who", "--here"], "{call:?}");
    assert_eq!(
        call[3..],
        ["--as", handle, "--client", "claude-code"],
        "{call:?}"
    );
    call[2].clone()
}

/// `dispatcher · last check <h:mm am/pm>`, then `rest`.
fn assert_checked_in(status: &str, rest: &str) {
    let time = status
        .strip_prefix("dispatcher · last check ")
        .and_then(|s| s.strip_suffix(rest))
        .unwrap_or_else(|| panic!("status {status:?}"));
    let (clock, half) = time.split_once(' ').unwrap();
    assert!(half == "am" || half == "pm", "{status:?}");
    let (h, m) = clock.split_once(':').unwrap();
    let h: u32 = h.parse().unwrap();
    assert!(
        (1..=12).contains(&h) && !clock.starts_with('0'),
        "{status:?}"
    );
    assert!(m.len() == 2 && m.parse::<u32>().unwrap() < 60, "{status:?}");
}

#[test]
fn nothing_new_says_exactly_that_and_checks_in() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let run = check(&p, &[], &[("FAKE_WAIT_STATUS", "6")]);
    assert_eq!(
        (run.code, run.stdout.as_str()),
        (0, "nothing new\n"),
        "{run:?}"
    );
    let calls = p.calls();
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert_eq!(calls[0], JOIN_ALL);
    assert_eq!(calls[1], PEEK);
    assert_checked_in(&status_set(&calls[2], "pc-dispatch"), "");
}

#[test]
fn unread_work_is_counted_and_still_checks_in() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let page = TWO_UNREAD.replace(r#""remaining":0"#, r#""remaining":3"#);
    let run = check(
        &p,
        &[],
        &[("FAKE_WAIT_STATUS", "0"), ("FAKE_WAIT_OUT", &page)],
    );
    assert_eq!(
        (run.code, run.stdout.as_str()),
        (0, "unread: 5\n"),
        "{run:?}"
    );
    let calls = p.calls();
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert_eq!(calls[0], JOIN_ALL);
    assert_eq!(calls[1], PEEK);
    assert_checked_in(&status_set(&calls[2], "pc-dispatch"), "");
}

#[test]
fn a_page_without_remaining_counts_its_posts() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let page = TWO_UNREAD.replace(r#","remaining":0"#, "");
    let run = check(
        &p,
        &[],
        &[("FAKE_WAIT_STATUS", "0"), ("FAKE_WAIT_OUT", &page)],
    );
    assert_eq!(
        (run.code, run.stdout.as_str()),
        (0, "unread: 2\n"),
        "{run:?}"
    );
}

#[test]
fn running_workers_show_in_the_status() {
    for (arg, rest) in [
        ("0", ""),
        ("1", " · 1 task running"),
        ("3", " · 3 tasks running"),
    ] {
        let p = Project::new(Some("BEACON_NODE=pc\n"));
        let run = check(&p, &[arg], &[]);
        assert_eq!(
            (run.code, run.stdout.as_str()),
            (0, "nothing new\n"),
            "{run:?}"
        );
        let calls = p.calls();
        assert_checked_in(&status_set(&calls[2], "pc-dispatch"), rest);
    }
}

#[test]
fn a_down_server_fails_loudly() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let run = check(
        &p,
        &[],
        &[
            ("FAKE_WAIT_STATUS", "3"),
            (
                "FAKE_WAIT_ERR",
                "beacon isn't running here. Start it with: docker compose up -d",
            ),
        ],
    );
    assert_eq!((run.code, run.stdout.as_str()), (3, ""), "{run:?}");
    assert!(run.stderr.contains("beacon is unreachable"), "{run:?}");
    assert!(run.stderr.contains("beacon isn't running here"), "{run:?}");
    let calls = p.calls();
    assert_eq!(calls.len(), 2, "no check-in without a server: {calls:?}");
    assert_eq!(calls[0], JOIN_ALL);
    assert_eq!(calls[1], PEEK);
}

#[test]
fn other_failures_are_passed_on() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let run = check(
        &p,
        &[],
        &[
            ("FAKE_WAIT_STATUS", "4"),
            ("FAKE_WAIT_ERR", "beacon: wrong BEACON_SECRET"),
        ],
    );
    assert_eq!((run.code, run.stdout.as_str()), (4, ""), "{run:?}");
    assert!(run.stderr.contains("exit 4"), "{run:?}");
    assert!(run.stderr.contains("wrong BEACON_SECRET"), "{run:?}");
}

#[test]
fn a_failed_check_in_is_an_error() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let run = check(
        &p,
        &[],
        &[("FAKE_WHO_STATUS", "3"), ("FAKE_WHO_ERR", "beacon: gone")],
    );
    assert_eq!(run.code, 3, "{run:?}");
    assert_eq!(run.stdout, "nothing new\n");
    assert!(run.stderr.contains("could not set the status"), "{run:?}");
    assert!(run.stderr.contains("beacon: gone"), "{run:?}");
}

#[test]
fn joining_on_a_down_server_fails_loudly_and_asks_nothing_more() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let run = check(
        &p,
        &[],
        &[
            ("FAKE_JOIN_STATUS", "3"),
            (
                "FAKE_JOIN_ERR",
                "beacon isn't running here. Start it with: docker compose up -d",
            ),
        ],
    );
    assert_eq!((run.code, run.stdout.as_str()), (3, ""), "{run:?}");
    assert!(
        run.stderr
            .contains("beacon is unreachable as pc-dispatch (exit 3): "),
        "{run:?}"
    );
    assert!(run.stderr.contains("beacon isn't running here"), "{run:?}");
    assert_eq!(p.calls(), [JOIN_ALL], "no peek and no check-in");
}

#[test]
fn other_join_failures_are_passed_on() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let run = check(
        &p,
        &[],
        &[
            ("FAKE_JOIN_STATUS", "4"),
            ("FAKE_JOIN_ERR", "beacon: wrong BEACON_SECRET"),
        ],
    );
    assert_eq!((run.code, run.stdout.as_str()), (4, ""), "{run:?}");
    assert!(
        run.stderr
            .contains("could not join the channels as pc-dispatch (exit 4): "),
        "{run:?}"
    );
    assert!(run.stderr.contains("wrong BEACON_SECRET"), "{run:?}");
    assert_eq!(p.calls(), [JOIN_ALL], "no peek and no check-in");
}

/// What `wait` says on stderr for a handle with no joins.
const NO_JOINS_HINT: &str = "beacon: pc-dispatch hasn't joined any channel, so only @mentions \
                             and announcements reach it; join your project's channel and \
                             general (./beacon join CHANNEL)";

/// The peek's stderr lands in what the script reads; a hint there is no post.
#[test]
fn the_no_joins_hint_is_not_counted() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let run = check(
        &p,
        &[],
        &[("FAKE_WAIT_STATUS", "6"), ("FAKE_WAIT_ERR", NO_JOINS_HINT)],
    );
    assert_eq!(
        (run.code, run.stdout.as_str()),
        (0, "nothing new\n"),
        "{run:?}"
    );
    let run = check(
        &p,
        &[],
        &[
            ("FAKE_WAIT_STATUS", "0"),
            ("FAKE_WAIT_OUT", TWO_UNREAD),
            ("FAKE_WAIT_ERR", NO_JOINS_HINT),
        ],
    );
    assert_eq!(
        (run.code, run.stdout.as_str()),
        (0, "unread: 2\n"),
        "{run:?}"
    );
}

/// What the script says when joining every channel left it in none.
const NO_CHANNELS_WARNING: &str = "dispatch-check: warning: pc-dispatch has joined no channels, \
                                   so only @mentions and announcements reach it\n";

/// Joined to nothing after joining everything: the board has no channel.
#[test]
fn no_channels_at_all_is_a_warning() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let run = check(
        &p,
        &[],
        &[(
            "FAKE_JOIN_OUT",
            r#"{"v":1,"ok":true,"added":[],"joined":[]}"#,
        )],
    );
    assert_eq!(
        (run.code, run.stdout.as_str(), run.stderr.as_str()),
        (0, "nothing new\n", NO_CHANNELS_WARNING),
        "{run:?}"
    );
    let calls = p.calls();
    assert_eq!(calls.len(), 3, "it still peeks and checks in: {calls:?}");
    assert_checked_in(&status_set(&calls[2], "pc-dispatch"), "");

    // Anything joined, just now or before: no warning.
    for out in [
        r#"{"v":1,"ok":true,"added":["beacon"],"joined":["beacon"]}"#,
        r#"{"v":1,"ok":true,"added":[],"joined":["general"]}"#,
    ] {
        let run = check(&p, &[], &[("FAKE_JOIN_OUT", out)]);
        assert_eq!(
            (run.code, run.stdout.as_str(), run.stderr.as_str()),
            (0, "nothing new\n", ""),
            "{out}: {run:?}"
        );
    }
}

#[test]
fn handle_prints_the_handle_and_asks_nothing() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let run = check(&p, &["--handle"], &[]);
    assert_eq!(
        (run.code, run.stdout.as_str()),
        (0, "pc-dispatch\n"),
        "{run:?}"
    );
    assert!(p.calls().is_empty());
}

#[test]
fn the_node_is_read_from_env_files_as_compose_reads_them() {
    for env in ENV_SPELLINGS_OF_PC {
        let p = Project::new(Some(env));
        let run = check(&p, &["--handle"], &[]);
        assert_eq!(
            (run.code, run.stdout.as_str()),
            (0, "pc-dispatch\n"),
            "{env:?}: {run:?}"
        );
        check(&p, &[], &[]);
        let calls = p.calls();
        assert_eq!(calls[0], JOIN_ALL, "{env:?}");
        assert_eq!(calls[1], PEEK, "{env:?}");
    }
}

#[test]
fn an_explicit_handle_wins() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let run = check(&p, &[], &[("BEACON_DISPATCH_HANDLE", "pc-ops")]);
    assert_eq!(run.code, 0, "{run:?}");
    let calls = p.calls();
    assert_eq!(calls.len(), 3, "{calls:?}");
    for call in &calls[..2] {
        assert!(
            call.windows(2).any(|w| w == ["--as", "pc-ops"]),
            "{calls:?}"
        );
    }
    status_set(&calls[2], "pc-ops");
}

#[test]
fn no_node_is_a_clear_error() {
    for env in [None, Some("BEACON_SECRET=s\n"), Some("BEACON_NODE=\"\"\n")] {
        let p = Project::new(env);
        for args in [&[][..], &["--handle"][..]] {
            let run = check(&p, args, &[]);
            assert_eq!((run.code, run.stdout.as_str()), (1, ""), "{env:?}: {run:?}");
            assert!(run.stderr.contains("BEACON_NODE"), "{run:?}");
        }
        assert!(p.calls().is_empty());
    }
}

#[test]
fn a_bad_argument_is_a_usage_error() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    for arg in ["two", "-1", "--json"] {
        let run = check(&p, &[arg], &[]);
        assert_eq!((run.code, run.stdout.as_str()), (1, ""), "{arg}: {run:?}");
        assert!(run.stderr.contains("usage"), "{run:?}");
    }
    assert!(p.calls().is_empty());
}

/// The status `handle` has on the board, as `who --json` says.
fn status_on_board(p: &Project, handle: &str) -> String {
    let who: Value = serde_json::from_str(&p.beacon(&["who", "--json"])).unwrap();
    let agents = who["agents"].as_array().unwrap();
    let me = agents
        .iter()
        .find(|a| a["handle"] == handle)
        .unwrap_or_else(|| panic!("{handle} not in {who}"));
    me["status"].as_str().unwrap().to_string()
}

/// The channels `handle` has joined, as the node has them.
async fn joins_on_node(node: &common::TestNode, handle: &str) -> Value {
    let joins: Value = node
        .get(&format!("/api/joins/{handle}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    joins["channels"].clone()
}

/// Bug 8JS7T1E5: a dispatcher that never joined a channel is woken only by
/// mentions and announcements, so a plain request in a channel never counted
/// and the check said `nothing new` while the request sat on the board.
#[tokio::test(flavor = "multi_thread")]
async fn a_dispatcher_that_never_joined_sees_plain_channel_posts() {
    let node = common::spawn_node("pc", &[]).await;
    let p = Project::real(Some("BEACON_NODE=pc\n"), &node.url, &node.secret);
    tokio::task::block_in_place(|| {
        p.beacon(&["channel", "new", "beacon"]);
        p.beacon(&[
            "post",
            "beacon",
            "please bump the deps",
            "--as",
            "pc-configs",
        ]);
        let run = check(&p, &[], &[]);
        assert_eq!(
            (run.code, run.stdout.as_str()),
            (0, "unread: 1\n"),
            "{run:?}"
        );
    });
}

/// Every check joins every channel, so one made after the last check counts
/// too; and the check only joins and peeks: nothing gets marked read.
#[tokio::test(flavor = "multi_thread")]
async fn each_check_joins_the_channels_made_since_the_last() {
    let node = common::spawn_node("pc", &[]).await;
    let p = Project::real(Some("BEACON_NODE=pc\n"), &node.url, &node.secret);
    let run = tokio::task::block_in_place(|| check(&p, &[], &[]));
    assert_eq!(
        (run.code, run.stdout.as_str(), run.stderr.as_str()),
        (0, "nothing new\n", ""),
        "{run:?}"
    );
    assert_eq!(
        joins_on_node(&node, "pc-dispatch").await,
        json!(["general"])
    );

    let run = tokio::task::block_in_place(|| {
        p.beacon(&["channel", "new", "later"]);
        p.beacon(&["post", "later", "a request", "--as", "pc-configs"]);
        check(&p, &[], &[])
    });
    assert_eq!(
        (run.code, run.stdout.as_str()),
        (0, "unread: 1\n"),
        "{run:?}"
    );
    assert_eq!(
        joins_on_node(&node, "pc-dispatch").await,
        json!(["general", "later"])
    );

    let unread = tokio::task::block_in_place(|| {
        p.beacon(&[
            "read",
            "--unread",
            "--json",
            "--as",
            "pc-dispatch",
            "--client",
            "claude-code",
        ])
    });
    let unread: Value = serde_json::from_str(&unread).unwrap();
    let posts = unread["posts"].as_array().unwrap();
    assert_eq!(posts.len(), 1, "{unread}");
    assert_eq!(posts[0]["body"], "a request", "{unread}");
}

/// The real `join --all --json` on a board with no channel at all, as the
/// script reads it: a warning, and the check goes on.
#[tokio::test(flavor = "multi_thread")]
async fn a_board_without_channels_is_a_warning() {
    let node = common::spawn_node("pc", &[]).await;
    let p = Project::real(Some("BEACON_NODE=pc\n"), &node.url, &node.secret);
    tokio::task::block_in_place(|| {
        p.beacon(&["channel", "delete", "general"]);
        let run = check(&p, &[], &[]);
        assert_eq!(
            (run.code, run.stdout.as_str(), run.stderr.as_str()),
            (0, "nothing new\n", NO_CHANNELS_WARNING),
            "{run:?}"
        );
        assert_checked_in(&status_on_board(&p, "pc-dispatch"), "");
    });
}

/// The real CLI against a real node: the JSON the script counts (a page of
/// posts and `remaining` beyond it), the call that sets the status, and that
/// nothing gets marked read.
#[tokio::test(flavor = "multi_thread")]
async fn against_a_real_node() {
    let node = common::spawn_node("pc", &[]).await;
    let p = Project::real(Some("BEACON_NODE=pc\n"), &node.url, &node.secret);
    tokio::task::spawn_blocking(move || {
        let me = ["--as", "pc-dispatch", "--client", "claude-code"];
        // No `join`: the check joins every channel itself.
        p.beacon(&["channel", "new", "beacon"]);

        let run = check(&p, &[], &[]);
        assert_eq!(
            (run.code, run.stdout.as_str()),
            (0, "nothing new\n"),
            "{run:?}"
        );
        assert_checked_in(&status_on_board(&p, "pc-dispatch"), "");

        // One more than a page of unread posts (the node sends 50 at most).
        for n in 0..51 {
            let body = format!("request {n}");
            p.beacon(&["post", "beacon", &body, "--as", "pc-configs"]);
        }
        let run = check(&p, &["2"], &[]);
        assert_eq!(
            (run.code, run.stdout.as_str()),
            (0, "unread: 51\n"),
            "{run:?}"
        );
        assert_checked_in(&status_on_board(&p, "pc-dispatch"), " · 2 tasks running");

        // Only peeked: all of it is still unread.
        let run = check(&p, &[], &[]);
        assert_eq!(run.stdout, "unread: 51\n", "{run:?}");
        let unread: Value =
            serde_json::from_str(&p.beacon(&[&["read", "--unread", "--json"][..], &me].concat()))
                .unwrap();
        assert_eq!(unread["posts"].as_array().unwrap().len(), 50);
        assert_eq!(unread["remaining"], 1);
    })
    .await
    .unwrap();
}
