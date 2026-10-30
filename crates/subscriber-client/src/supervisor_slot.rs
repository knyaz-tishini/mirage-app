//! The live edge-session supervisor, swappable while the local proxy runs.
//!
//! The configuration decides the target and the protocol. When the target
//! changes, the session must change with it — but the process, the local
//! listener and the tunnel device must not. This slot is that seam: the
//! listener reads the supervisor that is live when a connection arrives, so a
//! new configuration replaces the session in place instead of tearing the
//! whole client down.
//!
//! The old supervisor is dropped on replacement. Dropping it aborts its dial
//! loop (see [`crate::session::EdgeSessionSupervisor`]'s `Drop`), so the
//! previous target is never dialed behind the new one.

use std::sync::{Arc, RwLock};

use crate::session::EdgeSessionSupervisor;

/// A handle to the live supervisor. Cheap to clone, safe to swap.
pub struct SupervisorSlot {
    current: RwLock<Arc<EdgeSessionSupervisor>>,
}

impl SupervisorSlot {
    /// Wrap a live supervisor.
    pub fn new(supervisor: Arc<EdgeSessionSupervisor>) -> Arc<Self> {
        Arc::new(Self { current: RwLock::new(supervisor) })
    }

    /// The supervisor that is live right now.
    pub fn current(&self) -> Arc<EdgeSessionSupervisor> {
        self.current
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    /// Swap in a new supervisor. The old one's dial loop stops with it.
    pub fn replace(&self, next: Arc<EdgeSessionSupervisor>) {
        let previous = {
            let mut slot = self.current.write().unwrap_or_else(|poison| poison.into_inner());
            std::mem::replace(&mut *slot, next)
        };
        previous.stop();
    }
}
