//! The SOCKS5 UDP association over the packet plane.
//!
//! RFC 1928 §7: the client opens a TCP control connection, asks for a UDP
//! association, and then sends its datagrams to the address the server
//! named. Every datagram carries its own destination, and every answer comes
//! back on the same association.
//!
//! Over the plane this is a plain UDP socket inside the tunnel: one socket
//! for the whole association, one datagram per destination, the name in the
//! header resolved THROUGH the tunnel.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::AsyncReadExt;
use tokio::net::{TcpStream, UdpSocket};
use tracing::{debug, info, warn};

use crate::plane_net::PlaneStack;
use crate::socks5::{parse_socks5_udp_request, split_target, write_udp_associate_reply, REP_SUCCESS};

/// The biggest datagram the relay takes.
const RELAY_BUFFER: usize = 64 * 1024;

/// Serve one UDP association over the plane.
///
/// The control connection is the SOCKS5 one: when it closes, the association
/// is over.
pub async fn serve_udp_association(
    mut stream: TcpStream,
    stack: Arc<PlaneStack>,
) -> io::Result<()> {
    // The relay socket the SOCKS5 client sends its datagrams to.
    let relay = UdpSocket::bind("127.0.0.1:0").await?;
    let bound = relay.local_addr()?;
    write_udp_associate_reply(&mut stream, REP_SUCCESS, bound).await?;
    info!(%bound, "socks5: udp association started over the plane");

    // The socket inside the tunnel.
    let mut tunnel = stack.open_udp().await?;

    // Where the client's datagrams come from.
    let mut client: Option<SocketAddr> = None;
    // The header shape of every destination, so the answers come back in the
    // shape the client itself used - a client that only understands the ATYP
    // it sent always gets that ATYP back.
    let mut shapes: HashMap<SocketAddr, Vec<u8>> = HashMap::new();

    let mut buf = vec![0u8; RELAY_BUFFER];
    loop {
        tokio::select! {
            // The control connection ended: the association is over.
            closed = read_control(&mut stream) => {
                if closed.is_err() {
                    break;
                }
            }
            // A datagram from the SOCKS5 client.
            result = relay.recv_from(&mut buf) => {
                let (read, from) = result?;
                if client.is_none() {
                    client = Some(from);
                }
                let Ok(request) = parse_socks5_udp_request(&buf[..read]) else {
                    debug!("socks5: dropping a datagram with a malformed header");
                    continue;
                };
                if request.frag != 0 {
                    // RFC 1928 leaves the reassembly optional; this deployment
                    // implements none of it.
                    debug!(frag = request.frag, "socks5: dropping a fragmented datagram");
                    continue;
                }
                let Some((host, port)) = split_target(&request.target) else {
                    debug!(target = %request.target, "socks5: dropping a datagram with a malformed destination");
                    continue;
                };
                // The name is resolved THROUGH the tunnel.
                let addresses = match stack.resolve(&host).await {
                    Ok(addresses) => addresses,
                    Err(e) => {
                        warn!(target = %request.target, error = %e, "socks5: the name did not resolve");
                        continue;
                    }
                };
                for ip in addresses {
                    let addr = SocketAddr::new(ip, port);
                    shapes.insert(addr, request.header_echo.clone());
                    if tunnel.send_to(&request.data, addr).is_ok() {
                        break;
                    }
                }
            }
            // A datagram from the tunnel.
            result = tunnel.recv_from() => {
                let Some((data, from)) = result else {
                    break;
                };
                let Some(client) = client else {
                    continue;
                };
                let header = shapes
                    .get(&from)
                    .cloned()
                    .unwrap_or_else(|| header_for(&from));
                let mut out = Vec::with_capacity(3 + header.len() + data.len());
                out.extend_from_slice(&[0x00, 0x00, 0x00]);
                out.extend_from_slice(&header);
                out.extend_from_slice(&data);
                if relay.send_to(&out, client).await.is_err() {
                    break;
                }
            }
        }
    }
    info!("socks5: udp association over the plane ended");
    Ok(())
}

/// Read the control connection.
///
/// The client sends nothing on it after the request; the end of the
/// connection is the end of the association.
async fn read_control(stream: &mut TcpStream) -> io::Result<()> {
    let mut buf = [0u8; 64];
    let read = stream.read(&mut buf).await?;
    if read == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the control connection closed",
        ));
    }
    Ok(())
}

/// The SOCKS5 UDP header of an answer that has no shape of its own.
fn header_for(addr: &SocketAddr) -> Vec<u8> {
    let mut out = Vec::with_capacity(19);
    match addr {
        SocketAddr::V4(v4) => {
            out.push(0x01);
            out.extend_from_slice(&v4.ip().octets());
            out.extend_from_slice(&v4.port().to_be_bytes());
        }
        SocketAddr::V6(v6) => {
            out.push(0x04);
            out.extend_from_slice(&v6.ip().octets());
            out.extend_from_slice(&v6.port().to_be_bytes());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    #[test]
    fn an_ipv4_answer_carries_the_ipv4_shape() {
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 53));
        let header = header_for(&addr);
        assert_eq!(header[0], 0x01);
        assert_eq!(&header[1..5], &[1, 2, 3, 4]);
        assert_eq!(&header[5..7], &53u16.to_be_bytes());
    }
}
