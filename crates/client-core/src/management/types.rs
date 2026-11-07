use mirage_core::config::CamouflageMode;
use serde::{Deserialize, Serialize};

/// Live status snapshot returned by `GET /admin/status`.
#[derive(Debug, Serialize)]
pub struct ClientAdminStatus {
    /// The `chain_id` of the currently active chain.
    pub active_chain_id: String,
    /// Whether the SOCKS5 listener is active.
    pub mode_socks5: bool,
    /// Whether the HTTP CONNECT proxy is active.
    pub mode_http: bool,
    /// Whether the TUN stack is active.
    pub mode_tun: bool,
    /// Total number of pool slots.
    pub pool_size: usize,
    /// Pool slots in `Idle` state (ready to lease).
    pub pool_idle: usize,
    /// Pool slots currently leased to active requests.
    pub pool_leased: usize,
    /// Pool slots undergoing reconnection.
    pub pool_reconnecting: usize,
    /// Whether cover-traffic is enabled on the active chain.
    pub cover_traffic_enabled: bool,
    /// Monotonically increasing counter; incremented on every successful reconfig.
    pub config_version: u64,
}

/// Request body for `POST /admin/chain/switch`.
#[derive(Debug, Deserialize)]
pub struct SwitchChainRequest {
    /// The `chain_id` to switch to.  Must exist in the current client config.
    pub chain_id: String,
    /// If `true` (or omitted as default `false`), all idle pool slots are
    /// immediately marked for reconnection to the new chain.  Leased slots
    /// finish on the old chain and are replaced when they return.
    pub drain: Option<bool>,
}

/// Request body for `PUT /admin/mode`.
///
/// Each field is optional — omit to leave that mode unchanged.
/// Disabling SOCKS5 when it is the only active mode returns `400 Bad Request`.
#[derive(Debug, Deserialize)]
pub struct SetModeRequest {
    /// Enable or disable the SOCKS5 listener.
    pub socks5: Option<bool>,
    /// Enable or disable the HTTP CONNECT proxy.
    pub http: Option<bool>,
    /// Enable or disable the TUN stack (Linux only).
    pub tun: Option<bool>,
}

/// Request body for `PUT /admin/camouflage`.
#[derive(Debug, Deserialize)]
pub struct SetCamouflageRequest {
    /// The chain whose camouflage mode to update.
    pub chain_id: String,
    /// New camouflage mode.
    pub mode: CamouflageMode,
}

/// Request body for `PUT /admin/shaping`.
#[derive(Debug, Deserialize)]
pub struct SetShapingRequest {
    /// The chain whose shaping preset to update.
    pub chain_id: String,
    /// Shaping preset name.  One of `"none"`, `"web_browsing"`, `"cloud_api"`, `"streaming"`.
    pub preset: String,
}

/// Request body for `PUT /admin/cover-traffic`.
#[derive(Debug, Deserialize)]
pub struct SetCoverTrafficRequest {
    /// Enable or disable cover-traffic on the active chain.
    pub enabled: bool,
}

/// Generic response returned by mutation endpoints.
#[derive(Debug, Serialize)]
pub struct ReconfigResponse {
    /// `"ok"` on success.
    pub status: String,
    /// Config version after the mutation.
    pub config_version: u64,
    /// Human-readable description of what was done.
    pub message: String,
}

/// Per-chain pool stats returned by `/admin/chains` and `/admin/chain/{name}/stats`.
#[derive(Debug, Serialize)]
pub struct ChainPoolInfo {
    /// Chain identifier.
    pub chain_id: String,
    /// Priority value (higher = preferred).
    pub priority: u8,
    /// Dedicated SOCKS5 bind address, if configured.
    pub socks5_bind: Option<String>,
    /// Whether this is the default (highest-priority) chain.
    pub is_default: bool,
    /// Total pool slots.
    pub pool_size: usize,
    /// Idle pool slots.
    pub pool_idle: usize,
    /// Leased pool slots.
    pub pool_leased: usize,
    /// Slots undergoing reconnection.
    pub pool_reconnecting: usize,
}
