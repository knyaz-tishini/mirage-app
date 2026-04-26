//! mirage-chain-worker — single-chain SOCKS5 worker process.
//!
//! Receives IPC commands from supervisor over stdin/stdout (JSON-line framing).
//! Binds a SOCKS5 listener with SO_REUSEPORT (or SO_REUSEADDR on Windows).
//! Spawns a stub connection handler per accepted connection.

mod chain_stub;

use chain_stub::ChainStub;
use mirage_chain_ipc::{SupervisorMsg, WorkerMsg, WorkerState, read_msg, write_msg};
use socket2::{Domain, Protocol, Socket, Type};
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
};
use tokio::{
    io::{BufReader, BufWriter},
    net::TcpListener,
    sync::mpsc,
};
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

    info!(config_path = %config_path, "using config path (stub — not loaded)");

    let addr: SocketAddr = socks5_bind.parse().map_err(|e| {
        anyhow::anyhow!("invalid socks5_bind address '{}': {}", socks5_bind, e)
    })?;

    let listener = bind_reuseport(addr)?;
    let listener = TcpListener::from_std(listener)?;

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

    let active = Arc::new(AtomicU32::new(0));
    let total = Arc::new(AtomicU64::new(0));
    let chain = Arc::new(ChainStub::new(chain_id.clone()));

    // Shutdown signal for accept loop.
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

    let accept_active = active.clone();
    let accept_total = total.clone();
    let accept_chain = chain.clone();
    let accept_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    break;
                }
                result = listener.accept() => {
                    match result {
                        Ok((stream, _)) => {
                            accept_active.fetch_add(1, Ordering::Relaxed);
                            accept_total.fetch_add(1, Ordering::Relaxed);
                            let c = accept_chain.clone();
                            let active_ref = accept_active.clone();
                            tokio::spawn(async move {
                                c.handle_connection(stream).await;
                                active_ref.fetch_sub(1, Ordering::Relaxed);
                            });
                        }
                        Err(e) => {
                            info!("accept error (likely closed): {e}");
                            break;
                        }
                    }
                }
            }
        }
    });

    // IPC command loop — runs until Drain or Shutdown.
    let mut drain_timeout_secs: u64 = 30;

    loop {
        match read_msg::<_, SupervisorMsg>(&mut reader).await? {
            None => {
                warn!("stdin closed — shutting down");
                let _ = shutdown_tx.send(()).await;
                break;
            }
            Some(SupervisorMsg::Drain { timeout_secs }) => {
                info!(timeout_secs, "received Drain");
                drain_timeout_secs = timeout_secs;
                let _ = shutdown_tx.send(()).await;
                break;
            }
            Some(SupervisorMsg::Shutdown) => {
                info!("received Shutdown — exiting immediately");
                let _ = shutdown_tx.send(()).await;
                accept_handle.await.ok();
                return Ok(());
            }
            Some(SupervisorMsg::Ping) => {
                let snap_active = active.load(Ordering::Relaxed);
                let snap_total = total.load(Ordering::Relaxed);
                write_msg(
                    &mut writer,
                    &WorkerMsg::Status {
                        chain_id: chain_id.clone(),
                        active_connections: snap_active,
                        total_connections: snap_total,
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

    accept_handle.await.ok();

    // Drain phase: wait for active connections to reach zero or timeout.
    let drain_deadline = tokio::time::Instant::now()
        + tokio::time::Duration::from_secs(drain_timeout_secs);

    loop {
        let snap = active.load(Ordering::Relaxed);
        write_msg(
            &mut writer,
            &WorkerMsg::Draining {
                chain_id: chain_id.clone(),
                active_connections: snap,
            },
        )
        .await?;

        if snap == 0 {
            break;
        }
        if tokio::time::Instant::now() >= drain_deadline {
            warn!(active = snap, "drain timeout reached — forcing exit");
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

/// Bind a TCP listener with SO_REUSEPORT (Linux) or SO_REUSEADDR (Windows).
fn bind_reuseport(addr: SocketAddr) -> std::io::Result<std::net::TcpListener> {
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let sock = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    #[cfg(unix)]
    sock.set_reuse_port(true)?;
    sock.set_reuse_address(true)?;
    sock.bind(&addr.into())?;
    sock.listen(1024)?;
    Ok(sock.into())
}
