//! Tests for the `./beacon` wrapper script.
//!
//! The wrapper is run with `sh` against a fake `docker` (and a fake `readlink`
//! that only intercepts `/etc/localtime`) placed first on `PATH`. The fake
//! docker records each call's argv NUL-separated, plus its working directory,
//! environment and stdin, so no Docker daemon is needed.

use std::collections::HashMap;
use std::fs;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::TempDir;

const REPO: &str = env!("CARGO_MANIFEST_DIR");

/// Everything the wrapper puts before the arguments for the CLI inside the
/// container (`docker compose exec … <service> <binary>`), without `-T`.
const ENV_FLAGS: &[&str] = &[
    "-e",
    "BEACON_AGENT",
    "-e",
    "BEACON_CLIENT",
    "-e",
    "TERM",
    "-e",
    "COLORTERM",
    "-e",
    "TZ",
    "-e",
    "BEACON_TTY",
];

const FAKE_DOCKER: &str = r#"#!/bin/sh
dir=${FAKE_DOCKER_DIR:?}
case "$1 $2" in
"compose ps")
    printf '%s\0' "$@" > "$dir/ps.argv"
    if [ -n "${FAKE_PS_FAIL:-}" ]; then
        printf '%s\n' "$FAKE_PS_FAIL" >&2
        exit 1
    fi
    [ -n "${FAKE_NOT_RUNNING:-}" ] || echo 0123456789ab
    exit 0
    ;;
"compose exec")
    printf '%s\0' "$@" > "$dir/exec.argv"
    pwd -P > "$dir/exec.cwd"
    env > "$dir/exec.env"
    cat > "$dir/exec.stdin"
    if [ -n "${FAKE_EXEC_STDOUT_FILE:-}" ]; then
        cat "$FAKE_EXEC_STDOUT_FILE"
    fi
    exit "${FAKE_EXEC_STATUS:-0}"
    ;;
esac
echo "fake docker: unexpected call: $*" >&2
exit 99
"#;

const FAKE_READLINK: &str = r#"#!/bin/sh
if [ "$#" -eq 1 ] && [ "$1" = /etc/localtime ]; then
    [ -n "${FAKE_LOCALTIME_TARGET:-}" ] || exit 1
    printf '%s\n' "$FAKE_LOCALTIME_TARGET"
    exit 0
fi
exec "${REAL_READLINK:?}" "$@"
"#;

fn repo() -> PathBuf {
    fs::canonicalize(REPO).unwrap()
}

fn find_on_path(name: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
        .unwrap_or_else(|| panic!("{name} not found on PATH"))
}

fn write_script(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

struct Run {
    code: i32,
    stdout: Vec<u8>,
    stderr: String,
}

struct Sandbox {
    _tmp: TempDir,
    root: PathBuf,
    bin: PathBuf,
    state: PathBuf,
    home: PathBuf,
    /// A directory that is neither the repo nor the fake-docker state dir.
    work: PathBuf,
}

impl Sandbox {
    fn new() -> Sandbox {
        let tmp = TempDir::new().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let bin = root.join("fakebin");
        let state = root.join("state");
        let home = root.join("home");
        let work = root.join("work");
        for dir in [&bin, &state, &home, &work] {
            fs::create_dir(dir).unwrap();
        }
        write_script(&bin.join("docker"), FAKE_DOCKER);
        write_script(&bin.join("readlink"), FAKE_READLINK);
        Sandbox {
            _tmp: tmp,
            root,
            bin,
            state,
            home,
            work,
        }
    }

    /// `sh ./beacon <args>` from the repo root, with the fakes in place.
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = self.command_for(Path::new("./beacon"), Path::new(REPO));
        cmd.args(args);
        cmd
    }

    /// `sh <script>` run in `cwd`, with the fakes in place.
    fn command_for(&self, script: &Path, cwd: &Path) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg(script).current_dir(cwd);
        self.prepare(&mut cmd);
        cmd
    }

    fn prepare(&self, cmd: &mut Command) {
        let mut path = std::ffi::OsString::from(&self.bin);
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap());
        cmd.env("PATH", path)
            .env("HOME", &self.home)
            .env("FAKE_DOCKER_DIR", &self.state)
            .env("REAL_READLINK", find_on_path("readlink"))
            .env_remove("TZ")
            .env_remove("BEACON_AGENT")
            .env_remove("BEACON_CLIENT")
            .env_remove("BEACON_TTY")
            .stdin(Stdio::null());
    }

    fn run(&self, mut cmd: Command) -> Run {
        let out = cmd.output().unwrap();
        Run {
            code: out.status.code().expect("wrapper was killed by a signal"),
            stdout: out.stdout,
            stderr: String::from_utf8(out.stderr).unwrap(),
        }
    }

    fn argv(&self, call: &str) -> Vec<String> {
        let bytes = fs::read(self.state.join(format!("{call}.argv")))
            .unwrap_or_else(|e| panic!("fake docker saw no `{call}` call: {e}"));
        let mut parts: Vec<&[u8]> = bytes.split(|b| *b == 0).collect();
        assert_eq!(parts.pop(), Some(&b""[..]), "argv is NUL-terminated");
        parts
            .into_iter()
            .map(|p| String::from_utf8(p.to_vec()).unwrap())
            .collect()
    }

    fn called(&self, call: &str) -> bool {
        self.state.join(format!("{call}.argv")).exists()
    }

    fn exec_cwd(&self) -> PathBuf {
        PathBuf::from(
            fs::read_to_string(self.state.join("exec.cwd"))
                .unwrap()
                .trim_end(),
        )
    }

    fn exec_stdin(&self) -> Vec<u8> {
        fs::read(self.state.join("exec.stdin")).unwrap()
    }

    fn exec_env(&self) -> HashMap<String, String> {
        fs::read_to_string(self.state.join("exec.env"))
            .unwrap()
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
    }
}

/// The argv the wrapper must hand to docker for a non-interactive command.
fn exec_argv(args: &[&str]) -> Vec<String> {
    let mut v: Vec<&str> = vec!["compose", "exec", "-T"];
    v.extend(ENV_FLAGS);
    v.extend(["beacon", "beacon"]);
    v.extend(args);
    v.into_iter().map(String::from).collect()
}

fn s(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

// A pseudo-terminal whose slave end can be given to a child as its stdout.
fn open_pty() -> (OwnedFd, OwnedFd) {
    let (mut master, mut slave) = (0, 0);
    // SAFETY: openpty writes two valid fds on success; the null pointers mean
    // "default name, termios and window size".
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(rc, 0, "openpty failed: {}", std::io::Error::last_os_error());
    // SAFETY: both fds are fresh and owned by nobody else.
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

/// Run `cmd` with its stdout on a terminal; returns exit code and stderr.
fn run_on_tty(mut cmd: Command) -> (i32, String) {
    let (master, slave) = open_pty();
    cmd.stdout(Stdio::from(slave)).stderr(Stdio::piped());
    let child = cmd.spawn().unwrap();
    drop(cmd); // closes our copy of the slave end
    let out = child.wait_with_output().unwrap();
    drop(master);
    (
        out.status.code().expect("wrapper was killed by a signal"),
        String::from_utf8(out.stderr).unwrap(),
    )
}

#[test]
fn wrapper_passes_args_verbatim() {
    let sets: &[&[&str]] = &[
        &[
            "post",
            "beacon",
            "it's \"quoted\" $HOME `x` 🚀",
            "--tag",
            "done",
        ],
        &[
            "post",
            "beacon",
            "line one\nline two\n\ttabbed\n\nend",
            "--tag",
            "done",
        ],
        &[
            "post",
            "beacon",
            "--",
            "-rf is not a flag",
            "-n",
            "-e",
            "--tag=x",
        ],
        &[
            "post",
            "beacon",
            "",
            "  padded  ",
            "*",
            "?",
            "[a]",
            "~",
            "\\n",
            "%s",
            "a;b|c&d>e<f",
            "$(echo hi)",
            "$((1+1))",
        ],
        &["post", "beacon", "attach", "get", "skill", "install"],
    ];
    for args in sets {
        let sb = Sandbox::new();
        let run = sb.run(sb.command(args));
        assert_eq!(run.code, 0, "args {args:?}: {}", run.stderr);
        assert_eq!(run.stderr, "");
        assert_eq!(sb.argv("exec"), exec_argv(args), "args {args:?}");
    }
}

#[test]
fn wrapper_checks_the_service_is_running_before_exec() {
    let sb = Sandbox::new();
    let run = sb.run(sb.command(&["status"]));
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(
        sb.argv("ps"),
        s(&["compose", "ps", "--status", "running", "-q", "beacon"])
    );
    assert_eq!(sb.exec_cwd(), repo(), "compose runs from the repo root");
}

#[test]
fn exit_status_passes_through() {
    for code in [1, 2, 3, 4, 5, 6] {
        let sb = Sandbox::new();
        let mut cmd = sb.command(&["wait", "--timeout", "1"]);
        cmd.env("FAKE_EXEC_STATUS", code.to_string());
        assert_eq!(sb.run(cmd).code, code);
    }
}

#[test]
fn no_args_runs_tui_with_tty() {
    let sb = Sandbox::new();
    let (code, stderr) = run_on_tty(sb.command(&[]));
    assert_eq!((code, stderr.as_str()), (0, ""));
    // No -T: compose allocates a terminal for the TUI.
    let mut expected = s(&["compose", "exec"]);
    expected.extend(s(ENV_FLAGS));
    expected.extend(s(&["beacon", "beacon", "tui"]));
    assert_eq!(sb.argv("exec"), expected);
    assert_eq!(
        sb.exec_env().get("BEACON_TTY").map(String::as_str),
        Some("1")
    );

    // `./beacon tui` is the same thing spelled out.
    let sb = Sandbox::new();
    let (code, _) = run_on_tty(sb.command(&["tui"]));
    assert_eq!(code, 0);
    assert_eq!(sb.argv("exec"), expected);
}

#[test]
fn beacon_tty_is_only_set_when_stdout_is_a_terminal() {
    let sb = Sandbox::new();
    let mut cmd = sb.command(&["status"]);
    cmd.env("BEACON_TTY", "1"); // stale value from the caller's environment
    let run = sb.run(cmd);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(sb.exec_env().get("BEACON_TTY"), None);

    let (code, _) = run_on_tty(sb.command(&["status"]));
    assert_eq!(code, 0);
    assert_eq!(
        sb.exec_env().get("BEACON_TTY").map(String::as_str),
        Some("1")
    );
}

#[test]
fn caller_environment_reaches_docker() {
    let sb = Sandbox::new();
    let mut cmd = sb.command(&["who"]);
    cmd.env("BEACON_AGENT", "pc-beacon")
        .env("BEACON_CLIENT", "codex")
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor");
    assert_eq!(sb.run(cmd).code, 0);
    let env = sb.exec_env();
    assert_eq!(env["BEACON_AGENT"], "pc-beacon");
    assert_eq!(env["BEACON_CLIENT"], "codex");
    assert_eq!(env["TERM"], "xterm-256color");
    assert_eq!(env["COLORTERM"], "truecolor");
}

#[test]
fn attach_streams_host_file() {
    let sb = Sandbox::new();
    let file = sb.work.join("report (final).log");
    let bytes: &[u8] = b"\x00\x01binary\r\n\xff\xfe\n\n no trailing newline";
    fs::write(&file, bytes).unwrap();

    let run = sb.run(sb.command(&[
        "attach",
        "builds",
        file.to_str().unwrap(),
        "see the \"log\"",
        "--tag",
        "done",
    ]));
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.stderr, "");
    assert_eq!(sb.exec_stdin(), bytes);
    assert_eq!(
        sb.argv("exec"),
        exec_argv(&[
            "attach",
            "builds",
            "-",
            "--name",
            "report (final).log",
            "see the \"log\"",
            "--tag",
            "done"
        ])
    );
}

#[test]
fn attach_relative_path_from_other_dir() {
    let sb = Sandbox::new();
    fs::create_dir(sb.work.join("sub")).unwrap();
    fs::write(sb.work.join("notes.txt"), b"top level bytes").unwrap();
    fs::write(sb.work.join("sub").join("deep.bin"), b"\x00deep\xff").unwrap();
    let script = repo().join("beacon");

    let run = sb.run({
        let mut cmd = sb.command_for(&script, &sb.work);
        cmd.args(["attach", "general", "notes.txt"]);
        cmd
    });
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(sb.exec_stdin(), b"top level bytes");
    assert_eq!(
        sb.argv("exec"),
        exec_argv(&["attach", "general", "-", "--name", "notes.txt"])
    );
    assert_eq!(
        sb.exec_cwd(),
        repo(),
        "compose still runs from the repo root"
    );

    let run = sb.run({
        let mut cmd = sb.command_for(&script, &sb.work);
        cmd.args(["attach", "general", "./sub/deep.bin", "a note"]);
        cmd
    });
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(sb.exec_stdin(), b"\x00deep\xff");
    assert_eq!(
        sb.argv("exec"),
        exec_argv(&["attach", "general", "-", "--name", "deep.bin", "a note"])
    );
}

#[test]
fn attach_missing_file_exits_2_without_calling_docker() {
    let sb = Sandbox::new();
    let run = sb.run(sb.command(&["attach", "general", "does/not/exist.txt", "hi"]));
    assert_eq!(run.code, 2);
    assert_eq!(run.stderr, "no such file: does/not/exist.txt\n");
    assert!(!sb.called("ps") && !sb.called("exec"));
}

#[test]
fn attach_with_stdin_dash_is_left_to_the_cli() {
    let sb = Sandbox::new();
    let run = sb.run(sb.command(&["attach", "general", "-", "--name", "x.txt"]));
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(
        sb.argv("exec"),
        exec_argv(&["attach", "general", "-", "--name", "x.txt"])
    );
}

#[test]
fn get_dash_o_redirects() {
    let sb = Sandbox::new();
    let blob: &[u8] = b"\x00\x01\x02 attachment \xff\n\r\n";
    let source = sb.root.join("blob");
    fs::write(&source, blob).unwrap();
    let out = sb.work.join("saved.bin");

    let mut cmd = sb.command(&["get", "01ABCDEF", "-o", out.to_str().unwrap()]);
    cmd.env("FAKE_EXEC_STDOUT_FILE", &source);
    let run = sb.run(cmd);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.stderr, "");
    assert_eq!(run.stdout, b"", "bytes go to the file, not the terminal");
    assert_eq!(fs::read(&out).unwrap(), blob);
    assert_eq!(sb.argv("exec"), exec_argv(&["get", "01ABCDEF"]));

    // Spelled --output, with the path before the id or joined by `=`.
    for (args, name) in [
        (vec!["get", "--output", "long.bin", "01ABCDEF"], "long.bin"),
        (vec!["get", "01ABCDEF", "--output=joined.bin"], "joined.bin"),
    ] {
        let script = repo().join("beacon");
        let mut cmd = sb.command_for(&script, &sb.work);
        cmd.args(&args).env("FAKE_EXEC_STDOUT_FILE", &source);
        let run = sb.run(cmd);
        assert_eq!(run.code, 0, "{args:?}: {}", run.stderr);
        assert_eq!(fs::read(sb.work.join(name)).unwrap(), blob);
        assert_eq!(sb.argv("exec"), exec_argv(&["get", "01ABCDEF"]));
    }
}

#[test]
fn get_dash_o_relative_path() {
    let sb = Sandbox::new();
    let blob: &[u8] = b"relative output \x00\xff";
    let source = sb.root.join("blob");
    fs::write(&source, blob).unwrap();
    fs::create_dir(sb.work.join("out")).unwrap();
    let script = repo().join("beacon");

    for (arg, landed) in [
        ("saved.bin", "saved.bin"),
        ("./out/saved.bin", "out/saved.bin"),
    ] {
        let mut cmd = sb.command_for(&script, &sb.work);
        cmd.args(["get", "01ABCDEF", "-o", arg])
            .env("FAKE_EXEC_STDOUT_FILE", &source);
        let run = sb.run(cmd);
        assert_eq!(run.code, 0, "{arg}: {}", run.stderr);
        assert_eq!(fs::read(sb.work.join(landed)).unwrap(), blob, "{arg}");
        assert!(
            !repo().join("saved.bin").exists(),
            "the file must land in the caller's directory, not the repo"
        );
    }
    assert!(fs::read_dir(&sb.work).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".beacon")
    }));
}

#[test]
fn get_without_o_streams_to_stdout() {
    let sb = Sandbox::new();
    let source = sb.root.join("blob");
    fs::write(&source, b"to stdout").unwrap();
    for args in [vec!["get", "01ABCDEF"], vec!["get", "01ABCDEF", "-o", "-"]] {
        let mut cmd = sb.command(&args);
        cmd.env("FAKE_EXEC_STDOUT_FILE", &source);
        let run = sb.run(cmd);
        assert_eq!(run.code, 0, "{args:?}: {}", run.stderr);
        assert_eq!(run.stdout, b"to stdout");
        assert_eq!(sb.argv("exec"), exec_argv(&["get", "01ABCDEF"]));
    }
}

#[test]
fn get_failure_removes_partial_file_and_keeps_the_old_one() {
    let sb = Sandbox::new();
    let source = sb.root.join("partial");
    fs::write(&source, b"half of the bytes").unwrap();

    let fresh = sb.work.join("fresh.bin");
    let existing = sb.work.join("existing.bin");
    fs::write(&existing, b"old contents").unwrap();
    for out in [&fresh, &existing] {
        let mut cmd = sb.command(&["get", "01ABCDEF", "-o", out.to_str().unwrap()]);
        cmd.env("FAKE_EXEC_STDOUT_FILE", &source)
            .env("FAKE_EXEC_STATUS", "4");
        let run = sb.run(cmd);
        assert_eq!(run.code, 4, "the CLI's exit code is passed on");
        assert_eq!(sb.argv("exec"), exec_argv(&["get", "01ABCDEF"]));
    }
    assert!(!fresh.exists(), "partial output must not be left behind");
    assert_eq!(fs::read(&existing).unwrap(), b"old contents");
    let leftovers: Vec<_> = fs::read_dir(&sb.work)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(leftovers.len(), 1, "temp files cleaned up: {leftovers:?}");
}

#[test]
fn get_dash_o_to_missing_directory_fails_cleanly() {
    let sb = Sandbox::new();
    let run = sb.run(sb.command(&["get", "01ABCDEF", "-o", "/no/such/dir/x.bin"]));
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("no such directory"), "{}", run.stderr);
    assert!(!sb.called("exec"));
}

#[test]
fn not_running_exits_3() {
    let sb = Sandbox::new();
    let mut cmd = sb.command(&["post", "general", "hi"]);
    cmd.env("FAKE_NOT_RUNNING", "1");
    let run = sb.run(cmd);
    assert_eq!(run.code, 3);
    assert_eq!(
        run.stderr,
        "beacon isn't running here. Start it with: docker compose up -d   (in the beacon repo)\n"
    );
    assert_eq!(run.stdout, b"");
    assert!(
        !sb.called("exec"),
        "nothing is exec'd when the service is down"
    );

    // The TUI and the stream-carrying commands get the same answer.
    for args in [
        &[][..],
        &["attach", "general", "Cargo.toml"][..],
        &["get", "x", "-o", "/tmp/never"][..],
    ] {
        let sb = Sandbox::new();
        let mut cmd = sb.command(args);
        cmd.env("FAKE_NOT_RUNNING", "1");
        let run = sb.run(cmd);
        assert_eq!(run.code, 3, "args {args:?}");
        assert!(!sb.called("exec"));
    }
}

#[test]
fn docker_permission_denied_prints_the_group_hint() {
    let sb = Sandbox::new();
    let msg = "permission denied while trying to connect to the docker API at unix:///var/run/docker.sock";
    let mut cmd = sb.command(&["read", "general"]);
    cmd.env("FAKE_PS_FAIL", msg);
    let run = sb.run(cmd);
    assert_eq!(run.code, 3);
    assert_eq!(
        run.stderr,
        format!("{msg}\nadd yourself to the docker group or run: sg docker -c './beacon …'\n")
    );
    assert!(!sb.called("exec"));
}

#[test]
fn other_docker_failures_are_relayed_with_exit_3() {
    let sb = Sandbox::new();
    let msg = "failed to connect to the docker API at unix:///var/run/docker.sock; is the daemon running?";
    let mut cmd = sb.command(&["read", "general"]);
    cmd.env("FAKE_PS_FAIL", msg);
    let run = sb.run(cmd);
    assert_eq!(run.code, 3);
    assert_eq!(run.stderr, format!("{msg}\n"));
    assert!(!sb.called("exec"));
}

#[test]
fn tz_derived_from_localtime() {
    let cases: &[(&str, Option<&str>)] = &[
        // Debian, Ubuntu, Alpine, Fedora: absolute link into /usr/share/zoneinfo.
        (
            "/usr/share/zoneinfo/America/New_York",
            Some("America/New_York"),
        ),
        // systemd's timedatectl writes a relative link.
        ("../usr/share/zoneinfo/Europe/London", Some("Europe/London")),
        // macOS.
        ("/var/db/timezone/zoneinfo/Asia/Tokyo", Some("Asia/Tokyo")),
        (
            "/usr/share/zoneinfo/America/Argentina/Buenos_Aires",
            Some("America/Argentina/Buenos_Aires"),
        ),
        ("/usr/share/zoneinfo/Etc/GMT+5", Some("Etc/GMT+5")),
        ("/usr/share/zoneinfo/UTC", Some("UTC")),
        // Not a zoneinfo link, or nothing after it: leave TZ alone.
        ("/etc/some/other/file", None),
        ("/usr/share/zoneinfo/", None),
        ("/usr/share/zoneinfo/not a zone", None),
    ];
    for (target, expected) in cases {
        let sb = Sandbox::new();
        let mut cmd = sb.command(&["status"]);
        cmd.env("FAKE_LOCALTIME_TARGET", target);
        let run = sb.run(cmd);
        assert_eq!(run.code, 0, "{target}: {}", run.stderr);
        assert_eq!(
            sb.exec_env().get("TZ").map(String::as_str),
            *expected,
            "link target {target}"
        );
    }

    // No /etc/localtime link at all.
    let sb = Sandbox::new();
    assert_eq!(sb.run(sb.command(&["status"])).code, 0);
    assert_eq!(sb.exec_env().get("TZ"), None);

    // An explicit TZ always wins; an empty one does not count.
    let sb = Sandbox::new();
    let mut cmd = sb.command(&["status"]);
    cmd.env(
        "FAKE_LOCALTIME_TARGET",
        "/usr/share/zoneinfo/America/New_York",
    )
    .env("TZ", "Asia/Tokyo");
    assert_eq!(sb.run(cmd).code, 0);
    assert_eq!(sb.exec_env()["TZ"], "Asia/Tokyo");

    let sb = Sandbox::new();
    let mut cmd = sb.command(&["status"]);
    cmd.env(
        "FAKE_LOCALTIME_TARGET",
        "/usr/share/zoneinfo/America/New_York",
    )
    .env("TZ", "");
    assert_eq!(sb.run(cmd).code, 0);
    assert_eq!(sb.exec_env()["TZ"], "America/New_York");
}

#[test]
fn symlinked_wrapper_resolves_the_repo() {
    let sb = Sandbox::new();
    let real = repo().join("beacon");

    // An absolute link, as `ln -s ~/Projects/beacon/beacon ~/.local/bin/beacon` makes.
    let abs_dir = sb.root.join("a");
    fs::create_dir(&abs_dir).unwrap();
    let abs_link = abs_dir.join("beacon");
    symlink(&real, &abs_link).unwrap();

    // A relative link to that link, one directory over.
    let rel_dir = sb.root.join("b");
    fs::create_dir(&rel_dir).unwrap();
    let rel_link = rel_dir.join("beacon-rel");
    symlink("../a/beacon", &rel_link).unwrap();

    // And one more hop, relative to a bare file name in the same directory.
    let hop_link = rel_dir.join("beacon-hop");
    symlink("beacon-rel", &hop_link).unwrap();

    for link in [&abs_link, &rel_link, &hop_link] {
        let mut cmd = sb.command_for(link, &sb.work);
        cmd.arg("status");
        let run = sb.run(cmd);
        assert_eq!(run.code, 0, "{}: {}", link.display(), run.stderr);
        assert_eq!(sb.exec_cwd(), repo(), "via {}", link.display());
        assert_eq!(sb.argv("exec"), exec_argv(&["status"]));
        fs::remove_file(sb.state.join("exec.cwd")).unwrap();
    }
}

#[test]
fn runs_by_name_through_path_and_is_executable() {
    let mode = fs::metadata(repo().join("beacon"))
        .unwrap()
        .permissions()
        .mode();
    assert!(
        mode & 0o111 == 0o111,
        "./beacon must be executable, mode {mode:o}"
    );

    let sb = Sandbox::new();
    let link_dir = sb.root.join("links");
    fs::create_dir(&link_dir).unwrap();
    symlink(repo().join("beacon"), link_dir.join("beacon")).unwrap();

    let mut cmd = Command::new("beacon");
    sb.prepare(&mut cmd);
    let mut path = std::ffi::OsString::from(&link_dir);
    path.push(":");
    path.push(&sb.bin);
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap());
    cmd.env("PATH", path)
        .current_dir(&sb.work)
        .args(["channels", "--json"]);
    let run = sb.run(cmd);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(sb.exec_cwd(), repo());
    assert_eq!(sb.argv("exec"), exec_argv(&["channels", "--json"]));
}

fn installed(sb: &Sandbox, tool: &str) -> Option<Vec<u8>> {
    fs::read(sb.home.join(format!(".{tool}/skills/beacon/SKILL.md"))).ok()
}

#[test]
fn skill_install_copies_to_the_host() {
    let skill = fs::read(repo().join("skills/beacon/SKILL.md")).unwrap();
    let cases: &[(&[&str], bool, bool)] = &[
        (&["skill", "install", "claude"], true, false),
        (&["skill", "install", "codex"], false, true),
        (&["skill", "install", "all"], true, true),
        (&["skill", "install"], true, true),
    ];
    for (args, claude, codex) in cases {
        let sb = Sandbox::new();
        let mut cmd = sb.command_for(&repo().join("beacon"), &sb.work);
        cmd.args(*args);
        let run = sb.run(cmd);
        assert_eq!(run.code, 0, "{args:?}: {}", run.stderr);
        assert_eq!(installed(&sb, "claude").is_some(), *claude, "{args:?}");
        assert_eq!(installed(&sb, "codex").is_some(), *codex, "{args:?}");
        for tool in ["claude", "codex"] {
            if let Some(copy) = installed(&sb, tool) {
                assert_eq!(copy, skill, "{args:?} {tool}");
            }
        }
        let out = String::from_utf8(run.stdout).unwrap();
        assert!(
            out.contains("AGENTS.md"),
            "prints the AGENTS.md snippet: {out}"
        );
        assert!(
            out.contains(repo().join("skills/beacon/SKILL.md").to_str().unwrap()),
            "snippet points at the skill in this repo: {out}"
        );
        assert!(
            !sb.called("ps") && !sb.called("exec"),
            "host-only: no docker"
        );
    }

    // Re-running replaces a stale copy.
    let sb = Sandbox::new();
    let dest = sb.home.join(".claude/skills/beacon");
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("SKILL.md"), b"stale").unwrap();
    assert_eq!(sb.run(sb.command(&["skill", "install", "claude"])).code, 0);
    assert_eq!(installed(&sb, "claude").unwrap(), skill);
}

#[test]
fn skill_install_rejects_unknown_targets() {
    let sb = Sandbox::new();
    let run = sb.run(sb.command(&["skill", "install", "cursor"]));
    assert_eq!(run.code, 1);
    assert!(
        run.stderr
            .contains("usage: ./beacon skill install [claude|codex|all]"),
        "{}",
        run.stderr
    );
    assert!(installed(&sb, "claude").is_none() && installed(&sb, "codex").is_none());
}

#[test]
fn other_skill_subcommands_run_in_the_container() {
    let sb = Sandbox::new();
    let run = sb.run(sb.command(&["skill", "show"]));
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(sb.argv("exec"), exec_argv(&["skill", "show"]));
}
