//! Assembles a validated [`EntranceArtefact`] into a running local proxy.
//!
//! One [`ChainHandle`] per entrance (`crate::chain_build`), each dialed by
//! its own [`StaticKeyProtocol`] (`crate::dial`) presenting the subscriber's
//! persistent identity, registered into one [`ChainSet`], fronted by a
//! [`RouterImpl`] (default `StickyPerFlow` — a live flow stays on the face it
//! started on; fresh flows spread across faces, per
//! `thin-customer-connector-2026-09-17.md` §5) driving one shared
//! [`Socks5Server`]. Everything below the artefact translation is reused
//! machinery from `mirage-client-core`, unmodified.
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
    socks5::Socks5Server,
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
use crate::chain_build::build_chain_config;
use crate::dial::StaticKeyProtocol;
use crate::tun_feed::TunFeed;

/// Every way [`SubscriberClient::build`] refuses.
#[derive(Debug, Error)]
pub enum SubscriberClientError {
    #[error(transparent)]
    Artefact(#[from] ArtefactError),

    #[error(transparent)]
    Mirage(#[from] MirageError),

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

        for (i, entrance) in artefact.entrances.iter().enumerate() {
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

        let socks5 = Socks5Server::new_with_router(options.socks5_bind, Arc::clone(&router), all_pools);

        // `run()`'s own `_config: Arc<ClientConfig>` parameter is unused in
        // its implementation (kept for API parity with the non-router
        // constructors) — a minimal placeholder is enough. `routing`
        // (all-empty `RoutingConfig`) means every flow decides `Chain`, never
        // `Direct`: the router-driven path is the only one this crate uses.
        let client_config = Arc::new(placeholder_client_config()?);
        let routing_table = Arc::new(RoutingTable::new(&RoutingConfig::default()));

        let socks5_bind = options.socks5_bind;
        info!(bind = %socks5_bind, entrances = chain_set.chains.len(), "subscriber-client: SOCKS5 listener starting");
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
        client.shutdown();
    }
}
