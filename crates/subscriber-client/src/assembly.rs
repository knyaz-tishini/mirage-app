//! Assembles a validated [`EntranceArtefact`] into a running local proxy.
//!
//! One [`ChainHandle`] per TCP-shaped entrance (`crate::chain_build`), each
//! dialed by its own [`StaticKeyProtocol`] (`crate::dial`) presenting the
//! subscriber's persistent identity, registered into one [`ChainSet`],
//! fronted by a [`RouterImpl`] (default `StickyPerFlow` — a live flow stays
//! on the face it started on; fresh flows spread across faces, per
//! `thin-customer-connector-2026-09-17.md` §5) driving one shared
//! [`Socks5Server`] for SOCKS5 CONNECT. Every UDP-shaped entrance
//! (`TransportCamouflage::is_udp()`) instead becomes a
//! [`crate::udp_dial::UdpEntranceHandle`], never entering `chain_set` — see
//! that module's own doc for why — and is wired into the same
//! `Socks5Server` as its `UdpAssociateSupport::DatagramEntrance` for SOCKS5
//! UDP ASSOCIATE. The two paths share no state: a TCP flow cannot reach a
//! UDP entrance and a UDP flow cannot reach `chain_set`, structurally.
//! Everything below the artefact translation is reused machinery from
//! `mirage-client-core`.
//!
//! **Dependency boundaries that keep the forbidden things out of this
//! crate** (`thin-customer-connector-2026-09-17.md` §3):
//! - `peer_table` is `Arc::new(ArcSwap::from_pointee(PeerTable::empty()))`
//!   and NOTHING in this module ever writes to it — there is no code path
//!   here that could populate a node roster, by accident or otherwise.
//!   Every `HopConfig` this crate builds sets `public_key = Some(hex)`
//!   (`crate::chain_build`), so the empty peer_table is never even consulted
//!   at dial time.
//! - Every `ChainConfig` built here has exactly one hop
//!   (`crate::chain_build::build_chain_config`) — the artefact's own shape
//!   (`entrances: Vec<ArtefactEntrance>`, flat) gives this module no data to
//!   build a second hop from even if it tried.
//! - `credential_params` is a permanently-`None` cell and `capability_tokens`
//!   is `capability_pool::null_capability_cell()` — this crate's
//!   `Cargo.toml` does not depend on anything under
//!   `mirage_core::crypto::credential`, so there is no ring membership, no
//!   blind-RSA batch, and no signed-capability pool reachable from here even
//!   as an import.
//! - This crate's `Cargo.toml` has no dependency on `mirage-debug-shell` and
//!   no import of `mirage_client_core::monitor_link::MonitorLink` (only its
//!   two plain data structs, `MonitorConfig`/`MonitorChain`, used by
//!   `crate::tun_feed` to synthesise a config locally) — no debug tunnel
//!   listener, no monitor WS dial, anywhere in this crate.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use arc_swap::ArcSwap;
use dashmap::DashMap;
use mirage_client_core::{
    capability_pool,
    chain_set::router::{Router, RouterImpl, RoutingPolicy},
    pool::TunnelPool,
    socks5::{DatagramEntranceDialer, Socks5Server, UdpAssociateSupport},
    token_pool::TokenPool,
    ChainHandle, ChainId, ChainMetrics, ChainSet, ChainState, HealthProbeConfig, PrewarmStrategy,
};
use mirage_core::{
    config::{AuthMode, ClientConfig, MirageEndpointConfig, RoutingConfig, TransportKind, TunnelPoolConfig},
    error::MirageError,
    mirage::mesh::PeerTable,
    routing::RoutingTable,
};
use mirage_protocol::{negotiation::ProtocolChain, traits::TcpProtocol};
use thiserror::Error;
use tokio::sync::Notify;
use tracing::{info, warn};

use crate::artefact::{ArtefactError, ArtefactMode, EntranceArtefact};
use crate::chain_build::{build_chain_config, entrance_chain_id};
use crate::dial::StaticKeyProtocol;
use crate::tun_feed::TunFeed;
use crate::udp_dial::{UdpDialError, UdpEntranceHandle};

/// Every way [`SubscriberClient::build`] refuses.
#[derive(Debug, Error)]
pub enum SubscriberClientError {
    #[error(transparent)]
    Artefact(#[from] ArtefactError),

    #[error(transparent)]
    Mirage(#[from] MirageError),

    #[error(transparent)]
    UdpDial(#[from] UdpDialError),

    #[error("subscriber client: entrance chain map came back empty after a validated artefact")]
    NoUsableEntrance,

    #[error("subscriber client: failed to build the placeholder ClientConfig: {0}")]
    ClientConfig(serde_json::Error),
}

/// Tunable knobs for [`SubscriberClient::build`]. Everything else this
/// module needs comes from the artefact.
pub struct SubscriberClientOptions {
    /// Local bind address for the shared, router-backed SOCKS5 listener.
    pub socks5_bind: SocketAddr,
    /// Sharding policy across entrance faces. Default: `StickyPerFlow` — see
    /// the module doc.
    pub routing_policy: RoutingPolicy,
    /// Per-chain tunnel pool sizing. Default: `TunnelPoolConfig::default()`
    /// (3 warm slots, 120s idle timeout).
    pub pool: TunnelPoolConfig,
    /// When `Some`, also feed `client-core`'s `TunBridge` — see
    /// `crate::tun_feed`. `None` (default) leaves TUN untouched.
    pub tun_api_url: Option<String>,
}

impl Default for SubscriberClientOptions {
    fn default() -> Self {
        Self {
            socks5_bind: "127.0.0.1:11080".parse().unwrap_or_else(|_| {
                SocketAddr::from(([127, 0, 0, 1], 11080))
            }),
            routing_policy: RoutingPolicy::StickyPerFlow { hash_seed: 0, ttl_secs: 300 },
            pool: TunnelPoolConfig::default(),
            tun_api_url: None,
        }
    }
}

/// A running subscriber connector: the assembled chain set, its router, and
/// the background tasks serving the local proxy (and, optionally, TUN).
pub struct SubscriberClient {
    pub chain_set: Arc<ChainSet>,
    pub router: Arc<dyn Router>,
    pub socks5_bind: SocketAddr,
    /// Every UDP-shaped entrance from the artefact, in artefact order.
    /// Never mixed into `chain_set` — see `crate::udp_dial`'s module doc.
    /// SOCKS5 UDP ASSOCIATE uses the first entry for which
    /// [`UdpEntranceHandle::is_dialable`] is `true`; empty means ASSOCIATE
    /// is refused cleanly (`UdpAssociateSupport::Unsupported`).
    pub udp_entrances: Vec<Arc<UdpEntranceHandle>>,
    socks5_task: tokio::task::JoinHandle<()>,
    _tun_feed: Option<TunFeed>,
}

impl SubscriberClient {
    /// Validate `artefact`, build one chain per entrance, and stand up the
    /// local SOCKS5 listener (plus, if configured, the TUN feed).
    ///
    /// Uses [`TunnelPool::new_lazy`] for every entrance — zero dials at
    /// construction, the first leased connection triggers the first dial.
    /// This keeps `build` itself free of any network dependency (every
    /// entrance may be unreachable at build time without this call failing)
    /// and matches the pool's own "fallback role" semantics already used
    /// elsewhere in this workspace.
    pub async fn build(
        artefact: &EntranceArtefact,
        options: SubscriberClientOptions,
    ) -> Result<Self, SubscriberClientError> {
        artefact.validate()?;

        if let ArtefactMode::Country(code) = &artefact.mode {
            warn!(
                country = %code,
                "entrance artefact requests a named egress country, but no dial-path plumbing \
                 honours it yet (thin-customer-connector-2026-09-17.md §8 #6) — applying as a \
                 named no-op, NOT full-auto"
            );
        }

        let client_static_priv = artefact.client_static_priv()?;
        let mesh_id = artefact.mesh_id.as_bytes().to_vec();

        // Permanently empty and never written — see the module doc's
        // dependency-boundary note on peer_table.
        let peer_table = Arc::new(ArcSwap::from_pointee(PeerTable::empty()));
        let credential_params = Arc::new(ArcSwap::from_pointee(None));
        let token_pool = Arc::new(ArcSwap::from_pointee(TokenPool::default()));
        let ring_credential = Arc::new(ArcSwap::from_pointee(None));
        let capability_tokens = capability_pool::null_capability_cell();

        let chain_map: Arc<DashMap<ChainId, Arc<ChainHandle>>> = Arc::new(DashMap::new());
        let mut udp_entrances: Vec<Arc<UdpEntranceHandle>> = Vec::new();

        for (i, entrance) in artefact.entrances.iter().enumerate() {
            // UDP-shaped entrance: never enters `chain_set`/`TunnelPool` —
            // see `crate::udp_dial`'s own module doc for why — and never
            // mixes with the TCP CONNECT router. SOCKS5 CONNECT only ever
            // reads `chain_set` below; SOCKS5 UDP ASSOCIATE only ever reads
            // `udp_entrances`. Router rule enforced structurally: the two
            // paths share no state.
            if entrance.camouflage.is_udp() {
                let handle = Arc::new(UdpEntranceHandle::try_new(
                    entrance_chain_id(i),
                    entrance,
                    mesh_id.clone(),
                    client_static_priv,
                )?);
                if !handle.is_dialable() {
                    warn!(
                        chain_id = %handle.chain_id,
                        camouflage = handle.camouflage().label(),
                        "entrance is UDP-shaped but this client's UDP dial only supports \
                         RawUdp (no outer wrapper) — SOCKS5 UDP ASSOCIATE will skip it"
                    );
                }
                udp_entrances.push(handle);
                continue;
            }

            let chain_cfg = build_chain_config(i, entrance);
            let chain_id = ChainId(chain_cfg.chain_id.clone());

            let ep_config = MirageEndpointConfig {
                transport: TransportKind::Tcp,
                auth: AuthMode::Blake3,
                camouflage: entrance.camouflage.clone(),
                byte_cipher: None,
                byte_cipher_mode: None,
            };
            let proto: Box<dyn TcpProtocol> =
                Box::new(StaticKeyProtocol::new(ep_config, mesh_id.clone(), client_static_priv));
            let chain_protocols = Arc::new(ProtocolChain::new(vec![proto]));

            let pool = TunnelPool::new_lazy(
                chain_cfg.clone(),
                chain_protocols,
                options.pool.clone(),
                mesh_id.clone(),
                Arc::clone(&peer_table),
                Arc::clone(&credential_params),
                Arc::clone(&token_pool),
                Arc::clone(&ring_credential),
                Arc::clone(&capability_tokens),
                false, // credential_gated — this crate never presents one, see module doc
            )
            .await?;

            let handle = Arc::new(ChainHandle {
                id: chain_id.clone(),
                config: Arc::new(ArcSwap::from_pointee(chain_cfg)),
                pool,
                mux: None, // v1 scope: TunnelPool-only per entrance, see crate CLAUDE.md
                metrics: ChainMetrics::new(),
                state: Arc::new(ArcSwap::from_pointee(ChainState::Active)),
            });
            chain_map.insert(chain_id, handle);
        }

        let chain_set = Arc::new(ChainSet {
            chains: chain_map,
            prewarm: PrewarmStrategy::default(),
            peer_table,
            peer_table_notify: Arc::new(Notify::new()),
            mesh_id,
            probe_config: HealthProbeConfig::default(),
        });
        chain_set.spawn_health_probes();

        let router = RouterImpl::new(Arc::clone(&chain_set), options.routing_policy);

        let all_pools: HashMap<ChainId, Arc<TunnelPool>> = chain_set
            .chains
            .iter()
            .map(|e| (e.key().clone(), Arc::clone(&e.value().pool)))
            .collect();
        let primary_pool = chain_set
            .chains
            .iter()
            .next()
            .map(|e| Arc::clone(&e.value().pool))
            .ok_or(SubscriberClientError::NoUsableEntrance)?;

        // SOCKS5 UDP ASSOCIATE: the first dialable (RawUdp-camouflage) UDP
        // entrance, if any — see `crate::udp_dial`'s module doc. Zero UDP
        // entrances, or none dialable, means ASSOCIATE is refused cleanly
        // rather than falling through to `Socks5Server`'s pool/mux/raw
        // fallback, which is not shaped for this crate's TCP-entrance pools.
        let udp_associate_support = udp_entrances
            .iter()
            .find(|h| h.is_dialable())
            .cloned()
            .map(|h| UdpAssociateSupport::DatagramEntrance(h as Arc<dyn DatagramEntranceDialer>))
            .unwrap_or(UdpAssociateSupport::Unsupported);

        let socks5 = Socks5Server::new_with_router(options.socks5_bind, Arc::clone(&router), all_pools)
            .with_udp_associate(udp_associate_support);

        // `run()`'s own `_config: Arc<ClientConfig>` parameter is unused in
        // its implementation (kept for API parity with the non-router
        // constructors) — a minimal placeholder is enough. `routing`
        // (all-empty `RoutingConfig`) means every flow decides `Chain`, never
        // `Direct`: the router-driven path is the only one this crate uses.
        let client_config = Arc::new(placeholder_client_config()?);
        let routing_table = Arc::new(RoutingTable::new(&RoutingConfig::default()));

        let socks5_bind = options.socks5_bind;
        info!(
            bind = %socks5_bind,
            entrances = chain_set.chains.len(),
            udp_entrances = udp_entrances.len(),
            "subscriber-client: SOCKS5 listener starting"
        );
        let socks5_task = tokio::spawn(async move {
            if let Err(e) = socks5.run(primary_pool, None, client_config, routing_table).await {
                tracing::error!(error = %e, "subscriber-client: SOCKS5 listener exited");
            }
        });

        let tun_feed = match options.tun_api_url {
            Some(url) => {
                info!(tun_api_url = %url, "subscriber-client: feeding TUN chain registry");
                Some(TunFeed::start(url, socks5_bind))
            }
            None => None,
        };

        Ok(Self {
            chain_set,
            router,
            socks5_bind,
            udp_entrances,
            socks5_task,
            _tun_feed: tun_feed,
        })
    }

    /// Abort the SOCKS5 accept loop. The TUN feed's watcher task is aborted
    /// on drop (`TunFeed`'s `JoinHandle` is dropped, which detaches rather
    /// than aborts — the last chain state it pushed simply stops updating).
    pub fn shutdown(&self) {
        self.socks5_task.abort();
    }
}

fn placeholder_client_config() -> Result<ClientConfig, SubscriberClientError> {
    serde_json::from_value(serde_json::json!({
        "client_id": "subscriber-client",
        "chains": [],
        "routing": {},
        "psk": null,
    }))
    .map_err(SubscriberClientError::ClientConfig)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artefact::{ArtefactEntrance, EntranceArtefact};
    use mirage_core::config::TransportCamouflage;

    fn sample_artefact(n_entrances: usize) -> EntranceArtefact {
        let entrances = (0..n_entrances)
            .map(|i| ArtefactEntrance {
                addr: format!("127.0.0.1:{}", 20000 + i),
                server_pubkey_hex: format!("{:02x}", i).repeat(32),
                camouflage: TransportCamouflage::None,
            })
            .collect();
        EntranceArtefact {
            v: crate::artefact::CURRENT_VERSION,
            private_key_hex: "11".repeat(32),
            mesh_id: "MESH2".to_owned(),
            entrances,
            mode: ArtefactMode::FullAuto,
            label: "pilot-1".to_owned(),
            issued_at_ms: 1_726_000_000_000,
        }
    }

    // ── a refusal when the artefact carries no usable entrance ─────────────

    #[tokio::test]
    async fn build_refuses_an_artefact_with_no_entrances() {
        let artefact = sample_artefact(0);
        let bind: SocketAddr = "127.0.0.1:0".parse().expect("valid loopback addr");
        let opts = SubscriberClientOptions { socks5_bind: bind, ..Default::default() };
        match SubscriberClient::build(&artefact, opts).await {
            Err(SubscriberClientError::Artefact(ArtefactError::NoEntrances)) => {}
            Err(other) => panic!("expected Artefact(NoEntrances), got a different error: {other}"),
            Ok(_) => panic!("expected a refusal, build succeeded"),
        }
    }

    // ── build assembles one chain per entrance, all Active, all lazy ───────

    #[tokio::test]
    async fn build_assembles_one_chain_handle_per_entrance() {
        let artefact = sample_artefact(3);
        let bind: SocketAddr = "127.0.0.1:0".parse().expect("valid loopback addr");
        let opts = SubscriberClientOptions { socks5_bind: bind, ..Default::default() };
        let client = SubscriberClient::build(&artefact, opts).await.expect("build must succeed");

        assert_eq!(client.chain_set.chains.len(), 3);
        for i in 0..3 {
            let id = ChainId(format!("entrance-{i}"));
            let handle = client.chain_set.chains.get(&id).expect("chain must be present");
            assert!(handle.is_usable(), "freshly built chain must start Active/usable");
        }
        assert!(client.udp_entrances.is_empty(), "an all-TCP artefact must yield no UDP entrances");
        client.shutdown();
    }

    // ── mixed artefact: TCP entrances build chain_set, UDP entrances don't ─

    fn mixed_artefact() -> EntranceArtefact {
        EntranceArtefact {
            v: crate::artefact::CURRENT_VERSION,
            private_key_hex: "11".repeat(32),
            mesh_id: "MESH2".to_owned(),
            entrances: vec![
                ArtefactEntrance {
                    addr: "172.86.76.34:80".to_owned(),
                    server_pubkey_hex: "22".repeat(32),
                    camouflage: TransportCamouflage::WebSocket {
                        path: "/ws".to_owned(),
                        host: "cdn.example.com".to_owned(),
                    },
                },
                ArtefactEntrance {
                    addr: "172.86.76.34:853".to_owned(),
                    server_pubkey_hex: "33".repeat(32),
                    camouflage: TransportCamouflage::RawUdp {
                        bind_addr: "0.0.0.0:853".parse().expect("valid addr"),
                    },
                },
            ],
            mode: ArtefactMode::FullAuto,
            label: "pilot-1".to_owned(),
            issued_at_ms: 1_726_000_000_000,
        }
    }

    /// Live evidence shape (2026-09-23): `172.86.76.34:80` (web_socket) +
    /// `172.86.76.34:853` (raw_udp) — the TCP face must build exactly one
    /// `chain_set` entry, the UDP face must build exactly one
    /// `udp_entrances` entry, and neither list contains the other's face.
    #[tokio::test]
    async fn build_splits_a_mixed_artefact_into_tcp_chain_set_and_udp_entrance() {
        let artefact = mixed_artefact();
        let bind: SocketAddr = "127.0.0.1:0".parse().expect("valid loopback addr");
        let opts = SubscriberClientOptions { socks5_bind: bind, ..Default::default() };
        let client = SubscriberClient::build(&artefact, opts).await.expect("build must succeed");

        assert_eq!(client.chain_set.chains.len(), 1, "only the TCP-shaped entrance builds a chain");
        assert!(
            client.chain_set.chains.get(&ChainId("entrance-0".to_owned())).is_some(),
            "the web_socket entrance (index 0) must be the TCP chain"
        );

        assert_eq!(client.udp_entrances.len(), 1, "only the raw_udp entrance builds a UDP entrance");
        assert_eq!(client.udp_entrances[0].chain_id, "entrance-1");
        assert!(client.udp_entrances[0].is_dialable(), "raw_udp is the one dialable camouflage");

        client.shutdown();
    }

    // ── SOCKS5 UDP ASSOCIATE wiring, end to end over a real TCP connection ─

    /// A real, currently-free `127.0.0.1` port — grabbed from the OS by
    /// binding then immediately releasing, since `SubscriberClient::build`
    /// never reports back the actual bound address for a `:0` request (its
    /// own `socks5_bind` field is `options.socks5_bind` verbatim).
    async fn free_local_addr() -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral");
        listener.local_addr().expect("local addr")
    }

    /// Connect to `bind`, retrying briefly: `Socks5Server::run`'s
    /// `TcpListener::bind` happens inside a `tokio::spawn`ed task, so it may
    /// not have run yet the instant `SubscriberClient::build` returns.
    async fn connect_with_retry(bind: SocketAddr) -> tokio::net::TcpStream {
        for _ in 0..200 {
            if let Ok(stream) = tokio::net::TcpStream::connect(bind).await {
                return stream;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("SOCKS5 listener at {bind} never came up");
    }

    /// Reads the SOCKS5 reply's VER/REP bytes from a freshly-connected
    /// stream — enough to distinguish `SOCKS5_REP_SUCCESS` (0x00) from
    /// `SOCKS5_REP_GENERAL_FAILURE` (0x01) without decoding the rest of the
    /// (variable-length) bound-address reply.
    async fn socks5_udp_associate_rep_byte(bind: SocketAddr) -> u8 {
        let mut stream = connect_with_retry(bind).await;

        // Greeting: version 5, one method, no-auth.
        tokio::io::AsyncWriteExt::write_all(&mut stream, &[0x05, 0x01, 0x00])
            .await
            .expect("write greeting");
        let mut greeting_reply = [0u8; 2];
        tokio::io::AsyncReadExt::read_exact(&mut stream, &mut greeting_reply)
            .await
            .expect("read greeting reply");
        assert_eq!(greeting_reply, [0x05, 0x00], "server must accept no-auth");

        // Request: CMD=UDP_ASSOCIATE(0x03), ATYP=IPv4, addr 0.0.0.0:0 (RFC 1928 §4
        // — the client's own declared address, ignored by this server).
        tokio::io::AsyncWriteExt::write_all(
            &mut stream,
            &[0x05, 0x03, 0x00, 0x01, 0, 0, 0, 0, 0, 0],
        )
        .await
        .expect("write UDP ASSOCIATE request");

        let mut reply_header = [0u8; 2];
        tokio::io::AsyncReadExt::read_exact(&mut stream, &mut reply_header)
            .await
            .expect("read UDP ASSOCIATE reply header");
        assert_eq!(reply_header[0], 0x05, "reply must be SOCKS5");
        reply_header[1]
    }

    /// No UDP entrance in the artefact → ASSOCIATE must be refused cleanly
    /// (`SOCKS5_REP_GENERAL_FAILURE`), never a false success that then hangs
    /// or silently mis-dials over a TCP-shaped entrance chain.
    #[tokio::test]
    async fn associate_is_refused_cleanly_with_no_udp_entrance() {
        let artefact = sample_artefact(2); // all-TCP, per `sample_artefact`.
        let bind = free_local_addr().await;
        let opts = SubscriberClientOptions { socks5_bind: bind, ..Default::default() };
        let client = SubscriberClient::build(&artefact, opts).await.expect("build must succeed");
        assert!(client.udp_entrances.is_empty());

        let rep = socks5_udp_associate_rep_byte(client.socks5_bind).await;
        assert_eq!(rep, 0x01, "no UDP entrance must reply SOCKS5_REP_GENERAL_FAILURE, not success");

        client.shutdown();
    }

    /// A dialable (raw_udp) UDP entrance in the artefact → ASSOCIATE must
    /// reach `SOCKS5_REP_SUCCESS`, proving the `UdpAssociateSupport::
    /// DatagramEntrance` wiring is reached end to end over a real SOCKS5
    /// connection (the datagram relay itself is proven separately by
    /// `crate::udp_dial`'s own round-trip test).
    #[tokio::test]
    async fn associate_succeeds_when_a_dialable_udp_entrance_exists() {
        let artefact = mixed_artefact();
        let bind = free_local_addr().await;
        let opts = SubscriberClientOptions { socks5_bind: bind, ..Default::default() };
        let client = SubscriberClient::build(&artefact, opts).await.expect("build must succeed");
        assert_eq!(client.udp_entrances.len(), 1);

        let rep = socks5_udp_associate_rep_byte(client.socks5_bind).await;
        assert_eq!(rep, 0x00, "a dialable UDP entrance must reply SOCKS5_REP_SUCCESS");

        client.shutdown();
    }
}
