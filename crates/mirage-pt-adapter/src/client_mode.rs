//! Client mode (contract §3) — SOCKS5 listener → one-time `ChainBinding` +
//! `PeerTable` fetch → per-connection Mirage dial → splice.
//!
//! Directly ported from `operator-box/src/socks5_test.rs`'s
//! `handle_socks5_conn` (lines 238-363), with exactly ONE behavioral
//! change (contract §3 step 4): the frame sent into the tunnel is always
//! `cfg.bridge_target`, never the SOCKS5-CONNECT-parsed address — a Tor
//! PT's local SOCKS5 proxy has exactly one real destination (the bridge),
//! regardless of what Tor's own bridge-line nominally asked for. The
//! SOCKS5 handshake itself is still parsed and replied to correctly
//! because Tor's local SOCKS5 client expects a normal RFC1928 exchange.

use std::sync::Arc;

use mirage_core::mirage::mesh::{ChainBinding, PeerTable, PeerTableEntry};
use mirage_mesh_client::{mesh_protocol_chain_versioned, MeshClient, MeshClientConfig};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, warn};

use crate::config::ClientModeConfig;
use crate::pt_ipc::{PtStdout, TRANSPORT_NAME};

/// Fetch binding+peers, bind the SOCKS5 listener, emit `CMETHOD`, run the
/// accept loop until the process exits.
pub async fn run_client_mode(
    cfg: ClientModeConfig,
    ipc: &mut PtStdout<tokio::io::Stdout>,
) -> anyhow::Result<()> {
    // Step 1 (contract §3): one-time ChainBinding + PeerTable fetch,
    // mirroring native_test.rs's fetch_from_monitor flow (binding +
    // port_assignment overrides + peer_table), over plain reqwest HTTPS
    // against the monitor's public HTTP surface — pt-adapter carries no
    // operator SenderProof identity, so it cannot use operator-box's own
    // wire-authenticated relay_get.
    let (binding, peer_table) = match fetch_binding_and_peers(&cfg).await {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "client_mode: startup fetch failed");
            let _ = ipc
                .cmethod_error(TRANSPORT_NAME, &format!("startup fetch: {e}"))
                .await;
            let _ = ipc.cmethods_done().await;
            return Err(e);
        }
    };
    let binding = Arc::new(binding);
    let peer_table = Arc::new(peer_table);
    let mesh_id_bytes = Arc::new(cfg.mesh_id.clone());
    let bridge_target = Arc::new(cfg.bridge_target.clone());

    // Step 2: OS-assigned ephemeral port — PT-v1 contract: any local port
    // is fine, report it back via CMETHOD (unlike socks5_test.rs's
    // caller-configurable port).
    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(e) => {
            let _ = ipc
                .cmethod_error(TRANSPORT_NAME, &format!("bind: {e}"))
                .await;
            let _ = ipc.cmethods_done().await;
            return Err(e.into());
        }
    };
    let local_addr = listener.local_addr()?;

    // Step 3: report success.
    ipc.cmethod(TRANSPORT_NAME, local_addr).await?;
    ipc.cmethods_done().await?;
    info!(
        socks5_addr = %local_addr,
        chain_id = %cfg.chain_id,
        bridge_target = %cfg.bridge_target,
        "client_mode: listening"
    );

    // Steps 4-7: accept loop, one task per connection.
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "client_mode: accept error");
                continue;
            }
        };
        info!(peer = %peer, "client_mode: accepted connection");
        let binding = Arc::clone(&binding);
        let peer_table = Arc::clone(&peer_table);
        let mesh_id_bytes = Arc::clone(&mesh_id_bytes);
        let bridge_target = Arc::clone(&bridge_target);
        tokio::spawn(async move {
            if let Err(e) =
                handle_socks5_conn(stream, &binding, &peer_table, &mesh_id_bytes, &bridge_target)
                    .await
            {
                warn!(error = %e, "client_mode: conn handler error");
            }
        });
    }
}

/// Ported from `socks5_test.rs::handle_socks5_conn` — see module doc for
/// the one intentional behavioral change.
async fn handle_socks5_conn(
    mut stream: TcpStream,
    binding: &ChainBinding,
    peer_table: &PeerTable,
    mesh_id_bytes: &[u8],
    bridge_target: &str,
) -> anyhow::Result<()> {
    // ── SOCKS5 greeting ──────────────────────────────────────────────
    // Read: [0x05, nmethods, method0, ...]
    let mut hdr = [0u8; 2];
    stream.read_exact(&mut hdr).await?;
    anyhow::ensure!(hdr[0] == 0x05, "socks5 version mismatch: got 0x{:02x}", hdr[0]);
    let nmethods = hdr[1] as usize;
    if nmethods > 0 {
        let mut methods = vec![0u8; nmethods];
        stream.read_exact(&mut methods).await?;
    }
    // Reply: no auth required.
    stream.write_all(&[0x05, 0x00]).await?;

    // ── SOCKS5 CONNECT request ───────────────────────────────────────
    // [0x05, cmd=0x01, rsv=0x00, atyp, ...addr, port_hi, port_lo]. The
    // address+port are parsed only so the handshake stays RFC1928-valid
    // for Tor's local SOCKS5 client — they are never used as the Mirage
    // target (contract §3 step 4: that is always `bridge_target`).
    let mut req_hdr = [0u8; 4];
    stream.read_exact(&mut req_hdr).await?;
    anyhow::ensure!(req_hdr[0] == 0x05, "socks5 version in request: 0x{:02x}", req_hdr[0]);
    anyhow::ensure!(
        req_hdr[1] == 0x01,
        "socks5 unsupported cmd 0x{:02x} (only CONNECT)",
        req_hdr[1]
    );

    match req_hdr[3] {
        0x01 => {
            // IPv4 — 4 bytes.
            let mut addr = [0u8; 4];
            stream.read_exact(&mut addr).await?;
        }
        0x03 => {
            // Domain — 1 byte length + N bytes.
            let mut len_buf = [0u8; 1];
            stream.read_exact(&mut len_buf).await?;
            let mut domain = vec![0u8; len_buf[0] as usize];
            stream.read_exact(&mut domain).await?;
        }
        0x04 => {
            // IPv6 — 16 bytes.
            let mut addr = [0u8; 16];
            stream.read_exact(&mut addr).await?;
        }
        other => anyhow::bail!("unsupported atyp 0x{other:02x}"),
    }
    let mut port_buf = [0u8; 2];
    stream.read_exact(&mut port_buf).await?;

    // Reply success: [0x05,0x00,0x00,0x01, 0,0,0,0, 0,0]
    stream
        .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await?;

    info!(bridge_target = %bridge_target, "client_mode: CONNECT — forced to fixed bridge target");

    // ── Dial Mirage tunnel ────────────────────────────────────────────
    // V9 forward-secret wire, matching socks5_test.rs:308.
    let protocol_chain = Arc::new(mesh_protocol_chain_versioned(mesh_id_bytes, 9));
    let mut priv_key = [0u8; 32];
    getrandom::getrandom(&mut priv_key).map_err(|e| anyhow::anyhow!("keygen: {e}"))?;

    let client = MeshClient::new(MeshClientConfig {
        priv_key: zeroize::Zeroizing::new(priv_key),
        mesh_id: mesh_id_bytes.to_vec(),
        protocol: mirage_core::config::TcpProtocolId::Mirage,
        use_mux: false,
        protocol_chain,
        cover_traffic: Default::default(),
        credential: None,
        credential_params: None,
    });

    let mut tunnel = client
        .dial(binding, peer_table)
        .await
        .map_err(|e| anyhow::anyhow!("dial: {e}"))?;

    // Single-hop direct chain only (contract §3 step 7 / §6.1): raw
    // (non-length-prefixed) target frame, matching direct.rs's
    // handle_exit_with_prefix parse path. The spike's test chain is
    // deliberately hops=1 — reject anything else rather than silently
    // mis-framing the target.
    anyhow::ensure!(
        binding.legs.is_empty(),
        "mirage-pt-adapter spike only supports single-hop direct chains (binding.legs non-empty)"
    );
    tunnel
        .send(bridge_target.as_bytes())
        .await
        .map_err(|e| anyhow::anyhow!("send target frame: {e}"))?;

    // ── Bidirectional splice ────────────────────────────────────────
    // Do NOT wrap `tunnel` in a Mutex — socks5_test.rs's doc comment
    // explains why: a single Mutex around the tunnel deadlocks
    // recv-vs-send (recv holds the lock awaiting the response, so send
    // can never deliver the request).
    let (mut io, _pump) = mirage_mesh_client::tunnel_into_io(tunnel);
    let _ = tokio::io::copy_bidirectional(&mut stream, &mut io).await;

    Ok(())
}

/// Minimal port_assignment shape from monitor's `/relay/chains/{id}`
/// response — mirrors `native_test.rs::PortAssignment`.
#[derive(Debug, Deserialize)]
struct PortAssignment {
    node_id: String,
    bind_port: u16,
}

/// Fetch `ChainBinding` + `PeerTable` from `cfg.via_monitor`'s public HTTP
/// surface. Mirrors `native_test.rs::fetch_from_monitor`'s documented flow
/// (binding fetch, port_assignment override, peer_table fetch) but over
/// plain reqwest HTTPS instead of the Mirage-wire-authenticated
/// `MeshClient::relay_get` operator-box uses internally.
async fn fetch_binding_and_peers(
    cfg: &ClientModeConfig,
) -> anyhow::Result<(ChainBinding, PeerTable)> {
    let http = reqwest::Client::builder().build()?;

    let mut chain_req = http.get(format!(
        "{}/relay/chains/{}",
        cfg.monitor_base_url.trim_end_matches('/'),
        cfg.chain_id
    ));
    if let Some(token) = &cfg.monitor_bearer {
        chain_req = chain_req.bearer_auth(token);
    }
    let chain_json: Value = chain_req.send().await?.error_for_status()?.json().await?;

    let binding: ChainBinding = serde_json::from_value(
        chain_json
            .get("binding")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("chain response missing 'binding' field"))?,
    )?;

    let port_assignments: Vec<PortAssignment> = chain_json
        .get("port_assignments")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let mut peers_req = http.get(format!(
        "{}/relay/peers",
        cfg.monitor_base_url.trim_end_matches('/')
    ));
    if let Some(token) = &cfg.monitor_bearer {
        peers_req = peers_req.bearer_auth(token);
    }
    let peers_json: Value = peers_req.send().await?.error_for_status()?.json().await?;
    let mut peer_table = parse_peer_table(&peers_json)?;

    // Apply bind_port overrides (mirrors native_test.rs's
    // resolve_chain_dial_state pattern) so the dial hits the correct
    // per-chain bind_port on each node's public IP, not the node's
    // default listener port.
    for entry in &mut peer_table.peers {
        if let Some(pa) = port_assignments.iter().find(|a| a.node_id == entry.node_id.as_str()) {
            let host = host_from_addr_owned(&entry.addr);
            entry.addr = format!("{host}:{}", pa.bind_port);
        }
    }

    info!(
        chain_id = %cfg.chain_id,
        via = %cfg.via_monitor,
        peers = peer_table.peers.len(),
        "client_mode: fetched binding+peers"
    );

    Ok((binding, peer_table))
}

fn parse_peer_table(v: &Value) -> anyhow::Result<PeerTable> {
    let generation = v.get("generation").and_then(|g| g.as_u64()).unwrap_or(0);

    let raw_peers = v
        .get("peers")
        .and_then(|p| p.as_array())
        .ok_or_else(|| anyhow::anyhow!("peer_table JSON missing 'peers' array"))?;

    let mut peers = Vec::with_capacity(raw_peers.len());
    for (i, peer) in raw_peers.iter().enumerate() {
        let node_id = peer
            .get("node_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("peers[{i}] missing node_id"))?
            .to_owned();
        let pubkey_hex = peer
            .get("pubkey_hex")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("peers[{i}] ({node_id}) missing pubkey_hex"))?
            .to_owned();
        let addr = peer
            .get("addr")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("peers[{i}] ({node_id}) missing addr"))?
            .to_owned();
        let labels: Vec<String> = peer
            .get("labels")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let updated_at_ms = peer.get("updated_at_ms").and_then(Value::as_u64).unwrap_or(0);
        let responsible_monitor_id = peer
            .get("responsible_monitor_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let mesh_id_str = peer.get("mesh_id").and_then(Value::as_str).unwrap_or("").to_owned();

        peers.push(PeerTableEntry {
            mesh_id: mesh_id_str.into(),
            node_id: node_id.into(),
            pubkey_hex,
            addr,
            labels,
            updated_at_ms,
            responsible_monitor_id,
            aead_suite_preference: Vec::new(),
        });
    }

    Ok(PeerTable { generation, peers })
}

/// Extract host from `host:port` or `[ipv6]:port` as an owned String.
fn host_from_addr_owned(addr: &str) -> String {
    if addr.starts_with('[') {
        if let Some(end) = addr.find(']') {
            return addr[..=end].to_owned();
        }
    }
    match addr.rfind(':') {
        Some(pos) => addr[..pos].to_owned(),
        None => addr.to_owned(),
    }
}
