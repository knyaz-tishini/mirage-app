pub mod killswitch;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
};

use mirage_core::{
    config::{ChainConfig, ClientConfig, ProtocolId, TcpProtocolId},
    crypto::KeyPair,
    routing::RoutingTable,
};
use mirage_protocol::{
    aliases::{mirage_base, mirage_tls},
    canonical::{
        shadowtls_v3::{ShadowTlsV3Config, ShadowTlsV3Protocol},
        websocket_xray::WebsocketXrayProtocol,
    },
    negotiation::ProtocolChain,
    traits::TcpProtocol,
};
use mirage_client_core::{
    dns_forwarder, management,
    endpoint_protocol::EndpointProtocol,
    discovery::select_all_chains,
    http_proxy::HttpProxyServer,
    monitor_link::MonitorLink,
    mux_pool::MuxPool,
    pool::TunnelPool,
    runtime::ClientRuntime,
    socks5::Socks5Server,
    tun_bridge::TunBridge,
};
use mirage_runtime_guard::{MemoryGuard, MemoryGuardConfig};
use tokio_util::sync::CancellationToken;
use tracing::info;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Options for `run()`.  Standalone binary passes all-true; embedded caller
/// passes `install_panic_hook: false` and `install_memory_guard: false`.
pub struct CliRunOptions {
    /// Path to the ClientConfig JSON file.
    pub config_path: PathBuf,
    /// Local SOCKS5 listen address for the default chain.
    /// `None` → `127.0.0.1:19480`.
    pub bind: Option<SocketAddr>,
    /// Whether to install a process-exit panic hook.
    /// Set `false` when embedding — the host process manages panic handling.
    pub install_panic_hook: bool,
    /// Whether to spawn the memory guard watchdog.
    /// Set `false` when embedding — the host process manages memory pressure.
    pub install_memory_guard: bool,
    /// If `true`, the memory guard calls `exit(0)` on fatal threshold.
    /// Ignored when `install_memory_guard` is `false`.
    pub exit_on_memory_fatal: bool,
}

impl CliRunOptions {
    /// Defaults for standalone binary usage.
    pub fn standalone(config_path: PathBuf, bind: SocketAddr) -> Self {
        Self {
            config_path,
            bind: Some(bind),
            install_panic_hook: true,
            install_memory_guard: true,
            exit_on_memory_fatal: true,
        }
    }
}

/// Run the mirage-client-cli business logic.
///
/// Blocking until the default SOCKS5 server exits or Ctrl-C is received.
/// Suitable for both standalone binary (`#[tokio::main]`) and embedded use
/// (`tokio::task::spawn(cli::run(opts))`).
pub async fn run(opts: CliRunOptions) -> Result<(), BoxError> {
    // ── Panic hook ────────────────────────────────────────────────────────────
    if opts.install_panic_hook {
        std::panic::set_hook(Box::new(|info| {
            let location = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_else(|| "<unknown>".to_string());
            let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "<non-string panic payload>".to_string()
            };
            tracing::error!(
                location = %location,
                message = %msg,
                "panic — process exiting for respawn"
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
            std::process::exit(1);
        }));
    }

    // ── Load config ───────────────────────────────────────────────────────────
    let raw = std::fs::read_to_string(&opts.config_path).map_err(|e| {
        format!("failed to read config {:?}: {}", opts.config_path, e)
    })?;
    let config: ClientConfig =
        serde_json::from_str(&raw).map_err(|e| format!("failed to parse config: {}", e))?;

    // Validate the private key format (64-char hex) even though the new driver
    // API derives its own key material internally.  This catches config errors early.
    let _key_bytes = decode_hex_key(&config.private_key)
        .map_err(|e| format!("invalid private_key in config: {}", e))?;

    // ── Build default (Mirage-only) protocol chain ────────────────────────────
    //
    // Used as a fallback for chains that select a non-canonical protocol or
    // omit per-chain config blocks. Chains that select WebsocketXray /
    // ShadowTlsV3 / etc. get a dedicated single-protocol chain built per-chain
    // below (see `build_chain_protocol_chain`).
    let mut default_protocols: Vec<Box<dyn TcpProtocol>> = Vec::new();
    default_protocols.push(Box::new(EndpointProtocol::new(
        mirage_base(),
        TcpProtocolId::Mirage,
    )));
    if config.psk.is_some() {
        default_protocols.push(Box::new(EndpointProtocol::new(
            mirage_tls("cloudflare.com", vec!["h2".into(), "http/1.1".into()]),
            TcpProtocolId::MirageShadowTls,
        )));
    }
    let default_chain_protocols = Arc::new(ProtocolChain::new(default_protocols));

    // Client identity keypair (decoded from config.private_key) — required for
    // canonical protocol constructors that take a KeyPair (WebsocketXray /
    // ShadowTlsV3 / VlessReality).
    let client_keypair_bytes: [u8; 32] = decode_hex_key(&config.private_key)
        .map_err(|e| format!("invalid private_key in config: {}", e))?;

    let router = Arc::new(RoutingTable::new(&config.routing));

    // ── Build tunnel pools (one per chain) ────────────────────────────────────
    let all_chains = select_all_chains(&config);
    if all_chains.is_empty() {
        return Err("no chains configured".into());
    }

    let explicit_default: Option<String> = serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|v| {
            v.get("routing")?
                .get("default_chain")?
                .as_str()
                .map(String::from)
        });
    let default_chain_id = explicit_default.unwrap_or_else(|| all_chains[0].chain_id.clone());
    let _default_chain = (*all_chains
        .iter()
        .find(|c| c.chain_id == default_chain_id)
        .unwrap_or(&all_chains[0]))
    .clone();

    let pool_cfg_base = config.pool.clone();
    let use_mux = config.use_mux;

    let mut pool_tasks = tokio::task::JoinSet::new();
    for chain in &all_chains {
        let pool_cfg = if use_mux {
            let mut cfg = pool_cfg_base.clone();
            cfg.pool_size = 3;
            cfg
        } else {
            pool_cfg_base.clone()
        };

        info!(
            chain_id = %chain.chain_id,
            pool_size = pool_cfg.pool_size,
            use_mux,
            idle_timeout_secs = pool_cfg.idle_timeout_secs,
            "creating tunnel pool for chain"
        );

        let chain_owned = (*chain).clone();
        let protocols = match build_chain_protocol_chain(&chain_owned, &client_keypair_bytes) {
            Ok(Some(p)) => Arc::new(p),
            Ok(None) => Arc::clone(&default_chain_protocols),
            Err(e) => {
                tracing::warn!(
                    chain = %chain_owned.chain_id,
                    "per-chain protocol build failed, skipping: {e}"
                );
                continue;
            }
        };
        pool_tasks.spawn(async move {
            let id = chain_owned.chain_id.clone();
            match TunnelPool::new(chain_owned, protocols, pool_cfg).await {
                Ok(pool) => Some((id, pool)),
                Err(e) => {
                    tracing::warn!(chain = %id, "pool init failed, skipping: {e}");
                    None
                }
            }
        });
    }

    let mut pools: HashMap<String, Arc<TunnelPool>> = HashMap::new();
    while let Some(result) = pool_tasks.join_next().await {
        if let Ok(Some((id, pool))) = result {
            pools.insert(id, pool);
        }
    }

    if pools.is_empty() {
        return Err("all tunnel pools failed to initialize".into());
    }

    let default_pool = pools
        .get(&default_chain_id)
        .cloned()
        .expect("default pool inserted above");

    // ── Mux pools (one per chain when use_mux=true) ───────────────────────────
    let mut mux_pools: HashMap<String, Arc<MuxPool>> = HashMap::new();
    if use_mux {
        info!(chains = all_chains.len(), "creating mux pools (all chains)");
        let mut mux_tasks = tokio::task::JoinSet::new();
        for chain in &all_chains {
            let chain_owned = (*chain).clone();
            let id = chain.chain_id.clone();
            let protocols = match build_chain_protocol_chain(&chain_owned, &client_keypair_bytes) {
                Ok(Some(p)) => Arc::new(p),
                Ok(None) => Arc::clone(&default_chain_protocols),
                Err(e) => {
                    tracing::warn!(chain = %id, "per-chain protocol build failed (mux): {e}");
                    continue;
                }
            };
            let cfg = pool_cfg_base.clone();
            mux_tasks.spawn(async move {
                match MuxPool::new(chain_owned, protocols, cfg).await {
                    Ok(mux) => Some((id, mux)),
                    Err(e) => {
                        tracing::warn!(
                            chain = %id,
                            "mux pool init failed, will fall back to TCP: {e}"
                        );
                        None
                    }
                }
            });
        }
        while let Some(result) = mux_tasks.join_next().await {
            if let Ok(Some((id, mux))) = result {
                mux_pools.insert(id, mux);
            }
        }
    }

    let mux_pool: Option<Arc<MuxPool>> = mux_pools.get(&default_chain_id).cloned();

    // ── Kill switch ───────────────────────────────────────────────────────────
    let mut kill_switch = if config.kill_switch {
        if config.chains.is_empty() {
            return Err("no chains configured for kill switch".into());
        }
        let relay_addrs: Vec<SocketAddr> = config
            .chains
            .iter()
            .filter_map(|c| c.l1.as_ref().map(|n| n.addr))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let mut ks = killswitch::KillSwitch::new(relay_addrs);
        ks.activate()
            .map_err(|e| format!("kill switch activate failed: {e}"))?;
        Some(ks)
    } else {
        None
    };

    // ── DNS forwarder ─────────────────────────────────────────────────────────
    if config.dns.enabled {
        let dns_cfg = config.dns.clone();
        match dns_forwarder::DnsForwarder::new(dns_cfg) {
            Ok(fwd) => {
                let fwd = Arc::new(fwd);
                tokio::spawn(async move {
                    if let Err(e) = fwd.run().await {
                        tracing::error!("dns forwarder error: {e}");
                    }
                });
            }
            Err(e) => {
                return Err(format!("dns forwarder init failed: {e}").into());
            }
        }
    }

    // ── Mode flags ────────────────────────────────────────────────────────────
    let mode_http = config.http_proxy.enabled;

    let management_key = config.management_key.clone();
    let management_addr = config
        .management_addr
        .clone()
        .unwrap_or_else(|| "127.0.0.1:19483".to_owned());

    // ── Log buffer (created here for embedded usage; main.rs creates it for standalone) ──
    // In embedded mode the caller is responsible for tracing setup; we still
    // need a buffer for the management server's log endpoint.
    let log_buffer: Arc<std::sync::Mutex<VecDeque<String>>> =
        Arc::new(std::sync::Mutex::new(VecDeque::with_capacity(2000)));

    // ── Build ClientRuntime (multi-pool) ──────────────────────────────────────
    let runtime = ClientRuntime::new_multi(
        pools.clone(),
        default_chain_id.clone(),
        mux_pool.clone(),
        config.clone(),
        true,      // mode_socks5 always active
        mode_http,
        false,     // TUN mode handled by mirage2tun
        Arc::clone(&log_buffer),
    );

    // ── Monitor link (optional) ───────────────────────────────────────────────
    if let Some(ref monitor_url) = config.monitor_url {
        let (link, mut config_rx) = MonitorLink::new(vec![monitor_url.clone()], None);
        link.spawn();
        info!(url = %monitor_url, "monitor link spawned");

        let health_rx = link.subscribe();
        let rt_for_health = Arc::clone(&runtime);
        let tun_url_for_health = config.tun_api_url.clone();
        tokio::spawn(mirage_client_core::health_watcher::run_health_watcher(
            rt_for_health,
            health_rx,
            tun_url_for_health,
        ));
        info!("health watcher spawned");

        if let Some(ref tun_api_url) = config.tun_api_url {
            let bridge = Arc::new(TunBridge::new(tun_api_url.clone()));
            let tun_rx = link.subscribe();
            bridge.spawn_watcher(tun_rx);
            info!(url = %tun_api_url, "TUN bridge spawned");
        }

        let rt_for_watcher = Arc::clone(&runtime);
        let default_protocols_for_watcher = Arc::clone(&default_chain_protocols);
        let client_kp_for_watcher = client_keypair_bytes;
        let pool_cfg_for_watcher = pool_cfg_base.clone();
        let use_mux_for_watcher = use_mux;

        tokio::spawn(async move {
            loop {
                if config_rx.changed().await.is_err() {
                    break;
                }
                let snapshot = config_rx.borrow().clone();
                let Some(cfg) = snapshot else { continue };

                tracing::info!(
                    version = cfg.version,
                    chains = cfg.chains.len(),
                    "monitor config received — evaluating pool hot-swap"
                );

                let current = rt_for_watcher.pool_snapshot();
                let mut new_map: HashMap<String, Arc<TunnelPool>> = (*current).clone();
                let mut added: Vec<String> = Vec::new();
                let kept: Vec<String> = current.keys().cloned().collect();

                for monitor_chain in &cfg.chains {
                    let chain_id = monitor_chain.name();

                    if current.contains_key(chain_id) {
                        // Already exists — keep the live pool, healthy flag
                        // is consumed by health_watcher separately.
                    } else {
                        let pool_cfg = if use_mux_for_watcher {
                            let mut c = pool_cfg_for_watcher.clone();
                            c.pool_size = 3;
                            c
                        } else {
                            pool_cfg_for_watcher.clone()
                        };

                        let chain_cfg = monitor_chain.chain.clone();
                        let protocols_for_chain = match build_chain_protocol_chain(
                            &chain_cfg,
                            &client_kp_for_watcher,
                        ) {
                            Ok(Some(p)) => Arc::new(p),
                            Ok(None) => Arc::clone(&default_protocols_for_watcher),
                            Err(e) => {
                                tracing::warn!(
                                    chain = %chain_id,
                                    "monitor: per-chain protocol build failed: {e}"
                                );
                                continue;
                            }
                        };

                        match TunnelPool::new(
                            chain_cfg,
                            protocols_for_chain,
                            pool_cfg,
                        )
                        .await
                        {
                            Ok(pool) => {
                                new_map.insert(chain_id.to_string(), pool);
                                added.push(chain_id.to_string());
                            }
                            Err(e) => {
                                tracing::warn!(
                                    chain = %chain_id,
                                    "monitor: pool init failed, skipping: {e}"
                                );
                            }
                        }
                    }
                }

                if added.is_empty() {
                    tracing::debug!(version = cfg.version, "pool hot-swap: no changes");
                    continue;
                }

                tracing::info!(added = ?added, kept = ?kept, "pool hot-swap");
                rt_for_watcher.swap_pools(new_map);
            }
        });
    }

    // ── HTTP CONNECT proxy ────────────────────────────────────────────────────
    if mode_http {
        let bind_addr: SocketAddr =
            config.http_proxy.bind_addr.parse().map_err(|e| {
                format!(
                    "invalid http_proxy.bind_addr '{}': {e}",
                    config.http_proxy.bind_addr
                )
            })?;
        let http_server = HttpProxyServer::new(bind_addr);
        let pool2 = Arc::clone(&default_pool);
        let mux_pool2 = mux_pool.clone();
        let router2 = Arc::clone(&router);
        let token = CancellationToken::new();
        let token_clone = token.clone();
        tokio::spawn(async move {
            tokio::select! {
                result = http_server.run(pool2, mux_pool2, router2) => {
                    if let Err(e) = result {
                        tracing::error!("HTTP CONNECT proxy error: {e}");
                    }
                }
                _ = token_clone.cancelled() => {
                    info!("HTTP CONNECT proxy cancelled");
                }
            }
        });
        *runtime.http_cancel.lock().await = Some(token);
        info!(%bind_addr, "HTTP CONNECT proxy spawned");
    }

    // ── Memory guard ──────────────────────────────────────────────────────────
    let _memory_guard = if opts.install_memory_guard {
        let exit_on_fatal = opts.exit_on_memory_fatal;
        Some(MemoryGuard::spawn(
            MemoryGuardConfig::default(),
            Box::new(move || {
                tracing::warn!("memory guard: fatal threshold — exiting for respawn");
                if exit_on_fatal {
                    std::process::exit(0);
                }
            }),
        ))
    } else {
        None
    };

    let bind_addr = opts
        .bind
        .unwrap_or_else(|| "127.0.0.1:19480".parse().expect("hardcoded addr is valid"));

    info!(bind = %bind_addr, "mirage-client starting");

    // ── Management server (optional) ──────────────────────────────────────────
    let config = Arc::new(config);

    if let Some(key) = management_key {
        let rt = Arc::clone(&runtime);
        let psk = Arc::new(key);
        let addr = management_addr.clone();
        tokio::spawn(async move {
            if let Err(e) = management::server::run(rt, psk, &addr).await {
                tracing::error!("management server error: {e}");
            }
        });
        info!(addr = %management_addr, "management server spawned");
    } else {
        tracing::debug!("management_key not set — management server disabled");
    }

    // ── Per-chain SOCKS5 listeners ────────────────────────────────────────────
    let chains_sorted = select_all_chains(&config);
    let chain_pools_sorted: Vec<(String, Arc<TunnelPool>)> = chains_sorted
        .iter()
        .filter_map(|c| {
            pools
                .get(&c.chain_id)
                .map(|p| (c.chain_id.clone(), Arc::clone(p)))
        })
        .collect();

    for chain in &chains_sorted {
        let Some(ref bind_str) = chain.socks5_bind else {
            continue;
        };
        let chain_bind_addr: SocketAddr = bind_str.parse().map_err(|e| {
            format!(
                "invalid socks5_bind '{}' for chain '{}': {e}",
                bind_str, chain.chain_id
            )
        })?;

        let primary_pool = pools
            .get(&chain.chain_id)
            .cloned()
            .expect("pool was inserted above for every chain");

        let failover: Vec<(String, Arc<TunnelPool>)> = chain_pools_sorted
            .iter()
            .filter(|(id, _)| id != &chain.chain_id)
            .cloned()
            .collect();

        let server = Socks5Server::new_with_failover(chain_bind_addr, failover);
        let router2 = Arc::clone(&router);
        let cfg2 = Arc::clone(&config);
        let chain_id = chain.chain_id.clone();
        let chain_mux = mux_pools.get(&chain_id).cloned();
        tokio::spawn(async move {
            info!(
                chain = %chain_id,
                addr = %chain_bind_addr,
                mux = chain_mux.is_some(),
                "per-chain SOCKS5 listener starting"
            );
            if let Err(e) = server.run(primary_pool, chain_mux, cfg2, router2).await {
                tracing::error!(chain = %chain_id, "per-chain SOCKS5 error: {e}");
            }
        });
    }

    // ── Default SOCKS5 listener ───────────────────────────────────────────────
    let default_failover: Vec<(String, Arc<TunnelPool>)> = chain_pools_sorted
        .iter()
        .filter(|(id, _)| id != &default_chain_id)
        .cloned()
        .collect();

    let server = Socks5Server::new_with_failover(bind_addr, default_failover);

    tokio::select! {
        res = server.run(default_pool, mux_pool, config, router) => {
            res?;
        }
        _ = tokio::signal::ctrl_c() => {
            info!("received Ctrl-C, shutting down");
        }
    }

    let _ = runtime.http_cancel.lock().await.take().map(|t| t.cancel());
    let _ = runtime.tun_cancel.lock().await.take().map(|t| t.cancel());
    drop(runtime);

    // ── Graceful shutdown ─────────────────────────────────────────────────────
    if let Some(ref mut ks) = kill_switch {
        if let Err(e) = ks.deactivate() {
            tracing::warn!("kill switch deactivate on shutdown: {e}");
        }
    }

    Ok(())
}

// ── Re-export LogCaptureLayer for main.rs ─────────────────────────────────────
pub use mirage_client_core::log_capture::LogCaptureLayer;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Decode a 64-character hex string into a 32-byte array.
fn decode_hex_key(hex: &str) -> Result<[u8; 32], String> {
    if hex.len() != 64 {
        return Err(format!("expected 64 hex chars, got {}", hex.len()));
    }
    let mut out = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_nibble(b: u8) -> Result<u8, String> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(format!("invalid hex character: {}", b as char)),
    }
}

/// Build a per-chain `ProtocolChain` containing exactly the canonical TCP
/// protocol the chain selected, instantiated from the chain's per-protocol
/// config block.
///
/// Returns:
/// - `Ok(Some(chain))` — chain selected a canonical protocol and we built it.
///   The caller MUST use this `ProtocolChain` for that chain (not the global
///   default Mirage chain).
/// - `Ok(None)` — chain selected `Mirage` / `MirageShadowTls` (legacy) /
///   anything not yet wired here. Caller falls back to the default chain.
/// - `Err(...)` — config block was missing or malformed for a protocol that
///   requires it (e.g. ShadowTlsV3 PSK hex decode failed). Chain should be
///   skipped so the cli doesn't silently dial Mirage where ShadowTlsV3 was
///   asked.
fn build_chain_protocol_chain(
    chain: &ChainConfig,
    client_keypair_bytes: &[u8; 32],
) -> Result<Option<ProtocolChain>, BoxError> {
    use mirage_core::config::TcpProtocolId as P;

    let proto_id = match &chain.protocol {
        ProtocolId::Tcp(id) => *id,
        ProtocolId::Udp(_) => return Ok(None), // UDP chains use a different driver path
    };

    let kp = KeyPair::from_secret(*client_keypair_bytes);
    let mut protocols: Vec<Box<dyn TcpProtocol>> = Vec::new();
    match proto_id {
        P::WebsocketXray => {
            let cfg = chain.websocket_xray.clone().ok_or_else(|| {
                format!(
                    "chain {}: protocol=WebsocketXray but websocket_xray block missing",
                    chain.chain_id
                )
            })?;
            protocols.push(Box::new(WebsocketXrayProtocol::new(kp, cfg.path, cfg.host)));
        }
        P::ShadowTlsV3 | P::MirageShadowTls => {
            let cfg = chain.shadowtls_v3.clone().ok_or_else(|| {
                format!(
                    "chain {}: protocol={:?} but shadowtls_v3 block missing",
                    chain.chain_id, proto_id
                )
            })?;
            let runtime_cfg = ShadowTlsV3Config {
                psk: cfg.psk,
                backend_host: cfg.backend_host,
                backend_port: cfg.backend_port,
            };
            let proto = ShadowTlsV3Protocol::new(kp, &runtime_cfg)
                .map_err(|e| format!("chain {}: ShadowTlsV3Protocol::new failed: {e}", chain.chain_id))?;
            protocols.push(Box::new(proto));
        }
        // Mirage / MirageTls / MirageWebsocket / MirageReality / VlessReality /
        // exit endpoints — fall through to the default Mirage-based protocol
        // chain. Adding wiring for the remaining canonical xray protocols is
        // a follow-up (todo: VlessReality, MirageWebsocket).
        _ => return Ok(None),
    }
    Ok(Some(ProtocolChain::new(protocols)))
}
