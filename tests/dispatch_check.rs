//! Tests for `scripts/dispatch-check.sh`, the one cheap call a beacon dispatcher
//! makes on every `/loop` tick: join every channel, peek at the board, check
//! in, and say whether there is anything to do. The project and its fake
//! `beacon` are in `fake_beacon`.

mod common;
mod fake_beacon;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use fake_beacon::{Project, Run, ENV_SPELLINGS_OF_PC, STATE, TWO_UNREAD};
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
    assert_checked_in(&status_set(&calls[2], "pc-dispatch"), " · every 2m");
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
    assert_checked_in(&status_set(&calls[2], "pc-dispatch"), " · every 2m");
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
        ("0", " · every 2m"),
        ("1", " · every 2m · 1 task running"),
        ("3", " · every 2m · 3 tasks running"),
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
const NO_JOINS_HINT: &str = "beacon: pc-dispatch hasn't joined any channel, so only @mentions, \
                             announcements and replies to its posts reach it; join your project's channel and \
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
                                   so only @mentions, announcements and replies to its posts reach it\n";

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
    assert_checked_in(&status_set(&calls[2], "pc-dispatch"), " · every 2m");

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
        for args in [&[][..], &["--handle"], &["--interval"], &["--reset"]] {
            let run = check(&p, args, &[]);
            assert_eq!((run.code, run.stdout.as_str()), (1, ""), "{env:?}: {run:?}");
            assert!(run.stderr.contains("BEACON_NODE"), "{run:?}");
        }
        assert!(p.calls().is_empty());
        assert!(!p.path().join(STATE).exists());
    }
}

#[test]
fn a_bad_argument_is_a_usage_error() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    for args in [
        &["two"][..],
        &["-1"],
        &["--json"],
        &["1", "2"],
        &["--hold", "x"],
        &["--handle", "--hold"],
        &["2", "--handle"],
        &["--interval", "2"],
        &["--reset", "--interval"],
        &["--handle", "--interval"],
    ] {
        let run = check(&p, args, &[]);
        assert_eq!(
            (run.code, run.stdout.as_str()),
            (1, ""),
            "{args:?}: {run:?}"
        );
        assert!(run.stderr.contains("usage"), "{run:?}");
    }
    assert!(p.calls().is_empty());
    assert!(!p.path().join(STATE).exists());
}

// The loop's pace. A quiet check (nothing unread, no worker running, no
// `--hold`) counts; 15 in a row at 2m back the loop off to 5m, 12 more at 5m to
// 10m. Anything to do brings it back to 2m. A second line, `backoff: Nm`, says
// when a check changed the pace.

/// The status the last call set, as `handle`.
fn last_status(p: &Project, handle: &str) -> String {
    let calls = p.calls();
    status_set(calls.last().expect("no calls"), handle)
}

/// `n` quiet checks in a row, none of which changes the pace.
fn quiet_checks(p: &Project, n: usize, args: &[&str], env: &[(&str, &str)]) {
    for i in 1..=n {
        let run = check(p, args, env);
        assert_eq!(
            (run.code, run.stdout.as_str(), run.stderr.as_str()),
            (0, "nothing new\n", ""),
            "quiet check {i} of {n}: {run:?}"
        );
    }
}

/// One quiet check, which must change the pace to `pace`.
fn backs_off(p: &Project, env: &[(&str, &str)], pace: &str) {
    let run = check(p, &[], env);
    assert_eq!(
        (run.code, run.stdout.clone(), run.stderr.as_str()),
        (0, format!("nothing new\nbackoff: {pace}\n"), ""),
        "{run:?}"
    );
}

/// Quiet checks from 2m until the loop has backed off to `pace`.
fn back_off_to(p: &Project, pace: &str) {
    quiet_checks(p, 14, &[], &[]);
    backs_off(p, &[], "5m");
    if pace == "10m" {
        quiet_checks(p, 11, &[], &[]);
        backs_off(p, &[], "10m");
    }
}

/// What `--interval` prints, after making sure it asked beacon nothing.
fn pace(p: &Project, env: &[(&str, &str)]) -> String {
    let before = p.calls().len();
    let run = check(p, &["--interval"], env);
    assert_eq!((run.code, run.stderr.as_str()), (0, ""), "{run:?}");
    assert_eq!(p.calls().len(), before, "--interval asks beacon nothing");
    run.stdout
}

/// Root can write anywhere, so permission cases have nothing to check for it.
fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

#[test]
fn a_quiet_board_backs_the_loop_off_to_5m_then_10m() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    // No workers running is the same as not saying.
    quiet_checks(&p, 7, &[], &[]);
    quiet_checks(&p, 7, &["0"], &[]);
    assert_checked_in(&last_status(&p, "pc-dispatch"), " · every 2m");
    backs_off(&p, &[], "5m");
    assert_checked_in(&last_status(&p, "pc-dispatch"), " · every 5m");

    quiet_checks(&p, 11, &[], &[]);
    assert_checked_in(&last_status(&p, "pc-dispatch"), " · every 5m");
    backs_off(&p, &[], "10m");
    assert_checked_in(&last_status(&p, "pc-dispatch"), " · every 10m");

    // 10m is as slow as it gets.
    quiet_checks(&p, 30, &[], &[]);
    assert_checked_in(&last_status(&p, "pc-dispatch"), " · every 10m");
}

/// A check with something to do, and what it says.
struct Busy<'a> {
    what: &'a str,
    args: &'a [&'a str],
    env: &'a [(&'a str, &'a str)],
    first_line: &'a str,
    status: &'a str,
}

/// Unread work, a running worker and `--hold` each bring the loop back to 2m,
/// saying so only when the pace was slower, and start the count over.
#[test]
fn anything_to_do_brings_the_loop_back_to_2m() {
    let unread = [("FAKE_WAIT_STATUS", "0"), ("FAKE_WAIT_OUT", TWO_UNREAD)];
    let busy = [
        Busy {
            what: "unread work",
            args: &[],
            env: &unread,
            first_line: "unread: 2\n",
            status: " · every 2m",
        },
        Busy {
            what: "a worker",
            args: &["1"],
            env: &[],
            first_line: "nothing new\n",
            status: " · every 2m · 1 task running",
        },
        Busy {
            what: "a hold",
            args: &["--hold"],
            env: &[],
            first_line: "nothing new\n",
            status: " · every 2m",
        },
    ];
    for Busy {
        what,
        args,
        env,
        first_line,
        status,
    } in busy
    {
        let p = Project::new(Some("BEACON_NODE=pc\n"));
        for from in ["5m", "10m"] {
            back_off_to(&p, from);
            let run = check(&p, args, env);
            assert_eq!(
                (run.code, run.stdout.clone(), run.stderr.as_str()),
                (0, format!("{first_line}backoff: 2m\n"), ""),
                "{what} at {from}: {run:?}"
            );
            assert_checked_in(&last_status(&p, "pc-dispatch"), status);
            assert_eq!(pace(&p, &[]), "2m\n", "{what} at {from}");
        }

        // Already at 2m, after 14 quiet checks: no line, and the count starts
        // over, so it takes 15 more quiet checks to back off.
        quiet_checks(&p, 14, &[], &[]);
        let run = check(&p, args, env);
        assert_eq!(
            (run.code, run.stdout.as_str(), run.stderr.as_str()),
            (0, first_line, ""),
            "{what} at 2m: {run:?}"
        );
        assert_checked_in(&last_status(&p, "pc-dispatch"), status);
        quiet_checks(&p, 14, &[], &[]);
        backs_off(&p, &[], "5m");
    }
}

/// The first check of a session (`--reset`) starts at 2m, like the fresh
/// `/loop 2m` it runs in, whatever an earlier session left; it never prints a
/// backoff line, and it is a normal check besides.
#[test]
fn reset_starts_the_pace_over_silently_then_checks() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    back_off_to(&p, "10m");
    let calls = p.calls().len();
    let run = check(&p, &["--reset"], &[]);
    assert_eq!(
        (run.code, run.stdout.as_str(), run.stderr.as_str()),
        (0, "nothing new\n", ""),
        "{run:?}"
    );
    let all = p.calls();
    assert_eq!(all.len(), calls + 3, "a whole check: {all:?}");
    assert_eq!(all[calls], JOIN_ALL);
    assert_eq!(all[calls + 1], PEEK);
    assert_checked_in(&last_status(&p, "pc-dispatch"), " · every 2m");
    assert_eq!(pace(&p, &[]), "2m\n");
    // The reset check was the first quiet one at 2m.
    quiet_checks(&p, 13, &[], &[]);
    backs_off(&p, &[], "5m");

    // From 5m, with unread work and workers: still no line.
    let run = check(
        &p,
        &["--reset", "2"],
        &[("FAKE_WAIT_STATUS", "0"), ("FAKE_WAIT_OUT", TWO_UNREAD)],
    );
    assert_eq!(
        (run.code, run.stdout.as_str(), run.stderr.as_str()),
        (0, "unread: 2\n", ""),
        "{run:?}"
    );
    assert_checked_in(
        &last_status(&p, "pc-dispatch"),
        " · every 2m · 2 tasks running",
    );

    // On a fresh state too.
    let fresh = Project::new(Some("BEACON_NODE=pc\n"));
    let run = check(&fresh, &["--reset"], &[]);
    assert_eq!(
        (run.code, run.stdout.as_str(), run.stderr.as_str()),
        (0, "nothing new\n", ""),
        "{run:?}"
    );
}

/// A session's first check resets the pace even when beacon is down, so the
/// session's later checks don't carry on from an earlier session's pace.
#[test]
fn reset_takes_even_when_the_check_fails() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    back_off_to(&p, "10m");
    let run = check(&p, &["--reset"], &[("FAKE_WAIT_STATUS", "3")]);
    assert_eq!((run.code, run.stdout.as_str()), (3, ""), "{run:?}");
    assert_eq!(pace(&p, &[]), "2m\n");
}

/// A check that fails before it peeked knows nothing about the board: it
/// counts neither as quiet nor as busy.
#[test]
fn a_failed_check_leaves_the_pace_alone() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    quiet_checks(&p, 14, &[], &[]);
    for (var, code) in [
        ("FAKE_JOIN_STATUS", 3),
        ("FAKE_JOIN_STATUS", 4),
        ("FAKE_WAIT_STATUS", 3),
        ("FAKE_WAIT_STATUS", 4),
    ] {
        let run = check(&p, &[], &[(var, &code.to_string())]);
        assert_eq!((run.code, run.stdout.as_str()), (code, ""), "{run:?}");
    }
    backs_off(&p, &[], "5m");

    // Nor does a failed check with a worker running bring it back to 2m.
    quiet_checks(&p, 11, &[], &[]);
    backs_off(&p, &[], "10m");
    let run = check(&p, &["1"], &[("FAKE_WAIT_STATUS", "3")]);
    assert_eq!((run.code, run.stdout.as_str()), (3, ""), "{run:?}");
    assert_eq!(pace(&p, &[]), "10m\n");
}

#[test]
fn interval_prints_the_pace_and_asks_nothing() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    assert_eq!(pace(&p, &[]), "2m\n");
    assert!(p.calls().is_empty());
    assert!(!p.path().join(STATE).exists(), "asking writes nothing");

    // Asking is no check: it doesn't count as a quiet one.
    quiet_checks(&p, 10, &[], &[]);
    for _ in 0..5 {
        assert_eq!(pace(&p, &[]), "2m\n");
    }
    quiet_checks(&p, 4, &[], &[]);
    backs_off(&p, &[], "5m");
    assert_eq!(pace(&p, &[]), "5m\n");

    quiet_checks(&p, 11, &[], &[]);
    backs_off(&p, &[], "10m");
    assert_eq!(pace(&p, &[]), "10m\n");
}

#[test]
fn options_come_in_any_order() {
    let cases: [(&[&str], &str, &str); 6] = [
        (
            &["--hold", "0"],
            "nothing new\nbackoff: 2m\n",
            " · every 2m",
        ),
        (
            &["0", "--hold"],
            "nothing new\nbackoff: 2m\n",
            " · every 2m",
        ),
        (
            &["--reset", "2"],
            "nothing new\n",
            " · every 2m · 2 tasks running",
        ),
        (
            &["2", "--reset"],
            "nothing new\n",
            " · every 2m · 2 tasks running",
        ),
        (
            &["--hold", "--reset", "2"],
            "nothing new\n",
            " · every 2m · 2 tasks running",
        ),
        (
            &["2", "--reset", "--hold"],
            "nothing new\n",
            " · every 2m · 2 tasks running",
        ),
    ];
    for (args, stdout, status) in cases {
        let p = Project::new(Some("BEACON_NODE=pc\n"));
        back_off_to(&p, "5m");
        let run = check(&p, args, &[]);
        assert_eq!(
            (run.code, run.stdout.as_str(), run.stderr.as_str()),
            (0, stdout, ""),
            "{args:?}: {run:?}"
        );
        assert_checked_in(&last_status(&p, "pc-dispatch"), status);
        assert_eq!(pace(&p, &[]), "2m\n", "{args:?}");
    }
}

/// Whatever is in the state file, if it isn't a state the script wrote, the
/// check starts afresh at 2m with no quiet checks counted, and says nothing.
#[test]
fn a_garbled_state_is_a_fresh_start() {
    for garbage in [
        "",
        "\n",
        "nonsense\n",
        "7m 3\n",
        "5m\n",
        "5m x\n",
        "2m 14x\n",
        "2m -1\n",
        "2m 014\n",
        "2m 99999999999999999999\n",
        "5m 3 more\n",
        "m 3\n",
        "\u{0}\u{1}\u{fffd}",
    ] {
        let p = Project::new(Some("BEACON_NODE=pc\n"));
        p.write(STATE, garbage);
        assert_eq!(pace(&p, &[]), "2m\n", "{garbage:?}");
        quiet_checks(&p, 14, &[], &[]);
        backs_off(&p, &[], "5m");
    }
}

/// A state file the script can't read is a fresh start too, and the check
/// replaces it.
#[test]
fn an_unreadable_state_is_a_fresh_start() {
    if is_root() {
        return;
    }
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    back_off_to(&p, "5m");
    let state = p.path().join(STATE);
    fs::set_permissions(&state, fs::Permissions::from_mode(0o000)).unwrap();
    assert_eq!(pace(&p, &[]), "2m\n");
    quiet_checks(&p, 14, &[], &[]);
    backs_off(&p, &[], "5m");
}

/// The state's directory is made when it's missing, and the state is written
/// whole through a temporary file that doesn't stay behind.
#[test]
fn a_missing_state_directory_is_made() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let dir = p.path().join("deep").join("er");
    let state = dir.join("dispatch.state");
    let env = [("BEACON_DISPATCH_STATE", state.to_str().unwrap())];
    assert_eq!(pace(&p, &env), "2m\n");
    assert!(!dir.exists(), "asking makes nothing");
    quiet_checks(&p, 14, &[], &env);
    backs_off(&p, &env, "5m");
    assert_eq!(pace(&p, &env), "5m\n");
    let files: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(files, ["dispatch.state"]);
}

/// When the state can't be saved, the check still works, warns, and keeps
/// the loop at 2m.
#[test]
fn an_unwritable_state_warns_and_stays_at_2m() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    p.write("a-file", "not a directory\n");
    let under_a_file = p.path().join("a-file").join("dispatch.state");
    let a_directory = p.path().join("a-directory");
    fs::create_dir(&a_directory).unwrap();
    for state in [&under_a_file, &a_directory] {
        let env = [("BEACON_DISPATCH_STATE", state.to_str().unwrap())];
        for flags in [&[][..], &["--reset"]] {
            for i in 1..=20 {
                let run = check(&p, flags, &env);
                assert_eq!(
                    (run.code, run.stdout.as_str()),
                    (0, "nothing new\n"),
                    "{state:?} {flags:?} check {i}: {run:?}"
                );
                assert!(
                    run.stderr.starts_with("dispatch-check: warning: ")
                        && run.stderr.contains(state.to_str().unwrap())
                        && run.stderr.ends_with('\n')
                        && run.stderr.lines().count() == 1,
                    "{state:?} {flags:?} check {i}: {run:?}"
                );
                assert_checked_in(&last_status(&p, "pc-dispatch"), " · every 2m");
            }
        }
        assert_eq!(pace(&p, &env), "2m\n", "{state:?}");
    }
    assert_eq!(
        fs::read_dir(&a_directory).unwrap().count(),
        0,
        "nothing left in the directory"
    );
}

/// A read-only home: no state directory can be made there.
#[test]
fn a_read_only_home_warns_and_stays_at_2m() {
    if is_root() {
        return;
    }
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let home = p.path().join("home");
    fs::create_dir(&home).unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o555)).unwrap();
    let env = [
        ("BEACON_DISPATCH_STATE", ""),
        ("XDG_STATE_HOME", ""),
        ("HOME", home.to_str().unwrap()),
    ];
    for i in 1..=20 {
        let run = check(&p, &[], &env);
        assert_eq!(
            (run.code, run.stdout.as_str()),
            (0, "nothing new\n"),
            "check {i}: {run:?}"
        );
        assert!(
            run.stderr.starts_with("dispatch-check: warning: "),
            "check {i}: {run:?}"
        );
        assert_checked_in(&last_status(&p, "pc-dispatch"), " · every 2m");
    }
    fs::set_permissions(&home, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A state that was saved before but can't be saved now (its directory went
/// read-only): the loop goes back to 2m, the pace it can keep without one.
#[test]
fn a_state_that_cannot_be_saved_any_more_goes_back_to_2m() {
    if is_root() {
        return;
    }
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let dir = p.path().join("states");
    let state = dir.join("dispatch.state");
    let env = [("BEACON_DISPATCH_STATE", state.to_str().unwrap())];
    quiet_checks(&p, 14, &[], &env);
    backs_off(&p, &env, "5m");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
    let run = check(&p, &[], &env);
    let reset = check(&p, &["--reset"], &env);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        (run.code, run.stdout.as_str()),
        (0, "nothing new\nbackoff: 2m\n"),
        "{run:?}"
    );
    assert!(
        run.stderr.starts_with("dispatch-check: warning: "),
        "{run:?}"
    );
    assert_checked_in(&last_status(&p, "pc-dispatch"), " · every 2m");

    // Starting over says nothing about the pace, even then.
    assert_eq!(
        (reset.code, reset.stdout.as_str()),
        (0, "nothing new\n"),
        "{reset:?}"
    );
    assert_eq!(reset.stderr.lines().count(), 1, "{reset:?}");
}

/// Without `BEACON_DISPATCH_STATE`, each handle keeps its own state under
/// `$XDG_STATE_HOME/beacon`.
#[test]
fn each_handle_keeps_its_own_pace() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let xdg = p.path().join("xdg");
    let xdg = xdg.to_str().unwrap();
    let pc = [("BEACON_DISPATCH_STATE", ""), ("XDG_STATE_HOME", xdg)];
    let ops = [
        ("BEACON_DISPATCH_STATE", ""),
        ("XDG_STATE_HOME", xdg),
        ("BEACON_DISPATCH_HANDLE", "pc-ops"),
    ];
    quiet_checks(&p, 14, &[], &pc);
    backs_off(&p, &pc, "5m");
    assert_eq!(pace(&p, &pc), "5m\n");
    assert_eq!(pace(&p, &ops), "2m\n");

    quiet_checks(&p, 14, &[], &ops);
    backs_off(&p, &ops, "5m");
    assert_checked_in(&last_status(&p, "pc-ops"), " · every 5m");
    quiet_checks(&p, 11, &[], &ops);
    backs_off(&p, &ops, "10m");
    assert_eq!(pace(&p, &pc), "5m\n");

    let dir = Path::new(xdg).join("beacon");
    assert!(dir.join("dispatch-pc-dispatch.state").is_file());
    assert!(dir.join("dispatch-pc-ops.state").is_file());
    assert!(!p.path().join(STATE).exists());
}

/// With no `XDG_STATE_HOME` either, the state is under `~/.local/state`.
#[test]
fn without_xdg_state_home_the_state_is_in_home() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let home = p.path().join("home");
    let env = [
        ("BEACON_DISPATCH_STATE", ""),
        ("XDG_STATE_HOME", ""),
        ("HOME", home.to_str().unwrap()),
    ];
    quiet_checks(&p, 1, &[], &env);
    assert!(home
        .join(".local/state/beacon/dispatch-pc-dispatch.state")
        .is_file());
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
        assert_checked_in(&status_on_board(&p, "pc-dispatch"), " · every 2m");
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
        assert_checked_in(&status_on_board(&p, "pc-dispatch"), " · every 2m");

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
        assert_checked_in(
            &status_on_board(&p, "pc-dispatch"),
            " · every 2m · 2 tasks running",
        );

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
