//! The API handlers. Each one checks its input, makes one or two calls on the
//! store (or [`AppState::commit`] for a write) and shapes the answer.
//!
//! Posts, channels and presence are always rendered from derived state (the
//! rows the store returns), never from raw events; the one exception is the
//! answer to a post this node has just committed, whose event was validated
//! on the way in.

use axum::extract::State;
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::Deserialize;

use super::{now_ms, ApiFailure, ApiJson, ApiPath, ApiQuery, ApiResult, AppState};
use crate::api::*;
use crate::error::Error;
use crate::model::{
    normalize_channel, parse_mentions, validate_handle, validate_tag, Attachment, EventKind,
    NewPost, Presence, BODY_MAX,
};
use crate::store::{Applied, PostQuery, PostRow, Store};

/// Posts per page when a request does not say.
const DEFAULT_LIMIT: usize = 50;
/// The most posts a request may ask for.
const MAX_LIMIT: usize = 500;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/status", get(status))
        .route("/api/channels", get(list_channels).post(create_channel))
        .route("/api/channels/{name}", delete(delete_channel))
        .route(
            "/api/channels/{name}/posts",
            get(channel_posts).post(create_post),
        )
        .route("/api/posts", get(cross_channel_posts))
        .route("/api/posts/{id}", get(get_post).delete(delete_post))
        .route("/api/who", get(who).post(heartbeat))
        .route("/api/joins/{handle}", get(get_joins).put(put_joins))
        .route(
            "/api/read-cursors/{handle}",
            get(get_cursors).put(put_cursor),
        )
        .route("/api/unread/{handle}", get(unread))
}

// ----------------------------------------------------------------- helpers

/// A page size: 50 by default, at least 1, at most 500 (more is cut to 500).
fn page_limit(limit: Option<u32>) -> Result<usize, Error> {
    match limit {
        None => Ok(DEFAULT_LIMIT),
        Some(0) => Err(Error::Usage("limit must be at least 1".into())),
        Some(n) => Ok((n as usize).min(MAX_LIMIT)),
    }
}

/// A yes/no query parameter: `1`/`true`/`yes` or `0`/`false`/`no`.
fn flag(name: &str, value: Option<&str>) -> Result<bool, Error> {
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

fn posts_json(app: &AppState, rows: &[PostRow], remaining: Option<u32>) -> PostsResponse {
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
        have,
    }))
}

// ---------------------------------------------------------------- channels

#[derive(Deserialize)]
struct ChannelsQuery {
    handle: Option<String>,
}

async fn list_channels(
    State(app): State<AppState>,
    ApiQuery(q): ApiQuery<ChannelsQuery>,
) -> ApiResult<ChannelsJson> {
    let handle = q.handle.as_deref().map(validate_handle).transpose()?;
    let (rows, unread) = app
        .with_store(move |s| {
            let rows = s.channels()?;
            let unread = match &handle {
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
    ApiJson(req): ApiJson<NewPostRequest>,
) -> ApiResult<PostCreated> {
    Ok(Json(post_to_channel(&app, &name, req, None).await?))
}

/// Posts `req` (with `attachment`, if the bytes were already stored) to the
/// channel `name`. The JSON and the multipart forms of the route share this.
pub async fn post_to_channel(
    app: &AppState,
    name: &str,
    req: NewPostRequest,
    attachment: Option<Attachment>,
) -> Result<PostCreated, ApiFailure> {
    let channel = normalize_channel(name)?;
    let author = validate_handle(&req.author)?;
    let mut tags: Vec<String> = Vec::new();
    for tag in &req.tags {
        let tag = validate_tag(tag)?;
        if !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    if req.body.len() > BODY_MAX {
        return Err(ApiFailure::too_large(format!(
            "post body is {} bytes, over the {BODY_MAX}-byte limit",
            req.body.len()
        )));
    }
    if req.body.trim().is_empty() && attachment.is_none() {
        return Err(Error::Usage("the post is empty: give it some text".into()).into());
    }

    let lookup = channel.clone();
    let exists = app.with_store(move |s| s.channel(&lookup)).await?.is_some();
    if !exists {
        if !req.create {
            return Err(Error::NotFound(format!(
                "no channel #{channel}: create it with `./beacon channel new {channel}`, \
                 or post with --create"
            ))
            .into());
        }
        let kind = EventKind::ChannelCreated {
            name: channel.clone(),
            topic: None,
        };
        app.commit(kind).await?;
    }

    let post = NewPost {
        post_id: String::new(), // the event's id becomes the post's
        channel,
        author,
        author_kind: req.author_kind,
        client: clean(req.client),
        node: app.cfg.node.clone(),
        mentions: parse_mentions(&req.body),
        body: req.body,
        tags,
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
    let EventKind::PostCreated(p) = &applied.event.kind else {
        return Err(Error::Internal(anyhow::anyhow!(
            "committed event {} is not a post",
            applied.event.id
        )));
    };
    Ok(PostRow {
        id: p.post_id.clone(),
        channel: p.channel.clone(),
        ts: applied.event.ts,
        author: p.author.clone(),
        author_kind: p.author_kind,
        client: p.client.clone(),
        node: p.node.clone(),
        body: p.body.clone(),
        tags: p.tags.clone(),
        mentions: p.mentions.clone(),
        attachment: p.attachment.clone(),
        arrival: applied.arrival,
    })
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
