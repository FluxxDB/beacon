//! Server configuration, read from the `BEACON_*` environment variables.

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::error::Error;
use crate::model::validate_handle;

const DEFAULT_LISTEN: &str = "0.0.0.0:7700";
const DEFAULT_HUMAN: &str = "Dang";
const DEFAULT_MAX_ATTACHMENT_MB: u64 = 50;
/// How long a stream may be silent before it sends a keep-alive comment.
const DEFAULT_SSE_KEEP_ALIVE: Duration = Duration::from_secs(15);
const DEFAULT_DATA: &str = "/data";

/// Everything `beacon serve` needs to know.
#[derive(Clone)]
pub struct Config {
    /// This device's name (`BEACON_NODE`): a handle-like name, unique per device.
    pub node: String,
    /// The URL peers reach this node at (`BEACON_URL`), announced to them.
    pub url: Option<String>,
    /// Peer URLs to sync with (`BEACON_PEERS`, comma-separated), as written;
    /// sync normalizes and dedupes them.
    pub peers: Vec<String>,
    /// The bearer token every request must carry (`BEACON_SECRET`).
    pub secret: String,
    /// Where the HTTP server listens (`BEACON_LISTEN`).
    pub listen: SocketAddr,
    /// The handle of the person at the TUI (`BEACON_HUMAN`).
    pub human: String,
    /// Largest attachment, in MiB (`BEACON_MAX_ATTACHMENT_MB`).
    pub max_attachment_mb: u64,
    /// Proxy for outbound peer requests (`BEACON_PROXY`), e.g. `socks5h://127.0.0.1:1055`.
    pub proxy: Option<String>,
    /// Where the database and attachment files live (`BEACON_DATA`).
    pub data_dir: PathBuf,
    /// How long `GET /api/stream` may be silent before it sends a keep-alive
    /// comment (15 s). Not a setting: tests shorten it.
    #[doc(hidden)]
    pub sse_keep_alive: Duration,
}

impl Config {
    /// A config for node `node` with every optional setting at its default.
    /// Nothing is validated; [`Config::from_vars`] does that for real input.
    pub fn new(node: &str, secret: &str, data_dir: impl Into<PathBuf>) -> Config {
        Config {
            node: node.to_string(),
            url: None,
            peers: Vec::new(),
            secret: secret.to_string(),
            listen: DEFAULT_LISTEN.parse().expect("default listen address"),
            human: DEFAULT_HUMAN.to_string(),
            max_attachment_mb: DEFAULT_MAX_ATTACHMENT_MB,
            proxy: None,
            data_dir: data_dir.into(),
            sse_keep_alive: DEFAULT_SSE_KEEP_ALIVE,
        }
    }

    /// Reads the configuration from the process environment.
    pub fn from_env() -> Result<Config, Error> {
        Config::from_vars(|key| std::env::var(key).ok())
    }

    /// Reads the configuration through `get`, which returns a variable's
    /// value. An empty or blank value counts as unset (a compose env file
    /// passes `VAR=` through as an empty string). A bad value is
    /// [`Error::Usage`] naming the variable.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Config, Error> {
        let var = |key: &str| {
            get(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let required =
            |key: &str| var(key).ok_or_else(|| Error::Usage(format!("{key} is required")));
        let invalid =
            |key: &str, why: &dyn fmt::Display| Error::Usage(format!("{key} is invalid: {why}"));

        let node = required("BEACON_NODE")?;
        validate_handle(&node).map_err(|e| invalid("BEACON_NODE", &e))?;
        // Kept exactly as given: a client sends whatever its own variable holds.
        let secret = get("BEACON_SECRET")
            .filter(|v| !v.is_empty())
            .ok_or_else(|| Error::Usage("BEACON_SECRET is required".to_string()))?;

        let mut cfg = Config::new(
            &node,
            &secret,
            var("BEACON_DATA").unwrap_or_else(|| DEFAULT_DATA.to_string()),
        );
        if let Some(listen) = var("BEACON_LISTEN") {
            cfg.listen = listen.parse().map_err(|_| {
                invalid(
                    "BEACON_LISTEN",
                    &format_args!("{listen:?} is not a host:port address"),
                )
            })?;
        }
        if let Some(human) = var("BEACON_HUMAN") {
            cfg.human = validate_handle(&human).map_err(|e| invalid("BEACON_HUMAN", &e))?;
        }
        if let Some(mb) = var("BEACON_MAX_ATTACHMENT_MB") {
            cfg.max_attachment_mb = match mb.parse::<u64>() {
                Ok(n) if n > 0 => n,
                _ => {
                    return Err(invalid(
                        "BEACON_MAX_ATTACHMENT_MB",
                        &format_args!("{mb:?} is not a whole number of megabytes, at least 1"),
                    ))
                }
            };
        }
        cfg.url = var("BEACON_URL");
        cfg.proxy = var("BEACON_PROXY");
        cfg.peers = var("BEACON_PEERS")
            .map(|peers| {
                peers
                    .split(',')
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        Ok(cfg)
    }

    /// The attachment size limit in bytes.
    pub fn max_attachment_bytes(&self) -> u64 {
        self.max_attachment_mb.saturating_mul(1024 * 1024)
    }
}

/// Like the derived `Debug`, but never prints the secret.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("node", &self.node)
            .field("url", &self.url)
            .field("peers", &self.peers)
            .field("secret", &"<redacted>")
            .field("listen", &self.listen)
            .field("human", &self.human)
            .field("max_attachment_mb", &self.max_attachment_mb)
            .field("proxy", &self.proxy)
            .field("data_dir", &self.data_dir)
            .field("sse_keep_alive", &self.sse_keep_alive)
            .finish()
    }
}
