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
use std::sync::{Arc, Mutex};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tracing::{debug, error, info, warn};

const HTTP_200: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Type: text/plain\r\n\r\nOK";
const HTTP_404: &[u8] = b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nContent-Type: text/plain\r\n\r\nNot Found";
const HTTP_405: &[u8] = b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 18\r\nContent-Type: text/plain\r\n\r\nMethod Not Allowed";

/// One worker's live status, snapshotted by the supervisor main loop.
#[derive(Debug, Clone)]
pub struct WorkerStatusSnapshot {
    pub chain_id: String,
    pub pid: Option<u32>,
    /// `HandleState` rendered via `Debug` (e.g. `"Running"`, `"Exited"`).
    pub state: String,
}

/// Shared status the main loop writes and the control endpoint reads.
pub type SharedStatus = Arc<Mutex<Vec<WorkerStatusSnapshot>>>;

/// Hand-build a JSON array of the worker snapshots (no serde dep needed; the
/// fields are simple slugs). `Debug` formatting of `&str`/`String` yields a
/// quoted, escaped JSON string for ASCII content.
fn status_json(snap: &[WorkerStatusSnapshot]) -> String {
    let items: Vec<String> = snap
        .iter()
        .map(|w| {
            let pid = w.pid.map(|p| p.to_string()).unwrap_or_else(|| "null".into());
            format!(
                "{{\"chain_id\":{:?},\"pid\":{},\"state\":{:?}}}",
                w.chain_id, pid, w.state
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// Write a `200 OK` JSON response with the given body.
async fn write_json_200(stream: &mut TcpStream, body: &str) -> std::io::Result<()> {
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(resp.as_bytes()).await
}

/// Spawn the control HTTP listener.
///
/// Sends `()` on `reload_tx` every time `POST /chains/reload` is received.
/// The supervisor main loop reacts by triggering an immediate config reconcile.
pub fn spawn_control_endpoint(
    bind: SocketAddr,
    reload_tx: mpsc::Sender<()>,
    status: SharedStatus,
) {
    tokio::spawn(async move {
        match run_control_listener(bind, reload_tx, status).await {
            Ok(()) => {}
            Err(e) => error!("control endpoint exited with error: {e}"),
        }
    });
}

async fn run_control_listener(
    bind: SocketAddr,
    reload_tx: mpsc::Sender<()>,
    status: SharedStatus,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    info!(addr = %bind, "supervisor control endpoint started");

    loop {
        let (stream, peer) = listener.accept().await?;
        debug!(%peer, "control: new connection");
        let tx = reload_tx.clone();
        let status = Arc::clone(&status);
        tokio::spawn(async move {
            if let Err(e) = handle_control_conn(stream, tx, status).await {
                warn!(%peer, "control connection error: {e}");
            }
        });
    }
}

async fn handle_control_conn(
    mut stream: TcpStream,
    reload_tx: mpsc::Sender<()>,
    status: SharedStatus,
) -> std::io::Result<()> {
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
        ("GET", "/admin/health") => {
            // Liveness + worker count (parity with cli1's `/admin/health`).
            let n = status.lock().map(|s| s.len()).unwrap_or(0);
            let body = format!("{{\"status\":\"ok\",\"workers\":{n}}}");
            write_json_200(&mut stream, &body).await?;
        }
        ("GET", "/status") => {
            // Per-worker live state snapshot (chain_id, pid, state).
            let body = {
                let guard = status.lock().unwrap_or_else(|p| p.into_inner());
                status_json(&guard)
            };
            write_json_200(&mut stream, &body).await?;
        }
        ("GET", _) => {
            stream.write_all(HTTP_404).await?;
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
