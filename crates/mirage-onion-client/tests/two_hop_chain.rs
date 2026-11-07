//! Two hops, walked for real.
//!
//! The stand is not a mock: the entry and the exit are the PRODUCTION node's
//! own handlers (`mirage-node`'s relay peel and FS exit), and the client is
//! the production onion dial. The point of the test is exactly that seam —
//! that the chain the client walks is a chain a real node accepts, credential
//! and all.
//!
//! Two hops are the whole chain: an entry that takes the traffic in and an
//! exit that puts it out. One more hop would only relay in between.
//!
//! # Why the hops are not on loopback
//!
//! The node's SSRF guard refuses loopback unconditionally — an exit that
//! could be pointed at 127.0.0.1 would be a probe into the machine it runs
//! on. So the stand binds the real interface and the chain names that
//! address. The private-range part of the guard is relaxed for the stand,
//! because a private range is what a real interface has.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwap;
use ed25519_dalek::SigningKey;
use mirage_client_core::relay::TunnelCompat;
use mirage_core::config::TransportCamouflage;
use mirage_core::crypto::{CredentialParams, KeyPair, SpentSet};
use mirage_mux::tokio_compat::{SessionHandle, TokioMuxStream};
use mirage_node::core::role_signed_gate::RoleSignedReplayCache;
use mirage_node::family::mirage::exit::onion::{handle_onion_exit, OnionExitV7Config};
use mirage_node::family::mirage::exit::policy;
use mirage_node::family::mirage::relay::onion_peel::{handle_onion_peel, OnionRelayConfig};
use mirage_onion_client::{dial, OnionChainSpec, OnionHopSpec};
use mirage_protocol::auth::v9::CLIENT_HELLO_LEN;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

/// The mesh both the chain and the credential speak.
const MESH_ID: &str = "mirage-vpn";

/// The answer the target gives, through the chain, to the customer.
const GREETING: &[u8] = b"hello from the target";

/// A non-loopback address of this machine that can reach itself.
///
/// The node's guard refuses loopback outright, so a local stand has to use a
/// real interface. The address is the one the machine would use to reach the
/// outside world — bound, then "connected" (no packet is sent) to learn it.
fn stand_ip() -> Ipv4Addr {
    let sock = UdpSocket::bind("0.0.0.0:0").expect("bind a udp socket");
    sock.connect("8.8.8.8:80").expect("learn the outbound address");
    let ip = sock.local_addr().expect("the local address").ip();
    match ip {
        IpAddr::V4(v4) if !v4.is_loopback() => v4,
        other => panic!(
            "the stand needs a non-loopback address of this machine, got {other} - \
             run it on a machine with a routable interface"
        ),
    }
}

/// Bind a listener on every interface and report the address to name it by.
///
/// The chain names the hop by the address the customer dials, so the stand
/// needs the interface address, not the wildcard.
async fn bind_hop() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("0.0.0.0:0").await.expect("bind the hop");
    let port = listener.local_addr().expect("the hop address").port();
    (listener, SocketAddr::new(IpAddr::V4(stand_ip()), port))
}

/// The chain's trust anchor: the operator keypair both the client and the
/// exit trust.
fn operator_key() -> (SigningKey, String) {
    let key = SigningKey::from_bytes(&[0x5Au8; 32]);
    let pubkey = hex::encode(key.verifying_key().to_bytes());
    (key, pubkey)
}

/// The stand's target: what the customer actually reaches through the chain.
async fn spawn_target() -> SocketAddr {
    let listener = TcpListener::bind("0.0.0.0:0").await.expect("bind the target");
    let addr = SocketAddr::new(IpAddr::V4(stand_ip()), listener.local_addr().expect("addr").port());
    tokio::spawn(async move {
        let (mut sock, _peer) = listener.accept().await.expect("the target accepts");
        let _ = sock.write_all(GREETING).await;
        let _ = sock.flush().await;
        // Hold the connection open long enough for the answer to travel back
        // through the chain before the socket drops.
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    addr
}

/// The face hello a dial into a chain FACE owes.
///
/// A node's chain face is fronted by a perimeter whose classifier will not
/// call a connection wire until this many bytes that parse as a V9
/// ClientHello have arrived; below it, `chain_splice` skips exactly this
/// many before reading the envelope.
///
/// The CLIENT always writes it - it dials the entry's face. A RELAY writes it
/// only when it dials the next hop's face, which is the camouflaged case; a
/// relay dialing a raw port does not (and the raw port expects nothing).
///
/// The stand has no perimeter, so it skips the bytes itself. Without this the
/// first four bytes on the wire are the hello's, and the envelope length comes
/// out as `0x09000000`.
async fn skip_face_hello(stream: &mut tokio::net::TcpStream) {
    let mut hello = [0u8; CLIENT_HELLO_LEN];
    stream
        .read_exact(&mut hello)
        .await
        .expect("the face hello must arrive");
}

/// The exit: the chain's last hop. It peels its layer, then serves the mux
/// the client ends up talking to.
async fn spawn_exit(node_id: &str, mesh_id: &str) -> (SocketAddr, Arc<KeyPair>) {
    let (listener, addr) = bind_hop().await;
    let keypair = Arc::new(KeyPair::generate());
    let keypair_for_task = Arc::clone(&keypair);
    let node_id = node_id.to_owned();
    let mesh_id = mesh_id.as_bytes().to_vec();
    tokio::spawn(async move {
        loop {
            let (sock, peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => break,
            };
            // The relay dialed this hop RAW (no camouflage), so no face hello
            // was written and none is expected.
            let cfg = OnionExitV7Config {
                hop_keypair: Arc::clone(&keypair_for_task),
                chain_position: 1,
                chain_length: 2,
                node_id: node_id.clone(),
                // The credential is enforced unconditionally: `None` fails
                // closed rather than opening the relay. The stand's trust
                // anchor is the chain's operator key, so the exit admits the
                // client's operator-signed chain leg.
                credential_params: Arc::new(ArcSwap::from_pointee(Some(
                    CredentialParams::default(),
                ))),
                spent_set: Arc::new(Mutex::new(SpentSet::new())),
                mesh_id: mesh_id.clone(),
                role_signed_replay: Arc::new(Mutex::new(RoleSignedReplayCache::new())),
            };
            tokio::spawn(async move {
                if let Err(e) = handle_onion_exit(cfg, sock, peer).await {
                    eprintln!("[stand] the exit refused: {e}");
                }
            });
        }
    });
    (addr, keypair)
}

/// The entry: the chain's first hop. It peels its layer and forwards what is
/// left to the next hop.
async fn spawn_entry(node_id: &str) -> (SocketAddr, Arc<KeyPair>) {
    let (listener, addr) = bind_hop().await;
    let keypair = Arc::new(KeyPair::generate());
    let keypair_for_task = Arc::clone(&keypair);
    let node_id = node_id.to_owned();
    tokio::spawn(async move {
        loop {
            let (mut sock, peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => break,
            };
            skip_face_hello(&mut sock).await;
            let cfg = OnionRelayConfig {
                hop_keypair: Arc::clone(&keypair_for_task),
                chain_position: 0,
                chain_length: 2,
                outgoing_camouflage: None,
                next_hop_pubkey: [0u8; 32],
                outgoing_sni: String::new(),
                node_id: node_id.clone(),
            };
            tokio::spawn(async move {
                if let Err(e) = handle_onion_peel(cfg, sock, peer, None).await {
                    eprintln!("[stand] the entry refused: {e}");
                }
            });
        }
    });
    (addr, keypair)
}

/// The client's chain: the two hops, in the order the traffic walks them.
fn chain_spec(
    entry: &OnionHopSpec,
    exit: &OnionHopSpec,
    operator_pubkey_hex: &str,
    operator_secret_hex: &str,
) -> OnionChainSpec {
    OnionChainSpec {
        chain_id: "stand".to_owned(),
        mesh_id: MESH_ID.to_owned(),
        self_id: Uuid::new_v4(),
        operator_pubkey_hex: operator_pubkey_hex.to_owned(),
        operator_secret_hex: operator_secret_hex.to_owned(),
        identity_not_after_ms: now_ms() + 60 * 60 * 1000,
        client_static_priv: [0x11u8; 32],
        hops: vec![entry.clone(), exit.clone()],
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// A hop in the chain's own shape.
fn hop(node_id: &str, addr: SocketAddr, keypair: &KeyPair) -> OnionHopSpec {
    OnionHopSpec {
        node_id: node_id.to_owned(),
        addr: addr.to_string(),
        server_pubkey_hex: hex::encode(keypair.public.to_bytes()),
        // The entry wears no mask, and says so: the chain's face says raw,
        // which is what the client dials. A hop that wore none but said
        // nothing would leave the client to guess its face from the peer
        // table — and the client holds none.
        camouflage: Some(TransportCamouflage::None),
    }
}

/// The whole chain, walked by the client, with the traffic coming out of the
/// exit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_client_walks_a_two_hop_chain_and_the_traffic_comes_out_of_the_exit() {
    // The node's own logs — the stand has to say WHY it refused.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_test_writer()
        .try_init();

    // The exit trusts the chain's operator key - a standalone box points the
    // binary at its own anchor through this variable.
    let (operator, operator_pubkey_hex) = operator_key();
    std::env::set_var("MIRAGE_OPERATOR_PUBKEY_HEX", &operator_pubkey_hex);
    // The stand's hops and its target are on this machine's own interface.
    policy::set_allow_private_exit_targets(true);

    let target_addr = spawn_target().await;
    let (exit_addr, exit_key) = spawn_exit("node-1", MESH_ID).await;
    let (entry_addr, entry_key) = spawn_entry("node-0").await;

    let spec = chain_spec(
        &hop("node-0", entry_addr, &entry_key),
        &hop("node-1", exit_addr, &exit_key),
        &operator_pubkey_hex,
        &hex::encode(operator.to_bytes()),
    );

    // The chain itself: the client dials the entry, the entry forwards to the
    // exit, and the client ends up on the exit's mux session.
    let tunnel = dial(&spec).await.expect("the chain must be walked");
    let session = SessionHandle::client(TunnelCompat::wrap(tunnel));

    let mut stream = TokioMuxStream(session.open_stream().await.expect("open a stream"));
    // The exit's own target frame: `[2B len][target]`.
    let target = target_addr.to_string();
    let mut frame = (target.len() as u16).to_be_bytes().to_vec();
    frame.extend_from_slice(target.as_bytes());
    stream.write_all(&frame).await.expect("write the target frame");

    let mut answer = vec![0u8; GREETING.len()];
    stream
        .read_exact(&mut answer)
        .await
        .expect("the answer must come back through the chain");
    assert_eq!(&answer[..], GREETING);
}
