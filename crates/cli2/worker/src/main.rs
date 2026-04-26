//! mirage-chain-worker — single-chain SOCKS5 worker process.
//!
//! Receives IPC commands from supervisor over stdin/stdout (JSON-line framing).
//! Loads a ChainConfig from the config file, builds a TunnelPool, and runs
//! a Socks5Server for the assigned chain.

use std::{net::SocketAddr, sync::Arc};

use anyhow::Context;
use mirage_chain_ipc::{SupervisorMsg, WorkerMsg, WorkerState, read_msg, write_msg};
use mirage_client_core::{
    endpoint_protocol::EndpointProtocol,
    pool::TunnelPool,
    socks5::Socks5Server,
};
use mirage_core::config::{ClientConfig, TcpProtocolId};
use mirage_protocol::{
    aliases::{mirage_base, mirage_tls},
    negotiation::ProtocolChain,
    traits::TcpProtocol,
};
use tokio::io::{BufReader, BufWriter};
use tracing::{info, warn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_env("MIRAGE_LOG").add_directive(
                "mirage_chain_worker=debug".parse().expect("valid directive"),
            ),
        )
        .with_writer(std::io::stderr)
        .init();

    let chain_id =
        std::env::var("MIRAGE_CHAIN_ID").unwrap_or_else(|_| "unknown".to_string());

    info!(chain_id = %chain_id, "worker starting");

    // IPC over stdin/stdout.
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut writer = BufWriter::new(stdout);

    // Wait for Start message.
    let (socks5_bind, config_path) = loop {
        match read_msg::<_, SupervisorMsg>(&mut reader).await? {
            None => {
                info!("stdin closed before Start — exiting");
                return Ok(());
            }
            Some(SupervisorMsg::Start {
                chain_id: cid,
                socks5_bind,
                config_path,
            }) => {
                info!(
                    chain_id = %cid,
                    bind = %socks5_bind,
                    "received Start"
                );
                break (socks5_bind, config_path);
            }
            Some(SupervisorMsg::Shutdown) => {
                info!("Shutdown before Start — exiting");
                return Ok(());
            }
            Some(other) => {
                warn!(?other, "unexpected message before Start — ignoring");
            }
        }
    };

    let addr: SocketAddr = socks5_bind.parse().with_context(|| {
        format!("invalid socks5_bind address '{socks5_bind}'")
    })?;

    // Load and parse config.
    let raw = std::fs::read_to_string(&config_path)
        .with_context(|| format!("read config {config_path}"))?;
    let client_config: ClientConfig = serde_json::from_str(&raw)
        .with_context(|| format!("parse config {config_path}"))?;

    // Find the chain this worker is responsible for.
    let chain = client_config
        .chains
        .iter()
        .find(|c| c.chain_id == chain_id)
        .ok_or_else(|| anyhow::anyhow!("chain '{}' not found in config", chain_id))?
        .clone();

    // Build protocol chain (mirrors cli/src/main.rs).
    let mut protocols: Vec<Box<dyn TcpProtocol>> = Vec::new();
    protocols.push(Box::new(EndpointProtocol::new(
        mirage_base(),
        TcpProtocolId::Mirage,
    )));
    if client_config.psk.is_some() {
        protocols.push(Box::new(EndpointProtocol::new(
            mirage_tls("cloudflare.com", vec!["h2".into(), "http/1.1".into()]),
            TcpProtocolId::MirageShadowTls,
        )));
    }
    let chain_protocols = Arc::new(ProtocolChain::new(protocols));

    // Build tunnel pool.
    let pool_cfg = client_config.pool.clone();
    let pool = TunnelPool::new(chain, chain_protocols, pool_cfg)
        .await
        .map_err(|e| anyhow::anyhow!("TunnelPool::new failed: {e}"))?;
    // TunnelPool::new already returns Arc<TunnelPool>.

    // Announce readiness.
    let pid = std::process::id();
    write_msg(
        &mut writer,
        &WorkerMsg::Ready {
            chain_id: chain_id.clone(),
            pid,
        },
    )
    .await?;
    info!(chain_id = %chain_id, pid, "sent Ready");

    // Spawn SOCKS5 server.
    let server = Socks5Server::new(addr);
    let server_handle = tokio::spawn({
        let pool = Arc::clone(&pool);
        async move {
            if let Err(e) = server.run_simple(pool).await {
                tracing::error!(error = %e, "SOCKS5 server exited with error");
            }
        }
    });

    // IPC command loop — runs until Drain or Shutdown.
    let mut drain_timeout_secs: u64 = 30;

    loop {
        match read_msg::<_, SupervisorMsg>(&mut reader).await? {
            None => {
                warn!("stdin closed — shutting down");
                server_handle.abort();
                break;
            }
            Some(SupervisorMsg::Drain { timeout_secs }) => {
                info!(timeout_secs, "received Drain");
                drain_timeout_secs = timeout_secs;
                server_handle.abort();
                break;
            }
            Some(SupervisorMsg::Shutdown) => {
                info!("received Shutdown — exiting immediately");
                server_handle.abort();
                return Ok(());
            }
            Some(SupervisorMsg::Ping) => {
                let stats = pool.pool_stats().await;
                let active_connections = stats.leased as u32;
                write_msg(
                    &mut writer,
                    &WorkerMsg::Status {
                        chain_id: chain_id.clone(),
                        active_connections,
                        total_connections: 0,
                        state: WorkerState::Ready,
                    },
                )
                .await?;
            }
            Some(SupervisorMsg::Start { .. }) => {
                warn!("received duplicate Start — ignoring");
            }
        }
    }

    // Drain phase: wait for leased tunnels to reach zero or timeout.
    let drain_deadline = tokio::time::Instant::now()
        + tokio::time::Duration::from_secs(drain_timeout_secs);

    loop {
        let stats = pool.pool_stats().await;
        let active = stats.leased as u32;
        write_msg(
            &mut writer,
            &WorkerMsg::Draining {
                chain_id: chain_id.clone(),
                active_connections: active,
            },
        )
        .await?;

        if active == 0 {
            break;
        }
        if tokio::time::Instant::now() >= drain_deadline {
            warn!(active, "drain timeout reached — forcing exit");
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    write_msg(
        &mut writer,
        &WorkerMsg::Drained {
            chain_id: chain_id.clone(),
        },
    )
    .await?;
    info!(chain_id = %chain_id, "drained — exiting");
    Ok(())
}
