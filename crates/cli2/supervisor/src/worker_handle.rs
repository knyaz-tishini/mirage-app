//! WorkerHandle: owns a spawned worker process + its IPC channels.

use anyhow::Context;
use mirage_chain_ipc::{SupervisorMsg, WorkerMsg, WorkerState, read_msg, write_msg};
use std::{
    path::Path,
    process::Stdio,
    time::Instant,
};
use tokio::{
    io::{BufReader, BufWriter},
    process::{Child, ChildStdin},
    sync::mpsc,
};
use tracing::{debug, error, info, warn};

use crate::config::ChainConfig;

/// Max restarts in RESTART_WINDOW_SECS before giving up.
const MAX_RESTARTS_IN_WINDOW: u32 = 5;
/// Window length for restart rate limiting.
const RESTART_WINDOW_SECS: u64 = 60;

/// Live state of the worker as seen by the supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleState {
    Spawned,
    Ready,
    Draining,
    Exited,
}

/// Messages forwarded from the per-handle reader task to the supervisor loop.
#[derive(Debug)]
pub struct WorkerEvent {
    pub msg: WorkerMsg,
}

/// Restart rate tracker.
#[derive(Debug, Default)]
pub struct RestartTracker {
    /// Timestamps of recent restarts (pruned to RESTART_WINDOW_SECS).
    recent: Vec<Instant>,
    /// Total restarts ever.
    pub total: u32,
    /// Set to true once the worker has been rate-limited out of respawning.
    pub exhausted: bool,
}

impl RestartTracker {
    /// Record a restart attempt. Returns `false` if the rate limit is hit.
    pub fn record(&mut self) -> bool {
        if self.exhausted {
            return false;
        }
        let now = Instant::now();
        let window = std::time::Duration::from_secs(RESTART_WINDOW_SECS);
        self.recent.retain(|t| now.duration_since(*t) < window);
        self.recent.push(now);
        self.total += 1;
        if self.recent.len() as u32 > MAX_RESTARTS_IN_WINDOW {
            self.exhausted = true;
            return false;
        }
        true
    }
}

/// Handle to a running worker process.
pub struct WorkerHandle {
    pub chain_id: String,
    pub pid: Option<u32>,
    pub state: HandleState,
    /// Set by rollout code before intentionally killing/draining the worker,
    /// so the respawn watchdog does not trigger a restart.
    pub intentional_restart: bool,
    /// Tracks how often this worker has been respawned.
    pub restart_tracker: RestartTracker,
    child: Child,
    stdin_writer: BufWriter<ChildStdin>,
    /// Receives WorkerMsg events forwarded by the background reader task.
    pub rx: mpsc::Receiver<WorkerEvent>,
}

impl WorkerHandle {
    /// Spawn a new worker process for `chain_config` using `binary_path`.
    ///
    /// The worker communicates over its stdin/stdout. A background task
    /// reads its stdout and forwards [`WorkerEvent`]s through `rx`.
    ///
    /// Workers are spawned in their own process group (Unix: `process_group(0)`,
    /// Windows: `CREATE_NEW_PROCESS_GROUP`). This means a Ctrl-C sent to the
    /// supervisor console does **not** propagate to workers. However, because
    /// IPC uses stdin/stdout pipes, if the supervisor exits its stdin pipe
    /// closes → the worker sees EOF → clean shutdown. This is the v1
    /// limitation: true detachment requires switching to named-pipe/socket IPC.
    pub fn spawn(
        binary_path: &Path,
        chain_config: &ChainConfig,
    ) -> anyhow::Result<Self> {
        Self::spawn_with_tracker(binary_path, chain_config, RestartTracker::default())
    }

    /// Internal: spawn reusing an existing [`RestartTracker`].
    pub fn spawn_with_tracker(
        binary_path: &Path,
        chain_config: &ChainConfig,
        restart_tracker: RestartTracker,
    ) -> anyhow::Result<Self> {
        // Build a std::process::Command first so we can apply platform-specific
        // process-group flags that tokio::process::Command may not expose
        // directly (e.g. Windows CommandExt).
        let mut std_cmd = std::process::Command::new(binary_path);
        std_cmd
            .env("MIRAGE_CHAIN_ID", &chain_config.id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Own process group: won't receive Ctrl-C from supervisor's console.
            std_cmd.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NEW_PROCESS_GROUP (0x200): child won't receive Ctrl-C from
            // parent console. We do NOT use DETACHED_PROCESS (0x8) because that
            // closes stdio handles — we need them for IPC.
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            std_cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }

        // Convert to tokio::process::Command for async process management.
        let mut cmd = tokio::process::Command::from(std_cmd);

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "spawning worker {:?} for chain {}",
                binary_path, chain_config.id
            )
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("worker stdin not available"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("worker stdout not available"))?;

        let stdin_writer = BufWriter::new(stdin);
        let (tx, rx) = mpsc::channel::<WorkerEvent>(32);

        let chain_id_reader = chain_config.id.clone();
        let mut reader = BufReader::new(stdout);
        tokio::spawn(async move {
            loop {
                match read_msg::<_, WorkerMsg>(&mut reader).await {
                    Ok(Some(msg)) => {
                        debug!(chain_id = %chain_id_reader, ?msg, "worker → supervisor");
                        if tx.send(WorkerEvent { msg }).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        info!(chain_id = %chain_id_reader, "worker stdout closed");
                        break;
                    }
                    Err(e) => {
                        error!(chain_id = %chain_id_reader, "IPC read error: {e}");
                        break;
                    }
                }
            }
        });

        info!(
            chain_id = %chain_config.id,
            binary = ?binary_path,
            "worker spawned"
        );

        Ok(Self {
            chain_id: chain_config.id.clone(),
            pid: None,
            state: HandleState::Spawned,
            intentional_restart: false,
            restart_tracker,
            child,
            stdin_writer,
            rx,
        })
    }

    /// Non-blocking poll: returns `Some(exit_status_code)` if child has exited,
    /// `None` if still running.
    pub fn try_wait(&mut self) -> Option<Option<i32>> {
        // tokio::process::Child::try_wait is synchronous (does not require .await).
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.state = HandleState::Exited;
                Some(status.code())
            }
            Ok(None) => None,
            Err(e) => {
                warn!(chain_id = %self.chain_id, "try_wait error: {e}");
                None
            }
        }
    }

    /// Send a message to the worker over its stdin.
    pub async fn send(&mut self, msg: &SupervisorMsg) -> anyhow::Result<()> {
        write_msg(&mut self.stdin_writer, msg)
            .await
            .with_context(|| format!("sending to worker {}", self.chain_id))
    }

    /// Send `Start` with the chain config.
    pub async fn send_start(&mut self, chain: &ChainConfig) -> anyhow::Result<()> {
        self.send(&SupervisorMsg::Start {
            chain_id: chain.id.clone(),
            socks5_bind: chain.socks5_bind.clone(),
            config_path: chain.config_path.clone(),
        })
        .await
    }

    /// Send `Drain` and transition to draining state.
    pub async fn send_drain(&mut self, timeout_secs: u64) -> anyhow::Result<()> {
        self.state = HandleState::Draining;
        self.send(&SupervisorMsg::Drain { timeout_secs }).await
    }

    /// Send `Shutdown` (force exit).
    pub async fn send_shutdown(&mut self) -> anyhow::Result<()> {
        self.send(&SupervisorMsg::Shutdown).await
    }

    /// Wait for the child process to exit.
    pub async fn wait(&mut self) -> anyhow::Result<()> {
        let status = self
            .child
            .wait()
            .await
            .with_context(|| format!("waiting for worker {}", self.chain_id))?;
        self.state = HandleState::Exited;
        if !status.success() {
            warn!(
                chain_id = %self.chain_id,
                code = ?status.code(),
                "worker exited with non-zero status"
            );
        }
        Ok(())
    }

    /// Kill the child process immediately.
    pub async fn kill(&mut self) {
        if let Err(e) = self.child.kill().await {
            warn!(chain_id = %self.chain_id, "kill error: {e}");
        }
        self.state = HandleState::Exited;
    }

    /// Process a received WorkerMsg and update handle state.
    pub fn apply_msg(&mut self, msg: &WorkerMsg) {
        match msg {
            WorkerMsg::Ready { pid, .. } => {
                self.pid = Some(*pid);
                self.state = HandleState::Ready;
            }
            WorkerMsg::Draining { .. } => {
                self.state = HandleState::Draining;
            }
            WorkerMsg::Drained { .. } => {
                self.state = HandleState::Exited;
            }
            WorkerMsg::Error { message, .. } => {
                error!(chain_id = %self.chain_id, "worker error: {message}");
            }
            WorkerMsg::Status { state, .. } => {
                if *state == WorkerState::Ready {
                    self.state = HandleState::Ready;
                }
            }
        }
    }
}
