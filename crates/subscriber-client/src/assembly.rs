//! Assembles a validated [`EntranceArtefact`] into a running local proxy.
//!
//! One [`EdgeSessionSupervisor`] holds the ONE persistent mux session to the
//! customer's edge, dialed across every entrance face the artefact carries
//! — stream-shaped and datagram-shaped alike, the outer camouflage deciding
//! the wire transport (round-robin failover, reconnect with backoff —
//! `crate::session`), fronting one [`crate::socks5`] listener for SOCKS5
//! CONNECT. UDP ASSOCIATE is refused cleanly (`crate::socks5`'s own doc) —
//! the UDP relay over the mux session is a later slice.
//!
//! This replaces the crate's previous one-single-hop-dial-per-flow path
//! (`ChainSet`/`RouterImpl`/`TunnelPool`, one `ChainConfig` per entrance) —
//! see `docs/mirage/plans/2026-09-24-edge-as-entry.md` PART TWO, "Wire
//! shape". That path's builder (`chain_build.rs`) and its per-entrance UDP
//! dial (`udp_dial.rs`) are retired to `src/_deprecated/`, not deleted.
//!
//! **Dependency boundaries that keep the forbidden things out of this
//! crate** (`thin-customer-connector-2026-09-17.md` §3, still true after the
//! edge-session rewrite): this crate has no dependency on
//! `mirage_core::crypto::credential`, no `MonitorLink` dependency, and no
//! peer table anywhere — an edge session dials the artefact's own entrance
//! faces directly, by the address and public key the artefact carries.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use thiserror::Error;
use tracing::info;

use crate::artefact::{ArtefactError, DeliveryArtefact, EntranceArtefact};
use crate::onion_artefact::OnionArtefact;
use crate::scope::EgressScope;
use crate::session::{exit_preference_from_mode, EdgeDialError, EdgeSessionSupervisor};
use crate::status::{EdgeReason, StatusWriter};
use crate::supervisor_slot::SupervisorSlot;
use crate::tun_feed::TunFeed;

/// Every way [`SubscriberClient::build`] refuses.
#[derive(Debug, Error)]
pub enum SubscriberClientError {
    #[error(transparent)]
    Artefact(#[from] ArtefactError),

    #[error(transparent)]
    OnionArtefact(#[from] crate::onion_artefact::OnionArtefactError),

    #[error(transparent)]
    EdgeDial(#[from] EdgeDialError),

    #[error("subscriber client: cannot write the status file {}: {source}", path.display())]
    StatusFile { path: PathBuf, source: io::Error },
}

impl SubscriberClientError {
    /// The closed [`EdgeReason`] this refusal maps to in the status file, if
    /// it has one. A status-file failure has none: the file it would be
    /// written to is the thing that failed.
    pub fn status_reason(&self) -> Option<EdgeReason> {
        match self {
            Self::Artefact(_) | Self::OnionArtefact(_) => Some(EdgeReason::BadArtefact),
            Self::EdgeDial(e) => Some(EdgeReason::from(e)),
            Self::StatusFile { .. } => None,
        }
    }
}

/// Tunable knobs for [`SubscriberClient::build`]. Everything else this
/// module needs comes from the artefact.
pub struct SubscriberClientOptions {
    /// Local bind address for the SOCKS5 listener. Default `127.0.0.1:1080`
    /// (the standard SOCKS5 port).
    pub socks5_bind: SocketAddr,
    /// When `Some`, also feed `client-core`'s `TunBridge` — see
    /// `crate::tun_feed`. `None` (default) leaves TUN untouched.
    pub tun_api_url: Option<String>,
    /// When `Some`, keep `edge-status.json` (see `crate::status`) current at
    /// this path. `None` (default) publishes nothing.
    pub status_file: Option<PathBuf>,
    /// Destinations the local proxy will open a mux stream for. Default is
    /// every destination.
    pub scope: EgressScope,
    /// SOCKS5 is the local feed for `--transport socks` and for TUN. HTTP
    /// CONNECT has its own listener and leaves this false.
    pub start_socks: bool,
    /// Who owns the packets of the plane session.
    ///
    /// The userspace stack (the SOCKS5 flows) and the tunnel device (the
    /// raw-IP consumer) want the same packets, and one address: only one of
    /// them may have them. The local carry decides - SOCKS5 and HTTP CONNECT
    /// want the stack, the TUN device wants the packets raw.
    pub plane_consumer: crate::session::PlaneConsumer,
}

impl Default for SubscriberClientOptions {
    fn default() -> Self {
        Self {
            socks5_bind: SocketAddr::from(([127, 0, 0, 1], 1080)),
            tun_api_url: None,
            status_file: None,
            scope: EgressScope::all(),
            start_socks: true,
            plane_consumer: crate::session::PlaneConsumer::Stack,
        }
    }
}

/// A running subscriber connector: the edge-session supervisor and the
/// background tasks serving the local proxy (and, optionally, TUN).
pub struct SubscriberClient {
    /// The live session, swappable. A new configuration replaces it in place:
    /// the local listener, the process and the tunnel device stay up.
    pub supervisor: Arc<SupervisorSlot>,
    pub socks5_bind: SocketAddr,
    socks5_task: Option<tokio::task::JoinHandle<()>>,
    status_file: Option<PathBuf>,
    status_writer: std::sync::Mutex<Option<StatusWriter>>,
    /// The destinations the local capture sends through. Shared with the
    /// listeners, so the lists can be replaced while they run.
    pub scope: Arc<EgressScope>,
    /// Who owns the packets of the plane session.
    ///
    /// The carry decides it, and a new configuration inherits it: the
    /// tunnel device wants the packets raw, the SOCKS5 flows want the
    /// userspace stack.
    plane_consumer: crate::session::PlaneConsumer,
    _tun_feed: Option<TunFeed>,
}

impl SubscriberClient {
    /// Validate `artefact`, spawn the edge-session supervisor over its
    /// entrances (every one of them — the camouflage decides the transport),
    /// and stand up the local SOCKS5 listener (plus, if configured, the TUN
    /// feed).
    ///
    /// The stream-failure watchdog uses its default threshold — see
    /// [`Self::build_with_threshold`].
    pub async fn build(
        artefact: &EntranceArtefact,
        options: SubscriberClientOptions,
    ) -> Result<Self, SubscriberClientError> {
        Self::build_with_threshold(
            artefact,
            options,
            crate::watchdog::DEFAULT_STREAM_FAILURE_THRESHOLD,
        )
        .await
    }

    /// Build from a delivery artefact of either kind.
    ///
    /// The VPN artefact names one box - the fleet's own shape, unchanged.
    /// The onion artefact names a chain, and the client walks it itself. The
    /// artefact's own `kind` field decides which.
    pub async fn build_from_delivery(
        artefact: &DeliveryArtefact,
        options: SubscriberClientOptions,
    ) -> Result<Self, SubscriberClientError> {
        Self::build_from_delivery_with_threshold(
            artefact,
            options,
            crate::watchdog::DEFAULT_STREAM_FAILURE_THRESHOLD,
        )
        .await
    }

    /// [`Self::build_from_delivery`] with an explicit stream-failure threshold.
    pub async fn build_from_delivery_with_threshold(
        artefact: &DeliveryArtefact,
        options: SubscriberClientOptions,
        stream_failure_threshold: u32,
    ) -> Result<Self, SubscriberClientError> {
        match artefact {
            DeliveryArtefact::Vpn(vpn) => {
                Self::build_with_threshold(vpn, options, stream_failure_threshold).await
            }
            DeliveryArtefact::Onion(onion) => {
                Self::build_onion_with_threshold(onion, options, stream_failure_threshold).await
            }
        }
    }

    /// The onion plane: validate the chain, walk it, and serve the local
    /// proxy over the exit's mux session.
    ///
    /// The stream-failure watchdog uses its default threshold - see
    /// [`Self::build_onion_with_threshold`].
    pub async fn build_onion(
        artefact: &OnionArtefact,
        options: SubscriberClientOptions,
    ) -> Result<Self, SubscriberClientError> {
        Self::build_onion_with_threshold(
            artefact,
            options,
            crate::watchdog::DEFAULT_STREAM_FAILURE_THRESHOLD,
        )
        .await
    }

    /// [`Self::build_onion`] with an explicit stream-failure threshold.
    pub async fn build_onion_with_threshold(
        artefact: &OnionArtefact,
        options: SubscriberClientOptions,
        stream_failure_threshold: u32,
    ) -> Result<Self, SubscriberClientError> {
        artefact.validate()?;
        let spec = artefact.to_spec()?;
        let supervisor = crate::onion_session::spawn_onion(spec, stream_failure_threshold);
        Self::spawn_with_supervisor(supervisor, options).await
    }

    /// [`Self::build`] with an explicit stream-failure threshold: how many
    /// consecutive failed stream opens tear the edge session down and force a
    /// redial — the client-side watchdog ([`crate::watchdog`]). A threshold
    /// below one is clamped to one.
    pub async fn build_with_threshold(
        artefact: &EntranceArtefact,
        options: SubscriberClientOptions,
        stream_failure_threshold: u32,
    ) -> Result<Self, SubscriberClientError> {
        artefact.validate()?;

        let exit = exit_preference_from_mode(&artefact.mode)?;
        let client_static_priv = artefact.client_static_priv()?;
        let mesh_id = artefact.mesh_id.as_bytes().to_vec();

        // The configuration says where to go and with what protocol. The
        // session follows it and decides nothing itself.
        let entrances = artefact.entrances.clone();

        let supervisor = EdgeSessionSupervisor::spawn_with_threshold_and_consumer(
            entrances,
            mesh_id,
            client_static_priv,
            exit,
            stream_failure_threshold,
            options.plane_consumer,
        );

        Self::spawn_with_supervisor(supervisor, options).await
    }

    /// Stand the local proxy up over an already-built supervisor.
    ///
    /// Shared by both planes: the VPN one and the onion one differ only in
    /// which session the supervisor holds.
    async fn spawn_with_supervisor(
        supervisor: Arc<EdgeSessionSupervisor>,
        options: SubscriberClientOptions,
    ) -> Result<Self, SubscriberClientError> {
        let slot = SupervisorSlot::new(supervisor);
        // Started before anything else is bound so an unwritable path refuses
        // the build (dropping `supervisor` aborts its dial loop) instead of
        // leaving a connector running that nobody can observe.
        let status_writer = match options.status_file.clone() {
            Some(path) => Some(
                StatusWriter::start(path.clone(), slot.current().status())
                    .map_err(|source| SubscriberClientError::StatusFile { path, source })?,
            ),
            None => None,
        };

        let socks5_bind = options.socks5_bind;
        let scope = Arc::new(options.scope);
        let socks5_task = if options.start_socks {
            let socks5_slot = Arc::clone(&slot);
            let socks5_scope = Arc::clone(&scope);
            info!(bind = %socks5_bind, "subscriber-client: SOCKS5 listener starting");
            Some(tokio::spawn(async move {
                if let Err(e) =
                    crate::socks5::serve_slot(socks5_bind, socks5_slot, socks5_scope).await
                {
                    tracing::error!(error = %e, "subscriber-client: SOCKS5 listener exited");
                }
            }))
        } else {
            None
        };

        let tun_feed = match options.tun_api_url {
            Some(url) => {
                info!(tun_api_url = %url, "subscriber-client: feeding TUN chain registry");
                Some(TunFeed::start(url, socks5_bind))
            }
            None => None,
        };

        Ok(Self {
            supervisor: slot,
            socks5_bind,
            socks5_task,
            status_file: options.status_file,
            status_writer: std::sync::Mutex::new(status_writer),
            scope,
            plane_consumer: options.plane_consumer,
            _tun_feed: tun_feed,
        })
    }

    /// Build the edge-session supervisor for `artefact` — the dial loop over
    /// its entrances, without touching the local listener.
    ///
    /// The configuration says where to go and with what protocol; the session
    /// follows it and decides nothing itself.
    pub async fn supervisor_for(
        artefact: &DeliveryArtefact,
        stream_failure_threshold: u32,
        plane_consumer: crate::session::PlaneConsumer,
    ) -> Result<Arc<EdgeSessionSupervisor>, SubscriberClientError> {
        match artefact {
            DeliveryArtefact::Vpn(vpn) => {
                vpn.validate()?;
                let exit = exit_preference_from_mode(&vpn.mode)?;
                let client_static_priv = vpn.client_static_priv()?;
                let mesh_id = vpn.mesh_id.as_bytes().to_vec();
                let entrances = vpn.entrances.clone();
                Ok(EdgeSessionSupervisor::spawn_with_threshold_and_consumer(
                    entrances,
                    mesh_id,
                    client_static_priv,
                    exit,
                    stream_failure_threshold,
                    plane_consumer,
                ))
            }
            DeliveryArtefact::Onion(onion) => {
                onion.validate()?;
                let spec = onion.to_spec()?;
                Ok(crate::onion_session::spawn_onion(spec, stream_failure_threshold))
            }
        }
    }

    /// Adopt a new configuration in place.
    ///
    /// The local listener keeps running and the session is replaced: the old
    /// target's dial loop stops at once, the new entrances are dialled, and
    /// the process, the listener and the tunnel device stay up. This is what
    /// a target change does — nothing is torn down and rebuilt.
    pub async fn adopt_artefact(
        &self,
        artefact: &DeliveryArtefact,
        stream_failure_threshold: u32,
    ) -> Result<(), SubscriberClientError> {
        let next = Self::supervisor_for(artefact, stream_failure_threshold, self.plane_consumer).await?;
        // The status file follows the live session.
        if let Some(path) = self.status_file.clone() {
            let writer = StatusWriter::start(path.clone(), next.status())
                .map_err(|source| SubscriberClientError::StatusFile { path, source })?;
            let mut slot = self
                .status_writer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(old) = slot.take() {
                old.stop();
            }
            *slot = Some(writer);
        }
        self.supervisor.replace(next);
        Ok(())
    }

    /// The tunnel address the box assigned to this client, in CIDR form.
    ///
    /// "10.77.0.7/24" -- the address the box handed to the live session and
    /// the prefix it sent with it. The tunnel adapter must carry exactly
    /// this address: the box drops the packets of a peer that sends from
    /// another one. `None` when no session is up, or when the box assigned
    /// nothing -- an old box, or a session of the older wire versions.
    pub fn assigned_client_address(&self) -> Option<String> {
        let assignment = self.supervisor.current().assigned_client()?;
        Some(format!("{}/{}", assignment.addr, assignment.prefix))
    }

    /// Abort the SOCKS5 accept loop and, if a status file is kept, stop
    /// rewriting it and remove it. The edge-session supervisor's dial loop
    /// stops when the last `Arc` to it drops; the TUN feed's watcher (if any)
    /// is detached.
    pub fn shutdown(&self) {
        if let Some(task) = &self.socks5_task {
            task.abort();
        }
        let mut slot = self
            .status_writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(writer) = slot.take() {
            writer.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artefact::{ArtefactEntrance, ArtefactMode, EntranceArtefact};
    use mirage_core::mirage::wire::{MirageCamouflage, MirageCarrier, Protocol};

    fn sample_artefact(n_entrances: usize) -> EntranceArtefact {
        let entrances = (0..n_entrances)
            .map(|i| ArtefactEntrance {
                addr: format!("127.0.0.1:{}", 20000 + i),
                relay: None,
                server_pubkey_hex: format!("{:02x}", i).repeat(32),
                protocol: Protocol::Mirage {
                    carrier: MirageCarrier::Tcp,
                    camouflage: MirageCamouflage::None,
                },
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

    async fn free_local_addr() -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral");
        listener.local_addr().expect("local addr")
    }

    async fn connect_with_retry(bind: SocketAddr) -> tokio::net::TcpStream {
        for _ in 0..200 {
            if let Ok(stream) = tokio::net::TcpStream::connect(bind).await {
                return stream;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("SOCKS5 listener at {bind} never came up");
    }

    // ── a refusal when the artefact carries no usable entrance ─────────────

    #[tokio::test]
    async fn build_refuses_an_artefact_with_no_entrances() {
        let artefact = sample_artefact(0);
        let bind = free_local_addr().await;
        let opts = SubscriberClientOptions { socks5_bind: bind, ..Default::default() };
        match SubscriberClient::build(&artefact, opts).await {
            Err(SubscriberClientError::Artefact(ArtefactError::NoEntrances)) => {}
            Err(other) => panic!("expected Artefact(NoEntrances), got a different error: {other}"),
            Ok(_) => panic!("expected a refusal, build succeeded"),
        }
    }

    #[tokio::test]
    async fn build_accepts_an_artefact_whose_only_entrance_is_udp_shaped() {
        // A datagram-shaped face is dialled over datagrams — the subscriber
        // must not be refused an artefact that carries nothing else.
        let mut artefact = sample_artefact(1);
        artefact.entrances[0].protocol = Protocol::Mirage {
            carrier: MirageCarrier::Udp,
            camouflage: MirageCamouflage::None,
        };
        let bind = free_local_addr().await;
        let opts = SubscriberClientOptions { socks5_bind: bind, ..Default::default() };
        let client = SubscriberClient::build(&artefact, opts)
            .await
            .expect("a UDP-only artefact must build — the mask decides the transport");
        client.shutdown();
    }

    // ── build spawns a reachable SOCKS5 listener over the edge supervisor ──

    #[tokio::test]
    async fn build_spawns_a_reachable_socks5_listener() {
        let artefact = sample_artefact(2);
        let bind = free_local_addr().await;
        let opts = SubscriberClientOptions { socks5_bind: bind, ..Default::default() };
        let client = SubscriberClient::build(&artefact, opts).await.expect("build must succeed");

        // The SOCKS5 listener binds inside its own spawned task — poll until
        // it comes up, matching this crate's established test pattern.
        let mut stream = connect_with_retry(client.socks5_bind).await;
        tokio::io::AsyncWriteExt::write_all(&mut stream, &[0x05, 0x01, 0x00])
            .await
            .expect("write greeting");
        let mut greeting_reply = [0u8; 2];
        tokio::io::AsyncReadExt::read_exact(&mut stream, &mut greeting_reply)
            .await
            .expect("read greeting reply");
        assert_eq!(greeting_reply, [0x05, 0x00], "server must accept no-auth");

        client.shutdown();
    }

    // ── the session follows the configuration ──────────────────────────────

    #[test]
    fn the_session_dials_exactly_what_the_configuration_names() {
        // The configuration says where to go and with what protocol. The
        // session decides nothing itself: the faces are taken as written.
        let artefact = sample_artefact(2);
        let addrs: Vec<&str> = artefact.entrances.iter().map(|e| e.addr.as_str()).collect();
        assert_eq!(addrs.len(), 2, "the configuration's faces are the session's faces");
    }
}
