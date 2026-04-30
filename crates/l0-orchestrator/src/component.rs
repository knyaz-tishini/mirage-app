//! Trait abstraction over "a thing the orchestrator runs".
//!
//! Two concrete implementations:
//! * [`crate::process::ProcessComponent`] — OS subprocess (desktop).
//! * [`crate::task::TaskComponent`] — embedded `tokio::task` (mobile).

use async_trait::async_trait;

/// Process or task exit status, normalised to an i32 code.
///
/// `0` means clean exit; non-zero is failure (or `-1` if the platform did not
/// surface a code, e.g. signal-killed).
#[derive(Debug, Clone, Copy)]
pub struct ExitStatus(pub i32);

impl ExitStatus {
    pub fn is_success(&self) -> bool { self.0 == 0 }
}

/// One running component (process or task). Returned by [`ComponentSpawner::spawn`].
///
/// `wait()` resolves when the component exits. `kill()` requests termination
/// (best-effort — mobile task mode honours cooperative cancellation only).
#[async_trait]
pub trait ComponentHandle: Send {
    async fn wait(&mut self) -> anyhow::Result<ExitStatus>;
    async fn kill(&mut self) -> anyhow::Result<()>;
    fn name(&self) -> &str;
}

/// Component-side metadata. The supervisor uses [`ComponentSpawner`] to spawn
/// a [`ComponentHandle`] without caring whether it's a process or a task.
#[derive(Debug, Clone)]
pub struct Component {
    pub name: String,
    pub kind: crate::config::ComponentKind,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Platform-specific way to start a [`Component`].
///
/// Desktop builds inject [`crate::process::ProcessSpawner`]; mobile builds inject
/// [`crate::task::TaskSpawner`]. The supervisor calls `spawn` once per component
/// and reuses the resulting [`ComponentHandle`] until it exits.
#[async_trait]
pub trait ComponentSpawner: Send + Sync {
    async fn spawn(&self, component: &Component) -> anyhow::Result<Box<dyn ComponentHandle>>;
}
