//! Task-mode spawner: each component is an embedded `tokio::task`.
//!
//! Used on mobile (Android/iOS) where the orchestrator runs as a single
//! process and spawning subprocesses is restricted or unavailable.
//!
//! Components must expose a library `run()` entry point. We dispatch on
//! [`crate::config::ComponentKind`] and call the matching crate.

use async_trait::async_trait;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::component::{Component, ComponentHandle, ExitStatus};
use crate::config::ComponentKind;

/// Spawn a component as an embedded tokio task.
///
/// `args` is parsed by the component-specific adapter — typically expected
/// to be `["--config", "<path>"]`.
pub async fn spawn_task(component: &Component) -> anyhow::Result<Box<dyn ComponentHandle>> {
    info!(
        name = %component.name,
        kind = ?component.kind,
        args = ?component.args,
        "orchestrator: spawning task"
    );

    let cancel = CancellationToken::new();
    let cancel_child = cancel.clone();
    let name = component.name.clone();
    let kind = component.kind;
    let args = component.args.clone();

    let join: JoinHandle<i32> = tokio::spawn(async move {
        let result = tokio::select! {
            _ = cancel_child.cancelled() => {
                warn!(%name, "task component: cancellation requested");
                Ok(0)
            }
            r = run_component(kind, args) => r,
        };
        match result {
            Ok(code) => code,
            Err(e) => {
                warn!(%name, error = %e, "task component returned error");
                1
            }
        }
    });

    Ok(Box::new(TaskHandle {
        name: component.name.clone(),
        join: Some(join),
        cancel,
    }))
}

async fn run_component(
    kind: ComponentKind,
    args: Vec<String>,
) -> anyhow::Result<i32> {
    match kind {
        ComponentKind::Sentinel => {
            let cfg_path = extract_config_path(&args)
                .ok_or_else(|| anyhow::anyhow!("sentinel task requires --config <path>"))?;
            let raw = std::fs::read_to_string(&cfg_path)
                .map_err(|e| anyhow::anyhow!("read sentinel config: {}", e))?;
            let mut cfg: mirage_l0_sentinel::SentinelConfig =
                toml::from_str(&raw).map_err(|e| anyhow::anyhow!("parse sentinel config: {}", e))?;
            // Embedded mode: don't install panic hook, don't exit on memory fatal.
            cfg.install_panic_hook = false;
            cfg.exit_on_memory_fatal = false;
            mirage_l0_sentinel::run(cfg).await?;
            Ok(0)
        }
        ComponentKind::Cli => {
            let cfg_path = extract_config_path(&args)
                .ok_or_else(|| anyhow::anyhow!("cli task requires --config <path>"))?;
            let opts = mirage_client_cli::CliRunOptions {
                config_path: cfg_path.into(),
                bind: extract_bind(&args),
                install_panic_hook: false,
                install_memory_guard: false,
                exit_on_memory_fatal: false,
            };
            mirage_client_cli::run(opts)
                .await
                .map_err(|e| anyhow::anyhow!("cli run failed: {}", e))?;
            Ok(0)
        }
        ComponentKind::Generic => {
            anyhow::bail!("Generic component kind cannot run in task mode — use process mode")
        }
    }
}

fn extract_config_path(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == "--config" || a == "-c" {
            return iter.next().cloned();
        }
        if let Some(rest) = a.strip_prefix("--config=") {
            return Some(rest.to_string());
        }
    }
    None
}

fn extract_bind(args: &[String]) -> Option<std::net::SocketAddr> {
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == "--bind" || a == "-b" {
            return iter.next().and_then(|s| s.parse().ok());
        }
        if let Some(rest) = a.strip_prefix("--bind=") {
            return rest.parse().ok();
        }
    }
    None
}

struct TaskHandle {
    name: String,
    join: Option<JoinHandle<i32>>,
    cancel: CancellationToken,
}

#[async_trait]
impl ComponentHandle for TaskHandle {
    async fn wait(&mut self) -> anyhow::Result<ExitStatus> {
        let Some(handle) = self.join.take() else {
            return Ok(ExitStatus(-1));
        };
        match handle.await {
            Ok(code) => Ok(ExitStatus(code)),
            Err(e) if e.is_cancelled() => Ok(ExitStatus(0)),
            Err(e) => Err(anyhow::anyhow!("task join error: {}", e)),
        }
    }

    async fn kill(&mut self) -> anyhow::Result<()> {
        self.cancel.cancel();
        // Give the task a moment to observe cancellation; non-blocking.
        Ok(())
    }

    fn name(&self) -> &str { &self.name }
}

