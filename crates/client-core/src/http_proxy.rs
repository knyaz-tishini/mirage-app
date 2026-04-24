//! HTTP CONNECT proxy server.
//!
//! Listens on a TCP port and handles `CONNECT host:port HTTP/1.1` requests by
//! leasing a pre-warmed tunnel from the [`TunnelPool`] and relaying data
//! bidirectionally.  Plain (non-CONNECT) HTTP requests receive a `405 Method
//! Not Allowed` response — CONNECT is the intended use case for HTTPS tunnelling.
//!
//! The implementation shares [`TunnelPool`] with the SOCKS5 server and reuses
//! the same [`relay_loop`] for data forwarding.

use std::{net::SocketAddr, sync::Arc};

use mirage_core::{error::MirageError, routing::RouteDecision, routing::RoutingTable};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tracing::{debug, error};

use mirage_protocol::traits::Tunnel as _;

use crate::{mux_pool::MuxPool, pool::TunnelPool, relay::{relay_loop, TunnelCompat}};

// Maximum number of bytes to read while consuming HTTP request headers.
const MAX_HEADER_BYTES: usize = 8192;

/// A minimal HTTP CONNECT proxy server.
pub struct HttpProxyServer {
    bind_addr: SocketAddr,
}

impl HttpProxyServer {
    /// Create a new server that will listen on `bind_addr`.
    pub fn new(bind_addr: SocketAddr) -> Self {
        Self { bind_addr }
    }

    /// Accept connections forever, spawning a handler task per connection.
    pub async fn run(
        &self,
        pool: Arc<TunnelPool>,
        mux_pool: Option<Arc<MuxPool>>,
        router: Arc<RoutingTable>,
    ) -> Result<(), MirageError> {
        let listener = TcpListener::bind(self.bind_addr).await?;
        tracing::info!(addr = %self.bind_addr, "HTTP CONNECT proxy listening");

        loop {
            let (stream, peer) = listener.accept().await?;
            debug!(%peer, "new HTTP proxy connection");

            let pool = Arc::clone(&pool);
            let mux_pool = mux_pool.clone();
            let router = Arc::clone(&router);

            tokio::spawn(async move {
                if let Err(e) = handle_http_connection(stream, pool, mux_pool, router).await {
                    error!(%peer, "HTTP proxy connection error: {e}");
                }
            });
        }
    }
}

/// Parsed result of the HTTP request line.
enum ParsedRequest {
    /// `CONNECT host:port HTTP/1.x`
    Connect { host: String, port: u16 },
    /// Any other method — will be rejected with 405.
    Other,
}

/// Handle one HTTP proxy connection.
async fn handle_http_connection(
    mut stream: TcpStream,
    pool: Arc<TunnelPool>,
    mux_pool: Option<Arc<MuxPool>>,
    router: Arc<RoutingTable>,
) -> Result<(), MirageError> {
    // Read until we see the end of the HTTP header block (\r\n\r\n).
    let mut buf = Vec::with_capacity(512);
    loop {
        let mut byte = [0u8; 1];
        match stream.read_exact(&mut byte).await {
            Ok(_) => {}
            Err(e) => return Err(MirageError::Io(e)),
        }
        buf.push(byte[0]);

        if buf.len() > MAX_HEADER_BYTES {
            write_error_response(&mut stream, 413, "Request Too Large").await;
            return Err(MirageError::Protocol("HTTP request headers exceeded 8 KiB".into()));
        }

        // Look for the \r\n\r\n header terminator.
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
    }

    // Parse the first line only — that is enough to handle CONNECT.
    let request = parse_request_line(&buf)?;

    match request {
        ParsedRequest::Connect { host, port } => {
            handle_connect(stream, pool, mux_pool, router, host, port).await
        }
        ParsedRequest::Other => {
            // Plain HTTP forwarding is not implemented.
            write_error_response(&mut stream, 405, "Method Not Allowed").await;
            Ok(())
        }
    }
}

/// Handle a `CONNECT host:port` request.
async fn handle_connect(
    mut stream: TcpStream,
    pool: Arc<TunnelPool>,
    mux_pool: Option<Arc<MuxPool>>,
    router: Arc<RoutingTable>,
    host: String,
    port: u16,
) -> Result<(), MirageError> {
    let target_addr = format!("{host}:{port}");
    let decision = router.decide_domain(&host);
    let decision_label = match &decision {
        RouteDecision::Direct => "direct",
        RouteDecision::Chain => "chain",
    };
    debug!(target = %target_addr, decision = decision_label, "HTTP CONNECT routing decision");

    match decision {
        RouteDecision::Direct => {
            let mut target = TcpStream::connect(&target_addr).await.map_err(|e| {
                MirageError::NodeUnreachable(format!(
                    "direct connect to {target_addr} failed: {e}"
                ))
            })?;
            write_connect_ok(&mut stream).await?;
            tokio::io::copy_bidirectional(&mut stream, &mut target)
                .await
                .map_err(MirageError::Io)?;
        }
        RouteDecision::Chain => {
            if let Some(mp) = mux_pool {
                route_connect_via_mux_pool(&mut stream, mp, &target_addr).await?;
            } else {
                let mut lease = pool.lease().await?;

                let send_result = if lease.is_onion {
                    let target_bytes = target_addr.as_bytes();
                    let len_prefix = (target_bytes.len() as u16).to_be_bytes();
                    let mut framed = Vec::with_capacity(2 + target_bytes.len());
                    framed.extend_from_slice(&len_prefix);
                    framed.extend_from_slice(target_bytes);
                    lease.send(&framed).await
                } else {
                    lease.send(target_addr.as_bytes()).await
                };

                if let Err(e) = send_result {
                    lease.poison();
                    write_error_response(&mut stream, 502, "Bad Gateway").await;
                    return Err(e);
                }

                write_connect_ok(&mut stream).await?;

                lease.set_relay_active(true);
                let tunnel = lease.take_tunnel().ok_or(MirageError::TunnelClosed)?;
                let mut compat = TunnelCompat::wrap(tunnel);
                let result = relay_loop(&mut stream, &mut compat).await;
                lease.poison();
                result?;
            }
        }
    }

    Ok(())
}

/// Open a mux stream, send the target address, then relay.
async fn route_connect_via_mux_pool(
    client: &mut TcpStream,
    mux_pool: Arc<MuxPool>,
    target_addr: &str,
) -> Result<(), MirageError> {
    let is_onion = mux_pool.is_onion.load(std::sync::atomic::Ordering::Acquire);
    let mut stream = mux_pool.open_stream().await?;

    if is_onion {
        let target_bytes = target_addr.as_bytes();
        let len_prefix = (target_bytes.len() as u16).to_be_bytes();
        let mut framed = Vec::with_capacity(2 + target_bytes.len());
        framed.extend_from_slice(&len_prefix);
        framed.extend_from_slice(target_bytes);
        tokio::io::AsyncWriteExt::write_all(&mut stream, &framed).await
            .map_err(MirageError::Io)?;
    } else {
        tokio::io::AsyncWriteExt::write_all(&mut stream, target_addr.as_bytes()).await
            .map_err(MirageError::Io)?;
    }

    write_connect_ok(client).await?;

    relay_loop(client, &mut stream).await
}

/// Parse the HTTP request line from a raw header buffer.
///
/// Only the first line is examined.  Everything else is ignored.
fn parse_request_line(buf: &[u8]) -> Result<ParsedRequest, MirageError> {
    // Extract the first line (up to the first \r\n or \n).
    let line_end = buf
        .windows(2)
        .position(|w| w == b"\r\n")
        .or_else(|| buf.iter().position(|&b| b == b'\n'))
        .unwrap_or(buf.len());

    let first_line = std::str::from_utf8(&buf[..line_end]).map_err(|_| {
        MirageError::Protocol("HTTP request line is not valid UTF-8".into())
    })?;

    let mut parts = first_line.splitn(3, ' ');
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");

    if !method.eq_ignore_ascii_case("CONNECT") {
        return Ok(ParsedRequest::Other);
    }

    // CONNECT target is "host:port".
    let (host, port_str) = target.rsplit_once(':').ok_or_else(|| {
        MirageError::Protocol(format!("CONNECT target missing port: {target}"))
    })?;

    let port: u16 = port_str.parse().map_err(|_| {
        MirageError::Protocol(format!("CONNECT port is not a valid u16: {port_str}"))
    })?;

    if host.is_empty() {
        return Err(MirageError::Protocol("CONNECT target host is empty".into()));
    }

    Ok(ParsedRequest::Connect {
        host: host.to_owned(),
        port,
    })
}

/// Write `HTTP/1.1 200 Connection established\r\n\r\n` to the stream.
async fn write_connect_ok(stream: &mut TcpStream) -> Result<(), MirageError> {
    stream
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await
        .map_err(MirageError::Io)
}

/// Write an HTTP error response (best-effort — errors are silently dropped).
async fn write_error_response(stream: &mut TcpStream, status: u16, reason: &str) {
    let response = format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\n\r\n");
    let _ = stream.write_all(response.as_bytes()).await;
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &[u8]) -> Result<ParsedRequest, MirageError> {
        parse_request_line(raw)
    }

    #[test]
    fn test_parse_connect_request() {
        let raw = b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let req = parse(raw).expect("should parse");
        match req {
            ParsedRequest::Connect { host, port } => {
                assert_eq!(host, "example.com");
                assert_eq!(port, 443);
            }
            ParsedRequest::Other => panic!("expected Connect"),
        }
    }

    #[test]
    fn test_parse_connect_ipv4() {
        let raw = b"CONNECT 1.2.3.4:80 HTTP/1.1\r\n\r\n";
        let req = parse(raw).expect("should parse");
        match req {
            ParsedRequest::Connect { host, port } => {
                assert_eq!(host, "1.2.3.4");
                assert_eq!(port, 80);
            }
            ParsedRequest::Other => panic!("expected Connect"),
        }
    }

    #[test]
    fn test_parse_plain_get_returns_other() {
        let raw = b"GET http://example.com/path HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let req = parse(raw).expect("should parse");
        assert!(matches!(req, ParsedRequest::Other));
    }

    #[test]
    fn test_parse_post_returns_other() {
        let raw = b"POST http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let req = parse(raw).expect("should parse");
        assert!(matches!(req, ParsedRequest::Other));
    }

    #[test]
    fn test_parse_connect_missing_port() {
        let raw = b"CONNECT example.com HTTP/1.1\r\n\r\n";
        assert!(parse(raw).is_err());
    }

    #[test]
    fn test_parse_connect_invalid_port() {
        let raw = b"CONNECT example.com:notaport HTTP/1.1\r\n\r\n";
        assert!(parse(raw).is_err());
    }

    #[test]
    fn test_parse_empty_host() {
        let raw = b"CONNECT :443 HTTP/1.1\r\n\r\n";
        assert!(parse(raw).is_err());
    }

    #[test]
    fn test_parse_connect_with_ipv6() {
        // IPv6 address in CONNECT uses [::1]:443 form — rsplit_once(':') on
        // "[::1]:443" splits at the last colon, giving host="[::1]" port="443".
        let raw = b"CONNECT [::1]:8080 HTTP/1.1\r\n\r\n";
        let req = parse(raw).expect("should parse");
        match req {
            ParsedRequest::Connect { host, port } => {
                assert_eq!(host, "[::1]");
                assert_eq!(port, 8080);
            }
            ParsedRequest::Other => panic!("expected Connect"),
        }
    }
}
