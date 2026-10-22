//! A throwaway beacon repo for the dispatcher scripts' tests
//! (`scripts/dispatch-stop-hook.sh`, `scripts/dispatch-check.sh`).
//!
//! The scripts run with `CLAUDE_PROJECT_DIR` pointing at a temporary directory
//! that holds an optional `.env` and a fake `beacon`. The fake records every
//! call and answers as the `FAKE_*` variables say, so no node is needed:
//!
//! - `join` prints `FAKE_JOIN_OUT` on stdout and `FAKE_JOIN_ERR` on stderr, and
//!   exits `FAKE_JOIN_STATUS` (default 0);
//! - `wait` prints `FAKE_WAIT_OUT` on stdout and `FAKE_WAIT_ERR` on stderr, and
//!   exits `FAKE_WAIT_STATUS` (default 6: nothing new);
//! - `who` prints `FAKE_WHO_ERR` on stderr and exits `FAKE_WHO_STATUS`
//!   (default 0).
//!
//! [`Project::real`] puts the real CLI there instead, pointed at a test node.
//!
//! The dispatcher's backoff state lives in the project too: at [`STATE`]
//! (`BEACON_DISPATCH_STATE`), or under `xdg-state/` for a test that blanks that
//! variable, so no test touches the real `~/.local/state`.
//!
//! Each test binary uses a different subset, hence `allow(dead_code)`.
#![allow(dead_code)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::TempDir;

pub const REPO: &str = env!("CARGO_MANIFEST_DIR");

/// The marker `.claude/commands/dispatch.md` carries, built from parts so that
/// this source never contains it whole.
pub const MARKER: &str = concat!("BEACON-DISPATCHER", "-SESSION");

const FAKE_BEACON: &str = r#"#!/bin/sh
dir=$(dirname "$0")
{
    printf '%s\0' "$@"
    printf '\n'
} >>"$dir/beacon.calls"
case ${1-} in
join)
    [ -n "${FAKE_JOIN_OUT:-}" ] && printf '%s\n' "$FAKE_JOIN_OUT"
    [ -n "${FAKE_JOIN_ERR:-}" ] && printf '%s\n' "$FAKE_JOIN_ERR" >&2
    exit "${FAKE_JOIN_STATUS:-0}"
    ;;
wait)
    [ -n "${FAKE_WAIT_OUT:-}" ] && printf '%s\n' "$FAKE_WAIT_OUT"
    [ -n "${FAKE_WAIT_ERR:-}" ] && printf '%s\n' "$FAKE_WAIT_ERR" >&2
    exit "${FAKE_WAIT_STATUS:-6}"
    ;;
who)
    [ -n "${FAKE_WHO_ERR:-}" ] && printf '%s\n' "$FAKE_WHO_ERR" >&2
    exit "${FAKE_WHO_STATUS:-0}"
    ;;
esac
echo "fake beacon: unexpected command ${1-}" >&2
exit 1
"#;

/// Where [`Project::run`] points `BEACON_DISPATCH_STATE`, in the project.
pub const STATE: &str = "dispatch.state";

/// Two unread posts and nothing beyond them, as `wait --json` prints them.
pub const TWO_UNREAD: &str = r#"{"v":1,"posts":[{"v":1,"id":"01AAAAAAAAAAAAAAAAAAAAAAAA","short_id":"AAAAAAAA","body":"do x"},{"v":1,"id":"01BBBBBBBBBBBBBBBBBBBBBBBB","short_id":"BBBBBBBB","body":"do y"}],"remaining":0}"#;

/// What a script printed and how it ended.
#[derive(Debug)]
pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

pub struct Project {
    dir: TempDir,
}

impl Project {
    /// A project with the fake `beacon` and, if given, this `.env`.
    pub fn new(env_file: Option<&str>) -> Project {
        Project::with_beacon(env_file, FAKE_BEACON)
    }

    /// A project whose `beacon` is the real CLI, talking to the node at `url`.
    pub fn real(env_file: Option<&str>, url: &str, secret: &str) -> Project {
        let shim = format!(
            "#!/bin/sh\nunset BEACON_AGENT BEACON_CLIENT BEACON_HUMAN BEACON_NODE RUST_LOG\n\
             BEACON_SERVER='{url}' BEACON_SECRET='{secret}' exec '{}' \"$@\"\n",
            env!("CARGO_BIN_EXE_beacon")
        );
        Project::with_beacon(env_file, &shim)
    }

    fn with_beacon(env_file: Option<&str>, script: &str) -> Project {
        let dir = TempDir::new().unwrap();
        let fake = dir.path().join("beacon");
        fs::write(&fake, script).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        if let Some(contents) = env_file {
            fs::write(dir.path().join(".env"), contents).unwrap();
        }
        Project { dir }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Writes `text` to `name` in the project and returns its path.
    pub fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        fs::write(&path, text).unwrap();
        path
    }

    /// Runs this project's `beacon` (the real one, see [`Project::real`]) and
    /// returns its stdout; it must succeed.
    pub fn beacon(&self, args: &[&str]) -> String {
        let out = Command::new(self.dir.path().join("beacon"))
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "beacon {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// The fake `beacon`'s calls in order, each as its argv.
    pub fn calls(&self) -> Vec<Vec<String>> {
        let Ok(raw) = fs::read(self.dir.path().join("beacon.calls")) else {
            return Vec::new();
        };
        raw.split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                line.split(|b| *b == 0)
                    .filter(|s| !s.is_empty())
                    .map(|s| String::from_utf8_lossy(s).into_owned())
                    .collect()
            })
            .collect()
    }

    /// Runs `scripts/<script>` with `sh`, these arguments and stdin, for this
    /// project, with the `FAKE_*` and handle variables cleared and the backoff
    /// state kept in the project (see [`STATE`]) before `env`.
    pub fn run(&self, script: &str, args: &[&str], stdin: &str, env: &[(&str, &str)]) -> Run {
        let mut cmd = Command::new("sh");
        cmd.arg(Path::new(REPO).join("scripts").join(script))
            .args(args);
        self.finish(cmd, stdin, env)
    }

    /// [`Project::run`] with stderr merged into stdout, the way a terminal or a
    /// tool call shows them: `stdout` holds both, in the order they came.
    pub fn run_merged(&self, script: &str, args: &[&str], env: &[(&str, &str)]) -> Run {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exec sh \"$@\" 2>&1", "sh"])
            .arg(Path::new(REPO).join("scripts").join(script))
            .args(args);
        self.finish(cmd, "", env)
    }

    fn finish(&self, mut cmd: Command, stdin: &str, env: &[(&str, &str)]) -> Run {
        cmd.env("CLAUDE_PROJECT_DIR", self.dir.path())
            .env_remove("BEACON_DISPATCH_HANDLE")
            .env("BEACON_DISPATCH_STATE", self.dir.path().join(STATE))
            .env("XDG_STATE_HOME", self.dir.path().join("xdg-state"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for var in [
            "FAKE_JOIN_OUT",
            "FAKE_JOIN_ERR",
            "FAKE_JOIN_STATUS",
            "FAKE_WAIT_OUT",
            "FAKE_WAIT_ERR",
            "FAKE_WAIT_STATUS",
            "FAKE_WHO_ERR",
            "FAKE_WHO_STATUS",
        ] {
            cmd.env_remove(var);
        }
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
        Run {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }
}

/// `.env` files that all name the node `pc`, in the shapes people write them.
pub const ENV_SPELLINGS_OF_PC: &[&str] = &[
    "BEACON_NODE=pc\n",
    "BEACON_NODE=\"pc\"\n",
    "BEACON_NODE='pc'\n",
    "BEACON_SECRET=s\r\nBEACON_NODE=pc\r\nBEACON_PORT=7700\r\n",
    "BEACON_NODE=pc # this device\n",
    "BEACON_NODE=pc\t# this device\n",
    "BEACON_NODE=\"pc\" # this device\n",
    "BEACON_NODE=pc   \n",
    "BEACON_NODE=\"pc\"\r\n",
    "# BEACON_NODE=mac\nBEACON_NODE=pc\n",
    // Docker Compose takes the last assignment.
    "BEACON_NODE=mac\nBEACON_NODE=pc\n",
];
