//! The attachment store on its own: streaming writes, the size limit,
//! verified writes for sync, and what is left on disk when a transfer fails.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use beacon::blobs::{is_sha256, Blobs, PutError};
use beacon::error::Error;
use bytes::Bytes;
use common::*;
use futures_util::stream::{self, Stream, StreamExt};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::io::AsyncReadExt;

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `data` as a stream of chunks of `size` bytes.
fn chunked(data: &[u8], size: usize) -> impl Stream<Item = Result<Bytes, Error>> + Send + 'static {
    let chunks: Vec<Result<Bytes, Error>> = data
        .chunks(size.max(1))
        .map(|c| Ok(Bytes::copy_from_slice(c)))
        .collect();
    stream::iter(chunks)
}

/// The names in `dir`, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn store() -> (TempDir, Blobs) {
    let dir = TempDir::new().unwrap();
    let blobs = Blobs::new(dir.path());
    (dir, blobs)
}

async fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    let start = Instant::now();
    while !check() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "gave up waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[test]
fn only_lowercase_hex_of_the_right_length_is_a_sha256() {
    assert!(is_sha256(&"ab".repeat(32)));
    assert!(is_sha256(&"0123456789abcdef".repeat(4)));
    for bad in [
        String::new(),
        "ab".repeat(31),
        "ab".repeat(33),
        "AB".repeat(32),
        "ag".repeat(32),
        format!("{}/", "a".repeat(63)),
        format!("../{}", "a".repeat(61)),
        format!("{} ", "a".repeat(63)),
        format!("{}\n", "a".repeat(63)),
    ] {
        assert!(!is_sha256(&bad), "{bad:?}");
    }
}

#[tokio::test]
async fn put_stream_stores_a_blob_under_its_hash() {
    let (dir, blobs) = store();
    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let (sha, len) = blobs
        .put_stream(chunked(&data, 65_536), 1 << 20)
        .await
        .unwrap();
    assert_eq!(sha, sha256_hex(&data));
    assert_eq!(len, 300_000);
    assert!(blobs.has(&sha));
    assert_eq!(std::fs::read(blobs.path(&sha)).unwrap(), data);
    assert_eq!(names(dir.path()), std::slice::from_ref(&sha));

    // The same bytes again change nothing.
    let again = blobs
        .put_stream(chunked(&data, 1000), 1 << 20)
        .await
        .unwrap();
    assert_eq!(again, (sha.clone(), 300_000));
    assert_eq!(names(dir.path()), [sha]);
}

#[tokio::test]
async fn put_stream_takes_an_empty_file() {
    let (dir, blobs) = store();
    let (sha, len) = blobs
        .put_stream(stream::empty::<Result<Bytes, Error>>(), 10)
        .await
        .unwrap();
    assert_eq!(len, 0);
    assert_eq!(sha, sha256_hex(b""));
    assert!(blobs.has(&sha));
    assert_eq!(names(dir.path()), [sha]);
}

#[tokio::test]
async fn put_stream_refuses_more_than_the_limit_and_cleans_up() {
    let (dir, blobs) = store();
    let data = vec![9u8; 1001];
    let err = blobs
        .put_stream(chunked(&data, 100), 1000)
        .await
        .unwrap_err();
    assert!(
        matches!(err, PutError::TooLarge { max_bytes: 1000 }),
        "{err:?}"
    );
    assert!(names(dir.path()).is_empty(), "no temp file left");

    // Exactly the limit is fine.
    blobs
        .put_stream(chunked(&data[..1000], 100), 1000)
        .await
        .unwrap();
    assert_eq!(names(dir.path()).len(), 1);

    // It stops reading once over the limit instead of draining the stream.
    let (dir, blobs) = store();
    let endless = stream::repeat_with(|| Ok::<_, Error>(Bytes::from_static(&[0u8; 4096])));
    let err = blobs.put_stream(endless, 100_000).await.unwrap_err();
    assert!(matches!(err, PutError::TooLarge { .. }));
    assert!(names(dir.path()).is_empty());
}

#[test]
fn the_limit_error_says_it_in_megabytes() {
    let err = PutError::TooLarge { max_bytes: 5 << 20 };
    assert_eq!(err.to_string(), "attachment is over 5 MB");
    match Error::from(err) {
        Error::Usage(message) => assert_eq!(message, "attachment is over 5 MB"),
        other => panic!("expected a usage error, got {other:?}"),
    }
}

#[tokio::test]
async fn put_stream_removes_its_temp_file_when_the_stream_fails() {
    let (dir, blobs) = store();
    let parts: Vec<Result<Bytes, Error>> = vec![
        Ok(Bytes::from_static(&[1u8; 5000])),
        Ok(Bytes::from_static(&[2u8; 5000])),
        Err(Error::Unreachable("connection lost".into())),
        Ok(Bytes::from_static(&[3u8; 5000])),
    ];
    let err = blobs
        .put_stream(stream::iter(parts), 1 << 20)
        .await
        .unwrap_err();
    match err {
        PutError::Failed(Error::Unreachable(message)) => assert_eq!(message, "connection lost"),
        other => panic!("unexpected {other:?}"),
    }
    assert!(names(dir.path()).is_empty());
}

#[tokio::test]
async fn dropping_an_upload_midway_removes_its_temp_file() {
    let (dir, blobs) = store();
    let first = stream::iter([Ok::<_, Error>(Bytes::from_static(&[1u8; 4096]))]);
    let stalled = first.chain(stream::pending());
    let task = tokio::spawn({
        let blobs = blobs.clone();
        async move { blobs.put_stream(stalled, 1 << 20).await }
    });
    // The upload is under way: its temp file is on disk.
    wait_until("the temp file", || names(dir.path()).len() == 1).await;
    // Hanging up cancels the future where it stands.
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(names(dir.path()).is_empty());
}

#[tokio::test]
async fn temp_files_never_look_like_blobs() {
    let (dir, blobs) = store();
    let first = stream::iter([Ok::<_, Error>(Bytes::from_static(&[1u8; 100]))]);
    let task = tokio::spawn({
        let blobs = blobs.clone();
        async move {
            blobs
                .put_stream(first.chain(stream::pending()), 1 << 20)
                .await
        }
    });
    wait_until("the temp file", || names(dir.path()).len() == 1).await;
    let temp = names(dir.path()).remove(0);
    assert!(!is_sha256(&temp), "{temp}");
    assert!(!blobs.has(&temp));
    task.abort();
}

#[tokio::test]
async fn clear_temp_removes_leftovers_and_keeps_blobs() {
    let (dir, blobs) = store();
    let (sha, _) = blobs.put_stream(chunked(b"keep me", 3), 100).await.unwrap();
    // What a process killed mid-upload leaves behind.
    let first = stream::iter([Ok::<_, Error>(Bytes::from_static(&[1u8; 100]))]);
    let task = tokio::spawn({
        let blobs = blobs.clone();
        async move {
            blobs
                .put_stream(first.chain(stream::pending()), 1 << 20)
                .await
        }
    });
    wait_until("the temp file", || names(dir.path()).len() == 2).await;
    task.abort();
    let _ = task.await;
    std::fs::write(dir.path().join(".tmp-stale"), b"left over").unwrap();

    blobs.clear_temp();
    assert_eq!(names(dir.path()), [sha]);
}

#[tokio::test]
async fn put_verified_accepts_the_bytes_it_was_promised() {
    let (dir, blobs) = store();
    let data = b"the build log from the mac".to_vec();
    let sha = sha256_hex(&data);
    assert!(!blobs.has(&sha));
    blobs
        .put_verified(&sha, chunked(&data, 5), data.len() as u64)
        .await
        .unwrap();
    assert!(blobs.has(&sha));
    assert_eq!(std::fs::read(blobs.path(&sha)).unwrap(), data);
    assert_eq!(names(dir.path()), std::slice::from_ref(&sha));

    // Asking again is a no-op, and does not need the stream.
    blobs
        .put_verified(&sha, stream::empty::<Result<Bytes, Error>>(), 0)
        .await
        .unwrap();
    assert_eq!(std::fs::read(blobs.path(&sha)).unwrap(), data);
}

#[tokio::test]
async fn put_verified_reads_no_more_than_it_was_told() {
    let (dir, blobs) = store();
    let data = b"exactly twenty bytes".to_vec();
    let sha = sha256_hex(&data);
    // A peer that sends more than the size it announced is cut off at it,
    // and nothing is kept, even though the bytes would have matched.
    let err = blobs
        .put_verified(&sha, chunked(&data, 3), 19)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Usage(_)), "{err:?}");
    assert!(err.to_string().contains("over 19 bytes"), "{err}");
    assert!(!blobs.has(&sha));
    assert!(names(dir.path()).is_empty());

    // The stream is not read past the bound: a source that would go on
    // forever is cut off too.
    let endless = stream::iter(std::iter::repeat_with(|| {
        Ok::<_, Error>(Bytes::from_static(b"0123456789"))
    }));
    let err = blobs.put_verified(&sha, endless, 1000).await.unwrap_err();
    assert!(matches!(err, Error::Usage(_)), "{err:?}");
    assert!(names(dir.path()).is_empty());

    // Exactly the bound is fine.
    blobs
        .put_verified(&sha, chunked(&data, 3), 20)
        .await
        .unwrap();
    assert!(blobs.has(&sha));
}

#[tokio::test]
async fn put_verified_rejects_bytes_that_do_not_match() {
    let (dir, blobs) = store();
    let promised = sha256_hex(b"what the peer announced");
    let err = blobs
        .put_verified(&promised, chunked(b"something else entirely", 4), 1000)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Usage(_)), "{err:?}");
    assert!(!blobs.has(&promised));
    // Nothing is kept, not even under the hash of what was received.
    assert!(names(dir.path()).is_empty());

    // A stream that breaks is an error too, and leaves nothing.
    let parts: Vec<Result<Bytes, Error>> = vec![
        Ok(Bytes::from_static(b"what the peer")),
        Err(Error::Unreachable("reset".into())),
    ];
    let err = blobs
        .put_verified(&promised, stream::iter(parts), 1000)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unreachable(_)), "{err:?}");
    assert!(names(dir.path()).is_empty());
}

#[tokio::test]
async fn names_that_are_not_a_sha_never_reach_the_disk() {
    let dir = TempDir::new().unwrap();
    let files = dir.path().join("files");
    std::fs::create_dir(&files).unwrap();
    std::fs::write(dir.path().join("secret"), b"outside").unwrap();
    let blobs = Blobs::new(&files);

    let bad = [
        "../secret",
        "..",
        ".",
        "",
        "/etc/passwd",
        "a/b",
        "..\\secret",
        &"A".repeat(64),
        &"a".repeat(63),
        &"g".repeat(64),
    ];
    for name in bad {
        assert!(!blobs.has(name), "{name:?}");
        let path = blobs.path(name);
        assert_eq!(path.parent(), Some(files.as_path()), "{name:?} -> {path:?}");
        assert!(!path.exists(), "{name:?} -> {path:?}");
        assert!(blobs.open(name).await.unwrap().is_none(), "{name:?}");
        let err = blobs
            .put_verified(name, chunked(b"x", 1), 1)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Usage(_)), "{name:?}: {err:?}");
    }
    assert!(names(&files).is_empty());
    assert_eq!(names(dir.path()), ["files", "secret"]);
}

#[tokio::test]
async fn open_gives_the_file_and_its_length() {
    let (_dir, blobs) = store();
    let (sha, _) = blobs
        .put_stream(chunked(b"0123456789", 4), 100)
        .await
        .unwrap();
    let (mut file, len) = blobs.open(&sha).await.unwrap().expect("held");
    assert_eq!(len, 10);
    let mut text = String::new();
    file.read_to_string(&mut text).await.unwrap();
    assert_eq!(text, "0123456789");
    assert!(blobs.open(&"cd".repeat(32)).await.unwrap().is_none());
}

#[tokio::test]
async fn a_peers_attachment_is_unavailable_until_its_bytes_arrive() {
    use beacon::api::PostJson;
    use beacon::model::{Attachment, AuthorKind, Event, EventKind, NewPost};

    let node = spawn_node("pc", &[]).await;
    let data = b"log from the mac".to_vec();
    let sha = sha256_hex(&data);
    let id = ulid::Ulid::generate().to_string();
    let event = Event {
        origin: "mac~abcd1234".into(),
        seq: 1,
        id: id.clone(),
        ts: 1_000,
        kind: EventKind::PostCreated(NewPost {
            post_id: id.clone(),
            channel: "general".into(),
            author: "mac-juicebox".into(),
            author_kind: AuthorKind::Agent,
            client: None,
            node: "mac".into(),
            body: "see the log".into(),
            tags: vec![],
            mentions: vec![],
            attachment: Some(Attachment {
                sha256: sha.clone(),
                name: "mac.log".into(),
                size: data.len() as u64,
            }),
        }),
    };
    node.state().ingest(event).await.unwrap().expect("new");

    let available = |node: &TestNode| {
        let path = format!("/api/posts/{id}");
        let req = node.get(&path);
        async move {
            let post: PostJson = req.send().await.unwrap().json().await.unwrap();
            post.attachment.expect("an attachment").available
        }
    };
    assert!(!available(&node).await);
    let resp = node.get(&format!("/api/files/{sha}")).send().await.unwrap();
    assert_eq!(resp.status(), 404);

    // Bytes that do not match are refused and the post stays unavailable.
    let blobs = &node.state().blobs;
    assert!(blobs
        .put_verified(&sha, chunked(b"tampered", 3), 100)
        .await
        .is_err());
    assert!(!available(&node).await);
    assert!(names(&node.dir.path().join("files")).is_empty());

    // The right ones make it available, and downloadable.
    blobs
        .put_verified(&sha, chunked(&data, 3), data.len() as u64)
        .await
        .unwrap();
    assert!(available(&node).await);
    let resp = node.get(&format!("/api/files/{sha}")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), data.as_slice());
}
