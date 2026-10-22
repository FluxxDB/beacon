//! Tests for `scripts/dispatch-stop-hook.sh`, the Claude Code Stop hook that keeps
//! a beacon dispatcher session from going idle with unread work. The project
//! and its fake `beacon` are in `fake_beacon`.

mod common;
mod fake_beacon;

use std::fs;
use std::path::{Path, PathBuf};

use fake_beacon::{Project, Run, ENV_SPELLINGS_OF_PC, MARKER, REPO, TWO_UNREAD};
use serde_json::{json, Value};

const HOOK: &str = "dispatch-stop-hook.sh";

fn hook_input(transcript: &Path, stop_hook_active: bool) -> String {
    json!({
        "session_id": "s1",
        "transcript_path": transcript,
        "hook_event_name": "Stop",
        "stop_hook_active": stop_hook_active,
    })
    .to_string()
}

/// One transcript line per record, as Claude Code writes them.
fn transcript(p: &Project, records: &[Value]) -> PathBuf {
    let mut text = String::new();
    for r in records {
        text.push_str(&r.to_string());
        text.push('\n');
    }
    p.write("transcript.jsonl", &text)
}

/// How Claude Code records a slash command's (or skill's) expanded text.
fn command_expansion(text: &str) -> Value {
    json!({
        "parentUuid": null,
        "isSidechain": false,
        "type": "user",
        "message": {"role": "user", "content": [{"type": "text", "text": text}]},
        "isMeta": true,
        "uuid": "u1",
    })
}

/// A prompt the user typed.
fn typed_prompt(text: &str) -> Value {
    json!({"type": "user", "message": {"role": "user", "content": text}, "uuid": "u0"})
}

/// What a tool (Read, Bash...) gave back to the agent.
fn tool_result(text: &str) -> Value {
    json!({
        "type": "user",
        "message": {"role": "user", "content": [
            {"tool_use_id": "toolu_1", "type": "tool_result", "content": text}
        ]},
        "uuid": "u2",
    })
}

fn assistant_text(text: &str) -> Value {
    json!({
        "type": "assistant",
        "message": {"role": "assistant", "content": [{"type": "text", "text": text}]},
        "uuid": "a1",
    })
}

fn dispatch_command() -> String {
    format!("You are this device's beacon dispatcher. Dispatcher marker: {MARKER} (leave it).")
}

fn dispatcher_transcript(p: &Project) -> PathBuf {
    transcript(
        p,
        &[
            typed_prompt("/loop 2m /dispatch"),
            command_expansion(&dispatch_command()),
        ],
    )
}

fn unread(env: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
    let mut all = vec![("FAKE_WAIT_STATUS", "0"), ("FAKE_WAIT_OUT", TWO_UNREAD)];
    all.extend_from_slice(env);
    all
}

fn answer(run: &Run) -> Value {
    assert_eq!(run.code, 0, "{run:?}");
    serde_json::from_str(run.stdout.trim()).unwrap_or_else(|e| panic!("{e}: {run:?}"))
}

fn assert_silent(p: &Project, run: &Run) {
    assert_eq!((run.code, run.stdout.as_str()), (0, ""), "{run:?}");
    assert!(p.calls().is_empty(), "beacon must not even be asked");
}

#[test]
fn a_normal_session_is_left_alone() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let t = transcript(&p, &[typed_prompt("fix a bug")]);
    let run = p.run(HOOK, &[], &hook_input(&t, false), &unread(&[]));
    assert_silent(&p, &run);
}

#[test]
fn the_marker_in_a_tool_result_is_ignored() {
    // A session that read dispatch.md, this hook or its tests, or ran
    // `git show` on them, is not a dispatcher.
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let t = transcript(
        &p,
        &[
            typed_prompt("review the dispatcher commit"),
            assistant_text("Reading the command file."),
            tool_result(&format!(
                "1\t---\n2\tdescription: ...\n5\t{}",
                dispatch_command()
            )),
        ],
    );
    let run = p.run(HOOK, &[], &hook_input(&t, false), &unread(&[]));
    assert_silent(&p, &run);
}

#[test]
fn the_marker_in_an_assistant_message_is_ignored() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let t = transcript(
        &p,
        &[
            typed_prompt("what makes a session a dispatcher?"),
            assistant_text(&format!("The hook looks for {MARKER} in the prompts.")),
        ],
    );
    let run = p.run(HOOK, &[], &hook_input(&t, false), &unread(&[]));
    assert_silent(&p, &run);
}

#[test]
fn the_marker_in_a_background_agents_report_is_ignored() {
    // A subagent's report arrives as a user-role string, and a reviewer of the
    // dispatcher quotes the marker.
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let t = transcript(
        &p,
        &[
            typed_prompt("review the dispatcher in the background"),
            json!({
                "type": "user",
                "message": {"role": "user", "content": format!(
                    "<task-notification>\n<task-id>a1</task-id>\n<status>completed</status>\n\
                     <result>The command carries {MARKER} on line 5.</result>\n</task-notification>"
                )},
                "origin": {"kind": "task-notification"},
            }),
        ],
    );
    let run = p.run(HOOK, &[], &hook_input(&t, false), &unread(&[]));
    assert_silent(&p, &run);
}

#[test]
fn the_marker_in_a_compaction_summary_is_ignored() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let t = transcript(
        &p,
        &[json!({
            "type": "user",
            "message": {"role": "user", "content": format!(
                "This session is being continued from a previous conversation. Summary: \
                 we read dispatch.md, which carries {MARKER}."
            )},
            "isCompactSummary": true,
        })],
    );
    let run = p.run(HOOK, &[], &hook_input(&t, false), &unread(&[]));
    assert_silent(&p, &run);
}

#[test]
fn the_real_dispatch_command_makes_a_dispatcher() {
    // The body of .claude/commands/dispatch.md as Claude Code expands it: it
    // must carry the marker and nothing that the hook takes for a non-prompt.
    let command = fs::read_to_string(Path::new(REPO).join(".claude/commands/dispatch.md")).unwrap();
    let body = command
        .splitn(3, "---\n")
        .nth(2)
        .expect("frontmatter, then the body");
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let t = transcript(
        &p,
        &[typed_prompt("/loop 2m /dispatch"), command_expansion(body)],
    );
    let run = p.run(HOOK, &[], &hook_input(&t, false), &unread(&[]));
    assert_eq!(answer(&run)["decision"], "block");
}

#[test]
fn the_marker_in_a_user_text_message_makes_a_dispatcher() {
    for record in [
        command_expansion(&dispatch_command()),
        typed_prompt(&dispatch_command()),
    ] {
        let p = Project::new(Some("BEACON_NODE=pc\n"));
        let t = transcript(&p, &[record.clone(), tool_result("ok")]);
        let run = p.run(HOOK, &[], &hook_input(&t, false), &unread(&[]));
        assert_eq!(answer(&run)["decision"], "block", "{record}");
    }
}

#[test]
fn a_dispatcher_with_unread_work_is_kept_going() {
    let p = Project::new(Some("BEACON_SECRET=s\nBEACON_NODE=pc\n"));
    let t = dispatcher_transcript(&p);
    let run = p.run(HOOK, &[], &hook_input(&t, false), &unread(&[]));
    let answer = answer(&run);
    assert_eq!(answer["decision"], "block");
    assert_eq!(
        answer["reason"],
        "Beacon has at least 2 unread post(s) for pc-dispatch. Read them \
         (./beacon read --unread --as pc-dispatch --client claude-code); act only on what \
         /dispatch says is yours, then stop."
    );
}

#[test]
fn the_count_includes_the_posts_beyond_the_page() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let t = dispatcher_transcript(&p);
    let page = TWO_UNREAD.replace(r#""remaining":0"#, r#""remaining":48"#);
    let run = p.run(
        HOOK,
        &[],
        &hook_input(&t, false),
        &[("FAKE_WAIT_STATUS", "0"), ("FAKE_WAIT_OUT", &page)],
    );
    let reason = answer(&run)["reason"].as_str().unwrap().to_string();
    assert!(
        reason.starts_with("Beacon has at least 50 unread post(s)"),
        "{reason}"
    );
}

#[test]
fn it_peeks_as_the_devices_dispatcher_without_marking_read() {
    let p = Project::new(Some("BEACON_NODE=mac\n"));
    let t = dispatcher_transcript(&p);
    p.run(HOOK, &[], &hook_input(&t, false), &[]);
    assert_eq!(
        p.calls(),
        [[
            "wait",
            "--timeout",
            "0",
            "--as",
            "mac-dispatch",
            "--client",
            "claude-code",
            "--json"
        ]]
    );
}

#[test]
fn the_node_is_read_from_env_files_as_compose_reads_them() {
    for env in ENV_SPELLINGS_OF_PC {
        let p = Project::new(Some(env));
        let t = dispatcher_transcript(&p);
        p.run(HOOK, &[], &hook_input(&t, false), &[]);
        let calls = p.calls();
        assert_eq!(calls.len(), 1, "{env:?}: {calls:?}");
        assert!(
            calls[0].windows(2).any(|w| w == ["--as", "pc-dispatch"]),
            "{env:?}: {calls:?}"
        );
    }
}

#[test]
fn an_explicit_handle_wins() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let t = dispatcher_transcript(&p);
    p.run(
        HOOK,
        &[],
        &hook_input(&t, false),
        &[("BEACON_DISPATCH_HANDLE", "pc-ops")],
    );
    let calls = p.calls();
    assert!(
        calls[0].windows(2).any(|w| w == ["--as", "pc-ops"]),
        "{calls:?}"
    );
}

#[test]
fn nothing_unread_lets_it_stop() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let t = dispatcher_transcript(&p);
    let run = p.run(
        HOOK,
        &[],
        &hook_input(&t, false),
        &[("FAKE_WAIT_STATUS", "6")],
    );
    assert_eq!((run.code, run.stdout.as_str()), (0, ""), "{run:?}");
}

#[test]
fn a_second_stop_in_a_row_always_goes_through() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let t = dispatcher_transcript(&p);
    for active in [r#""stop_hook_active":true"#, r#""stop_hook_active": true"#] {
        let input = format!(r#"{{"transcript_path":"{}",{active}}}"#, t.display());
        let run = p.run(HOOK, &[], &input, &unread(&[]));
        assert_eq!((run.code, run.stdout.as_str()), (0, ""), "{input}");
    }
    assert!(p.calls().is_empty());
}

#[test]
fn a_down_server_is_reported_not_blocked() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let t = dispatcher_transcript(&p);
    let run = p.run(
        HOOK,
        &[],
        &hook_input(&t, false),
        &[("FAKE_WAIT_STATUS", "3")],
    );
    let answer = answer(&run);
    assert!(answer.get("decision").is_none(), "{answer}");
    assert_eq!(
        answer["systemMessage"],
        "Dispatcher: beacon unreachable from the Stop hook (stack down, or docker not on \
         the hook's PATH)"
    );
}

#[test]
fn a_failing_check_is_reported_not_blocked() {
    for status in ["1", "2", "4"] {
        let p = Project::new(Some("BEACON_NODE=pc\n"));
        let t = dispatcher_transcript(&p);
        let run = p.run(
            HOOK,
            &[],
            &hook_input(&t, false),
            &[
                ("FAKE_WAIT_STATUS", status),
                ("FAKE_WAIT_ERR", "beacon: bad \"handle\" \\ here\n  details"),
            ],
        );
        let answer = answer(&run);
        assert!(answer.get("decision").is_none(), "{answer}");
        let message = answer["systemMessage"].as_str().unwrap();
        assert!(
            message.starts_with("Dispatcher: the Stop hook's beacon check as pc-dispatch failed"),
            "{message}"
        );
        assert!(message.contains(&format!("exit {status}")), "{message}");
        assert!(message.contains("beacon: bad handle  here"), "{message}");
    }
}

#[test]
fn a_dispatcher_without_a_handle_is_told_why() {
    for env in [None, Some("BEACON_SECRET=s\n"), Some("BEACON_NODE=\n")] {
        let p = Project::new(env);
        let t = dispatcher_transcript(&p);
        let run = p.run(HOOK, &[], &hook_input(&t, false), &unread(&[]));
        let answer = answer(&run);
        assert!(answer.get("decision").is_none(), "{answer}");
        let message = answer["systemMessage"].as_str().unwrap();
        assert!(message.contains("BEACON_NODE"), "{message}");
        assert!(p.calls().is_empty(), "{env:?}");
    }
}

#[test]
fn odd_inputs_never_trap_the_session() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let cases = [
        String::new(),
        "not json".to_string(),
        r#"{"transcript_path":"/no/such/file"}"#.to_string(),
        r#"{"transcript_path":""}"#.to_string(),
    ];
    for input in &cases {
        let run = p.run(HOOK, &[], input, &unread(&[]));
        assert_eq!((run.code, run.stdout.as_str()), (0, ""), "input {input:?}");
    }
    assert!(p.calls().is_empty());
}

#[test]
fn the_dispatch_command_carries_the_marker_and_says_where_it_runs() {
    let command = fs::read_to_string(Path::new(REPO).join(".claude/commands/dispatch.md")).unwrap();
    assert!(command.contains(MARKER));
    assert!(
        command.contains(
            "description: Only for the dedicated beacon dispatcher session started with \
             `/loop 2m /dispatch`; never run it from another session."
        ),
        "{command}"
    );
}

#[test]
fn the_settings_run_the_hook_and_ask_before_deleting() {
    let settings: Value = serde_json::from_str(
        &fs::read_to_string(Path::new(REPO).join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    let hook = &settings["hooks"]["Stop"][0]["hooks"][0];
    assert_eq!(hook["type"], "command");
    assert!(hook["command"]
        .as_str()
        .unwrap()
        .ends_with("/scripts/dispatch-stop-hook.sh"));

    let rules = |kind: &str| -> Vec<String> {
        settings["permissions"][kind]
            .as_array()
            .unwrap_or_else(|| panic!("no {kind} rules"))
            .iter()
            .map(|r| r.as_str().unwrap().to_string())
            .collect()
    };
    let (allow, ask) = (rules("allow"), rules("ask"));
    for cli in [
        "./beacon",
        "~/Projects/beacon/beacon",
        "/home/soybean/Projects/beacon/beacon",
        "/Users/dang/Projects/beacon/beacon",
    ] {
        assert!(
            allow.contains(&format!("Bash({cli} *)")),
            "{cli}: {allow:?}"
        );
        for destructive in ["delete", "channel delete"] {
            let rule = format!("Bash({cli} {destructive} *)");
            assert!(ask.contains(&rule), "{rule}: {ask:?}");
        }
    }
    for check in ["scripts/dispatch-check.sh", "./scripts/dispatch-check.sh"] {
        assert!(
            allow.contains(&format!("Bash({check})")),
            "{check}: {allow:?}"
        );
        assert!(
            allow.contains(&format!("Bash({check} *)")),
            "{check}: {allow:?}"
        );
    }
}

/// The real CLI against a real node, then against no node at all.
#[tokio::test(flavor = "multi_thread")]
async fn against_a_real_node() {
    let node = common::spawn_node("pc", &[]).await;
    let p = Project::real(Some("BEACON_NODE=pc\n"), &node.url, &node.secret);
    tokio::task::spawn_blocking(move || {
        let t = dispatcher_transcript(&p);
        let input = hook_input(&t, false);
        p.beacon(&["channel", "new", "beacon"]);
        p.beacon(&[
            "join",
            "beacon",
            "--as",
            "pc-dispatch",
            "--client",
            "claude-code",
        ]);
        let run = p.run(HOOK, &[], &input, &[]);
        assert_eq!((run.code, run.stdout.as_str()), (0, ""), "{run:?}");

        p.beacon(&["post", "beacon", "please do x", "--as", "pc-configs"]);
        let run = p.run(HOOK, &[], &input, &[]);
        let reason = answer(&run)["reason"].as_str().unwrap().to_string();
        assert!(
            reason.starts_with("Beacon has at least 1 unread post(s) for pc-dispatch."),
            "{reason}"
        );

        let gone = Project::real(Some("BEACON_NODE=pc\n"), "http://127.0.0.1:1", "s");
        let t = dispatcher_transcript(&gone);
        let run = gone.run(HOOK, &[], &hook_input(&t, false), &[]);
        let message = answer(&run)["systemMessage"].as_str().unwrap().to_string();
        assert!(message.contains("beacon unreachable"), "{message}");
    })
    .await
    .unwrap();
}
