//! The HTTP server: [`start`] opens the store, binds the port and serves the
//! API in [`routes`]. Every request needs `Authorization: Bearer $BEACON_SECRET`.
//!
//! Handlers stay thin over [`Store`]. Every write goes through
//! [`AppState::commit`] (a change made on this node) or [`AppState::ingest`]
//! (an event from a peer), which store it, announce it on
//! [`AppState::arrivals`] and wake the sync engine.

pub mod routes;
pub mod stream;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::anyhow;
use axum::extract::multipart::{MultipartError, MultipartRejection};
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use crate::api::{ApiError, V};
use crate::blobs::{Blobs, PutError};
use crate::config::Config;
use crate::error::Error;
use crate::model::{validate_event, Event, EventKind};
use crate::store::{Applied, Store};
use crate::sync::SyncHandle;

/// How many announced events [`AppState::arrivals`] buffers per receiver
/// before a slow one lags.
const ARRIVALS_CAPACITY: usize = 1024;
/// How long [`NodeHandle::shutdown`] waits for requests in flight before it
/// stops the server anyway.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Milliseconds since the Unix epoch, now.
pub fn now_ms() -> i64 {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX)
}

// -------------------------------------------------------------- app state

/// Everything a handler (or the sync engine) needs; cheap to clone.
#[derive(Clone)]
pub struct AppState {
    /// The board. Take it through [`AppState::with_store`].
    pub store: Arc<Mutex<Store>>,
    pub cfg: Arc<Config>,
    pub blobs: Blobs,
    /// Every event that was stored and derived board state, in arrival order:
    /// the ones this node committed and the valid ones it ingested. An event
    /// that fails [`validate_event`] is stored (so sync stays gap-free) but
    /// never announced here. An announced event can still be hidden (its
    /// post deleted, its channel gone), so look a post up in the store
    /// before rendering it.
    pub arrivals: broadcast::Sender<Applied>,
    pub sync: SyncHandle,
    /// Fires when the node is asked to stop; long-lived tasks and responses
    /// (streams, long polls, sync loops) must end when it does.
    pub shutdown: Shutdown,
}

impl AppState {
    /// Runs `f` on the store, under its lock, on a thread where blocking is
    /// fine. Keep `f` short; it holds up every other request.
    pub async fn with_store<T, F>(&self, f: F) -> Result<T, Error>
    where
        T: Send + 'static,
        F: FnOnce(&mut Store) -> Result<T, Error> + Send + 'static,
    {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            // A panic in another request leaves the database consistent (every
            // write is a transaction), so a poisoned lock is still usable.
            let mut guard = store.lock().unwrap_or_else(PoisonError::into_inner);
            f(&mut guard)
        })
        .await
        .map_err(|e| Error::Internal(anyhow!("store task failed: {e}")))?
    }

    /// Records a change made on this node: stores it as the next event of our
    /// own origin, announces it on [`AppState::arrivals`] and wakes sync.
    pub async fn commit(&self, kind: EventKind) -> Result<Applied, Error> {
        self.commit_if(kind, |_, _| Ok(true))
            .await?
            .ok_or_else(|| Error::Internal(anyhow!("unconditional commit stored nothing")))
    }

    /// [`AppState::commit`], but only if `when` says so. `when` runs under the
    /// same lock as the write, with the time the event will carry, so a check
    /// like "is this heartbeat due" cannot be raced by another request.
    /// Returns `None`, writing nothing, when `when` returns `false`.
    pub async fn commit_if<F>(&self, kind: EventKind, when: F) -> Result<Option<Applied>, Error>
    where
        F: FnOnce(&Store, i64) -> Result<bool, Error> + Send + 'static,
    {
        let arrivals = self.arrivals.clone();
        let applied = self
            .with_store(move |store| {
                // Read under the lock: a request that waited for it must not
                // stamp its event with the time it started waiting.
                let now = now_ms();
                if !when(store, now)? {
                    return Ok(None);
                }
                // `append_local` validates, so a local event always derives.
                let applied = store.append_local(kind, now)?;
                // Sent under the lock so receivers see arrival order.
                let _ = arrivals.send(applied.clone());
                Ok(Some(applied))
            })
            .await?;
        if applied.is_some() {
            self.sync.notify();
        }
        Ok(applied)
    }

    /// Stores an event that came from a peer. `None` if we already had it.
    /// A new event is relayed (sync is woken) whatever it says, but only one
    /// that passes [`validate_event`] is announced on [`AppState::arrivals`]:
    /// the store keeps an invalid one without deriving anything from it.
    pub async fn ingest(&self, ev: Event) -> Result<Option<Applied>, Error> {
        let arrivals = self.arrivals.clone();
        let applied = self
            .with_store(move |store| {
                let applied = store.apply(&ev)?;
                if let Some(applied) = &applied {
                    if validate_event(&applied.event).is_ok() {
                        let _ = arrivals.send(applied.clone());
                    }
                }
                Ok(applied)
            })
            .await?;
        if applied.is_some() {
            self.sync.notify();
        }
        Ok(applied)
    }
}

/// A signal that the node is shutting down.
#[derive(Debug, Clone)]
pub struct Shutdown(watch::Receiver<bool>);

impl Shutdown {
    /// Whether shutdown has started.
    pub fn is_triggered(&self) -> bool {
        *self.0.borrow() || self.0.has_changed().is_err()
    }

    /// Completes when shutdown starts (or the node handle is dropped).
    pub async fn triggered(&self) {
        let mut rx = self.0.clone();
        // An error means the sender is gone, which is a stop as well.
        let _ = rx.wait_for(|stop| *stop).await;
    }
}

// ------------------------------------------------------------- start / stop

/// A running node.
pub struct NodeHandle {
    /// The address the server is bound to (the real port when `listen` asked for 0).
    pub addr: SocketAddr,
    state: AppState,
    stop: watch::Sender<bool>,
    server: JoinHandle<()>,
}

impl NodeHandle {
    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// Stops the node and waits until its port and database are released:
    /// requests in flight finish (up to a few seconds), then the listener
    /// and the store are dropped.
    pub async fn shutdown(mut self) {
        let _ = self.stop.send(true);
        if tokio::time::timeout(SHUTDOWN_GRACE, &mut self.server)
            .await
            .is_err()
        {
            tracing::warn!("requests still open after {SHUTDOWN_GRACE:?}; stopping anyway");
            self.server.abort();
            let _ = (&mut self.server).await;
        }
    }
}

/// Opens the store under `cfg.data_dir`, binds `cfg.listen` and serves the API.
pub async fn start(cfg: Config) -> Result<NodeHandle, Error> {
    let internal = |what: String| move |e: std::io::Error| Error::Internal(anyhow!("{what}: {e}"));
    let files = cfg.data_dir.join("files");
    std::fs::create_dir_all(&files)
        .map_err(internal(format!("cannot create {}", files.display())))?;
    let store = Store::open(&cfg.data_dir.join("beacon.db"), &cfg.node)?;
    let listener = TcpListener::bind(cfg.listen)
        .await
        .map_err(internal(format!("cannot listen on {}", cfg.listen)))?;
    let addr = listener
        .local_addr()
        .map_err(internal("cannot read the listen address".to_string()))?;

    // Nothing is uploading yet, so any temp file is from a run that died.
    let blobs = Blobs::new(files);
    blobs.clear_temp();

    let (stop, stop_rx) = watch::channel(false);
    let (arrivals, _) = broadcast::channel(ARRIVALS_CAPACITY);
    let state = AppState {
        store: Arc::new(Mutex::new(store)),
        blobs,
        cfg: Arc::new(cfg),
        arrivals,
        sync: SyncHandle::new(),
        shutdown: Shutdown(stop_rx),
    };

    let app = router(state.clone());
    let shutdown = state.shutdown.clone();
    let node = state.cfg.node.clone();
    let server = tokio::spawn(async move {
        tracing::info!(%addr, %node, "beacon listening");
        let served = axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown.triggered().await })
            .await;
        if let Err(e) = served {
            tracing::error!("server stopped: {e}");
        }
    });
    Ok(NodeHandle {
        addr,
        state,
        stop,
        server,
    })
}

/// The whole API behind the secret check.
pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(routes::router())
        .merge(stream::router(&state))
        .fallback(no_such_endpoint)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_secret,
        ))
        .with_state(state)
}

async fn no_such_endpoint() -> ApiFailure {
    Error::NotFound("no such endpoint".into()).into()
}

async fn method_not_allowed() -> ApiFailure {
    ApiFailure {
        status: StatusCode::METHOD_NOT_ALLOWED,
        error: Error::Usage("that method is not allowed on this endpoint".into()),
    }
}

// -------------------------------------------------------------------- auth

/// Whether `given` is `expected`, in time that does not depend on how much of
/// it matches (or on its length): both are hashed, and the hashes compared
/// without stopping at the first difference.
pub fn secret_matches(given: &str, expected: &str) -> bool {
    let given = Sha256::digest(given.as_bytes());
    let expected = Sha256::digest(expected.as_bytes());
    let differs = given
        .iter()
        .zip(expected.iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b));
    std::hint::black_box(differs) == 0
}

/// The token of an `Authorization: Bearer <token>` header value.
fn bearer_token(header: &str) -> Option<&str> {
    let (scheme, token) = header.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

/// Rejects every request that does not carry the secret, whatever its path.
async fn require_secret(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let given = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer_token);
    match given {
        Some(token) if secret_matches(token, &state.cfg.secret) => next.run(req).await,
        _ => ApiFailure::from(Error::Auth).into_response(),
    }
}

// ------------------------------------------------------ errors and extractors

/// A failed request: an [`Error`] and the HTTP status it is answered with.
#[derive(Debug)]
pub struct ApiFailure {
    pub status: StatusCode,
    pub error: Error,
}

impl ApiFailure {
    /// A usage error answered with `413 Payload Too Large`.
    pub fn too_large(message: impl Into<String>) -> ApiFailure {
        ApiFailure {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            error: Error::Usage(message.into()),
        }
    }
}

/// The status and `code` an [`Error`] is answered with by default.
fn status_and_code(error: &Error) -> (StatusCode, &'static str) {
    match error {
        Error::Usage(_) => (StatusCode::BAD_REQUEST, "usage"),
        Error::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
        Error::Ambiguous(_) | Error::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
        Error::Auth => (StatusCode::UNAUTHORIZED, "auth"),
        Error::Unreachable(_) | Error::Interrupted(_) | Error::Timeout | Error::Internal(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "internal")
        }
    }
}

impl From<Error> for ApiFailure {
    fn from(error: Error) -> Self {
        ApiFailure {
            status: status_and_code(&error).0,
            error,
        }
    }
}

impl IntoResponse for ApiFailure {
    fn into_response(self) -> Response {
        let (_, code) = status_and_code(&self.error);
        let message = match &self.error {
            Error::Internal(e) => {
                tracing::error!("internal error: {e:#}");
                format!("internal error: {e:#}")
            }
            other => other.to_string(),
        };
        let body = ApiError {
            v: V,
            error: message,
            code: code.to_string(),
        };
        let mut response = (self.status, Json(body)).into_response();
        if matches!(self.error, Error::Auth) {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

/// A request that could not be read: 413 if it was too large, else 400.
fn unreadable(status: StatusCode, text: String) -> ApiFailure {
    let status = if status == StatusCode::PAYLOAD_TOO_LARGE {
        status
    } else {
        StatusCode::BAD_REQUEST
    };
    ApiFailure {
        status,
        error: Error::Usage(text),
    }
}

impl From<JsonRejection> for ApiFailure {
    fn from(rejection: JsonRejection) -> Self {
        unreadable(rejection.status(), rejection.body_text())
    }
}

impl From<QueryRejection> for ApiFailure {
    fn from(rejection: QueryRejection) -> Self {
        unreadable(rejection.status(), rejection.body_text())
    }
}

impl From<PathRejection> for ApiFailure {
    fn from(rejection: PathRejection) -> Self {
        unreadable(rejection.status(), rejection.body_text())
    }
}

impl From<MultipartRejection> for ApiFailure {
    fn from(rejection: MultipartRejection) -> Self {
        unreadable(rejection.status(), rejection.body_text())
    }
}

impl From<MultipartError> for ApiFailure {
    fn from(error: MultipartError) -> Self {
        unreadable(error.status(), error.body_text())
    }
}

impl From<PutError> for ApiFailure {
    /// An attachment over the limit is a 413; the rest is an ordinary error.
    fn from(error: PutError) -> Self {
        match error {
            PutError::TooLarge { .. } => ApiFailure::too_large(error.to_string()),
            PutError::Failed(e) => e.into(),
        }
    }
}

/// `Json` that fails with an [`ApiError`] body.
pub struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiFailure;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiFailure> {
        Ok(ApiJson(Json::<T>::from_request(req, state).await?.0))
    }
}

/// `Query` that fails with an [`ApiError`] body.
pub struct ApiQuery<T>(pub T);

impl<S, T> FromRequestParts<S> for ApiQuery<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiFailure;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiFailure> {
        Ok(ApiQuery(
            Query::<T>::from_request_parts(parts, state).await?.0,
        ))
    }
}

/// `Path` that fails with an [`ApiError`] body.
pub struct ApiPath<T>(pub T);

impl<S, T> FromRequestParts<S> for ApiPath<T>
where
    S: Send + Sync,
    T: DeserializeOwned + Send,
{
    type Rejection = ApiFailure;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiFailure> {
        Ok(ApiPath(
            Path::<T>::from_request_parts(parts, state).await?.0,
        ))
    }
}

/// What a handler returns: a JSON body, or an [`ApiFailure`].
pub type ApiResult<T> = Result<Json<T>, ApiFailure>;
