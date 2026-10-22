//! The agent CLI, end to end: the real `beacon` binary against a real node
//! on a loopback port, with the time zone pinned to `America/New_York`.

mod common;

use std::time::Duration;

use assert_cmd::Command;
use beacon::api::{
    AttachmentJson, ChannelJson, ChannelsJson, Delivery, NewPostRequest, PeerJson, PeersJson,
    PostCreated, PostJson, PostsResponse, StatusJson, WhoRequest,
};
use beacon::cli::human::{self, Clock};
use beacon::client::Client;
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

    /// Runs the binary and waits for it. The node keeps serving meanwhile, so
    /// the blocking wait happens off the async workers.
    async fn run(self) -> Out {
        tokio::task::spawn_blocking(move || {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_beacon"));
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
                .args(&self.args)
                .write_stdin(self.stdin)
                .timeout(Duration::from_secs(30));
            for (key, value) in &self.env {
                cmd.env(key, value);
            }
            let output = cmd.output().expect("run beacon");
            Out {
                code: output.status.code().expect("exit code"),
                stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
                stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
            }
        })
        .await
        .unwrap()
    }

    /// Runs it and insists on exit 0.
    async fn ok(self) -> Out {
        let args = self.args.clone();
        let out = self.run().await;
        assert_eq!(out.code, 0, "beacon {args:?}: {out:?}");
        out
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

/// A free loopback port nothing listens on.
fn closed_port() -> u16 {
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
    let url = format!("http://127.0.0.1:{}", closed_port());
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
async fn peers_lists_what_the_node_reports() {
    // Nothing listens on port 1 (binding it needs root).
    let away = "http://127.0.0.1:1/";
    let node = spawn_node_with("pc", &[away], |cfg| {
        cfg.sync_base_backoff = Duration::from_millis(100);
    })
    .await;
    // Wait until sync has tried the peer once, so the answer is settled.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let peers = loop {
        assert!(
            std::time::Instant::now() < deadline,
            "the peer was never tried"
        );
        let resp = node.get("/api/peers").send().await.unwrap();
        let body: PeersJson = resp.json().await.unwrap();
        if !body.peers.is_empty() && !body.peers[0].reachable {
            break body;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let json = beacon(&node, &["peers", "--json"]).ok().await.json();
    assert_eq!(json, serde_json::to_value(&peers).unwrap());
    let text = beacon(&node, &["peers"]).ok().await;
    let url = away.trim_end_matches('/');
    // `general` is the one event the peer has never been sent.
    assert_eq!(text.text(), format!("○ ?  {url}  unreachable · 1 queued"));
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
    let port = closed_port();
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
    let nowhere = format!("http://127.0.0.1:{}", closed_port());
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
        matches!(&err, Error::Unreachable(m) if m.contains("may have been")),
        "{err}"
    );
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
        ("a-b_c.1/x", "a-b_c.1/x"),
    ] {
        p.attachment.as_mut().unwrap().name = name.into();
        let lines = human::post_lines(&p, &c);
        let want =
            format!("          attachment: {name} (12 KB)  →  ./beacon get Q8ZK7C2M -o {quoted}");
        assert_eq!(lines[2], want);
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
