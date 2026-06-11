pub mod killswitch;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
};

use arc_swap::ArcSwap;
use mirage_core::{
    config::{AuthMode, ChainConfig, ClientConfig, MirageEndpointConfig, ProtocolId,
             ReplyMode, TransportCamouflage, TransportKind},
    mirage::mesh::PeerTable,
    routing::RoutingTable,
};
use mirage_protocol::{
    endpoint_protocol::EndpointProtocol,
    negotiation::ProtocolChain,
    traits::TcpProtocol,
};
use mirage_client_core::{
    dns_forwarder, management,
    chain_set::ChainSet,
    discovery::select_all_chains,
    http_proxy::HttpProxyServer,
    monitor_link::{MonitorLink, MonitorTransport},
    mux_pool::MuxPool,
    pool::TunnelPool,
    runtime::ClientRuntime,
    socks5::{Socks5Server, SocksFailurePolicy},
    tun_bridge::TunBridge,
    wire_health::WireHealthMap,
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

    let mesh_id = config.mesh_id.as_bytes().to_vec();
    let router = Arc::new(RoutingTable::new(&config.routing));

    // ── Shared peer table (Phase 8.5) ─────────────────────────────────────────
    //
    // Created upfront as an empty table and shared with all TunnelPool /
    // MuxPool instances.  When a MonitorLink is present it writes into this
    // same Arc on every Config frame, so pools transparently pick up rotated
    // node keys on the next dial without any additional wiring.
    //
    // Without a monitor link only chains with `hop.public_key = Some(hex)`
    // are dialable; chains relying on dynamic resolution will fail at the
    // first dial with `PubkeyUnresolved`.
    let peer_table: Arc<ArcSwap<PeerTable>> =
        Arc::new(ArcSwap::from_pointee(PeerTable::empty()));

    // ── Shared credential params arc (ZT dial authz) ──────────────────────────
    //
    // Created upfront as None and shared with all TunnelPool / MuxPool instances.
    // MonitorLink writes into this ArcSwap on every `CredentialParams` WS frame
    // from the monitor, so pools transparently pick up new epoch secrets without
    // any extra plumbing.  The same arc is passed to MonitorLink::new_with_credential_arc
    // below so that ONE ArcSwap is shared between pools and the link.
    let credential_params: Arc<ArcSwap<Option<mirage_core::crypto::credential::CredentialParams>>> =
        Arc::new(ArcSwap::from_pointee(None));

    // ── Shared blind-token pool arc (Phase 4 spend) ────────────────────────────
    //
    // MonitorLink fills this on every successful blind-issue exchange.  Pools
    // drain it one token per dial.  Pre-allocated and shared so pools created
    // before MonitorLink see the same pool.
    let token_pool: Arc<ArcSwap<mirage_client_core::token_pool::TokenPool>> =
        Arc::new(ArcSwap::from_pointee(mirage_client_core::token_pool::TokenPool::default()));

    // ── Ring-membership credential (scheme 0x04, static) ─────────────────────
    //
    // Parsed once from config at startup. None when the config doesn't carry
    // ring_member_sk_hex / ring_path / ring_epoch_id. Shared across all pools
    // via Arc (no ArcSwap needed — the material never changes at runtime).
    let ring_credential: Arc<Option<mirage_client_core::RingCredential>> =
        Arc::new(ring_credential_from_config(&config));

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
        let protocols = match build_chain_protocol_chain(&chain_owned, &mesh_id) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                tracing::warn!(
                    chain = %chain_owned.chain_id,
                    "per-chain protocol build failed, skipping: {e}"
                );
                continue;
            }
        };
        let mid = mesh_id.clone();
        let pt = Arc::clone(&peer_table);
        let cred = Arc::clone(&credential_params);
        let tok = Arc::clone(&token_pool);
        let ring = Arc::clone(&ring_credential);
        pool_tasks.spawn(async move {
            let id = chain_owned.chain_id.clone();
            match TunnelPool::new(chain_owned, protocols, pool_cfg, mid, pt, cred, tok, ring).await {
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

    // ── Mux pools (one per chain when use_mux=true AND reply=onion) ──────────────
    // MuxPool requires onion reply mode. Chains with reply=raw have exits that
    // byte-pump raw data; a mux session on top causes frame garbling → EOF.
    let mut mux_pools: HashMap<String, Arc<MuxPool>> = HashMap::new();
    if use_mux {
        info!(chains = all_chains.len(), "creating mux pools (onion-reply chains only)");
        let mut mux_tasks = tokio::task::JoinSet::new();
        for chain in &all_chains {
            // Skip reply=raw chains — they never get AEAD-wrapped mux replies.
            if chain.reply != ReplyMode::Onion {
                continue;
            }
            let chain_owned = (*chain).clone();
            let id = chain.chain_id.clone();
            let protocols = match build_chain_protocol_chain(&chain_owned, &mesh_id) {
                Ok(p) => Arc::new(p),
                Err(e) => {
                    tracing::warn!(chain = %id, "per-chain protocol build failed (mux): {e}");
                    continue;
                }
            };
            let cfg = pool_cfg_base.clone();
            let mid = mesh_id.clone();
            let pt = Arc::clone(&peer_table);
            let cred = Arc::clone(&credential_params);
            let tok = Arc::clone(&token_pool);
            let ring = Arc::clone(&ring_credential);
            mux_tasks.spawn(async move {
                match MuxPool::new(chain_owned, protocols, cfg, mid, pt, cred, tok, ring).await {
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
        // Entry hop (hops[0]) is the relay the kill-switch must allow through.
        let relay_addrs: Vec<SocketAddr> = config
            .chains
            .iter()
            .filter_map(|c| {
                let addr_str = c.hops.first()?.addr.clone();
                addr_str.parse::<SocketAddr>().ok()
            })
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        // Invariant 1: bypass is sacred — source bypass CIDRs from the
        // routing config's `direct_cidrs` list (the same list that drives
        // RouteDecision::Direct in the SOCKS5/HTTP proxy path).
        let bypass_cidrs = config.routing.direct_cidrs.clone();
        let mut ks = killswitch::KillSwitch::new(relay_addrs)
            .with_bypass_cidrs(bypass_cidrs);
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
    //
    // `mux_pools` is the full per-chain map — health_watcher chain switches
    // pick up the matching mux pool without reconnecting. `mux_pool` is the
    // default chain's pool (legacy single-pool API kept for callers that
    // don't yet know about the map).
    let runtime = ClientRuntime::new_multi(
        pools.clone(),
        default_chain_id.clone(),
        mux_pool.clone(),
        mux_pools.clone(),
        config.clone(),
        true,      // mode_socks5 always active
        mode_http,
        false,     // TUN mode handled by mirage2tun
        Arc::clone(&log_buffer),
    );

    // ── Monitor link (optional) ───────────────────────────────────────────────
    if let Some(ref monitor_url) = config.monitor_url {
        // Convert ClientConfig.monitor_transports (Wave 2 field) to MonitorTransport
        // enum used by MonitorLink.  Falls back to the legacy monitor_url when the
        // transports list is absent.
        use mirage_core::config::MonitorTransportConfig;
        let mut transports: Vec<MonitorTransport> = Vec::new();

        // Wire transport — prepended first (preferred over DirectWs/OverChain).
        // Built from ClientConfig.monitor_wire_addr + monitor_wire_pubkey_hex
        // when both are present (task #78 — mirrors l0-sentinel construction).
        if let (Some(ref wire_addr), Some(ref wire_pubkey)) = (
            &config.monitor_wire_addr,
            &config.monitor_wire_pubkey_hex,
        ) {
            tracing::info!(
                addr = %wire_addr,
                "cli: Wire transport configured — prepending to MonitorLink transports"
            );
            transports.push(MonitorTransport::Wire {
                monitor_addr: wire_addr.clone(),
                monitor_pubkey_hex: wire_pubkey.clone(),
                mesh_id: config.mesh_id.clone(),
                host_hint: None,
            });
        }

        // OverChain transports only (DirectWs removed — Wire is primary,
        // OverChain is the restricted-network fallback).
        for t in &config.monitor_transports {
            match t {
                MonitorTransportConfig::DirectWs { .. } => {
                    // DirectWs has been removed — skip silently.
                    tracing::debug!("cli: DirectWs transport config ignored (removed)");
                }
                MonitorTransportConfig::OverChain {
                    chain_id,
                    monitor_endpoint,
                    ..
                } => {
                    // cli is monolithic — no separate worker process — so
                    // worker_socks5 for option-b SOCKS5 fallback points at
                    // our own SOCKS5 listener for this chain.
                    let socks5_bind = config
                        .chains
                        .iter()
                        .find(|c| c.chain_id == *chain_id)
                        .and_then(|c| c.socks5_bind.as_ref())
                        .and_then(|s| s.parse::<std::net::SocketAddr>().ok());
                    let monitor_addr = monitor_endpoint.parse::<std::net::SocketAddr>();
                    match (socks5_bind, monitor_addr) {
                        (Some(ws), Ok(addr)) => {
                            transports.push(MonitorTransport::OverChain {
                                chain_id: chain_id.clone().into(),
                                worker_socks5: ws,
                                monitor_addr: addr,
                            });
                        }
                        (None, _) => {
                            tracing::warn!(
                                "cli: over_chain references unknown chain_id={chain_id} or invalid socks5_bind"
                            );
                        }
                        (_, Err(e)) => {
                            tracing::warn!(
                                "cli: invalid monitor_endpoint '{}': {e}",
                                monitor_endpoint
                            );
                        }
                    }
                }
            }
        }

        // Blind-token credential wiring: verify monitor-pushed CredentialParams
        // against the operator key (when configured), and arm blind-RSA batch
        // requests with the enrol token. Mirrors l0-sentinel; on a bad
        // operator_pub_hex we log and proceed unverified rather than crash.
        let operator_pub = config.operator_pub_hex.as_deref().and_then(|hex_str| {
            match MonitorLink::parse_operator_pub_hex(hex_str) {
                Ok(k) => {
                    info!("cli: operator_pub_hex decoded — CredentialParams will be verified");
                    Some(k)
                }
                Err(e) => {
                    tracing::warn!(
                        "cli: invalid operator_pub_hex ({e}); CredentialParams applied unverified"
                    );
                    None
                }
            }
        });

        let (link, mut config_rx) = MonitorLink::new_with_credential_arc(
            vec![monitor_url.clone()],
            None,
            Arc::clone(&peer_table),
            Arc::clone(&credential_params),
            Arc::clone(&token_pool),
            ChainSet::empty(),
            transports,
            config.mesh_id.clone(),
            config.client_id.clone(),
            operator_pub,
        );
        link.set_enrol_token(config.enrol_token.clone());
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
        let mesh_id_for_watcher = mesh_id.clone();
        let pool_cfg_for_watcher = pool_cfg_base.clone();
        let use_mux_for_watcher = use_mux;
        let peer_table_for_watcher = Arc::clone(&peer_table);
        let credential_params_for_watcher = Arc::clone(&credential_params);
        let token_pool_for_watcher = Arc::clone(&token_pool);
        let ring_credential_for_watcher = Arc::clone(&ring_credential);

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
                            &mesh_id_for_watcher,
                        ) {
                            Ok(p) => Arc::new(p),
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
                            mesh_id_for_watcher.clone(),
                            Arc::clone(&peer_table_for_watcher),
                            Arc::clone(&credential_params_for_watcher),
                            Arc::clone(&token_pool_for_watcher),
                            Arc::clone(&ring_credential_for_watcher),
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

    // Shared wire-health map — single instance shared across all SOCKS5 servers
    // so a dead wire is detected immediately regardless of which server port
    // the next flow arrives on.
    let shared_wire_health = WireHealthMap::new();

    // ── Per-chain SOCKS5 listeners ────────────────────────────────────────────
    let chains_sorted = select_all_chains(&config);

    // ── Build per-chain kill-switch policy map ────────────────────────────────
    //
    // Maps chain_id → SocksFailurePolicy derived from ChainConfig.kill_switch
    // + ChainConfig.group.  Used by Socks5Server to enforce per-wire Strict /
    // General / Default semantics without any further config reads at runtime.
    // Built from chains_sorted (Arc<ClientConfig>-borrowed) after Arc wrapping.
    let all_chain_policies: HashMap<String, SocksFailurePolicy> = chains_sorted
        .iter()
        .map(|c| {
            let policy = SocksFailurePolicy::from_chain_config(
                c.kill_switch.as_ref(),
                c.group.as_deref(),
                &c.chain_id,
            );
            (c.chain_id.clone(), policy)
        })
        .collect();
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

        let server = Socks5Server::new_with_failover(chain_bind_addr, failover)
            .with_kill_switch(chain.chain_id.clone(), all_chain_policies.clone())
            .with_shared_wire_health(Arc::clone(&shared_wire_health));
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

    let server = Socks5Server::new_with_failover(bind_addr, default_failover)
        .with_kill_switch(default_chain_id.clone(), all_chain_policies)
        .with_shared_wire_health(Arc::clone(&shared_wire_health));

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

/// Build a per-chain `ProtocolChain` for a single Mirage TCP chain.
///
/// Reads `chain.hops[0].camouflage` and `chain.hops[0].byte_cipher` for the
/// entry-leg outer wrap. `mesh_id` is baked into every `EndpointProtocol` so
/// the Blake3 confirm label binds the handshake to this mesh.
///
/// Returns `Err` if the chain has no hops or if the protocol is not TCP
/// (UDP chains use `establish_udp_chain_tunnel` directly).
fn build_chain_protocol_chain(
    chain: &ChainConfig,
    mesh_id: &[u8],
) -> Result<ProtocolChain, BoxError> {
    let proto_id = match &chain.protocol {
        ProtocolId::Tcp(id) => *id,
        ProtocolId::Udp(_) => {
            return Err("UDP chains do not use ProtocolChain; handled by dial-core".into());
        }
    };

    let entry_hop = chain.hops.first().ok_or_else(|| {
        format!("chain {}: hops list is empty — cannot build ProtocolChain", chain.chain_id)
    })?;

    let endpoint = MirageEndpointConfig {
        transport: TransportKind::Tcp,
        auth: AuthMode::Blake3,
        camouflage: entry_hop.camouflage.clone().unwrap_or(TransportCamouflage::None),
        byte_cipher: None,
        byte_cipher_mode: entry_hop.byte_cipher.clone(),
    };

    let mut protocols: Vec<Box<dyn TcpProtocol>> = Vec::new();
    protocols.push(Box::new(EndpointProtocol::new(
        endpoint,
        proto_id,
        mesh_id.to_vec(),
    ).with_wire_version(9)));
    Ok(ProtocolChain::new(protocols))
}

/// Parse ring-membership credential material from the client config, if present
/// and well-formed. Returns None (with a warn) if any field is missing/invalid —
/// the client then dials without a 0x04 credential.
fn ring_credential_from_config(
    config: &mirage_core::config::ClientConfig,
) -> Option<mirage_client_core::RingCredential> {
    let sk_hex = config.ring_member_sk_hex.as_deref()?;
    let path_cfg = config.ring_path.as_ref()?;
    let epoch_id = config.ring_epoch_id?;
    let sk_bytes: [u8; 32] = hex::decode(sk_hex)
        .ok()?
        .try_into()
        .ok()?;
    let member_sk = mirage_ringmember::MemberSecret::from_bytes(sk_bytes).or_else(|| {
        tracing::warn!("ring_credential_from_config: ring_member_sk_hex is not a canonical ristretto scalar — ring credential disabled");
        None
    })?;
    let mut path = Vec::with_capacity(path_cfg.len());
    for step in path_cfg {
        let sib: [u8; 32] = hex::decode(&step.sibling_hex)
            .ok()?
            .try_into()
            .ok()?;
        path.push((sib, step.is_right));
    }
    Some(mirage_client_core::RingCredential::new(member_sk, path, epoch_id))
}
