//! The HTTP client sync talks to peers with: hyper's pooled client, going
//! through `BEACON_PROXY` (SOCKS5) when it is set, and through nothing else.
//!
//! Not reqwest, which the CLI uses: built without TLS, as beacon is, reqwest
//! 0.13 cannot use a SOCKS proxy at all (its plain connector refuses to dial
//! the proxy's own `socks5h://` address), and the PC reaches its peers only
//! through tailscaled's SOCKS5 proxy. This is the same SOCKS client reqwest
//! wraps, with a connector that dials it.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{
    HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, USER_AGENT,
};
use hyper::{Method, Request, Response, Uri};
use hyper_util::client::legacy::connect::proxy::SocksV5;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::{Client, ResponseFuture};
use hyper_util::rt::TokioExecutor;
use reqwest::Url;
use serde::de::DeserializeOwned;
use serde::Serialize;

use super::{INSTANCE_HEADER, NODE_HEADER};
use crate::config::Config;
use crate::error::Error;
use crate::model::validate_handle;

/// How long to wait for a TCP connection (to the peer, or to the proxy).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest JSON answer read: a page of 500 events of up to 64 KiB each fits.
const JSON_MAX: usize = 64 * 1024 * 1024;
/// How much of an error answer is kept for the log.
const ERROR_TEXT_MAX: usize = 4 * 1024;
/// A file download that gets no bytes for this long is given up.
const STALL: Duration = Duration::from_secs(30);

/// A proxy sync can use: `socks5h://host:port` (names are resolved by the
/// proxy, which is what tailscaled needs for MagicDNS names) or
/// `socks5://host:port` (resolved here). The port defaults to 1080.
/// Returns the address to dial and whether names are resolved here.
pub fn parse_proxy(s: &str) -> Option<(Uri, bool)> {
    let url = Url::parse(s.trim()).ok()?;
    let local_dns = match url.scheme() {
        "socks5h" => false,
        "socks5" => true,
        _ => return None,
    };
    // Not lowercased by the parser: socks5h is not a scheme it knows.
    let host = url
        .host_str()
        .filter(|h| !h.is_empty())?
        .to_ascii_lowercase();
    let plain = url.username().is_empty()
        && url.password().is_none()
        && matches!(url.path(), "" | "/")
        && url.query().is_none()
        && url.fragment().is_none();
    if !plain {
        return None;
    }
    let port = url.port().unwrap_or(1080);
    let uri = format!("{}://{host}:{port}", url.scheme()).parse().ok()?;
    Some((uri, local_dns))
}

#[derive(Clone)]
enum Pool {
    Direct(Client<HttpConnector, Full<Bytes>>),
    Socks(Client<SocksV5<HttpConnector>, Full<Bytes>>),
}

impl Pool {
    fn request(&self, req: Request<Full<Bytes>>) -> ResponseFuture {
        match self {
            Pool::Direct(client) => client.request(req),
            Pool::Socks(client) => client.request(req),
        }
    }
}

/// Requests to peers, each carrying the secret and our node name.
#[derive(Clone)]
pub struct PeerClient {
    pool: Pool,
    headers: HeaderMap,
    /// Our node name, and this run's id ([`INSTANCE_HEADER`]).
    node: String,
    instance: String,
    /// URLs that answered with our name from another run: other devices
    /// with the same `BEACON_NODE`, each warned about once.
    same_name: Arc<Mutex<BTreeSet<String>>>,
}

impl PeerClient {
    /// The client for `cfg`, for the run `instance`: through `cfg.proxy`
    /// when set. Fails if the secret, the node name or the proxy cannot be
    /// used.
    pub fn new(cfg: &Config, instance: &str) -> Result<PeerClient, Error> {
        let mut headers = HeaderMap::new();
        let mut bearer =
            HeaderValue::from_str(&format!("Bearer {}", cfg.secret)).map_err(|_| {
                Error::Usage("BEACON_SECRET cannot be sent in an HTTP header".to_string())
            })?;
        bearer.set_sensitive(true);
        headers.insert(AUTHORIZATION, bearer);
        let node = HeaderValue::from_str(&cfg.node)
            .map_err(|_| Error::Usage(format!("BEACON_NODE {:?} is not a valid name", cfg.node)))?;
        headers.insert(NODE_HEADER, node);
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static(concat!("beacon-sync/", env!("CARGO_PKG_VERSION"))),
        );

        let mut dialer = HttpConnector::new();
        dialer.set_connect_timeout(Some(CONNECT_TIMEOUT));
        dialer.set_nodelay(true);
        let builder = Client::builder(TokioExecutor::new());
        let pool = match &cfg.proxy {
            None => Pool::Direct(builder.build(dialer)),
            Some(proxy) => {
                let (proxy, local_dns) = parse_proxy(proxy).ok_or_else(|| {
                    Error::Usage(format!(
                        "BEACON_PROXY {proxy:?} is not a socks5h:// or socks5:// URL"
                    ))
                })?;
                // The dialer connects to the proxy, whose address is not http://.
                dialer.enforce_http(false);
                Pool::Socks(builder.build(SocksV5::new(proxy, dialer).local_dns(local_dns)))
            }
        };
        Ok(PeerClient {
            pool,
            headers,
            node: cfg.node.clone(),
            instance: instance.to_string(),
            same_name: Arc::default(),
        })
    }

    /// This run's id.
    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// The URLs that answered with our name from another run, sorted.
    pub fn same_name(&self) -> Vec<String> {
        lock(&self.same_name).iter().cloned().collect()
    }

    /// The node that signed an answer from `base`. One with our name but
    /// another run's id is another device given the same `BEACON_NODE`
    /// (our own address under another name answers with our run's id):
    /// warned about once per URL and remembered for `peers`. Either way the
    /// name is returned, and sync never syncs with that URL, as it cannot
    /// tell the two apart.
    fn signed(&self, base: &str, node: String, instance: Option<String>) -> String {
        let other_run = instance.is_some_and(|run| run != self.instance);
        if node == self.node && other_run && lock(&self.same_name).insert(base.to_string()) {
            tracing::warn!(
                url = %base,
                "another device is also named {node}; give each device its own BEACON_NODE"
            );
        }
        node
    }

    async fn send(
        &self,
        method: Method,
        uri: Uri,
        json: Option<Vec<u8>>,
    ) -> anyhow::Result<Response<Incoming>> {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(headers) = req.headers_mut() {
            headers.extend(self.headers.clone());
            if json.is_some() {
                headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            }
        }
        let req = req.body(Full::new(Bytes::from(json.unwrap_or_default())))?;
        Ok(self.pool.request(req).await?)
    }

    /// `GET {base}{path}`, answered with JSON signed by a node; all of it
    /// within `timeout`. Returns the node and the answer.
    pub async fn get_json<T: DeserializeOwned>(
        &self,
        base: &str,
        path: &str,
        timeout: Duration,
    ) -> anyhow::Result<(String, T)> {
        let uri = endpoint(base, path)?;
        let (node, run, answer) = within(timeout, async {
            let resp = self.send(Method::GET, uri, None).await?;
            read_json(resp).await
        })
        .await?;
        Ok((self.signed(base, node, run), answer))
    }

    /// `POST {base}{path}` with `body` as JSON, answered with JSON signed by
    /// a node; all of it within `timeout`.
    pub async fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        base: &str,
        path: &str,
        body: &B,
        timeout: Duration,
    ) -> anyhow::Result<(String, T)> {
        let uri = endpoint(base, path)?;
        let json = serde_json::to_vec(body)?;
        let (node, run, answer) = within(timeout, async {
            let resp = self.send(Method::POST, uri, Some(json)).await?;
            read_json(resp).await
        })
        .await?;
        Ok((self.signed(base, node, run), answer))
    }

    /// `GET {base}{path}` as a stream of bytes: the answer must start within
    /// `timeout`, be a success, and say it holds at most `limit` bytes if it
    /// says; then each chunk must come within 30 s of the last.
    pub async fn download(
        &self,
        base: &str,
        path: &str,
        timeout: Duration,
        limit: u64,
    ) -> anyhow::Result<impl Stream<Item = Result<Bytes, Error>> + Send + 'static> {
        let uri = endpoint(base, path)?;
        let resp = within(timeout, self.send(Method::GET, uri, None)).await?;
        let resp = success(resp).await?;
        let length = resp
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        if let Some(length) = length {
            if length > limit {
                bail!("{length} bytes, more than the {limit} expected");
            }
        }
        Ok(stall_guard(resp.into_body().into_data_stream()))
    }
}

/// `fut`, or an error once `timeout` has passed.
async fn within<T>(
    timeout: Duration,
    fut: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| anyhow!("no answer within {timeout:?}"))?
}

/// `{base}{path}` as a URI with the port written out: the SOCKS client takes
/// a missing port to mean 443.
fn endpoint(base: &str, path: &str) -> anyhow::Result<Uri> {
    let url = Url::parse(base).with_context(|| format!("bad peer URL {base:?}"))?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("peer URL {base:?} has no host"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("peer URL {base:?} has no port"))?;
    Ok(format!("http://{host}:{port}{path}").parse()?)
}

/// The answer if it is a success, else an error with what the peer said.
async fn success(resp: Response<Incoming>) -> anyhow::Result<Response<Incoming>> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let text = match Limited::new(resp.into_body(), ERROR_TEXT_MAX)
        .collect()
        .await
    {
        Ok(body) => String::from_utf8_lossy(&body.to_bytes()).into_owned(),
        Err(_) => String::new(),
    };
    bail!("HTTP {status}: {}", text.trim())
}

/// The node an answer is signed with, the run of it that answered (if it
/// says: an older build does not), and its JSON body.
async fn read_json<T: DeserializeOwned>(
    resp: Response<Incoming>,
) -> anyhow::Result<(String, Option<String>, T)> {
    let resp = success(resp).await?;
    let node = resp
        .headers()
        .get(NODE_HEADER)
        .ok_or_else(|| anyhow!("the answer names no node: not a beacon node?"))?
        .to_str()
        .context("the node name in the answer is not text")?;
    let node = validate_handle(node).map_err(|e| anyhow!("the answer's node name: {e}"))?;
    let run = resp
        .headers()
        .get(INSTANCE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = Limited::new(resp.into_body(), JSON_MAX)
        .collect()
        .await
        .map_err(|e| anyhow!("cannot read the answer: {e}"))?
        .to_bytes();
    let value = serde_json::from_slice(&body).context("a bad answer")?;
    Ok((node, run, value))
}

/// Lock a std mutex; a panic elsewhere leaves the data usable.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The body's chunks, failing if none comes for [`STALL`].
fn stall_guard<S>(body: S) -> impl Stream<Item = Result<Bytes, Error>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, hyper::Error>> + Send + 'static,
{
    futures_util::stream::unfold(Some(Box::pin(body)), |body| async move {
        let mut body = body?;
        match tokio::time::timeout(STALL, body.next()).await {
            Ok(Some(Ok(chunk))) => Some((Ok(chunk), Some(body))),
            Ok(Some(Err(e))) => Some((Err(Error::Unreachable(e.to_string())), None)),
            Ok(None) => None,
            Err(_) => Some((
                Err(Error::Unreachable(format!("no data for {STALL:?}"))),
                None,
            )),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxies_are_socks5() {
        let (uri, local) = parse_proxy("socks5h://127.0.0.1:1055").unwrap();
        assert_eq!(uri.to_string(), "socks5h://127.0.0.1:1055/");
        assert!(!local);
        let (uri, local) = parse_proxy(" SOCKS5://Proxy ").unwrap();
        assert_eq!((uri.host(), uri.port_u16()), (Some("proxy"), Some(1080)));
        assert!(local);
        for bad in [
            "",
            "127.0.0.1:1055",
            "http://127.0.0.1:3128",
            "socks4://127.0.0.1:1080",
            "socks5h://",
            "socks5h://u:p@127.0.0.1:1055",
            "socks5h://127.0.0.1:not-a-port",
            "socks5h://127.0.0.1:1055/x",
        ] {
            assert_eq!(parse_proxy(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn endpoints_spell_out_the_port() {
        let uri = endpoint("http://mac", "/api/events?wait=25").unwrap();
        assert_eq!(uri.to_string(), "http://mac:80/api/events?wait=25");
        let uri = endpoint("http://[fd7a::1]:7700", "/api/files/ab").unwrap();
        assert_eq!(uri.to_string(), "http://[fd7a::1]:7700/api/files/ab");
    }
}
