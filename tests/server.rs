//! The authenticated HTTP API over the store, tested against real nodes on
//! loopback ports, plus the config that starts them.

mod common;

use std::collections::HashMap;

use beacon::api::{ChannelsJson, PostCreated, PostJson, PostsResponse, WhoJson};
use beacon::config::Config;
use beacon::error::Error;
use beacon::model::{Event, EventKind, VersionVector};
use beacon::server::secret_matches;
use common::*;
use reqwest::{Method, Response, StatusCode};
use serde_json::{json, Value};
use tokio::sync::broadcast::error::TryRecvError;

/// The body of a response that must have status `status`.
async fn expect(resp: Response, status: StatusCode) -> Value {
    let got = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(got, status, "body: {text}");
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

async fn ok(resp: Response) -> Value {
    expect(resp, StatusCode::OK).await
}

/// Asserts the response is an `ApiError` with this status and code.
async fn expect_error(resp: Response, status: StatusCode, code: &str) -> String {
    let body = expect(resp, status).await;
    assert_eq!(body["v"], 1, "{body}");
    assert_eq!(body["code"], code, "{body}");
    body["error"].as_str().expect("error text").to_string()
}

async fn new_channel(node: &TestNode, name: &str) {
    let resp = node
        .post("/api/channels")
        .json(&json!({ "name": name, "topic": null }))
        .send()
        .await
        .unwrap();
    ok(resp).await;
}

/// Posts `body` as an agent; returns the `PostCreated` JSON.
async fn agent_post(
    node: &TestNode,
    channel: &str,
    author: &str,
    body: &str,
    tags: &[&str],
) -> Value {
    send_post(node, channel, author, "agent", body, tags).await
}

/// Posts `body` as Dang; returns the `PostCreated` JSON.
async fn human_post(node: &TestNode, channel: &str, body: &str) -> Value {
    send_post(node, channel, "Dang", "human", body, &[]).await
}

async fn send_post(
    node: &TestNode,
    channel: &str,
    author: &str,
    kind: &str,
    body: &str,
    tags: &[&str],
) -> Value {
    let resp = node
        .post(&format!("/api/channels/{channel}/posts"))
        .json(&json!({
            "body": body, "tags": tags, "author": author,
            "author_kind": kind, "client": "claude-code", "create": false
        }))
        .send()
        .await
        .unwrap();
    ok(resp).await
}

async fn channel_bodies(node: &TestNode, channel: &str) -> Vec<String> {
    let resp = node
        .get(&format!("/api/channels/{channel}/posts"))
        .send()
        .await
        .unwrap();
    let posts: PostsResponse = serde_json::from_value(ok(resp).await).unwrap();
    posts.posts.into_iter().map(|p| p.body).collect()
}

fn post_id(created: &Value) -> String {
    created["post"]["id"].as_str().unwrap().to_string()
}

// ---------------------------------------------------------------- auth

#[tokio::test]
async fn rejects_missing_or_wrong_secret() {
    let node = spawn_node("pc", &[]).await;
    let plain = reqwest::Client::new();
    let url = |path: &str| format!("{}{path}", node.url);

    let resp = plain.get(url("/api/health")).send().await.unwrap();
    assert_eq!(resp.status(), 401);
    assert_eq!(resp.headers()["www-authenticate"], "Bearer");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["v"], 1);
    assert_eq!(body["code"], "auth");
    assert!(body["error"].as_str().unwrap().contains("BEACON_SECRET"));

    let resp = plain
        .get(url("/api/health"))
        .bearer_auth("wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // The right secret works.
    let resp = node.get("/api/health").send().await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn every_route_requires_the_secret() {
    let node = spawn_node("pc", &[]).await;
    let plain = reqwest::Client::new();
    let routes = [
        (Method::GET, "/api/health"),
        (Method::GET, "/api/status"),
        (Method::GET, "/api/channels"),
        (Method::POST, "/api/channels"),
        (Method::DELETE, "/api/channels/general"),
        (Method::GET, "/api/channels/general/posts"),
        (Method::POST, "/api/channels/general/posts"),
        (Method::GET, "/api/posts"),
        (Method::GET, "/api/posts/01JDB7K4TR"),
        (Method::DELETE, "/api/posts/01JDB7K4TR"),
        (Method::GET, "/api/who"),
        (Method::POST, "/api/who"),
        (Method::GET, "/api/joins/pc-configs"),
        (Method::PUT, "/api/joins/pc-configs"),
        (Method::GET, "/api/read-cursors/pc-configs"),
        (Method::PUT, "/api/read-cursors/pc-configs"),
        (Method::GET, "/api/unread/pc-configs"),
        // Paths that do not exist must not reveal that either.
        (Method::GET, "/api/nope"),
        (Method::GET, "/"),
    ];
    // Right secret with a letter added or removed, other schemes, no token.
    let almost = [
        "Bearer test-secre".to_string(),
        "Bearer test-secret!".to_string(),
        "Bearer ".to_string(),
        "Bearer".to_string(),
        "Basic test-secret".to_string(),
        "test-secret".to_string(),
        String::new(),
    ];
    for (method, path) in routes {
        let resp = plain
            .request(method.clone(), format!("{}{path}", node.url))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "{method} {path} without a header");
        for header in &almost {
            let resp = plain
                .request(method.clone(), format!("{}{path}", node.url))
                .header("authorization", header)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 401, "{method} {path} with {header:?}");
        }
    }
    // The scheme name is case-insensitive.
    let resp = plain
        .get(format!("{}/api/health", node.url))
        .header("authorization", format!("bearer {}", node.secret))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[test]
fn secret_comparison() {
    assert!(secret_matches("s3cret", "s3cret"));
    assert!(!secret_matches("s3cret", "s3cre"));
    assert!(!secret_matches("s3cre", "s3cret"));
    assert!(!secret_matches("", "s3cret"));
    assert!(!secret_matches("S3cret", "s3cret"));
    assert!(secret_matches("", ""));
}

#[tokio::test]
async fn unknown_route_is_404_json_after_auth() {
    let node = spawn_node("pc", &[]).await;
    let resp = node.get("/api/nope").send().await.unwrap();
    expect_error(resp, StatusCode::NOT_FOUND, "not_found").await;
    let resp = node.delete("/api/health").send().await.unwrap();
    expect_error(resp, StatusCode::METHOD_NOT_ALLOWED, "usage").await;
}

// ------------------------------------------------------- health, status

#[tokio::test]
async fn health_and_status() {
    let node = spawn_node("pc", &[]).await;
    let health = ok(node.get("/api/health").send().await.unwrap()).await;
    assert_eq!(health, json!({ "v": 1, "node": "pc", "ok": true }));

    let status = ok(node.get("/api/status").send().await.unwrap()).await;
    assert_eq!(status["v"], 1);
    assert_eq!(status["node"], "pc");
    assert!(!status["version"].as_str().unwrap().is_empty());
    assert_eq!(status["peers"], json!([]));
    // `general` was created by this node's own origin: `pc~<db id>`.
    let have = status["have"].as_object().unwrap();
    assert_eq!(have.len(), 1, "{status}");
    let (origin, seq) = have.iter().next().unwrap();
    assert!(origin.starts_with("pc~"), "{origin}");
    assert_eq!(*seq, 1);
}

// -------------------------------------------------------------- channels

#[tokio::test]
async fn fresh_node_lists_general() {
    let node = spawn_node("pc", &[]).await;
    let body = ok(node.get("/api/channels").send().await.unwrap()).await;
    assert_eq!(body["v"], 1);
    let channels: ChannelsJson = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(channels.channels.len(), 1);
    assert_eq!(channels.channels[0].name, "general");
    assert_eq!(channels.channels[0].unread, None);
    assert_eq!(body["channels"][0]["unread"], Value::Null);
}

#[tokio::test]
async fn create_channel_normalizes_and_is_idempotent() {
    let node = spawn_node("pc", &[]).await;
    let resp = node
        .post("/api/channels")
        .json(&json!({ "name": "#Beacon", "topic": "the board" }))
        .send()
        .await
        .unwrap();
    let body = ok(resp).await;
    assert_eq!(body["v"], 1);
    assert_eq!(body["ok"], true);
    assert_eq!(body["created"], true);
    assert_eq!(body["channel"]["name"], "beacon");
    assert_eq!(body["channel"]["topic"], "the board");

    // Again, same topic or none: nothing new, nothing changed.
    for topic in [json!("the board"), Value::Null] {
        let resp = node
            .post("/api/channels")
            .json(&json!({ "name": "beacon", "topic": topic }))
            .send()
            .await
            .unwrap();
        let body = ok(resp).await;
        assert_eq!(body["created"], false);
        assert_eq!(body["channel"]["topic"], "the board");
    }
    // A different topic changes it.
    let resp = node
        .post("/api/channels")
        .json(&json!({ "name": "beacon", "topic": "new topic" }))
        .send()
        .await
        .unwrap();
    assert_eq!(ok(resp).await["channel"]["topic"], "new topic");

    let channels = ok(node.get("/api/channels").send().await.unwrap()).await;
    let names: Vec<&str> = channels["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["general", "beacon"]);
}

#[tokio::test]
async fn delete_channel_hides_channel() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "builds").await;
    agent_post(&node, "builds", "pc-configs", "ci is red", &[]).await;

    let resp = node.delete("/api/channels/%23Builds").send().await.unwrap();
    let ack = ok(resp).await;
    assert_eq!(ack, json!({ "v": 1, "ok": true, "deleted": "builds" }));

    let channels = ok(node.get("/api/channels").send().await.unwrap()).await;
    let names: Vec<&str> = channels["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["general"]);
    let resp = node.get("/api/channels/builds/posts").send().await.unwrap();
    expect_error(resp, StatusCode::NOT_FOUND, "not_found").await;
    // Its posts are hidden everywhere, not just in the channel.
    let all = ok(node.get("/api/posts").send().await.unwrap()).await;
    assert_eq!(all["posts"], json!([]));

    // Deleting what is gone is a 404; so is deleting what never was.
    let resp = node.delete("/api/channels/builds").send().await.unwrap();
    expect_error(resp, StatusCode::NOT_FOUND, "not_found").await;
    let resp = node.delete("/api/channels/nothing").send().await.unwrap();
    expect_error(resp, StatusCode::NOT_FOUND, "not_found").await;

    // Creating it again brings the posts back.
    new_channel(&node, "builds").await;
    assert_eq!(channel_bodies(&node, "builds").await, ["ci is red"]);
}

// ----------------------------------------------------------------- posts

#[tokio::test]
async fn create_post_read_back() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;

    let resp = node
        .post("/api/channels/beacon/posts")
        .json(&json!({
            "body": "on it @mac-juicebox",
            "tags": ["claim"],
            "author": "pc-configs",
            "author_kind": "agent",
            "client": "claude-code",
            "create": false
        }))
        .send()
        .await
        .unwrap();
    let raw = ok(resp).await;
    let created: PostCreated = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(created.v, 1);
    assert_eq!(created.delivery, vec![]);
    let post = &created.post;
    assert_eq!(post.v, 1);
    assert_eq!(post.mentions, ["mac-juicebox"]);
    assert_eq!(post.tags, ["claim"]);
    assert_eq!(post.channel, "beacon");
    assert_eq!(post.author, "pc-configs");
    assert_eq!(post.node, "pc");
    assert_eq!(post.client.as_deref(), Some("claude-code"));
    assert!(!post.announcement);
    assert_eq!(post.id.len(), 26);
    assert_eq!(post.short_id, &post.id[18..]);
    assert!(post.attachment.is_none());
    assert_eq!(raw["delivery"], json!([]));
    assert_eq!(raw["post"]["author_kind"], "agent");
    assert_eq!(raw["post"]["tags"], json!(["claim"]));
    // ts is ISO-8601 with milliseconds; ts_ms is the same instant.
    let iso = raw["post"]["ts"].as_str().unwrap();
    assert!(iso.ends_with('Z') && iso.len() == 24, "{iso}");
    assert_eq!(iso, beacon::timefmt::iso8601(post.ts_ms));

    // Reading it back gives the same post.
    let resp = node.get("/api/channels/beacon/posts").send().await.unwrap();
    let read = ok(resp).await;
    assert_eq!(read["v"], 1);
    assert_eq!(read["remaining"], Value::Null);
    let list: PostsResponse = serde_json::from_value(read).unwrap();
    assert_eq!(list.posts.len(), 1);
    assert_eq!(&list.posts[0], post);

    // So does fetching it by id, and by its short id.
    for id in [post.id.as_str(), post.short_id.as_str()] {
        let resp = node.get(&format!("/api/posts/{id}")).send().await.unwrap();
        let body = ok(resp).await;
        assert_eq!(body["v"], 1);
        let one: PostJson = serde_json::from_value(body).unwrap();
        assert_eq!(&one, post);
    }
}

#[tokio::test]
async fn human_posts_are_announcements() {
    let node = spawn_node("pc", &[]).await;
    let created = human_post(&node, "general", "ship it").await;
    assert_eq!(created["post"]["announcement"], true);
    assert_eq!(created["post"]["author_kind"], "human");
    assert_eq!(created["post"]["author"], "Dang");
}

#[tokio::test]
async fn post_bodies_arrive_byte_identical() {
    let node = spawn_node("pc", &[]).await;
    let body = "-rf is not a flag \"quoted\" $HOME `ls`\nsecond line 🚀\n\ttabbed  ";
    let created = agent_post(&node, "general", "pc-configs", body, &[]).await;
    assert_eq!(created["post"]["body"], body);
    assert_eq!(channel_bodies(&node, "general").await, [body]);
}

#[tokio::test]
async fn post_to_missing_channel_404_unless_create() {
    let node = spawn_node("pc", &[]).await;
    let request = |create: bool| {
        json!({
            "body": "hello", "tags": [], "author": "pc-configs",
            "author_kind": "agent", "client": null, "create": create
        })
    };

    let resp = node
        .post("/api/channels/typo/posts")
        .json(&request(false))
        .send()
        .await
        .unwrap();
    let message = expect_error(resp, StatusCode::NOT_FOUND, "not_found").await;
    assert!(message.contains("typo"), "{message}");
    assert!(message.contains("--create"), "{message}");
    assert!(message.contains("./beacon channel new typo"), "{message}");
    // Nothing was created by the failed post.
    let channels = ok(node.get("/api/channels").send().await.unwrap()).await;
    assert_eq!(channels["channels"].as_array().unwrap().len(), 1);

    let resp = node
        .post("/api/channels/typo/posts")
        .json(&request(true))
        .send()
        .await
        .unwrap();
    let created = ok(resp).await;
    assert_eq!(created["post"]["channel"], "typo");
    let channels = ok(node.get("/api/channels").send().await.unwrap()).await;
    let names: Vec<&str> = channels["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["general", "typo"]);
    assert_eq!(channel_bodies(&node, "typo").await, ["hello"]);
}

#[tokio::test]
async fn post_channel_name_is_normalized() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    let created = agent_post(&node, "%23Beacon", "pc-configs", "hi", &[]).await;
    assert_eq!(created["post"]["channel"], "beacon");
}

#[tokio::test]
async fn duplicate_tags_are_collapsed() {
    let node = spawn_node("pc", &[]).await;
    let created = agent_post(
        &node,
        "general",
        "pc-configs",
        "x",
        &["claim", "fyi", "claim"],
    )
    .await;
    assert_eq!(created["post"]["tags"], json!(["claim", "fyi"]));
}

#[tokio::test]
async fn delete_post_hides_it() {
    let node = spawn_node("pc", &[]).await;
    let keep = agent_post(&node, "general", "pc-configs", "keep", &[]).await;
    let gone = agent_post(&node, "general", "pc-configs", "gone", &[]).await;
    let id = post_id(&gone);
    assert_ne!(id, post_id(&keep));

    // By short id, as `delete` accepts.
    let short = &id[18..];
    let resp = node
        .delete(&format!("/api/posts/{short}?by=pc-configs"))
        .send()
        .await
        .unwrap();
    let ack = ok(resp).await;
    assert_eq!(ack, json!({ "v": 1, "ok": true, "deleted": id }));

    assert_eq!(channel_bodies(&node, "general").await, ["keep"]);
    let resp = node.get(&format!("/api/posts/{id}")).send().await.unwrap();
    expect_error(resp, StatusCode::NOT_FOUND, "not_found").await;
    // Already gone: 404, and the first delete stays the only tombstone.
    let resp = node
        .delete(&format!("/api/posts/{id}?by=pc-configs"))
        .send()
        .await
        .unwrap();
    expect_error(resp, StatusCode::NOT_FOUND, "not_found").await;
}

#[tokio::test]
async fn delete_post_records_who_deleted_it() {
    let node = spawn_node("pc", &[]).await;
    let first = agent_post(&node, "general", "pc-configs", "one", &[]).await;
    let second = agent_post(&node, "general", "pc-configs", "two", &[]).await;
    let resp = node
        .delete(&format!("/api/posts/{}?by=mac-juicebox", post_id(&first)))
        .send()
        .await
        .unwrap();
    ok(resp).await;
    // Without `by`, the node's human (BEACON_HUMAN, default Dang) is the deleter.
    let resp = node
        .delete(&format!("/api/posts/{}", post_id(&second)))
        .send()
        .await
        .unwrap();
    ok(resp).await;

    let events = node
        .state()
        .with_store(|s| s.events_after(&VersionVector::new(), 1000))
        .await
        .unwrap();
    let deleters: Vec<String> = events
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::PostDeleted { by, .. } => Some(by),
            _ => None,
        })
        .collect();
    assert_eq!(deleters, ["mac-juicebox", "Dang"]);
}

#[tokio::test]
async fn post_ids_resolve_or_fail_cleanly() {
    let node = spawn_node("pc", &[]).await;
    let resp = node.get("/api/posts/abc").send().await.unwrap();
    expect_error(resp, StatusCode::BAD_REQUEST, "usage").await;
    let resp = node.get("/api/posts/ZZZZZZZZ").send().await.unwrap();
    expect_error(resp, StatusCode::NOT_FOUND, "not_found").await;
    let resp = node
        .delete("/api/posts/ZZZZZZZZ?by=pc-configs")
        .send()
        .await
        .unwrap();
    expect_error(resp, StatusCode::NOT_FOUND, "not_found").await;
}

#[tokio::test]
async fn bad_input_is_400_usage() {
    let node = spawn_node("pc", &[]).await;
    let good_post = json!({
        "body": "hi", "tags": [], "author": "pc-configs",
        "author_kind": "agent", "client": null, "create": false
    });
    let with = |key: &str, value: Value| {
        let mut v = good_post.clone();
        v[key] = value;
        v
    };
    let cases: Vec<(Method, &str, Value)> = vec![
        (Method::POST, "/api/channels", json!({ "name": "gpu jobs" })),
        (
            Method::POST,
            "/api/channels",
            json!({ "name": "a".repeat(33) }),
        ),
        (Method::POST, "/api/channels", json!({})),
        (Method::DELETE, "/api/channels/bad%20name", Value::Null),
        (Method::GET, "/api/channels/bad%20name/posts", Value::Null),
        (
            Method::POST,
            "/api/channels/bad%20name/posts",
            good_post.clone(),
        ),
        (
            Method::POST,
            "/api/channels/general/posts",
            with("author", json!("bad handle")),
        ),
        (
            Method::POST,
            "/api/channels/general/posts",
            with("author", json!("")),
        ),
        (
            Method::POST,
            "/api/channels/general/posts",
            with("tags", json!(["Claim"])),
        ),
        (
            Method::POST,
            "/api/channels/general/posts",
            with("body", json!("")),
        ),
        (
            Method::POST,
            "/api/channels/general/posts",
            with("body", json!("  \n ")),
        ),
        (
            Method::POST,
            "/api/channels/general/posts",
            with("author_kind", json!("robot")),
        ),
        (
            Method::POST,
            "/api/channels/general/posts",
            json!({ "body": "no author" }),
        ),
        (
            Method::GET,
            "/api/channels/general/posts?before=garbage",
            Value::Null,
        ),
        (
            Method::GET,
            "/api/channels/general/posts?before=x:y",
            Value::Null,
        ),
        (
            Method::GET,
            "/api/channels/general/posts?limit=-1",
            Value::Null,
        ),
        (
            Method::GET,
            "/api/channels/general/posts?tag=UPPER",
            Value::Null,
        ),
        (
            Method::GET,
            "/api/channels?handle=bad%20handle",
            Value::Null,
        ),
        (Method::GET, "/api/posts?mentions=bad%20handle", Value::Null),
        (Method::GET, "/api/posts?announcements=maybe", Value::Null),
        (
            Method::DELETE,
            "/api/posts/ZZZZZZZZ?by=bad%20handle",
            Value::Null,
        ),
        (
            Method::POST,
            "/api/who",
            json!({ "handle": "bad handle", "kind": "agent" }),
        ),
        (
            Method::POST,
            "/api/who",
            json!({ "handle": "pc-configs", "kind": "alien" }),
        ),
        (Method::GET, "/api/joins/bad%20handle", Value::Null),
        (
            Method::PUT,
            "/api/joins/pc-configs",
            json!({ "channels": ["not valid"] }),
        ),
        (
            Method::PUT,
            "/api/joins/pc-configs",
            json!({ "channels": "beacon" }),
        ),
        (
            Method::PUT,
            "/api/read-cursors/pc-configs",
            json!({ "channel": "no good", "arrival": 1 }),
        ),
        (
            Method::PUT,
            "/api/read-cursors/pc-configs",
            json!({ "channel": "general" }),
        ),
        (Method::GET, "/api/unread/bad%20handle", Value::Null),
        (
            Method::GET,
            "/api/unread/pc-configs?advance=sometimes",
            Value::Null,
        ),
    ];
    for (method, path, body) in cases {
        let mut req = node.request(method.clone(), path);
        if !body.is_null() {
            req = req.json(&body);
        }
        let resp = req.send().await.unwrap();
        let message = expect_error(resp, StatusCode::BAD_REQUEST, "usage").await;
        assert!(!message.is_empty(), "{method} {path}");
    }
    // Malformed JSON and the wrong content type are usage errors too.
    let resp = node
        .post("/api/channels")
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    expect_error(resp, StatusCode::BAD_REQUEST, "usage").await;
    let resp = node
        .post("/api/channels")
        .header("content-type", "text/plain")
        .body("name=builds")
        .send()
        .await
        .unwrap();
    expect_error(resp, StatusCode::BAD_REQUEST, "usage").await;
    // Nothing above created anything.
    let channels = ok(node.get("/api/channels").send().await.unwrap()).await;
    assert_eq!(channels["channels"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn oversized_bodies_are_413() {
    let node = spawn_node("pc", &[]).await;
    let post = |body: String| {
        json!({
            "body": body, "tags": [], "author": "pc-configs",
            "author_kind": "agent", "client": null, "create": false
        })
    };
    // Exactly at the limit is fine.
    let resp = node
        .post("/api/channels/general/posts")
        .json(&post("x".repeat(beacon::model::BODY_MAX)))
        .send()
        .await
        .unwrap();
    ok(resp).await;
    // One byte over is not.
    let resp = node
        .post("/api/channels/general/posts")
        .json(&post("x".repeat(beacon::model::BODY_MAX + 1)))
        .send()
        .await
        .unwrap();
    expect_error(resp, StatusCode::PAYLOAD_TOO_LARGE, "usage").await;
    // A JSON request far beyond any post is refused before it is read in full.
    let resp = node
        .post("/api/channels/general/posts")
        .json(&post("x".repeat(3 * 1024 * 1024)))
        .send()
        .await
        .unwrap();
    expect_error(resp, StatusCode::PAYLOAD_TOO_LARGE, "usage").await;
    assert_eq!(channel_bodies(&node, "general").await.len(), 1);
}

#[tokio::test]
async fn paging_a_channel_with_before_limit_and_tag() {
    let node = spawn_node("pc", &[]).await;
    for i in 1..=5 {
        let tag: &[&str] = if i % 2 == 1 { &["claim"] } else { &[] };
        agent_post(&node, "general", "pc-configs", &format!("m{i}"), tag).await;
    }
    let get = |query: String| {
        let node = &node;
        async move {
            let resp = node
                .get(&format!("/api/channels/general/posts{query}"))
                .send()
                .await
                .unwrap();
            let list: PostsResponse = serde_json::from_value(ok(resp).await).unwrap();
            list.posts
        }
    };
    let newest = get("?limit=2".into()).await;
    let bodies: Vec<&str> = newest.iter().map(|p| p.body.as_str()).collect();
    assert_eq!(bodies, ["m4", "m5"]);

    let cursor = format!("?limit=2&before={}:{}", newest[0].ts_ms, newest[0].id);
    let older = get(cursor).await;
    let bodies: Vec<&str> = older.iter().map(|p| p.body.as_str()).collect();
    assert_eq!(bodies, ["m2", "m3"]);

    let tagged = get("?tag=claim".into()).await;
    let bodies: Vec<&str> = tagged.iter().map(|p| p.body.as_str()).collect();
    assert_eq!(bodies, ["m1", "m3", "m5"]);

    // The default page is the whole (small) channel, oldest first.
    assert_eq!(get(String::new()).await.len(), 5);
}

#[tokio::test]
async fn cross_channel_queries() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    human_post(&node, "general", "all hands").await;
    agent_post(
        &node,
        "beacon",
        "mac-juicebox",
        "@pc-configs take this",
        &["claim"],
    )
    .await;
    agent_post(&node, "beacon", "mac-juicebox", "plain", &[]).await;

    let query = |q: &'static str| {
        let node = &node;
        async move {
            let resp = node.get(&format!("/api/posts{q}")).send().await.unwrap();
            let body = ok(resp).await;
            assert_eq!(body["v"], 1);
            assert_eq!(body["remaining"], Value::Null);
            let list: PostsResponse = serde_json::from_value(body).unwrap();
            list.posts.into_iter().map(|p| p.body).collect::<Vec<_>>()
        }
    };
    assert_eq!(
        query("").await,
        ["all hands", "@pc-configs take this", "plain"]
    );
    assert_eq!(
        query("?mentions=PC-Configs").await,
        ["@pc-configs take this"]
    );
    assert_eq!(query("?announcements=1").await, ["all hands"]);
    assert_eq!(query("?announcements=0").await.len(), 3);
    assert_eq!(query("?tag=claim").await, ["@pc-configs take this"]);
    assert_eq!(query("?limit=2").await, ["@pc-configs take this", "plain"]);
}

// ---------------------------------------------------------------- presence

#[tokio::test]
async fn heartbeat_emits_once_a_minute() {
    let node = spawn_node("pc", &[]).await;
    let beat = json!({ "handle": "pc-configs", "kind": "agent", "client": "claude-code" });

    let resp = node.post("/api/who").json(&beat).send().await.unwrap();
    let first = ok(resp).await;
    assert_eq!(first, json!({ "v": 1, "ok": true, "recorded": true }));
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let resp = node.post("/api/who").json(&beat).send().await.unwrap();
    let second = ok(resp).await;
    assert_eq!(second, json!({ "v": 1, "ok": true, "recorded": false }));

    assert_eq!(agent_seen_events(&node).await, 1);

    // A new status is recorded at once; the same status again is not.
    let with_status = json!({
        "handle": "pc-configs", "kind": "agent", "client": "claude-code",
        "status": "building the crate"
    });
    for expect_recorded in [true, false] {
        let resp = node
            .post("/api/who")
            .json(&with_status)
            .send()
            .await
            .unwrap();
        assert_eq!(ok(resp).await["recorded"], expect_recorded);
    }
    assert_eq!(agent_seen_events(&node).await, 2);

    // Another handle has its own minute.
    let other = json!({ "handle": "mac-juicebox", "kind": "agent", "client": "codex" });
    let resp = node.post("/api/who").json(&other).send().await.unwrap();
    assert_eq!(ok(resp).await["recorded"], true);
    assert_eq!(agent_seen_events(&node).await, 3);
}

async fn agent_seen_events(node: &TestNode) -> usize {
    let events = node
        .state()
        .with_store(|s| s.events_after(&VersionVector::new(), 1000))
        .await
        .unwrap();
    events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::AgentSeen(_)))
        .count()
}

#[tokio::test]
async fn who_lists_what_heartbeats_recorded() {
    let node = spawn_node("pc", &[]).await;
    let resp = node.get("/api/who").send().await.unwrap();
    assert_eq!(ok(resp).await, json!({ "v": 1, "agents": [] }));

    let resp = node
        .post("/api/who")
        .json(&json!({
            "handle": "pc-configs", "kind": "agent", "client": "claude-code",
            "status": "working on the scaffold"
        }))
        .send()
        .await
        .unwrap();
    ok(resp).await;
    let resp = node
        .post("/api/who")
        .json(&json!({ "handle": "Dang", "kind": "human", "client": "tui" }))
        .send()
        .await
        .unwrap();
    ok(resp).await;

    let raw = ok(node.get("/api/who").send().await.unwrap()).await;
    assert_eq!(raw["v"], 1);
    let who: WhoJson = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(who.agents.len(), 2);
    let agent = who
        .agents
        .iter()
        .find(|a| a.handle == "pc-configs")
        .unwrap();
    assert_eq!(agent.client, "claude-code");
    assert_eq!(agent.node, "pc");
    assert_eq!(agent.status.as_deref(), Some("working on the scaffold"));
    assert!(agent.active);
    assert_eq!(
        agent.last_seen,
        beacon::timefmt::iso8601(agent.last_seen_ms)
    );
    let human = who.agents.iter().find(|a| a.handle == "Dang").unwrap();
    assert_eq!(human.kind, beacon::model::AuthorKind::Human);
    assert_eq!(human.status, None);
    let kinds: Vec<&Value> = raw["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| &a["kind"])
        .collect();
    assert!(kinds.contains(&&json!("human")) && kinds.contains(&&json!("agent")));
}

// ------------------------------------------------- joins, cursors, unread

#[tokio::test]
async fn joins_roundtrip() {
    let node = spawn_node("pc", &[]).await;
    let resp = node.get("/api/joins/pc-configs").send().await.unwrap();
    assert_eq!(
        ok(resp).await,
        json!({ "v": 1, "ok": true, "handle": "pc-configs", "channels": [] })
    );
    // Names normalize and need not exist yet; the answer is the new list.
    let resp = node
        .put("/api/joins/pc-configs")
        .json(&json!({ "channels": ["#Beacon", "vllm", "beacon"] }))
        .send()
        .await
        .unwrap();
    let put = ok(resp).await;
    assert_eq!(put["ok"], true);
    assert_eq!(put["channels"], json!(["beacon", "vllm"]));
    let resp = node.get("/api/joins/pc-configs").send().await.unwrap();
    assert_eq!(ok(resp).await["channels"], json!(["beacon", "vllm"]));
    // Replaces, not merges; another handle is unaffected.
    let resp = node
        .put("/api/joins/pc-configs")
        .json(&json!({ "channels": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(ok(resp).await["channels"], json!([]));
}

#[tokio::test]
async fn read_cursors_roundtrip_and_never_go_backwards() {
    let node = spawn_node("pc", &[]).await;
    let resp = node
        .get("/api/read-cursors/pc-configs")
        .send()
        .await
        .unwrap();
    assert_eq!(
        ok(resp).await,
        json!({ "v": 1, "ok": true, "handle": "pc-configs", "cursors": {} })
    );
    for i in 0..3 {
        agent_post(&node, "general", "mac-juicebox", &format!("m{i}"), &[]).await;
    }
    let head = node
        .state()
        .with_store(|s| Ok(s.arrival_head()))
        .await
        .unwrap();
    assert_eq!(head, 4, "general, then three posts");
    for (arrival, expected) in [(3, 3), (2, 3), (head, head)] {
        let resp = node
            .put("/api/read-cursors/pc-configs")
            .json(&json!({ "channel": "#General", "arrival": arrival }))
            .send()
            .await
            .unwrap();
        let body = ok(resp).await;
        assert_eq!(body["ok"], true);
        assert_eq!(body["cursors"], json!({ "general": expected }));
    }
    let resp = node
        .get("/api/read-cursors/pc-configs")
        .send()
        .await
        .unwrap();
    assert_eq!(ok(resp).await["cursors"], json!({ "general": head }));
}

#[tokio::test]
async fn read_cursor_for_an_unknown_channel_is_404() {
    let node = spawn_node("pc", &[]).await;
    let put = |channel: &str| {
        node.put("/api/read-cursors/pc-configs")
            .json(&json!({ "channel": channel, "arrival": 1 }))
    };
    let resp = put("nowhere").send().await.unwrap();
    let message = expect_error(resp, StatusCode::NOT_FOUND, "not_found").await;
    assert!(message.contains("nowhere"), "{message}");

    // A channel that was deleted is as unknown as one that never was.
    new_channel(&node, "builds").await;
    ok(put("builds").send().await.unwrap()).await;
    node.delete("/api/channels/builds").send().await.unwrap();
    let resp = put("builds").send().await.unwrap();
    expect_error(resp, StatusCode::NOT_FOUND, "not_found").await;

    // No cursor was stored for the one that never existed.
    let resp = node
        .get("/api/read-cursors/pc-configs")
        .send()
        .await
        .unwrap();
    let cursors = ok(resp).await["cursors"].clone();
    assert!(cursors.get("nowhere").is_none(), "{cursors}");
}

#[tokio::test]
async fn read_cursor_beyond_the_arrival_head_is_400() {
    let node = spawn_node("pc", &[]).await;
    let head = node
        .state()
        .with_store(|s| Ok(s.arrival_head()))
        .await
        .unwrap();
    let put = |arrival: i64| {
        node.put("/api/read-cursors/pc-configs")
            .json(&json!({ "channel": "general", "arrival": arrival }))
    };
    // The head itself is fine.
    let body = ok(put(head).send().await.unwrap()).await;
    assert_eq!(body["cursors"], json!({ "general": head }));
    // One past it would hide a post that has not arrived yet.
    let resp = put(head + 1).send().await.unwrap();
    let message = expect_error(resp, StatusCode::BAD_REQUEST, "usage").await;
    assert!(message.contains(&head.to_string()), "{message}");
    let resp = put(i64::MAX).send().await.unwrap();
    expect_error(resp, StatusCode::BAD_REQUEST, "usage").await;
    let resp = put(-1).send().await.unwrap();
    expect_error(resp, StatusCode::BAD_REQUEST, "usage").await;

    // The refused values left the cursor where it was, so a post that
    // arrives afterwards is still unread.
    let resp = node
        .get("/api/read-cursors/pc-configs")
        .send()
        .await
        .unwrap();
    assert_eq!(ok(resp).await["cursors"], json!({ "general": head }));
    agent_post(&node, "general", "mac-juicebox", "later", &[]).await;
    let resp = node
        .get("/api/channels?handle=pc-configs")
        .send()
        .await
        .unwrap();
    assert_eq!(ok(resp).await["channels"][0]["unread"], 1);
}

#[tokio::test]
async fn posts_carry_their_arrival_for_cursors() {
    let node = spawn_node("pc", &[]).await;
    let created = agent_post(&node, "general", "mac-juicebox", "hi", &[]).await;
    let arrival = created["post"]["arrival"].as_i64().unwrap();
    assert!(arrival > 1, "general was arrival 1; the post came after");
    // Marking the channel read up to that arrival clears its unread count.
    let count = |node: &TestNode| {
        let req = node.get("/api/channels?handle=pc-configs");
        async move {
            let body = ok(req.send().await.unwrap()).await;
            body["channels"][0]["unread"].clone()
        }
    };
    assert_eq!(count(&node).await, 1);
    let resp = node
        .put("/api/read-cursors/pc-configs")
        .json(&json!({ "channel": "general", "arrival": arrival }))
        .send()
        .await
        .unwrap();
    ok(resp).await;
    assert_eq!(count(&node).await, 0);
}

#[tokio::test]
async fn channels_carry_unread_when_a_handle_is_given() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    new_channel(&node, "vllm").await;
    agent_post(&node, "beacon", "mac-juicebox", "one", &[]).await;
    agent_post(&node, "beacon", "mac-juicebox", "two", &[]).await;
    agent_post(&node, "beacon", "pc-configs", "mine", &[]).await;
    agent_post(&node, "vllm", "mac-juicebox", "elsewhere", &[]).await;
    human_post(&node, "vllm", "announcement").await;

    let resp = node
        .get("/api/channels?handle=pc-configs")
        .send()
        .await
        .unwrap();
    let body = ok(resp).await;
    assert_eq!(body["announcements_unread"], 1);
    let channels: ChannelsJson = serde_json::from_value(body).unwrap();
    assert_eq!(channels.announcements_unread, Some(1));
    let unread: HashMap<&str, Option<u32>> = channels
        .channels
        .iter()
        .map(|c| (c.name.as_str(), c.unread))
        .collect();
    // Every post by someone else counts, joined channel or not; one's own do not.
    assert_eq!(unread["beacon"], Some(2));
    assert_eq!(unread["vllm"], Some(2));
    assert_eq!(unread["general"], Some(0));

    // Dang wrote the announcement himself, so for him there is none unread.
    let resp = node.get("/api/channels?handle=Dang").send().await.unwrap();
    assert_eq!(ok(resp).await["announcements_unread"], 0);

    // Without a handle both are null, not 0.
    let resp = node.get("/api/channels").send().await.unwrap();
    let body = ok(resp).await;
    assert_eq!(body["announcements_unread"], Value::Null);
    assert!(body["channels"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c["unread"].is_null()));

    // Reading vllm up to the newest post clears the announcement too.
    let head = node
        .state()
        .with_store(|s| Ok(s.arrival_head()))
        .await
        .unwrap();
    let resp = node
        .put("/api/read-cursors/pc-configs")
        .json(&json!({ "channel": "vllm", "arrival": head }))
        .send()
        .await
        .unwrap();
    ok(resp).await;
    let resp = node
        .get("/api/channels?handle=pc-configs")
        .send()
        .await
        .unwrap();
    let body = ok(resp).await;
    assert_eq!(body["announcements_unread"], 0);
    assert_eq!(body["channels"][2]["name"], "vllm");
    assert_eq!(body["channels"][2]["unread"], 0);
}

#[tokio::test]
async fn unread_endpoint_advances() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    human_post(&node, "beacon", "read this first").await;

    // Without `advance` nothing is marked read.
    for _ in 0..2 {
        let resp = node.get("/api/unread/pc-configs").send().await.unwrap();
        let body = ok(resp).await;
        assert_eq!(body["v"], 1);
        assert_eq!(body["remaining"], 0);
        assert_eq!(body["posts"].as_array().unwrap().len(), 1);
    }
    // With it, the first call shows the post and the second nothing.
    let resp = node
        .get("/api/unread/pc-configs?advance=1")
        .send()
        .await
        .unwrap();
    let first: PostsResponse = serde_json::from_value(ok(resp).await).unwrap();
    assert_eq!(first.posts.len(), 1);
    assert_eq!(first.posts[0].body, "read this first");
    assert_eq!(first.remaining, Some(0));

    let resp = node
        .get("/api/unread/pc-configs?advance=1")
        .send()
        .await
        .unwrap();
    let second: PostsResponse = serde_json::from_value(ok(resp).await).unwrap();
    assert!(second.posts.is_empty());
    assert_eq!(second.remaining, Some(0));

    // A new post is unread again.
    human_post(&node, "beacon", "and this").await;
    let resp = node
        .get("/api/unread/pc-configs?advance=true")
        .send()
        .await
        .unwrap();
    let third: PostsResponse = serde_json::from_value(ok(resp).await).unwrap();
    assert_eq!(third.posts.len(), 1);
    assert_eq!(third.posts[0].body, "and this");
}

#[tokio::test]
async fn unread_follows_relevance_and_pages() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    new_channel(&node, "vllm").await;
    node.put("/api/joins/pc-configs")
        .json(&json!({ "channels": ["beacon"] }))
        .send()
        .await
        .unwrap();
    agent_post(&node, "beacon", "mac-juicebox", "joined channel", &[]).await; // yes
    agent_post(&node, "vllm", "mac-juicebox", "not mine", &[]).await; // no
    human_post(&node, "vllm", "announcement").await; // yes, first
    agent_post(&node, "vllm", "mac-juicebox", "@pc-configs mention", &[]).await; // yes
    agent_post(&node, "beacon", "pc-configs", "my own", &[]).await; // no
    agent_post(&node, "general", "mac-juicebox", "@all hands", &[]).await; // yes

    let resp = node.get("/api/unread/pc-configs").send().await.unwrap();
    let list: PostsResponse = serde_json::from_value(ok(resp).await).unwrap();
    let bodies: Vec<&str> = list.posts.iter().map(|p| p.body.as_str()).collect();
    assert_eq!(bodies.len(), 4, "{bodies:?}");
    assert_eq!(bodies[0], "announcement");
    assert!(bodies.contains(&"joined channel"));
    assert!(bodies.contains(&"@pc-configs mention"));
    assert!(bodies.contains(&"@all hands"));
    assert_eq!(list.remaining, Some(0));

    // A page of 2 says how many are left, then advancing walks through them.
    let resp = node
        .get("/api/unread/pc-configs?limit=2&advance=1")
        .send()
        .await
        .unwrap();
    let page: PostsResponse = serde_json::from_value(ok(resp).await).unwrap();
    assert_eq!(page.posts.len(), 2);
    assert_eq!(page.remaining, Some(2));
}

// ------------------------------------------------------ writes and arrivals

#[tokio::test]
async fn arrivals_broadcast_only_what_derived() {
    let node = spawn_node("pc", &[]).await;
    let state = node.state().clone();
    let mut rx = state.arrivals.subscribe();

    // A local write is announced.
    let kind = EventKind::ChannelCreated {
        name: "builds".into(),
        topic: None,
    };
    let local = state.commit(kind).await.unwrap();
    assert_eq!(rx.recv().await.unwrap(), local);

    // So is a remote event that is new and valid.
    let remote = |seq: u64, name: &str| Event {
        origin: "mac~abcd1234".into(),
        seq,
        id: ulid::Ulid::generate().to_string(),
        ts: 1_000 + seq as i64,
        kind: EventKind::ChannelCreated {
            name: name.into(),
            topic: None,
        },
    };
    let good = remote(1, "vllm");
    let applied = state.ingest(good.clone()).await.unwrap().unwrap();
    assert_eq!(applied.event, good);
    assert_eq!(rx.recv().await.unwrap(), applied);

    // One it already has changes nothing and is not announced.
    assert!(state.ingest(good).await.unwrap().is_none());
    // An event that breaks the rules is stored (so sync stays gap-free) but
    // derives nothing, so it is not announced either.
    let bad = remote(2, "Not A Channel");
    assert!(state.ingest(bad).await.unwrap().is_some());
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));

    let resp = node.get("/api/channels").send().await.unwrap();
    let channels: ChannelsJson = serde_json::from_value(ok(resp).await).unwrap();
    let names: Vec<&str> = channels.channels.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["general", "builds", "vllm"]);
    // The stored invalid event still counts toward what this node has.
    let have = state.with_store(|s| Ok(s.version_vector())).await.unwrap();
    assert_eq!(have["mac~abcd1234"], 2);
}

#[tokio::test]
async fn http_writes_are_broadcast_in_arrival_order() {
    let node = spawn_node("pc", &[]).await;
    let mut rx = node.state().arrivals.subscribe();
    new_channel(&node, "beacon").await;
    agent_post(&node, "beacon", "pc-configs", "hi", &[]).await;
    let first = rx.recv().await.unwrap();
    let second = rx.recv().await.unwrap();
    assert!(matches!(first.event.kind, EventKind::ChannelCreated { .. }));
    assert!(matches!(second.event.kind, EventKind::PostCreated(_)));
    assert!(first.arrival < second.arrival);
}

#[tokio::test]
async fn data_survives_a_restart_on_the_same_dir_and_port() {
    let node = spawn_node("pc", &[]).await;
    new_channel(&node, "beacon").await;
    agent_post(&node, "beacon", "pc-configs", "before", &[]).await;
    let have_before = ok(node.get("/api/status").send().await.unwrap()).await["have"].clone();
    let addr = node.handle.addr;

    let node = node.stop().await.restart().await;
    assert_eq!(node.handle.addr, addr);
    assert_eq!(channel_bodies(&node, "beacon").await, ["before"]);
    let status = ok(node.get("/api/status").send().await.unwrap()).await;
    assert_eq!(status["have"], have_before);
    // And it keeps taking writes under the same origin.
    agent_post(&node, "beacon", "pc-configs", "after", &[]).await;
    assert_eq!(channel_bodies(&node, "beacon").await, ["before", "after"]);
}

// ------------------------------------------------------------------ config

fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |key| map.get(key).cloned()
}

fn usage(result: Result<Config, Error>) -> String {
    match result {
        Err(Error::Usage(message)) => message,
        other => panic!("expected a usage error, got {other:?}"),
    }
}

#[test]
fn config_defaults() {
    let cfg =
        Config::from_vars(vars(&[("BEACON_NODE", "pc"), ("BEACON_SECRET", "s3cret")])).unwrap();
    assert_eq!(cfg.node, "pc");
    assert_eq!(cfg.secret, "s3cret");
    assert_eq!(cfg.url, None);
    assert!(cfg.peers.is_empty());
    assert_eq!(cfg.listen, "0.0.0.0:7700".parse().unwrap());
    assert_eq!(cfg.human, "Dang");
    assert_eq!(cfg.max_attachment_mb, 50);
    assert_eq!(cfg.proxy, None);
    assert_eq!(cfg.data_dir, std::path::PathBuf::from("/data"));
}

#[test]
fn config_reads_every_variable() {
    let cfg = Config::from_vars(vars(&[
        ("BEACON_NODE", "mac"),
        ("BEACON_SECRET", "s3cret"),
        ("BEACON_URL", "http://dangs-macbook-pro:7700"),
        (
            "BEACON_PEERS",
            " http://soybean-wsl:7700 , http://b:7700/ ,, ",
        ),
        ("BEACON_LISTEN", "127.0.0.1:9000"),
        ("BEACON_HUMAN", "Boss"),
        ("BEACON_MAX_ATTACHMENT_MB", "7"),
        ("BEACON_PROXY", "socks5h://127.0.0.1:1055"),
        ("BEACON_DATA", "/tmp/beacon-data"),
    ]))
    .unwrap();
    assert_eq!(cfg.node, "mac");
    assert_eq!(cfg.url.as_deref(), Some("http://dangs-macbook-pro:7700"));
    assert_eq!(cfg.peers, ["http://soybean-wsl:7700", "http://b:7700/"]);
    assert_eq!(cfg.listen, "127.0.0.1:9000".parse().unwrap());
    assert_eq!(cfg.human, "Boss");
    assert_eq!(cfg.max_attachment_mb, 7);
    assert_eq!(cfg.proxy.as_deref(), Some("socks5h://127.0.0.1:1055"));
    assert_eq!(cfg.data_dir, std::path::PathBuf::from("/tmp/beacon-data"));
}

#[test]
fn config_treats_empty_values_as_unset() {
    // Compose passes `VAR=` from an env file as an empty string.
    let cfg = Config::from_vars(vars(&[
        ("BEACON_NODE", "pc"),
        ("BEACON_SECRET", "s3cret"),
        ("BEACON_URL", ""),
        ("BEACON_PEERS", ""),
        ("BEACON_LISTEN", ""),
        ("BEACON_HUMAN", ""),
        ("BEACON_MAX_ATTACHMENT_MB", ""),
        ("BEACON_PROXY", "  "),
        ("BEACON_DATA", ""),
    ]))
    .unwrap();
    assert_eq!(cfg.url, None);
    assert!(cfg.peers.is_empty());
    assert_eq!(cfg.human, "Dang");
    assert_eq!(cfg.max_attachment_mb, 50);
    assert_eq!(cfg.proxy, None);
    assert_eq!(cfg.data_dir, std::path::PathBuf::from("/data"));
}

#[test]
fn config_requires_node_and_secret() {
    let message = usage(Config::from_vars(vars(&[("BEACON_SECRET", "s")])));
    assert!(message.contains("BEACON_NODE"), "{message}");
    let message = usage(Config::from_vars(vars(&[
        ("BEACON_NODE", "pc"),
        ("BEACON_SECRET", ""),
    ])));
    assert!(message.contains("BEACON_SECRET"), "{message}");
    let message = usage(Config::from_vars(vars(&[("BEACON_NODE", "pc")])));
    assert!(message.contains("BEACON_SECRET"), "{message}");
}

#[test]
fn config_validates_names_and_numbers_naming_the_variable() {
    let base = [("BEACON_NODE", "pc"), ("BEACON_SECRET", "s")];
    let bad = |var: &str, value: &str| {
        let mut pairs = base.to_vec();
        pairs.retain(|(k, _)| *k != var);
        pairs.push((var, value));
        usage(Config::from_vars(vars(&pairs)))
    };
    for (var, value) in [
        ("BEACON_HUMAN", "bad handle"),
        ("BEACON_HUMAN", "-Dang"),
        ("BEACON_NODE", "my node"),
        // `~` separates the node from the database id in an origin.
        ("BEACON_NODE", "pc~1"),
        ("BEACON_NODE", &"n".repeat(33)),
        ("BEACON_LISTEN", "not-an-address"),
        ("BEACON_LISTEN", "7700"),
        ("BEACON_MAX_ATTACHMENT_MB", "lots"),
        ("BEACON_MAX_ATTACHMENT_MB", "-1"),
        ("BEACON_MAX_ATTACHMENT_MB", "0"),
    ] {
        let message = bad(var, value);
        assert!(message.contains(var), "{var}={value:?}: {message}");
    }
}

#[test]
fn config_debug_does_not_print_the_secret() {
    let cfg =
        Config::from_vars(vars(&[("BEACON_NODE", "pc"), ("BEACON_SECRET", "hunter2")])).unwrap();
    let shown = format!("{cfg:?}");
    assert!(!shown.contains("hunter2"), "{shown}");
    assert!(shown.contains("pc"));
}
