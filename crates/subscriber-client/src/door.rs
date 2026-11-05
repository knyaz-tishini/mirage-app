//! A non-Mirage entrance.
//!
//! The session probe opens the real protocol. A SOCKS flow opens it again
//! and carries the target: the shared preamble on a stream door, the
//! Shadowsocks address on that protocol, and the same preamble inside
//! IPv4/UDP for WireGuard and AmneziaWG.

use std::net::SocketAddr;
use std::pin::Pin;
use std::str::FromStr;
use std::time::Duration;

use mirage_core::mirage::wire::Protocol;
use mirage_protocol::camouflage::{CamouflageLayer, UdpCamouflageLayer};
use mirage_protocol::preamble::write_preamble;
use mirage_protocol::protocols::l3::amneziawg::{self, AwgObfuscation};
use mirage_protocol::protocols::l3::icmp::IcmpCamouflage;
use mirage_protocol::protocols::l3::wireguard::{
    datagram_with_payload, payload_of, take_tunn_result, tunn_from_hex, CARRY_CHUNK, Tunn, TunnStep,
};
use mirage_protocol::protocols::l4::quic::QuicCamouflage;
use mirage_protocol::protocols::l5::tls::TlsRealCamouflage;
use mirage_protocol::protocols::l7::dns::{DnsEncoding, DnsQueryType, DnsTunnelCamouflage};
use mirage_protocol::protocols::l7::grpc::GrpcCamouflage;
use mirage_protocol::protocols::l7::http3::Http3Camouflage;
use mirage_protocol::protocols::l7::shadowsocks::{
    CipherKind, Context, ProxyClientStream, ServerConfig, ServerType,
};
use mirage_protocol::protocols::l7::ssh::SshCamouflage;
use mirage_protocol::protocols::l7::websocket::WebSocketCamouflage;
use mirage_protocol::protocols::l7::webtransport::WebTransportCamouflage;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::artefact::ArtefactEntrance;

/// One byte stream after the protocol handshake.
///
/// `AsyncRead` and `AsyncWrite` are both non-auto traits, so a trait object
/// names this one trait and the two stream traits stay its supertraits.
pub(crate) trait DoorStream: AsyncRead + AsyncWrite + Send {}

impl<T> DoorStream for T where T: AsyncRead + AsyncWrite + Send {}

/// The byte stream a door hands to SOCKS after the target is written.
pub(crate) type DoorIo = Pin<Box<dyn DoorStream>>;

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// One real handshake. The stream may be closed afterwards.
pub(crate) async fn probe(entrance: &ArtefactEntrance) -> Result<(), String> {
    let entrance = crate::envelope::front(entrance).await?;
    match &entrance.protocol {
        Protocol::Mirage { .. } => Err("mirage is not a protocol door".to_owned()),
        Protocol::WireGuard { .. } => prove_wireguard(&entrance).await,
        Protocol::AmneziaWg { .. } => prove_amneziawg(&entrance).await,
        Protocol::Shadowsocks { .. } => {
            let mut stream = open_shadowsocks(&entrance, "127.0.0.1", 1).await?;
            tokio::time::timeout(PROBE_TIMEOUT, async {
                stream.write_all(b"x").await.map_err(|e| e.to_string())?;
                stream.flush().await.map_err(|e| e.to_string())?;
                Ok::<(), String>(())
            })
            .await
            .map_err(|_| "shadowsocks handshake timed out".to_owned())??;
            prove_read(&mut stream).await
        }
        Protocol::Dns { .. } => {
            let _stream = tokio::time::timeout(PROBE_TIMEOUT, open_stream(&entrance))
                .await
                .map_err(|_| "dns handshake timed out".to_owned())??;
            Ok(())
        }
        Protocol::Icmp { .. } => {
            let mut stream = open_stream(&entrance).await?;
            write_preamble(&mut stream, "127.0.0.1", 1)
                .await
                .map_err(|_| "preamble was refused".to_owned())?;
            prove_read(&mut stream).await
        }
        _ => {
            let _stream = tokio::time::timeout(PROBE_TIMEOUT, open_stream(&entrance))
                .await
                .map_err(|_| format!("{} handshake timed out", entrance.protocol.label()))??;
            Ok(())
        }
    }
}

/// Open the entrance's protocol and carry `host:port` on it.
pub(crate) async fn open_carrying(
    entrance: &ArtefactEntrance,
    host: &str,
    port: u16,
) -> Result<DoorIo, String> {
    let entrance = crate::envelope::front(entrance).await?;
    match &entrance.protocol {
        Protocol::Mirage { .. } => Err("mirage is not a protocol door".to_owned()),
        Protocol::WireGuard { .. } => open_wireguard(&entrance, host, port).await,
        Protocol::AmneziaWg { .. } => open_amneziawg(&entrance, host, port).await,
        Protocol::Shadowsocks { .. } => open_shadowsocks(&entrance, host, port).await,
        _ => {
            let mut stream = open_stream(&entrance).await?;
            write_preamble(&mut stream, host, port)
                .await
                .map_err(|_| "preamble was refused".to_owned())?;
            Ok(stream)
        }
    }
}

async fn prove_read(stream: &mut DoorIo) -> Result<(), String> {
    let mut buf = [0u8; 1];
    match tokio::time::timeout(PROBE_TIMEOUT, stream.read(&mut buf)).await {
        Ok(_) => Ok(()),
        Err(_) => Err("protocol door timed out".to_owned()),
    }
}

async fn open_stream(entrance: &ArtefactEntrance) -> Result<DoorIo, String> {
    let label = entrance.protocol.label();
    match &entrance.protocol {
        Protocol::Tls { sni, alpn } => {
            let layer = TlsRealCamouflage::new(sni.clone(), alpn.clone(), None, None)
                .map_err(|_| format!("{label} handshake failed"))?;
            let tcp = tcp_connect(&entrance.addr).await?;
            let stream = layer
                .wrap_client(tcp, sni)
                .await
                .map_err(|_| format!("{label} handshake failed"))?;
            Ok(Box::pin(stream))
        }
        Protocol::WebSocket { path, host } => {
            let layer = WebSocketCamouflage::new(path.clone(), host.clone());
            let tcp = tcp_connect(&entrance.addr).await?;
            let stream = layer
                .wrap_client(tcp, "")
                .await
                .map_err(|_| format!("{label} handshake failed"))?;
            Ok(Box::pin(stream))
        }
        Protocol::Ssh {
            ssh_user,
            ssh_host_key,
            ssh_private_key,
            ssh_password,
            forward_host,
            forward_port,
        } => {
            let layer = SshCamouflage {
                ssh_user: ssh_user.clone(),
                ssh_host_key: ssh_host_key.clone(),
                ssh_private_key: ssh_private_key.clone(),
                ssh_password: ssh_password.clone(),
                forward_host: forward_host.clone(),
                forward_port: *forward_port,
            };
            let tcp = tcp_connect(&entrance.addr).await?;
            let stream = layer
                .wrap_client(tcp, "")
                .await
                .map_err(|_| "ssh handshake failed".to_owned())?;
            Ok(Box::pin(stream))
        }
        Protocol::Grpc {
            service_name,
            method_name,
            authority,
            use_tls,
        } => {
            let layer = GrpcCamouflage {
                service_name: service_name.clone(),
                method_name: method_name.clone(),
                authority: authority.clone(),
                use_tls: *use_tls,
            };
            let tcp = tcp_connect(&entrance.addr).await?;
            let stream = layer
                .wrap_client(tcp, "")
                .await
                .map_err(|_| format!("{label} handshake failed"))?;
            Ok(Box::pin(stream))
        }
        Protocol::Quic { alpn } => {
            let layer = QuicCamouflage::new(alpn.clone()).map_err(|_| format!("{label} handshake failed"))?;
            let addr = resolve(&entrance.addr).await?;
            let stream = layer
                .connect_client(addr, host_of(&entrance.addr))
                .await
                .map_err(|_| format!("{label} handshake failed"))?;
            Ok(Box::pin(stream))
        }
        Protocol::Http3 {
            authority,
            tunnel_path,
            alpn,
        } => {
            let layer = Http3Camouflage::new(authority.clone(), tunnel_path.clone(), alpn.clone())
                .map_err(|_| format!("{label} handshake failed"))?;
            let addr = resolve(&entrance.addr).await?;
            let stream = layer
                .connect_client(addr, authority)
                .await
                .map_err(|_| format!("{label} handshake failed"))?;
            Ok(Box::pin(stream))
        }
        Protocol::WebTransport { path, authority } => {
            let layer = WebTransportCamouflage::new(path.clone(), authority.clone())
                .map_err(|_| format!("{label} handshake failed"))?;
            let addr = resolve(&entrance.addr).await?;
            let stream = layer
                .connect_client(addr, authority)
                .await
                .map_err(|_| format!("{label} handshake failed"))?;
            Ok(Box::pin(stream))
        }
        Protocol::Dns {
            domain,
            encoding,
            query_type,
            ..
        } => {
            let encoding = DnsEncoding::from_name(encoding)?;
            let query_type = DnsQueryType::from_name(query_type)?;
            let layer = DnsTunnelCamouflage::new(domain.clone(), encoding, query_type);
            let addr = resolve(&entrance.addr).await?;
            let stream = layer
                .connect_client(addr, "")
                .await
                .map_err(|_| format!("{label} handshake failed"))?;
            Ok(Box::pin(stream))
        }
        Protocol::Icmp {
            peer_addr,
            identifier,
            mtu,
        } => {
            // Not a TCP/UDP proxy. The echo goes to the relay IP, and the
            // first payload is the terminator envelope. Later writes,
            // including write_preamble, stay one payload each.
            let peer = match entrance.relay.as_deref() {
                Some(relay) => resolve(relay).await?.ip(),
                None => *peer_addr,
            };
            let layer = IcmpCamouflage::new(peer, *identifier, *mtu);
            let addr = resolve(&entrance.addr).await?;
            let mut stream = layer
                .connect_client(addr, "")
                .await
                .map_err(|_| format!("{label} handshake failed"))?;
            if entrance.relay.is_some() {
                let envelope = crate::envelope::target_envelope(&entrance.addr)?;
                stream
                    .write_all(&envelope)
                    .await
                    .map_err(|_| format!("{label} envelope failed"))?;
            }
            Ok(Box::pin(stream))
        }
        Protocol::Shadowsocks { .. }
        | Protocol::Mirage { .. }
        | Protocol::WireGuard { .. }
        | Protocol::AmneziaWg { .. } => {
            Err(format!("{label} is not a stream door"))
        }
    }
}

async fn open_shadowsocks(entrance: &ArtefactEntrance, host: &str, port: u16) -> Result<DoorIo, String> {
    let Protocol::Shadowsocks { method, password } = &entrance.protocol else {
        return Err("shadowsocks door is missing its cipher".to_owned());
    };
    let cipher = CipherKind::from_str(method).map_err(|_| "shadowsocks method was refused".to_owned())?;
    let server = resolve(&entrance.addr).await?;
    let config = ServerConfig::new(server, password.clone(), cipher)
        .map_err(|_| "shadowsocks config was refused".to_owned())?;
    let context = Context::new_shared(ServerType::Local);
    let stream = ProxyClientStream::connect(context, &config, (host.to_owned(), port))
        .await
        .map_err(|_| "shadowsocks connect failed".to_owned())?;
    Ok(Box::pin(stream))
}

async fn prove_wireguard(entrance: &ArtefactEntrance) -> Result<(), String> {
    let (mut tunn, peer, socket) = wireguard_session(entrance).await?;
    let mut scratch = [0u8; 2048];
    handshake_wireguard(&mut tunn, &socket, peer, &mut scratch).await
}

async fn open_wireguard(entrance: &ArtefactEntrance, host: &str, port: u16) -> Result<DoorIo, String> {
    let (mut tunn, peer, socket) = wireguard_session(entrance).await?;
    let mut scratch = [0u8; 2048];
    handshake_wireguard(&mut tunn, &socket, peer, &mut scratch).await?;
    let (mut app, tunnel_side) = tokio::io::duplex(64 * 1024);
    write_preamble(&mut app, host, port)
        .await
        .map_err(|_| "preamble was refused".to_owned())?;
    tokio::spawn(async move {
        let _ = bridge_wireguard(tunnel_side, socket, tunn, peer).await;
    });
    Ok(Box::pin(app))
}

async fn wireguard_session(
    entrance: &ArtefactEntrance,
) -> Result<(Tunn, SocketAddr, tokio::net::UdpSocket), String> {
    let Protocol::WireGuard {
        static_private_hex,
        peer_public_hex,
        preshared_key_hex,
        ..
    } = &entrance.protocol
    else {
        return Err("wireguard door is missing its keys".to_owned());
    };
    let tunn = tunn_from_hex(
        static_private_hex,
        peer_public_hex,
        preshared_key_hex.as_deref(),
        1,
    )?;
    let peer = resolve(&entrance.addr).await?;
    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|_| "wireguard socket failed".to_owned())?;
    Ok((tunn, peer, socket))
}

async fn handshake_wireguard(
    tunn: &mut Tunn,
    socket: &tokio::net::UdpSocket,
    peer: SocketAddr,
    scratch: &mut [u8],
) -> Result<(), String> {
    let init = match take_tunn_result(tunn.format_handshake_initiation(scratch, false)) {
        TunnStep::Network(pkt) => pkt,
        _ => return Err("wireguard handshake did not start".to_owned()),
    };
    socket
        .send_to(&init, peer)
        .await
        .map_err(|_| "wireguard send failed".to_owned())?;
    let deadline = tokio::time::Instant::now() + PROBE_TIMEOUT;
    let mut buf = [0u8; 2048];
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return Err("wireguard handshake timed out".to_owned());
        }
        let (n, _) = tokio::time::timeout(left, socket.recv_from(&mut buf))
            .await
            .map_err(|_| "wireguard handshake timed out".to_owned())?
            .map_err(|_| "wireguard recv failed".to_owned())?;
        if finish_wireguard(tunn, socket, peer, &buf[..n], scratch).await? {
            return Ok(());
        }
    }
}

/// Drive one received datagram. `Ok(true)` once the handshake response has
/// been answered with its keepalive and nothing further is queued.
async fn finish_wireguard(
    tunn: &mut Tunn,
    socket: &tokio::net::UdpSocket,
    peer: SocketAddr,
    packet: &[u8],
    scratch: &mut [u8],
) -> Result<bool, String> {
    let mut datagram = packet.to_vec();
    let mut answered = false;
    loop {
        let step = take_tunn_result(tunn.decapsulate(
            if datagram.is_empty() {
                None
            } else {
                Some(peer.ip())
            },
            &datagram,
            scratch,
        ));
        datagram.clear();
        match step {
            TunnStep::Network(pkt) => {
                socket
                    .send_to(&pkt, peer)
                    .await
                    .map_err(|_| "wireguard send failed".to_owned())?;
                answered = true;
            }
            TunnStep::Inner(_) => {}
            TunnStep::Done => return Ok(answered),
            TunnStep::Failed => return Ok(false),
        }
    }
}

async fn bridge_wireguard(
    mut side: tokio::io::DuplexStream,
    socket: tokio::net::UdpSocket,
    mut tunn: Tunn,
    peer: SocketAddr,
) -> Result<(), String> {
    let mut scratch = [0u8; 2048];
    let mut udp_buf = [0u8; 2048];
    let mut read_buf = [0u8; CARRY_CHUNK];
    loop {
        tokio::select! {
            read = side.read(&mut read_buf) => {
                let n = match read {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let datagram = datagram_with_payload(&read_buf[..n]);
                if let TunnStep::Network(out) = take_tunn_result(tunn.encapsulate(&datagram, &mut scratch)) {
                    socket.send_to(&out, peer).await.map_err(|_| "wireguard send failed".to_owned())?;
                }
            }
            recvd = socket.recv_from(&mut udp_buf) => {
                let (n, _) = recvd.map_err(|_| "wireguard recv failed".to_owned())?;
                write_inners_wireguard(&mut tunn, &socket, peer, &udp_buf[..n], &mut scratch, &mut side).await?;
            }
        }
    }
    Ok(())
}

async fn write_inners_wireguard(
    tunn: &mut Tunn,
    socket: &tokio::net::UdpSocket,
    peer: SocketAddr,
    packet: &[u8],
    scratch: &mut [u8],
    side: &mut tokio::io::DuplexStream,
) -> Result<(), String> {
    let mut datagram = packet.to_vec();
    loop {
        let step = take_tunn_result(tunn.decapsulate(
            if datagram.is_empty() {
                None
            } else {
                Some(peer.ip())
            },
            &datagram,
            scratch,
        ));
        datagram.clear();
        match step {
            TunnStep::Network(pkt) => {
                socket
                    .send_to(&pkt, peer)
                    .await
                    .map_err(|_| "wireguard send failed".to_owned())?;
            }
            TunnStep::Inner(pkt) => {
                if let Some(payload) = payload_of(&pkt) {
                    side.write_all(payload)
                        .await
                        .map_err(|_| "wireguard carry failed".to_owned())?;
                }
            }
            TunnStep::Done | TunnStep::Failed => return Ok(()),
        }
    }
}

async fn prove_amneziawg(entrance: &ArtefactEntrance) -> Result<(), String> {
    let (mut tunn, peer, socket) = amneziawg_session(entrance).await?;
    let mut scratch = vec![0u8; 65535];
    handshake_amneziawg(&mut tunn, &socket, peer, &mut scratch).await
}

async fn open_amneziawg(entrance: &ArtefactEntrance, host: &str, port: u16) -> Result<DoorIo, String> {
    let (mut tunn, peer, socket) = amneziawg_session(entrance).await?;
    let mut scratch = vec![0u8; 65535];
    handshake_amneziawg(&mut tunn, &socket, peer, &mut scratch).await?;
    let (mut app, tunnel_side) = tokio::io::duplex(64 * 1024);
    write_preamble(&mut app, host, port)
        .await
        .map_err(|_| "preamble was refused".to_owned())?;
    tokio::spawn(async move {
        let _ = bridge_amneziawg(tunnel_side, socket, tunn, peer).await;
    });
    Ok(Box::pin(app))
}

async fn amneziawg_session(
    entrance: &ArtefactEntrance,
) -> Result<(amneziawg::Tunn, SocketAddr, tokio::net::UdpSocket), String> {
    let Protocol::AmneziaWg {
        static_private_hex,
        peer_public_hex,
        preshared_key_hex,
        jc,
        jmin,
        jmax,
        s1,
        s2,
        s3,
        s4,
        h1,
        h2,
        h3,
        h4,
        ..
    } = &entrance.protocol
    else {
        return Err("amneziawg door is missing its keys".to_owned());
    };
    let tunn = amneziawg::tunn_from_hex(
        static_private_hex,
        peer_public_hex,
        preshared_key_hex.as_deref(),
        &AwgObfuscation {
            jc: *jc,
            jmin: *jmin,
            jmax: *jmax,
            s1: *s1,
            s2: *s2,
            s3: *s3,
            s4: *s4,
            h1: h1.clone(),
            h2: h2.clone(),
            h3: h3.clone(),
            h4: h4.clone(),
        },
        1,
    )?;
    let peer = resolve(&entrance.addr).await?;
    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|_| "amneziawg socket failed".to_owned())?;
    Ok((tunn, peer, socket))
}

async fn handshake_amneziawg(
    tunn: &mut amneziawg::Tunn,
    socket: &tokio::net::UdpSocket,
    peer: SocketAddr,
    scratch: &mut [u8],
) -> Result<(), String> {
    let init = match amneziawg::take_tunn_result(tunn.format_handshake_initiation(scratch, false)) {
        amneziawg::TunnStep::Network(pkt) => pkt,
        _ => return Err("amneziawg handshake did not start".to_owned()),
    };
    send_awg_junk(tunn, socket, peer).await?;
    socket
        .send_to(&init, peer)
        .await
        .map_err(|_| "amneziawg send failed".to_owned())?;
    let deadline = tokio::time::Instant::now() + PROBE_TIMEOUT;
    let mut buf = vec![0u8; 65535];
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return Err("amneziawg handshake timed out".to_owned());
        }
        let (n, _) = tokio::time::timeout(left, socket.recv_from(&mut buf))
            .await
            .map_err(|_| "amneziawg handshake timed out".to_owned())?
            .map_err(|_| "amneziawg recv failed".to_owned())?;
        if finish_amneziawg(tunn, socket, peer, &buf[..n], scratch).await? {
            return Ok(());
        }
    }
}

async fn finish_amneziawg(
    tunn: &mut amneziawg::Tunn,
    socket: &tokio::net::UdpSocket,
    peer: SocketAddr,
    packet: &[u8],
    scratch: &mut [u8],
) -> Result<bool, String> {
    let mut datagram = packet.to_vec();
    let mut answered = false;
    loop {
        let step = amneziawg::take_tunn_result(tunn.decapsulate(
            if datagram.is_empty() {
                None
            } else {
                Some(peer.ip())
            },
            &datagram,
            scratch,
        ));
        datagram.clear();
        send_awg_junk(tunn, socket, peer).await?;
        match step {
            amneziawg::TunnStep::Network(pkt) => {
                socket
                    .send_to(&pkt, peer)
                    .await
                    .map_err(|_| "amneziawg send failed".to_owned())?;
                answered = true;
            }
            amneziawg::TunnStep::Inner(_) => {}
            amneziawg::TunnStep::Done => return Ok(answered),
            amneziawg::TunnStep::Failed => return Ok(false),
        }
    }
}

async fn send_awg_junk(
    tunn: &mut amneziawg::Tunn,
    socket: &tokio::net::UdpSocket,
    peer: SocketAddr,
) -> Result<(), String> {
    while let Some(pkt) = tunn.poll_outgoing_packet() {
        socket
            .send_to(&pkt, peer)
            .await
            .map_err(|_| "amneziawg send failed".to_owned())?;
    }
    Ok(())
}

async fn bridge_amneziawg(
    mut side: tokio::io::DuplexStream,
    socket: tokio::net::UdpSocket,
    mut tunn: amneziawg::Tunn,
    peer: SocketAddr,
) -> Result<(), String> {
    let mut scratch = vec![0u8; 65535];
    let mut udp_buf = vec![0u8; 65535];
    let mut read_buf = [0u8; CARRY_CHUNK];
    loop {
        tokio::select! {
            read = side.read(&mut read_buf) => {
                let n = match read {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let datagram = datagram_with_payload(&read_buf[..n]);
                let sent = amneziawg::take_tunn_result(tunn.encapsulate(&datagram, &mut scratch));
                send_awg_junk(&mut tunn, &socket, peer).await?;
                if let amneziawg::TunnStep::Network(out) = sent {
                    socket.send_to(&out, peer).await.map_err(|_| "amneziawg send failed".to_owned())?;
                }
            }
            recvd = socket.recv_from(&mut udp_buf) => {
                let (n, _) = recvd.map_err(|_| "amneziawg recv failed".to_owned())?;
                write_inners_amneziawg(&mut tunn, &socket, peer, &udp_buf[..n], &mut scratch, &mut side).await?;
            }
        }
    }
    Ok(())
}

async fn write_inners_amneziawg(
    tunn: &mut amneziawg::Tunn,
    socket: &tokio::net::UdpSocket,
    peer: SocketAddr,
    packet: &[u8],
    scratch: &mut [u8],
    side: &mut tokio::io::DuplexStream,
) -> Result<(), String> {
    let mut datagram = packet.to_vec();
    loop {
        let step = amneziawg::take_tunn_result(tunn.decapsulate(
            if datagram.is_empty() {
                None
            } else {
                Some(peer.ip())
            },
            &datagram,
            scratch,
        ));
        datagram.clear();
        send_awg_junk(tunn, socket, peer).await?;
        match step {
            amneziawg::TunnStep::Network(pkt) => {
                socket
                    .send_to(&pkt, peer)
                    .await
                    .map_err(|_| "amneziawg send failed".to_owned())?;
            }
            amneziawg::TunnStep::Inner(pkt) => {
                if let Some(payload) = payload_of(&pkt) {
                    side.write_all(payload)
                        .await
                        .map_err(|_| "amneziawg carry failed".to_owned())?;
                }
            }
            amneziawg::TunnStep::Done | amneziawg::TunnStep::Failed => return Ok(()),
        }
    }
}

async fn tcp_connect(addr: &str) -> Result<TcpStream, String> {
    TcpStream::connect(addr)
        .await
        .map_err(|_| "tcp connect failed".to_owned())
}

async fn resolve(addr: &str) -> Result<SocketAddr, String> {
    if let Ok(parsed) = addr.parse() {
        return Ok(parsed);
    }
    let mut found = tokio::net::lookup_host(addr)
        .await
        .map_err(|_| "dial address did not resolve".to_owned())?;
    found
        .next()
        .ok_or_else(|| "dial address did not resolve".to_owned())
}

fn host_of(addr: &str) -> &str {
    if let Some(rest) = addr.strip_prefix('[') {
        if let Some((host, _)) = rest.split_once(']') {
            return host;
        }
    }
    addr.rsplit_once(':').map(|(host, _)| host).unwrap_or(addr)
}
