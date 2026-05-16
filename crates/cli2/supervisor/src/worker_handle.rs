//! WorkerHandle: owns a worker process + its IPC channels (named pipe / Unix socket).

use anyhow::Context;
use mirage_chain_ipc::{
    SupervisorMsg, WorkerMsg, WorkerState,
    pipe::{PipePair, connect_with_retry},
    pipe_path,
};
use std::{path::Path, time::Instant};
use tokio::process::Child;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config::ChainConfig;

/// Max restarts in RESTART_WINDOW_SECS before giving up.
const MAX_RESTARTS_IN_WINDOW: u32 = 5;
/// Window length for restart rate limiting.
const RESTART_WINDOW_SECS: u64 = 60;
/// How many times to retry connecting to the worker pipe after spawn.
const PIPE_CONNECT_ATTEMPTS: u32 = 25;
/// Delay between pipe connect retries (ms).
const PIPE_CONNECT_DELAY_MS: u64 = 200;

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

/// Handle to a worker process. IPC is via named pipe / Unix socket.
pub struct WorkerHandle {
    pub chain_id: String,
    pub pid: Option<u32>,
    pub state: HandleState,
    /// Set by rollout code before intentionally killing/draining the worker.
    pub intentional_restart: bool,
    /// Tracks how often this worker has been respawned.
    pub restart_tracker: RestartTracker,
    /// Child process handle. `None` for reattached orphans (supervisor didn't spawn them).
    child: Option<Child>,
    /// IPC write half (send SupervisorMsg to worker).
    ipc_writer: IpcSender,
    /// Receives WorkerMsg events forwarded by the background reader task.
    pub rx: mpsc::Receiver<WorkerEvent>,
}

/// Thin wrapper so the mpsc sender side is clearly typed.
struct IpcSender {
    tx: mpsc::Sender<SupervisorMsg>,
}

impl IpcSender {
    async fn send(&self, msg: SupervisorMsg) -> anyhow::Result<()> {
        self.tx
            .send(msg)
            .await
            .map_err(|_| anyhow::anyhow!("IPC sender channel closed"))
    }
}

impl WorkerHandle {
    /// Spawn a new worker process for `chain_config` using `binary_path`.
    ///
    /// After spawn, connects to its named pipe / Unix socket with retries.
    /// Expects the worker to send `Hello` on connect; sends `Continue` if the
    /// worker already has config (shouldn't happen on fresh spawn, but safe).
    pub fn spawn(binary_path: &Path, chain_config: &ChainConfig) -> anyhow::Result<Self> {
        Self::spawn_with_tracker(binary_path, chain_config, RestartTracker::default())
    }

    /// Internal: spawn reusing an existing [`RestartTracker`].
    pub fn spawn_with_tracker(
        binary_path: &Path,
        chain_config: &ChainConfig,
        restart_tracker: RestartTracker,
    ) -> anyhow::Result<Self> {
        use std::process::Stdio;

        let mut std_cmd = std::process::Command::new(binary_path);
        std_cmd
            .env("MIRAGE_CHAIN_ID", &chain_config.id)
            // stdin/stdout/stderr: all inherited or null — no IPC pipes.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Own process group + session so the worker is fully detached.
            unsafe {
                std_cmd.pre_exec(|| {
                    // setsid() creates a new session; worker survives supervisor exit.
                    libc::setsid();
                    Ok(())
                });
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // DETACHED_PROCESS (0x8): no console; CREATE_NEW_PROCESS_GROUP (0x200):
            // won't receive Ctrl-C from parent. Combined they fully detach the worker.
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            std_cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        }

        let mut cmd = tokio::process::Command::from(std_cmd);
        let child = cmd.spawn().with_context(|| {
            format!(
                "spawning worker {:?} for chain {}",
                binary_path, chain_config.id
            )
        })?;

        info!(
            chain_id = %chain_config.id,
            binary = ?binary_path,
            "worker spawned — will connect to pipe"
        );

        let chain_id = chain_config.id.clone();
        let pipe_addr = pipe_path(&chain_id);

        // Build mpsc channels. A background task owns the PipePair and
        // proxies messages in both directions.
        let (sup_tx, sup_rx) = mpsc::channel::<SupervisorMsg>(32);
        let (worker_tx, worker_rx) = mpsc::channel::<WorkerEvent>(32);

        let chain_id_bg = chain_id.clone();
        tokio::spawn(async move {
            // Connect to the worker's pipe with retry.
            let mut ipc = match connect_with_retry(
                &pipe_addr,
                PIPE_CONNECT_ATTEMPTS,
                PIPE_CONNECT_DELAY_MS,
            )
            .await
            {
                Ok(p) => p,
                Err(e) => {
                    error!(chain_id = %chain_id_bg, "could not connect to worker pipe: {e}");
                    return;
                }
            };

            // Worker sends Hello immediately; read + log it, then send Continue.
            match ipc.read_msg::<WorkerMsg>().await {
                Ok(Some(WorkerMsg::Hello { ref chain_id, pid, .. })) => {
                    info!(chain_id = %chain_id, pid, "worker Hello (fresh spawn)");
                }
                Ok(Some(other)) => {
                    debug!(chain_id = %chain_id_bg, ?other, "first message from new worker (not Hello)");
                }
                Ok(None) => {
                    warn!(chain_id = %chain_id_bg, "worker pipe EOF before Hello");
                    return;
                }
                Err(e) => {
                    error!(chain_id = %chain_id_bg, "IPC read error waiting for Hello: {e}");
                    return;
                }
            }

            // Worker waits for Continue or a Start message.
            // For fresh spawns we use Continue here; the caller then sends Start separately.
            // (Caller uses send_start() which goes through sup_rx.)
            // We do NOT send Continue here — we let the normal send path deliver Start.
            // Just proceed to the bidirectional relay loop.

            ipc_relay_loop(ipc, sup_rx, worker_tx, chain_id_bg).await;
        });

        Ok(Self {
            chain_id,
            pid: None,
            state: HandleState::Spawned,
            intentional_restart: false,
            restart_tracker,
            child: Some(child),
            ipc_writer: IpcSender { tx: sup_tx },
            rx: worker_rx,
        })
    }

    /// Construct a `WorkerHandle` from a reattached orphan (no child process owned here).
    ///
    /// `ipc` is already connected and `Hello` has already been read by `discover_orphans`.
    /// After calling this, the caller must send `Continue` via `send_continue()`.
    pub fn reattach(chain_id: String, pid: u32, ipc: PipePair) -> Self {
        let (sup_tx, sup_rx) = mpsc::channel::<SupervisorMsg>(32);
        let (worker_tx, worker_rx) = mpsc::channel::<WorkerEvent>(32);

        let chain_id_bg = chain_id.clone();
        tokio::spawn(async move {
            ipc_relay_loop(ipc, sup_rx, worker_tx, chain_id_bg).await;
        });

        Self {
            chain_id,
            pid: Some(pid),
            state: HandleState::Ready, // orphan was running; assume Ready until proven otherwise
            intentional_restart: false,
            restart_tracker: RestartTracker::default(),
            child: None,
            ipc_writer: IpcSender { tx: sup_tx },
            rx: worker_rx,
        }
    }

    /// Non-blocking poll: returns `Some(exit_status_code)` if child has exited,
    /// `None` if still running or if this is a reattached orphan (no owned child).
    pub fn try_wait(&mut self) -> Option<Option<i32>> {
        let child = self.child.as_mut()?;
        match child.try_wait() {
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

    /// Send a message to the worker.
    pub async fn send(&mut self, msg: SupervisorMsg) -> anyhow::Result<()> {
        self.ipc_writer
            .send(msg)
            .await
            .with_context(|| format!("sending to worker {}", self.chain_id))
    }

    /// Send `Start` with the chain config and an optional initial peer table.
    pub async fn send_start(
        &mut self,
        chain: &ChainConfig,
        initial_peer_table: Option<String>,
    ) -> anyhow::Result<()> {
        self.send(SupervisorMsg::Start {
            chain_id: chain.id.clone(),
            socks5_bind: chain.socks5_bind.clone(),
            config_path: chain.config_path.clone(),
            peer_table_json: initial_peer_table,
        })
        .await
    }

    /// Push a live peer table update to the worker (Phase 8.5).
    pub async fn send_peer_table_update(
        &mut self,
        peer_table_json: String,
    ) -> anyhow::Result<()> {
        self.send(SupervisorMsg::PeerTableUpdate { peer_table_json })
            .await
    }

    /// Tell the designated MonitorLink host worker to spawn its MonitorLink.
    pub async fn send_setup_monitor_link(
        &mut self,
        monitor_urls: Vec<String>,
        monitor_api_key: Option<String>,
        transports_json: String,
    ) -> anyhow::Result<()> {
        self.send(SupervisorMsg::SetupMonitorLink {
            monitor_urls,
            monitor_api_key,
            transports_json,
        })
        .await
    }

    /// Send `Continue` to a reattached orphan worker.
    pub async fn send_continue(&mut self) -> anyhow::Result<()> {
        self.send(SupervisorMsg::Continue).await
    }

    /// Send `Drain` and transition to draining state.
    pub async fn send_drain(&mut self, timeout_secs: u64) -> anyhow::Result<()> {
        self.state = HandleState::Draining;
        self.send(SupervisorMsg::Drain { timeout_secs }).await
    }

    /// Send `Shutdown` (force exit).
    pub async fn send_shutdown(&mut self) -> anyhow::Result<()> {
        self.send(SupervisorMsg::Shutdown).await
    }

    /// Wait for the child process to exit. No-op for reattached orphans.
    pub async fn wait(&mut self) -> anyhow::Result<()> {
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        let status = child
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

    /// Kill the child process immediately. No-op for reattached orphans.
    pub async fn kill(&mut self) {
        if let Some(child) = self.child.as_mut() {
            if let Err(e) = child.kill().await {
                warn!(chain_id = %self.chain_id, "kill error: {e}");
            }
        }
        self.state = HandleState::Exited;
    }

    /// Process a received WorkerMsg and update handle state.
    ///
    /// `PeerTableSnapshot` is intentionally NOT handled here — the main loop
    /// routes it to `pt_tx` before calling `apply_msg`.
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
            WorkerMsg::Hello { pid, .. } => {
                // Hello on an established connection means supervisor reconnected
                // to an existing worker. Update PID in case it's useful.
                self.pid = Some(*pid);
            }
            WorkerMsg::PeerTableSnapshot { generation, .. } => {
                debug!(chain_id = %self.chain_id, generation, "worker sent PeerTableSnapshot");
            }
        }
    }
}

/// Background task: relay messages between the supervisor mpsc and the pipe.
async fn ipc_relay_loop(
    mut ipc: PipePair,
    mut sup_rx: mpsc::Receiver<SupervisorMsg>,
    worker_tx: mpsc::Sender<WorkerEvent>,
    chain_id: String,
) {
    loop {
        tokio::select! {
            // Supervisor wants to send a message to the worker.
            msg = sup_rx.recv() => {
                let Some(msg) = msg else {
                    // Supervisor dropped the sender — clean exit.
                    break;
                };
                if let Err(e) = ipc.write_msg(&msg).await {
                    error!(chain_id = %chain_id, "IPC write error: {e}");
                    break;
                }
            }

            // Worker sent a message to the supervisor.
            result = ipc.read_msg::<WorkerMsg>() => {
                match result {
                    Ok(Some(msg)) => {
                        debug!(chain_id = %chain_id, ?msg, "worker → supervisor");
                        if worker_tx.send(WorkerEvent { msg }).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        info!(chain_id = %chain_id, "worker pipe closed (EOF)");
                        break;
                    }
                    Err(e) => {
                        error!(chain_id = %chain_id, "IPC read error: {e}");
                        break;
                    }
                }
            }
        }
    }
}
