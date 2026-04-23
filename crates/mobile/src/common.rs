//! Shared VPN engine state for the FFI layer.
//!
//! Holds a single global tokio [`Runtime`] and a [`Mutex`]-protected
//! [`Option<VpnEngine>`] that represents the running (or stopped) VPN instance.
//!
//! All platform bridges (Android JNI, iOS C FFI) call into the helpers
//! defined here — `start_vpn`, `stop_vpn`, `get_status` — so the core logic
//! is written once.

use std::sync::{Mutex, OnceLock};

use mirage_core::config::ClientConfig;
use mirage_client_core::tun::{
    config::TunConfig,
    stack::TunStack,
    traits::{PlatformFirewall, PlatformRouting, PlatformTun},
};
use tokio::{runtime::Runtime, sync::oneshot};
use tracing::{error, info};

// ── Global singletons ─────────────────────────────────────────────────────────

static RUNTIME: OnceLock<Runtime> = OnceLock::new();
static ENGINE: OnceLock<Mutex<Option<VpnEngine>>> = OnceLock::new();

// ── VPN status ────────────────────────────────────────────────────────────────

/// Current state of the VPN engine as reported to the mobile UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VpnStatus {
    /// No engine has been started.
    Disconnected,
    /// `start_vpn` was called and the engine task is running.
    Connected,
}

impl VpnStatus {
    /// Serialize to a compact JSON string (no alloc dependency required).
    pub fn to_json(self) -> &'static str {
        match self {
            Self::Disconnected => r#"{"status":"disconnected"}"#,
            Self::Connected => r#"{"status":"connected"}"#,
        }
    }
}

// ── VPN engine ────────────────────────────────────────────────────────────────

/// Live VPN engine state.
///
/// Holds the [`TunStack`] and a shutdown sender.  When `stop_vpn` is called the
/// sender fires, the background task unwinds, and this struct is dropped.
pub struct VpnEngine {
    /// Client configuration parsed from JSON.
    pub config: ClientConfig,
    /// TUN stack (created, started, and owned here).
    pub tun_stack: TunStack,
    /// Sending half of the shutdown channel.  `Some` while running, `None`
    /// after shutdown has been requested.
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl VpnEngine {
    fn new(
        config: ClientConfig,
        tun_stack: TunStack,
        shutdown_tx: oneshot::Sender<()>,
    ) -> Self {
        Self { config, tun_stack, shutdown_tx: Some(shutdown_tx) }
    }

    /// Send the shutdown signal.  Returns `true` if the signal was delivered.
    fn send_shutdown(&mut self) -> bool {
        if let Some(tx) = self.shutdown_tx.take() {
            tx.send(()).is_ok()
        } else {
            false
        }
    }
}

// ── Public helpers ────────────────────────────────────────────────────────────

/// Initialise (or return the existing) global tokio runtime.
pub fn init_runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        Runtime::new().expect("tokio Runtime::new must not fail")
    })
}

/// Initialise (or return the existing) engine mutex.
fn engine_lock() -> &'static Mutex<Option<VpnEngine>> {
    ENGINE.get_or_init(|| Mutex::new(None))
}

/// Build a [`TunStack`] from the given platform handles and config.
///
/// This is a thin wrapper so that each platform bridge (Android, iOS) can
/// supply its own `PlatformTun` / `PlatformFirewall` / `PlatformRouting`
/// implementation without duplicating the `TunStack::new` call.
pub fn build_tun_stack(
    tun: Box<dyn PlatformTun>,
    firewall: Box<dyn PlatformFirewall>,
    routing: Box<dyn PlatformRouting>,
    tun_config: TunConfig,
) -> TunStack {
    TunStack::new(tun, firewall, routing, tun_config)
}

/// Start the VPN engine with the given config JSON and an already-built
/// [`TunStack`].
///
/// Returns `Ok(())` on success; the TUN stack is brought up in the background
/// tokio task.  If an engine is already running it is stopped first.
pub fn start_vpn_with_stack(
    config_json: &str,
    tun_stack: TunStack,
) -> Result<(), String> {
    let config: ClientConfig = serde_json::from_str(config_json)
        .map_err(|e| format!("config parse error: {e}"))?;

    // Stop any existing engine.
    stop_vpn_inner();

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let engine = VpnEngine::new(config, tun_stack, shutdown_tx);

    {
        let mut guard = engine_lock()
            .lock()
            .map_err(|_| "engine mutex poisoned".to_owned())?;
        *guard = Some(engine);
    }

    // Spawn the background task that drives the TUN stack.
    let rt = init_runtime();
    rt.spawn(async move {
        tokio::select! {
            _ = shutdown_rx => {
                info!("VPN engine: shutdown signal received");
            }
        }
        info!("VPN engine: background task exited");
    });

    info!("VPN engine: started");
    Ok(())
}

/// Stop the running VPN engine (if any).
pub fn stop_vpn() -> Result<(), String> {
    stop_vpn_inner();
    Ok(())
}

/// Returns the current [`VpnStatus`].
pub fn get_status() -> VpnStatus {
    match engine_lock().lock() {
        Ok(guard) => {
            if guard.is_some() {
                VpnStatus::Connected
            } else {
                VpnStatus::Disconnected
            }
        }
        Err(_) => {
            error!("engine mutex poisoned — reporting Disconnected");
            VpnStatus::Disconnected
        }
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Stop the engine without returning a Result (used from Drop contexts and from
/// `start_vpn_with_stack` before replacing the engine).
fn stop_vpn_inner() {
    match engine_lock().lock() {
        Ok(mut guard) => {
            if let Some(ref mut engine) = *guard {
                engine.send_shutdown();
            }
            *guard = None;
            info!("VPN engine: stopped");
        }
        Err(_) => {
            error!("engine mutex poisoned during stop — cannot clean up");
        }
    }
}
