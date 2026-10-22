//! The API handlers. Each one checks its input, makes one or two calls on the
//! store (or [`AppState::commit`] for a write) and shapes the answer.
//!
//! Posts, channels and presence are always rendered from derived state (the
//! rows the store returns), never from raw events; the one exception is the
//! answer to a post this node has just committed, whose event was validated
//! on the way in.

use axum::body::Body;
use axum::extract::multipart::Field;
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::{delete, get};
use axum::{Json, Router};
use futures_util::{StreamExt, TryStreamExt};
use serde::Deserialize;
use tokio_util::io::ReaderStream;

use super::{now_ms, ApiFailure, ApiJson, ApiPath, ApiQuery, ApiResult, AppState};
use crate::api::*;
use crate::blobs::{is_sha256, PutError};
use crate::error::Error;
use crate::model::{
    normalize_channel, parse_mentions, validate_attachment_name, validate_body_text,
    validate_client, validate_handle, validate_status, validate_tag, validate_tag_count,
    validate_topic, Attachment, AuthorKind, EventKind, NewPost, Presence, BODY_MAX,
};
use crate::store::{Applied, PostQuery, PostRow, Store};

/// Posts per page when a request does not say.
const DEFAULT_LIMIT: usize = 50;
/// The most posts a request may ask for.
const MAX_LIMIT: usize = 500;
/// Largest JSON request body. (Axum's own default, applied by hand because
/// the posting route lifts the default for uploads.)
const JSON_BODY_MAX: u64 = 2 * 1024 * 1024;
/// Largest `meta` part of an upload.
const META_MAX: usize = 256 * 1024;
/// Room in an upload's body for the multipart framing around its parts.
const FRAMING_MAX: u64 = 64 * 1024;
/// How much of a file is read from disk at a time when serving it.
const FILE_CHUNK: usize = 64 * 1024;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/status", get(status))
        .route("/api/peers", get(list_peers))
        .route("/api/channels", get(list_channels).post(create_channel))
        .route("/api/channels/{name}", delete(delete_channel))
        .route(
            "/api/channels/{name}/posts",
            // A post is JSON or a multipart upload. `create_post` limits the
            // body itself, by kind: the upload limit is the node's setting.
            get(channel_posts)
                .post(create_post)
                .layer(DefaultBodyLimit::disable()),
        )
        .route("/api/files/{sha256}", get(get_file))
        .route("/api/posts", get(cross_channel_posts))
        .route("/api/posts/{id}", get(get_post).delete(delete_post))
        .route("/api/who", get(who).post(heartbeat))
        .route("/api/who/{handle}", delete(forget))
        .route("/api/joins/{handle}", get(get_joins).put(put_joins))
        .route(
            "/api/read-cursors/{handle}",
            get(get_cursors).put(put_cursor),
        )
        .route("/api/unread/{handle}", get(unread))
}

// ----------------------------------------------------------------- helpers

/// A page size: 50 by default, at least 1, at most 500 (more is cut to 500).
pub(super) fn page_limit(limit: Option<u32>) -> Result<usize, Error> {
    match limit {
        None => Ok(DEFAULT_LIMIT),
        Some(0) => Err(Error::Usage("limit must be at least 1".into())),
        Some(n) => Ok((n as usize).min(MAX_LIMIT)),
    }
}

/// A yes/no query parameter: `1`/`true`/`yes` or `0`/`false`/`no`.
pub(super) fn flag(name: &str, value: Option<&str>) -> Result<bool, Error> {
    match value {
        None | Some("0" | "false" | "no" | "") => Ok(false),
        Some("1" | "true" | "yes") => Ok(true),
        Some(other) => Err(Error::Usage(format!(
            "{name} must be 1 or 0, not {other:?}"
        ))),
    }
}

/// `<ts_ms>:<id>`, the place a page of posts ends.
fn parse_before(before: &str) -> Result<(i64, String), Error> {
    let bad = || {
        Error::Usage(format!(
            "before must look like <ts_ms>:<post id>, not {before:?}"
        ))
    };
    let (ts, id) = before.split_once(':').ok_or_else(bad)?;
    let ts = ts.parse().map_err(|_| bad())?;
    if id.is_empty() {
        return Err(bad());
    }
    Ok((ts, id.to_string()))
}

/// Trims text and turns blank into `None`.
fn clean(text: Option<String>) -> Option<String> {
    text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

pub(super) fn posts_json(
    app: &AppState,
    rows: &[PostRow],
    remaining: Option<u32>,
) -> PostsResponse {
    PostsResponse {
        v: V,
        posts: rows
            .iter()
            .map(|p| PostJson::from_row(p, &app.blobs))
            .collect(),
        remaining,
    }
}

fn no_such_channel(name: &str) -> Error {
    Error::NotFound(format!("no channel #{name}"))
}

fn require_channel(store: &Store, name: &str) -> Result<(), Error> {
    store
        .channel(name)?
        .map(drop)
        .ok_or_else(|| no_such_channel(name))
}

// ------------------------------------------------------------ health, status

async fn health(State(app): State<AppState>) -> Json<HealthJson> {
    Json(HealthJson {
        v: V,
        node: app.cfg.node.clone(),
        ok: true,
    })
}

async fn status(State(app): State<AppState>) -> ApiResult<StatusJson> {
    let have = app.with_store(|s| Ok(s.version_vector())).await?;
    Ok(Json(StatusJson {
        v: V,
        node: app.cfg.node.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        peers: app.sync.peers(),
        same_name: app.sync.same_name(),
        have,
    }))
}

/// `GET /api/peers`: the peers this node syncs with and how each is doing.
async fn list_peers(State(app): State<AppState>) -> Json<PeersJson> {
    Json(PeersJson {
        v: V,
        peers: app.sync.peers(),
        same_name: app.sync.same_name(),
    })
}

// ---------------------------------------------------------------- channels

#[derive(Deserialize)]
struct ChannelsQuery {
    handle: Option<String>,
    relevant: Option<String>,
}

/// `GET /api/channels`: the visible channels. With a `handle`, each carries
/// how many posts are new to it: every post by someone else (the TUI's
/// badges), or with `relevant=1` only the ones `/api/unread` would list,
/// which is what an agent can clear by reading.
async fn list_channels(
    State(app): State<AppState>,
    ApiQuery(q): ApiQuery<ChannelsQuery>,
) -> ApiResult<ChannelsJson> {
    let handle = q.handle.as_deref().map(validate_handle).transpose()?;
    let relevant = flag("relevant", q.relevant.as_deref())?;
    if relevant && handle.is_none() {
        return Err(Error::Usage("relevant=1 needs a handle".into()).into());
    }
    let (rows, unread) = app
        .with_store(move |s| {
            let rows = s.channels()?;
            let unread = match &handle {
                Some(h) if relevant => {
                    Some((s.unread_relevant_counts(h)?, s.unread_announcements(h)?))
                }
                Some(h) => Some((s.unread_counts(h)?, s.unread_announcements(h)?)),
                None => None,
            };
            Ok((rows, unread))
        })
        .await?;
    let channels = rows
        .iter()
        .map(|row| {
            let n = unread
                .as_ref()
                .map(|(counts, _)| counts.get(&row.name).copied().unwrap_or(0));
            ChannelJson::from_row(row, n)
        })
        .collect();
    Ok(Json(ChannelsJson {
        v: V,
        channels,
        announcements_unread: unread.map(|(_, announcements)| announcements),
    }))
}

async fn create_channel(
    State(app): State<AppState>,
    ApiJson(req): ApiJson<NewChannelRequest>,
) -> ApiResult<ChannelAck> {
    let name = normalize_channel(&req.name)?;
    let topic = clean(req.topic);
    if let Some(topic) = &topic {
        validate_topic(topic)?;
    }
    let lookup = name.clone();
    let existing = app.with_store(move |s| s.channel(&lookup)).await?;
    // Asking again for a channel that is there changes nothing, unless it
    // brings a different topic.
    let needs_event = match &existing {
        None => true,
        Some(row) => topic.is_some() && topic != row.topic,
    };
    if needs_event {
        let kind = EventKind::ChannelCreated {
            name: name.clone(),
            topic,
        };
        app.commit(kind).await?;
    }
    let row = app
        .with_store(move |s| {
            s.channel(&name)?
                .ok_or_else(|| Error::Internal(anyhow::anyhow!("channel {name} vanished")))
        })
        .await?;
    Ok(Json(ChannelAck {
        v: V,
        ok: true,
        created: existing.is_none(),
        channel: ChannelJson::from_row(&row, None),
    }))
}

async fn delete_channel(
    State(app): State<AppState>,
    ApiPath(name): ApiPath<String>,
) -> ApiResult<DeleteAck> {
    let name = normalize_channel(&name)?;
    let lookup = name.clone();
    let deleted = app
        .commit_if(
            EventKind::ChannelDeleted { name: name.clone() },
            move |s, _| Ok(s.channel(&lookup)?.is_some()),
        )
        .await?;
    if deleted.is_none() {
        return Err(no_such_channel(&name).into());
    }
    Ok(Json(DeleteAck {
        v: V,
        ok: true,
        deleted: name,
    }))
}

// ------------------------------------------------------------------- posts

#[derive(Deserialize)]
struct ChannelPostsQuery {
    before: Option<String>,
    limit: Option<u32>,
    tag: Option<String>,
}

async fn channel_posts(
    State(app): State<AppState>,
    ApiPath(name): ApiPath<String>,
    ApiQuery(q): ApiQuery<ChannelPostsQuery>,
) -> ApiResult<PostsResponse> {
    let query = PostQuery {
        channel: Some(normalize_channel(&name)?),
        before: q.before.as_deref().map(parse_before).transpose()?,
        limit: page_limit(q.limit)?,
        tag: q.tag.as_deref().map(validate_tag).transpose()?,
        ..PostQuery::default()
    };
    let rows = app
        .with_store(move |s| {
            if let Some(channel) = &query.channel {
                require_channel(s, channel)?;
            }
            s.posts(&query)
        })
        .await?;
    Ok(Json(posts_json(&app, &rows, None)))
}

#[derive(Deserialize)]
struct CrossChannelQuery {
    mentions: Option<String>,
    announcements: Option<String>,
    tag: Option<String>,
    limit: Option<u32>,
}

async fn cross_channel_posts(
    State(app): State<AppState>,
    ApiQuery(q): ApiQuery<CrossChannelQuery>,
) -> ApiResult<PostsResponse> {
    let query = PostQuery {
        limit: page_limit(q.limit)?,
        tag: q.tag.as_deref().map(validate_tag).transpose()?,
        mentions_of: q.mentions.as_deref().map(validate_handle).transpose()?,
        announcements_only: flag("announcements", q.announcements.as_deref())?,
        ..PostQuery::default()
    };
    let rows = app.with_store(move |s| s.posts(&query)).await?;
    Ok(Json(posts_json(&app, &rows, None)))
}

async fn create_post(
    State(app): State<AppState>,
    ApiPath(name): ApiPath<String>,
    req: Request,
) -> ApiResult<PostCreated> {
    let multipart = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.trim_start()
                .to_ascii_lowercase()
                .starts_with("multipart/form-data")
        });
    let created = if multipart {
        post_with_upload(&app, &name, req).await?
    } else {
        let req = limit_body(req, JSON_BODY_MAX);
        let ApiJson(body) = ApiJson::<NewPostRequest>::from_request(req, &app).await?;
        post_to_channel(&app, &name, body, None).await?
    };
    Ok(Json(created))
}

/// `req` with its body cut off after `max` bytes (reading further fails with
/// a length-limit error, which the extractors answer with 413).
fn limit_body(req: Request, max: u64) -> Request {
    let max = usize::try_from(max).unwrap_or(usize::MAX);
    req.map(|body| Body::new(http_body_util::Limited::new(body, max)))
}

/// The multipart form of the posting route: a `meta` part (the JSON post
/// request), then optionally a `file` part whose file name is the
/// attachment's name. The file is streamed to disk, never held in memory.
///
/// `meta` must come first, so the post can be checked before a single byte
/// of the file is kept: a request that would be refused is refused unread.
async fn post_with_upload(
    app: &AppState,
    name: &str,
    req: Request,
) -> Result<PostCreated, ApiFailure> {
    let max_bytes = app.cfg.max_attachment_bytes();
    let body_max = max_bytes.saturating_add(META_MAX as u64 + FRAMING_MAX);
    let mut form = Multipart::from_request(limit_body(req, body_max), app).await?;

    let mut meta: Option<NewPostRequest> = None;
    let mut attachment: Option<Attachment> = None;
    while let Some(field) = form.next_field().await? {
        match (field.name(), &meta, &attachment) {
            (Some("meta"), None, None) => meta = Some(read_meta(field).await?),
            (Some("file"), Some(meta), None) => {
                let name_in_form = attachment_name(field.file_name())?;
                // Everything that can be checked without the file, now.
                prepare_post(app, name, meta, true).await?;
                attachment = Some(store_upload(app, field, name_in_form, max_bytes).await?);
            }
            (Some("file"), None, None) => {
                return Err(usage("send the meta part before the file part"));
            }
            (Some("meta" | "file"), _, _) => {
                return Err(usage(
                    "a post takes one meta part, then at most one file part",
                ));
            }
            (other, _, _) => {
                return Err(usage(format!("unexpected part {other:?} in the upload")));
            }
        }
    }
    let meta = meta.ok_or_else(|| usage("the upload needs a meta part"))?;
    post_to_channel(app, name, meta, attachment).await
}

fn usage(message: impl Into<String>) -> ApiFailure {
    Error::Usage(message.into()).into()
}

/// The `meta` part: the same JSON the plain form takes.
async fn read_meta(mut field: Field<'_>) -> Result<NewPostRequest, ApiFailure> {
    let mut json = Vec::new();
    while let Some(chunk) = field.chunk().await? {
        if json.len() + chunk.len() > META_MAX {
            return Err(ApiFailure::too_large(format!(
                "the meta part is over {} KB",
                META_MAX / 1024
            )));
        }
        json.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&json)
        .map_err(|e| usage(format!("the meta part is not a post request: {e}")))
}

/// Streams a `file` part into the blob store.
async fn store_upload(
    app: &AppState,
    field: Field<'_>,
    name: String,
    max_bytes: u64,
) -> Result<Attachment, ApiFailure> {
    let chunks = field.map_err(move |e| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            PutError::TooLarge { max_bytes }
        } else {
            PutError::Failed(Error::Usage(format!(
                "upload interrupted: {}",
                e.body_text()
            )))
        }
    });
    let (sha256, size) = app.blobs.put_stream(chunks, max_bytes).await?;
    Ok(Attachment { sha256, name, size })
}

/// The attachment name for a file name from a form: just the file name, in
/// case the client sent a path.
fn attachment_name(raw: Option<&str>) -> Result<String, Error> {
    let raw = raw.ok_or_else(|| Error::Usage("the file part needs a file name".into()))?;
    let name = raw.rsplit(['/', '\\']).next().unwrap_or(raw).trim();
    if matches!(name, "" | "." | "..") {
        return Err(Error::Usage(format!(
            "{raw:?} is not a usable attachment name"
        )));
    }
    validate_attachment_name(name)?;
    Ok(name.to_string())
}

/// A post request that passed every check that needs no write.
struct Prepared {
    channel: String,
    author: String,
    tags: Vec<String>,
    client: Option<String>,
    channel_exists: bool,
}

/// Checks a post request and looks the channel up, changing nothing. The
/// upload path runs it before it reads the file, and [`post_to_channel`] runs
/// it again before it writes. `attached`: the post carries a file, so an
/// empty body is fine.
async fn prepare_post(
    app: &AppState,
    name: &str,
    req: &NewPostRequest,
    attached: bool,
) -> Result<Prepared, ApiFailure> {
    let channel = normalize_channel(name)?;
    let author = validate_handle(&req.author)?;
    if req.author_kind == AuthorKind::Human && !author.eq_ignore_ascii_case(&app.cfg.human) {
        return Err(usage("only BEACON_HUMAN can post as human"));
    }
    let client = clean(req.client.clone());
    if let Some(client) = &client {
        validate_client(client)?;
    }
    let mut tags: Vec<String> = Vec::new();
    for tag in &req.tags {
        let tag = validate_tag(tag)?;
        if !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    validate_tag_count(tags.len())?;
    if req.body.len() > BODY_MAX {
        return Err(ApiFailure::too_large(format!(
            "post body is {} bytes, over the {BODY_MAX}-byte limit",
            req.body.len()
        )));
    }
    validate_body_text(&req.body)?;
    if req.body.trim().is_empty() && !attached {
        return Err(usage("the post is empty: give it some text"));
    }

    let lookup = channel.clone();
    let channel_exists = app.with_store(move |s| s.channel(&lookup)).await?.is_some();
    if !channel_exists && !req.create {
        return Err(Error::NotFound(format!(
            "no channel #{channel}: create it with `./beacon channel new {channel}`, \
             or post with --create"
        ))
        .into());
    }
    Ok(Prepared {
        channel,
        author,
        tags,
        client,
        channel_exists,
    })
}

/// Posts `req` (with `attachment`, if the bytes were already stored) to the
/// channel `name`. The JSON and the multipart forms of the route share this.
pub async fn post_to_channel(
    app: &AppState,
    name: &str,
    req: NewPostRequest,
    attachment: Option<Attachment>,
) -> Result<PostCreated, ApiFailure> {
    let prepared = prepare_post(app, name, &req, attachment.is_some()).await?;
    if !prepared.channel_exists {
        let kind = EventKind::ChannelCreated {
            name: prepared.channel.clone(),
            topic: None,
        };
        app.commit(kind).await?;
    }

    let post = NewPost {
        post_id: String::new(), // the event's id becomes the post's
        channel: prepared.channel,
        author: prepared.author,
        author_kind: req.author_kind,
        client: prepared.client,
        node: app.cfg.node.clone(),
        mentions: parse_mentions(&req.body),
        body: req.body,
        tags: prepared.tags,
        attachment,
    };
    let applied = app.commit(EventKind::PostCreated(post)).await?;
    let row = committed_post(&applied)?;
    let delivery = app.sync.push_now(&applied.event).await;
    Ok(PostCreated {
        v: V,
        post: PostJson::from_row(&row, &app.blobs),
        delivery,
    })
}

/// The post a `post_created` event this node just committed made. Safe to
/// render without asking the store: [`AppState::commit`] validated it.
fn committed_post(applied: &Applied) -> Result<PostRow, Error> {
    PostRow::from_applied(applied).ok_or_else(|| {
        Error::Internal(anyhow::anyhow!(
            "committed event {} is not a post",
            applied.event.id
        ))
    })
}

// ------------------------------------------------------------- attachments

/// `GET /api/files/:sha256`: the attachment's bytes, streamed from disk.
async fn get_file(
    State(app): State<AppState>,
    ApiPath(sha256): ApiPath<String>,
) -> Result<Response, ApiFailure> {
    if !is_sha256(&sha256) {
        return Err(usage(format!(
            "{sha256:?} is not a sha256: expected 64 lowercase hex digits"
        )));
    }
    let Some((file, len)) = app.blobs.open(&sha256).await? else {
        return Err(Error::NotFound(format!("no attachment {sha256} on this node")).into());
    };
    // A download does not outlive the node.
    let shutdown = app.shutdown.clone();
    let chunks = ReaderStream::with_capacity(file, FILE_CHUNK)
        .take_until(async move { shutdown.triggered().await });
    let mut response = Response::new(Body::from_stream(chunks));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

async fn get_post(
    State(app): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<PostJson> {
    let row = app.with_store(move |s| s.post(&id)).await?;
    Ok(Json(PostJson::from_row(&row, &app.blobs)))
}

#[derive(Deserialize)]
struct DeletePostQuery {
    by: Option<String>,
}

async fn delete_post(
    State(app): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiQuery(q): ApiQuery<DeletePostQuery>,
) -> ApiResult<DeleteAck> {
    // Without `by`, the person at the keyboard (BEACON_HUMAN) is deleting.
    let by = match q.by.as_deref() {
        Some(by) => validate_handle(by)?,
        None => app.cfg.human.clone(),
    };
    let row = app.with_store(move |s| s.post(&id)).await?;
    app.commit(EventKind::PostDeleted {
        post_id: row.id.clone(),
        by,
    })
    .await?;
    Ok(Json(DeleteAck {
        v: V,
        ok: true,
        deleted: row.id,
    }))
}

// ---------------------------------------------------------------- presence

async fn who(State(app): State<AppState>) -> ApiResult<WhoJson> {
    let now = now_ms();
    let rows = app.with_store(move |s| s.who(now)).await?;
    Ok(Json(WhoJson {
        v: V,
        agents: rows
            .iter()
            .map(|r| PresenceJson::from_row(r, now))
            .collect(),
    }))
}

async fn heartbeat(
    State(app): State<AppState>,
    ApiJson(req): ApiJson<WhoRequest>,
) -> ApiResult<WhoAck> {
    let presence = Presence {
        handle: validate_handle(&req.handle)?,
        kind: req.kind,
        client: clean(Some(req.client)).unwrap_or_else(|| "other".to_string()),
        node: app.cfg.node.clone(),
        status: clean(req.status),
    };
    // Checked here, not left to the write: a heartbeat that is not due is
    // dropped, and a bad one must be refused all the same.
    validate_client(&presence.client)?;
    if let Some(status) = &presence.status {
        validate_status(status)?;
    }
    let check = presence.clone();
    let recorded = app
        .commit_if(EventKind::AgentSeen(presence), move |s, now| {
            Ok(s.presence_due(&check, now))
        })
        .await?;
    Ok(Json(WhoAck {
        v: V,
        ok: true,
        recorded: recorded.is_some(),
    }))
}

#[derive(Deserialize)]
struct ForgetQuery {
    by: Option<String>,
}

/// `DELETE /api/who/:handle`: takes a handle `who` lists off the list on
/// every device (an `agent_forgotten` event), until it checks in again. The
/// handle matches case-insensitively; the event carries it as listed. Not
/// listed (never seen, quiet for 2 hours, or forgotten already) is 404.
async fn forget(
    State(app): State<AppState>,
    ApiPath(handle): ApiPath<String>,
    ApiQuery(q): ApiQuery<ForgetQuery>,
) -> ApiResult<ForgetAck> {
    let asked = validate_handle(&handle)?;
    // Without `by`, the person at the keyboard (BEACON_HUMAN) is forgetting.
    let by = match q.by.as_deref() {
        Some(by) => validate_handle(by)?,
        None => app.cfg.human.clone(),
    };
    let lookup = asked.clone();
    let listed = app
        .with_store(move |s| {
            let rows = s.who(now_ms())?;
            let exact = rows.iter().find(|r| r.handle == lookup);
            let any_case = || rows.iter().find(|r| r.handle.eq_ignore_ascii_case(&lookup));
            Ok(exact.or_else(any_case).map(|r| r.handle.clone()))
        })
        .await?;
    let not_listed = || {
        Error::NotFound(format!(
            "{asked} is not listed in who: never seen, quiet for 2 hours, or forgotten already"
        ))
    };
    let handle = listed.ok_or_else(not_listed)?;
    let check = handle.clone();
    let forgot = app
        .commit_if(
            EventKind::AgentForgotten {
                handle: handle.clone(),
                by,
            },
            // Under the write's lock: two forgets at once record one event.
            move |s, now| Ok(s.who(now)?.iter().any(|r| r.handle == check)),
        )
        .await?;
    if forgot.is_none() {
        return Err(not_listed().into());
    }
    Ok(Json(ForgetAck {
        v: V,
        ok: true,
        forgot: handle,
    }))
}

// ------------------------------------------------- joins, cursors and unread

async fn get_joins(
    State(app): State<AppState>,
    ApiPath(handle): ApiPath<String>,
) -> ApiResult<JoinsJson> {
    let handle = validate_handle(&handle)?;
    let who = handle.clone();
    let channels = app.with_store(move |s| s.joins(&who)).await?;
    Ok(Json(JoinsJson {
        v: V,
        ok: true,
        handle,
        channels,
    }))
}

async fn put_joins(
    State(app): State<AppState>,
    ApiPath(handle): ApiPath<String>,
    ApiJson(req): ApiJson<JoinsRequest>,
) -> ApiResult<JoinsJson> {
    let handle = validate_handle(&handle)?;
    let who = handle.clone();
    let channels = app
        .with_store(move |s| {
            s.set_joins(&who, &req.channels)?;
            s.joins(&who)
        })
        .await?;
    Ok(Json(JoinsJson {
        v: V,
        ok: true,
        handle,
        channels,
    }))
}

async fn get_cursors(
    State(app): State<AppState>,
    ApiPath(handle): ApiPath<String>,
) -> ApiResult<CursorsJson> {
    let handle = validate_handle(&handle)?;
    let who = handle.clone();
    let cursors = app.with_store(move |s| s.cursors(&who)).await?;
    Ok(Json(CursorsJson {
        v: V,
        ok: true,
        handle,
        cursors,
    }))
}

async fn put_cursor(
    State(app): State<AppState>,
    ApiPath(handle): ApiPath<String>,
    ApiJson(req): ApiJson<CursorRequest>,
) -> ApiResult<CursorsJson> {
    let handle = validate_handle(&handle)?;
    let channel = normalize_channel(&req.channel)?;
    let arrival = req.arrival;
    let who = handle.clone();
    let cursors = app
        .with_store(move |s| {
            require_channel(s, &channel)?;
            // A cursor past what has arrived would hide posts that come later.
            let head = s.arrival_head();
            if !(0..=head).contains(&arrival) {
                return Err(Error::Usage(format!(
                    "arrival {arrival} is outside 0..={head}, \
                     the newest arrival on this node"
                )));
            }
            s.set_cursor(&who, &channel, arrival)?;
            s.cursors(&who)
        })
        .await?;
    Ok(Json(CursorsJson {
        v: V,
        ok: true,
        handle,
        cursors,
    }))
}

#[derive(Deserialize)]
struct UnreadQuery {
    limit: Option<u32>,
    advance: Option<String>,
}

async fn unread(
    State(app): State<AppState>,
    ApiPath(handle): ApiPath<String>,
    ApiQuery(q): ApiQuery<UnreadQuery>,
) -> ApiResult<PostsResponse> {
    let handle = validate_handle(&handle)?;
    let limit = page_limit(q.limit)?;
    let advance = flag("advance", q.advance.as_deref())?;
    let page = app
        .with_store(move |s| {
            let page = s.unread(&handle, limit)?;
            if advance {
                s.mark_read(&handle, &page.posts)?;
            }
            Ok(page)
        })
        .await?;
    Ok(Json(posts_json(&app, &page.posts, Some(page.remaining))))
}
