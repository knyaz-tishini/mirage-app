//! Loopback HTTP CONNECT proxy over the same edge session as SOCKS5.
//!
//! Only CONNECT is accepted. A destination outside the user's scope is
//! refused locally and never opens a mux stream. The artefact still decides
//! which edge the allowed flow is carried to.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use crate::scope::EgressScope;
use crate::session::EdgeSessionSupervisor;

const HEADER_CAP: usize = 8 * 1024;
const SESSION_WAIT: Duration = Duration::from_secs(5);

pub async fn serve(
    bind: SocketAddr,
    supervisor: Arc<EdgeSessionSupervisor>,
    scope: Arc<EgressScope>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    info!(bind = %bind, "subscriber-client: HTTP CONNECT listener ready");
    loop {
        let (stream, peer) = listener.accept().await?;
        debug!(%peer, "http: accepted connection");
        let supervisor = Arc::clone(&supervisor);
        let scope = Arc::clone(&scope);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, supervisor, scope).await {
                debug!(error = %e, "http: connection ended");
            }
        });
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    supervisor: Arc<EdgeSessionSupervisor>,
    scope: Arc<EgressScope>,
) -> std::io::Result<()> {
    let header = match read_header(&mut stream).await {
        Ok(h) => h,
        Err(e) => {
            debug!(error = %e, "http: header read failed");
            return Ok(());
        }
    };
    let text = String::from_utf8_lossy(&header);
    let Some(request) = text.lines().next() else {
        return Ok(());
    };
    let Some((host, port)) = parse_connect(request) else {
        write_status(&mut stream, 405, "Method Not Allowed").await?;
        return Ok(());
    };
    let target = format!("{host}:{port}");
    let exe = crate::peer::owning_exe(&stream);
    if !scope.allows_flow(&target, exe.as_deref()) {
        debug!(target = %target, "http: destination outside the selected sites or apps");
        write_status(&mut stream, 403, "Forbidden").await?;
        return Ok(());
    }

    let session = match supervisor.wait_for_session(SESSION_WAIT).await {
        Some(session) => session,
        None => {
            warn!(target = %target, "http: no live edge session");
            write_status(&mut stream, 502, "Bad Gateway").await?;
            return Ok(());
        }
    };
    let mut edge = match session.open_tcp_stream(&target).await {
        Ok(edge) => edge,
        Err(e) => {
            warn!(target = %target, error = %e, "http: failed to open a mux stream");
            write_status(&mut stream, 502, "Bad Gateway").await?;
            return Ok(());
        }
    };
    stream
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    let _ = crate::relay::copy_bidirectional_with_flow_idle_timeout(&mut stream, &mut edge).await;
    Ok(())
}

async fn read_header(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        if buf.len() >= HEADER_CAP {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "http header exceeds 8KiB",
            ));
        }
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "http header ended early",
            ));
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            return Ok(buf);
        }
    }
}

/// `CONNECT host:port HTTP/1.x` → host and port. Bracketed IPv6 is accepted.
fn parse_connect(line: &str) -> Option<(String, u16)> {
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    let target = parts.next()?;
    let version = parts.next()?;
    if !method.eq_ignore_ascii_case("CONNECT") {
        return None;
    }
    if !version.starts_with("HTTP/1.") {
        return None;
    }
    let (host, port) = crate::scope::split_host_port(target).ok()?;
    let port = port?;
    if host.is_empty() {
        return None;
    }
    Some((host, port))
}

async fn write_status(stream: &mut TcpStream, code: u16, reason: &str) -> std::io::Result<()> {
    let body = format!("HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    stream.write_all(body.as_bytes()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_connect_host_port() {
        assert_eq!(
            parse_connect("CONNECT api.ipify.org:443 HTTP/1.1"),
            Some(("api.ipify.org".into(), 443))
        );
        assert_eq!(
            parse_connect("CONNECT [2001:db8::1]:443 HTTP/1.1"),
            Some(("2001:db8::1".into(), 443))
        );
        assert!(parse_connect("GET / HTTP/1.1").is_none());
    }
}
