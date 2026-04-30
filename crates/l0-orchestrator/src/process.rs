//! Process-mode spawner: each component is a child OS process.
//!
//! Used on desktop where fork/exec is cheap and crash isolation is desirable.
//! The supervisor calls [`spawn_process`] directly with the binary path from
//! [`crate::config::ComponentConfig::binary`].

use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::{Child, Command};
use tracing::{info, warn};

use crate::component::{Component, ComponentHandle, ExitStatus};

pub async fn spawn_process(
    component: &Component,
    binary: &std::path::Path,
) -> anyhow::Result<Box<dyn ComponentHandle>> {
    info!(
        name = %component.name,
        binary = %binary.display(),
        args = ?component.args,
        "orchestrator: spawning process"
    );

    let mut cmd = Command::new(binary);
    cmd.args(&component.args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for (k, v) in &component.env {
        cmd.env(k, v);
    }

    let child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn {:?}: {}", binary, e))?;

    Ok(Box::new(ProcessHandle {
        name: component.name.clone(),
        child: Some(child),
    }))
}

struct ProcessHandle {
    name: String,
    child: Option<Child>,
}

#[async_trait]
impl ComponentHandle for ProcessHandle {
    async fn wait(&mut self) -> anyhow::Result<ExitStatus> {
        let Some(child) = self.child.as_mut() else {
            return Ok(ExitStatus(-1));
        };
        let status = child.wait().await?;
        Ok(ExitStatus(status.code().unwrap_or(-1)))
    }

    async fn kill(&mut self) -> anyhow::Result<()> {
        if let Some(child) = self.child.as_mut() {
            if let Err(e) = child.kill().await {
                warn!(name = %self.name, error = %e, "orchestrator: kill failed");
            }
        }
        Ok(())
    }

    fn name(&self) -> &str { &self.name }
}
