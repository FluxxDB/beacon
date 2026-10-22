//! Helpers shared by the integration tests: start a real node on a free port
//! with a throwaway data directory, and talk to it over HTTP with the right
//! secret. Each test binary uses a different subset, hence `allow(dead_code)`.
//!
//! To build a different kind of node (several peers, a small attachment limit)
//! use [`spawn_node_with`] to tweak the [`Config`]; to take a node down and
//! bring it back on the same data directory and port, use [`TestNode::stop`]
//! and [`StoppedNode::restart`]. Nodes that must know each other's URLs
//! before they start (peers, `BEACON_URL`) take a port with [`reserve`] first.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::Path;

use beacon::config::Config;
use beacon::server::{AppState, NodeHandle};
use reqwest::{Client, RequestBuilder};
use tempfile::TempDir;

/// The shared secret every test node is started with.
pub const SECRET: &str = "test-secret";

/// A running node, its data directory and a client that already knows the
/// secret.
pub struct TestNode {
    pub name: String,
    /// `http://127.0.0.1:<port>`.
    pub url: String,
    pub secret: String,
    pub handle: NodeHandle,
    pub dir: TempDir,
    /// The config the node was started with (its `listen` may be port 0).
    pub cfg: Config,
    client: Client,
}

/// A node that was stopped: everything needed to start it again on the same
/// data directory and address.
pub struct StoppedNode {
    pub name: String,
    pub addr: SocketAddr,
    pub dir: TempDir,
    pub cfg: Config,
}

/// A config for node `name` that listens on a free loopback port and keeps its
/// data in `dir`.
pub fn test_config(name: &str, peers: &[&str], dir: &Path) -> Config {
    let mut cfg = Config::new(name, SECRET, dir);
    cfg.listen = "127.0.0.1:0".parse().unwrap();
    cfg.peers = peers.iter().map(|p| p.to_string()).collect();
    cfg
}

/// A loopback port already bound for a node that has not started yet, so its
/// URL can go into configs (its own `BEACON_URL`, other nodes' peers) before
/// it runs. The node is served on this very socket: no other test can take
/// the port in between.
pub struct Reserved {
    /// `http://127.0.0.1:<port>`.
    pub url: String,
    listener: std::net::TcpListener,
}

/// Binds a free loopback port for a node started later with [`Reserved::start`].
pub fn reserve() -> Reserved {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    Reserved { url, listener }
}

impl Reserved {
    /// Starts node `name` on the reserved port, with the given peer URLs and
    /// a chance to change the config first.
    pub async fn start(
        self,
        name: &str,
        peers: &[&str],
        tweak: impl FnOnce(&mut Config),
    ) -> TestNode {
        let dir = TempDir::new().unwrap();
        let mut cfg = test_config(name, peers, dir.path());
        cfg.listen = self.listener.local_addr().unwrap();
        tweak(&mut cfg);
        let handle = beacon::server::start_on(cfg.clone(), self.listener)
            .await
            .expect("start node");
        TestNode::from_handle(cfg, dir, handle)
    }
}

/// Starts node `name` with the given peer URLs.
pub async fn spawn_node(name: &str, peers: &[&str]) -> TestNode {
    spawn_node_with(name, peers, |_| {}).await
}

/// [`spawn_node`], with a chance to change the config first.
pub async fn spawn_node_with(
    name: &str,
    peers: &[&str],
    tweak: impl FnOnce(&mut Config),
) -> TestNode {
    let dir = TempDir::new().unwrap();
    let mut cfg = test_config(name, peers, dir.path());
    tweak(&mut cfg);
    TestNode::start(cfg, dir).await
}

impl TestNode {
    /// Starts a node from `cfg`, whose data lives in `dir`.
    pub async fn start(cfg: Config, dir: TempDir) -> TestNode {
        let handle = beacon::server::start(cfg.clone())
            .await
            .expect("start node");
        TestNode::from_handle(cfg, dir, handle)
    }

    fn from_handle(cfg: Config, dir: TempDir, handle: NodeHandle) -> TestNode {
        TestNode {
            name: cfg.node.clone(),
            url: format!("http://{}", handle.addr),
            secret: cfg.secret.clone(),
            handle,
            dir,
            cfg,
            client: Client::new(),
        }
    }

    /// Shuts the node down, waiting until its port and database are free.
    pub async fn stop(self) -> StoppedNode {
        let addr = self.handle.addr;
        self.handle.shutdown().await;
        StoppedNode {
            name: self.name,
            addr,
            dir: self.dir,
            cfg: self.cfg,
        }
    }

    pub fn state(&self) -> &AppState {
        self.handle.state()
    }

    /// An authenticated request to `path` (`/api/...`).
    pub fn request(&self, method: reqwest::Method, path: &str) -> RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(&self.secret)
    }

    pub fn get(&self, path: &str) -> RequestBuilder {
        self.request(reqwest::Method::GET, path)
    }

    pub fn post(&self, path: &str) -> RequestBuilder {
        self.request(reqwest::Method::POST, path)
    }

    pub fn put(&self, path: &str) -> RequestBuilder {
        self.request(reqwest::Method::PUT, path)
    }

    pub fn delete(&self, path: &str) -> RequestBuilder {
        self.request(reqwest::Method::DELETE, path)
    }
}

impl StoppedNode {
    /// Starts the node again on the same data directory and the same address.
    pub async fn restart(self) -> TestNode {
        let mut cfg = self.cfg;
        cfg.listen = self.addr;
        TestNode::start(cfg, self.dir).await
    }
}
