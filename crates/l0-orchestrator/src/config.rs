//! `orchestrator.toml` schema.
//!
//! ```toml
//! [orchestrator]
//! mode = "process"   # or "task"
//!
//! [[component]]
//! name    = "sentinel"
//! kind    = "sentinel"
//! binary  = "C:/.../mirage-l0-sentinel.exe"   # process mode only
//! args    = ["--config", "C:/.../sentinel.toml"]
//! restart = "always"                          # always | on-failure | never
//!
//! [[component]]
//! name    = "cli"
//! kind    = "cli"
//! # No binary line — sentinel will spawn cli on its own (legacy desktop path).
//! # If orchestrator should manage cli directly, set:
//! # binary = ".../mirage-client-cli.exe"
//! # args   = ["--config", ".../client-config.json"]
//! restart = "on-failure"
//! ```

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct OrchestratorConfig {
    pub orchestrator: OrchestratorMeta,
    #[serde(rename = "component", default)]
    pub components: Vec<ComponentConfig>,
}

#[derive(Debug, Deserialize)]
pub struct OrchestratorMeta {
    /// `"process"` (desktop, default) or `"task"` (mobile, embedded).
    #[serde(default = "default_mode")]
    pub mode: ExecutionMode,
}

fn default_mode() -> ExecutionMode { ExecutionMode::Process }

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionMode {
    Process,
    Task,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ComponentConfig {
    /// Stable identifier (used for logs, restart bookkeeping).
    pub name: String,
    /// Which embedded `run()` function to call in task mode, or which
    /// well-known binary to spawn in process mode.
    pub kind: ComponentKind,
    /// Path to executable (process mode only).
    #[serde(default)]
    pub binary: Option<std::path::PathBuf>,
    /// Args passed to either the binary or the embedded `run()` adapter.
    /// Sentinel/cli expect `["--config", "<path>"]`.
    #[serde(default)]
    pub args: Vec<String>,
    /// `KEY=VALUE` env vars (process mode); ignored in task mode.
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default = "default_restart")]
    pub restart: RestartPolicy,
}

fn default_restart() -> RestartPolicy { RestartPolicy::OnFailure }

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Always,
    OnFailure,
    Never,
}

/// Which component flavour — drives task-mode dispatch (which `run()` to call)
/// and influences default args in process mode.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ComponentKind {
    Sentinel,
    Cli,
    /// Generic — orchestrator just spawns the binary; not embeddable in task mode.
    Generic,
}

impl OrchestratorConfig {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read orchestrator.toml at {:?}: {}", path, e))?;
        toml::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("parse orchestrator.toml: {}", e))
    }
}
