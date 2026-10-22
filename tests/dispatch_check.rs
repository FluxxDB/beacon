//! Tests for `scripts/dispatch-check.sh`, the one cheap call a beacon dispatcher
//! makes on every `/loop` tick: peek at the board, check in, and say whether
//! there is anything to do. The project and its fake `beacon` are in
//! `fake_beacon`.

mod common;
mod fake_beacon;

use fake_beacon::{Project, Run, ENV_SPELLINGS_OF_PC, TWO_UNREAD};
use serde_json::Value;

const CHECK: &str = "dispatch-check.sh";

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
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0], PEEK);
    assert_checked_in(&status_set(&calls[1], "pc-dispatch"), "");
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
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0], PEEK);
    assert_checked_in(&status_set(&calls[1], "pc-dispatch"), "");
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
        assert_checked_in(&status_set(&calls[1], "pc-dispatch"), rest);
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
    assert_eq!(p.calls().len(), 1, "no check-in without a server");
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
        assert_eq!(p.calls()[0], PEEK, "{env:?}");
    }
}

#[test]
fn an_explicit_handle_wins() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let run = check(&p, &[], &[("BEACON_DISPATCH_HANDLE", "pc-ops")]);
    assert_eq!(run.code, 0, "{run:?}");
    let calls = p.calls();
    assert!(
        calls[0].windows(2).any(|w| w == ["--as", "pc-ops"]),
        "{calls:?}"
    );
    status_set(&calls[1], "pc-ops");
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

/// The real CLI against a real node: the JSON the script counts (a page of
/// posts and `remaining` beyond it), the call that sets the status, and that
/// nothing gets marked read.
#[tokio::test(flavor = "multi_thread")]
async fn against_a_real_node() {
    let node = common::spawn_node("pc", &[]).await;
    let p = Project::real(Some("BEACON_NODE=pc\n"), &node.url, &node.secret);
    tokio::task::spawn_blocking(move || {
        let me = ["--as", "pc-dispatch", "--client", "claude-code"];
        p.beacon(&["channel", "new", "beacon"]);
        p.beacon(&[&["join", "beacon"][..], &me].concat());

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

/// Bug 8JS7T1E5, replayed on a real node: the dispatcher asks in #beacon
/// without joining it, and pc-beacon answers `re <id>:` mentioning only
/// Dang. The answer is unread for the dispatcher. The peek it makes (`wait`)
/// is checked before the script runs, with no channel joined, so it still
/// shows a reply alone reaching its asker if the script comes to join
/// channels itself.
#[tokio::test(flavor = "multi_thread")]
async fn the_answer_to_the_dispatchers_question_is_unread() {
    let node = common::spawn_node("pc", &[]).await;
    let p = Project::real(Some("BEACON_NODE=pc\n"), &node.url, &node.secret);
    let me = ["--as", "pc-dispatch", "--client", "claude-code"];
    let (p, answer) = tokio::task::spawn_blocking(move || {
        p.beacon(&["channel", "new", "beacon"]);
        let ask = [
            &["post", "beacon", "is task G merged yet?", "--json"][..],
            &me,
        ]
        .concat();
        let asked: Value = serde_json::from_str(&p.beacon(&ask)).unwrap();
        let answer = format!(
            "re {}: not yet, @Dang has it",
            asked["post"]["short_id"].as_str().unwrap()
        );
        p.beacon(&["post", "beacon", &answer, "--as", "pc-beacon"]);
        (p, answer)
    })
    .await
    .unwrap();
    let joins: Value = node
        .get("/api/joins/pc-dispatch")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(joins["channels"], serde_json::json!([]));

    tokio::task::spawn_blocking(move || {
        let peek = std::process::Command::new(p.path().join("beacon"))
            .args([&["wait", "--timeout", "0", "--json"][..], &me].concat())
            .output()
            .unwrap();
        assert_eq!(peek.status.code(), Some(0), "nothing to wake it: {peek:?}");
        let page: Value = serde_json::from_slice(&peek.stdout).unwrap();
        assert_eq!(page["posts"].as_array().unwrap().len(), 1, "{page}");
        assert_eq!(page["posts"][0]["body"], answer.as_str());

        let run = check(&p, &[], &[]);
        assert_eq!(
            (run.code, run.stdout.as_str()),
            (0, "unread: 1\n"),
            "{run:?}"
        );
    })
    .await
    .unwrap();
}
