mod killswitch;

use std::{collections::{HashMap, HashSet, VecDeque}, net::SocketAddr, path::PathBuf, sync::Arc};

use clap::Parser;
use mirage_core::{config::{ClientConfig, TcpProtocolId}, routing::RoutingTable};
use mirage_protocol::{
    aliases::{mirage_base, mirage_tls},
    negotiation::ProtocolChain,
    traits::TcpProtocol,
};

use mirage_client_core::{
    dns_forwarder, management,
    endpoint_protocol::EndpointProtocol,
    discovery::select_all_chains,
    http_proxy::HttpProxyServer,
    log_capture::LogCaptureLayer,
    monitor_link::MonitorLink,
    mux_pool::MuxPool,
    pool::TunnelPool,
    runtime::ClientRuntime,
    socks5::Socks5Server,
    tun_bridge::TunBridge,
};
use tokio_util::sync::CancellationToken;
use tracing::info;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// mirage-client — local SOCKS5 proxy that routes traffic through the Mirage chain.
#[derive(Parser)]
#[command(name = "mirage-client", version, about)]
struct Args {
    /// Path to the ClientConfig JSON file.
    #[arg(long, short = 'c')]
    config: PathBuf,

    /// Local SOCKS5 listen address (default chain).
    #[arg(long, short = 'b', default_value = "127.0.0.1:19480")]
    bind: SocketAddr,
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    use tracing_subscriber::prelude::*;

    let log_buffer: Arc<std::sync::Mutex<VecDeque<String>>> =
        Arc::new(std::sync::Mutex::new(VecDeque::with_capacity(2000)));

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let fmt_layer = tracing_subscriber::fmt::layer();
    let capture_layer = LogCaptureLayer::new(Arc::clone(&log_buffer), 2000);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .with(capture_layer)
        .init();

    let args = Args::parse();

    let raw = std::fs::read_to_string(&args.config).map_err(|e| {
        format!("failed to read config {:?}: {}", args.config, e)
    })?;
    let config: ClientConfig = serde_json::from_str(&raw)
        .map_err(|e| format!("failed to parse config: {}", e))?;

    // Validate the private key format (64-char hex) even though the new driver
    // API derives its own key material internally.  This catches config errors early.
    let _key_bytes = decode_hex_key(&config.private_key)
        .map_err(|e| format!("invalid private_key in config: {}", e))?;

    // ── Build protocol chain ───────────────────────────────────────────────────
    // Mirage is always the first (highest-priority) protocol.  When `psk` is
    // present in the config we additionally add a TLS-camouflaged variant as a
    // second option; the ProtocolChain will try them in order.
    //
    // UDP protocols (Hysteria2, QUIC, etc.) are NOT part of this TCP chain —
    // they are handled via dedicated UDP sockets.
    let mut protocols: Vec<Box<dyn TcpProtocol>> = Vec::new();

    protocols.push(Box::new(EndpointProtocol::new(
        mirage_base(),
        TcpProtocolId::Mirage,
    )));

    if config.psk.is_some() {
        // When PSK is configured, offer TLS camouflage as a second option.
        // ShadowTLS is not in the new aliases API; mirage_tls provides the
        // closest equivalent (TCP + real TLS 1.3 outer session).
        protocols.push(Box::new(EndpointProtocol::new(
            mirage_tls("cloudflare.com", vec!["h2".into(), "http/1.1".into()]),
            TcpProtocolId::MirageShadowTls,
        )));
    }

    let chain_protocols = Arc::new(ProtocolChain::new(protocols));
    let router = Arc::new(RoutingTable::new(&config.routing));

    // ── Build tunnel pools (one per chain) ────────────────────────────────────
    let all_chains = select_all_chains(&config);
    if all_chains.is_empty() {
        return Err("no chains configured".into());
    }

    let default_chain = all_chains[0].clone();
    let default_chain_id = default_chain.chain_id.clone();

    let pool_cfg_base = config.pool.clone();
    let use_mux = config.use_mux;

    // Build one TunnelPool per chain.
    let mut pools: HashMap<String, Arc<TunnelPool>> = HashMap::new();
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

        let pool = TunnelPool::new(
            (*chain).clone(),
            Arc::clone(&chain_protocols),
            pool_cfg,
        )
        .await
        .map_err(|e| format!("tunnel pool init failed for chain '{}': {e}", chain.chain_id))?;

        pools.insert(chain.chain_id.clone(), pool);
    }

    // Default pool (highest-priority chain).
    let default_pool = pools
        .get(&default_chain_id)
        .cloned()
        .expect("default pool inserted above");

    // ── Mux pool (default chain only) ─────────────────────────────────────────
    let mux_pool: Option<Arc<MuxPool>> = if use_mux {
        info!("creating mux pool (multiplexed mode, default chain)");
        let mux = MuxPool::new(
            default_chain.clone(),
            Arc::clone(&chain_protocols),
            pool_cfg_base.clone(),
        )
        .await
        .map_err(|e| format!("mux pool init failed: {e}"))?;
        Some(mux)
    } else {
        None
    };

    // ── Kill switch ────────────────────────────────────────────────────────────
    // Must be activated before the server starts accepting connections.
    // Collect ALL unique L1 relay addresses from ALL chains so the kill switch
    // permits pool reconnections on every chain, not just the first one.
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

    // ── DNS forwarder ──────────────────────────────────────────────────────────
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

    // ── Mode flags ─────────────────────────────────────────────────────────────
    let mode_http = config.http_proxy.enabled;

    let management_key = config.management_key.clone();
    let management_addr = config
        .management_addr
        .clone()
        .unwrap_or_else(|| "127.0.0.1:19483".to_owned());

    // ── Build ClientRuntime (multi-pool) ───────────────────────────────────────
    let runtime = ClientRuntime::new_multi(
        pools.clone(),
        default_chain_id.clone(),
        mux_pool.clone(),
        config.clone(),
        true,      // mode_socks5 is always active
        mode_http,
        false,     // TUN mode is handled by mirage2tun
        Arc::clone(&log_buffer),
    );

    // ── Monitor link (optional) ────────────────────────────────────────────────
    // When `monitor_url` is set in config, spawn a background task that polls
    // mirage2monitor for chain config updates and reacts to them.
    if let Some(ref monitor_url) = config.monitor_url {
        let (link, mut config_rx) = MonitorLink::new(monitor_url.clone());
        link.spawn();
        info!(url = %monitor_url, "monitor link spawned");

        // TUN bridge: when tun_api_url is configured, forward every config
        // update to the TUN chain registry automatically.
        if let Some(ref tun_api_url) = config.tun_api_url {
            let bridge = Arc::new(TunBridge::new(tun_api_url.clone()));
            let tun_rx = link.subscribe();
            bridge.spawn_watcher(tun_rx);
            info!(url = %tun_api_url, "TUN bridge spawned");
        }

        // Watch for config changes and perform pool hot-swap.
        let rt_for_watcher = Arc::clone(&runtime);
        let protocols_for_watcher = Arc::clone(&chain_protocols);
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

                // Load current pools snapshot for diffing.
                let current = rt_for_watcher.pool_snapshot();

                let mut new_map: HashMap<String, Arc<TunnelPool>> = HashMap::new();
                let mut added: Vec<String> = Vec::new();
                let mut kept: Vec<String> = Vec::new();

                for monitor_chain in &cfg.chains {
                    let chain_id = &monitor_chain.name;

                    if let Some(existing_pool) = current.get(chain_id) {
                        // Chain unchanged — reuse existing pool.
                        new_map.insert(chain_id.clone(), Arc::clone(existing_pool));
                        kept.push(chain_id.clone());
                    } else {
                        // New chain — build a ChainConfig from monitor data.
                        let pool_cfg = if use_mux_for_watcher {
                            let mut c = pool_cfg_for_watcher.clone();
                            c.pool_size = 3;
                            c
                        } else {
                            pool_cfg_for_watcher.clone()
                        };

                        let node_addr: std::net::SocketAddr =
                            match monitor_chain.node_addr.parse() {
                                Ok(a) => a,
                                Err(e) => {
                                    tracing::warn!(
                                        chain = %chain_id,
                                        addr = %monitor_chain.node_addr,
                                        "monitor: invalid node_addr, skipping: {e}"
                                    );
                                    continue;
                                }
                            };

                        let chain_cfg = mirage_core::config::ChainConfig {
                            chain_id: chain_id.clone(),
                            l1: Some(mirage_core::config::ChainNode {
                                addr: node_addr,
                                public_key: String::new(),
                                sni: None,
                            }),
                            l2: None,
                            l1_transparent: false,
                            hops: Vec::new(),
                            handshake: Default::default(),
                            reply: Default::default(),
                            protocol: mirage_core::config::ProtocolId::Tcp(TcpProtocolId::Mirage),
                            priority: 0,
                            shaping: Default::default(),
                            camouflage: Default::default(),
                            cover_traffic: Default::default(),
                            byte_cipher: None,
                            socks5_bind: None,
                        };

                        match TunnelPool::new(
                            chain_cfg,
                            Arc::clone(&protocols_for_watcher),
                            pool_cfg,
                        )
                        .await
                        {
                            Ok(pool) => {
                                new_map.insert(chain_id.clone(), pool);
                                added.push(chain_id.clone());
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

                let removed: Vec<String> = current
                    .keys()
                    .filter(|id| !new_map.contains_key(*id))
                    .cloned()
                    .collect();

                if added.is_empty() && removed.is_empty() {
                    tracing::debug!(version = cfg.version, "pool hot-swap: no changes");
                    continue;
                }

                tracing::info!(
                    added = ?added,
                    removed = ?removed,
                    kept = ?kept,
                    "pool hot-swap"
                );

                rt_for_watcher.swap_pools(new_map);
            }
        });
    }

    // ── HTTP CONNECT proxy ─────────────────────────────────────────────────────
    if mode_http {
        let bind_addr: SocketAddr = config.http_proxy.bind_addr.parse().map_err(|e| {
            format!("invalid http_proxy.bind_addr '{}': {e}", config.http_proxy.bind_addr)
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

    info!(bind = %args.bind, "mirage-client starting");

    // ── Management server (optional) ───────────────────────────────────────────
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

    // ── Per-chain SOCKS5 listeners ─────────────────────────────────────────────
    // Build failover list (all chains sorted by priority, for each server
    // the list excludes the primary pool of that server).
    //
    // The `all_chains` slice is already sorted highest-priority-first.
    let chains_sorted = select_all_chains(&config);
    let chain_pools_sorted: Vec<(String, Arc<TunnelPool>)> = chains_sorted
        .iter()
        .filter_map(|c| {
            pools.get(&c.chain_id).map(|p| (c.chain_id.clone(), Arc::clone(p)))
        })
        .collect();

    // Spawn a dedicated SOCKS5 listener for every chain with a `socks5_bind`.
    for chain in &chains_sorted {
        let Some(ref bind_str) = chain.socks5_bind else {
            continue;
        };
        let bind_addr: SocketAddr = bind_str.parse().map_err(|e| {
            format!(
                "invalid socks5_bind '{}' for chain '{}': {e}",
                bind_str, chain.chain_id
            )
        })?;

        let primary_pool = pools
            .get(&chain.chain_id)
            .cloned()
            .expect("pool was inserted above for every chain");

        // Failover = all other chains in priority order.
        let failover: Vec<(String, Arc<TunnelPool>)> = chain_pools_sorted
            .iter()
            .filter(|(id, _)| id != &chain.chain_id)
            .cloned()
            .collect();

        let server = Socks5Server::new_with_failover(bind_addr, failover);
        let router2 = Arc::clone(&router);
        let cfg2 = Arc::clone(&config);
        let chain_id = chain.chain_id.clone();
        tokio::spawn(async move {
            info!(
                chain = %chain_id,
                addr = %bind_addr,
                "per-chain SOCKS5 listener starting"
            );
            if let Err(e) = server.run(primary_pool, None, cfg2, router2).await {
                tracing::error!(chain = %chain_id, "per-chain SOCKS5 error: {e}");
            }
        });
    }

    // ── Default SOCKS5 listener (args.bind → default chain) ───────────────────
    // Build failover list for the default listener (all non-default chains).
    let default_failover: Vec<(String, Arc<TunnelPool>)> = chain_pools_sorted
        .iter()
        .filter(|(id, _)| id != &default_chain_id)
        .cloned()
        .collect();

    let server = Socks5Server::new_with_failover(args.bind, default_failover);

    tokio::select! {
        res = server.run(default_pool, mux_pool, config, router) => {
            res?;
        }
        _ = tokio::signal::ctrl_c() => {
            info!("received Ctrl-C, shutting down");
        }
    }

    // Runtime is kept alive for management server background task until
    // the process exits.  Cancel any running optional servers first.
    let _ = runtime.http_cancel.lock().await.take().map(|t| t.cancel());
    let _ = runtime.tun_cancel.lock().await.take().map(|t| t.cancel());
    drop(runtime);

    // ── Graceful shutdown ──────────────────────────────────────────────────────
    if let Some(ref mut ks) = kill_switch {
        if let Err(e) = ks.deactivate() {
            tracing::warn!("kill switch deactivate on shutdown: {e}");
        }
    }

    Ok(())
}

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
