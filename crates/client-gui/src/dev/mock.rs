//! Mock engine — lets the UI run on a development box with no L0 stack, no
//! sockets and no child processes. Dev-tools only.
//!
//! Connecting takes [`CONNECT_DELAY`] to come up; disconnecting is immediate.
//! `initially_up` starts the tunnel already alive (`--connected`, used for
//! deterministic screenshot capture).

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::backend::{EngineLink, EngineStatus};

/// How long a mock connect takes before the tunnel reads as alive.
const CONNECT_DELAY: Duration = Duration::from_millis(700);

struct MockState {
    alive: bool,
    connecting_since: Option<Instant>,
}

/// In-process stand-in for the sentinel link.
pub struct MockEngine {
    state: Mutex<MockState>,
}

impl MockEngine {
    pub fn new(initially_up: bool) -> Self {
        Self {
            state: Mutex::new(MockState {
                alive: initially_up,
                connecting_since: None,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, MockState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl EngineLink for MockEngine {
    fn status(&self) -> EngineStatus {
        let mut st = self.lock();
        if st
            .connecting_since
            .is_some_and(|at| at.elapsed() >= CONNECT_DELAY)
        {
            st.alive = true;
            st.connecting_since = None;
        }
        EngineStatus {
            online: true,
            tun_alive: st.alive,
            socks: st.alive,
            mode: "socks".to_owned(),
            ..EngineStatus::default()
        }
    }

    fn connect(&self) {
        let mut st = self.lock();
        if !st.alive && st.connecting_since.is_none() {
            st.connecting_since = Some(Instant::now());
        }
    }

    fn disconnect(&self) {
        let mut st = self.lock();
        st.alive = false;
        st.connecting_since = None;
    }

    fn is_bound(&self) -> bool {
        true
    }

    fn bound_name(&self) -> Option<String> {
        Some("device".to_owned())
    }
}
