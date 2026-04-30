//! Per-component restart loop.
//!
//! For each [`crate::config::ComponentConfig`] the supervisor:
//! 1. Calls the platform-specific spawner to start it
//! 2. Awaits exit
//! 3. Applies [`crate::config::RestartPolicy`] (Always / OnFailure / Never)
//! 4. On restart, exponential backoff capped at 30 s
//!
//! Components run independently; a single component crashing does not stop
//! the others. Ctrl-C cascades `kill()` to every active handle.

use std::time::Duration;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::component::{Component, ComponentHandle};
use crate::config::{ComponentConfig, ExecutionMode, OrchestratorConfig, RestartPolicy};

pub struct Supervisor {
    cfg: OrchestratorConfig,
    shutdown: CancellationToken,
}

impl Supervisor {
    pub fn new(cfg: OrchestratorConfig) -> Self {
        Self { cfg, shutdown: CancellationToken::new() }
    }

    pub fn shutdown_token(&self) -> CancellationToken { self.shutdown.clone() }

    /// Start all components and wait until all per-component loops exit
    /// (either naturally or after shutdown was requested).
    pub async fn run(self) -> anyhow::Result<()> {
        let mode = self.cfg.orchestrator.mode;
        info!(?mode, components = self.cfg.components.len(), "orchestrator: starting");

        let (kill_tx, _) = watch::channel(false);

        let mut handles = Vec::new();
        for cc in self.cfg.components.iter().cloned() {
            let kill_rx = kill_tx.subscribe();
            let shutdown = self.shutdown.clone();
            let h = tokio::spawn(async move {
                supervise_one(cc, mode, kill_rx, shutdown).await;
            });
            handles.push(h);
        }

        // Wait for shutdown signal (Ctrl-C or external token).
        tokio::select! {
            _ = self.shutdown.cancelled() => {
                info!("orchestrator: shutdown token fired");
            }
            _ = tokio::signal::ctrl_c() => {
                info!("orchestrator: Ctrl-C received");
                self.shutdown.cancel();
            }
        }

        // Broadcast "stop" to every per-component loop.
        let _ = kill_tx.send(true);

        // Collect all per-component supervision loops with a deadline.
        let drain = async {
            for h in handles { let _ = h.await; }
        };
        match tokio::time::timeout(Duration::from_secs(10), drain).await {
            Ok(()) => info!("orchestrator: all components stopped"),
            Err(_) => warn!("orchestrator: 10s drain timeout exceeded — exiting anyway"),
        }
        Ok(())
    }
}

async fn supervise_one(
    cc: ComponentConfig,
    mode: ExecutionMode,
    mut kill_rx: watch::Receiver<bool>,
    shutdown: CancellationToken,
) {
    let mut backoff = Duration::from_millis(500);

    loop {
        if *kill_rx.borrow() || shutdown.is_cancelled() {
            info!(component = %cc.name, "supervisor: shutdown — exiting loop");
            return;
        }

        let component = Component {
            name: cc.name.clone(),
            kind: cc.kind,
            args: cc.args.clone(),
            env: parse_env(&cc.env),
        };

        let handle_res = match mode {
            ExecutionMode::Process => {
                let Some(bin) = cc.binary.as_ref() else {
                    error!(component = %cc.name, "supervisor: process mode requires `binary`");
                    return;
                };
                crate::process::spawn_process(&component, bin).await
            }
            ExecutionMode::Task => crate::task::spawn_task(&component).await,
        };

        let mut handle: Box<dyn ComponentHandle> = match handle_res {
            Ok(h) => h,
            Err(e) => {
                error!(component = %cc.name, error = %e, "supervisor: spawn failed");
                if !apply_backoff(&mut backoff, &mut kill_rx).await { return; }
                continue;
            }
        };
        info!(component = %cc.name, "supervisor: started");

        let exit_code = tokio::select! {
            r = handle.wait() => match r {
                Ok(code) => {
                    info!(component = %cc.name, code = code.0, "supervisor: component exited");
                    code.0
                }
                Err(e) => {
                    error!(component = %cc.name, error = %e, "supervisor: wait failed");
                    -1
                }
            },
            _ = kill_rx.changed() => {
                if *kill_rx.borrow() {
                    info!(component = %cc.name, "supervisor: stopping component");
                    let _ = handle.kill().await;
                    let _ = tokio::time::timeout(Duration::from_secs(5), handle.wait()).await;
                    return;
                }
                continue;
            }
        };

        match cc.restart {
            RestartPolicy::Never => {
                info!(component = %cc.name, "supervisor: restart=never — done");
                return;
            }
            RestartPolicy::OnFailure if exit_code == 0 => {
                info!(component = %cc.name, "supervisor: clean exit + restart=on-failure — done");
                return;
            }
            _ => {
                warn!(
                    component = %cc.name,
                    exit_code,
                    backoff_ms = backoff.as_millis(),
                    "supervisor: restarting after backoff"
                );
                if !apply_backoff(&mut backoff, &mut kill_rx).await { return; }
            }
        }
    }
}

fn parse_env(env: &[String]) -> Vec<(String, String)> {
    env.iter()
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

/// Sleep `backoff` (cancellable). Doubles up to 30s for next iteration.
/// Returns `false` if shutdown fired during sleep.
async fn apply_backoff(
    backoff: &mut Duration,
    kill_rx: &mut watch::Receiver<bool>,
) -> bool {
    let cur = *backoff;
    *backoff = (*backoff * 2).min(Duration::from_secs(30));
    tokio::select! {
        _ = tokio::time::sleep(cur) => true,
        _ = kill_rx.changed() => !*kill_rx.borrow(),
    }
}
