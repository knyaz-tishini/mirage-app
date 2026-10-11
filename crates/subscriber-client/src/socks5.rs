//! A minimal, purpose-built SOCKS5 (RFC 1928) listener over one
//! [`EdgeSessionSupervisor`] — the customer's own local surface.
//!
//! **CONNECT** parses the request's destination (IPv4 / IPv6 / domain,
//! `ATYP` 0x01/0x04/0x03) into the literal `host:port` string
//! [`EdgeSession::open_tcp_stream`] carries as a [`StreamOpen`] target — a
//! domain name is passed through UNRESOLVED, so DNS resolves at the exit,
//! never on this machine.
//!
//! **UDP ASSOCIATE** (§7) is served for real by
//! [`crate::udp_associate::serve_udp_association`] — this module owns only
//! the SOCKS5 wire-level pieces that path reuses: the per-datagram header
//! parser ([`parse_socks5_udp_request`]) and the ASSOCIATE reply writer
//! ([`write_udp_associate_reply`]), both `pub(crate)` for that.
//!
//! Not reused from `mirage_client_core::socks5::Socks5Server` on purpose:
//! that type is built around a `Router`/`ChainSet`/`TunnelPool` — a fixed
//! set of pre-built chains — which this crate no longer has. Every flow
//! here opens a fresh mux stream on the ONE live [`EdgeSession`], dynamically
//! at CONNECT/ASSOCIATE time, never resolved from a pre-built chain table.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use crate::scope::EgressScope;
use crate::session::EdgeSessionSupervisor;

const SOCKS5_VERSION: u8 = 0x05;
const METHOD_NO_AUTH: u8 = 0x00;
const METHOD_NO_ACCEPTABLE: u8 = 0xFF;

const CMD_CONNECT: u8 = 0x01;
const CMD_UDP_ASSOCIATE: u8 = 0x03;

const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;

pub(crate) const REP_SUCCESS: u8 = 0x00;
pub(crate) const REP_GENERAL_FAILURE: u8 = 0x01;
/// RFC 1928: connection not allowed by ruleset. Used when the destination
/// is outside the services the user selected.
pub(crate) const REP_NOT_ALLOWED: u8 = 0x02;
const REP_COMMAND_NOT_SUPPORTED: u8 = 0x07;
const REP_ADDRESS_TYPE_NOT_SUPPORTED: u8 = 0x08;

/// How long a CONNECT waits for a live edge session before giving up and
/// replying general failure.
const CONNECT_SESSION_WAIT: Duration = Duration::from_secs(5);

/// Every way reading a SOCKS5 request off the wire fails.
#[derive(Debug, Error)]
pub(crate) enum Socks5RequestError {
    #[error("socks5: client offered no acceptable auth method")]
    NoAcceptableAuthMethod,
    #[error("socks5: unsupported protocol version 0x{0:02x}")]
    UnsupportedVersion(u8),
    #[error("socks5: unsupported address type 0x{0:02x}")]
    UnsupportedAddressType(u8),
    #[error("socks5: destination address is malformed")]
    MalformedAddress,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Bind `bind` and serve SOCKS5 connections against `supervisor` until the
/// listener errors. Bind happens INSIDE this async fn (not before it is
/// spawned) — matches this crate's established pattern of standing up the
/// listener from within its own driving task.
pub async fn serve(
    bind: SocketAddr,
    supervisor: Arc<EdgeSessionSupervisor>,
    scope: Arc<EgressScope>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    info!(bind = %bind, "subscriber-client: SOCKS5 listener ready");
    loop {
        let (stream, peer) = listener.accept().await?;
        debug!(%peer, "socks5: accepted connection");
        let supervisor = Arc::clone(&supervisor);
        let scope = Arc::clone(&scope);
        tokio::spawn(async move {
            handle_connection(stream, supervisor, scope).await;
        });
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    supervisor: Arc<EdgeSessionSupervisor>,
    scope: Arc<EgressScope>,
) {
    let (cmd, target) = match read_request(&mut stream).await {
        Ok(v) => v,
        Err(Socks5RequestError::UnsupportedAddressType(atyp)) => {
            let _ = write_reply(&mut stream, REP_ADDRESS_TYPE_NOT_SUPPORTED).await;
            debug!(atyp, "socks5: unsupported address type");
            return;
        }
        Err(e) => {
            debug!(error = %e, "socks5: request read failed");
            return;
        }
    };

    match cmd {
        CMD_CONNECT => handle_connect(stream, target, supervisor, scope).await,
        CMD_UDP_ASSOCIATE => {
            if let Err(e) =
                crate::udp_associate::serve_udp_association(stream, supervisor, scope).await
            {
                debug!(error = %e, "socks5: udp associate ended with an io error");
            }
        }
        other => {
            debug!(cmd = other, "socks5: unsupported command");
            let _ = write_reply(&mut stream, REP_COMMAND_NOT_SUPPORTED).await;
        }
    }
}

async fn handle_connect(
    mut stream: TcpStream,
    target: String,
    supervisor: Arc<EdgeSessionSupervisor>,
    scope: Arc<EgressScope>,
) {
    let exe = crate::peer::owning_exe(&stream);
    if !scope.allows_flow(&target, exe.as_deref()) {
        debug!(target = %target, "socks5: destination outside the selected sites or apps");
        let _ = write_reply(&mut stream, REP_NOT_ALLOWED).await;
        return;
    }
    let session = match supervisor.wait_for_session(CONNECT_SESSION_WAIT).await {
        Some(session) => session,
        None => {
            warn!(target = %target, "socks5: no live edge session — refusing CONNECT");
            let _ = write_reply(&mut stream, REP_GENERAL_FAILURE).await;
            return;
        }
    };

    match session.open_tcp_stream(&target).await {
        Ok(mut edge_stream) => {
            if write_reply(&mut stream, REP_SUCCESS).await.is_err() {
                return;
            }
            match tokio::io::copy_bidirectional(&mut stream, &mut edge_stream).await {
                Ok((to_edge, from_edge)) => {
                    debug!(target = %target, to_edge, from_edge, "socks5: CONNECT flow ended");
                }
                Err(e) => {
                    debug!(target = %target, error = %e, "socks5: CONNECT flow ended with an error");
                }
            }
        }
        Err(e) => {
            warn!(target = %target, error = %e, "socks5: failed to open a mux stream to the edge");
            let _ = write_reply(&mut stream, REP_GENERAL_FAILURE).await;
        }
    }
}

/// Read the SOCKS5 greeting + request, replying to the greeting inline (a
/// malformed or unacceptable greeting has no request to read yet). Returns
/// `(cmd, target)` where `target` is the literal `host:port` string a
/// `StreamOpen` carries.
async fn read_request(stream: &mut TcpStream) -> Result<(u8, String), Socks5RequestError> {
    let mut greeting_hdr = [0u8; 2];
    stream.read_exact(&mut greeting_hdr).await?;
    if greeting_hdr[0] != SOCKS5_VERSION {
        return Err(Socks5RequestError::UnsupportedVersion(greeting_hdr[0]));
    }
    let nmethods = greeting_hdr[1] as usize;
    let mut methods = vec![0u8; nmethods];
    stream.read_exact(&mut methods).await?;
    if !methods.contains(&METHOD_NO_AUTH) {
        stream.write_all(&[SOCKS5_VERSION, METHOD_NO_ACCEPTABLE]).await?;
        return Err(Socks5RequestError::NoAcceptableAuthMethod);
    }
    stream.write_all(&[SOCKS5_VERSION, METHOD_NO_AUTH]).await?;

    let mut req_hdr = [0u8; 4];
    stream.read_exact(&mut req_hdr).await?;
    let [ver, cmd, _rsv, atyp] = req_hdr;
    if ver != SOCKS5_VERSION {
        return Err(Socks5RequestError::UnsupportedVersion(ver));
    }

    let addr_bytes: Vec<u8> = match atyp {
        ATYP_IPV4 => {
            let mut b = [0u8; 4];
            stream.read_exact(&mut b).await?;
            b.to_vec()
        }
        ATYP_IPV6 => {
            let mut b = [0u8; 16];
            stream.read_exact(&mut b).await?;
            b.to_vec()
        }
        ATYP_DOMAIN => {
            let mut len_buf = [0u8; 1];
            stream.read_exact(&mut len_buf).await?;
            let mut b = vec![0u8; len_buf[0] as usize];
            stream.read_exact(&mut b).await?;
            b
        }
        other => return Err(Socks5RequestError::UnsupportedAddressType(other)),
    };
    let mut port_buf = [0u8; 2];
    stream.read_exact(&mut port_buf).await?;
    let port = u16::from_be_bytes(port_buf);

    let target = target_from_socks5_address(atyp, &addr_bytes, port)?;
    Ok((cmd, target))
}

/// Parse a SOCKS5 request's already-read `ATYP` + address bytes + port into
/// the literal `host:port` string a `StreamOpen` target carries. A domain
/// name is passed through verbatim — see the module doc.
fn target_from_socks5_address(
    atyp: u8,
    addr_bytes: &[u8],
    port: u16,
) -> Result<String, Socks5RequestError> {
    match atyp {
        ATYP_IPV4 => {
            let octets: [u8; 4] =
                addr_bytes.try_into().map_err(|_| Socks5RequestError::MalformedAddress)?;
            Ok(format!("{}:{port}", Ipv4Addr::from(octets)))
        }
        ATYP_DOMAIN => {
            let domain =
                std::str::from_utf8(addr_bytes).map_err(|_| Socks5RequestError::MalformedAddress)?;
            Ok(format!("{domain}:{port}"))
        }
        ATYP_IPV6 => {
            let octets: [u8; 16] =
                addr_bytes.try_into().map_err(|_| Socks5RequestError::MalformedAddress)?;
            Ok(format!("[{}]:{port}", Ipv6Addr::from(octets)))
        }
        other => Err(Socks5RequestError::UnsupportedAddressType(other)),
    }
}

pub(crate) async fn write_reply(stream: &mut TcpStream, rep: u8) -> std::io::Result<()> {
    // BND.ADDR/BND.PORT are unused by every caller of this listener (it
    // never tells the client to reconnect elsewhere) — 0.0.0.0:0, matching
    // the crate's previous SOCKS5 wiring.
    stream
        .write_all(&[SOCKS5_VERSION, rep, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
        .await
}

/// Reply to a UDP ASSOCIATE request with `rep` and the relay's own bound
/// `BND.ADDR`/`BND.PORT` — unlike CONNECT's [`write_reply`] (always
/// `0.0.0.0:0`, unused by every one of ITS callers), a UDP ASSOCIATE client
/// needs this address for real: RFC 1928 §7 requires every subsequent UDP
/// datagram be sent TO it.
pub(crate) async fn write_udp_associate_reply(
    stream: &mut TcpStream,
    rep: u8,
    bound: SocketAddr,
) -> std::io::Result<()> {
    let mut out = vec![SOCKS5_VERSION, rep, 0x00];
    match bound {
        SocketAddr::V4(v4) => {
            out.push(ATYP_IPV4);
            out.extend_from_slice(&v4.ip().octets());
            out.extend_from_slice(&v4.port().to_be_bytes());
        }
        SocketAddr::V6(v6) => {
            out.push(ATYP_IPV6);
            out.extend_from_slice(&v6.ip().octets());
            out.extend_from_slice(&v6.port().to_be_bytes());
        }
    }
    stream.write_all(&out).await
}

/// One already-received UDP datagram's SOCKS5 §7 header, parsed:
/// `[2B RSV][1B FRAG][1B ATYP][DST.ADDR][DST.PORT][DATA]`. Pure, no I/O —
/// used by `crate::udp_associate` against one `recv_from` buffer at a time.
#[derive(Debug)]
pub(crate) struct Socks5UdpRequest {
    /// `0` for an unfragmented datagram — `crate::udp_associate` refuses
    /// anything else (RFC 1928 leaves reassembly optional; this deployment
    /// implements none of it, see that module's doc).
    pub frag: u8,
    /// `ATYP + DST.ADDR + DST.PORT` exactly as received — echoed back
    /// verbatim in every reply belonging to this destination's flow, so a
    /// client that only understands the ATYP it itself sent (e.g.
    /// `mirage2tun`'s `parse_socks5_udp_response`, IPv4/IPv6 only) always
    /// gets back the same shape.
    pub header_echo: Vec<u8>,
    /// The `host:port` string this destination becomes as a `StreamOpen`
    /// target — see [`target_from_socks5_address`].
    pub target: String,
    pub data: Vec<u8>,
}

/// Every way parsing one UDP datagram's SOCKS5 header fails.
#[derive(Debug, Error)]
pub(crate) enum Socks5UdpHeaderError {
    #[error("socks5 udp: datagram truncated (need at least {need} bytes, have {have})")]
    Truncated { need: usize, have: usize },
    #[error("socks5 udp: malformed destination: {0}")]
    Address(#[from] Socks5RequestError),
}

pub(crate) fn parse_socks5_udp_request(buf: &[u8]) -> Result<Socks5UdpRequest, Socks5UdpHeaderError> {
    if buf.len() < 4 {
        return Err(Socks5UdpHeaderError::Truncated { need: 4, have: buf.len() });
    }
    let frag = buf[2];
    let atyp = buf[3];
    let mut pos = 4usize;
    let addr_bytes: &[u8] = match atyp {
        ATYP_IPV4 => {
            let end = pos + 4;
            if buf.len() < end {
                return Err(Socks5UdpHeaderError::Truncated { need: end, have: buf.len() });
            }
            let bytes = &buf[pos..end];
            pos = end;
            bytes
        }
        ATYP_IPV6 => {
            let end = pos + 16;
            if buf.len() < end {
                return Err(Socks5UdpHeaderError::Truncated { need: end, have: buf.len() });
            }
            let bytes = &buf[pos..end];
            pos = end;
            bytes
        }
        ATYP_DOMAIN => {
            if buf.len() < pos + 1 {
                return Err(Socks5UdpHeaderError::Truncated { need: pos + 1, have: buf.len() });
            }
            let len = buf[pos] as usize;
            pos += 1;
            let end = pos + len;
            if buf.len() < end {
                return Err(Socks5UdpHeaderError::Truncated { need: end, have: buf.len() });
            }
            let bytes = &buf[pos..end];
            pos = end;
            bytes
        }
        other => return Err(Socks5RequestError::UnsupportedAddressType(other).into()),
    };
    if buf.len() < pos + 2 {
        return Err(Socks5UdpHeaderError::Truncated { need: pos + 2, have: buf.len() });
    }
    let port = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
    let header_echo = buf[3..pos + 2].to_vec();
    pos += 2;
    let target = target_from_socks5_address(atyp, addr_bytes, port)?;
    Ok(Socks5UdpRequest { frag, header_echo, target, data: buf[pos..].to_vec() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mirage_control_plane::ExitPreference;

    // ── target_from_socks5_address: pure, no I/O ────────────────────────

    #[test]
    fn ipv4_address_becomes_dotted_host_port() {
        assert_eq!(
            target_from_socks5_address(ATYP_IPV4, &[93, 184, 216, 34], 443).expect("valid"),
            "93.184.216.34:443"
        );
    }

    #[test]
    fn domain_name_passes_through_unresolved() {
        assert_eq!(
            target_from_socks5_address(ATYP_DOMAIN, b"example.com", 443).expect("valid"),
            "example.com:443",
            "a domain must ride through verbatim — DNS resolves at the exit, never here"
        );
    }

    #[test]
    fn ipv6_address_is_bracketed() {
        let loopback = Ipv6Addr::LOCALHOST.octets();
        assert_eq!(
            target_from_socks5_address(ATYP_IPV6, &loopback, 443).expect("valid"),
            "[::1]:443"
        );
    }

    #[test]
    fn unknown_address_type_is_named() {
        let err = target_from_socks5_address(0x7f, &[], 0).expect_err("must refuse");
        assert!(matches!(err, Socks5RequestError::UnsupportedAddressType(0x7f)));
    }

    // ── parse_socks5_udp_request: pure, no I/O ──────────────────────────

    #[test]
    fn parse_udp_request_ipv4() {
        let mut buf = vec![0x00, 0x00, 0x00, ATYP_IPV4];
        buf.extend_from_slice(&[93, 184, 216, 34]);
        buf.extend_from_slice(&443u16.to_be_bytes());
        buf.extend_from_slice(b"payload");
        let req = parse_socks5_udp_request(&buf).expect("valid header");
        assert_eq!(req.frag, 0);
        assert_eq!(req.target, "93.184.216.34:443");
        assert_eq!(req.data, b"payload");
        assert_eq!(req.header_echo, buf[3..10], "ATYP+ADDR+PORT, verbatim");
    }

    #[test]
    fn parse_udp_request_ipv6() {
        let mut buf = vec![0x00, 0x00, 0x00, ATYP_IPV6];
        buf.extend_from_slice(&Ipv6Addr::LOCALHOST.octets());
        buf.extend_from_slice(&53u16.to_be_bytes());
        buf.extend_from_slice(b"dns-query");
        let req = parse_socks5_udp_request(&buf).expect("valid header");
        assert_eq!(req.target, "[::1]:53");
        assert_eq!(req.data, b"dns-query");
    }

    #[test]
    fn parse_udp_request_domain() {
        let mut buf = vec![0x00, 0x00, 0x00, ATYP_DOMAIN, 11u8];
        buf.extend_from_slice(b"example.com");
        buf.extend_from_slice(&80u16.to_be_bytes());
        buf.extend_from_slice(b"data");
        let req = parse_socks5_udp_request(&buf).expect("valid header");
        assert_eq!(req.target, "example.com:80");
        assert_eq!(req.data, b"data");
    }

    #[test]
    fn parse_udp_request_preserves_frag_for_the_caller_to_refuse() {
        let mut buf = vec![0x00, 0x00, 0x07, ATYP_IPV4]; // FRAG = 7, not 0.
        buf.extend_from_slice(&[1, 2, 3, 4]);
        buf.extend_from_slice(&1u16.to_be_bytes());
        let req = parse_socks5_udp_request(&buf).expect("header still parses — refusal is the caller's job");
        assert_eq!(req.frag, 7);
    }

    #[test]
    fn parse_udp_request_truncated_is_named() {
        let err = parse_socks5_udp_request(&[0x00, 0x00]).expect_err("too short for even a header");
        assert!(matches!(err, Socks5UdpHeaderError::Truncated { need: 4, have: 2 }));
    }

    #[test]
    fn parse_udp_request_unsupported_address_type_is_named() {
        let buf = [0x00, 0x00, 0x00, 0x7f];
        let err = parse_socks5_udp_request(&buf).expect_err("must refuse");
        assert!(matches!(
            err,
            Socks5UdpHeaderError::Address(Socks5RequestError::UnsupportedAddressType(0x7f))
        ));
    }

    async fn free_local_addr() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral");
        listener.local_addr().expect("local addr")
    }

    #[tokio::test]
    async fn connect_with_no_live_session_replies_general_failure() {
        let bind = free_local_addr().await;
        let listener = TcpListener::bind(bind).await.expect("bind");
        // No entrances at all -> the supervisor never has a live session.
        let supervisor =
            EdgeSessionSupervisor::spawn(Vec::new(), b"MESH2".to_vec(), [0x11u8; 32], ExitPreference::Auto);
        tokio::spawn(async move {
            let (stream, _peer) = listener.accept().await.expect("accept");
            handle_connection(stream, supervisor, std::sync::Arc::new(EgressScope::all())).await;
        });

        let mut client = TcpStream::connect(bind).await.expect("connect");
        client.write_all(&[0x05, 0x01, 0x00]).await.expect("write greeting");
        let mut greeting_reply = [0u8; 2];
        client.read_exact(&mut greeting_reply).await.expect("read greeting reply");
        assert_eq!(greeting_reply, [0x05, 0x00]);

        // CMD=CONNECT(0x01), ATYP=domain, "example.com:443".
        let mut req = vec![0x05, 0x01, 0x00, 0x03, 11u8];
        req.extend_from_slice(b"example.com");
        req.extend_from_slice(&443u16.to_be_bytes());
        client.write_all(&req).await.expect("write request");

        let mut reply = [0u8; 10];
        client.read_exact(&mut reply).await.expect("read reply");
        assert_eq!(reply[0], SOCKS5_VERSION);
        assert_eq!(reply[1], REP_GENERAL_FAILURE);
    }
}
