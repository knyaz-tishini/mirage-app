//! IPC protocol for supervisor ↔ worker communication.
//!
//! JSON-line framing over duplex byte stream.
//! Transport-agnostic: works over named pipe (Windows) and Unix socket (Linux/macOS).

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

pub mod pipe;
pub mod transport;

/// Wire-format mirror of `MonitorTransportConfig` from `supervisor/src/config.rs`.
///
/// Embedded here so the worker crate can deserialize transport specs without
/// depending on the supervisor crate. Both types must stay structurally in sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum MonitorTransportSpec {
    /// Standard WebSocket subscribe.
    DirectWs {
        url: String,
        #[serde(default)]
        sni: Option<String>,
    },
    /// Binary frame protocol via the worker's SOCKS5 (option b — chain as
    /// transport, no decrypted tunnel).
    OverChain {
        /// Local SOCKS5 address of the chain worker. Supervisor injects this
        /// from the matching `[[chains]]` entry's `socks5_bind`.
        worker_socks5: SocketAddr,
        /// CONNECT target (passed to worker SOCKS5). Loopback of the
        /// monitor's over-chain listener.
        #[serde(default = "default_monitor_addr")]
        monitor_addr: SocketAddr,
        /// Chain identifier — informational, used in logs.
        #[serde(default)]
        chain_id: String,
    },
    /// Direct Mirage-wire WebSocket — dials the monitor's wire endpoint
    /// (`:19444`) using an ephemeral X25519 client key, performs the V9
    /// Mirage AEAD handshake (anonymous, no `DeclaredIdentity`), then
    /// upgrades to WebSocket over the resulting tunnel.
    ///
    /// Preferred over `DirectWs` — attempted first by `MonitorLink`.
    Wire {
        /// IP:port of the monitor's wire listener, e.g. `"103.231.75.144:19444"`.
        monitor_addr: String,
        /// 64-character hex of the monitor's static X25519 public key.
        monitor_pubkey_hex: String,
        /// Cluster/mesh identifier baked into the V9 handshake confirm.
        mesh_id: String,
    },
}

fn default_monitor_addr() -> SocketAddr {
    "127.0.0.1:19445".parse().expect("static addr is valid")
}

pub use transport::{IpcReader, IpcWriter};

/// Messages sent from supervisor to worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SupervisorMsg {
    /// Start serving — worker should build pool, bind SOCKS5, signal Ready.
    Start {
        chain_id: String,
        socks5_bind: String,
        config_path: String,
        /// Initial peer table JSON snapshot from monitor (Phase 8.5).
        /// `None` on cold start before any monitor config is received;
        /// worker falls back to `PeerTable::empty()` and waits for
        /// `PeerTableUpdate` to arrive before attempting dynamic-key dials.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        peer_table_json: Option<String>,
    },
    /// Stop accepting new connections, drain existing.
    Drain { timeout_secs: u64 },
    /// Force exit immediately.
    Shutdown,
    /// Status ping.
    Ping,
    /// Supervisor acknowledges reattach — continue current state.
    Continue,
    /// Supervisor requests worker to drain and exit (will be respawned fresh).
    Restart { reason: String },
    /// Live peer table update pushed by supervisor from MonitorLink (Phase 8.5).
    ///
    /// Worker deserializes and stores into its `Arc<ArcSwap<PeerTable>>`.
    /// Sent on every config snapshot that contains a changed `peer_table`
    /// generation, and also when supervisor first receives the initial snapshot.
    PeerTableUpdate {
        /// JSON-serialized `mirage_core::mirage::mesh::PeerTable`.
        peer_table_json: String,
    },
    /// Sent by supervisor to the designated `monitor_link_host = true` worker
    /// after all workers are Ready.
    ///
    /// The worker constructs a `MonitorLink` using its own `ChainSet` (single
    /// chain it manages) and starts it. Peer table snapshots flow back via
    /// `WorkerMsg::PeerTableSnapshot`. The worker's own `TunnelPool` / `MuxPool`
    /// share the same `Arc<ArcSwap<PeerTable>>` as the `MonitorLink` so the
    /// chain self-resolves from its own snapshots.
    SetupMonitorLink {
        /// Base URL list for the monitor (DirectWs transport).
        monitor_urls: Vec<String>,
        /// Optional Bearer token for monitor auth.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        monitor_api_key: Option<String>,
        /// JSON-serialized `Vec<MonitorTransportConfig>` from supervisor.toml.
        /// The worker deserializes and converts to `MonitorTransport` variants.
        transports_json: String,
    },
}

/// Messages sent from worker to supervisor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerMsg {
    /// Worker has bound its SOCKS5 listener and pool is ready.
    Ready { chain_id: String, pid: u32 },
    /// Worker is draining (after Drain message).
    Draining {
        chain_id: String,
        active_connections: u32,
    },
    /// Worker has fully drained and is about to exit.
    Drained { chain_id: String },
    /// Status snapshot (reply to Ping).
    Status {
        chain_id: String,
        active_connections: u32,
        total_connections: u64,
        state: WorkerState,
    },
    /// Worker encountered fatal error.
    Error { chain_id: String, message: String },
    /// First message after every supervisor connection (initial + reattach).
    Hello {
        chain_id: String,
        pid: u32,
        started_at_unix: u64,
        version: String,
        state: WorkerState,
    },
    /// Peer table snapshot from the designated MonitorLink host worker.
    ///
    /// Sent whenever the worker's `MonitorLink` receives a config push with a
    /// new `peer_table` generation. Supervisor broadcasts `PeerTableUpdate` to
    /// all other workers on receipt.
    PeerTableSnapshot {
        /// Monotonically increasing generation counter from the monitor.
        generation: u64,
        /// JSON-serialized `mirage_core::mirage::mesh::PeerTable`.
        peer_table_json: String,
    },
}

/// Worker lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    Starting,
    Ready,
    Draining,
    Exiting,
}

/// Read one JSON-line message from `reader`. Returns `None` on EOF.
pub async fn read_msg<R, T>(reader: &mut R) -> std::io::Result<Option<T>>
where
    R: tokio::io::AsyncBufReadExt + Unpin,
    T: serde::de::DeserializeOwned,
{
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Ok(None);
    }
    let trimmed = line.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        return Ok(None);
    }
    let msg = serde_json::from_str(trimmed).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("json parse error: {e}"),
        )
    })?;
    Ok(Some(msg))
}

/// Write one JSON-line message to `writer`.
pub async fn write_msg<W, T>(writer: &mut W, msg: &T) -> std::io::Result<()>
where
    W: tokio::io::AsyncWriteExt + Unpin,
    T: serde::Serialize,
{
    let json = serde_json::to_string(msg).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("json serialize error: {e}"),
        )
    })?;
    writer.write_all(json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

/// Returns `\\.\pipe\mirage-cli2-{chain_id}` on Windows,
/// `${TEMP}/mirage-cli2/{chain_id}.sock` on Unix.
pub fn pipe_path(chain_id: &str) -> String {
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\mirage-cli2-{chain_id}")
    }
    #[cfg(not(windows))]
    {
        let tmp = std::env::temp_dir();
        tmp.join("mirage-cli2")
            .join(format!("{chain_id}.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

/// Returns `${TEMP}/mirage-cli2/{chain_id}.pid`.
pub fn pid_file_path(chain_id: &str) -> std::path::PathBuf {
    std::env::temp_dir()
        .join("mirage-cli2")
        .join(format!("{chain_id}.pid"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── SupervisorMsg serialization ───────────────────────────────────────────

    #[test]
    fn peer_table_update_round_trip() {
        let pt_json = r#"{"generation":7,"peers":[{"node_id":"v2-nl","pubkey_hex":"aabb","addr":"1.2.3.4:5000","mesh_id":"staging-v2","responsible_monitor_id":"","labels":[],"updated_at_ms":0}]}"#;
        let msg = SupervisorMsg::PeerTableUpdate {
            peer_table_json: pt_json.to_string(),
        };
        let serialized = serde_json::to_string(&msg).expect("serialize PeerTableUpdate");
        // Must contain the discriminator tag.
        assert!(
            serialized.contains(r#""type":"peer_table_update""#),
            "expected tag in: {serialized}"
        );
        let deserialized: SupervisorMsg =
            serde_json::from_str(&serialized).expect("deserialize PeerTableUpdate");
        match deserialized {
            SupervisorMsg::PeerTableUpdate {
                peer_table_json: got,
            } => {
                assert_eq!(got, pt_json);
            }
            other => panic!("expected PeerTableUpdate, got {other:?}"),
        }
    }

    #[test]
    fn start_without_peer_table_omits_field() {
        let msg = SupervisorMsg::Start {
            chain_id: "chain-a".to_string(),
            socks5_bind: "127.0.0.1:1080".to_string(),
            config_path: "/tmp/cfg.json".to_string(),
            peer_table_json: None,
        };
        let serialized = serde_json::to_string(&msg).expect("serialize Start (no peer_table)");
        // peer_table_json is skip_serializing_if = None → must NOT appear in wire.
        assert!(
            !serialized.contains("peer_table_json"),
            "expected field absent, got: {serialized}"
        );
        let deserialized: SupervisorMsg =
            serde_json::from_str(&serialized).expect("deserialize Start (no peer_table)");
        match deserialized {
            SupervisorMsg::Start {
                peer_table_json: None,
                ..
            } => {}
            other => panic!("expected Start{{peer_table_json:None}}, got {other:?}"),
        }
    }

    #[test]
    fn start_with_peer_table_round_trip() {
        let pt_json = r#"{"generation":3,"peers":[]}"#;
        let msg = SupervisorMsg::Start {
            chain_id: "chain-b".to_string(),
            socks5_bind: "127.0.0.1:1081".to_string(),
            config_path: "/tmp/cfg2.json".to_string(),
            peer_table_json: Some(pt_json.to_string()),
        };
        let serialized = serde_json::to_string(&msg).expect("serialize Start (with peer_table)");
        let deserialized: SupervisorMsg =
            serde_json::from_str(&serialized).expect("deserialize Start (with peer_table)");
        match deserialized {
            SupervisorMsg::Start {
                peer_table_json: Some(got),
                chain_id,
                ..
            } => {
                assert_eq!(got, pt_json);
                assert_eq!(chain_id, "chain-b");
            }
            other => panic!("expected Start{{peer_table_json:Some}}, got {other:?}"),
        }
    }

    // ── Supervisor: extract peer_table JSON from a config snapshot ────────────
    //
    // This unit-test mocks the supervisor's extraction logic without pulling in
    // the full MonitorLink / mirage-core dependency.  It verifies that a
    // JSON string holding a PeerTable can be re-serialized from a known
    // serde_json::Value and compared.

    #[test]
    fn supervisor_extract_peer_table_from_config_snapshot() {
        // Simulated raw config snapshot as the supervisor receives it.
        let snapshot_json = r#"{
            "version": 5,
            "chains": [],
            "peer_table": {
                "generation": 12,
                "peers": [
                    {
                        "node_id": "entry-nl",
                        "pubkey_hex": "deadbeef",
                        "addr": "10.0.0.1:4000",
                        "mesh_id": "staging-v2",
                        "responsible_monitor_id": "",
                        "updated_at_ms": 1700000000000
                    }
                ]
            }
        }"#;

        let value: serde_json::Value =
            serde_json::from_str(snapshot_json).expect("parse snapshot");

        // Supervisor extracts the peer_table sub-object and serializes it.
        let pt_value = value
            .get("peer_table")
            .expect("peer_table key present")
            .clone();
        let pt_json = serde_json::to_string(&pt_value).expect("serialize peer_table");

        // Must be valid JSON containing expected fields.
        assert!(pt_json.contains("entry-nl"), "node_id present");
        assert!(pt_json.contains("12"), "generation present");

        // Roundtrip: parse back and check generation.
        let parsed: serde_json::Value =
            serde_json::from_str(&pt_json).expect("roundtrip parse");
        assert_eq!(
            parsed["generation"].as_u64().unwrap_or(0),
            12,
            "generation roundtrip"
        );
    }
}
