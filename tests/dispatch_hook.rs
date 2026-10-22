//! Tests for `scripts/dispatch-stop-hook.sh`, the Claude Code Stop hook that keeps
//! a beacon dispatcher session from going idle with unread work.
//!
//! The hook runs with `CLAUDE_PROJECT_DIR` pointing at a temporary project whose
//! `beacon` is a fake: it records its argv NUL-separated and answers as
//! `FAKE_BEACON_STATUS` / `FAKE_BEACON_OUT` say, so no node is needed.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;
use tempfile::TempDir;

const REPO: &str = env!("CARGO_MANIFEST_DIR");
const MARKER: &str = "BEACON-DISPATCHER-SESSION";

const FAKE_BEACON: &str = r#"#!/bin/sh
dir=$(dirname "$0")
printf '%s\0' "$@" > "$dir/beacon.argv"
[ -n "${FAKE_BEACON_OUT:-}" ] && printf '%s\n' "$FAKE_BEACON_OUT"
exit "${FAKE_BEACON_STATUS:-6}"
"#;

const TWO_UNREAD: &str = r#"{"v":1,"posts":[{"v":1,"id":"01AAAAAAAAAAAAAAAAAAAAAAAA","short_id":"AAAAAAAA","body":"do x"},{"v":1,"id":"01BBBBBBBBBBBBBBBBBBBBBBBB","short_id":"BBBBBBBB","body":"do y"}],"remaining":0}"#;

struct Project {
    dir: TempDir,
}

impl Project {
    fn new(env_file: Option<&str>) -> Project {
        let dir = TempDir::new().unwrap();
        let fake = dir.path().join("beacon");
        fs::write(&fake, FAKE_BEACON).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        if let Some(contents) = env_file {
            fs::write(dir.path().join(".env"), contents).unwrap();
        }
        Project { dir }
    }

    fn transcript(&self, name: &str, text: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        fs::write(&path, text).unwrap();
        path
    }

    fn beacon_argv(&self) -> Option<Vec<String>> {
        let raw = fs::read(self.dir.path().join("beacon.argv")).ok()?;
        Some(
            raw.split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect(),
        )
    }

    fn run(&self, stdin: &str, env: &[(&str, &str)]) -> (i32, String) {
        let mut cmd = Command::new("sh");
        cmd.arg(Path::new(REPO).join("scripts/dispatch-stop-hook.sh"))
            .env("CLAUDE_PROJECT_DIR", self.dir.path())
            .env_remove("BEACON_DISPATCH_HANDLE")
            .env_remove("FAKE_BEACON_STATUS")
            .env_remove("FAKE_BEACON_OUT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }
}

fn hook_input(transcript: &Path, stop_hook_active: bool) -> String {
    serde_json::json!({
        "session_id": "s1",
        "transcript_path": transcript,
        "hook_event_name": "Stop",
        "stop_hook_active": stop_hook_active,
    })
    .to_string()
}

fn dispatcher_transcript(p: &Project) -> PathBuf {
    p.transcript(
        "dispatcher.jsonl",
        &format!(
            "{{\"type\":\"user\",\"message\":\"Dispatcher marker: {MARKER} (the project's Stop hook uses it)\"}}\n"
        ),
    )
}

#[test]
fn a_normal_session_is_left_alone() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let t = p.transcript(
        "normal.jsonl",
        "{\"type\":\"user\",\"message\":\"fix a bug\"}\n",
    );
    let (code, out) = p.run(
        &hook_input(&t, false),
        &[("FAKE_BEACON_STATUS", "0"), ("FAKE_BEACON_OUT", TWO_UNREAD)],
    );
    assert_eq!(code, 0);
    assert_eq!(out, "");
    assert!(p.beacon_argv().is_none(), "beacon must not even be asked");
}

#[test]
fn a_dispatcher_with_unread_work_is_kept_going() {
    let p = Project::new(Some("BEACON_SECRET=s\nBEACON_NODE=pc\n"));
    let t = dispatcher_transcript(&p);
    let (code, out) = p.run(
        &hook_input(&t, false),
        &[("FAKE_BEACON_STATUS", "0"), ("FAKE_BEACON_OUT", TWO_UNREAD)],
    );
    assert_eq!(code, 0);
    let answer: Value = serde_json::from_str(out.trim()).expect("one JSON object");
    assert_eq!(answer["decision"], "block");
    let reason = answer["reason"].as_str().unwrap();
    assert!(
        reason.contains("2 unread post(s) for pc-dispatch"),
        "{reason}"
    );
    assert!(
        reason.contains("./beacon read --unread --as pc-dispatch"),
        "{reason}"
    );
}

#[test]
fn it_peeks_as_the_devices_dispatcher_without_marking_read() {
    let p = Project::new(Some("BEACON_NODE=mac\n"));
    let t = dispatcher_transcript(&p);
    p.run(&hook_input(&t, false), &[]);
    let argv = p.beacon_argv().expect("beacon was asked");
    assert_eq!(
        argv,
        [
            "wait",
            "--timeout",
            "0",
            "--as",
            "mac-dispatch",
            "--client",
            "claude-code",
            "--json"
        ]
    );
}

#[test]
fn an_explicit_handle_wins() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let t = dispatcher_transcript(&p);
    p.run(
        &hook_input(&t, false),
        &[("BEACON_DISPATCH_HANDLE", "pc-ops")],
    );
    let argv = p.beacon_argv().unwrap();
    assert!(argv.windows(2).any(|w| w == ["--as", "pc-ops"]), "{argv:?}");
}

#[test]
fn nothing_unread_lets_it_stop() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let t = dispatcher_transcript(&p);
    let (code, out) = p.run(&hook_input(&t, false), &[("FAKE_BEACON_STATUS", "6")]);
    assert_eq!((code, out.as_str()), (0, ""));
}

#[test]
fn a_second_stop_in_a_row_always_goes_through() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let t = dispatcher_transcript(&p);
    for active in [r#""stop_hook_active":true"#, r#""stop_hook_active": true"#] {
        let input = format!(r#"{{"transcript_path":"{}",{active}}}"#, t.display());
        let (code, out) = p.run(
            &input,
            &[("FAKE_BEACON_STATUS", "0"), ("FAKE_BEACON_OUT", TWO_UNREAD)],
        );
        assert_eq!((code, out.as_str()), (0, ""), "{input}");
    }
    assert!(p.beacon_argv().is_none());
}

#[test]
fn a_down_server_is_reported_not_blocked() {
    let p = Project::new(Some("BEACON_NODE=pc\n"));
    let t = dispatcher_transcript(&p);
    let (code, out) = p.run(&hook_input(&t, false), &[("FAKE_BEACON_STATUS", "3")]);
    assert_eq!(code, 0);
    let answer: Value = serde_json::from_str(out.trim()).unwrap();
    assert!(answer.get("decision").is_none(), "{out}");
    assert!(answer["systemMessage"]
        .as_str()
        .unwrap()
        .contains("unreachable"));
}

#[test]
fn odd_inputs_never_trap_the_session() {
    let p = Project::new(None); // no .env, so no handle
    let t = dispatcher_transcript(&p);
    let cases = [
        String::new(),
        "not json".to_string(),
        r#"{"transcript_path":"/no/such/file"}"#.to_string(),
        hook_input(&t, false), // marker present, but no .env to name the handle
    ];
    for input in &cases {
        let (code, out) = p.run(
            input,
            &[("FAKE_BEACON_STATUS", "0"), ("FAKE_BEACON_OUT", TWO_UNREAD)],
        );
        assert_eq!((code, out.as_str()), (0, ""), "input {input:?}");
    }
}

#[test]
fn the_dispatch_command_carries_the_marker() {
    let command = fs::read_to_string(Path::new(REPO).join(".claude/commands/dispatch.md")).unwrap();
    assert!(command.contains(MARKER));
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
}
