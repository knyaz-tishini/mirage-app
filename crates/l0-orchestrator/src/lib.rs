//! mirage-l0-orchestrator — runs and supervises L0 components.
//!
//! Two execution modes:
//!
//! * **Process mode** (desktop): each component is a child OS process. Crashes
//!   are isolated; soft-restart (e.g. OTA) just respawns the affected child.
//!   This is what the desktop binary uses.
//!
//! * **Task mode** (mobile): each component is a `tokio::task` inside the
//!   orchestrator process. Mobile platforms (Android/iOS) restrict process
//!   spawning, so cli/sentinel run as embedded async functions via their
//!   library `run(...)` entry points.
//!
//! Components implement [`Component`]; spawners implement [`ComponentSpawner`].
//! The orchestrator iterates a config-driven component list, asks each
//! component to start through the platform's spawner, and supervises with
//! per-component restart policy.

pub mod component;
pub mod config;
pub mod process;
pub mod task;
pub mod supervisor;

pub use component::{Component, ComponentHandle, ComponentSpawner, ExitStatus};
pub use config::{OrchestratorConfig, ComponentConfig, ComponentKind, RestartPolicy};
pub use supervisor::Supervisor;
