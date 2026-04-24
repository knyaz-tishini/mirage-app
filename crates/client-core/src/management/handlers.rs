use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use mirage_core::config::ShapingPreset;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::management::types::{
    ChainPoolInfo, ClientAdminStatus, ReconfigResponse, SetCamouflageRequest,
    SetCoverTrafficRequest, SetModeRequest, SetShapingRequest, SwitchChainRequest,
};
use crate::runtime::ClientRuntime;

/// `GET /admin/health` — public endpoint, no authentication required.
pub async fn get_health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

/// `GET /admin/status` — returns a live snapshot of client state.
pub async fn get_status(State(runtime): State<Arc<ClientRuntime>>) -> impl IntoResponse {
    let stats = runtime.active_pool().pool_stats().await;
    let active_chain_id = runtime.active_chain_id.load().as_ref().clone();
    let config_version = runtime.config_version.load(Ordering::SeqCst);
    let (mode_socks5, mode_http, mode_tun) = runtime.modes();

    // Read cover_traffic enabled flag from the active chain config.
    let cover_traffic_enabled = {
        let config = runtime.config.load();
        let chain_id = runtime.active_chain_id.load();
        config
            .chains
            .iter()
            .find(|c| c.chain_id == chain_id.as_ref().as_str())
            .map(|c| c.cover_traffic.enabled)
            .unwrap_or(false)
    };

    let status = ClientAdminStatus {
        active_chain_id,
        mode_socks5,
        mode_http,
        mode_tun,
        pool_size: stats.size,
        pool_idle: stats.idle,
        pool_leased: stats.leased,
        pool_reconnecting: stats.reconnecting,
        cover_traffic_enabled,
        config_version,
    };

    Json(status)
}

/// `POST /admin/chain/switch` — switch the active chain at runtime.
///
/// Finds the chain by `chain_id` in the loaded config, updates the active
/// chain reference, and optionally marks all idle pool slots for reconnection
/// so new leases will use the new chain endpoints.
pub async fn post_chain_switch(
    State(runtime): State<Arc<ClientRuntime>>,
    Json(req): Json<SwitchChainRequest>,
) -> Response {
    // Look up the requested chain in the current config.
    let config = runtime.config.load();
    let chain = match config.chains.iter().find(|c| c.chain_id == req.chain_id) {
        Some(c) => c.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("chain '{}' not found in config", req.chain_id) })),
            )
                .into_response();
        }
    };

    // Update the active chain reference atomically.
    runtime
        .active_chain_id
        .store(Arc::new(chain.chain_id.clone()));

    // Update the pool's active chain under the mutex.
    // When drain=true (or unset, defaulting to false), mark idle slots for
    // reconnection so the next reconnect cycle uses the new chain.
    let drain = req.drain.unwrap_or(false);
    let pool = runtime.active_pool();
    if drain {
        pool.switch_chain(chain).await;
    } else {
        pool.switch_chain_lazy(chain).await;
    }

    let new_version = runtime.config_version.fetch_add(1, Ordering::SeqCst) + 1;

    let message = if drain {
        format!(
            "switched to chain '{}' (drain: idle slots marked for reconnect)",
            req.chain_id
        )
    } else {
        format!(
            "switched to chain '{}' (lazy: new leases use new chain, in-flight unaffected)",
            req.chain_id
        )
    };

    Json(ReconfigResponse {
        status: "ok".to_owned(),
        config_version: new_version,
        message,
    })
    .into_response()
}

/// `PUT /admin/mode` — enable or disable HTTP CONNECT proxy and/or TUN stack at runtime.
///
/// SOCKS5 cannot be disabled if it is the only active mode.
/// Enabling HTTP or TUN spawns a new task with a fresh `CancellationToken`.
/// Disabling cancels the token and clears the handle.
///
/// Note: enabling HTTP or TUN requires that the bind address is already
/// configured in the client config.  If the HTTP proxy bind address is
/// unavailable (e.g. port in use), the spawn will log an error.
pub async fn put_mode(
    State(runtime): State<Arc<ClientRuntime>>,
    Json(req): Json<SetModeRequest>,
) -> Response {
    let (socks5_now, http_now, tun_now) = runtime.modes();

    // ── Validate: cannot disable SOCKS5 if it would be the only mode left ──
    if req.socks5 == Some(false) {
        let http_after = req.http.unwrap_or(http_now as bool);
        let tun_after = req.tun.unwrap_or(tun_now as bool);
        if !http_after && !tun_after {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "cannot disable socks5: it is the only active mode"
                })),
            )
                .into_response();
        }
    }

    let mut changes: Vec<String> = Vec::new();

    // ── Handle HTTP mode ───────────────────────────────────────────────────────
    if let Some(enable_http) = req.http {
        if enable_http && !http_now {
            // Enabling: read bind address from config and spawn.
            let config = runtime.config.load();
            let bind_addr_str = config.http_proxy.bind_addr.clone();
            drop(config);

            let bind_addr: SocketAddr = match bind_addr_str.parse() {
                Ok(a) => a,
                Err(e) => {
                    return (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(json!({
                            "error": format!(
                                "invalid http_proxy.bind_addr '{}': {e}",
                                bind_addr_str
                            )
                        })),
                    )
                        .into_response();
                }
            };

            let token = CancellationToken::new();
            let token_clone = token.clone();
            let pool2 = runtime.active_pool();
            let mux_pool2 = runtime.mux_pool.clone();
            let config2 = runtime.config.load();
            let router2 = Arc::new(mirage_core::routing::RoutingTable::new(&config2.routing));
            drop(config2);

            let http_server = crate::http_proxy::HttpProxyServer::new(bind_addr);
            tokio::spawn(async move {
                tokio::select! {
                    result = http_server.run(pool2, mux_pool2, router2) => {
                        if let Err(e) = result {
                            tracing::error!("HTTP CONNECT proxy error: {e}");
                        }
                    }
                    _ = token_clone.cancelled() => {
                        tracing::info!("HTTP CONNECT proxy cancelled via mode switch");
                    }
                }
            });

            *runtime.http_cancel.lock().await = Some(token);
            runtime.mode_http.store(true, Ordering::SeqCst);
            changes.push(format!("HTTP CONNECT proxy enabled on {bind_addr}"));
        } else if !enable_http && http_now {
            // Disabling: cancel the running task.
            if let Some(token) = runtime.http_cancel.lock().await.take() {
                token.cancel();
            }
            runtime.mode_http.store(false, Ordering::SeqCst);
            changes.push("HTTP CONNECT proxy disabled".to_owned());
        }
        // else: no change needed
    }

    // ── Handle TUN mode ────────────────────────────────────────────────────────
    if let Some(enable_tun) = req.tun {
        if enable_tun && !tun_now {
            // Enabling TUN: only supported on Linux; the startup path requires
            // access to the relay address and socks port which are not tracked
            // on the runtime after startup.  Return 501 to indicate that a
            // full restart with `tun.enabled = true` in the config is needed.
            return (
                StatusCode::NOT_IMPLEMENTED,
                Json(json!({
                    "error": "enabling TUN at runtime is not yet supported; \
                              set tun.enabled=true in config and restart"
                })),
            )
                .into_response();
        } else if !enable_tun && tun_now {
            // Disabling: cancel the TUN task token and update the flag.
            if let Some(token) = runtime.tun_cancel.lock().await.take() {
                token.cancel();
            }
            runtime.mode_tun.store(false, Ordering::SeqCst);
            changes.push(
                "TUN mode disabled (iptables rules remain until process restart)".to_owned(),
            );
        }
    }

    // ── Handle SOCKS5 mode flag ────────────────────────────────────────────────
    // The SOCKS5 listener itself is managed in main.rs and cannot be
    // spawned/cancelled from here without a larger refactor.  We track the flag
    // so status reflects intent; disabling SOCKS5 here just records that it has
    // been administratively suppressed — the listener keeps running until
    // process restart.
    if let Some(enable_socks5) = req.socks5 {
        if !enable_socks5 && socks5_now {
            runtime.mode_socks5.store(false, Ordering::SeqCst);
            changes.push(
                "SOCKS5 flag cleared (listener continues until restart)".to_owned(),
            );
        } else if enable_socks5 && !socks5_now {
            runtime.mode_socks5.store(true, Ordering::SeqCst);
            changes.push("SOCKS5 flag set".to_owned());
        }
    }

    let new_version = runtime.config_version.fetch_add(1, Ordering::SeqCst) + 1;

    let message = if changes.is_empty() {
        "no mode changes applied".to_owned()
    } else {
        changes.join("; ")
    };

    Json(ReconfigResponse {
        status: "ok".to_owned(),
        config_version: new_version,
        message,
    })
    .into_response()
}

/// `PUT /admin/camouflage` — update the camouflage mode for a chain in the live config.
///
/// The change is applied to the in-memory `ClientConfig` snapshot via ArcSwap.
/// New tunnels (established after the swap) will use the new camouflage mode.
/// Existing tunnels are unaffected until they reconnect.
pub async fn put_camouflage(
    State(runtime): State<Arc<ClientRuntime>>,
    Json(req): Json<SetCamouflageRequest>,
) -> Response {
    let config = runtime.config.load();
    let mut new_config: mirage_core::config::ClientConfig = (**config).clone();
    drop(config);

    let chain = match new_config
        .chains
        .iter_mut()
        .find(|c| c.chain_id == req.chain_id)
    {
        Some(c) => c,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("chain '{}' not found", req.chain_id) })),
            )
                .into_response();
        }
    };

    chain.camouflage = req.mode;
    runtime.config.store(Arc::new(new_config));

    let new_version = runtime.config_version.fetch_add(1, Ordering::SeqCst) + 1;

    Json(ReconfigResponse {
        status: "ok".to_owned(),
        config_version: new_version,
        message: format!("camouflage updated for chain '{}'", req.chain_id),
    })
    .into_response()
}

/// `PUT /admin/shaping` — update the traffic-shaping preset for a chain in the live config.
pub async fn put_shaping(
    State(runtime): State<Arc<ClientRuntime>>,
    Json(req): Json<SetShapingRequest>,
) -> Response {
    let preset: ShapingPreset = match req.preset.as_str() {
        "none" => ShapingPreset::None,
        "web_browsing" => ShapingPreset::WebBrowsing,
        "cloud_api" => ShapingPreset::CloudApi,
        "streaming" => ShapingPreset::Streaming,
        other => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "error": format!(
                        "unknown shaping preset '{}'; valid: none, web_browsing, cloud_api, streaming",
                        other
                    )
                })),
            )
                .into_response();
        }
    };

    let config = runtime.config.load();
    let mut new_config: mirage_core::config::ClientConfig = (**config).clone();
    drop(config);

    let chain = match new_config
        .chains
        .iter_mut()
        .find(|c| c.chain_id == req.chain_id)
    {
        Some(c) => c,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("chain '{}' not found", req.chain_id) })),
            )
                .into_response();
        }
    };

    chain.shaping = preset;
    runtime.config.store(Arc::new(new_config));

    let new_version = runtime.config_version.fetch_add(1, Ordering::SeqCst) + 1;

    Json(ReconfigResponse {
        status: "ok".to_owned(),
        config_version: new_version,
        message: format!(
            "shaping preset set to '{}' for chain '{}'",
            req.preset, req.chain_id
        ),
    })
    .into_response()
}

/// `PUT /admin/cover-traffic` — enable or disable cover traffic on the active chain.
pub async fn put_cover_traffic(
    State(runtime): State<Arc<ClientRuntime>>,
    Json(req): Json<SetCoverTrafficRequest>,
) -> Response {
    let active_chain_id = runtime.active_chain_id.load().as_ref().clone();

    let config = runtime.config.load();
    let mut new_config: mirage_core::config::ClientConfig = (**config).clone();
    drop(config);

    let chain = match new_config
        .chains
        .iter_mut()
        .find(|c| c.chain_id == active_chain_id)
    {
        Some(c) => c,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": format!("active chain '{}' not found in config", active_chain_id)
                })),
            )
                .into_response();
        }
    };

    chain.cover_traffic.enabled = req.enabled;
    runtime.config.store(Arc::new(new_config));

    let new_version = runtime.config_version.fetch_add(1, Ordering::SeqCst) + 1;

    let state = if req.enabled { "enabled" } else { "disabled" };
    Json(ReconfigResponse {
        status: "ok".to_owned(),
        config_version: new_version,
        message: format!(
            "cover traffic {state} for chain '{active_chain_id}' \
             (takes effect on next tunnel reconnect)"
        ),
    })
    .into_response()
}

/// `GET /admin/logs` — returns the in-memory log buffer as a JSON array of strings.
pub async fn get_logs(State(runtime): State<Arc<ClientRuntime>>) -> impl IntoResponse {
    let logs: Vec<String> = runtime
        .log_buffer
        .lock()
        .map(|buf| buf.iter().cloned().collect())
        .unwrap_or_default();
    Json(logs)
}

/// `GET /admin/chains` — list all active chains with pool stats and SOCKS5 ports.
pub async fn get_chains(State(runtime): State<Arc<ClientRuntime>>) -> impl IntoResponse {
    let config = runtime.config.load();
    let default_id = &runtime.default_chain_id;

    let mut infos: Vec<ChainPoolInfo> = Vec::with_capacity(config.chains.len());
    for chain in &config.chains {
        let (size, idle, leased, reconnecting) =
            if let Some(pool) = runtime.pool_for_chain(&chain.chain_id) {
                let s = pool.pool_stats().await;
                (s.size, s.idle, s.leased, s.reconnecting)
            } else {
                (0, 0, 0, 0)
            };
        infos.push(ChainPoolInfo {
            chain_id: chain.chain_id.clone(),
            priority: chain.priority,
            socks5_bind: chain.socks5_bind.clone(),
            is_default: &chain.chain_id == default_id,
            pool_size: size,
            pool_idle: idle,
            pool_leased: leased,
            pool_reconnecting: reconnecting,
        });
    }

    // Sort highest priority first.
    infos.sort_by(|a, b| b.priority.cmp(&a.priority));
    Json(infos)
}

/// `GET /admin/chain/{name}/stats` — per-chain pool stats.
pub async fn get_chain_stats(
    State(runtime): State<Arc<ClientRuntime>>,
    Path(name): Path<String>,
) -> Response {
    let config = runtime.config.load();
    let chain = match config.chains.iter().find(|c| c.chain_id == name) {
        Some(c) => c,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("chain '{name}' not found") })),
            )
                .into_response();
        }
    };

    let (size, idle, leased, reconnecting) =
        if let Some(pool) = runtime.pool_for_chain(&name) {
            let s = pool.pool_stats().await;
            (s.size, s.idle, s.leased, s.reconnecting)
        } else {
            (0, 0, 0, 0)
        };

    let info = ChainPoolInfo {
        chain_id: name.clone(),
        priority: chain.priority,
        socks5_bind: chain.socks5_bind.clone(),
        is_default: name == runtime.default_chain_id,
        pool_size: size,
        pool_idle: idle,
        pool_leased: leased,
        pool_reconnecting: reconnecting,
    };

    Json(info).into_response()
}

// ── Type alias for SocketAddr (used in put_mode) ──────────────────────────────

use std::net::SocketAddr;
