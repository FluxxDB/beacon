//! The agent CLI, end to end: the real `beacon` binary against a real node
//! on a loopback port, with the time zone pinned to `America/New_York`.

mod common;

use std::time::Duration;

use assert_cmd::Command;
use beacon::api::{
    AttachmentJson, ChannelJson, ChannelsJson, Delivery, NewPostRequest, PeerJson, PostCreated,
    PostJson, PostsResponse, PresenceJson, StatusJson, StreamEvent, WhoRequest,
};
use beacon::cli::human::{self, Clock};
use beacon::client::{Client, SseMessage, SseParser};
use beacon::error::Error;
use beacon::model::{parse_mentions, Attachment, AuthorKind, Event, EventKind, NewPost};
use beacon::timefmt::iso8601;
use common::*;
use jiff::tz::TimeZone;
use serde_json::{json, Value};

// ------------------------------------------------------------------ harness

/// What a run of the binary printed and how it ended.
#[derive(Debug)]
struct Out {
    code: i32,
    stdout: String,
    stderr: String,
    /// stdout as bytes, for commands that print files.
    raw: Vec<u8>,
}

impl Out {
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {:?}", self.stdout))
    }

    /// stdout without its final newline.
    fn text(&self) -> &str {
        self.stdout.strip_suffix('\n').unwrap_or(&self.stdout)
    }
}

/// One run of the binary: arguments, extra environment, stdin.
struct Run {
    args: Vec<String>,
    server: String,
    secret: String,
    env: Vec<(String, String)>,
    stdin: Vec<u8>,
}

/// A run against `node`, with its URL and secret in the environment.
fn beacon(node: &TestNode, args: &[&str]) -> Run {
    Run {
        args: args.iter().map(|a| a.to_string()).collect(),
        server: node.url.clone(),
        secret: node.secret.clone(),
        env: Vec::new(),
        stdin: Vec::new(),
    }
}

impl Run {
    fn env(mut self, key: &str, value: &str) -> Run {
        self.env.push((key.to_string(), value.to_string()));
        self
    }

    fn server(mut self, url: &str) -> Run {
        self.server = url.to_string();
        self
    }

    fn secret(mut self, secret: &str) -> Run {
        self.secret = secret.to_string();
        self
    }

    fn stdin(mut self, bytes: &str) -> Run {
        self.stdin = bytes.as_bytes().to_vec();
        self
    }

    fn stdin_bytes(mut self, bytes: Vec<u8>) -> Run {
        self.stdin = bytes;
        self
    }

    /// The command line: the environment every run gets, then this run's own.
    fn command(&self) -> std::process::Command {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_beacon"));
        for var in [
            "BEACON_AGENT",
            "BEACON_CLIENT",
            "BEACON_HUMAN",
            "BEACON_NODE",
            "RUST_LOG",
        ] {
            cmd.env_remove(var);
        }
        cmd.env("BEACON_SERVER", &self.server)
            .env("BEACON_SECRET", &self.secret)
            .env("TZ", "America/New_York")
            .args(&self.args);
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        cmd
    }

    /// Runs the binary and waits for it. The node keeps serving meanwhile, so
    /// the blocking wait happens off the async workers.
    async fn run(self) -> Out {
        tokio::task::spawn_blocking(move || {
            let mut cmd = Command::from_std(self.command());
            cmd.write_stdin(self.stdin).timeout(Duration::from_secs(30));
            let output = cmd.output().expect("run beacon");
            Out {
                code: output.status.code().expect("exit code"),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
                raw: output.stdout,
            }
        })
        .await
        .unwrap()
    }

    /// Starts the binary and leaves it running: for `wait` and `watch`, whose
    /// output arrives while the test posts. Dropping the child kills it.
    fn spawn(self) -> Child {
        use std::process::Stdio;
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

        let mut cmd = tokio::process::Command::from(self.command());
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().expect("start beacon");
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        let stderr = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let mut pipe = child.stderr.take().unwrap();
        let sink = stderr.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            while let Ok(n) = pipe.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                sink.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });
        Child {
            child,
            stdout,
            stderr,
            args: self.args,
        }
    }

    /// Runs it and insists on exit 0.
    async fn ok(self) -> Out {
        let args = self.args.clone();
        let out = self.run().await;
        assert_eq!(out.code, 0, "beacon {args:?}: {out:?}");
        out
    }
}

/// A run of the binary that is still going (see [`Run::spawn`]). Every wait on
/// it has a limit, so a hang fails the test instead of blocking it.
struct Child {
    child: tokio::process::Child,
    stdout: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
    stderr: std::sync::Arc<std::sync::Mutex<String>>,
    args: Vec<String>,
}

impl Child {
    /// The next line of stdout, within `limit`.
    async fn line_within(&mut self, limit: Duration) -> String {
        match tokio::time::timeout(limit, self.stdout.next_line()).await {
            Ok(Ok(Some(line))) => line,
            other => panic!(
                "beacon {:?}: no line of stdout within {limit:?} ({other:?}); stderr: {:?}",
                self.args,
                self.stderr()
            ),
        }
    }

    async fn line(&mut self) -> String {
        self.line_within(Duration::from_secs(10)).await
    }

    /// Everything the child wrote to stderr so far.
    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    /// Waits for the child to end on its own, within `limit`; returns its exit
    /// code and what is left on stdout.
    async fn finish_within(&mut self, limit: Duration) -> (i32, String) {
        let status = match tokio::time::timeout(limit, self.child.wait()).await {
            Ok(status) => status.expect("wait for beacon"),
            Err(_) => panic!(
                "beacon {:?} did not exit within {limit:?}; stderr: {:?}",
                self.args,
                self.stderr()
            ),
        };
        let mut rest = String::new();
        while let Ok(Some(line)) = self.stdout.next_line().await {
            rest.push_str(&line);
            rest.push('\n');
        }
        // Let the stderr reader catch up with the closed pipe.
        tokio::time::sleep(Duration::from_millis(50)).await;
        (status.code().expect("exit code"), rest)
    }

    async fn kill(mut self) {
        let _ = self.child.kill().await;
    }
}

/// Creates channel `name` through the CLI.
async fn new_channel(node: &TestNode, name: &str) {
    beacon(node, &["channel", "new", name]).ok().await;
}

/// Posts `message` to `channel` as agent `handle`; returns the `--json` answer.
async fn post_as(node: &TestNode, handle: &str, channel: &str, message: &str) -> Value {
    beacon(node, &["post", channel, message, "--as", handle, "--json"])
        .ok()
        .await
        .json()
}

/// The posts of `channel` as the API has them.
async fn api_posts(node: &TestNode, channel: &str) -> Vec<Value> {
    let body: Value = node
        .get(&format!("/api/channels/{channel}/posts"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body["posts"].as_array().unwrap().clone()
}

/// A free loopback port, for a node that is about to be started on it.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

// ------------------------------------------------- seeding a node with history

const REMOTE_ORIGIN: &str = "mac~abcd1234";

fn ms(iso: &str) -> i64 {
    iso.parse::<jiff::Timestamp>().unwrap().as_millisecond()
}

/// A 26-character id whose last eight characters (the short id) are `short`.
fn ulid(short: &str) -> String {
    assert_eq!(short.len(), 8);
    format!("01JDB6Q8ZK00000000{short}")
}

fn channel_event(seq: u64, name: &str, topic: Option<&str>) -> Event {
    Event {
        origin: REMOTE_ORIGIN.into(),
        seq,
        id: ulid(&format!("CHAN{seq:04}")),
        ts: ms("2025-03-14T21:00:00Z"),
        kind: EventKind::ChannelCreated {
            name: name.into(),
            topic: topic.map(String::from),
        },
    }
}

/// A post that arrived from the device `mac`, written at `ts` (ISO-8601).
struct Remote<'a> {
    seq: u64,
    short: &'a str,
    ts: &'a str,
    channel: &'a str,
    author: &'a str,
    human: bool,
    client: Option<&'a str>,
    body: &'a str,
    tags: &'a [&'a str],
    attachment: Option<(&'a str, u64)>,
}

impl Remote<'_> {
    fn event(&self) -> Event {
        let id = ulid(self.short);
        Event {
            origin: REMOTE_ORIGIN.into(),
            seq: self.seq,
            id: id.clone(),
            ts: ms(self.ts),
            kind: EventKind::PostCreated(NewPost {
                post_id: id,
                channel: self.channel.into(),
                author: self.author.into(),
                author_kind: if self.human {
                    AuthorKind::Human
                } else {
                    AuthorKind::Agent
                },
                client: self.client.map(String::from),
                node: "mac".into(),
                body: self.body.into(),
                mentions: parse_mentions(self.body),
                tags: self.tags.iter().map(|t| t.to_string()).collect(),
                attachment: self.attachment.map(|(name, size)| Attachment {
                    sha256: "ab".repeat(32),
                    name: name.into(),
                    size,
                }),
            }),
        }
    }
}

async fn seed(node: &TestNode, events: Vec<Event>) {
    for ev in events {
        node.state().ingest(ev).await.unwrap().expect("a new event");
    }
}

/// The history the snapshots are about: three days of #beacon on the Mac.
async fn seed_beacon_history(node: &TestNode) {
    seed(
        node,
        vec![
            channel_event(1, "beacon", Some("the agent message board")),
            Remote {
                seq: 2,
                short: "Q8ZK7C2M",
                ts: "2025-03-14T22:31:00Z",
                channel: "beacon",
                author: "mac-juicebox",
                human: false,
                client: Some("codex"),
                body: "arm64 image builds in 2m14s, all 41 tests pass",
                tags: &["done"],
                attachment: Some(("test-report.txt", 12_288)),
            }
            .event(),
            Remote {
                seq: 3,
                short: "7C2MF4T3",
                ts: "2025-03-14T22:36:00Z",
                channel: "beacon",
                author: "mac-juicebox",
                human: false,
                client: Some("codex"),
                body: "@pc-vllm e2e passed across both machines. want me to tag v0.1?",
                tags: &[],
                attachment: None,
            }
            .event(),
            Remote {
                seq: 4,
                short: "R9XW5VHB",
                ts: "2025-03-15T14:02:00Z",
                channel: "beacon",
                author: "Dang",
                human: true,
                client: None,
                body: "Tag it. Then freeze main until I say otherwise.\nThanks!",
                tags: &[],
                attachment: None,
            }
            .event(),
        ],
    )
    .await;
}

// ------------------------------------------------------------ human output

#[tokio::test(flavor = "multi_thread")]
async fn post_and_read_human() {
    let node = spawn_node("pc", &[]).await;
    seed_beacon_history(&node).await;

    let out = beacon(&node, &["read", "beacon"]).ok().await;
    assert_eq!(out.stderr, "");
    insta::assert_snapshot!(out.text());
}

#[tokio::test(flavor = "multi_thread")]
async fn read_unread_json_schema() {
    let node = spawn_node("pc", &[]).await;
    seed_beacon_history(&node).await;

    let out = beacon(&node, &["read", "--unread", "--json", "--as", "pc-vllm"])
        .ok()
        .await;
    let mut body = out.json();
    assert_eq!(out.stdout.matches('\n').count(), 1, "one line of JSON");
    // `arrival` is the node's own numbering; the rest is fixed by the seed.
    for post in body["posts"].as_array_mut().unwrap() {
        assert!(post["arrival"].as_i64().unwrap() > 0);
        post["arrival"] = json!(0);
    }
    insta::assert_snapshot!(serde_json::to_string_pretty(&body).unwrap());
}

/// The field set of a post in `--json` is a contract.
#[tokio::test(flavor = "multi_thread")]
async fn json_post_fields_are_fixed() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    let created = post_as(&node, "pc-configs", "beacon", "hello").await;
    assert_eq!(created["v"], 1);
    let post = created["post"].as_object().unwrap();
    let mut keys: Vec<&str> = post.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "announcement",
            "arrival",
            "attachment",
            "author",
            "author_kind",
            "body",
            "channel",
            "client",
            "id",
            "mentions",
            "node",
            "short_id",
            "tags",
            "ts",
            "ts_ms",
            "v"
        ]
    );
    assert_eq!(created["delivery"], json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn times_follow_tz_and_old_posts_get_a_day_line() {
    let node = spawn_node("pc", &[]).await;
    seed_beacon_history(&node).await;

    let text = beacon(&node, &["read", "beacon"]).ok().await.stdout;
    // 22:31 UTC on 14 March 2025 is 6:31 pm in New York (EDT).
    assert!(text.contains(" 6:31 pm  mac-juicebox"), "{text}");
    // The same instant in Tokyo is already the next morning.
    let tokyo = beacon(&node, &["read", "beacon"])
        .env("TZ", "Asia/Tokyo")
        .ok()
        .await
        .stdout;
    assert!(tokyo.contains(" 7:31 am  mac-juicebox"), "{tokyo}");
    assert!(tokyo.contains("── Sat Mar 15 ──"), "{tokyo}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_post_has_no_day_line() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    post_as(&node, "pc-configs", "beacon", "just now").await;
    let out = beacon(&node, &["read", "beacon"]).ok().await;
    assert!(!out.stdout.contains("──"), "{}", out.stdout);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    // `{time:>8}  {author}  {kind} · {client} · {node}  {short_id}`
    let header = lines[0];
    assert!(header.starts_with(' ') || header.as_bytes()[0].is_ascii_digit());
    assert!(
        header[8..].starts_with("  pc-configs  agent · other · pc  "),
        "{header}"
    );
    assert_eq!(lines[1], "          just now");
}

#[tokio::test(flavor = "multi_thread")]
async fn read_an_empty_channel_says_so() {
    let node = spawn_node("pc", &[]).await;
    let out = beacon(&node, &["read", "general"]).ok().await;
    assert_eq!(out.text(), "no posts in #general");
    let json = beacon(&node, &["read", "general", "--json"])
        .ok()
        .await
        .json();
    assert_eq!(json, json!({ "v": 1, "posts": [], "remaining": null }));
}

// ------------------------------------------------------------------- post

#[tokio::test(flavor = "multi_thread")]
async fn post_reports_delivery_no_peers() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    let out = beacon(&node, &["post", "beacon", "hello", "--as", "pc-configs"])
        .ok()
        .await;
    let line = out.text();
    let short = line
        .strip_prefix("posted ")
        .and_then(|rest| rest.strip_suffix(" to #beacon · no peers yet"))
        .unwrap_or_else(|| panic!("unexpected output: {line:?}"));
    assert_eq!(short.len(), 8);
    assert!(short.bytes().all(|b| b.is_ascii_alphanumeric()));
    // It is the tail of the stored post's id.
    let posts = api_posts(&node, "beacon").await;
    assert!(posts[0]["id"].as_str().unwrap().ends_with(short));
}

#[tokio::test(flavor = "multi_thread")]
async fn post_message_with_leading_dash() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;

    let tricky = "-rf is not a flag \"quoted\" $HOME 🚀";
    beacon(
        &node,
        &["post", "beacon", "--as", "pc-configs", "--", tricky],
    )
    .ok()
    .await;

    // After the message, and before it, options still work.
    let two = "`tick` $(not run) 'single'\nsecond line\twith a tab";
    beacon(
        &node,
        &["post", "beacon", two, "--tag", "fyi", "--as", "pc-configs"],
    )
    .ok()
    .await;
    beacon(
        &node,
        &[
            "post",
            "beacon",
            "--tag",
            "fyi",
            "--as",
            "pc-configs",
            "--",
            "--create",
        ],
    )
    .ok()
    .await;
    // Without `--`, a lone dash-word is taken as the message when it is not a flag.
    beacon(&node, &["post", "beacon", "-rf", "--as", "pc-configs"])
        .ok()
        .await;

    let bodies: Vec<String> = api_posts(&node, "beacon")
        .await
        .iter()
        .map(|p| p["body"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(bodies, [tricky, two, "--create", "-rf"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn post_reads_a_multiline_body_from_stdin() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;

    let body = "first line\n\n  indented `code` $HOME \"q\"\nlast line 🚀";
    beacon(&node, &["post", "beacon", "-", "--as", "pc-configs"])
        .stdin(&format!("{body}\n"))
        .ok()
        .await;
    let posts = api_posts(&node, "beacon").await;
    // Stdin is the message as it came, final newline included.
    assert_eq!(posts[0]["body"], format!("{body}\n"));

    // Invalid UTF-8 is refused, not mangled.
    let bad = tokio::task::spawn_blocking({
        let url = node.url.clone();
        move || {
            Command::new(env!("CARGO_BIN_EXE_beacon"))
                .env("BEACON_SERVER", url)
                .env("BEACON_SECRET", SECRET)
                .args(["post", "beacon", "-", "--as", "pc-configs"])
                .write_stdin(vec![0xff, 0xfe, b'x'])
                .output()
                .unwrap()
        }
    })
    .await
    .unwrap();
    assert_eq!(bad.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("UTF-8"));
    assert_eq!(api_posts(&node, "beacon").await.len(), 1);
}

/// The node refuses a carriage return, and pipes from Windows or WSL end
/// their lines with CRLF: line breaks are sent as LF, whatever they were.
#[tokio::test(flavor = "multi_thread")]
async fn post_sends_line_breaks_as_lf() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;

    beacon(&node, &["post", "beacon", "-", "--as", "pc-configs"])
        .stdin("a\r\nb\r\n")
        .ok()
        .await;
    beacon(
        &node,
        &[
            "post",
            "beacon",
            "--as",
            "pc-configs",
            "--",
            "c\r\nd\re\nf\r",
        ],
    )
    .ok()
    .await;
    beacon(&node, &["post", "beacon", "-", "--as", "pc-configs"])
        .stdin("x\ry\r\r\nz")
        .ok()
        .await;

    let bodies: Vec<String> = api_posts(&node, "beacon")
        .await
        .iter()
        .map(|p| p["body"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(bodies, ["a\nb\n", "c\nd\ne\nf\n", "x\ny\n\nz"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_channel_exits_2_with_hint() {
    let node = spawn_node("pc", &[]).await;
    let out = beacon(&node, &["post", "nosuch", "hi", "--as", "pc-configs"])
        .run()
        .await;
    assert_eq!(out.code, 2, "{out:?}");
    assert_eq!(out.stdout, "");
    assert!(out.stderr.starts_with("beacon: "), "{}", out.stderr);
    assert!(out.stderr.contains("--create"), "{}", out.stderr);
    assert!(
        out.stderr.contains("./beacon channel new"),
        "{}",
        out.stderr
    );
    // Nothing was created.
    let channels = beacon(&node, &["channels", "--json"]).ok().await.json();
    assert_eq!(channels["channels"].as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn post_create_makes_the_channel() {
    let node = spawn_node("pc", &[]).await;
    beacon(
        &node,
        &["post", "builds", "first", "--create", "--as", "pc-configs"],
    )
    .ok()
    .await;
    assert_eq!(api_posts(&node, "builds").await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn hash_prefix_channel_accepted() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    beacon(&node, &["post", "#beacon", "hi", "--as", "pc-configs"])
        .ok()
        .await;
    let out = beacon(&node, &["read", "#Beacon"]).ok().await;
    assert!(out.stdout.contains("          hi"), "{}", out.stdout);
    assert_eq!(api_posts(&node, "beacon").await[0]["body"], "hi");
    // A name that is not a channel name is a usage error before any request.
    let bad = beacon(&node, &["post", "../etc", "hi", "--as", "pc-configs"])
        .run()
        .await;
    assert_eq!(bad.code, 1, "{bad:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn post_human_uses_beacon_human() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    // No handle needed; Dang is the author and the post is an announcement.
    let out = beacon(
        &node,
        &["post", "beacon", "freeze main", "--human", "--json"],
    )
    .ok()
    .await;
    let post = &out.json()["post"];
    assert_eq!(post["author"], "Dang");
    assert_eq!(post["author_kind"], "human");
    assert_eq!(post["announcement"], true);

    let text = beacon(&node, &["read", "beacon"]).ok().await.stdout;
    assert!(text.contains("  Dang  announcement · pc  "), "{text}");
    assert!(!text.contains("agent"), "{text}");

    // Dang does not show up as an agent that is "in" the board.
    let who: Value = node
        .get("/api/who")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(who["agents"], json!([]));
}

// ------------------------------------------------------------ errors, exits

#[tokio::test(flavor = "multi_thread")]
async fn unreachable_exits_3_with_hint() {
    let url = format!("http://127.0.0.1:{}", refused_port());
    let node = spawn_node("pc", &[]).await;
    for args in [
        &["health"][..],
        &["status"],
        &["channels"],
        &["read", "beacon", "--as", "pc-configs"],
        &["post", "beacon", "hi", "--as", "pc-configs"],
    ] {
        let out = beacon(&node, args).server(&url).run().await;
        assert_eq!(out.code, 3, "{args:?}: {out:?}");
        assert_eq!(out.stdout, "");
        assert!(out.stderr.starts_with("beacon: "), "{}", out.stderr);
        assert!(
            out.stderr.contains(
                "beacon isn't running here. Start it with: docker compose up -d   \
                 (in the beacon repo)"
            ),
            "{}",
            out.stderr
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_secret_exits_4() {
    let node = spawn_node("pc", &[]).await;
    for secret in ["not-the-secret", ""] {
        for args in [
            &["channels"][..],
            &["post", "general", "hi", "--as", "pc-configs"],
        ] {
            let out = beacon(&node, args).secret(secret).run().await;
            assert_eq!(out.code, 4, "{args:?}: {out:?}");
            assert!(out.stderr.starts_with("beacon: "), "{}", out.stderr);
            assert!(out.stderr.contains("BEACON_SECRET"), "{}", out.stderr);
        }
    }
    // No post slipped through.
    assert!(api_posts(&node, "general").await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_others_post_needs_force() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    let created = post_as(&node, "mac-juicebox", "beacon", "mine, not yours").await;
    let short = created["post"]["short_id"].as_str().unwrap().to_string();

    let refused = beacon(&node, &["delete", &short, "--as", "pc-configs"])
        .run()
        .await;
    assert_eq!(refused.code, 1, "{refused:?}");
    assert!(
        refused.stderr.contains("mac-juicebox"),
        "{}",
        refused.stderr
    );
    assert!(refused.stderr.contains("--force"), "{}", refused.stderr);
    assert_eq!(api_posts(&node, "beacon").await.len(), 1);

    let forced = beacon(&node, &["delete", &short, "--force", "--as", "pc-configs"])
        .ok()
        .await;
    assert_eq!(forced.text(), format!("deleted {short} from #beacon"));
    assert!(api_posts(&node, "beacon").await.is_empty());

    // Your own post needs no force; JSON carries the full id.
    let own = post_as(&node, "pc-configs", "beacon", "oops").await;
    let full = own["post"]["id"].as_str().unwrap();
    let out = beacon(&node, &["delete", full, "--as", "pc-configs", "--json"])
        .ok()
        .await;
    assert_eq!(out.json(), json!({ "v": 1, "ok": true, "deleted": full }));

    // A post that is not there is exit 2; a handle is still required.
    let gone = beacon(&node, &["delete", "ZZZZZZZZ", "--as", "pc-configs"])
        .run()
        .await;
    assert_eq!(gone.code, 2, "{gone:?}");
    let nameless = beacon(&node, &["delete", &short]).run().await;
    assert_eq!(nameless.code, 1, "{nameless:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn strict_exits_5_for_a_handle_active_elsewhere() {
    let node = spawn_node("pc", &[]).await;
    // codex on this node takes the handle.
    beacon(
        &node,
        &["channels", "--as", "pc-configs", "--client", "codex"],
    )
    .ok()
    .await;

    // The same session again is fine, strict or not.
    beacon(
        &node,
        &[
            "channels",
            "--as",
            "pc-configs",
            "--client",
            "codex",
            "--strict",
        ],
    )
    .ok()
    .await;
    // A different client claiming it is a conflict.
    let out = beacon(
        &node,
        &[
            "channels",
            "--as",
            "pc-configs",
            "--client",
            "claude-code",
            "--strict",
        ],
    )
    .run()
    .await;
    assert_eq!(out.code, 5, "{out:?}");
    assert!(out.stderr.contains("pc-configs"), "{}", out.stderr);
    assert!(out.stderr.contains("codex"), "{}", out.stderr);
    // Without --strict nothing stops it; with a free handle strict is fine.
    beacon(
        &node,
        &["channels", "--as", "pc-configs", "--client", "claude-code"],
    )
    .ok()
    .await;
    beacon(
        &node,
        &[
            "channels", "--as", "pc-other", "--client", "codex", "--strict",
        ],
    )
    .ok()
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_that_need_a_handle_say_so() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    for args in [
        &["post", "beacon", "hi"][..],
        &["read", "--unread"],
        &["read", "--mentions"],
        &["delete", "ABCDEF12"],
    ] {
        let out = beacon(&node, args).run().await;
        assert_eq!(out.code, 1, "{args:?}: {out:?}");
        assert_eq!(out.stdout, "");
        assert_eq!(
            out.stderr.trim_end(),
            "beacon: set BEACON_AGENT or pass --as HANDLE",
            "{args:?}"
        );
    }
    // A blank BEACON_AGENT (the wrapper passes it through) counts as unset.
    let blank = beacon(&node, &["read", "--unread"])
        .env("BEACON_AGENT", "")
        .run()
        .await;
    assert_eq!(blank.code, 1, "{blank:?}");
    // The environment works as well as the flag.
    beacon(&node, &["read", "--unread"])
        .env("BEACON_AGENT", "pc-configs")
        .ok()
        .await;
    // A handle that is not one is a usage error.
    let bad = beacon(&node, &["read", "--unread", "--as", "no spaces"])
        .run()
        .await;
    assert_eq!(bad.code, 1, "{bad:?}");
    // Reading a channel and listing channels need no handle.
    beacon(&node, &["read", "beacon"]).ok().await;
    beacon(&node, &["channels"]).ok().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_errors_exit_1_not_clap_2() {
    let node = spawn_node("pc", &[]).await;
    for args in [
        &["frobnicate"][..],
        &["post", "beacon"],
        &["post", "beacon", "hi", "--nosuchflag"],
        &["read", "beacon", "--unread"],
        &["read", "--unread", "--mentions"],
        &["read", "--limit", "0"],
        &["read", "--limit", "many"],
        &["channel"],
        &["channel", "new"],
    ] {
        let out = beacon(&node, args).run().await;
        assert_eq!(out.code, 1, "{args:?}: {out:?}");
        assert_eq!(out.stdout, "");
        assert!(!out.stderr.is_empty());
    }
    // Help and version are not errors.
    let help = beacon(&node, &["--help"]).ok().await;
    assert!(help.stdout.contains("post"), "{}", help.stdout);
    let sub = beacon(&node, &["post", "--help"]).ok().await;
    assert!(sub.stdout.contains("--tag"), "{}", sub.stdout);
    let version = beacon(&node, &["--version"]).ok().await;
    assert!(version.stdout.starts_with("beacon "), "{}", version.stdout);
}

#[tokio::test(flavor = "multi_thread")]
async fn global_flags_work_before_and_after_the_subcommand() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;

    for args in [
        &[
            "--as",
            "pc-configs",
            "--client",
            "codex",
            "--json",
            "post",
            "beacon",
            "a",
        ][..],
        &[
            "post",
            "beacon",
            "b",
            "--as",
            "pc-configs",
            "--client",
            "codex",
            "--json",
        ],
        &[
            "--json",
            "post",
            "--as",
            "pc-configs",
            "beacon",
            "--client",
            "codex",
            "c",
        ],
    ] {
        let post = beacon(&node, args).ok().await.json();
        assert_eq!(post["post"]["author"], "pc-configs", "{args:?}");
        assert_eq!(post["post"]["client"], "codex", "{args:?}");
    }
    for args in [&["--json", "channels"][..], &["channels", "--json"]] {
        let out = beacon(&node, args).ok().await;
        assert_eq!(out.json()["v"], 1, "{args:?}");
    }
    // BEACON_CLIENT is the default client, the flag wins, blank means other.
    let from_env = beacon(
        &node,
        &["post", "beacon", "d", "--as", "pc-configs", "--json"],
    )
    .env("BEACON_CLIENT", "claude-code")
    .ok()
    .await;
    assert_eq!(from_env.json()["post"]["client"], "claude-code");
    let flag_wins = beacon(
        &node,
        &[
            "post",
            "beacon",
            "e",
            "--as",
            "pc-configs",
            "--client",
            "codex",
            "--json",
        ],
    )
    .env("BEACON_CLIENT", "claude-code")
    .ok()
    .await;
    assert_eq!(flag_wins.json()["post"]["client"], "codex");
    let blank = beacon(
        &node,
        &["post", "beacon", "f", "--as", "pc-configs", "--json"],
    )
    .env("BEACON_CLIENT", "")
    .ok()
    .await;
    assert_eq!(blank.json()["post"]["client"], "other");
}

// ------------------------------------------------------------- read modes

#[tokio::test(flavor = "multi_thread")]
async fn read_unread_summarizes_pages_and_advances() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    node.put("/api/joins/pc-vllm")
        .json(&json!({ "channels": ["beacon"] }))
        .send()
        .await
        .unwrap();
    // Arrival order: Dang's announcement in general, then three agent posts.
    beacon(&node, &["post", "general", "heads up", "--human"])
        .ok()
        .await;
    for body in ["one", "two", "three"] {
        post_as(&node, "mac-juicebox", "beacon", body).await;
    }

    let first = beacon(
        &node,
        &["read", "--unread", "--limit", "3", "--as", "pc-vllm"],
    )
    .ok()
    .await;
    let lines: Vec<&str> = first.text().lines().collect();
    assert_eq!(
        lines[0],
        "◆ announcements  1 new   # beacon  2 new   # general  1 new"
    );
    // Announcements first, from every channel; the channel is named because
    // the posts come from two.
    assert_eq!(lines[1], "# general");
    assert!(
        lines[2].contains("  Dang  announcement · pc  "),
        "{lines:?}"
    );
    assert_eq!(lines[3], "          heads up");
    assert_eq!(lines[4], "# beacon");
    assert!(
        lines[5].contains("  mac-juicebox  agent · other · pc  "),
        "{lines:?}"
    );
    assert_eq!(lines[6], "          one");
    assert_eq!(*lines.last().unwrap(), "1 more unread, run again");

    // Reading advanced the cursors: the rest comes next, in one channel.
    let second = beacon(&node, &["read", "--unread", "--as", "pc-vllm"])
        .ok()
        .await;
    let lines: Vec<&str> = second.text().lines().collect();
    assert_eq!(lines[0], "◆ announcements  none new   # beacon  1 new");
    assert!(
        lines[1].contains("  mac-juicebox  agent · other · pc  "),
        "{lines:?}"
    );
    assert_eq!(lines[2], "          three");
    assert_eq!(lines.len(), 3, "no heading and no paging note: {lines:?}");

    let third = beacon(&node, &["read", "--unread", "--as", "pc-vllm"])
        .ok()
        .await;
    assert_eq!(third.text(), "◆ announcements  none new   no new posts");
    let json = beacon(&node, &["read", "--unread", "--json", "--as", "pc-vllm"])
        .ok()
        .await
        .json();
    assert_eq!(json, json!({ "v": 1, "posts": [], "remaining": 0 }));

    // A plain read never touches the cursors.
    post_as(&node, "mac-juicebox", "beacon", "four").await;
    beacon(&node, &["read", "beacon", "--as", "pc-vllm"])
        .ok()
        .await;
    let again = beacon(&node, &["read", "--unread", "--json", "--as", "pc-vllm"])
        .ok()
        .await
        .json();
    assert_eq!(again["posts"].as_array().unwrap().len(), 1);
}

/// Posts are marked read only once they were printed: if the reader hung up,
/// they are shown again next time instead of being lost.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn unread_stays_unread_when_the_reader_hangs_up() {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;

    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    node.put("/api/joins/pc-vllm")
        .json(&json!({ "channels": ["beacon"] }))
        .send()
        .await
        .unwrap();
    post_as(&node, "mac-juicebox", "beacon", "do not lose me").await;

    // A stdout whose other end is already closed: writing to it fails.
    let (ours, theirs) = UnixStream::pair().unwrap();
    drop(theirs);
    let url = node.url.clone();
    let status = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_beacon"))
            .args(["read", "--unread", "--as", "pc-vllm"])
            .env("BEACON_SERVER", url)
            .env("BEACON_SECRET", SECRET)
            .stdout(Stdio::from(OwnedFd::from(ours)))
            .stderr(Stdio::piped())
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    // Not an error, and not a panic: `beacon read | head -1` is normal.
    assert_eq!(status.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&status.stderr), "");

    let again = beacon(&node, &["read", "--unread", "--json", "--as", "pc-vllm"])
        .ok()
        .await
        .json();
    assert_eq!(again["posts"].as_array().unwrap().len(), 1);
    // Read this time, it is gone.
    let after = beacon(&node, &["read", "--unread", "--json", "--as", "pc-vllm"])
        .ok()
        .await
        .json();
    assert!(after["posts"].as_array().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn read_mentions_announcements_tag_and_limit() {
    let node = spawn_node("pc", &[]).await;
    seed_beacon_history(&node).await;

    let mentions = beacon(&node, &["read", "--mentions", "--as", "pc-vllm"])
        .ok()
        .await;
    // Cross-channel, so the channel is named.
    assert!(
        mentions.stdout.starts_with("── Fri Mar 14 ──\n# beacon\n"),
        "{}",
        mentions.stdout
    );
    assert!(
        mentions.stdout.contains("@pc-vllm e2e passed"),
        "{}",
        mentions.stdout
    );
    assert!(!mentions.stdout.contains("arm64"), "{}", mentions.stdout);
    let nobody = beacon(&node, &["read", "--mentions", "--as", "nobody"])
        .ok()
        .await;
    assert_eq!(nobody.text(), "no posts");

    let announcements = beacon(&node, &["read", "--announcements", "--json"])
        .ok()
        .await
        .json();
    let posts = announcements["posts"].as_array().unwrap();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["author"], "Dang");

    let tagged = beacon(&node, &["read", "beacon", "--tag", "done", "--json"])
        .ok()
        .await
        .json();
    assert_eq!(tagged["posts"].as_array().unwrap().len(), 1);

    let latest = beacon(&node, &["read", "beacon", "--limit", "1", "--json"])
        .ok()
        .await
        .json();
    assert_eq!(
        latest["posts"][0]["author"], "Dang",
        "the newest, not the oldest"
    );

    // Everything, newest last, across channels.
    let all = beacon(&node, &["read", "--json"]).ok().await.json();
    assert_eq!(all["posts"].as_array().unwrap().len(), 3);
    // An unknown channel is exit 2.
    let missing = beacon(&node, &["read", "nosuch"]).run().await;
    assert_eq!(missing.code, 2, "{missing:?}");
}

// -------------------------------------------- channels, status, peers, health

#[tokio::test(flavor = "multi_thread")]
async fn channels_and_channel_commands() {
    let node = spawn_node("pc", &[]).await;

    let created = beacon(
        &node,
        &[
            "channel",
            "new",
            "#Beacon",
            "--topic",
            "the agent message board",
        ],
    )
    .ok()
    .await;
    assert_eq!(
        created.text(),
        "created #beacon · topic: the agent message board"
    );
    let again = beacon(&node, &["channel", "new", "beacon"]).ok().await;
    assert_eq!(
        again.text(),
        "#beacon already exists · topic: the agent message board"
    );
    let json = beacon(&node, &["channel", "new", "vllm", "--json"])
        .ok()
        .await
        .json();
    assert_eq!(json["v"], 1);
    assert_eq!(json["ok"], true);
    assert_eq!(json["created"], true);
    assert_eq!(json["channel"]["name"], "vllm");
    let bad = beacon(&node, &["channel", "new", "Not A Channel"])
        .run()
        .await;
    assert_eq!(bad.code, 1, "{bad:?}");

    let plain = beacon(&node, &["channels"]).ok().await;
    // In the order the channels were made; general is there from the start.
    assert_eq!(
        plain.text(),
        "# general\n# beacon   the agent message board\n# vllm"
    );

    // With a handle, counts of new posts and the announcements line appear.
    beacon(&node, &["post", "beacon", "hi", "--as", "mac-juicebox"])
        .ok()
        .await;
    beacon(&node, &["post", "beacon", "hello", "--human"])
        .ok()
        .await;
    let mine = beacon(&node, &["channels", "--as", "pc-configs"])
        .ok()
        .await;
    assert_eq!(
        mine.text(),
        "◆ announcements  1 new\n\
         # general\n\
         # beacon   the agent message board  2 new\n\
         # vllm"
    );
    let json = beacon(&node, &["channels", "--json", "--as", "pc-configs"])
        .ok()
        .await
        .json();
    assert_eq!(json["v"], 1);
    assert_eq!(json["announcements_unread"], 1);
    assert_eq!(json["channels"][1]["name"], "beacon");
    assert_eq!(json["channels"][1]["unread"], 2);
    assert_eq!(json["channels"][1]["topic"], "the agent message board");
    let no_handle = beacon(&node, &["channels", "--json"]).ok().await.json();
    assert_eq!(no_handle["announcements_unread"], Value::Null);
    assert_eq!(no_handle["channels"][0]["unread"], Value::Null);

    let deleted = beacon(&node, &["channel", "delete", "#vllm"]).ok().await;
    assert_eq!(deleted.text(), "deleted #vllm");
    let json = beacon(&node, &["channel", "delete", "beacon", "--json"])
        .ok()
        .await
        .json();
    assert_eq!(json, json!({ "v": 1, "ok": true, "deleted": "beacon" }));
    let gone = beacon(&node, &["channel", "delete", "beacon"]).run().await;
    assert_eq!(gone.code, 2, "{gone:?}");
    let names: Vec<String> = beacon(&node, &["channels", "--json"]).ok().await.json()["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["general"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn status_peers_and_health() {
    let node = spawn_node("pc", &[]).await;

    let health = beacon(&node, &["health"]).ok().await;
    assert_eq!(health.text(), "ok · pc");
    let json = beacon(&node, &["health", "--json"]).ok().await.json();
    assert_eq!(json, json!({ "v": 1, "node": "pc", "ok": true }));

    let status = beacon(&node, &["status"]).ok().await;
    assert_eq!(
        status.text(),
        format!("pc · beacon {} · no peers yet", env!("CARGO_PKG_VERSION"))
    );
    let json = beacon(&node, &["status", "--json"]).ok().await.json();
    assert_eq!(json["v"], 1);
    assert_eq!(json["node"], "pc");
    assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(json["peers"], json!([]));
    assert!(json["have"].is_object());

    assert_eq!(beacon(&node, &["peers"]).ok().await.text(), "no peers yet");
    let json = beacon(&node, &["peers", "--json"]).ok().await.json();
    assert_eq!(json, json!({ "v": 1, "peers": [] }));
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_heartbeat_only_when_a_handle_is_known() {
    let node = spawn_node("pc", &[]).await;
    let who = |node: &TestNode| {
        let request = node.get("/api/who");
        async move {
            let body: Value = request.send().await.unwrap().json().await.unwrap();
            body["agents"].as_array().unwrap().clone()
        }
    };

    beacon(&node, &["channels"]).ok().await;
    beacon(&node, &["health", "--as", "pc-configs"]).ok().await;
    beacon(&node, &["status"]).ok().await;
    assert!(
        who(&node).await.is_empty(),
        "no handle, no heartbeat; health is a probe"
    );

    beacon(
        &node,
        &["channels", "--as", "pc-configs", "--client", "claude-code"],
    )
    .ok()
    .await;
    let agents = who(&node).await;
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["handle"], "pc-configs");
    assert_eq!(agents[0]["kind"], "agent");
    assert_eq!(agents[0]["client"], "claude-code");
    assert_eq!(agents[0]["node"], "pc");
    assert_eq!(agents[0]["active"], true);
}

// ------------------------------------------------------------------ serve

#[tokio::test(flavor = "multi_thread")]
async fn serve_runs_a_node_until_sigterm() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let url = format!("http://127.0.0.1:{port}");

    let data = dir.path().to_path_buf();
    let outcome = tokio::task::spawn_blocking(move || {
        let server = std::process::Command::new(env!("CARGO_BIN_EXE_beacon"))
            .arg("serve")
            .env("BEACON_NODE", "pc")
            .env("BEACON_SECRET", "test-secret")
            .env("BEACON_LISTEN", format!("127.0.0.1:{port}"))
            .env("BEACON_DATA", &data)
            .env_remove("RUST_LOG")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();

        // The healthcheck is how Docker finds out it is up.
        let mut healthy = false;
        for _ in 0..100 {
            let status = std::process::Command::new(env!("CARGO_BIN_EXE_beacon"))
                .arg("health")
                .env("BEACON_SERVER", &url)
                .env("BEACON_SECRET", "test-secret")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            if status.success() {
                healthy = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        // `docker stop` sends SIGTERM.
        let killed = std::process::Command::new("kill")
            .args(["-TERM", &server.id().to_string()])
            .status()
            .unwrap();
        let started = std::time::Instant::now();
        let output = server.wait_with_output().unwrap();
        (healthy, killed.success(), started.elapsed(), output)
    })
    .await
    .unwrap();

    let (healthy, killed, took, output) = outcome;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(healthy, "the node never came up: {stderr}");
    assert!(killed);
    assert!(took < Duration::from_secs(8), "slow shutdown: {took:?}");
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    // tracing is installed and defaults to info.
    assert!(stderr.contains("beacon listening"), "{stderr}");
    assert!(dir.path().join("beacon.db").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_needs_its_configuration() {
    let node = spawn_node("pc", &[]).await;
    let out = beacon(&node, &["serve"]).run().await;
    assert_eq!(out.code, 1, "{out:?}");
    assert_eq!(out.stderr.trim_end(), "beacon: BEACON_NODE is required");
}

// ------------------------------------------------------ ids and the exit codes

/// A post by `mac-juicebox` that arrived from the Mac.
fn remote_post<'a>(seq: u64, short: &'a str, channel: &'a str, body: &'a str) -> Remote<'a> {
    Remote {
        seq,
        short,
        ts: "2025-03-14T22:31:00Z",
        channel,
        author: "mac-juicebox",
        human: false,
        client: Some("codex"),
        body,
        tags: &[],
        attachment: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ambiguous_id_suffix_exits_5_listing_candidates() {
    let node = spawn_node("pc", &[]).await;
    seed(
        &node,
        vec![
            channel_event(1, "beacon", None),
            remote_post(2, "01ABCDEF", "beacon", "x").event(),
            remote_post(3, "02ABCDEF", "beacon", "y").event(),
        ],
    )
    .await;
    let out = beacon(&node, &["delete", "ABCDEF", "--as", "mac-juicebox"])
        .run()
        .await;
    assert_eq!(out.code, 5, "{out:?}");
    assert!(out.stderr.contains("01ABCDEF"), "{}", out.stderr);
    assert!(out.stderr.contains("02ABCDEF"), "{}", out.stderr);
    // Nothing was deleted; the longer, unique suffix works.
    assert_eq!(api_posts(&node, "beacon").await.len(), 2);
    beacon(&node, &["delete", "1ABCDEF", "--as", "mac-juicebox"])
        .ok()
        .await;
    assert_eq!(api_posts(&node, "beacon").await.len(), 1);
}

// ------------------------------------------------------------------- client

/// A client for `node` with the right secret.
fn client(node: &TestNode) -> Client {
    Client::new(&node.url, Some(SECRET)).unwrap()
}

#[test]
fn client_reads_its_server_from_the_environment() {
    let default = Client::from_vars(|_| None).unwrap();
    assert_eq!(default.base_url(), "http://127.0.0.1:7700");
    let vars = |server: &'static str| {
        move |key: &str| (key == "BEACON_SERVER").then(|| server.to_string())
    };
    for (given, base) in [
        ("http://soybean-wsl:7700/", "http://soybean-wsl:7700"),
        ("dangs-macbook-pro:7700", "http://dangs-macbook-pro:7700"),
        ("  http://127.0.0.1:9  ", "http://127.0.0.1:9"),
        ("", "http://127.0.0.1:7700"),
    ] {
        assert_eq!(Client::from_vars(vars(given)).unwrap().base_url(), base);
    }
    for bad in ["ftp://host", "https://host", "http://", "not a url"] {
        let err = Client::from_vars(vars(bad))
            .err()
            .unwrap_or_else(|| panic!("{bad:?} was accepted"));
        assert!(
            matches!(&err, Error::Usage(m) if m.contains("BEACON_SERVER")),
            "{bad}: {err}"
        );
        assert_eq!(err.exit_code(), 1);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn client_maps_failures_to_errors() {
    let node = spawn_node("pc", &[]).await;
    let client = client(&node);
    assert_eq!(client.health().await.unwrap().node, "pc");

    // 404 is NotFound, in the server's own words.
    let err = client.posts("nosuch", None, None).await.unwrap_err();
    assert!(
        matches!(&err, Error::NotFound(m) if m.contains("no channel #nosuch")),
        "{err}"
    );
    // 400 is Usage.
    let err = client.posts("general", Some(0), None).await.unwrap_err();
    assert!(
        matches!(&err, Error::Usage(m) if m.contains("limit")),
        "{err}"
    );
    // 401 is Auth, whether the secret is wrong or missing.
    let wrong = Client::new(&node.url, Some("wrong")).unwrap();
    assert!(matches!(wrong.channels(None).await, Err(Error::Auth)));
    let none = Client::new(&node.url, None).unwrap();
    assert!(matches!(none.channels(None).await, Err(Error::Auth)));
    // 409 is Conflict.
    seed(
        &node,
        vec![
            remote_post(1, "01ABCDEF", "general", "a").event(),
            remote_post(2, "02ABCDEF", "general", "b").event(),
        ],
    )
    .await;
    let err = client.post_by_id("ABCDEF").await.unwrap_err();
    assert!(
        matches!(&err, Error::Conflict(m) if m.contains("01ABCDEF")),
        "{err}"
    );
    assert_eq!(err.exit_code(), 5);
    // Nothing listening is Unreachable.
    let nowhere = format!("http://127.0.0.1:{}", refused_port());
    let err = Client::new(&nowhere, Some(SECRET))
        .unwrap()
        .health()
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Unreachable(m) if m.contains("127.0.0.1")),
        "{err}"
    );
    assert_eq!(err.exit_code(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn client_round_trips_every_route_it_wraps() {
    let node = spawn_node("pc", &[]).await;
    let client = client(&node);

    let ack = client
        .create_channel("#Beacon", Some("the board"))
        .await
        .unwrap();
    assert!(ack.created);
    assert_eq!(ack.channel.name, "beacon");
    let req = NewPostRequest {
        body: "hello".into(),
        tags: vec!["done".into()],
        author: "pc-configs".into(),
        author_kind: AuthorKind::Agent,
        client: Some("codex".into()),
        create: false,
    };
    let created = client.post("beacon", &req).await.unwrap();
    assert_eq!(created.post.body, "hello");
    assert!(created.delivery.is_empty());

    let page = client
        .posts("beacon", Some(10), Some("done"))
        .await
        .unwrap();
    assert_eq!(page.posts.len(), 1);
    assert_eq!(page.remaining, None);
    let none = client.posts("beacon", None, Some("fyi")).await.unwrap();
    assert!(none.posts.is_empty());
    let by_id = client.post_by_id(&created.post.short_id).await.unwrap();
    assert_eq!(by_id.id, created.post.id);
    let all = client.cross_posts(None, false, None, None).await.unwrap();
    assert_eq!(all.posts.len(), 1);
    let mentions = client
        .cross_posts(Some("pc-vllm"), false, None, None)
        .await
        .unwrap();
    assert!(mentions.posts.is_empty());

    // Presence, joins and unread.
    let who = WhoRequest {
        handle: "pc-vllm".into(),
        kind: AuthorKind::Agent,
        client: "codex".into(),
        status: None,
    };
    assert!(client.heartbeat(&who).await.unwrap().ok);
    assert_eq!(client.who().await.unwrap().agents[0].handle, "pc-vllm");
    let joined = client
        .set_joins("pc-vllm", &["beacon".to_string()])
        .await
        .unwrap();
    assert_eq!(joined.channels, ["beacon"]);
    assert_eq!(client.joins("pc-vllm").await.unwrap().channels, ["beacon"]);
    let unread = client.unread("pc-vllm", None, false).await.unwrap();
    assert_eq!((unread.posts.len(), unread.remaining), (1, Some(0)));
    let advanced = client.unread("pc-vllm", Some(1), true).await.unwrap();
    assert_eq!(advanced.posts.len(), 1);
    let after = client.unread("pc-vllm", None, false).await.unwrap();
    assert!(after.posts.is_empty());

    let channels = client.channels(Some("pc-vllm")).await.unwrap();
    assert_eq!(channels.announcements_unread, Some(0));
    let status = client.status().await.unwrap();
    assert_eq!(status.node, "pc");
    assert_eq!(client.peers().await.unwrap().peers, status.peers);

    let by = Some("pc-configs");
    let deleted = client.delete_post(&created.post.id, by).await.unwrap();
    assert_eq!(deleted.deleted, created.post.id);
    let gone = client.delete_channel("beacon").await.unwrap();
    assert_eq!(gone.deleted, "beacon");
}

/// A post that fails in flight is reported, never replayed: there is no
/// idempotency key, so a retry could duplicate it.
#[tokio::test(flavor = "multi_thread")]
async fn a_post_that_dies_in_flight_is_not_retried() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            // Read the request, then hang up without answering.
            let mut buf = vec![0u8; 4096];
            let _ = socket.read(&mut buf).await;
            drop(socket);
        }
    });

    let client = Client::new(&url, Some(SECRET)).unwrap();
    let req = NewPostRequest {
        body: "once".into(),
        tags: vec![],
        author: "pc-configs".into(),
        author_kind: AuthorKind::Agent,
        client: None,
        create: false,
    };
    let err = client.post("beacon", &req).await.unwrap_err();
    assert!(
        matches!(&err, Error::Interrupted(m) if m.contains("may have been stored")),
        "{err}"
    );
    assert_eq!(err.exit_code(), 3);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(seen.load(Ordering::SeqCst), 1);
}

// --------------------------------------------------------------- formatters
//
// Pure functions over the API types, tested with a fixed clock: Sunday
// 16 March 2025, noon in New York.

fn ny() -> TimeZone {
    TimeZone::get("America/New_York").unwrap()
}

fn clock() -> Clock {
    Clock::new(ny(), ms("2025-03-16T16:00:00Z"))
}

/// An agent post by `mac-juicebox` (codex on `mac`) in #beacon.
fn post(short: &str, iso: &str, body: &str) -> PostJson {
    PostJson {
        v: 1,
        id: ulid(short),
        short_id: short.to_string(),
        channel: "beacon".into(),
        ts: iso8601(ms(iso)),
        ts_ms: ms(iso),
        author: "mac-juicebox".into(),
        author_kind: AuthorKind::Agent,
        client: Some("codex".into()),
        node: "mac".into(),
        body: body.into(),
        tags: vec![],
        mentions: vec![],
        announcement: false,
        attachment: None,
        arrival: 1,
    }
}

fn dang(short: &str, iso: &str, body: &str) -> PostJson {
    PostJson {
        author: "Dang".into(),
        author_kind: AuthorKind::Human,
        client: Some("tui".into()),
        announcement: true,
        ..post(short, iso, body)
    }
}

fn in_channel(mut p: PostJson, channel: &str) -> PostJson {
    p.channel = channel.into();
    p
}

#[test]
fn sizes_are_1024_based_with_a_decimal_from_1_mb() {
    for (bytes, shown) in [
        (0, "0 B"),
        (512, "512 B"),
        (1023, "1023 B"),
        (1024, "1 KB"),
        (1536, "2 KB"),
        (12_288, "12 KB"),
        (1_047_552, "1023 KB"),
        (1_048_575, "1.0 MB"),
        (1_048_576, "1.0 MB"),
        (1_468_006, "1.4 MB"),
        (52_428_800, "50.0 MB"),
        (1_073_741_824, "1.0 GB"),
    ] {
        assert_eq!(human::format_size(bytes), shown, "{bytes}");
    }
}

#[test]
fn a_post_header_follows_the_mockup() {
    let c = clock();
    // Time right-aligned to 8, two spaces between fields, body indented 10.
    let mut p = post(
        "7C2MF4T3",
        "2025-03-14T22:36:00Z",
        "@Dang e2e passed across both machines.",
    );
    assert_eq!(
        human::post_lines(&p, &c),
        [
            " 6:36 pm  mac-juicebox  agent · codex · mac  7C2MF4T3",
            "          @Dang e2e passed across both machines.",
        ]
    );
    p.tags = vec!["done".into(), "handoff".into()];
    assert_eq!(
        human::post_lines(&p, &c)[0],
        " 6:36 pm  mac-juicebox  agent · codex · mac  [done handoff]  7C2MF4T3"
    );
    p.ts_ms = ms("2025-03-15T03:07:00Z"); // 11:07 pm the night before
    assert_eq!(
        human::post_lines(&p, &c)[0],
        "11:07 pm  mac-juicebox  agent · codex · mac  [done handoff]  7C2MF4T3"
    );
    // Without a client the segment is left out.
    p.client = None;
    p.tags.clear();
    assert_eq!(
        human::post_lines(&p, &c)[0],
        "11:07 pm  mac-juicebox  agent · mac  7C2MF4T3"
    );
    // Humans are announcements and show no client.
    let d = dang("R9XW5VHB", "2025-03-14T22:07:00Z", "Starting beacon today.");
    assert_eq!(
        human::post_lines(&d, &c),
        [
            " 6:07 pm  Dang  announcement · mac  R9XW5VHB",
            "          Starting beacon today.",
        ]
    );
}

#[test]
fn an_attachment_line_names_the_command_that_fetches_it() {
    let c = clock();
    let mut p = post(
        "Q8ZK7C2M",
        "2025-03-14T22:31:00Z",
        "arm64 image builds in 2m14s, all 41 tests pass",
    );
    p.tags = vec!["done".into()];
    p.attachment = Some(AttachmentJson {
        name: "test-report.txt".into(),
        size: 12_288,
        sha256: "ab".repeat(32),
        available: true,
    });
    assert_eq!(
        human::post_lines(&p, &c),
        [
            " 6:31 pm  mac-juicebox  agent · codex · mac  [done]  Q8ZK7C2M",
            "          arm64 image builds in 2m14s, all 41 tests pass",
            "          attachment: test-report.txt (12 KB)  →  ./beacon get Q8ZK7C2M -o test-report.txt",
        ]
    );
    // A name the shell would split or expand is quoted in the command only.
    for (name, quoted) in [
        ("my report.txt", "'my report.txt'"),
        ("it's.txt", r"'it'\''s.txt'"),
        ("$HOME.txt", "'$HOME.txt'"),
        ("a-b_c.1", "a-b_c.1"),
    ] {
        p.attachment.as_mut().unwrap().name = name.into();
        let lines = human::post_lines(&p, &c);
        let want =
            format!("          attachment: {name} (12 KB)  →  ./beacon get Q8ZK7C2M -o {quoted}");
        assert_eq!(lines[2], want);
    }
    // Peers choose the name. What is shown, and suggested for -o, is only its
    // last path component, and a name that starts with a dash cannot be read
    // as an option.
    for (name, shown, command) in [
        ("../../etc/passwd", "passwd", "passwd"),
        (r"C:\Users\me\report.txt", "report.txt", "report.txt"),
        ("a/b\\c.txt", "c.txt", "c.txt"),
        ("..", "attachment", "attachment"),
        (".", "attachment", "attachment"),
        ("dir/", "attachment", "attachment"),
        ("-rf.txt", "-rf.txt", "./-rf.txt"),
        ("sub/-x y.txt", "-x y.txt", "./'-x y.txt'"),
        ("evil\u{1b}[2J.txt", "evil_[2J.txt", "'evil_[2J.txt'"),
    ] {
        p.attachment.as_mut().unwrap().name = name.into();
        let lines = human::post_lines(&p, &c);
        let want =
            format!("          attachment: {shown} (12 KB)  →  ./beacon get Q8ZK7C2M -o {command}");
        assert_eq!(lines[2], want, "{name:?}");
    }
    // An attachment without text has no body line.
    p.body = String::new();
    p.attachment.as_mut().unwrap().name = "x.log".into();
    assert_eq!(human::post_lines(&p, &c).len(), 2);
}

#[test]
fn body_text_is_wrapped_at_100_columns_and_cleaned() {
    let c = clock();
    let words = ["lorem"; 30].join(" ");
    let lines = human::post_lines(&post("Q8ZK7C2M", "2025-03-14T22:31:00Z", &words), &c);
    assert_eq!(lines.len(), 3, "{lines:?}");
    for line in &lines[1..] {
        assert!(line.starts_with("          lorem"), "{line:?}");
        assert!(human::display_width(line) <= 100, "{line:?}");
    }
    // Greedy: 15 words fit in the 90 columns after the indent, 16 do not.
    assert_eq!(lines[1].len(), 10 + 15 * 5 + 14);

    // Blank lines and indentation survive; tabs, CRs and escapes do not.
    let body = "plan:\r\n\r\n    1. build\t(fast)\n\u{1b}[31mred\u{1b}[0m \u{7}bell";
    let lines = human::post_lines(&post("Q8ZK7C2M", "2025-03-14T22:31:00Z", body), &c);
    let want = [
        "          plan:".to_string(),
        String::new(),
        format!("{}1. build    (fast)", " ".repeat(14)),
        "          \u{fffd}[31mred\u{fffd}[0m \u{fffd}bell".to_string(),
    ];
    assert_eq!(&lines[1..], want);
    // Names are cleaned too.
    let mut p = post("Q8ZK7C2M", "2025-03-14T22:31:00Z", "x");
    p.author = "evil\u{1b}]0;title\u{7}".into();
    assert!(!human::post_lines(&p, &c)[0].contains('\u{1b}'));
}

#[test]
fn wrap_breaks_at_spaces_and_keeps_structure() {
    assert_eq!(human::wrap_text("a b c", 10), ["a b c"]);
    assert_eq!(human::wrap_text("aaa bbb ccc", 7), ["aaa bbb", "ccc"]);
    assert_eq!(human::wrap_text("aaa bbb", 3), ["aaa", "bbb"]);
    assert_eq!(human::wrap_text("", 10), [""]);
    assert_eq!(
        human::wrap_text("one\n\n  two  three\nfour", 20),
        ["one", "", "  two  three", "four"]
    );
    // The gap at a wrap and trailing spaces disappear; indentation stays.
    assert_eq!(human::wrap_text("  aaa bbb   ", 5), ["  aaa", "bbb"]);
}

/// No line starts with something that belongs to the character before it, or
/// ends on a joiner that wants the next one.
fn assert_graphemes_whole(text: &str, lines: &[String]) {
    assert_eq!(lines.concat(), text);
    for line in lines {
        let first = line.chars().next().unwrap();
        let last = line.chars().last().unwrap();
        let bad_start = matches!(first, '\u{200d}' | '\u{301}' | '\u{1f3fb}'..='\u{1f3ff}');
        assert!(
            !bad_start && last != '\u{200d}',
            "{text:?} broke into {lines:?}"
        );
    }
}

#[test]
fn wrap_hard_breaks_wide_and_long_tokens() {
    let width = |l: &String| human::display_width(l);

    let url = format!("https://example.com/{}", "a".repeat(200));
    let lines = human::wrap_text(&url, 90);
    assert_eq!(lines.concat(), url);
    assert_eq!(lines.iter().map(width).collect::<Vec<_>>(), [90, 90, 40]);

    // A long word starts on its own line rather than filling the last one.
    assert_eq!(
        human::wrap_text("see xxxxxxxxxxxx", 10),
        ["see", "xxxxxxxxxx", "xx"]
    );

    // Wide characters count two columns.
    assert_eq!(
        human::wrap_text("日本語のテキスト", 6),
        ["日本語", "のテキ", "スト"]
    );
    assert!(human::wrap_text("日本 語", 3).iter().all(|l| width(l) <= 3));

    // A grapheme is never split: ZWJ sequence, flag, combining mark, modifier.
    for text in ["ab👨‍👩‍👧cd", "ab🇯🇵🇺🇸cd", "abe\u{301}e\u{301}cd", "👍🏽👍🏽👍🏽"]
    {
        assert_graphemes_whole(text, &human::wrap_text(text, 4));
    }
    let family = human::wrap_text("ab👨‍👩‍👧cd", 4);
    assert!(family.iter().any(|l| l.contains("👨‍👩‍👧")), "{family:?}");
    let flags = human::wrap_text("ab🇯🇵🇺🇸cd", 4);
    assert!(flags.iter().any(|l| l.contains("🇯🇵")), "{flags:?}");
    assert!(flags.iter().any(|l| l.contains("🇺🇸")), "{flags:?}");
}

#[test]
fn day_lines_appear_when_the_day_changes_and_before_an_old_first_post() {
    let c = clock();
    let one = |p: PostJson| human::posts_text(&[p], &c, false);

    // Today is the starting point, so a post from today has no line.
    assert_eq!(
        one(post("AAAAAAAA", "2025-03-16T14:00:00Z", "x")),
        "10:00 am  mac-juicebox  agent · codex · mac  AAAAAAAA\n          x"
    );
    // An older first post is labelled.
    let old = one(post("AAAAAAAA", "2025-03-14T22:31:00Z", "x"));
    assert!(
        old.starts_with("── Fri Mar 14 ──\n 6:31 pm  mac-juicebox"),
        "{old}"
    );
    let yesterday = one(post("AAAAAAAA", "2025-03-15T14:00:00Z", "x"));
    assert!(yesterday.starts_with("── Yesterday ──\n"), "{yesterday}");

    let posts = [
        post("AAAAAAAA", "2025-03-15T14:00:00Z", "a"),
        post("BBBBBBBB", "2025-03-15T15:00:00Z", "b"),
        post("CCCCCCCC", "2025-03-16T14:00:00Z", "c"),
        post("DDDDDDDD", "2025-03-16T15:00:00Z", "d"),
    ];
    let text = human::posts_text(&posts, &c, false);
    let marks: Vec<&str> = text.lines().filter(|l| l.starts_with("──")).collect();
    assert_eq!(marks, ["── Yesterday ──", "── Today ──"]);
    // No blank lines between posts.
    assert!(!text.lines().any(str::is_empty));
}

#[test]
fn channel_lines_name_the_channel_when_asked() {
    let c = clock();
    let posts = [
        in_channel(post("AAAAAAAA", "2025-03-16T14:00:00Z", "a"), "beacon"),
        in_channel(post("BBBBBBBB", "2025-03-16T14:01:00Z", "b"), "beacon"),
        in_channel(post("CCCCCCCC", "2025-03-16T14:02:00Z", "c"), "vllm"),
        in_channel(post("DDDDDDDD", "2025-03-16T14:03:00Z", "d"), "beacon"),
    ];
    let with = human::posts_text(&posts, &c, true);
    let heads: Vec<&str> = with.lines().filter(|l| l.starts_with('#')).collect();
    assert_eq!(heads, ["# beacon", "# vllm", "# beacon"]);
    assert!(with.starts_with("# beacon\n10:00 am"), "{with}");
    assert!(!human::posts_text(&posts, &c, false).contains("# beacon"));

    // On a change of day too, the day line comes first.
    let old = in_channel(post("EEEEEEEE", "2025-03-14T22:31:00Z", "e"), "vllm");
    let text = human::posts_text(&[old], &c, true);
    assert!(
        text.starts_with("── Fri Mar 14 ──\n# vllm\n 6:31 pm"),
        "{text}"
    );
}

#[test]
fn the_unread_summary_counts_what_is_shown() {
    let c = clock();
    let resp = |posts: Vec<PostJson>, remaining| PostsResponse {
        v: 1,
        posts,
        remaining: Some(remaining),
    };

    assert_eq!(
        human::unread_text(&resp(vec![], 0), &c),
        "◆ announcements  none new   no new posts"
    );

    // The mockup's screen: no announcements, two posts in #beacon.
    let mut done = post(
        "Q8ZK7C2M",
        "2025-03-16T14:31:00Z",
        "arm64 image builds in 2m14s, all 41 tests pass",
    );
    done.tags = vec!["done".into()];
    let reply = post(
        "7C2MF4T3",
        "2025-03-16T14:36:00Z",
        "@Dang e2e passed across both machines. want me to tag v0.1?",
    );
    let want = [
        "◆ announcements  none new   # beacon  2 new",
        "10:31 am  mac-juicebox  agent · codex · mac  [done]  Q8ZK7C2M",
        "          arm64 image builds in 2m14s, all 41 tests pass",
        "10:36 am  mac-juicebox  agent · codex · mac  7C2MF4T3",
        "          @Dang e2e passed across both machines. want me to tag v0.1?",
    ]
    .join("\n");
    assert_eq!(
        human::unread_text(&resp(vec![done.clone(), reply.clone()], 0), &c),
        want
    );

    // Announcements first, as the server sorts them; channels alphabetical;
    // the paging note last.
    let ann = in_channel(
        dang("R9XW5VHB", "2025-03-16T14:40:00Z", "freeze main"),
        "general",
    );
    let text = human::unread_text(&resp(vec![ann, done, reply], 37), &c);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0],
        "◆ announcements  1 new   # beacon  2 new   # general  1 new"
    );
    assert_eq!(lines[1], "# general");
    assert!(lines[2].contains("Dang  announcement"), "{lines:?}");
    assert_eq!(lines[4], "# beacon");
    assert_eq!(*lines.last().unwrap(), "37 more unread, run again");
}

#[test]
fn posted_lines_report_delivery_per_peer() {
    let created = |delivery: Vec<Delivery>| PostCreated {
        v: 1,
        post: post("Q8ZK7C2M", "2025-03-16T14:31:00Z", "x"),
        delivery,
    };
    let d = |node: &str, delivered| Delivery {
        node: node.into(),
        delivered,
    };
    assert_eq!(
        human::posted_text(&created(vec![])),
        "posted Q8ZK7C2M to #beacon · no peers yet"
    );
    assert_eq!(
        human::posted_text(&created(vec![d("mac", true)])),
        "posted Q8ZK7C2M to #beacon · delivered to mac"
    );
    assert_eq!(
        human::posted_text(&created(vec![d("mac", true), d("pc-gpu", false)])),
        "posted Q8ZK7C2M to #beacon · delivered to mac · pc-gpu unreachable, queued"
    );
}

#[test]
fn channel_lists_align_topics_and_counts() {
    let ch = |name: &str, topic: Option<&str>, unread| ChannelJson {
        name: name.into(),
        topic: topic.map(String::from),
        unread,
    };
    let list = |channels, ann| ChannelsJson {
        v: 1,
        channels,
        announcements_unread: ann,
    };
    assert_eq!(
        human::channels_text(&list(vec![], None)),
        "no channels yet: create one with ./beacon channel new NAME"
    );
    assert_eq!(
        human::channels_text(&list(
            vec![ch("general", None, None), ch("vllm", None, None)],
            None
        )),
        "# general\n# vllm"
    );
    let want = [
        "◆ announcements  none new",
        "# beacon    the agent message board  2 new",
        "# general",
        "# juicebox  日本語                   11 new",
    ]
    .join("\n");
    assert_eq!(
        human::channels_text(&list(
            vec![
                ch("beacon", Some("the agent message board"), Some(2)),
                ch("general", None, Some(0)),
                ch("juicebox", Some("日本語"), Some(11)),
            ],
            Some(0),
        )),
        want
    );
    // Counts without any topics sit right after the names.
    assert_eq!(
        human::channels_text(&list(
            vec![ch("a", None, Some(1)), ch("bb", None, Some(3))],
            Some(2)
        )),
        "◆ announcements  2 new\n# a   1 new\n# bb  3 new"
    );
}

#[test]
fn peers_and_status_lines() {
    let now = ms("2025-03-16T16:00:00Z");
    let peer = |node: Option<&str>, url: &str, reachable, synced: Option<&str>, queued| PeerJson {
        node: node.map(String::from),
        url: url.into(),
        reachable,
        last_sync: synced.map(|iso| iso8601(ms(iso))),
        queued,
    };
    assert_eq!(human::peers_text(&[], now), "no peers yet");
    let peers = [
        peer(
            Some("mac"),
            "http://dangs-macbook-pro:7700",
            true,
            Some("2025-03-16T15:48:00Z"),
            0,
        ),
        peer(
            Some("pc-gpu"),
            "http://soybean-wsl:7700",
            false,
            Some("2025-03-16T12:00:00Z"),
            14,
        ),
        peer(None, "http://new:7700", true, None, 0),
        peer(Some("vm"), "http://vm:7700", false, None, 3),
        peer(
            Some("mac2"),
            "http://m2:7700",
            true,
            Some("2025-03-16T15:59:50Z"),
            2,
        ),
    ];
    let want = [
        "● mac     http://dangs-macbook-pro:7700  synced 12m ago",
        "○ pc-gpu  http://soybean-wsl:7700        unreachable, last synced 4h ago · 14 queued",
        "● ?       http://new:7700                connected",
        "○ vm      http://vm:7700                 unreachable · 3 queued",
        "● mac2    http://m2:7700                 synced just now · 2 queued",
    ]
    .join("\n");
    assert_eq!(human::peers_text(&peers, now), want);

    let status = |peers: Vec<PeerJson>| StatusJson {
        v: 1,
        node: "pc".into(),
        version: "0.1.0".into(),
        peers,
        have: Default::default(),
    };
    assert_eq!(
        human::status_text(&status(vec![]), now),
        "pc · beacon 0.1.0 · no peers yet"
    );
    assert_eq!(
        human::status_text(&status(vec![peers[0].clone()]), now),
        "pc · beacon 0.1.0 · 1 peer\n● mac  http://dangs-macbook-pro:7700  synced 12m ago"
    );
    let many = human::status_text(&status(peers.to_vec()), now);
    assert!(many.starts_with("pc · beacon 0.1.0 · 5 peers\n"), "{many}");
}

// ------------------------------------------ attach, get, who, join, wait, watch
//
// The commands that move files, show who is here and wake an idle agent. The
// ones that block (`wait`, `watch`) run as children of the test: every read
// from them has a limit, and a child that outlives its test is killed.

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Bytes that are not text: every byte value, line breaks included.
fn binary_blob(len: usize) -> Vec<u8> {
    (0..=255u8).cycle().take(len).collect()
}

/// How many tasks are listening for arrivals on `node` (streams, waits, sync).
fn listeners(node: &TestNode) -> usize {
    node.state().arrivals.receiver_count()
}

/// Waits (up to 5 s) until a stream or a wait has started listening on `node`,
/// that is, until there are more listeners than `before`.
async fn until_listening(node: &TestNode, before: usize) {
    for _ in 0..100 {
        if listeners(node) > before {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("nothing started listening for arrivals within 5 s");
}

async fn join_channel(node: &TestNode, handle: &str, channel: &str) {
    beacon(node, &["join", channel, "--as", handle]).ok().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_and_get_roundtrip() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    let bytes = binary_blob(300_000);

    let out = beacon(
        &node,
        &[
            "attach",
            "beacon",
            "-",
            "--name",
            "data.bin",
            "--tag",
            "done",
            "--as",
            "pc-configs",
            "--json",
            "-rf is not a flag",
        ],
    )
    .stdin_bytes(bytes.clone())
    .ok()
    .await;
    let created = out.json();
    assert_eq!(created["v"], 1);
    let post = &created["post"];
    assert_eq!(post["body"], "-rf is not a flag");
    assert_eq!(post["tags"], json!(["done"]));
    assert_eq!(post["author"], "pc-configs");
    assert_eq!(post["attachment"]["name"], "data.bin");
    assert_eq!(post["attachment"]["size"], 300_000);
    assert_eq!(post["attachment"]["sha256"], sha256_hex(&bytes));
    assert_eq!(post["attachment"]["available"], true);
    let id = post["short_id"].as_str().unwrap().to_string();

    // Without -o the bytes go to stdout, untouched.
    let got = beacon(&node, &["get", &id]).ok().await;
    assert_eq!(got.raw, bytes);
    assert_eq!(got.stderr, "");
    // The full id works too, and so does `-o -`.
    let full = post["id"].as_str().unwrap();
    assert_eq!(beacon(&node, &["get", full]).ok().await.raw, bytes);
    assert_eq!(
        beacon(&node, &["get", &id, "-o", "-"]).ok().await.raw,
        bytes
    );

    // `read` names the command that fetches it.
    let read = beacon(&node, &["read", "beacon"]).ok().await;
    assert!(
        read.stdout.contains(&format!(
            "attachment: data.bin (293 KB)  →  ./beacon get {id} -o data.bin"
        )),
        "{}",
        read.stdout
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_says_what_it_posted() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test-report.txt");
    std::fs::write(&path, vec![b'x'; 12_288]).unwrap();

    // A path works as well as stdin; the name is the file's own.
    let out = beacon(
        &node,
        &[
            "attach",
            "#beacon",
            path.to_str().unwrap(),
            "--as",
            "pc-configs",
        ],
    )
    .ok()
    .await;
    let text = out.text();
    assert!(
        text.starts_with("posted ")
            && text.ends_with(" to #beacon · attached test-report.txt (12 KB) · no peers yet"),
        "{text}"
    );
    assert_eq!(out.stderr, "");
    let posts = api_posts(&node, "beacon").await;
    assert_eq!(posts[0]["body"], "");
    assert_eq!(posts[0]["attachment"]["name"], "test-report.txt");
    assert_eq!(posts[0]["attachment"]["size"], 12_288);

    // --name wins over the file's name; an empty file is a file.
    let empty = dir.path().join("empty");
    std::fs::write(&empty, b"").unwrap();
    beacon(
        &node,
        &[
            "attach",
            "beacon",
            empty.to_str().unwrap(),
            "--name",
            "nothing here.txt",
            "--as",
            "pc-configs",
        ],
    )
    .ok()
    .await;
    let posts = api_posts(&node, "beacon").await;
    assert_eq!(posts[1]["attachment"]["name"], "nothing here.txt");
    assert_eq!(posts[1]["attachment"]["size"], 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_names_with_quotes_and_unicode_survive() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    for name in ["résumé (final).pdf", "say \"cheese\".txt", "日本語.log"] {
        let out = beacon(
            &node,
            &[
                "attach",
                "beacon",
                "-",
                "--name",
                name,
                "--as",
                "pc-configs",
                "--json",
            ],
        )
        .stdin("x")
        .ok()
        .await;
        assert_eq!(out.json()["post"]["attachment"]["name"], name);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_usage_errors() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let missing = dir.path().join("missing.txt");

    // `-` reads stdin, which has no name of its own.
    let out = beacon(&node, &["attach", "beacon", "-", "--as", "pc-configs"])
        .stdin("data")
        .run()
        .await;
    assert_eq!(out.code, 1, "{out:?}");
    assert!(out.stderr.contains("--name"), "{}", out.stderr);
    // No such file is exit 2; a directory is not a file.
    let out = beacon(
        &node,
        &[
            "attach",
            "beacon",
            missing.to_str().unwrap(),
            "--as",
            "pc-configs",
        ],
    )
    .run()
    .await;
    assert_eq!(out.code, 2, "{out:?}");
    assert!(out.stderr.contains("missing.txt"), "{}", out.stderr);
    let out = beacon(
        &node,
        &[
            "attach",
            "beacon",
            sub.to_str().unwrap(),
            "--as",
            "pc-configs",
        ],
    )
    .run()
    .await;
    assert_eq!(out.code, 1, "{out:?}");
    // A handle is needed, and the channel has to exist.
    let out = beacon(&node, &["attach", "beacon", "-", "--name", "a.txt"])
        .stdin("data")
        .run()
        .await;
    assert_eq!(out.code, 1, "{out:?}");
    assert_eq!(
        out.stderr.trim_end(),
        "beacon: set BEACON_AGENT or pass --as HANDLE"
    );
    let out = beacon(
        &node,
        &[
            "attach",
            "nosuch",
            "-",
            "--name",
            "a.txt",
            "--as",
            "pc-configs",
        ],
    )
    .stdin("data")
    .run()
    .await;
    assert_eq!(out.code, 2, "{out:?}");
    assert!(out.stdout.is_empty());
    // Nothing was stored by any of that.
    assert!(api_posts(&node, "beacon").await.is_empty());
}

/// An upload the node refuses before it has read it all (here: too big) is
/// reported in the node's words, not as a broken connection.
#[tokio::test(flavor = "multi_thread")]
async fn attach_over_the_limit_is_refused_in_the_nodes_words() {
    let node = spawn_node_with("pc", &[], |cfg| cfg.max_attachment_mb = 1).await;
    new_channel(&node, "beacon").await;
    let out = beacon(
        &node,
        &[
            "attach",
            "beacon",
            "-",
            "--name",
            "big.bin",
            "--as",
            "pc-configs",
        ],
    )
    .stdin_bytes(binary_blob(4 * 1024 * 1024))
    .run()
    .await;
    assert_eq!(out.code, 1, "{out:?}");
    assert!(out.stderr.starts_with("beacon: "), "{}", out.stderr);
    assert!(out.stderr.contains("1 MB"), "{}", out.stderr);
    assert!(!out.stderr.contains("docker compose"), "{}", out.stderr);
    assert!(api_posts(&node, "beacon").await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn get_unavailable_exits_2() {
    let node = spawn_node("pc", &[]).await;
    seed(
        &node,
        vec![
            channel_event(1, "beacon", None),
            // The post is here, the file is still on the Mac.
            Remote {
                attachment: Some(("test-report.txt", 12_288)),
                ..remote_post(2, "Q8ZK7C2M", "beacon", "report attached")
            }
            .event(),
            remote_post(3, "7C2MF4T3", "beacon", "no file").event(),
        ],
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("report.txt");

    for args in [
        &["get", "Q8ZK7C2M"][..],
        &["get", "Q8ZK7C2M", "-o", target.to_str().unwrap()],
    ] {
        let out = beacon(&node, args).run().await;
        assert_eq!(out.code, 2, "{args:?}: {out:?}");
        assert!(out.raw.is_empty());
        assert_eq!(
            out.stderr.trim_end(),
            "beacon: attachment not synced yet; try again when a peer that has it is online"
        );
    }
    assert!(
        !target.exists(),
        "no file is made for bytes that are not here"
    );

    // A post without an attachment, and a post that does not exist.
    let out = beacon(&node, &["get", "7C2MF4T3"]).run().await;
    assert_eq!(out.code, 2, "{out:?}");
    assert!(out.stderr.contains("has no attachment"), "{}", out.stderr);
    assert!(out.raw.is_empty());
    let out = beacon(&node, &["get", "ZZZZZZZZ"]).run().await;
    assert_eq!(out.code, 2, "{out:?}");
}

/// The node says the file is here but the blob is gone (or a peer's post
/// names a file only some other node holds): the same answer, exit 2.
#[tokio::test(flavor = "multi_thread")]
async fn get_with_the_file_gone_exits_2() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    let created = beacon(
        &node,
        &[
            "attach",
            "beacon",
            "-",
            "--name",
            "a.txt",
            "--as",
            "pc-configs",
            "--json",
        ],
    )
    .stdin("hello")
    .ok()
    .await
    .json();
    let sha = created["post"]["attachment"]["sha256"].as_str().unwrap();
    std::fs::remove_file(node.dir.path().join("files").join(sha)).unwrap();
    let id = created["post"]["short_id"].as_str().unwrap();
    let out = beacon(&node, &["get", id]).run().await;
    assert_eq!(out.code, 2, "{out:?}");
    assert!(out.stderr.contains("not synced yet"), "{}", out.stderr);
}

#[tokio::test(flavor = "multi_thread")]
async fn get_to_a_file_names_it_safely() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    let bytes = binary_blob(70_000);
    let created = beacon(
        &node,
        &[
            "attach",
            "beacon",
            "-",
            "--name",
            "data.bin",
            "--as",
            "pc-configs",
            "--json",
        ],
    )
    .stdin_bytes(bytes.clone())
    .ok()
    .await
    .json();
    let sha = created["post"]["attachment"]["sha256"].as_str().unwrap();
    let id = created["post"]["short_id"].as_str().unwrap();
    let dir = tempfile::tempdir().unwrap();

    // To a path: exactly there, and the answer says what was saved.
    let target = dir.path().join("copy.bin");
    let out = beacon(&node, &["get", id, "-o", target.to_str().unwrap()])
        .ok()
        .await;
    assert_eq!(std::fs::read(&target).unwrap(), bytes);
    assert_eq!(
        out.text(),
        format!("saved data.bin (68 KB) to {}", target.display())
    );
    // --json says the same in JSON.
    let again = beacon(
        &node,
        &["get", id, "-o", target.to_str().unwrap(), "--json"],
    )
    .ok()
    .await
    .json();
    assert_eq!(again["v"], 1);
    assert_eq!(again["ok"], true);
    assert_eq!(again["name"], "data.bin");
    assert_eq!(again["size"], 70_000);
    assert_eq!(again["path"], target.to_str().unwrap());
    // Nothing is left over from the download.
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(leftovers, ["copy.bin"]);

    // To a directory: under the attachment's own name.
    let into = dir.path().join("into");
    std::fs::create_dir(&into).unwrap();
    beacon(&node, &["get", id, "-o", into.to_str().unwrap()])
        .ok()
        .await;
    assert_eq!(std::fs::read(into.join("data.bin")).unwrap(), bytes);

    // A peer's post can name the file anything. Whatever it says, the file
    // lands inside the directory.
    for (seq, name, lands_as) in [
        (10, "../../etc/passwd", "passwd"),
        (11, r"..\..\evil.sh", "evil.sh"),
        (12, "a/b\\c.txt", "c.txt"),
        (13, "..", "attachment"),
        (14, "dir/", "attachment"),
    ] {
        let mut event = remote_post(seq, &format!("PEER{seq:04}"), "beacon", "from afar").event();
        if let EventKind::PostCreated(post) = &mut event.kind {
            post.attachment = Some(Attachment {
                sha256: sha.to_string(),
                name: name.to_string(),
                size: 70_000,
            });
        }
        seed(&node, vec![event]).await;
        let into = dir.path().join(format!("peer{seq}"));
        std::fs::create_dir(&into).unwrap();
        beacon(
            &node,
            &[
                "get",
                &format!("PEER{seq:04}"),
                "-o",
                into.to_str().unwrap(),
            ],
        )
        .ok()
        .await;
        let names: Vec<_> = std::fs::read_dir(&into)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, [lands_as], "{name:?}");
    }
    assert!(!dir.path().join("passwd").exists());
    assert!(!dir.path().join("evil.sh").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn who_lists_heartbeat_with_status() {
    let node = spawn_node("pc", &[]).await;
    let out = beacon(
        &node,
        &[
            "who",
            "--here",
            "scaffolding beacon crate",
            "--as",
            "pc-configs",
            "--client",
            "claude-code",
        ],
    )
    .ok()
    .await;
    // The brief's template: {dot} {handle:<16} {client} · {node:<6} {age:>4}   {status}
    let want = format!(
        "● {:<16} {} · {:<6} {:>4}   {}",
        "pc-configs", "claude-code", "pc", "now", "scaffolding beacon crate"
    );
    assert_eq!(out.text(), want);

    // Anyone can look, with no handle; a different client lines up.
    beacon(
        &node,
        &[
            "who",
            "--here",
            "arm64 build",
            "--as",
            "mac-juicebox",
            "--client",
            "codex",
        ],
    )
    .ok()
    .await;
    let out = beacon(&node, &["who"]).ok().await;
    assert_eq!(out.stderr, "");
    let lines: Vec<&str> = out.text().lines().collect();
    assert_eq!(lines.len(), 2, "{}", out.stdout);
    let status_column = |line: &str, status: &str| {
        let at = line
            .find(status)
            .unwrap_or_else(|| panic!("{line}: {status}"));
        human::display_width(&line[..at])
    };
    let rows = [
        lines.iter().find(|l| l.contains("pc-configs")).unwrap(),
        lines.iter().find(|l| l.contains("mac-juicebox")).unwrap(),
    ];
    assert_eq!(
        status_column(rows[0], "scaffolding"),
        status_column(rows[1], "arm64 build"),
        "{}",
        out.stdout
    );
    assert!(
        rows[1].starts_with(&format!("● {:<16} codex · pc ", "mac-juicebox")),
        "{}",
        rows[1]
    );

    // A later heartbeat without a status leaves the status alone.
    beacon(
        &node,
        &["channels", "--as", "pc-configs", "--client", "claude-code"],
    )
    .ok()
    .await;
    let again = beacon(&node, &["who"]).ok().await;
    assert!(
        again.stdout.contains("scaffolding beacon crate"),
        "{}",
        again.stdout
    );

    // JSON is the node's own answer.
    let json = beacon(&node, &["who", "--json"]).ok().await.json();
    assert_eq!(json["v"], 1);
    let agents = json["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 2);
    let me = agents.iter().find(|a| a["handle"] == "pc-configs").unwrap();
    assert_eq!(me["status"], "scaffolding beacon crate");
    assert_eq!(me["client"], "claude-code");
    assert_eq!(me["node"], "pc");
    assert_eq!(me["kind"], "agent");
    assert_eq!(me["active"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn who_here_needs_a_handle_and_a_valid_status() {
    let node = spawn_node("pc", &[]).await;
    let out = beacon(&node, &["who", "--here", "working"]).run().await;
    assert_eq!(out.code, 1, "{out:?}");
    assert_eq!(
        out.stderr.trim_end(),
        "beacon: set BEACON_AGENT or pass --as HANDLE"
    );
    // A status the node refuses is a usage error, not a crash.
    let long = "x".repeat(300);
    let out = beacon(&node, &["who", "--here", &long, "--as", "pc-configs"])
        .run()
        .await;
    assert_eq!(out.code, 1, "{out:?}");
    // No agents yet: a line that says so.
    let out = beacon(&node, &["who"]).ok().await;
    assert_eq!(out.text(), "nobody has been seen lately");
    // --here without text is just "I am here".
    let out = beacon(&node, &["who", "--here", "--as", "pc-configs"])
        .ok()
        .await;
    assert!(out.text().starts_with("● pc-configs "), "{}", out.stdout);
}

#[tokio::test(flavor = "multi_thread")]
async fn join_and_leave() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    new_channel(&node, "vllm").await;

    let out = beacon(&node, &["join", "#beacon", "--as", "pc-vllm"])
        .ok()
        .await;
    assert_eq!(
        out.text(),
        "joined #beacon · wait and watch will wake you for posts here"
    );
    // Again: nothing changes, nothing is wrong.
    beacon(&node, &["join", "beacon", "--as", "pc-vllm"])
        .ok()
        .await;
    let out = beacon(&node, &["join", "vllm", "--as", "pc-vllm", "--json"])
        .ok()
        .await;
    assert_eq!(
        out.json(),
        json!({ "v": 1, "ok": true, "channel": "vllm", "joined": ["beacon", "vllm"] })
    );
    // Joins are per handle.
    let other = beacon(&node, &["join", "beacon", "--as", "pc-configs", "--json"])
        .ok()
        .await
        .json();
    assert_eq!(other["joined"], json!(["beacon"]));

    let out = beacon(&node, &["leave", "#beacon", "--as", "pc-vllm"])
        .ok()
        .await;
    assert_eq!(
        out.text(),
        "left #beacon · mentions and announcements still wake you"
    );
    let out = beacon(&node, &["leave", "beacon", "--as", "pc-vllm", "--json"])
        .ok()
        .await;
    assert_eq!(
        out.json(),
        json!({ "v": 1, "ok": true, "channel": "beacon", "joined": ["vllm"] })
    );

    // The channel has to exist, whichever way; the handle has to be known.
    for verb in ["join", "leave"] {
        let out = beacon(&node, &[verb, "nosuch", "--as", "pc-vllm"])
            .run()
            .await;
        assert_eq!(out.code, 2, "{verb}: {out:?}");
        assert!(out.stderr.contains("no channel #nosuch"), "{}", out.stderr);
        let out = beacon(&node, &[verb, "beacon"]).run().await;
        assert_eq!(out.code, 1, "{verb}: {out:?}");
        let out = beacon(&node, &[verb, "../x", "--as", "pc-vllm"])
            .run()
            .await;
        assert_eq!(out.code, 1, "{verb}: {out:?}");
    }

    // A channel that was deleted can still be left.
    beacon(&node, &["channel", "delete", "vllm"]).ok().await;
    let out = beacon(&node, &["leave", "vllm", "--as", "pc-vllm", "--json"])
        .ok()
        .await;
    assert_eq!(out.json()["joined"], json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn join_then_wait_wakes() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    join_channel(&node, "pc-vllm", "beacon").await;

    let before = listeners(&node);
    let mut wait = beacon(&node, &["wait", "--timeout", "10", "--as", "pc-vllm"]).spawn();
    until_listening(&node, before).await;
    post_as(
        &node,
        "pc-configs",
        "beacon",
        "pushed main, arm64 is all yours",
    )
    .await;

    let (code, stdout) = wait.finish_within(Duration::from_secs(2)).await;
    assert_eq!(code, 0, "stderr: {}", wait.stderr());
    assert!(
        stdout.contains("pc-configs  agent · other · pc"),
        "{stdout}"
    );
    assert!(
        stdout.contains("pushed main, arm64 is all yours"),
        "{stdout}"
    );
    assert!(
        stdout.contains("# beacon"),
        "the channel is named: {stdout}"
    );
    assert_eq!(wait.stderr(), "");
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_timeout_exits_6() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    join_channel(&node, "pc-vllm", "beacon").await;
    let started = std::time::Instant::now();
    let out = beacon(&node, &["wait", "--timeout", "1", "--as", "pc-vllm"])
        .run()
        .await;
    assert_eq!(out.code, 6, "{out:?}");
    assert_eq!(out.stdout, "");
    assert_eq!(out.stderr.trim_end(), "no new posts in 1s");
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(900) && took < Duration::from_secs(5),
        "{took:?}"
    );
    // --json prints nothing either.
    let out = beacon(
        &node,
        &["wait", "--timeout", "0", "--json", "--as", "pc-vllm"],
    )
    .run()
    .await;
    assert_eq!((out.code, out.stdout.as_str()), (6, ""));
}

/// `wait` looks before it waits, so a post that came in since the last read is
/// never missed; and it only looks: the post is still unread afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn wait_returns_at_once_and_never_advances_cursors() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    join_channel(&node, "pc-vllm", "beacon").await;
    post_as(&node, "pc-configs", "beacon", "already here").await;

    for _ in 0..2 {
        let started = std::time::Instant::now();
        let out = beacon(&node, &["wait", "--timeout", "30", "--as", "pc-vllm"])
            .ok()
            .await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(out.stdout.contains("already here"), "{}", out.stdout);
    }
    let unread = beacon(&node, &["read", "--unread", "--as", "pc-vllm"])
        .ok()
        .await;
    assert!(unread.stdout.contains("already here"), "{}", unread.stdout);
    // Now it is read: waiting finds nothing.
    let out = beacon(&node, &["wait", "--timeout", "1", "--as", "pc-vllm"])
        .run()
        .await;
    assert_eq!(out.code, 6, "{out:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_wakes_for_mentions_and_announcements_not_for_chatter() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    new_channel(&node, "vllm").await;
    // pc-vllm has not joined anything; its own posts never wake it.
    post_as(&node, "pc-configs", "beacon", "just chatter").await;
    post_as(&node, "pc-vllm", "vllm", "talking to myself @pc-vllm").await;
    let out = beacon(&node, &["wait", "--timeout", "1", "--as", "pc-vllm"])
        .run()
        .await;
    assert_eq!(out.code, 6, "{out:?}");

    let before = listeners(&node);
    let mut wait = beacon(&node, &["wait", "--timeout", "10", "--as", "pc-vllm"]).spawn();
    until_listening(&node, before).await;
    post_as(
        &node,
        "pc-configs",
        "vllm",
        "@pc-vllm can you look at this?",
    )
    .await;
    let (code, stdout) = wait.finish_within(Duration::from_secs(2)).await;
    assert_eq!(code, 0);
    assert!(stdout.contains("can you look at this?"), "{stdout}");
    assert!(!stdout.contains("just chatter"), "{stdout}");

    // Dang's post wakes it whatever the channel; that one is read first.
    beacon(&node, &["read", "--unread", "--as", "pc-vllm"])
        .ok()
        .await;
    let before = listeners(&node);
    let mut wait = beacon(&node, &["wait", "--timeout", "10", "--as", "pc-vllm"]).spawn();
    until_listening(&node, before).await;
    beacon(&node, &["post", "beacon", "freeze main", "--human"])
        .ok()
        .await;
    let (code, stdout) = wait.finish_within(Duration::from_secs(2)).await;
    assert_eq!(code, 0);
    assert!(stdout.contains("freeze main"), "{stdout}");
    assert!(stdout.contains("announcement"), "{stdout}");
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_json_is_the_unread_page() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    join_channel(&node, "pc-vllm", "beacon").await;
    post_as(&node, "pc-configs", "beacon", "one").await;
    post_as(&node, "pc-configs", "beacon", "two").await;
    let out = beacon(&node, &["wait", "--json", "--as", "pc-vllm"])
        .ok()
        .await;
    assert_eq!(out.stdout.matches('\n').count(), 1, "one line of JSON");
    let body = out.json();
    assert_eq!(body["v"], 1);
    let bodies: Vec<&str> = body["posts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["body"].as_str().unwrap())
        .collect();
    assert_eq!(bodies, ["one", "two"]);
    assert_eq!(body["remaining"], 0);
    assert_eq!(body["posts"][0]["v"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_and_watch_need_a_handle_and_a_node() {
    let node = spawn_node("pc", &[]).await;
    for args in [
        &["wait"][..],
        &["watch"],
        &["join", "general"],
        &["leave", "general"],
    ] {
        let out = beacon(&node, args).run().await;
        assert_eq!(out.code, 1, "{args:?}: {out:?}");
        assert_eq!(
            out.stderr.trim_end(),
            "beacon: set BEACON_AGENT or pass --as HANDLE",
            "{args:?}"
        );
    }
    // With the node down, they say so at once (exit 3, with the hint).
    let url = format!("http://127.0.0.1:{}", refused_port());
    for args in [
        &["wait", "--timeout", "30", "--as", "pc-vllm"][..],
        &["watch", "--as", "pc-vllm"],
        &["join", "general", "--as", "pc-vllm"],
        &["who"],
    ] {
        let started = std::time::Instant::now();
        let out = beacon(&node, args).server(&url).run().await;
        assert_eq!(out.code, 3, "{args:?}: {out:?}");
        assert_eq!(out.stdout, "");
        assert!(
            out.stderr.contains("docker compose up -d"),
            "{}",
            out.stderr
        );
        assert!(started.elapsed() < Duration::from_secs(5), "{args:?}");
    }
    // A bad secret is exit 4 here as everywhere.
    let out = beacon(&node, &["wait", "--as", "pc-vllm"])
        .secret("wrong")
        .run()
        .await;
    assert_eq!(out.code, 4, "{out:?}");
}

/// A node that goes away and comes back while `wait` is blocked does not end
/// the wait early or lose the wake-up.
#[tokio::test(flavor = "multi_thread")]
async fn wait_reconnects_when_the_node_restarts() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    join_channel(&node, "pc-vllm", "beacon").await;

    let before = listeners(&node);
    let mut wait = beacon(&node, &["wait", "--timeout", "30", "--as", "pc-vllm"]).spawn();
    until_listening(&node, before).await;
    let node = node.stop().await.restart().await;
    post_as(&node, "pc-configs", "beacon", "back again").await;

    let (code, stdout) = wait.finish_within(Duration::from_secs(10)).await;
    assert_eq!(code, 0, "stderr: {}", wait.stderr());
    assert!(stdout.contains("back again"), "{stdout}");
}

/// ...and if it stays away, the wait still ends at its own timeout with 6.
#[tokio::test(flavor = "multi_thread")]
async fn wait_gives_up_at_its_timeout_when_the_node_stays_away() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    join_channel(&node, "pc-vllm", "beacon").await;

    let before = listeners(&node);
    let mut wait = beacon(&node, &["wait", "--timeout", "3", "--as", "pc-vllm"]).spawn();
    until_listening(&node, before).await;
    let _gone = node.stop().await;
    let (code, stdout) = wait.finish_within(Duration::from_secs(8)).await;
    assert_eq!((code, stdout.as_str()), (6, ""));
    let stderr = wait.stderr();
    assert!(stderr.contains("beacon: reconnecting…"), "{stderr}");
    assert!(
        stderr.trim_end().ends_with("no new posts in 3s"),
        "{stderr}"
    );
}

/// The part of a `watch` line before the channel is a time: `6:07 pm`.
fn assert_clock_time(time: &str) {
    let (clock, suffix) = time.split_once(' ').unwrap_or_else(|| panic!("{time:?}"));
    let (hour, minute) = clock.split_once(':').unwrap_or_else(|| panic!("{time:?}"));
    assert!(matches!(suffix, "am" | "pm"), "{time:?}");
    assert!((1..=12).contains(&hour.parse::<u32>().unwrap()), "{time:?}");
    assert!(!hour.starts_with('0'), "{time:?}");
    assert!(
        minute.len() == 2 && minute.parse::<u32>().unwrap() < 60,
        "{time:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_prints_lines() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    new_channel(&node, "vllm").await;
    join_channel(&node, "pc-vllm", "beacon").await;

    let before = listeners(&node);
    let mut watch = beacon(&node, &["watch", "--as", "pc-vllm"]).spawn();
    until_listening(&node, before).await;
    post_as(&node, "pc-configs", "beacon", "first line\nsecond line").await;
    // Not joined, no mention, not Dang: not for pc-vllm.
    post_as(&node, "pc-configs", "vllm", "chatter elsewhere").await;
    // Its own posts never wake it.
    post_as(&node, "pc-vllm", "beacon", "my own post").await;
    post_as(&node, "mac-juicebox", "vllm", "@pc-vllm over here").await;

    let line = watch.line().await;
    let (time, rest) = line.split_once(" #").expect(&line);
    assert_clock_time(time);
    assert_eq!(rest, "beacon pc-configs: first line");
    let line = watch.line().await;
    let (time, rest) = line.split_once(" #").expect(&line);
    assert_clock_time(time);
    assert_eq!(rest, "vllm mac-juicebox: @pc-vllm over here");
    watch.kill().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_json_is_one_post_per_line() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    let before = listeners(&node);
    let mut watch = beacon(&node, &["watch", "--json", "--as", "pc-vllm"]).spawn();
    until_listening(&node, before).await;
    // Dang's posts wake every watcher.
    beacon(&node, &["post", "beacon", "line one\nline two", "--human"])
        .ok()
        .await;
    let line = watch.line().await;
    let post: Value = serde_json::from_str(&line).expect(&line);
    assert_eq!(post["v"], 1);
    assert_eq!(post["body"], "line one\nline two");
    assert_eq!(post["author"], "Dang");
    assert_eq!(post["announcement"], true);
    assert_eq!(post["channel"], "beacon");
    assert!(post["id"].as_str().unwrap().len() == 26);
    watch.kill().await;
}

/// When the node restarts, `watch` says so, reconnects, and prints the posts
/// that came in while it was not listening, once each and in order.
#[tokio::test(flavor = "multi_thread")]
async fn watch_reconnects_and_catches_up() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    join_channel(&node, "pc-vllm", "beacon").await;

    let before = listeners(&node);
    let mut watch = beacon(&node, &["watch", "--as", "pc-vllm"]).spawn();
    until_listening(&node, before).await;
    post_as(&node, "pc-configs", "beacon", "before").await;
    assert!(watch.line().await.ends_with("beacon pc-configs: before"));

    let node = node.stop().await.restart().await;
    // Written while the watcher is between connections.
    post_as(&node, "pc-configs", "beacon", "during").await;
    let line = watch.line_within(Duration::from_secs(15)).await;
    assert!(line.ends_with("beacon pc-configs: during"), "{line}");
    assert!(
        watch.stderr().contains("beacon: reconnecting…"),
        "{}",
        watch.stderr()
    );

    // Live again.
    post_as(&node, "pc-configs", "beacon", "after").await;
    assert!(watch.line().await.ends_with("beacon pc-configs: after"));
    // And nothing was printed twice.
    let more = tokio::time::timeout(Duration::from_millis(700), watch.stdout.next_line()).await;
    assert!(more.is_err(), "unexpected extra line: {more:?}");
    watch.kill().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_starts_with_what_is_new_not_what_is_old() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    join_channel(&node, "pc-vllm", "beacon").await;
    post_as(&node, "pc-configs", "beacon", "old news").await;

    let before = listeners(&node);
    let mut watch = beacon(&node, &["watch", "--as", "pc-vllm"]).spawn();
    until_listening(&node, before).await;
    post_as(&node, "pc-configs", "beacon", "new news").await;
    assert!(watch.line().await.ends_with("beacon pc-configs: new news"));
    watch.kill().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn skill_show_prints_the_embedded_skill() {
    let node = spawn_node("pc", &[]).await;
    let out = beacon(&node, &["skill", "show"]).ok().await;
    assert_eq!(out.stdout, include_str!("../skills/beacon/SKILL.md"));
    assert!(
        out.stdout.starts_with("---\nname: beacon\n"),
        "{}",
        out.stdout
    );
    // It needs no node and no handle.
    let url = format!("http://127.0.0.1:{}", refused_port());
    let out = beacon(&node, &["skill", "show"]).server(&url).ok().await;
    assert!(!out.stdout.is_empty());

    // Installing is the host's job (the wrapper's), not the container's.
    for args in [&["skill", "install"][..], &["skill", "install", "claude"]] {
        let out = beacon(&node, args).run().await;
        assert_eq!(out.code, 1, "{args:?}: {out:?}");
        assert_eq!(out.stdout, "");
        assert_eq!(
            out.stderr.trim_end(),
            "beacon: run ./beacon skill install on the host"
        );
    }
    let out = beacon(&node, &["skill"]).run().await;
    assert_eq!(out.code, 1, "{out:?}");
}

// ------------------------------------------- failures: what is said, and when

/// The `docker compose up -d` hint is for a node that is not there. A node
/// that was reached and then dropped the request is a different problem.
#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_request_is_not_a_missing_node() {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            // Read the request, then hang up without answering.
            let mut buf = vec![0u8; 4096];
            let _ = socket.read(&mut buf).await;
            drop(socket);
        }
    });
    let node = spawn_node("pc", &[]).await;

    // A post that may have been stored says so, and what to do about it.
    let out = beacon(&node, &["post", "beacon", "hi", "--human"])
        .server(&url)
        .run()
        .await;
    assert_eq!(out.code, 3, "{out:?}");
    assert!(out.stderr.starts_with("beacon: "), "{}", out.stderr);
    assert!(
        out.stderr.contains("may have been stored"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr
            .contains("run `./beacon read beacon` before posting again"),
        "{}",
        out.stderr
    );
    assert!(!out.stderr.contains("docker compose"), "{}", out.stderr);
    // So does an upload.
    let out = beacon(
        &node,
        &[
            "attach",
            "beacon",
            "-",
            "--name",
            "a.txt",
            "--as",
            "pc-configs",
        ],
    )
    .server(&url)
    .stdin("data")
    .run()
    .await;
    assert_eq!(out.code, 3, "{out:?}");
    assert!(!out.stderr.contains("docker compose"), "{}", out.stderr);
    // Anything else that dies on the way is just that: exit 3, no hint.
    let out = beacon(&node, &["channels"]).server(&url).run().await;
    assert_eq!(out.code, 3, "{out:?}");
    assert!(!out.stderr.contains("docker compose"), "{}", out.stderr);
}

/// The Docker healthcheck runs `beacon health` with whatever environment the
/// container has; it needs the node's address and secret, nothing else.
#[tokio::test(flavor = "multi_thread")]
async fn health_does_not_care_about_the_agent_variables() {
    let node = spawn_node("pc", &[]).await;
    let out = beacon(&node, &["health"])
        .env("BEACON_AGENT", "not a handle!")
        .env("BEACON_CLIENT", "")
        .ok()
        .await;
    assert_eq!(out.text(), "ok · pc");
    let out = beacon(&node, &["health", "--json", "--as", "bad handle"])
        .ok()
        .await;
    assert_eq!(out.json(), json!({ "v": 1, "node": "pc", "ok": true }));
    // Other commands still insist on a real handle.
    let out = beacon(&node, &["channels"])
        .env("BEACON_AGENT", "not a handle!")
        .run()
        .await;
    assert_eq!(out.code, 1, "{out:?}");
}

// -------------------------------- the client: files, waiting, the live stream

#[test]
fn server_sent_events_are_read_however_the_bytes_are_cut() {
    let wire = "\
: keep-alive\n\
\n\
event: post\n\
data: {\"a\":1}\n\
\n\
event:presence\r\n\
data: first\r\n\
data: second\r\n\
\r\n\
id: 7\n\
retry: 100\n\
event: ignored-for-want-of-data\n\
\n\
data\n\
\n\
event: peers\n\
data: [\"ü\",\"日本\"]\n\
\n\
: and a comment before the end\n";
    let want = [
        ("post", "{\"a\":1}"),
        ("presence", "first\nsecond"),
        // An empty data line is data (an empty string), with no event name.
        ("message", ""),
        ("peers", "[\"ü\",\"日本\"]"),
    ];
    // All at once, a byte at a time, and in every two-piece split (which cuts
    // lines, CRLFs and multi-byte characters in half).
    let mut splits: Vec<Vec<&[u8]>> = vec![vec![wire.as_bytes()]];
    splits.push(wire.as_bytes().chunks(1).collect());
    for at in 1..wire.len() {
        splits.push(vec![&wire.as_bytes()[..at], &wire.as_bytes()[at..]]);
    }
    for pieces in splits {
        let mut parser = SseParser::default();
        let mut got = Vec::new();
        for piece in &pieces {
            got.extend(parser.push(piece).unwrap());
        }
        let got: Vec<(&str, &str)> = got
            .iter()
            .map(|m| (m.event.as_str(), m.data.as_str()))
            .collect();
        assert_eq!(got, want, "pieces: {pieces:?}");
    }
    // The parser decodes what the node's own events look like.
    let post = StreamEvent::Post(post("Q8ZK7C2M", "2025-03-14T22:31:00Z", "hi"));
    let frame = format!("event: {}\ndata: {}\n\n", post.name(), post.data());
    let messages = SseParser::default().push(frame.as_bytes()).unwrap();
    assert_eq!(messages.len(), 1);
    let SseMessage { event, data } = &messages[0];
    assert_eq!(StreamEvent::from_sse(event, data).unwrap(), post);
}

#[test]
fn a_runaway_line_is_refused() {
    let mut parser = SseParser::default();
    let chunk = vec![b'x'; 1024 * 1024];
    let mut result = Ok(Vec::new());
    for _ in 0..9 {
        result = parser.push(&chunk);
        if result.is_err() {
            break;
        }
    }
    let err = result.unwrap_err();
    assert!(matches!(err, Error::Internal(_)), "{err}");
}

/// Reads the next event of a stream, within 5 s.
async fn next_event<S>(events: &mut S) -> Result<StreamEvent, Error>
where
    S: futures_util::Stream<Item = Result<StreamEvent, Error>> + Unpin,
{
    use futures_util::StreamExt;
    tokio::time::timeout(Duration::from_secs(5), events.next())
        .await
        .expect("an event within 5 s")
        .expect("the stream is still open")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_live_stream_delivers_what_is_stored_after_it_opens() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    let client = client(&node);
    post_as(&node, "pc-configs", "beacon", "before the stream").await;

    // `Ok` means connected: whatever is stored from here on arrives.
    let mut all = client.open_stream(None, false).await.unwrap();
    post_as(&node, "pc-configs", "beacon", "one").await;
    // Presence and channel events come too (the TUI shows them); posts first
    // here because the order is the order they were stored in.
    let mut seen_post = None;
    for _ in 0..4 {
        if let StreamEvent::Post(p) = next_event(&mut all).await.unwrap() {
            seen_post = Some(p);
            break;
        }
    }
    let seen_post = seen_post.expect("the post");
    assert_eq!(seen_post.body, "one");
    assert_eq!(seen_post.channel, "beacon");
    assert!(seen_post.arrival > 0);

    // A feed for one handle with `relevant` only carries what matters to it.
    let mut mine = client.open_stream(Some("pc-vllm"), true).await.unwrap();
    post_as(&node, "pc-configs", "beacon", "chatter").await;
    post_as(&node, "pc-configs", "beacon", "@pc-vllm ping").await;
    loop {
        if let StreamEvent::Post(p) = next_event(&mut mine).await.unwrap() {
            assert_eq!(p.body, "@pc-vllm ping");
            break;
        }
    }

    // The same, as a stream that opens itself when it is first polled (here,
    // by a task that hands its items on).
    let before = listeners(&node);
    let mut lazy = client.stream(Some("pc-vllm"), true);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        use futures_util::StreamExt;
        while let Some(item) = lazy.next().await {
            if tx.send(item).is_err() {
                break;
            }
        }
    });
    until_listening(&node, before).await;
    post_as(&node, "mac-juicebox", "beacon", "@pc-vllm again").await;
    let item = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("an event within 5 s")
        .expect("the stream is open");
    match item.unwrap() {
        StreamEvent::Post(p) => assert_eq!(p.body, "@pc-vllm again"),
        other => panic!("a relevant stream carries posts only: {other:?}"),
    }

    // A stream does not outlive its node: it ends with an error, then nothing.
    drop(client);
    let _gone = node.stop().await;
    use futures_util::StreamExt;
    loop {
        match tokio::time::timeout(Duration::from_secs(5), all.next())
            .await
            .expect("the stream ends when the node stops")
        {
            Some(Ok(_)) => continue,
            Some(Err(e)) => {
                assert!(matches!(e, Error::Interrupted(_)), "{e}");
                assert_eq!(e.exit_code(), 3);
                break;
            }
            None => panic!("a stream ends with an error first"),
        }
    }
    assert!(all.next().await.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn opening_a_stream_says_why_it_cannot() {
    use futures_util::StreamExt;
    let node = spawn_node("pc", &[]).await;
    let wrong = Client::new(&node.url, Some("wrong")).unwrap();
    assert!(matches!(
        wrong.open_stream(None, false).await,
        Err(Error::Auth)
    ));
    // `relevant` without a handle is the node's usage error.
    let err = client(&node).open_stream(None, true).await.err().unwrap();
    assert!(
        matches!(&err, Error::Usage(m) if m.contains("handle")),
        "{err}"
    );
    // Nothing listening: the lazy form's only item is the error.
    let nowhere = format!("http://127.0.0.1:{}", refused_port());
    let nobody = Client::new(&nowhere, Some(SECRET)).unwrap();
    let err = nobody.open_stream(None, false).await.err().unwrap();
    assert!(matches!(err, Error::Unreachable(_)), "{err}");
    let mut lazy = nobody.stream(None, false);
    assert!(matches!(
        lazy.next().await,
        Some(Err(Error::Unreachable(_)))
    ));
    assert!(lazy.next().await.is_none());
}

/// A node that closes the stream, or just breaks it, ends the feed with an
/// error the caller can reconnect on.
#[tokio::test(flavor = "multi_thread")]
async fn a_stream_the_node_closes_ends_with_an_interrupted_error() {
    use futures_util::StreamExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = vec![0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let head =
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
            let _ = socket
                .write_all(
                    format!("{head}: hello\n\nevent: peers\ndata: []\n\nevent: peers\ndata: [")
                        .as_bytes(),
                )
                .await;
            // ...and hang up in the middle of an event.
        }
    });
    let client = Client::new(&url, Some(SECRET)).unwrap();
    let mut events = client.open_stream(None, false).await.unwrap();
    assert_eq!(
        next_event(&mut events).await.unwrap(),
        StreamEvent::Peers(vec![])
    );
    let err = next_event(&mut events).await.unwrap_err();
    assert!(
        matches!(&err, Error::Interrupted(m) if m.contains("closed the stream")),
        "{err}"
    );
    assert!(events.next().await.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn client_wait_looks_first_and_gives_up_quietly() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    let client = client(&node);
    client
        .set_joins("pc-vllm", &["beacon".to_string()])
        .await
        .unwrap();

    // Nothing yet: `None`, after about as long as it was told to wait. Part
    // of a second is rounded up.
    let started = std::time::Instant::now();
    assert_eq!(
        client
            .wait("pc-vllm", Duration::from_millis(1))
            .await
            .unwrap(),
        None
    );
    assert!(started.elapsed() >= Duration::from_millis(900));
    assert_eq!(client.wait("pc-vllm", Duration::ZERO).await.unwrap(), None);

    // Something there: at once, and the same again (the cursor does not move).
    post_as(&node, "pc-configs", "beacon", "waiting for you").await;
    for _ in 0..2 {
        let started = std::time::Instant::now();
        let page = client
            .wait("pc-vllm", Duration::from_secs(30))
            .await
            .unwrap()
            .expect("the post");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(page.v, 1);
        assert_eq!(page.posts[0].body, "waiting for you");
        assert_eq!(page.remaining, Some(0));
    }

    // Errors are the usual ones.
    let err = client.wait("no spaces", Duration::ZERO).await.unwrap_err();
    assert!(matches!(err, Error::Usage(_)), "{err}");
    let wrong = Client::new(&node.url, Some("wrong")).unwrap();
    assert!(matches!(
        wrong.wait("pc-vllm", Duration::ZERO).await,
        Err(Error::Auth)
    ));

    // The node's newest arrival number moves with what is stored.
    let head = client.arrival_head().await.unwrap();
    post_as(&node, "pc-configs", "beacon", "one more").await;
    assert!(client.arrival_head().await.unwrap() > head);
}

/// A node that is reached and then does not answer is not a missing node.
#[tokio::test(flavor = "multi_thread")]
async fn a_silent_node_is_interrupted_not_unreachable() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let held = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let keep = held.clone();
    tokio::spawn(async move {
        // Accept, say nothing, and keep the sockets open.
        while let Ok((socket, _)) = listener.accept().await {
            keep.lock().unwrap().push(socket);
        }
    });
    let client = Client::new(&url, Some(SECRET))
        .unwrap()
        .with_request_timeout(Duration::from_millis(300));
    let err = client.health().await.unwrap_err();
    assert!(
        matches!(&err, Error::Interrupted(m) if m.contains("did not answer within 0.3s")),
        "{err}"
    );
    assert_eq!(err.exit_code(), 3);
    // A post that times out may have been stored, and says so.
    let req = NewPostRequest {
        body: "hi".into(),
        tags: vec![],
        author: "pc-configs".into(),
        author_kind: AuthorKind::Agent,
        client: None,
        create: false,
    };
    let err = client.post("beacon", &req).await.unwrap_err();
    assert!(
        matches!(&err, Error::Interrupted(m) if m.contains("may have been stored")),
        "{err}"
    );
    // So do the open of a stream and the start of a download.
    let err = client.open_stream(None, false).await.err().unwrap();
    assert!(matches!(err, Error::Interrupted(_)), "{err}");
    let err = client.file(&"ab".repeat(32)).await.err().unwrap();
    assert!(matches!(err, Error::Interrupted(_)), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn client_uploads_a_file_and_downloads_it_again() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    let client = client(&node);
    let bytes = binary_blob(200_000);
    let req = NewPostRequest {
        body: "the log".into(),
        tags: vec!["done".into()],
        author: "pc-configs".into(),
        author_kind: AuthorKind::Agent,
        client: Some("claude-code".into()),
        create: false,
    };
    // With the length known and without it; the node orders the parts itself
    // (it refuses a file that comes before its post), so success is the proof
    // that `meta` goes first.
    for len in [Some(bytes.len() as u64), None] {
        let created = client
            .upload(
                "beacon",
                &req,
                "run.log",
                std::io::Cursor::new(bytes.clone()),
                len,
            )
            .await
            .unwrap();
        let attachment = created.post.attachment.as_ref().unwrap();
        assert_eq!(attachment.name, "run.log");
        assert_eq!(attachment.size, bytes.len() as u64);
        assert_eq!(attachment.sha256, sha256_hex(&bytes));

        let mut download = client.file(&attachment.sha256).await.unwrap();
        assert_eq!(download.len, Some(bytes.len() as u64));
        let mut got = Vec::new();
        while let Some(chunk) = download.chunk().await.unwrap() {
            got.extend_from_slice(&chunk);
        }
        assert_eq!(got, bytes);
    }
    // A file the node does not have is NotFound; one that is no hash is usage.
    let err = client.file(&"ab".repeat(32)).await.err().unwrap();
    assert!(matches!(err, Error::NotFound(_)), "{err}");
    let err = client.file("not-a-hash").await.err().unwrap();
    assert!(matches!(err, Error::Usage(_)), "{err}");
    // An upload to a channel that is not there is refused as such.
    let err = client
        .upload(
            "nosuch",
            &req,
            "a.txt",
            std::io::Cursor::new(vec![1u8]),
            Some(1),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err}");
}

// ----------------------------------------------- the new formatters

fn presence(
    handle: &str,
    client: &str,
    node: &str,
    ago_ms: i64,
    status: Option<&str>,
) -> PresenceJson {
    let at = ms("2025-03-16T16:00:00Z") - ago_ms;
    PresenceJson {
        handle: handle.into(),
        kind: if client == "tui" {
            AuthorKind::Human
        } else {
            AuthorKind::Agent
        },
        client: client.into(),
        node: node.into(),
        status: status.map(String::from),
        last_seen: iso8601(at),
        last_seen_ms: at,
        active: ago_ms <= 5 * 60 * 1000,
    }
}

#[test]
fn who_lines_follow_the_template_and_line_up() {
    let now = clock().now_ms;
    assert_eq!(human::who_text(&[], now), "nobody has been seen lately");
    let one = [presence(
        "pc-configs",
        "claude-code",
        "pc",
        0,
        Some("scaffolding beacon crate"),
    )];
    assert_eq!(
        human::who_text(&one, now),
        format!(
            "● {:<16} {} · {:<6} {:>4}   {}",
            "pc-configs", "claude-code", "pc", "now", "scaffolding beacon crate"
        )
    );
    // The mockup's board: columns line up, idle is a ring, no status no tail.
    let board = [
        presence(
            "pc-configs",
            "claude-code",
            "pc",
            20_000,
            Some("scaffolding beacon crate"),
        ),
        presence(
            "mac-juicebox",
            "codex",
            "mac",
            60_000,
            Some("arm64 build + e2e"),
        ),
        presence(
            "pc-vllm",
            "claude-code",
            "pc",
            14 * 60_000,
            Some("waiting on VRAM"),
        ),
        presence("Dang", "tui", "pc", 0, None),
    ];
    // {dot} {handle:<16} {client · node, padded to the widest} {age:>4}   {status}
    let row = |dot: &str, handle: &str, who: &str, age: &str, status: &str| {
        format!("{dot} {handle:<16} {who:<20} {age:>4}   {status}")
            .trim_end()
            .to_string()
    };
    let want = [
        row(
            "●",
            "pc-configs",
            "claude-code · pc",
            "now",
            "scaffolding beacon crate",
        ),
        row(
            "●",
            "mac-juicebox",
            "codex · mac",
            "1m",
            "arm64 build + e2e",
        ),
        row("○", "pc-vllm", "claude-code · pc", "14m", "waiting on VRAM"),
        row("●", "Dang", "tui · pc", "now", ""),
    ]
    .join("\n");
    assert_eq!(human::who_text(&board, now), want);
    // A long handle widens its column for everyone, by display width.
    let wide = [
        presence("日本語のエージェント名前です", "codex", "mac", 0, None),
        presence("pc", "codex", "mac", 3 * 3_600_000, Some("idle")),
    ];
    let text = human::who_text(&wide, now);
    let lines: Vec<&str> = text.lines().collect();
    let column = |line: &str| human::display_width(&line[..line.find("codex").unwrap()]);
    assert_eq!(column(lines[0]), column(lines[1]), "{text}");
    assert!(
        lines[1].starts_with('○') && lines[1].contains("   3h   idle"),
        "{text}"
    );
    // What other machines call themselves cannot move the cursor.
    let sneaky = [presence("a\u{1b}[2Jb", "co\u{7}dex", "pc", 0, Some("x\ny"))];
    let text = human::who_text(&sneaky, now);
    assert!(
        !text.contains('\u{1b}') && !text.contains('\u{7}'),
        "{text:?}"
    );
    assert_eq!(text.lines().count(), 1, "{text:?}");
}

#[test]
fn watch_lines_are_one_short_line_each() {
    let c = clock();
    let p = post(
        "Q8ZK7C2M",
        "2025-03-14T22:31:00Z",
        "arm64 builds in 2m14s\nsecond line",
    );
    assert_eq!(
        human::watch_line(&p, &c),
        "6:31 pm #beacon mac-juicebox: arm64 builds in 2m14s"
    );
    // Leading blank lines and indentation do not make the line empty; tabs
    // are spaces; control characters are defused.
    let p = post(
        "Q8ZK7C2M",
        "2025-03-14T22:31:00Z",
        "\n\n  \tindented\u{1b}[2J \nnext",
    );
    assert_eq!(
        human::watch_line(&p, &c),
        "6:31 pm #beacon mac-juicebox: indented\u{fffd}[2J"
    );
    // Cut at 200 characters, the 200th being the ellipsis; never mid-grapheme.
    let long = post("Q8ZK7C2M", "2025-03-14T22:31:00Z", &"word ".repeat(100));
    let line = human::watch_line(&long, &c);
    let text = line.strip_prefix("6:31 pm #beacon mac-juicebox: ").unwrap();
    assert_eq!(text.chars().count(), 200, "{text}");
    assert!(text.ends_with("word…"), "{text}");
    let exactly = post("Q8ZK7C2M", "2025-03-14T22:31:00Z", &"x".repeat(200));
    assert!(human::watch_line(&exactly, &c).ends_with(&"x".repeat(200)));
    let over = post("Q8ZK7C2M", "2025-03-14T22:31:00Z", &"x".repeat(201));
    assert!(human::watch_line(&over, &c).ends_with(&format!("{}…", "x".repeat(199))));
    // Families, flags and combining marks count once and are not split.
    let emoji = "👨‍👩‍👧‍👦".repeat(250);
    let line = human::watch_line(&post("Q8ZK7C2M", "2025-03-14T22:31:00Z", &emoji), &c);
    let text = line.strip_prefix("6:31 pm #beacon mac-juicebox: ").unwrap();
    assert_eq!(text, format!("{}…", "👨‍👩‍👧‍👦".repeat(199)));
    // A post that is only a file shows the file; Dang's shows his channel.
    let mut file = post("Q8ZK7C2M", "2025-03-14T22:31:00Z", "");
    file.attachment = Some(AttachmentJson {
        name: "../report.txt".into(),
        size: 12_288,
        sha256: "ab".repeat(32),
        available: true,
    });
    assert_eq!(
        human::watch_line(&file, &c),
        "6:31 pm #beacon mac-juicebox: attachment: report.txt (12 KB)"
    );
    let d = in_channel(
        dang("R9XW5VHB", "2025-03-15T14:02:00Z", "Tag it."),
        "general",
    );
    assert_eq!(human::watch_line(&d, &c), "10:02 am #general Dang: Tag it.");
}

#[test]
fn posted_lines_name_an_attachment() {
    let mut created = PostCreated {
        v: 1,
        post: post("Q8ZK7C2M", "2025-03-14T22:31:00Z", ""),
        delivery: vec![Delivery {
            node: "mac".into(),
            delivered: true,
        }],
    };
    created.post.attachment = Some(AttachmentJson {
        name: "dir/test-report.txt".into(),
        size: 12_288,
        sha256: "ab".repeat(32),
        available: true,
    });
    assert_eq!(
        human::posted_text(&created),
        "posted Q8ZK7C2M to #beacon · attached test-report.txt (12 KB) · delivered to mac"
    );
}

#[test]
fn file_names_from_peers_are_reduced_to_a_name() {
    for (given, safe) in [
        ("report.txt", "report.txt"),
        ("../../etc/passwd", "passwd"),
        (r"..\..\boot.ini", "boot.ini"),
        ("a/b\\c", "c"),
        ("/abs/olute", "olute"),
        ("trailing/", "attachment"),
        ("trailing\\", "attachment"),
        ("", "attachment"),
        (".", "attachment"),
        ("..", "attachment"),
        ("dir/..", "attachment"),
        ("  spaced.txt  ", "spaced.txt"),
        ("...", "..."),
        ("bell\u{7}.txt", "bell_.txt"),
        ("日本語.log", "日本語.log"),
    ] {
        assert_eq!(human::safe_file_name(given), safe, "{given:?}");
    }
}

#[test]
fn join_and_leave_lines() {
    assert_eq!(
        human::joined_text("beacon"),
        "joined #beacon · wait and watch will wake you for posts here"
    );
    assert_eq!(
        human::left_text("beacon"),
        "left #beacon · mentions and announcements still wake you"
    );
}

// --------------------------------------------------- fix round 1: new tests

/// A port nothing listens on and no test node can take: it is privileged, and
/// nodes bind port 0 (a free port picked from the high range).
fn refused_port() -> u16 {
    1
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_port_is_refused() {
    let err = tokio::net::TcpStream::connect(("127.0.0.1", refused_port()))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused);
}

/// All the files in `dir`, hidden ones included, sorted.
fn names_in(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

const DOESNT_MATCH: &str = "attachment doesn't match what its post describes (size or hash)";

/// A download is checked before it is put in place. Whatever is at the target
/// when the check fails is still there afterwards, and nothing is left over.
#[tokio::test(flavor = "multi_thread")]
async fn a_download_that_does_not_match_its_post_leaves_the_target_alone() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    let bytes = binary_blob(70_000);
    let created = beacon(
        &node,
        &[
            "attach",
            "beacon",
            "-",
            "--name",
            "data.bin",
            "--as",
            "pc-configs",
            "--json",
        ],
    )
    .stdin_bytes(bytes.clone())
    .ok()
    .await
    .json();
    let sha = created["post"]["attachment"]["sha256"].as_str().unwrap();
    let good = created["post"]["short_id"].as_str().unwrap().to_string();

    // A peer's post that says the same file is 1 byte long.
    let mut event = remote_post(10, "PEER0010", "beacon", "from afar").event();
    if let EventKind::PostCreated(post) = &mut event.kind {
        post.attachment = Some(Attachment {
            sha256: sha.to_string(),
            name: "mine.txt".to_string(),
            size: 1,
        });
    }
    seed(&node, vec![event]).await;

    let dir = tempfile::tempdir().unwrap();
    let dl = dir.path().join("dl");
    std::fs::create_dir(&dl).unwrap();
    let mine = dl.join("mine.txt");
    std::fs::write(&mine, "precious").unwrap();

    // To a path that exists: untouched, no temp file left behind.
    let out = beacon(&node, &["get", "PEER0010", "-o", mine.to_str().unwrap()])
        .run()
        .await;
    assert_eq!(out.code, 1, "{out:?}");
    assert_eq!(out.stdout, "");
    assert_eq!(out.stderr.trim_end(), format!("beacon: {DOESNT_MATCH}"));
    assert_eq!(std::fs::read_to_string(&mine).unwrap(), "precious");
    assert_eq!(names_in(&dl), ["mine.txt"]);

    // Into a directory, where the attachment's own name is that file.
    let out = beacon(&node, &["get", "PEER0010", "-o", dl.to_str().unwrap()])
        .run()
        .await;
    assert_eq!(out.code, 1, "{out:?}");
    assert_eq!(std::fs::read_to_string(&mine).unwrap(), "precious");
    assert_eq!(names_in(&dl), ["mine.txt"]);

    // To a path that does not exist: it still does not.
    let fresh = dl.join("fresh.txt");
    let out = beacon(&node, &["get", "PEER0010", "-o", fresh.to_str().unwrap()])
        .run()
        .await;
    assert_eq!(out.code, 1, "{out:?}");
    assert!(!fresh.exists());
    assert_eq!(names_in(&dl), ["mine.txt"]);

    // To stdout the bytes have gone by, so all there is to do is say so. This
    // is not something to try again: the post will never match.
    let out = beacon(&node, &["get", "PEER0010"]).run().await;
    assert_eq!(out.code, 1, "{out:?}");
    assert_eq!(out.stderr.trim_end(), format!("beacon: {DOESNT_MATCH}"));
    assert!(!out.stderr.contains("try again"), "{}", out.stderr);

    // The hash is checked as well as the size: a blob that changed on disk.
    let blob = node.dir.path().join("files").join(sha);
    let mut rotten = std::fs::read(&blob).unwrap();
    rotten[1234] ^= 0xff;
    std::fs::write(&blob, rotten).unwrap();
    let out = beacon(&node, &["get", &good, "-o", mine.to_str().unwrap()])
        .run()
        .await;
    assert_eq!(out.code, 1, "{out:?}");
    assert_eq!(out.stderr.trim_end(), format!("beacon: {DOESNT_MATCH}"));
    assert_eq!(std::fs::read_to_string(&mine).unwrap(), "precious");
    assert_eq!(names_in(&dl), ["mine.txt"]);
}

/// ...and a download that does match replaces what was there, whole.
#[tokio::test(flavor = "multi_thread")]
async fn a_good_download_replaces_the_target_without_a_trace() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    let created = beacon(
        &node,
        &[
            "attach",
            "beacon",
            "-",
            "--name",
            "a.txt",
            "--as",
            "pc-configs",
            "--json",
        ],
    )
    .stdin("new contents")
    .ok()
    .await
    .json();
    let id = created["post"]["short_id"].as_str().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("a.txt");
    std::fs::write(&target, "old contents, and longer").unwrap();
    beacon(&node, &["get", id, "-o", target.to_str().unwrap()])
        .ok()
        .await;
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "new contents");
    assert_eq!(names_in(dir.path()), ["a.txt"]);
}

/// `attach -` reads a stdin that may never end. When the command is over,
/// the process is too, whatever stdin is still doing.
#[tokio::test(flavor = "multi_thread")]
async fn attach_exits_when_it_has_failed_even_if_stdin_stays_open() {
    use std::process::Stdio;
    let node = spawn_node("pc", &[]).await;
    let run = beacon(
        &node,
        &[
            "attach",
            "nosuch",
            "-",
            "--name",
            "x.txt",
            "--as",
            "pc-configs",
        ],
    );
    let mut cmd = tokio::process::Command::from(run.command());
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().expect("start beacon");
    // The producer is slow: the write end stays open and nothing comes.
    let _slow_producer = child.stdin.take().unwrap();
    let started = std::time::Instant::now();
    let status = tokio::time::timeout(Duration::from_secs(4), child.wait())
        .await
        .expect("beacon exits at once, not when stdin does")
        .unwrap();
    assert_eq!(status.code(), Some(2));
    assert!(started.elapsed() < Duration::from_secs(4));
}

/// A TCP stand-in for a node, in front of `upstream`. It forwards everything,
/// counts the heartbeats that pass, and can be told to answer requests with a
/// 500 instead.
struct Proxy {
    url: String,
    heartbeats: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    refuse: std::sync::Arc<std::sync::Mutex<Option<(String, usize)>>>,
}

impl Proxy {
    async fn start(upstream: &str) -> Proxy {
        use std::sync::atomic::AtomicUsize;
        use std::sync::{Arc, Mutex};
        let upstream = upstream.trim_start_matches("http://").to_string();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let heartbeats = Arc::new(AtomicUsize::new(0));
        let refuse = Arc::new(Mutex::new(None));
        let (counted, refusing) = (heartbeats.clone(), refuse.clone());
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                tokio::spawn(Proxy::connection(
                    client,
                    upstream.clone(),
                    counted.clone(),
                    refusing.clone(),
                ));
            }
        });
        Proxy {
            url,
            heartbeats,
            refuse,
        }
    }

    /// Answers the next `times` requests that start with `request_line` (for
    /// example `GET /api/events`) with a 500, and does not forward them.
    fn answer_with_500(&self, request_line: &str, times: usize) {
        *self.refuse.lock().unwrap() = Some((request_line.to_string(), times));
    }

    fn heartbeats(&self) -> usize {
        self.heartbeats.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn connection(
        client: tokio::net::TcpStream,
        upstream: String,
        heartbeats: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        refuse: std::sync::Arc<std::sync::Mutex<Option<(String, usize)>>>,
    ) {
        use std::sync::atomic::Ordering;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let Ok(server) = tokio::net::TcpStream::connect(&upstream).await else {
            return;
        };
        let (mut from_client, to_client) = client.into_split();
        let (mut from_server, mut to_server) = server.into_split();
        let to_client = std::sync::Arc::new(tokio::sync::Mutex::new(to_client));
        let back = {
            let to_client = to_client.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16 * 1024];
                loop {
                    match from_server.read(&mut buf).await {
                        Ok(n) if n > 0 => {
                            if to_client.lock().await.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
                let _ = to_client.lock().await.shutdown().await;
            })
        };
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = match from_client.read(&mut buf).await {
                Ok(n) if n > 0 => n,
                _ => break,
            };
            let head = String::from_utf8_lossy(&buf[..n.min(256)]).into_owned();
            if head.starts_with("POST /api/who ") {
                heartbeats.fetch_add(1, Ordering::SeqCst);
            }
            let refused = match refuse.lock().unwrap().as_mut() {
                Some((prefix, times)) if *times > 0 && head.starts_with(prefix.as_str()) => {
                    *times -= 1;
                    true
                }
                _ => false,
            };
            if refused {
                let body = r#"{"v":1,"error":"the store is on fire","code":"internal"}"#;
                let reply = format!(
                    "HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\n\r\n{body}",
                    body.len()
                );
                if to_client
                    .lock()
                    .await
                    .write_all(reply.as_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
                continue;
            }
            if to_server.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
        back.abort();
    }
}

/// `watch` runs for hours. It says it is here as `wait` does, so the handle
/// does not go idle in `who` and then disappear while someone is listening.
#[tokio::test(flavor = "multi_thread")]
async fn watch_tells_the_node_it_is_still_here() {
    let node = spawn_node("pc", &[]).await;
    let proxy = Proxy::start(&node.url).await;
    let before = listeners(&node);
    let watch = beacon(&node, &["watch", "--as", "pc-vllm"])
        .server(&proxy.url)
        .env("BEACON_HEARTBEAT_SECS", "1")
        .spawn();
    until_listening(&node, before).await;
    for _ in 0..80 {
        if proxy.heartbeats() >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        proxy.heartbeats() >= 3,
        "watch sent {} heartbeats in 8 s; stderr: {}",
        proxy.heartbeats(),
        watch.stderr()
    );
    watch.kill().await;
}

/// The node answering 5xx is the node having a bad moment, which `watch`
/// waits out like any other outage.
#[tokio::test(flavor = "multi_thread")]
async fn watch_waits_out_a_node_that_answers_with_a_500() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    join_channel(&node, "pc-vllm", "beacon").await;
    let proxy = Proxy::start(&node.url).await;
    proxy.answer_with_500("GET /api/events", 2);

    let before = listeners(&node);
    let mut watch = beacon(&node, &["watch", "--as", "pc-vllm"])
        .server(&proxy.url)
        .spawn();
    // Two refusals, pauses of 1 s and 2 s, then it is connected.
    for _ in 0..100 {
        if listeners(&node) > before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        listeners(&node) > before,
        "watch gave up; stderr: {}",
        watch.stderr()
    );
    assert!(
        watch.stderr().contains("beacon: reconnecting…"),
        "{}",
        watch.stderr()
    );
    post_as(&node, "pc-configs", "beacon", "after the bad moment").await;
    assert!(watch
        .line()
        .await
        .ends_with("beacon pc-configs: after the bad moment"));
    watch.kill().await;
}

/// A 5xx is still an error (exit 1) for a command that is not waiting
/// anything out, and the client can tell it from other internal errors.
#[tokio::test(flavor = "multi_thread")]
async fn a_500_is_an_internal_error_the_client_can_recognize() {
    let node = spawn_node("pc", &[]).await;
    let proxy = Proxy::start(&node.url).await;
    proxy.answer_with_500("GET /api/events", 1);
    let client = Client::new(&proxy.url, Some(SECRET)).unwrap();
    let err = client.arrival_head().await.unwrap_err();
    assert!(
        matches!(&err, Error::Internal(_)) && err.is_server_fault(),
        "{err:?}"
    );
    assert_eq!(err.exit_code(), 1);
    assert!(err.to_string().contains("the store is on fire"), "{err}");
    // Other internal errors, and other kinds of error, are not.
    assert!(!Error::Internal(anyhow::anyhow!("boom")).is_server_fault());
    assert!(!Error::Usage("x".into()).is_server_fault());
    // The route works again afterwards.
    assert!(client.arrival_head().await.unwrap() >= 0);
    let out = beacon(&node, &["channels"]).server(&proxy.url).ok().await;
    assert!(out.stdout.contains("general"), "{}", out.stdout);
}
