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
    case " $* " in
    *" BEACON_EXIT_ON_STDIN_EOF=1 "*)
        # The wrapper promises that stdin stays open while the client lives, so
        # there is no end to read to. With FAKE_EXEC_PROBE_STDIN, look whether
        # it is still open after a second.
        if [ -n "${FAKE_EXEC_PROBE_STDIN:-}" ]; then
            exec 4<&0
            cat <&4 > "$dir/exec.stdin" &
            pid=$!
            sleep 1
            if kill -0 "$pid" 2>/dev/null; then
                echo open > "$dir/exec.stdin.state"
                kill "$pid"
            else
                echo ended > "$dir/exec.stdin.state"
            fi
        fi
        ;;
    *)
        [ -t 0 ] || cat > "$dir/exec.stdin"
        ;;
    esac
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

/// A `mkfifo` that can be told to fail (for every path, or for those under a
/// directory) or to hang once the FIFO is made, so a signal can arrive while
/// the wrapper is still setting up the stdin for `watch` and `wait`.
const FAKE_MKFIFO: &str = r#"#!/bin/sh
if [ -n "${FAKE_MKFIFO_FAIL_ALL:-}" ]; then
    exit 1
fi
if [ -n "${FAKE_MKFIFO_FAIL_UNDER:-}" ]; then
    case $1 in
    "$FAKE_MKFIFO_FAIL_UNDER"/*) exit 1 ;;
    esac
fi
"${REAL_MKFIFO:?}" "$@" || exit $?
if [ -n "${FAKE_MKFIFO_HANG:-}" ]; then
    echo $$ > "${FAKE_DOCKER_DIR:?}/mkfifo.pid"
    sleep 30
fi
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

/// The argv the wrapper must hand to docker for `watch` and `wait`: the same,
/// plus the request that the program ends when its stdin does.
fn linked_exec_argv(args: &[&str]) -> Vec<String> {
    let mut v: Vec<&str> = vec!["compose", "exec", "-T"];
    v.extend(ENV_FLAGS);
    v.extend(["-e", "BEACON_EXIT_ON_STDIN_EOF=1", "beacon", "beacon"]);
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

/// Run `cmd` with its stdin and stdout on a terminal; returns exit code and stderr.
fn run_on_tty(mut cmd: Command) -> (i32, String) {
    let (master, slave) = open_pty();
    cmd.stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave))
        .stderr(Stdio::piped());
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
    // A plain command (exec'd straight away) and a linked one (wait, whose
    // stdin is made by the wrapper) both hand the CLI's status on untouched.
    let commands: &[&[&str]] = &[&["read", "general"], &["wait", "--timeout", "1"]];
    for args in commands {
        for code in [1, 2, 3, 4, 5, 6] {
            let sb = Sandbox::new();
            let mut cmd = sb.command(args);
            cmd.env("FAKE_EXEC_STATUS", code.to_string());
            assert_eq!(sb.run(cmd).code, code, "{args:?}");
        }
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

    // `./beacon tui` is the same thing spelled out.
    let sb = Sandbox::new();
    let (code, _) = run_on_tty(sb.command(&["tui"]));
    assert_eq!(code, 0);
    assert_eq!(sb.argv("exec"), expected);
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
            "--size",
            &bytes.len().to_string(),
            "see the \"log\"",
            "--tag",
            "done"
        ])
    );
}

#[test]
fn attach_tells_the_cli_the_files_size() {
    // R29: a stdin that ends early (the docker client was killed) must not
    // become a truncated post, so the CLI is told how long the file is.
    let sb = Sandbox::new();
    let cases: &[(&str, Vec<u8>)] = &[
        ("empty.txt", Vec::new()),
        ("one.txt", b"x".to_vec()),
        ("newline.txt", b"line\n".to_vec()),
        ("blank lines.txt", b"\n\n\n".to_vec()),
        ("binary.bin", (0..=255u8).cycle().take(100_003).collect()),
    ];
    for (name, bytes) in cases {
        let file = sb.work.join(name);
        fs::write(&file, bytes).unwrap();
        let run = sb.run(sb.command(&["attach", "general", file.to_str().unwrap()]));
        assert_eq!(run.code, 0, "{name}: {}", run.stderr);
        assert_eq!(sb.exec_stdin(), *bytes, "{name}");
        assert_eq!(
            sb.argv("exec"),
            exec_argv(&[
                "attach",
                "general",
                "-",
                "--name",
                name,
                "--size",
                &bytes.len().to_string()
            ]),
            "{name}"
        );
    }
}

/// BSD's `wc -c` pads its answer with spaces; the size passed on has none.
#[test]
fn attach_size_survives_a_wc_that_pads_its_answer() {
    let sb = Sandbox::new();
    write_script(
        &sb.bin.join("wc"),
        "#!/bin/sh\nout=$(\"${REAL_WC:?}\" \"$@\") || exit $?\nprintf '%12s\\n' \"$out\"\n",
    );
    let file = sb.work.join("padded.txt");
    fs::write(&file, b"twelve bytes").unwrap();
    let mut cmd = sb.command(&["attach", "general", file.to_str().unwrap()]);
    cmd.env("REAL_WC", find_on_path("wc"));
    let run = sb.run(cmd);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(
        sb.argv("exec"),
        exec_argv(&[
            "attach",
            "general",
            "-",
            "--name",
            "padded.txt",
            "--size",
            "12"
        ])
    );
}

#[test]
fn attach_without_a_size_never_reaches_docker() {
    let sb = Sandbox::new();
    write_script(&sb.bin.join("wc"), "#!/bin/sh\necho 'no idea'\n");
    let file = sb.work.join("f.txt");
    fs::write(&file, b"bytes").unwrap();
    let run = sb.run(sb.command(&["attach", "general", file.to_str().unwrap()]));
    assert_eq!(run.code, 1);
    assert_eq!(
        run.stderr,
        format!("can't tell how big {} is\n", file.to_str().unwrap())
    );
    assert!(!sb.called("exec"), "a file of unknown size was streamed");
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
        exec_argv(&[
            "attach",
            "general",
            "-",
            "--name",
            "notes.txt",
            "--size",
            "15"
        ])
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
        exec_argv(&["attach", "general", "-", "--name", "deep.bin", "--size", "6", "a note"])
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

    // The stream-carrying commands get the same answer.
    for args in [
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

    // So does the TUI, started from a terminal.
    let sb = Sandbox::new();
    let mut cmd = sb.command(&[]);
    cmd.env("FAKE_NOT_RUNNING", "1");
    let (code, stderr) = run_on_tty(cmd);
    assert_eq!(code, 3);
    assert!(stderr.contains("docker compose up -d"), "{stderr}");
    assert!(!sb.called("exec"));
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

#[test]
fn global_flags_before_attach_and_get() {
    let sb = Sandbox::new();
    let script = repo().join("beacon");
    fs::write(sb.work.join("f.txt"), b"file bytes").unwrap();
    let blob: &[u8] = b"\x00saved bytes\xff";
    let source = sb.root.join("blob");
    fs::write(&source, blob).unwrap();

    // get: the file is still written on the host, and the CLI still gets the
    // global flags (right after the subcommand).
    let get_cases: &[(&[&str], &str, &[&str])] = &[
        (
            &["--json", "get", "01ABC", "-o", "out1.bin"],
            "out1.bin",
            &["get", "--json", "01ABC"],
        ),
        (
            &[
                "--as",
                "me",
                "--client",
                "codex",
                "get",
                "01ABC",
                "--output=out2.bin",
            ],
            "out2.bin",
            &["get", "--as", "me", "--client", "codex", "01ABC"],
        ),
        (
            &["--as=me", "--strict", "get", "-o", "out3.bin", "01ABC"],
            "out3.bin",
            &["get", "--as=me", "--strict", "01ABC"],
        ),
    ];
    for (args, out, expected) in get_cases {
        let mut cmd = sb.command_for(&script, &sb.work);
        cmd.args(*args).env("FAKE_EXEC_STDOUT_FILE", &source);
        let run = sb.run(cmd);
        assert_eq!(run.code, 0, "{args:?}: {}", run.stderr);
        assert_eq!(run.stdout, b"", "{args:?}: bytes go to the file");
        assert_eq!(fs::read(sb.work.join(out)).unwrap(), blob, "{args:?}");
        assert!(!repo().join(out).exists(), "{args:?}: written in the repo");
        assert_eq!(sb.argv("exec"), exec_argv(expected), "{args:?}");
    }

    // attach: the file is streamed from the host, whatever comes first.
    let attach_cases: &[(&[&str], &[&str])] = &[
        (
            &["--as", "me", "attach", "general", "f.txt", "hi"],
            &[
                "attach", "--as", "me", "general", "-", "--name", "f.txt", "--size", "10", "hi",
            ],
        ),
        (
            &["--json", "--strict", "attach", "general", "./f.txt"],
            &[
                "attach", "--json", "--strict", "general", "-", "--name", "f.txt", "--size", "10",
            ],
        ),
        (
            &[
                "--client=codex",
                "attach",
                "general",
                "f.txt",
                "--tag",
                "done",
            ],
            &[
                "attach",
                "--client=codex",
                "general",
                "-",
                "--name",
                "f.txt",
                "--size",
                "10",
                "--tag",
                "done",
            ],
        ),
    ];
    for (args, expected) in attach_cases {
        let mut cmd = sb.command_for(&script, &sb.work);
        cmd.args(*args);
        let run = sb.run(cmd);
        assert_eq!(run.code, 0, "{args:?}: {}", run.stderr);
        assert_eq!(sb.exec_stdin(), b"file bytes", "{args:?}");
        assert_eq!(sb.argv("exec"), exec_argv(expected), "{args:?}");
    }

    // Only global flags, or global flags before a command the wrapper does not
    // handle itself: handed over exactly as typed.
    for args in [&["--json"][..], &["--json", "read", "general"][..]] {
        let run = sb.run(sb.command(args));
        assert_eq!(run.code, 0, "{args:?}: {}", run.stderr);
        assert_eq!(sb.argv("exec"), exec_argv(args), "{args:?}");
    }
}

#[test]
fn attach_with_tag_before_and_between_positionals() {
    let sb = Sandbox::new();
    let script = repo().join("beacon");
    fs::write(sb.work.join("f.txt"), b"file bytes").unwrap();

    // (arguments after `attach`, argv the CLI must receive, file streamed?)
    let cases: &[(&[&str], &[&str], bool)] = &[
        (
            &["--tag", "done", "general", "f.txt", "hi"],
            &[
                "attach", "--tag", "done", "general", "-", "--name", "f.txt", "--size", "10", "hi",
            ],
            true,
        ),
        (
            &["general", "--tag", "done", "f.txt"],
            &[
                "attach", "general", "--tag", "done", "-", "--name", "f.txt", "--size", "10",
            ],
            true,
        ),
        (
            &["--tag=done", "general", "f.txt", "msg", "--tag", "fyi"],
            &[
                "attach",
                "--tag=done",
                "general",
                "-",
                "--name",
                "f.txt",
                "--size",
                "10",
                "msg",
                "--tag",
                "fyi",
            ],
            true,
        ),
        // A name the user chose wins; the wrapper adds none of its own.
        (
            &["general", "f.txt", "--name", "custom.txt"],
            &[
                "attach",
                "general",
                "-",
                "--size",
                "10",
                "--name",
                "custom.txt",
            ],
            true,
        ),
        (
            &["--name=custom.txt", "general", "f.txt"],
            &[
                "attach",
                "--name=custom.txt",
                "general",
                "-",
                "--size",
                "10",
            ],
            true,
        ),
        // A message that looks like a flag, after `--`, is left alone.
        (
            &["general", "f.txt", "--", "-dash message"],
            &[
                "attach",
                "general",
                "-",
                "--name",
                "f.txt",
                "--size",
                "10",
                "--",
                "-dash message",
            ],
            true,
        ),
        // Help is the CLI's business, wherever it is asked for.
        (&["--help"], &["attach", "--help"], false),
        (
            &["general", "--help"],
            &["attach", "general", "--help"],
            false,
        ),
        (
            &["general", "f.txt", "--help"],
            &["attach", "general", "f.txt", "--help"],
            false,
        ),
    ];
    for (args, expected, streamed) in cases {
        let mut cmd = sb.command_for(&script, &sb.work);
        cmd.arg("attach").args(*args);
        let run = sb.run(cmd);
        assert_eq!(run.code, 0, "{args:?}: {}", run.stderr);
        assert_eq!(sb.argv("exec"), exec_argv(expected), "{args:?}");
        if *streamed {
            assert_eq!(sb.exec_stdin(), b"file bytes", "{args:?}");
        }
    }

    // The path is the second non-option argument even when options come first.
    let run = sb.run(sb.command(&["attach", "--tag", "done", "general", "no-such.txt"]));
    assert_eq!(run.code, 2);
    assert_eq!(run.stderr, "no such file: no-such.txt\n");
}

#[test]
fn get_output_flag_spellings() {
    let sb = Sandbox::new();
    let script = repo().join("beacon");
    let blob: &[u8] = b"spelling \x00 bytes";
    let source = sb.root.join("blob");
    fs::write(&source, blob).unwrap();
    let cases: &[(&[&str], &str)] = &[
        (&["get", "01ABC", "-ojoined.bin"], "joined.bin"),
        (&["get", "01ABC", "-o=equals.bin"], "equals.bin"),
        (&["get", "01ABC", "--output=long.bin"], "long.bin"),
        (&["get", "01ABC", "--output", "spaced.bin"], "spaced.bin"),
        (&["get", "-o", "first.bin", "01ABC"], "first.bin"),
    ];
    for (args, out) in cases {
        let mut cmd = sb.command_for(&script, &sb.work);
        cmd.args(*args).env("FAKE_EXEC_STDOUT_FILE", &source);
        let run = sb.run(cmd);
        assert_eq!(run.code, 0, "{args:?}: {}", run.stderr);
        assert_eq!(run.stdout, b"");
        assert_eq!(fs::read(sb.work.join(out)).unwrap(), blob, "{args:?}");
        assert_eq!(sb.argv("exec"), exec_argv(&["get", "01ABC"]), "{args:?}");
    }
}

#[test]
fn get_empty_output_path_is_a_usage_error() {
    for args in [
        &["get", "01ABC", "-o", ""][..],
        &["get", "01ABC", "--output", ""][..],
        &["get", "01ABC", "--output="][..],
        &["get", "01ABC", "-o="][..],
        &["get", "01ABC", "-o"][..],
    ] {
        let sb = Sandbox::new();
        let source = sb.root.join("blob");
        fs::write(&source, b"must never reach the terminal").unwrap();
        let mut cmd = sb.command(args);
        cmd.env("FAKE_EXEC_STDOUT_FILE", &source);
        let run = sb.run(cmd);
        assert_eq!(run.code, 1, "{args:?}");
        assert_eq!(run.stdout, b"", "{args:?}");
        assert_eq!(
            run.stderr, "usage: ./beacon get <post-id> [-o path]\n",
            "{args:?}"
        );
        assert!(!sb.called("exec"), "{args:?}");
    }
}

#[test]
fn get_dash_o_unwritable_directory_exits_1() {
    // Root can write anywhere, so there is nothing to check for it.
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let sb = Sandbox::new();
    let dir = sb.work.join("readonly");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
    let out = dir.join("x.bin");
    let run = sb.run(sb.command(&["get", "01ABC", "-o", out.to_str().unwrap()]));
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(run.code, 1);
    assert_eq!(run.stderr, format!("can't write to {}\n", dir.display()));
    assert!(!sb.called("exec"));
}

#[test]
fn get_dash_o_writes_straight_to_special_files() {
    let sb = Sandbox::new();
    let blob: &[u8] = b"through a fifo \x00\xff";
    let source = sb.root.join("blob");
    fs::write(&source, blob).unwrap();

    // /dev/null: no rename is possible (or wanted).
    let mut cmd = sb.command(&["get", "01ABC", "-o", "/dev/null"]);
    cmd.env("FAKE_EXEC_STDOUT_FILE", &source);
    let run = sb.run(cmd);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.stderr, "");
    assert_eq!(sb.argv("exec"), exec_argv(&["get", "01ABC"]));

    // A FIFO gets the bytes as they are written.
    let fifo = sb.work.join("pipe");
    let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: c_path is a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    let reader = {
        let fifo = fifo.clone();
        std::thread::spawn(move || fs::read(fifo).unwrap())
    };
    let mut cmd = sb.command(&["get", "01ABC", "-o", fifo.to_str().unwrap()]);
    cmd.env("FAKE_EXEC_STDOUT_FILE", &source);
    let run = sb.run(cmd);
    // If the wrapper never opened the FIFO the reader would wait forever: open
    // a writer ourselves (non-blocking, so it fails once the reader is gone).
    {
        use std::os::unix::fs::OpenOptionsExt;
        let _unblock = fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&fifo);
    }
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(reader.join().unwrap(), blob);
    assert!(fs::read_dir(&sb.work).unwrap().all(|e| !e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".beacon")));
}

#[test]
fn tui_without_a_terminal_explains_itself() {
    for args in [&[][..], &["tui"][..]] {
        let sb = Sandbox::new();
        let run = sb.run(sb.command(args));
        assert_eq!(run.code, 1, "args {args:?}");
        assert_eq!(
            run.stderr,
            "the TUI needs a terminal; agents use ./beacon <command> (see ./beacon skill show)\n"
        );
        assert_eq!(run.stdout, b"");
        assert!(!sb.called("ps") && !sb.called("exec"), "args {args:?}");
    }
}

// `docker compose exec` leaves the program it started running when its client
// dies (Ctrl-C, a killed agent, a closed terminal); all that program sees is its
// stdin closing. So `watch` and `wait`, which read no stdin, are given one that
// stays open for as long as the client lives, and are asked to end when it
// closes. (The sandbox gives the wrapper /dev/null as stdin: no lifeline of its
// own to pass on.)

#[test]
fn watch_and_wait_get_a_stdin_that_stays_open() {
    let cases: &[&[&str]] = &[
        &["watch", "--as", "pc-vllm"],
        &["wait", "--timeout", "5"],
        &["--as", "pc-vllm", "watch"],
        &["--json", "--client", "codex", "wait"],
        &["--as=pc-vllm", "--strict", "watch", "--json"],
    ];
    std::thread::scope(|scope| {
        for args in cases {
            scope.spawn(move || {
                let sb = Sandbox::new();
                let mut cmd = sb.command(args);
                cmd.env("FAKE_EXEC_PROBE_STDIN", "1");
                let run = sb.run(cmd);
                assert_eq!(run.code, 0, "{args:?}: {}", run.stderr);
                assert_eq!(sb.argv("exec"), linked_exec_argv(args), "{args:?}");
                let state = fs::read_to_string(sb.state.join("exec.stdin.state")).unwrap();
                assert_eq!(
                    state.trim(),
                    "open",
                    "{args:?}: stdin must not end while the client lives"
                );
            });
        }
    });
}

#[test]
fn other_commands_keep_the_stdin_they_were_given() {
    // `wait` and `watch` as words in a message or a channel name are not commands.
    let cases: &[&[&str]] = &[
        &["read", "general"],
        &["post", "general", "wait"],
        &["post", "general", "--as", "pc-vllm", "watch"],
        &["read", "watch"],
        &["--as", "wait", "who"],
        &["who"],
    ];
    for args in cases {
        let sb = Sandbox::new();
        let run = sb.run(sb.command(args));
        assert_eq!(run.code, 0, "{args:?}: {}", run.stderr);
        assert_eq!(sb.argv("exec"), exec_argv(args), "{args:?}");
    }
}

#[test]
fn the_stdin_for_watch_leaves_no_files_behind() {
    let sb = Sandbox::new();
    let tmp = sb.root.join("tmp");
    fs::create_dir(&tmp).unwrap();
    let mut cmd = sb.command(&["watch", "--as", "pc-vllm"]);
    cmd.env("TMPDIR", &tmp);
    let run = sb.run(cmd);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(
        sb.argv("exec"),
        linked_exec_argv(&["watch", "--as", "pc-vllm"])
    );
    let left: Vec<_> = fs::read_dir(&tmp).unwrap().collect();
    assert!(left.is_empty(), "left behind: {left:?}");
}

/// The probe of the fake docker: whether the stdin it was given is still open.
fn stdin_state(sb: &Sandbox) -> String {
    fs::read_to_string(sb.state.join("exec.stdin.state"))
        .unwrap()
        .trim()
        .to_string()
}

fn sandbox_with_fake_mkfifo() -> (Sandbox, PathBuf) {
    let sb = Sandbox::new();
    write_script(&sb.bin.join("mkfifo"), FAKE_MKFIFO);
    let tmp = sb.root.join("tmp");
    fs::create_dir(&tmp).unwrap();
    (sb, tmp)
}

#[test]
fn the_stdin_for_watch_falls_back_to_tmp_when_tmpdir_wont_do() {
    // A TMPDIR that is missing, or one where no FIFO can be made (some mounts
    // refuse them), is not a reason to leave `watch` unlinked: /tmp is tried.
    for fail_under_tmpdir in [false, true] {
        let (sb, tmp) = sandbox_with_fake_mkfifo();
        let mut cmd = sb.command(&["watch", "--as", "pc-vllm"]);
        cmd.env("REAL_MKFIFO", find_on_path("mkfifo"))
            .env("FAKE_EXEC_PROBE_STDIN", "1");
        if fail_under_tmpdir {
            cmd.env("TMPDIR", &tmp).env("FAKE_MKFIFO_FAIL_UNDER", &tmp);
        } else {
            cmd.env("TMPDIR", sb.root.join("no-such-dir"));
        }
        let run = sb.run(cmd);
        assert_eq!(run.code, 0, "{}", run.stderr);
        assert_eq!(run.stderr, "", "nothing to say: it worked");
        assert_eq!(
            sb.argv("exec"),
            linked_exec_argv(&["watch", "--as", "pc-vllm"]),
            "fail_under_tmpdir: {fail_under_tmpdir}"
        );
        assert_eq!(stdin_state(&sb), "open");
        let left: Vec<_> = fs::read_dir(&tmp).unwrap().collect();
        assert!(left.is_empty(), "left behind: {left:?}");
    }
}

#[test]
fn watch_still_runs_where_the_stdin_cannot_be_made() {
    // No FIFO anywhere: run it as before, and do not ask it to end on stdin
    // closing (it would end at once on the caller's /dev/null). One line says
    // what that costs.
    let (sb, tmp) = sandbox_with_fake_mkfifo();
    for args in [
        &["watch", "--as", "pc-vllm"][..],
        &["wait", "--timeout", "5"][..],
    ] {
        let mut cmd = sb.command(args);
        cmd.env("TMPDIR", &tmp)
            .env("REAL_MKFIFO", find_on_path("mkfifo"))
            .env("FAKE_MKFIFO_FAIL_ALL", "1");
        let run = sb.run(cmd);
        assert_eq!(run.code, 0, "{args:?}: {}", run.stderr);
        assert_eq!(sb.argv("exec"), exec_argv(args), "{args:?}");
        assert_eq!(
            run.stderr,
            "beacon: couldn't link wait/watch to this terminal; it may keep running after you stop it\n",
            "{args:?}"
        );
        let left: Vec<_> = fs::read_dir(&tmp).unwrap().collect();
        assert!(left.is_empty(), "left behind: {left:?}");
    }
    // No temp directory at all, and a /tmp that takes nothing: the same.
    let mut cmd = sb.command(&["watch", "--as", "pc-vllm"]);
    cmd.env("TMPDIR", sb.root.join("no-such-dir"))
        .env("REAL_MKFIFO", find_on_path("mkfifo"))
        .env("FAKE_MKFIFO_FAIL_ALL", "1");
    let run = sb.run(cmd);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(sb.argv("exec"), exec_argv(&["watch", "--as", "pc-vllm"]));
    assert_eq!(run.stderr.lines().count(), 1, "{}", run.stderr);
}

#[test]
fn a_signal_while_the_stdin_is_being_made_removes_its_directory() {
    use std::os::unix::process::CommandExt;
    // `exec` runs no EXIT trap, so the wrapper removes the directory itself
    // right before it. Until then, a wrapper that is stopped must not leave it.
    for (name, signal) in [("TERM", libc::SIGTERM), ("HUP", libc::SIGHUP)] {
        let (sb, tmp) = sandbox_with_fake_mkfifo();
        let mut cmd = sb.command(&["watch", "--as", "pc-vllm"]);
        cmd.env("TMPDIR", &tmp)
            .env("REAL_MKFIFO", find_on_path("mkfifo"))
            .env("FAKE_MKFIFO_HANG", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own group, so the signal reaches the hanging mkfifo too.
            .process_group(0);
        let child = cmd.spawn().unwrap();
        let group = -(child.id() as i32);
        let hung = sb.state.join("mkfifo.pid");
        for _ in 0..200 {
            if hung.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let ready = hung.exists();
        // SAFETY: plain kill(2) on the process group made for this child.
        unsafe { libc::kill(group, signal) };
        let out = child.wait_with_output().unwrap();
        assert!(ready, "{name}: the wrapper never got to mkfifo");
        assert_ne!(out.status.code(), Some(0), "{name}: {:?}", out.status);
        assert!(!sb.called("exec"), "{name}: docker was run after a signal");
        let left: Vec<_> = fs::read_dir(&tmp).unwrap().collect();
        assert!(left.is_empty(), "{name}: left behind: {left:?}");
    }
}
