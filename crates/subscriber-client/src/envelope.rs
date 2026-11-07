//! One terminator envelope in front of a dial.
//!
//! `addr` is where the protocol ends. When `relay` is set, this module
//! gives the protocol a loopback socket and sends the terminator to the
//! relay once: `u8 host_len (1..=255) | host | u16 big-endian port`.
//! The same bytes as `mirage_protocol::preamble::write_preamble`. Not a
//! per-packet header. ICMP is not proxied — the door writes that envelope
//! on the echo itself.

use std::net::SocketAddr;
use std::time::Duration;

use mirage_core::mirage::wire::Protocol;
use tokio::io::{copy_bidirectional, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

use crate::artefact::ArtefactEntrance;

const TCP_ACCEPT_WAIT: Duration = Duration::from_secs(30);
const UDP_IDLE: Duration = Duration::from_secs(120);

/// The terminator envelope for `addr` (`host:port`, brackets stripped).
pub(crate) fn target_envelope(addr: &str) -> Result<Vec<u8>, String> {
    let (host, port) = host_port(addr)?;
    let bytes = host.as_bytes();
    if bytes.is_empty() || bytes.len() > 255 {
        return Err("dial address is not host:port".to_owned());
    }
    let mut out = Vec::with_capacity(1 + bytes.len() + 2);
    out.push(bytes.len() as u8);
    out.extend_from_slice(bytes);
    out.extend_from_slice(&port.to_be_bytes());
    Ok(out)
}

/// Point the dial at the relay, or return `entrance` unchanged.
///
/// No relay is today's dial. ICMP stays unchanged (`relay` still set) so
/// the door can send the envelope as the first echo. A TCP entrance gets
/// one loopback stream; a UDP entrance gets one loopback datagram socket.
/// The returned entrance has `relay: None`, so a second call is a no-op.
pub(crate) async fn front(entrance: &ArtefactEntrance) -> Result<ArtefactEntrance, String> {
    let Some(relay) = entrance.relay.as_deref() else {
        return Ok(entrance.clone());
    };
    if matches!(&entrance.protocol, Protocol::Icmp { .. }) {
        return Ok(entrance.clone());
    }
    if entrance.protocol.is_udp() {
        front_udp(entrance, relay).await
    } else {
        front_tcp(entrance, relay).await
    }
}

async fn front_tcp(entrance: &ArtefactEntrance, relay: &str) -> Result<ArtefactEntrance, String> {
    let envelope = target_envelope(&entrance.addr)?;
    let relay_addr = resolve(relay).await?;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|_| "relay socket failed".to_owned())?;
    let local = listener.local_addr().map_err(|_| "relay socket failed".to_owned())?;
    // Detached on purpose: the copy outlives this function and ends on EOF.
    // Nobody connecting drops the listener after 30s.
    drop(tokio::spawn(async move {
        serve_tcp(listener, relay_addr, envelope).await;
    }));
    Ok(at_local(entrance, local))
}

async fn serve_tcp(listener: TcpListener, relay_addr: SocketAddr, envelope: Vec<u8>) {
    let accepted = tokio::time::timeout(TCP_ACCEPT_WAIT, listener.accept()).await;
    let mut inbound = match accepted {
        Ok(Ok((stream, _))) => stream,
        _ => return,
    };
    drop(listener);
    let mut outbound = match TcpStream::connect(relay_addr).await {
        Ok(stream) => stream,
        Err(_) => return,
    };
    if outbound.write_all(&envelope).await.is_err() || outbound.flush().await.is_err() {
        return;
    }
    let _ = copy_bidirectional(&mut inbound, &mut outbound).await;
}

async fn front_udp(entrance: &ArtefactEntrance, relay: &str) -> Result<ArtefactEntrance, String> {
    let envelope = target_envelope(&entrance.addr)?;
    let relay_addr = resolve(relay).await?;
    let local = UdpSocket::bind("127.0.0.1:0")
        .await
        .map_err(|_| "relay socket failed".to_owned())?;
    let outbound = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|_| "relay socket failed".to_owned())?;
    let local_addr = local.local_addr().map_err(|_| "relay socket failed".to_owned())?;
    outbound
        .send_to(&envelope, relay_addr)
        .await
        .map_err(|_| "relay envelope failed".to_owned())?;
    drop(tokio::spawn(async move {
        bridge_udp(local, outbound, relay_addr).await;
    }));
    Ok(at_local(entrance, local_addr))
}

async fn bridge_udp(local: UdpSocket, relay: UdpSocket, relay_addr: SocketAddr) {
    let mut from_local = vec![0u8; 65535];
    let mut from_relay = vec![0u8; 65535];
    let mut client: Option<SocketAddr> = None;
    let mut idle_at = tokio::time::Instant::now();
    loop {
        let left = UDP_IDLE.saturating_sub(idle_at.elapsed());
        if left.is_zero() {
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(left) => return,
            recvd = local.recv_from(&mut from_local) => {
                let Ok((n, src)) = recvd else { return };
                client = Some(src);
                idle_at = tokio::time::Instant::now();
                if relay.send_to(&from_local[..n], relay_addr).await.is_err() {
                    return;
                }
            }
            recvd = relay.recv_from(&mut from_relay) => {
                let Ok((n, src)) = recvd else { return };
                if src != relay_addr {
                    continue;
                }
                idle_at = tokio::time::Instant::now();
                let Some(dst) = client else { continue };
                if local.send_to(&from_relay[..n], dst).await.is_err() {
                    return;
                }
            }
        }
    }
}

fn at_local(entrance: &ArtefactEntrance, local: SocketAddr) -> ArtefactEntrance {
    let mut dial = entrance.clone();
    dial.addr = local.to_string();
    dial.relay = None;
    dial
}

/// Same rule as `door`: a literal socket address, otherwise one `lookup_host` result.
async fn resolve(addr: &str) -> Result<SocketAddr, String> {
    if let Ok(parsed) = addr.parse() {
        return Ok(parsed);
    }
    let mut found = tokio::net::lookup_host(addr)
        .await
        .map_err(|_| "dial address did not resolve".to_owned())?;
    found.next().ok_or_else(|| "dial address did not resolve".to_owned())
}

/// Bracket-aware host/port split, matching `host_of` in `door`.
fn host_port(addr: &str) -> Result<(&str, u16), String> {
    let (host, port) = if let Some(rest) = addr.strip_prefix('[') {
        let Some((host, after)) = rest.split_once(']') else {
            return Err("dial address is not host:port".to_owned());
        };
        let Some(port) = after.strip_prefix(':') else {
            return Err("dial address is not host:port".to_owned());
        };
        (host, port)
    } else {
        let Some((host, port)) = addr.rsplit_once(':') else {
            return Err("dial address is not host:port".to_owned());
        };
        (host, port)
    };
    if host.is_empty() {
        return Err("dial address is not host:port".to_owned());
    }
    let port = port
        .parse::<u16>()
        .map_err(|_| "dial address is not host:port".to_owned())?;
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_is_length_host_and_big_endian_port() {
        let bytes = target_envelope("203.0.113.10:8443").unwrap();
        let host = b"203.0.113.10";
        assert_eq!(bytes[0], host.len() as u8);
        assert_eq!(&bytes[1..1 + host.len()], host);
        assert_eq!(&bytes[1 + host.len()..], &8443u16.to_be_bytes());
    }

    #[test]
    fn ipv6_brackets_are_not_part_of_the_host() {
        let bytes = target_envelope("[2001:db8::1]:443").unwrap();
        let host = b"2001:db8::1";
        assert_eq!(bytes[0], host.len() as u8);
        assert_eq!(&bytes[1..1 + host.len()], host);
        assert_eq!(&bytes[1 + host.len()..], &443u16.to_be_bytes());
    }
}
