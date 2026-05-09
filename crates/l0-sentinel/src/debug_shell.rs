//! Debug-shell child manager.
//!
//! When the monitor pushes `WsServerFrame::DebugTunnelOpen`, sentinel writes
//! the payload to `<state_dir>/debug-tunnel.json` and spawns
//! `mirage-debug-shell --config <path>`. On `DebugTunnelClose` (or sentinel
//! shutdown) the child is SIGTERMed.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use mirage_client_core::monitor_link::DebugTunnelOpen;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{info, warn};

pub struct DebugShellManager {
    binary: PathBuf,
    state_dir: PathBuf,
    child: Mutex<Option<Child>>,
}

impl DebugShellManager {
    pub fn new(binary: PathBuf, state_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            binary,
            state_dir,
            child: Mutex::new(None),
        })
    }

    /// Persist the tunnel payload + spawn (or replace) the child process.
    pub async fn open(&self, payload: &DebugTunnelOpen) -> Result<()> {
        if let Err(e) = tokio::fs::create_dir_all(&self.state_dir).await {
            warn!("[debug-shell] mkdir {:?} failed: {}", self.state_dir, e);
        }
        let cfg_path = self.state_dir.join("debug-tunnel.json");
        let json = serde_json::to_string_pretty(payload)
            .context("serialize DebugTunnelOpen")?;
        tokio::fs::write(&cfg_path, json.as_bytes())
            .await
            .with_context(|| format!("write {:?}", cfg_path))?;

        // Replace any existing child first.
        self.close().await;

        info!(
            "[debug-shell] spawning {:?} --config {:?}",
            self.binary, cfg_path
        );
        let child = Command::new(&self.binary)
            .arg("--config")
            .arg(&cfg_path)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn {:?}", self.binary))?;
        let mut g = self.child.lock().await;
        *g = Some(child);
        Ok(())
    }

    /// SIGTERM the child if alive; drop the JSON payload.
    pub async fn close(&self) {
        let mut g = self.child.lock().await;
        if let Some(mut child) = g.take() {
            #[cfg(unix)]
            {
                if let Some(pid) = child.id() {
                    unsafe {
                        libc::kill(pid as i32, libc::SIGTERM);
                    }
                }
            }
            #[cfg(not(unix))]
            {
                let _ = child.start_kill();
            }
            match tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await {
                Ok(Ok(s)) => info!("[debug-shell] child exited {:?}", s),
                Ok(Err(e)) => warn!("[debug-shell] wait error: {}", e),
                Err(_) => {
                    warn!("[debug-shell] child did not exit within 5s — SIGKILL");
                    let _ = child.kill().await;
                }
            }
        }
        let cfg_path = self.state_dir.join("debug-tunnel.json");
        let _ = tokio::fs::remove_file(&cfg_path).await;
    }
}

/// Resolve the state directory: `cfg.state_dir` if set, otherwise the parent
/// of `client_config_path`, otherwise `.`.
pub fn resolve_state_dir(state_dir: Option<&Path>, client_config_path: &Path) -> PathBuf {
    if let Some(d) = state_dir {
        return d.to_path_buf();
    }
    client_config_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}
