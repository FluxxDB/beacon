//! The HTTP client the CLI (and later the TUI) talks to its node with.
//!
//! One method per route. Failures become [`Error`]s whose variants carry the
//! exit codes: no connection is [`Error::Unreachable`], a connection that
//! broke or an answer that never came is [`Error::Interrupted`], 401 is
//! [`Error::Auth`], 404 [`Error::NotFound`], 409 [`Error::Conflict`] and
//! 400/413 [`Error::Usage`].
//!
//! Nothing here retries. A post has no idempotency key, so a request that
//! died after it left may or may not have been stored; the caller is told
//! so instead of being given a duplicate.
//!
//! Four routes last longer than the ordinary 30 s timeout allows:
//! [`Client::wait`] (a long poll), [`Client::open_stream`] (server-sent
//! events), [`Client::upload`] and [`Client::file`] (files). Each has its own
//! idea of when a silent connection is dead.

use std::collections::VecDeque;
use std::error::Error as StdError;
use std::pin::Pin;
use std::time::Duration;

use anyhow::anyhow;
use bytes::Bytes;
use futures_util::stream::{self, Stream, StreamExt};
use reqwest::multipart::{Form, Part};
use reqwest::{Body, Method, RequestBuilder, Response, StatusCode, Url};
use serde::de::DeserializeOwned;
use tokio::io::AsyncRead;
use tokio_util::io::ReaderStream;

use crate::api::*;
use crate::error::{Error, ServerFault};

/// Where the node listens when `BEACON_SERVER` does not say (inside the
/// container, and for a host that publishes the default port).
const DEFAULT_SERVER: &str = "http://127.0.0.1:7700";
/// How long to wait for a connection before calling the node unreachable.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long an ordinary request may take. Long-lived requests (`wait`, the
/// stream) set their own with [`RequestBuilder::timeout`].
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How much longer than it asked the node to wait a `wait` request is given
/// before its connection is called dead.
const WAIT_SLACK: Duration = Duration::from_secs(10);
/// A stream that has been silent this long is dead: the node sends a comment
/// every 15 s when nothing else happens.
const STREAM_IDLE: Duration = Duration::from_secs(45);
/// The most one line of a stream may take before the stream is refused.
const SSE_LINE_MAX: usize = 8 * 1024 * 1024;
/// Bytes of an upload handed to the connection at a time.
const UPLOAD_CHUNK: usize = 64 * 1024;
/// What the error for a post that may have been carried out says about it.
const MAYBE_STORED: &str = "the post may have been stored";

/// A connection to one node.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    /// An `http` URL with a host, so it can have path segments added.
    base: Url,
    secret: Option<String>,
    /// How long an ordinary request may take.
    timeout: Duration,
}

/// Like the derived `Debug`, but never prints the secret.
impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("base", &self.base_url())
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl Client {
    /// A client for the node at `server` (`http://host:port`; the scheme may
    /// be left off). `secret` goes out as the bearer token on every request.
    pub fn new(server: &str, secret: Option<&str>) -> Result<Client, Error> {
        let server = server.trim();
        let invalid = |why: &str| Error::Usage(format!("BEACON_SERVER is invalid: {why}"));
        let with_scheme = if server.contains("://") {
            server.to_string()
        } else {
            format!("http://{server}")
        };
        let base = Url::parse(&with_scheme).map_err(|e| invalid(&format!("{server:?} ({e})")))?;
        if base.scheme() != "http" {
            return Err(invalid(&format!(
                "{server:?} is not an http:// URL (beacon talks plain HTTP; \
                 peers are protected by Tailscale)"
            )));
        }
        if base.cannot_be_a_base() || base.host_str().is_none() {
            return Err(invalid(&format!("{server:?} has no host")));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("beacon/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Error::Internal(anyhow!("cannot build the HTTP client: {e}")))?;
        Ok(Client {
            http,
            base,
            secret: secret.filter(|s| !s.is_empty()).map(String::from),
            timeout: REQUEST_TIMEOUT,
        })
    }

    /// This client, with ordinary requests given `timeout` instead of 30 s.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Client {
        self.timeout = timeout;
        self
    }

    /// The client the `BEACON_SERVER` and `BEACON_SECRET` variables describe.
    pub fn from_env() -> Result<Client, Error> {
        Client::from_vars(|key| std::env::var(key).ok())
    }

    /// [`Client::from_env`] through `get`, which returns a variable's value.
    /// A blank `BEACON_SERVER` counts as unset; the secret is used exactly as
    /// given, as the server does.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Client, Error> {
        let server = get("BEACON_SERVER")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_SERVER.to_string());
        let secret = get("BEACON_SECRET");
        Client::new(&server, secret.as_deref())
    }

    /// The node's address, without a trailing slash.
    pub fn base_url(&self) -> &str {
        self.base.as_str().trim_end_matches('/')
    }

    // --------------------------------------------------------- plumbing

    /// A request to `/api/...`, each of `segments` percent-encoded into the
    /// path, carrying the secret and the default timeout.
    pub fn request(&self, method: Method, segments: &[&str]) -> RequestBuilder {
        self.request_with(method, segments, Query::default())
    }

    /// [`Client::request`] with query parameters.
    pub fn request_with(&self, method: Method, segments: &[&str], query: Query) -> RequestBuilder {
        self.untimed(method, segments, query).timeout(self.timeout)
    }

    /// [`Client::request_with`] without a limit on how long it may take: for
    /// the requests that last as long as a file or a stream does.
    fn untimed(&self, method: Method, segments: &[&str], query: Query) -> RequestBuilder {
        let mut url = self.base.clone();
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty().extend(segments);
        }
        if !query.0.is_empty() {
            url.query_pairs_mut().extend_pairs(&query.0);
        }
        let mut request = self.http.request(method, url);
        if let Some(secret) = &self.secret {
            request = request.bearer_auth(secret);
        }
        request
    }

    /// Sends `request` and reads a JSON answer.
    pub async fn json<T: DeserializeOwned>(&self, request: RequestBuilder) -> Result<T, Error> {
        self.call(request, None).await
    }

    /// [`Client::json`]. When the connection fails after the request may have
    /// reached the node, `unsure` is added to the error: what the caller may
    /// not assume (the request may have been carried out).
    async fn call<T: DeserializeOwned>(
        &self,
        request: RequestBuilder,
        unsure: Option<&str>,
    ) -> Result<T, Error> {
        let response = request
            .send()
            .await
            .map_err(|e| self.transport(&e, unsure))?;
        let status = response.status();
        if !status.is_success() {
            return Err(self.failure(status, response).await);
        }
        self.read_json(response, unsure).await
    }

    /// The body of a successful `response`, read as JSON.
    async fn read_json<T: DeserializeOwned>(
        &self,
        response: Response,
        unsure: Option<&str>,
    ) -> Result<T, Error> {
        let bytes = response
            .bytes()
            .await
            .map_err(|e| self.transport(&e, unsure))?;
        serde_json::from_slice(&bytes).map_err(|e| {
            Error::Internal(anyhow!("unexpected answer from {}: {e}", self.base_url()))
        })
    }

    /// A request that did not get an answer.
    fn transport(&self, e: &reqwest::Error, unsure: Option<&str>) -> Error {
        transport_error(self.base_url(), self.timeout, e, unsure)
    }

    /// The error for a non-2xx answer, in the server's own words.
    async fn failure(&self, status: StatusCode, response: Response) -> Error {
        let text = response.text().await.unwrap_or_default();
        let api: Option<ApiError> = serde_json::from_str(&text).ok();
        let message = match &api {
            Some(api) => api.error.clone(),
            None => {
                let text = text.trim();
                if text.is_empty() {
                    format!("{} answered {status}", self.base_url())
                } else {
                    let shown: String = text.chars().take(200).collect();
                    format!("{} answered {status}: {shown}", self.base_url())
                }
            }
        };
        match status.as_u16() {
            400 | 413 | 422 => Error::Usage(message),
            401 | 403 => Error::Auth,
            404 => Error::NotFound(message),
            409 => Error::Conflict(message),
            _ => match api.as_ref().map(|a| a.code.as_str()) {
                Some("usage") => Error::Usage(message),
                Some("not_found") => Error::NotFound(message),
                Some("conflict") => Error::Conflict(message),
                Some("auth") => Error::Auth,
                _ if status.is_server_error() => {
                    Error::Internal(anyhow::Error::new(ServerFault(message)))
                }
                _ => Error::Internal(anyhow!("{message}")),
            },
        }
    }

    // ----------------------------------------------------- node and peers

    /// `GET /api/health`: is the node up (the answer names it).
    pub async fn health(&self) -> Result<HealthJson, Error> {
        self.json(self.request(Method::GET, &["api", "health"]))
            .await
    }

    /// `GET /api/status`.
    pub async fn status(&self) -> Result<StatusJson, Error> {
        self.json(self.request(Method::GET, &["api", "status"]))
            .await
    }

    /// `GET /api/peers`: the peers and how sync with each is going.
    pub async fn peers(&self) -> Result<PeersJson, Error> {
        self.json(self.request(Method::GET, &["api", "peers"]))
            .await
    }

    // --------------------------------------------------------- channels

    /// `GET /api/channels`. With a `handle`, the channels carry that
    /// handle's unread counts and the answer its unread announcements.
    pub async fn channels(&self, handle: Option<&str>) -> Result<ChannelsJson, Error> {
        let query = Query::default().set("handle", handle);
        self.json(self.request_with(Method::GET, &["api", "channels"], query))
            .await
    }

    /// `POST /api/channels`: creates `name` (or changes the topic of the one
    /// that is there).
    pub async fn create_channel(
        &self,
        name: &str,
        topic: Option<&str>,
    ) -> Result<ChannelAck, Error> {
        let body = NewChannelRequest {
            name: name.to_string(),
            topic: topic.map(String::from),
        };
        self.json(self.request(Method::POST, &["api", "channels"]).json(&body))
            .await
    }

    /// `DELETE /api/channels/:name`.
    pub async fn delete_channel(&self, name: &str) -> Result<DeleteAck, Error> {
        self.json(self.request(Method::DELETE, &["api", "channels", name]))
            .await
    }

    // ------------------------------------------------------------ posts

    /// `POST /api/channels/:name/posts`. Sent once: if the connection dies
    /// on the way, the [`Error::Interrupted`] says the post may have been
    /// stored (and a caller should say how to find out).
    pub async fn post(&self, channel: &str, req: &NewPostRequest) -> Result<PostCreated, Error> {
        let request = self
            .request(Method::POST, &["api", "channels", channel, "posts"])
            .json(req);
        self.call(request, Some(MAYBE_STORED)).await
    }

    /// The same, with a file: `POST /api/channels/:name/posts` as a
    /// `multipart/form-data` upload whose `meta` part (the post) comes before
    /// its `file` part, so the node can refuse the post before it reads the
    /// file. The file is streamed from `file`, never held in memory; give its
    /// `len` if it is known. `name` is what the attachment is called.
    ///
    /// Like [`Client::post`], sent once. There is no limit on how long it
    /// takes: a file is as long as it is.
    pub async fn upload<R>(
        &self,
        channel: &str,
        req: &NewPostRequest,
        name: &str,
        file: R,
        len: Option<u64>,
    ) -> Result<PostCreated, Error>
    where
        R: AsyncRead + Send + Sync + 'static,
    {
        let meta = serde_json::to_string(req)
            .map_err(|e| Error::Internal(anyhow!("cannot encode the post: {e}")))?;
        let meta = Part::text(meta)
            .mime_str("application/json")
            .map_err(|e| Error::Internal(anyhow!("cannot describe the post: {e}")))?;
        let body = Body::wrap_stream(ReaderStream::with_capacity(file, UPLOAD_CHUNK));
        let file = match len {
            Some(len) => Part::stream_with_length(body, len),
            None => Part::stream(body),
        }
        .file_name(name.to_string());
        let form = Form::new().part("meta", meta).part("file", file);
        let request = self
            .untimed(
                Method::POST,
                &["api", "channels", channel, "posts"],
                Query::default(),
            )
            .multipart(form);
        self.call(request, Some(MAYBE_STORED)).await
    }

    /// `GET /api/files/:sha256`: the attachment's bytes, as they arrive. A
    /// node that does not have the file answers 404 ([`Error::NotFound`]).
    pub async fn file(&self, sha256: &str) -> Result<Download, Error> {
        let request = self.untimed(Method::GET, &["api", "files", sha256], Query::default());
        let response = tokio::time::timeout(self.timeout, request.send())
            .await
            .map_err(|_| self.silent())?
            .map_err(|e| self.transport(&e, None))?;
        let status = response.status();
        if !status.is_success() {
            return Err(self.failure(status, response).await);
        }
        Ok(Download {
            len: response.content_length(),
            response,
            base: self.base_url().to_string(),
            patience: self.timeout,
        })
    }

    /// The error for a node that did not answer in time.
    fn silent(&self) -> Error {
        Error::Interrupted(format!(
            "{} did not answer within {}",
            self.base_url(),
            secs(self.timeout)
        ))
    }

    /// `GET /api/channels/:name/posts`: the newest `limit` posts (the
    /// server's default when `None`), oldest first, optionally with `tag`.
    pub async fn posts(
        &self,
        channel: &str,
        limit: Option<u32>,
        tag: Option<&str>,
    ) -> Result<PostsResponse, Error> {
        let query = Query::default().set("limit", limit).set("tag", tag);
        let segments = ["api", "channels", channel, "posts"];
        self.json(self.request_with(Method::GET, &segments, query))
            .await
    }

    /// `GET /api/posts`: posts across all channels, newest `limit`, oldest
    /// first, that mention `mentions`, are announcements, and/or carry `tag`.
    pub async fn cross_posts(
        &self,
        mentions: Option<&str>,
        announcements: bool,
        tag: Option<&str>,
        limit: Option<u32>,
    ) -> Result<PostsResponse, Error> {
        let query = Query::default()
            .set("limit", limit)
            .set("tag", tag)
            .set("mentions", mentions)
            .set("announcements", announcements.then_some("1"));
        self.json(self.request_with(Method::GET, &["api", "posts"], query))
            .await
    }

    /// `GET /api/unread/:handle`: what is new for `handle`, announcements
    /// first. With `advance` the node marks those posts read.
    pub async fn unread(
        &self,
        handle: &str,
        limit: Option<u32>,
        advance: bool,
    ) -> Result<PostsResponse, Error> {
        let query = Query::default()
            .set("limit", limit)
            .set("advance", advance.then_some("1"));
        self.json(self.request_with(Method::GET, &["api", "unread", handle], query))
            .await
    }

    /// `GET /api/posts/:id`; `id` may be a unique suffix of the post's id.
    pub async fn post_by_id(&self, id: &str) -> Result<PostJson, Error> {
        self.json(self.request(Method::GET, &["api", "posts", id]))
            .await
    }

    /// `DELETE /api/posts/:id`, on behalf of `by` (the node's human when
    /// `None`).
    pub async fn delete_post(&self, id: &str, by: Option<&str>) -> Result<DeleteAck, Error> {
        let query = Query::default().set("by", by);
        self.json(self.request_with(Method::DELETE, &["api", "posts", id], query))
            .await
    }

    // --------------------------------------------- presence, joins, reads

    /// `POST /api/who`: "this handle is here".
    pub async fn heartbeat(&self, req: &WhoRequest) -> Result<WhoAck, Error> {
        self.json(self.request(Method::POST, &["api", "who"]).json(req))
            .await
    }

    /// `GET /api/who`: who has been seen lately.
    pub async fn who(&self) -> Result<WhoJson, Error> {
        self.json(self.request(Method::GET, &["api", "who"])).await
    }

    /// `GET /api/joins/:handle`.
    pub async fn joins(&self, handle: &str) -> Result<JoinsJson, Error> {
        self.json(self.request(Method::GET, &["api", "joins", handle]))
            .await
    }

    /// `PUT /api/joins/:handle`: replaces the channels `handle` joined.
    pub async fn set_joins(&self, handle: &str, channels: &[String]) -> Result<JoinsJson, Error> {
        let body = JoinsRequest {
            channels: channels.to_vec(),
        };
        self.json(
            self.request(Method::PUT, &["api", "joins", handle])
                .json(&body),
        )
        .await
    }

    /// `GET /api/read-cursors/:handle`.
    pub async fn cursors(&self, handle: &str) -> Result<CursorsJson, Error> {
        self.json(self.request(Method::GET, &["api", "read-cursors", handle]))
            .await
    }

    /// `PUT /api/read-cursors/:handle`: `handle` has read `channel` up to the
    /// post that arrived as `arrival` ([`PostJson::arrival`]).
    pub async fn set_cursor(
        &self,
        handle: &str,
        channel: &str,
        arrival: i64,
    ) -> Result<CursorsJson, Error> {
        let body = CursorRequest {
            channel: channel.to_string(),
            arrival,
        };
        self.json(
            self.request(Method::PUT, &["api", "read-cursors", handle])
                .json(&body),
        )
        .await
    }

    // ------------------------------------------------ waiting for news

    /// `GET /api/wait`: holds the request until `handle` has relevant unread
    /// posts and returns them (the node does not mark them read), or returns
    /// `None` when `timeout` (rounded up to whole seconds) passes without any.
    /// A node that is stopping also answers `None`, early, so a caller that
    /// wants to wait longer asks again. It looks before it waits: a post that
    /// is already there is answered at once.
    pub async fn wait(
        &self,
        handle: &str,
        timeout: Duration,
    ) -> Result<Option<PostsResponse>, Error> {
        let asked = timeout.as_secs() + u64::from(timeout.subsec_nanos() > 0);
        let query = Query::default()
            .set("handle", Some(handle))
            .set("timeout", Some(asked));
        let patience = Duration::from_secs(asked) + WAIT_SLACK;
        let response = self
            .request_with(Method::GET, &["api", "wait"], query)
            .timeout(patience)
            .send()
            .await
            .map_err(|e| transport_error(self.base_url(), patience, &e, None))?;
        let status = response.status();
        if status == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(self.failure(status, response).await);
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| transport_error(self.base_url(), patience, &e, None))?;
        let page = serde_json::from_slice(&bytes).map_err(|e| {
            Error::Internal(anyhow!("unexpected answer from {}: {e}", self.base_url()))
        })?;
        Ok(Some(page))
    }

    /// The newest arrival number on the node (the one [`PostJson::arrival`]
    /// counts in), so a client can tell what came in after it looked.
    pub async fn arrival_head(&self) -> Result<i64, Error> {
        let query = Query::default().set("limit", Some(1));
        let page: EventsResponse = self
            .json(self.request_with(Method::GET, &["api", "events"], query))
            .await?;
        Ok(page.head)
    }

    /// `GET /api/stream`: opens the live feed and returns it once the node has
    /// answered, so an `Ok` means "connected": everything stored from now on
    /// will arrive. With `relevant` (and a `handle`) the posts are only those
    /// that matter to the handle; other kinds of event are not filtered.
    ///
    /// The feed never ends well. The last item of a feed that broke is an
    /// `Err` ([`Error::Interrupted`]: the connection dropped, the node closed
    /// the stream or went silent for 45 s) and nothing follows it; opening a
    /// new one, after a pause, is up to the caller. It then misses what was
    /// stored in between, which the caller can read from the store.
    pub async fn open_stream(
        &self,
        handle: Option<&str>,
        relevant: bool,
    ) -> Result<impl Stream<Item = Result<StreamEvent, Error>> + Send + Unpin + 'static, Error>
    {
        let query = Query::default()
            .set("handle", handle)
            .set("relevant", relevant.then_some("1"));
        let request = self
            .untimed(Method::GET, &["api", "stream"], query)
            .header(reqwest::header::ACCEPT, "text/event-stream");
        let response = tokio::time::timeout(self.timeout, request.send())
            .await
            .map_err(|_| self.silent())?
            .map_err(|e| self.transport(&e, None))?;
        let status = response.status();
        if !status.is_success() {
            return Err(self.failure(status, response).await);
        }
        let open = OpenStream {
            bytes: Box::pin(response.bytes_stream()),
            parser: SseParser::default(),
            ready: VecDeque::new(),
            base: self.base_url().to_string(),
        };
        Ok(stream::unfold(Some(open), OpenStream::next).boxed())
    }

    /// [`Client::open_stream`] as a stream that opens itself when it is first
    /// polled: a failure to connect is its only item.
    pub fn stream(
        &self,
        handle: Option<&str>,
        relevant: bool,
    ) -> impl Stream<Item = Result<StreamEvent, Error>> + Send + Unpin + 'static {
        let client = self.clone();
        let handle = handle.map(String::from);
        stream::once(async move { client.open_stream(handle.as_deref(), relevant).await })
            .flat_map(|opened| match opened {
                Ok(events) => events.boxed(),
                Err(e) => stream::once(std::future::ready(Err(e))).boxed(),
            })
            .boxed()
    }
}

/// The error for a request that did not get an answer from the node at `base`.
/// Only a failure to connect is [`Error::Unreachable`]; once a connection
/// was made, a break or a silence is [`Error::Interrupted`]. `unsure` says what
/// a request that may have been carried out cannot be assumed to have not.
fn transport_error(
    base: &str,
    patience: Duration,
    e: &reqwest::Error,
    unsure: Option<&str>,
) -> Error {
    if e.is_builder() {
        return Error::Internal(anyhow!("cannot send the request to {base}: {e}"));
    }
    let mut reason = e.to_string();
    let mut source = e.source();
    while let Some(inner) = source {
        reason = inner.to_string();
        source = inner.source();
    }
    if e.is_connect() {
        return Error::Unreachable(format!("cannot reach {base} ({reason})"));
    }
    let maybe = unsure.map(|u| format!("; {u}")).unwrap_or_default();
    if e.is_timeout() {
        Error::Interrupted(format!(
            "{base} did not answer within {}{maybe}",
            secs(patience)
        ))
    } else {
        Error::Interrupted(format!("lost the connection to {base} ({reason}){maybe}"))
    }
}

/// `30s`, or `0.3s` for a part of a second.
fn secs(d: Duration) -> String {
    if d.subsec_nanos() == 0 {
        format!("{}s", d.as_secs())
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

// ------------------------------------------------------------------- files

/// A file being downloaded (see [`Client::file`]).
pub struct Download {
    response: Response,
    /// The node's address, for errors.
    base: String,
    /// How long the node may go without sending the next piece.
    patience: Duration,
    /// How many bytes the node says the file has.
    pub len: Option<u64>,
}

impl Download {
    /// The next piece of the file, or `None` at its end. A node that stops
    /// sending is given as long as a request normally takes.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>, Error> {
        let lost =
            |why: &str| Error::Interrupted(format!("lost the connection to {} ({why})", self.base));
        match tokio::time::timeout(self.patience, self.response.chunk()).await {
            Ok(Ok(chunk)) => Ok(chunk),
            Ok(Err(e)) => Err(lost(&e.to_string())),
            Err(_) => Err(lost(&format!("silent for {}", secs(self.patience)))),
        }
    }
}

// ----------------------------------------------------------- server-sent events

/// One server-sent event as it came.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseMessage {
    /// The `event:` name (`message` when the event had none).
    pub event: String,
    /// The `data:` lines, joined by line feeds.
    pub data: String,
}

/// Reads server-sent events out of the bytes of a response, however they are
/// cut into pieces. Comments (the node's keep-alives) and fields it does not
/// know are skipped.
#[derive(Debug, Default)]
pub struct SseParser {
    /// The start of a line that has not ended yet.
    pending: Vec<u8>,
    event: Option<String>,
    data: String,
    has_data: bool,
}

impl SseParser {
    /// Takes the next `bytes` of the stream and returns the events they
    /// complete. A line longer than 8 MiB is refused.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseMessage>, Error> {
        self.pending.extend_from_slice(bytes);
        let buffer = std::mem::take(&mut self.pending);
        let mut events = Vec::new();
        let mut start = 0;
        while let Some(at) = buffer[start..].iter().position(|&b| b == b'\n') {
            let end = start + at;
            let line = buffer[start..end]
                .strip_suffix(b"\r")
                .unwrap_or(&buffer[start..end]);
            events.extend(self.line(line));
            start = end + 1;
        }
        if buffer.len() - start > SSE_LINE_MAX {
            return Err(Error::Internal(anyhow!(
                "the node sent a line of over {} MiB in its event stream",
                SSE_LINE_MAX / (1024 * 1024)
            )));
        }
        self.pending = buffer[start..].to_vec();
        Ok(events)
    }

    /// One line, without its line break; an empty line ends an event.
    fn line(&mut self, line: &[u8]) -> Option<SseMessage> {
        if line.is_empty() {
            let event = self.event.take();
            let mut data = std::mem::take(&mut self.data);
            if !std::mem::take(&mut self.has_data) {
                return None;
            }
            data.pop(); // the line feed after the last data line
            let event = event.filter(|e| !e.is_empty());
            return Some(SseMessage {
                event: event.unwrap_or_else(|| "message".to_string()),
                data,
            });
        }
        if line[0] == b':' {
            return None;
        }
        let (field, value) = match line.iter().position(|&b| b == b':') {
            Some(at) => (&line[..at], &line[at + 1..]),
            None => (line, &line[line.len()..]),
        };
        let value = value.strip_prefix(b" ").unwrap_or(value);
        match field {
            b"event" => self.event = Some(String::from_utf8_lossy(value).into_owned()),
            b"data" => {
                self.data.push_str(&String::from_utf8_lossy(value));
                self.data.push('\n');
                self.has_data = true;
            }
            _ => {}
        }
        None
    }
}

/// A stream that is connected: where [`Client::open_stream`]'s events come from.
struct OpenStream {
    bytes: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    parser: SseParser,
    /// Events read but not handed out yet.
    ready: VecDeque<StreamEvent>,
    /// The node's address, for errors.
    base: String,
}

impl OpenStream {
    /// The next item of the feed; `state` is `None` once an error was handed
    /// out, which ends it.
    async fn next(
        state: Option<OpenStream>,
    ) -> Option<(Result<StreamEvent, Error>, Option<OpenStream>)> {
        let mut open = state?;
        loop {
            if let Some(event) = open.ready.pop_front() {
                return Some((Ok(event), Some(open)));
            }
            let lost = |why: String| {
                Error::Interrupted(format!("lost the connection to {} ({why})", open.base))
            };
            let failure = match tokio::time::timeout(STREAM_IDLE, open.bytes.next()).await {
                Err(_) => lost(format!("silent for {}", secs(STREAM_IDLE))),
                Ok(None) => lost("the node closed the stream".to_string()),
                Ok(Some(Err(e))) => lost(e.to_string()),
                Ok(Some(Ok(chunk))) => match open.parser.push(&chunk) {
                    Ok(messages) => {
                        // An event this client does not know (a newer node's) is skipped.
                        let known = messages
                            .iter()
                            .filter_map(|m| StreamEvent::from_sse(&m.event, &m.data).ok());
                        open.ready.extend(known);
                        continue;
                    }
                    Err(e) => e,
                },
            };
            return Some((Err(failure), None));
        }
    }
}

/// Query parameters for [`Client::request_with`]; unset ones are left out.
#[derive(Debug, Default)]
pub struct Query(Vec<(&'static str, String)>);

impl Query {
    /// Adds `name=value` if there is a value.
    pub fn set(mut self, name: &'static str, value: Option<impl ToString>) -> Query {
        if let Some(value) = value {
            self.0.push((name, value.to_string()));
        }
        self
    }
}
