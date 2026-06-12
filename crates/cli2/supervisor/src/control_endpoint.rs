//! Supervisor control HTTP listener (optional).
//!
//! When `control_http_bind` is set in `supervisor.toml`, the supervisor binds
//! a minimal HTTP server on that address.  The only route is:
//!
//! ```text
//! POST /chains/reload
//! ```
//!
//! Posting to this endpoint signals an immediate config reconcile — same code
//! path as the mtime poll loop.  This is useful when the sentinel has just
//! materialised a new chains config and wants to trigger adoption without
//! waiting up to `reload_poll_secs` seconds.
//!
//! The listener is purely local (127.0.0.1 by convention) and carries no auth.
//! It is NOT started unless `control_http_bind` is explicitly configured.

use std::net::SocketAddr;

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tracing::{debug, error, info, warn};

const HTTP_200: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Type: text/plain\r\n\r\nOK";
const HTTP_404: &[u8] = b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nContent-Type: text/plain\r\n\r\nNot Found";
const HTTP_405: &[u8] = b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 18\r\nContent-Type: text/plain\r\n\r\nMethod Not Allowed";

/// Spawn the control HTTP listener.
///
/// Sends `()` on `reload_tx` every time `POST /chains/reload` is received.
/// The supervisor main loop reacts by triggering an immediate config reconcile.
pub fn spawn_control_endpoint(bind: SocketAddr, reload_tx: mpsc::Sender<()>) {
    tokio::spawn(async move {
        match run_control_listener(bind, reload_tx).await {
            Ok(()) => {}
            Err(e) => error!("control endpoint exited with error: {e}"),
        }
    });
}

async fn run_control_listener(bind: SocketAddr, reload_tx: mpsc::Sender<()>) -> std::io::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    info!(addr = %bind, "supervisor control endpoint started");

    loop {
        let (stream, peer) = listener.accept().await?;
        debug!(%peer, "control: new connection");
        let tx = reload_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_control_conn(stream, tx).await {
                warn!(%peer, "control connection error: {e}");
            }
        });
    }
}

async fn handle_control_conn(mut stream: TcpStream, reload_tx: mpsc::Sender<()>) -> std::io::Result<()> {
    // Read the first line of the HTTP request (method + path + version).
    // We don't need the full request body — just method and path.
    let mut buf = [0u8; 512];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }

    let header = std::str::from_utf8(&buf[..n]).unwrap_or("");
    let first_line = header.lines().next().unwrap_or("");
    let mut parts = first_line.splitn(3, ' ');
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    match (method, path) {
        ("POST", "/chains/reload") => {
            info!("control: POST /chains/reload — triggering immediate reconcile");
            // Best-effort: if the receiver is gone the supervisor is shutting down.
            let _ = reload_tx.try_send(());
            stream.write_all(HTTP_200).await?;
        }
        ("POST", _) => {
            stream.write_all(HTTP_404).await?;
        }
        _ => {
            stream.write_all(HTTP_405).await?;
        }
    }

    stream.flush().await
}
