//! The authenticated HTTP API over the store, tested against real nodes on
//! loopback ports, plus the config that starts them.

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use beacon::api::{ChannelsJson, PostCreated, PostJson, PostsResponse, WhoJson};
use beacon::config::Config;
use beacon::error::Error;
use beacon::model::{AuthorKind, Event, EventKind, NewPost, VersionVector};
use beacon::server::{now_ms, secret_matches};
use common::*;
use reqwest::multipart::{Form, Part};
use reqwest::{Method, Response, StatusCode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
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
        (
            Method::GET,
            "/api/files/abababababababababababababababababababababababababababababababab",
        ),
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

// ------------------------------------------------------------- attachments

/// `len` bytes that look random but are the same on every run.
fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The names in `<data dir>/files`, sorted.
fn files_in(node: &TestNode) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(node.dir.path().join("files"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// Polls `check` until it holds, failing the test after five seconds.
async fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    let start = Instant::now();
    while !check() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "gave up waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn post_meta(body: &str) -> Value {
    json!({
        "body": body, "tags": [], "author": "pc-configs",
        "author_kind": "agent", "client": "claude-code", "create": false
    })
}

fn meta_part(meta: &Value) -> Part {
    Part::text(meta.to_string())
        .mime_str("application/json")
        .unwrap()
}

fn file_part(name: &str, bytes: Vec<u8>) -> Part {
    Part::bytes(bytes).file_name(name.to_string())
}

/// Posts `meta` with a file attached, as the multipart form: meta first.
async fn upload(
    node: &TestNode,
    channel: &str,
    meta: &Value,
    name: &str,
    bytes: Vec<u8>,
) -> Response {
    let form = Form::new()
        .part("meta", meta_part(meta))
        .part("file", file_part(name, bytes));
    send_form(node, channel, form).await
}

async fn send_form(node: &TestNode, channel: &str, form: Form) -> Response {
    node.post(&format!("/api/channels/{channel}/posts"))
        .multipart(form)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn upload_and_download_roundtrip() {
    let node = spawn_node("pc", &[]).await;
    let bytes = pseudo_random(1024 * 1024, 7);
    let sha = sha256_hex(&bytes);

    let meta = post_meta("full log attached");
    let resp = upload(&node, "general", &meta, "build-log.txt", bytes.clone()).await;
    let created: PostCreated = serde_json::from_value(ok(resp).await).unwrap();
    let att = created.post.attachment.clone().expect("an attachment");
    assert_eq!(att.name, "build-log.txt");
    assert_eq!(att.size, 1024 * 1024);
    assert_eq!(att.sha256, sha);
    assert!(att.available);
    assert_eq!(created.post.body, "full log attached");
    assert_eq!(created.post.author, "pc-configs");

    // The bytes come back identical.
    let resp = node.get(&format!("/api/files/{sha}")).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], "application/octet-stream");
    assert_eq!(resp.headers()["content-length"], "1048576");
    assert_eq!(resp.bytes().await.unwrap().as_ref(), bytes.as_slice());

    // Read back, the post carries the same attachment; the blob is the only file.
    let resp = node
        .get(&format!("/api/posts/{}", created.post.id))
        .send()
        .await
        .unwrap();
    let post: PostJson = serde_json::from_value(ok(resp).await).unwrap();
    assert_eq!(post.attachment, created.post.attachment);
    assert_eq!(files_in(&node), [sha]);
}

#[tokio::test]
async fn attachment_over_limit_413() {
    let node = spawn_node_with("pc", &[], |cfg| cfg.max_attachment_mb = 1).await;
    let meta = post_meta("too big");
    let resp = upload(
        &node,
        "general",
        &meta,
        "big.bin",
        pseudo_random(2 << 20, 3),
    )
    .await;
    let message = expect_error(resp, StatusCode::PAYLOAD_TOO_LARGE, "usage").await;
    assert!(message.contains("attachment is over 1 MB"), "{message}");
    assert!(channel_bodies(&node, "general").await.is_empty());
    assert_eq!(files_in(&node), Vec::<String>::new());

    // Exactly at the limit is fine; one byte more is not.
    let resp = upload(
        &node,
        "general",
        &meta,
        "fits.bin",
        pseudo_random(1 << 20, 4),
    )
    .await;
    ok(resp).await;
    let resp = upload(
        &node,
        "general",
        &meta,
        "no.bin",
        pseudo_random((1 << 20) + 1, 5),
    )
    .await;
    expect_error(resp, StatusCode::PAYLOAD_TOO_LARGE, "usage").await;
    assert_eq!(files_in(&node).len(), 1, "only the file that fit");
    assert_eq!(channel_bodies(&node, "general").await.len(), 1);
}

#[tokio::test]
async fn missing_blob_404() {
    let node = spawn_node("pc", &[]).await;
    let sha = "ab".repeat(32);
    let resp = node.get(&format!("/api/files/{sha}")).send().await.unwrap();
    let message = expect_error(resp, StatusCode::NOT_FOUND, "not_found").await;
    assert!(message.contains(&sha), "names the attachment: {message}");
}

#[tokio::test]
async fn file_names_that_are_not_a_sha256_are_400() {
    let node = spawn_node("pc", &[]).await;
    // A file the paths below would reach if they were let through.
    std::fs::write(node.dir.path().join("beacon-secret"), b"nope").unwrap();
    let bad = [
        "not-a-hash".to_string(),
        "AB".repeat(32),
        "ab".repeat(31),
        "ab".repeat(33),
        "zz".repeat(32),
        "..%2Fbeacon-secret".to_string(),
        "..%2F..%2Fetc%2Fpasswd".to_string(),
        format!("{}%2F..%2Fbeacon-secret", "ab".repeat(16)),
    ];
    for name in bad {
        let resp = node
            .get(&format!("/api/files/{name}"))
            .send()
            .await
            .unwrap();
        expect_error(resp, StatusCode::BAD_REQUEST, "usage").await;
    }
}

#[tokio::test]
async fn identical_uploads_share_one_blob() {
    let node = spawn_node("pc", &[]).await;
    let bytes = pseudo_random(100_000, 11);
    let sha = sha256_hex(&bytes);
    let mut ids = Vec::new();
    for name in ["first.log", "second.log"] {
        let resp = upload(&node, "general", &post_meta(name), name, bytes.clone()).await;
        let created = ok(resp).await;
        assert_eq!(created["post"]["attachment"]["sha256"], sha);
        assert_eq!(created["post"]["attachment"]["name"], name);
        assert_eq!(created["post"]["attachment"]["available"], true);
        ids.push(post_id(&created));
    }
    assert_ne!(ids[0], ids[1]);
    assert_eq!(files_in(&node), [sha]);
}

#[tokio::test]
async fn multipart_posts_follow_the_post_rules() {
    let node = spawn_node("pc", &[]).await;
    let bytes = pseudo_random(2048, 5);

    // A file alone is a post (an empty body is fine with an attachment).
    let resp = upload(&node, "general", &post_meta(""), "only.bin", bytes.clone()).await;
    let created = ok(resp).await;
    assert_eq!(created["post"]["body"], "");
    assert_eq!(created["post"]["attachment"]["size"], 2048);

    // Tags and mentions come from the meta part, like the JSON form.
    let meta = json!({
        "body": "see @dang", "tags": ["done", "done", "log"], "author": "pc-configs",
        "author_kind": "agent", "client": null, "create": false
    });
    let created = ok(upload(&node, "general", &meta, "a.txt", bytes.clone()).await).await;
    assert_eq!(created["post"]["tags"], json!(["done", "log"]));
    assert_eq!(created["post"]["mentions"], json!(["dang"]));
    assert_eq!(created["post"]["client"], Value::Null);

    // `create` makes the channel, after the file has arrived.
    let mut meta = post_meta("fresh");
    meta["create"] = json!(true);
    ok(upload(&node, "Fresh-Channel", &meta, "b.txt", bytes.clone()).await).await;
    let names: Vec<String> = {
        let resp = node.get("/api/channels").send().await.unwrap();
        let channels: ChannelsJson = serde_json::from_value(ok(resp).await).unwrap();
        channels.channels.into_iter().map(|c| c.name).collect()
    };
    assert_eq!(names, ["general", "fresh-channel"]);

    // A form with no file is the plain post.
    let form = Form::new().part("meta", meta_part(&post_meta("just text")));
    let created = ok(send_form(&node, "general", form).await).await;
    assert_eq!(created["post"]["attachment"], Value::Null);
    // ...and still needs something to say.
    let form = Form::new().part("meta", meta_part(&post_meta("  ")));
    let resp = send_form(&node, "general", form).await;
    expect_error(resp, StatusCode::BAD_REQUEST, "usage").await;
}

#[tokio::test]
async fn bad_uploads_are_refused_before_a_file_is_stored() {
    let node = spawn_node("pc", &[]).await;
    let bytes = || pseudo_random(64 * 1024, 9);
    let meta = post_meta("x");
    let with_meta = |key: &str, value: Value| {
        let mut m = meta.clone();
        m[key] = value;
        m
    };
    let usage = StatusCode::BAD_REQUEST;

    // (what is wrong, the form, the expected status and code)
    let cases: Vec<(&str, Form, StatusCode, &str)> = vec![
        (
            "the file comes before the meta",
            Form::new()
                .part("file", file_part("a.bin", bytes()))
                .part("meta", meta_part(&meta)),
            usage,
            "usage",
        ),
        (
            "no meta at all",
            Form::new().part("file", file_part("a.bin", bytes())),
            usage,
            "usage",
        ),
        (
            "an unknown part",
            Form::new()
                .part("meta", meta_part(&meta))
                .part("extra", Part::text("?"))
                .part("file", file_part("a.bin", bytes())),
            usage,
            "usage",
        ),
        (
            "meta twice",
            Form::new()
                .part("meta", meta_part(&meta))
                .part("meta", meta_part(&meta))
                .part("file", file_part("a.bin", bytes())),
            usage,
            "usage",
        ),
        (
            "a file without a name",
            Form::new()
                .part("meta", meta_part(&meta))
                .part("file", Part::bytes(bytes())),
            usage,
            "usage",
        ),
        (
            "meta that is not JSON",
            Form::new()
                .part("meta", Part::text("{not json"))
                .part("file", file_part("a.bin", bytes())),
            usage,
            "usage",
        ),
        (
            "meta missing a field",
            Form::new()
                .part("meta", meta_part(&json!({ "body": "no author" })))
                .part("file", file_part("a.bin", bytes())),
            usage,
            "usage",
        ),
        (
            "a bad author handle",
            Form::new()
                .part("meta", meta_part(&with_meta("author", json!("bad handle"))))
                .part("file", file_part("a.bin", bytes())),
            usage,
            "usage",
        ),
        (
            "only BEACON_HUMAN may post as human",
            Form::new()
                .part("meta", meta_part(&with_meta("author_kind", json!("human"))))
                .part("file", file_part("a.bin", bytes())),
            usage,
            "usage",
        ),
        (
            "a client name over 64 bytes",
            Form::new()
                .part(
                    "meta",
                    meta_part(&with_meta("client", json!("c".repeat(65)))),
                )
                .part("file", file_part("a.bin", bytes())),
            usage,
            "usage",
        ),
        (
            "a control character in the body",
            Form::new()
                .part("meta", meta_part(&with_meta("body", json!("a\u{1b}[0m"))))
                .part("file", file_part("a.bin", bytes())),
            usage,
            "usage",
        ),
        (
            "a name that reduces to nothing",
            Form::new()
                .part("meta", meta_part(&meta))
                .part("file", file_part("some/dir/", bytes())),
            usage,
            "usage",
        ),
        (
            "a file name over 255 bytes",
            Form::new()
                .part("meta", meta_part(&meta))
                .part("file", file_part(&"n".repeat(256), bytes())),
            usage,
            "usage",
        ),
        (
            "a channel that does not exist",
            Form::new()
                .part("meta", meta_part(&meta))
                .part("file", file_part("a.bin", bytes())),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
    ];
    for (what, form, status, code) in cases {
        // The channel case posts to a channel that is not there.
        let channel = if status == StatusCode::NOT_FOUND {
            "nowhere"
        } else {
            "general"
        };
        let resp = send_form(&node, channel, form).await;
        expect_error(resp, status, code).await;
        assert_eq!(files_in(&node), Vec::<String>::new(), "{what}");
        assert!(channel_bodies(&node, "general").await.is_empty(), "{what}");
    }

    // A channel name that cannot exist is refused the same way.
    let form = Form::new()
        .part("meta", meta_part(&meta))
        .part("file", file_part("a.bin", bytes()));
    let resp = send_form(&node, "bad%20name", form).await;
    expect_error(resp, StatusCode::BAD_REQUEST, "usage").await;
    assert_eq!(files_in(&node), Vec::<String>::new());

    // `multipart/form-data` without a boundary is not a form.
    let resp = node
        .post("/api/channels/general/posts")
        .header("content-type", "multipart/form-data")
        .body("whatever")
        .send()
        .await
        .unwrap();
    expect_error(resp, StatusCode::BAD_REQUEST, "usage").await;
}

#[tokio::test]
async fn attachment_names_are_reduced_to_a_file_name() {
    let node = spawn_node("pc", &[]).await;
    let cases = [
        ("report.pdf", "report.pdf"),
        ("../../etc/passwd", "passwd"),
        (r"C:\Users\dang\report.pdf", "report.pdf"),
        ("dir/sub/log.txt", "log.txt"),
        ("  spaced name.txt ", "spaced name.txt"),
        ("résumé – 日本語.txt", "résumé – 日本語.txt"),
        (".hidden", ".hidden"),
    ];
    for (sent, kept) in cases {
        let resp = upload(&node, "general", &post_meta("n"), sent, vec![1, 2, 3]).await;
        let created = ok(resp).await;
        assert_eq!(created["post"]["attachment"]["name"], kept, "sent {sent:?}");
    }
    for sent in ["..", ".", "a/..", "/"] {
        let resp = upload(&node, "general", &post_meta("n"), sent, vec![1, 2, 3]).await;
        expect_error(resp, StatusCode::BAD_REQUEST, "usage").await;
    }
}

#[tokio::test]
async fn json_posts_keep_their_2_mib_limit_and_the_upload_limit_is_per_node() {
    // A raised limit for uploads must not let big JSON through.
    let node = spawn_node_with("pc", &[], |cfg| cfg.max_attachment_mb = 8).await;
    let post = json!({
        "body": "x".repeat(3 * 1024 * 1024), "tags": [], "author": "pc-configs",
        "author_kind": "agent", "client": null, "create": false
    });
    let resp = node
        .post("/api/channels/general/posts")
        .json(&post)
        .send()
        .await
        .unwrap();
    expect_error(resp, StatusCode::PAYLOAD_TOO_LARGE, "usage").await;
    // Seven MB of file is fine on this node.
    let resp = upload(
        &node,
        "general",
        &post_meta("big"),
        "big.bin",
        vec![7; 7 << 20],
    )
    .await;
    ok(resp).await;
}

#[tokio::test]
async fn upload_cut_off_midway_leaves_nothing_behind() {
    let node = spawn_node("pc", &[]).await;
    let boundary = "----beacon-test-boundary";
    let meta = post_meta("never finished").to_string();
    let head = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"meta\"\r\n\
         Content-Type: application/json\r\n\r\n{meta}\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"big.bin\"\r\n\
         Content-Type: application/octet-stream\r\n\r\n"
    );
    // Promise 5 MB, send a quarter of one, then hang up.
    let announced = head.len() + 5 * 1024 * 1024;
    let request = format!(
        "POST /api/channels/general/posts HTTP/1.1\r\nHost: {}\r\n\
         Authorization: Bearer {SECRET}\r\n\
         Content-Type: multipart/form-data; boundary={boundary}\r\n\
         Content-Length: {announced}\r\n\r\n",
        node.handle.addr
    );
    let mut socket = TcpStream::connect(node.handle.addr).await.unwrap();
    socket.write_all(request.as_bytes()).await.unwrap();
    socket.write_all(head.as_bytes()).await.unwrap();
    socket.write_all(&vec![7u8; 256 * 1024]).await.unwrap();
    socket.flush().await.unwrap();

    // The server is holding the bytes it has so far.
    let files = node.dir.path().join("files");
    let in_progress = |dir: &Path| {
        std::fs::read_dir(dir)
            .unwrap()
            .any(|e| e.unwrap().metadata().unwrap().len() > 0)
    };
    wait_until("the partial upload to reach the disk", || {
        in_progress(&files)
    })
    .await;

    drop(socket);
    wait_until("the partial upload to be cleaned up", || {
        files_in(&node).is_empty()
    })
    .await;
    assert!(channel_bodies(&node, "general").await.is_empty());
    // The node still works.
    ok(upload(&node, "general", &post_meta("after"), "ok.bin", vec![1; 10]).await).await;
}

// ------------------------------------------------- free text and who posts

/// Posts the JSON `body` to #general and returns the response.
async fn post_json(node: &TestNode, channel: &str, body: &Value) -> Response {
    node.post(&format!("/api/channels/{channel}/posts"))
        .json(body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn free_text_limits_are_400_at_the_edge() {
    let node = spawn_node("pc", &[]).await;
    let post = |edit: &dyn Fn(&mut Value)| {
        let mut p = post_meta("hi");
        edit(&mut p);
        p
    };
    let refused = |resp: Response| expect_error(resp, StatusCode::BAD_REQUEST, "usage");

    // client: at most 64 bytes (bytes, not characters), no control characters.
    let p = post(&|p| p["client"] = json!("c".repeat(64)));
    ok(post_json(&node, "general", &p).await).await;
    let p = post(&|p| p["client"] = json!("c".repeat(65)));
    let message = refused(post_json(&node, "general", &p).await).await;
    assert!(message.contains("client"), "{message}");
    let p = post(&|p| p["client"] = json!("é".repeat(33)));
    refused(post_json(&node, "general", &p).await).await;
    let p = post(&|p| p["client"] = json!("code\u{1}"));
    refused(post_json(&node, "general", &p).await).await;

    // tags: at most 8 per post.
    let eight: Vec<String> = (1..=8).map(|i| format!("t{i}")).collect();
    let p = post(&|p| p["tags"] = json!(eight));
    ok(post_json(&node, "general", &p).await).await;
    let nine: Vec<String> = (1..=9).map(|i| format!("t{i}")).collect();
    let p = post(&|p| p["tags"] = json!(nine));
    let message = refused(post_json(&node, "general", &p).await).await;
    assert!(message.contains("tags"), "{message}");

    // body: only \n and \t among the control characters (Unicode Cc).
    let p = post(&|p| p["body"] = json!("line one\n\tline two\n"));
    ok(post_json(&node, "general", &p).await).await;
    for bad in [
        "red \u{1b}[31m",
        "nul\u{0}",
        "bell\u{7}",
        "del\u{7f}",
        "c1\u{85}",
        "a\r\nb",
    ] {
        let p = post(&|p| p["body"] = json!(bad));
        let message = refused(post_json(&node, "general", &p).await).await;
        assert!(message.contains("control character"), "{bad:?}: {message}");
    }
    // Other invisible characters are not control characters.
    let p = post(&|p| p["body"] = json!("zero\u{200b}width and \u{1f600}"));
    ok(post_json(&node, "general", &p).await).await;

    // A refused post has no side effects: not even the channel it asked for.
    let p = post(&|p| {
        p["create"] = json!(true);
        p["client"] = json!("c".repeat(65));
    });
    refused(post_json(&node, "fresh", &p).await).await;
    let resp = node.get("/api/channels/fresh/posts").send().await.unwrap();
    expect_error(resp, StatusCode::NOT_FOUND, "not_found").await;
}

#[tokio::test]
async fn presence_and_topic_limits_are_400_at_the_edge() {
    let node = spawn_node("pc", &[]).await;
    let refused = |resp: Response| expect_error(resp, StatusCode::BAD_REQUEST, "usage");
    let beat = |extra: Value| {
        let mut b = json!({ "handle": "pc-configs", "kind": "agent" });
        for (k, v) in extra.as_object().unwrap() {
            b[k] = v.clone();
        }
        b
    };
    let heartbeat = |b: Value| node.post("/api/who").json(&b).send();

    // status: at most 200 bytes.
    let resp = heartbeat(beat(json!({ "status": "s".repeat(200) })))
        .await
        .unwrap();
    assert_eq!(ok(resp).await["recorded"], true);
    // Refused even though a heartbeat for this handle is not due yet.
    let resp = heartbeat(beat(json!({ "status": "s".repeat(201) })))
        .await
        .unwrap();
    let message = refused(resp).await;
    assert!(message.contains("status"), "{message}");
    let resp = heartbeat(beat(json!({ "status": "é".repeat(101) })))
        .await
        .unwrap();
    refused(resp).await;
    let resp = heartbeat(beat(json!({ "status": "busy\u{7}" })))
        .await
        .unwrap();
    refused(resp).await;
    // client: at most 64 bytes.
    let resp = heartbeat(beat(json!({ "client": "c".repeat(64) })))
        .await
        .unwrap();
    ok(resp).await;
    let resp = heartbeat(beat(json!({ "client": "c".repeat(65) })))
        .await
        .unwrap();
    refused(resp).await;
    let resp = heartbeat(beat(json!({ "client": "co\u{0}dex" })))
        .await
        .unwrap();
    refused(resp).await;

    // topic: at most 200 bytes.
    let new_channel = |name: &str, topic: &str| {
        node.post("/api/channels")
            .json(&json!({ "name": name, "topic": topic }))
            .send()
    };
    ok(new_channel("a", &"t".repeat(200)).await.unwrap()).await;
    let message = refused(new_channel("b", &"t".repeat(201)).await.unwrap()).await;
    assert!(message.contains("topic"), "{message}");
    refused(new_channel("b", &"é".repeat(101)).await.unwrap()).await;
    refused(new_channel("b", "line\nbreak").await.unwrap()).await;
    // Setting a bad topic on a channel that exists changes nothing either.
    refused(new_channel("a", &"u".repeat(201)).await.unwrap()).await;
    let resp = node.get("/api/channels").send().await.unwrap();
    let channels: ChannelsJson = serde_json::from_value(ok(resp).await).unwrap();
    let names: Vec<&str> = channels.channels.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["general", "a"]);
    assert_eq!(channels.channels[1].topic, Some("t".repeat(200)));
}

#[tokio::test]
async fn only_beacon_human_posts_as_human() {
    let node = spawn_node("pc", &[]).await; // BEACON_HUMAN defaults to Dang
    let as_human = |author: &str| {
        let mut p = post_meta("announcement");
        p["author"] = json!(author);
        p["author_kind"] = json!("human");
        p
    };

    let resp = post_json(&node, "general", &as_human("pc-configs")).await;
    let message = expect_error(resp, StatusCode::BAD_REQUEST, "usage").await;
    assert_eq!(message, "only BEACON_HUMAN can post as human");
    assert!(channel_bodies(&node, "general").await.is_empty());

    // BEACON_HUMAN may, in any letter case.
    for author in ["Dang", "dang", "DANG"] {
        let created = ok(post_json(&node, "general", &as_human(author)).await).await;
        assert_eq!(created["post"]["announcement"], true, "{author}");
    }
    // The same rule holds for the multipart form.
    let resp = upload(
        &node,
        "general",
        &as_human("mac-juicebox"),
        "a.txt",
        vec![1],
    )
    .await;
    let message = expect_error(resp, StatusCode::BAD_REQUEST, "usage").await;
    assert_eq!(message, "only BEACON_HUMAN can post as human");
    ok(upload(&node, "general", &as_human("dang"), "a.txt", vec![1]).await).await;

    // The human is whoever this node is configured with.
    let node = spawn_node_with("pc", &[], |cfg| cfg.human = "Alice".into()).await;
    let resp = post_json(&node, "general", &as_human("Dang")).await;
    expect_error(resp, StatusCode::BAD_REQUEST, "usage").await;
    ok(post_json(&node, "general", &as_human("alice")).await).await;

    // Events from peers are not held to it: another device has its own human.
    let id = ulid::Ulid::generate().to_string();
    let remote = Event {
        origin: "mac~abcd1234".into(),
        seq: 1,
        id: id.clone(),
        ts: 1_000,
        kind: EventKind::PostCreated(NewPost {
            post_id: id,
            channel: "general".into(),
            author: "Dang".into(),
            author_kind: AuthorKind::Human,
            client: Some("tui".into()),
            node: "mac".into(),
            body: "from the mac".into(),
            tags: vec![],
            mentions: vec![],
            attachment: None,
        }),
    };
    assert!(node.state().ingest(remote).await.unwrap().is_some());
    // (Its ts is older, so it sorts first.)
    assert_eq!(
        channel_bodies(&node, "general").await,
        ["from the mac", "announcement"]
    );
}

#[tokio::test]
async fn commit_if_stamps_the_event_after_taking_the_lock() {
    let node = spawn_node("pc", &[]).await;
    let state = node.state().clone();
    // Hold the store for 300 ms, then commit while it is held.
    let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
    let holder = tokio::spawn({
        let state = state.clone();
        async move {
            state
                .with_store(move |_| {
                    let _ = locked_tx.send(());
                    std::thread::sleep(Duration::from_millis(300));
                    Ok(())
                })
                .await
        }
    });
    locked_rx.await.unwrap();
    let asked_at = now_ms();

    let seen = Arc::new(Mutex::new(None));
    let record = Arc::clone(&seen);
    let kind = EventKind::ChannelCreated {
        name: "late".into(),
        topic: None,
    };
    let applied = state
        .commit_if(kind, move |_, now| {
            *record.lock().unwrap() = Some(now);
            Ok(true)
        })
        .await
        .unwrap()
        .expect("committed");
    holder.await.unwrap().unwrap();

    // The time handed to `when` and carried by the event is the time the lock
    // was taken, not the time the request came in.
    let now = seen.lock().unwrap().expect("when ran");
    assert_eq!(applied.event.ts, now);
    assert!(
        now >= asked_at + 200,
        "stamped {}ms after the request, before the lock was free",
        now - asked_at
    );
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
