//! Peer sync. Task 7 replaces this stub with the real engine; until then the
//! node has no peers, nothing is queued and every push reaches nobody.

use crate::api::{Delivery, PeerJson};
use crate::model::Event;

/// The server's handle on the sync engine.
#[derive(Debug, Clone, Default)]
pub struct SyncHandle {}

impl SyncHandle {
    pub fn new() -> SyncHandle {
        SyncHandle::default()
    }

    /// A write was stored on this node: tell the pushers there is something to send.
    pub fn notify(&self) {}

    /// Pushes `ev` to every peer right away and reports who got it. With no
    /// peers there is nothing to report. (Async because the real one waits on
    /// the network.)
    pub async fn push_now(&self, _ev: &Event) -> Vec<Delivery> {
        Vec::new()
    }

    /// The peers this node syncs with, with their state.
    pub fn peers(&self) -> Vec<PeerJson> {
        Vec::new()
    }
}
