//! The HTTP client the CLI (and later the TUI) talks to its node with.
//!
//! One method per route. Failures become [`Error`]s whose variants carry the
//! exit codes: no connection is [`Error::Unreachable`], 401 is
//! [`Error::Auth`], 404 [`Error::NotFound`], 409 [`Error::Conflict`] and
//! 400/413 [`Error::Usage`].
//!
//! Nothing here retries. A post has no idempotency key, so a request that
//! died after it left may or may not have been stored; the caller is told
//! so instead of being given a duplicate.

use std::error::Error as StdError;
use std::time::Duration;

use anyhow::anyhow;
use reqwest::{Method, RequestBuilder, Response, StatusCode, Url};
use serde::de::DeserializeOwned;

use crate::api::*;
use crate::error::Error;

/// Where the node listens when `BEACON_SERVER` does not say (inside the
/// container, and for a host that publishes the default port).
const DEFAULT_SERVER: &str = "http://127.0.0.1:7700";
/// How long to wait for a connection before calling the node unreachable.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long an ordinary request may take. Long-lived requests (`wait`, the
/// stream) set their own with [`RequestBuilder::timeout`].
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A connection to one node.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    /// An `http` URL with a host, so it can have path segments added.
    base: Url,
    secret: Option<String>,
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
        })
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
        let mut url = self.base.clone();
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty().extend(segments);
        }
        if !query.0.is_empty() {
            url.query_pairs_mut().extend_pairs(&query.0);
        }
        let mut request = self.http.request(method, url).timeout(REQUEST_TIMEOUT);
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
        let base = self.base_url();
        if e.is_builder() {
            return Error::Internal(anyhow!("cannot send the request to {base}: {e}"));
        }
        let mut reason = e.to_string();
        let mut source = e.source();
        while let Some(inner) = source {
            reason = inner.to_string();
            source = inner.source();
        }
        match unsure {
            Some(unsure) if !e.is_connect() => Error::Unreachable(format!(
                "lost the connection to {base} ({reason}): {unsure}"
            )),
            _ if e.is_timeout() && !e.is_connect() => Error::Unreachable(format!(
                "{base} did not answer within {}s",
                REQUEST_TIMEOUT.as_secs()
            )),
            _ => Error::Unreachable(format!("cannot reach {base} ({reason})")),
        }
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
    /// on the way, the error says the post may have been stored.
    pub async fn post(&self, channel: &str, req: &NewPostRequest) -> Result<PostCreated, Error> {
        let request = self
            .request(Method::POST, &["api", "channels", channel, "posts"])
            .json(req);
        let unsure = format!(
            "the post may have been stored; run `./beacon read {channel}` before posting again"
        );
        self.call(request, Some(&unsure)).await
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
