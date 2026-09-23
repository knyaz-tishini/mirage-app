//! WorkerHandle: owns a worker process + its IPC channels (named pipe / Unix socket).

use anyhow::Context;
use mirage_chain_ipc::{
    SupervisorMsg, WorkerMsg, WorkerState,
    pipe::{PipePair, connect_with_retry},
    pipe_path,
};
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    io::Read as _,
    path::Path,
    time::Instant,
};
use tokio::process::Child;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config::ChainConfig;

/// Max bytes read from the chain JSON file when computing the fingerprint.
/// Ring-credential fields (ring_epoch_id, ring_member_sk_hex, ring_path) are
/// top-level so any credential rotation touches the first few hundred bytes.
/// 64 KiB is large enough to capture the full typical config and keep I/O cheap.
const CONTENT_SAMPLE_BYTES: usize = 65_536;

/// Sentinel mixed into the fingerprint when the config file cannot be read.
/// Using a non-zero value means that once the file becomes readable the hash
/// changes, triggering a respawn to pick up the real content.
const FILE_UNREADABLE_SENTINEL: u64 = 0xDEAD_C0DE_DEAD_C0DE;

/// Max restarts in RESTART_WINDOW_SECS before giving up.
const MAX_RESTARTS_IN_WINDOW: u32 = 5;
/// Window length for restart rate limiting.
const RESTART_WINDOW_SECS: u64 = 60;
/// How many times to retry connecting to the worker pipe after spawn.
/// 100 (= 20s at 200ms) not 25 (= 5s): a cold-cache worker binary on a small
/// box (8GB Mac tester) can take ~5-6s just to start and bind its pipe, so a
/// 5s window lost the race on the first run after every rebuild — the
/// supervisor gave up, SetupMonitorLink failed, and the MonitorLink never came
/// up (no credential_params, no relay). 20s covers a cold start with margin.
const PIPE_CONNECT_ATTEMPTS: u32 = 100;
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

/// Outcome of [`WorkerHandle::stop`]. See that method's doc for the box-mode
/// contract — `StuckInBoxMode` is never resolved by retrying `stop()` again;
/// the caller must exit the supervisor process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// The worker reached `Exited` — either it stopped itself over the
    /// graceful IPC path, or (workstation mode only) `kill()` was applied.
    Stopped,
    /// Box mode only: the worker did not reach `Exited` within the caller's
    /// wait, and `kill()` was never attempted (eBPF denies it once the
    /// warden's cgroup is armed). The whole supervisor must exit non-zero.
    StuckInBoxMode,
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

/// Compute a fingerprint of `chain` that covers both the supervisor-level
/// `ChainConfig` struct (id, socks5_bind, config_path, monitor_link_host) AND
/// the content of the JSON file at `chain.config_path`.
///
/// The ring credential fields (ring_epoch_id, ring_member_sk_hex, ring_path)
/// live inside that JSON file, not in `ChainConfig`, so a plain struct hash
/// would miss a credential-only rotation and leave the worker running with a
/// stale epoch baked into its in-memory `ClientConfig`.
///
/// Reading up to `CONTENT_SAMPLE_BYTES` is sufficient because ring fields are
/// top-level and any byte change in the file produces a different hash.
///
/// If the file cannot be read, `FILE_UNREADABLE_SENTINEL` is mixed into the
/// hash so that the fingerprint changes once the file becomes readable again,
/// causing a respawn that picks up the real content.
///
/// `DefaultHasher` is not stable across Rust releases but is fine here: we
/// only compare fingerprints within a single process lifetime.
pub fn chain_fingerprint(chain: &crate::config::ChainConfig) -> u64 {
    let json = serde_json::to_string(chain).unwrap_or_default();
    let mut h = DefaultHasher::new();
    json.hash(&mut h);

    // Mix in the content of the config file so credential rotations are detected.
    match std::fs::File::open(&chain.config_path) {
        Ok(mut f) => {
            let mut buf = vec![0u8; CONTENT_SAMPLE_BYTES];
            match f.read(&mut buf) {
                Ok(n) => buf[..n].hash(&mut h),
                Err(_) => FILE_UNREADABLE_SENTINEL.hash(&mut h),
            }
        }
        Err(_) => FILE_UNREADABLE_SENTINEL.hash(&mut h),
    }

    h.finish()
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
    /// Fingerprint of the `ChainConfig` this worker was started with.
    /// Computed at spawn time via [`chain_fingerprint`].  Used by the reconcile
    /// loop to detect same-id config changes that require a drain+respawn.
    pub config_fingerprint: u64,
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
    /// The effective config is fingerprinted at this point so the reconcile
    /// loop can later detect same-id config changes.
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

        let config_fingerprint = chain_fingerprint(chain_config);

        Ok(Self {
            chain_id,
            pid: None,
            state: HandleState::Spawned,
            intentional_restart: false,
            restart_tracker,
            config_fingerprint,
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
            // Orphan config is unknown to the supervisor — use 0 so the first
            // reconcile always treats it as "changed" if the config differs.
            config_fingerprint: 0,
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
    ///
    /// `box_mode` decides how the worker binds its own SOCKS5 listener:
    /// a unix socket at `config::worker_socks5_unix_path` in box mode (never
    /// a TCP port — owner ruling 2026-09-23), `chain.socks5_bind` in
    /// workstation mode. `Config::validate` already guarantees `chain` has
    /// the field appropriate to the mode by the time any handle reaches this
    /// call — the `ok_or_else` below is defense in depth, not the primary
    /// gate.
    pub async fn send_start(
        &mut self,
        chain: &ChainConfig,
        initial_peer_table: Option<String>,
        box_mode: bool,
    ) -> anyhow::Result<()> {
        let socks5_listen = if box_mode {
            mirage_chain_ipc::Socks5ListenSpec::Unix {
                path: crate::config::worker_socks5_unix_path(&chain.id)
                    .to_string_lossy()
                    .into_owned(),
            }
        } else {
            let addr = chain.socks5_bind.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "chain '{}' has no socks5_bind in workstation mode — Config::validate \
                     should have refused this earlier",
                    chain.id
                )
            })?;
            mirage_chain_ipc::Socks5ListenSpec::Tcp { addr }
        };
        self.send(SupervisorMsg::Start {
            chain_id: chain.id.clone(),
            socks5_listen,
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

    /// Push a live `CredentialParams` update to the worker.
    ///
    /// Mirrors [`send_peer_table_update`] — relays the MonitorLink host
    /// worker's operator-signed authority set to a sibling worker that has no
    /// `MonitorLink` of its own (see `WorkerMsg::CredentialParamsSnapshot`).
    pub async fn send_credential_params_update(
        &mut self,
        credential_params_json: String,
    ) -> anyhow::Result<()> {
        self.send(SupervisorMsg::CredentialParamsUpdate { credential_params_json })
            .await
    }

    /// Tell the designated MonitorLink host worker to spawn its MonitorLink.
    ///
    /// `tun_api_url` is forwarded to the worker so it can push split-tunnel
    /// policies to the local TUN daemon when the monitor pushes a new policy.
    pub async fn send_setup_monitor_link(
        &mut self,
        monitor_api_key: Option<String>,
        transports_json: String,
        tun_api_url: Option<String>,
    ) -> anyhow::Result<()> {
        self.send(SupervisorMsg::SetupMonitorLink {
            monitor_api_key,
            transports_json,
            tun_api_url,
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

    /// Stop this worker after the caller has already asked it to drain/shut
    /// down over IPC (`send_drain` / `send_shutdown`) and drained
    /// `WorkerMsg` events into `apply_msg` — i.e. after the same bounded wait
    /// every existing stop path already performs.
    ///
    /// Workstation mode (`box_mode = false`): unchanged behaviour.
    /// Unconditionally calls [`Self::kill`] — a no-op that only sets
    /// `Exited` when the worker already stopped on its own (reattached
    /// orphans have no owned `Child` either; `kill()` is harmless there too).
    ///
    /// Box mode (`box_mode = true`): NEVER calls `kill()`. The supervisor's
    /// own process runs inside the warden's kill-protected cgroup once
    /// armed, and every worker is spawned into the SAME cgroup
    /// (`spawn_with_tracker`'s `setsid()`, not a child of a different unit) —
    /// `child.kill()` would be denied EPERM for a worker exactly as it would
    /// be for the supervisor itself. If the worker already reached `Exited`
    /// (the graceful IPC stop worked), this is a no-op returning `Stopped`.
    /// If it did not, this returns `StopOutcome::StuckInBoxMode` and leaves
    /// the process untouched — the caller must log the worker by name and
    /// exit the whole supervisor non-zero so the warden restarts the unit
    /// (see the three call sites in `reconcile.rs` / `rollout.rs`).
    pub async fn stop(&mut self, box_mode: bool) -> StopOutcome {
        // One last chance to observe an OS-level exit that raced the
        // caller's wait loop. No-op for reattached orphans (no owned
        // `Child`) — cheap otherwise.
        let _ = self.try_wait();
        if self.state == HandleState::Exited {
            return StopOutcome::Stopped;
        }
        if box_mode {
            return StopOutcome::StuckInBoxMode;
        }
        self.kill().await;
        StopOutcome::Stopped
    }

    /// Process a received WorkerMsg and update handle state.
    ///
    /// `PeerTableSnapshot` / `CredentialParamsSnapshot` are intentionally NOT
    /// routed here — the main loop forwards them to `pt_tx` / `cred_tx`
    /// before calling `apply_msg`.
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
            WorkerMsg::CredentialParamsSnapshot { generation, .. } => {
                debug!(chain_id = %self.chain_id, generation, "worker sent CredentialParamsSnapshot");
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

#[cfg(test)]
mod tests {
    use super::*;
    use mirage_chain_ipc::pipe::{PipeListener, connect};

    /// Build a connected `(supervisor-side, worker-side)` `PipePair` pair
    /// over a fresh pipe/socket name, so parallel `cargo test` runs never
    /// collide. Mirrors the real supervisor↔worker wiring (worker binds,
    /// supervisor connects — `pipe.rs`'s own doc) without spawning a real
    /// worker process: the test drives the worker side directly as the
    /// "mock worker" the task asks for.
    async fn connected_test_pipes(tag: &str) -> (PipePair, PipePair) {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let chain_id = format!("test-{tag}-{}-{n}", std::process::id());
        let path = pipe_path(&chain_id);
        let mut listener = PipeListener::bind(&path).expect("bind test pipe");
        let (client, server) = tokio::join!(connect(&path), listener.accept());
        (
            client.expect("supervisor-side connect"),
            server.expect("worker-side accept"),
        )
    }

    /// Graceful stop path: the mock worker answers `Drain` with
    /// `Draining`→`Drained`, exactly like the real worker's `handle_session`
    /// (`worker/src/main.rs`). Box mode must resolve this via the IPC path
    /// alone — `stop(true)` must never need `kill()`.
    #[tokio::test]
    async fn stop_box_mode_graceful_worker_exits_without_kill() {
        let (client_ipc, mut server_ipc) = connected_test_pipes("graceful").await;

        let mock_worker = tokio::spawn(async move {
            match server_ipc.read_msg::<SupervisorMsg>().await {
                Ok(Some(SupervisorMsg::Drain { .. })) => {}
                other => panic!("mock worker expected Drain, got {other:?}"),
            }
            server_ipc
                .write_msg(&WorkerMsg::Draining {
                    chain_id: "c1".to_string(),
                    active_connections: 0,
                })
                .await
                .expect("mock worker write Draining");
            server_ipc
                .write_msg(&WorkerMsg::Drained { chain_id: "c1".to_string() })
                .await
                .expect("mock worker write Drained");
        });

        let mut handle = WorkerHandle::reattach("c1".to_string(), 4242, client_ipc);
        handle.send_drain(5).await.expect("send_drain");

        // Drain the WorkerMsg stream the same way `wait_for_drained_reconcile`
        // does — apply every event, stop once Exited.
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
        loop {
            let event = tokio::time::timeout_at(deadline, handle.rx.recv())
                .await
                .expect("timed out waiting for Drained")
                .expect("worker channel closed before Drained");
            handle.apply_msg(&event.msg);
            if handle.state == HandleState::Exited {
                break;
            }
        }

        assert_eq!(handle.stop(true).await, StopOutcome::Stopped);
        let _ = mock_worker.await;
    }

    /// Exit-when-stuck path: the mock worker accepts the connection but never
    /// answers `Drain` — the case `ManagedKillable` box mode exists for.
    /// `stop(true)` must report `StuckInBoxMode` and must NOT touch the
    /// process (there is nothing here to observe that directly, but the
    /// absence of a panic/hang proves `kill()` was never invoked on a stream
    /// this handle has no OS process behind).
    #[tokio::test]
    async fn stop_box_mode_returns_stuck_when_worker_never_responds() {
        let (client_ipc, mut server_ipc) = connected_test_pipes("stuck").await;

        let mock_worker = tokio::spawn(async move {
            let _ = server_ipc.read_msg::<SupervisorMsg>().await;
            // Hold the connection open without ever responding.
            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
        });

        let mut handle = WorkerHandle::reattach("c2".to_string(), 4343, client_ipc);
        handle.send_drain(5).await.expect("send_drain");

        // Bounded wait that elapses with nothing received — mirrors what a
        // caller's `wait_for_drained`-style loop does under its own timeout.
        let _ = tokio::time::timeout(tokio::time::Duration::from_millis(100), handle.rx.recv()).await;

        assert_eq!(
            handle.state,
            HandleState::Draining,
            "send_drain sets Draining immediately; nothing since has updated it"
        );
        assert_eq!(handle.stop(true).await, StopOutcome::StuckInBoxMode);

        mock_worker.abort();
    }

    /// Workstation mode keeps today's behaviour: `stop(false)` still
    /// resolves even though the mock worker never acknowledged — `kill()`
    /// unconditionally marks the handle `Exited` (a reattached handle has no
    /// owned `Child`, so this is the same no-op `kill()` already documents,
    /// not a new code path).
    #[tokio::test]
    async fn stop_workstation_mode_kills_even_without_worker_response() {
        let (client_ipc, mut server_ipc) = connected_test_pipes("workstation").await;

        let mock_worker = tokio::spawn(async move {
            let _ = server_ipc.read_msg::<SupervisorMsg>().await;
            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
        });

        let mut handle = WorkerHandle::reattach("c3".to_string(), 4444, client_ipc);
        handle.send_drain(5).await.expect("send_drain");

        assert_eq!(handle.stop(false).await, StopOutcome::Stopped);
        assert_eq!(handle.state, HandleState::Exited);

        mock_worker.abort();
    }
}
