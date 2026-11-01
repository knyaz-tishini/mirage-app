//! [`EdgeSession`] — the client's own persistent session to its edge.
//!
//! One V9 tunnel (dialed with [`crate::dial::StaticKeyProtocol`], presenting
//! the subscriber's persistent static key exactly as before), one
//! [`EdgeHello`]/[`EdgeWelcome`] exchange, then — on
//! [`EdgeOutcome::Accepted`] — one mux upgrade
//! (`mirage_control_plane::ONION_MUX_MAGIC`-shaped: no magic byte is sent
//! here because the edge upgrades unconditionally after `Accepted`, unlike
//! an onion exit that must be told to expect a mux client; see
//! `docs/mirage/plans/2026-09-24-edge-as-entry.md` PART TWO). Every
//! subsequent logical flow is one mux stream on that SAME session, opened
//! with a [`StreamOpen`] header as its first bytes — never a fresh V9
//! handshake per flow. This replaces the crate's previous
//! one-single-hop-dial-per-flow path (`crate::chain_build`, retired to
//! `crate::_deprecated`).
//!
//! [`EdgeSessionSupervisor`] holds one session up and reconnects with
//! exponential backoff whenever it closes. The edge assigns the session
//! face. This client speaks hello version 4 first, version 3 when the edge
//! refuses version 4, version 2 when the edge refuses version 3, and
//! version 1 when the edge refuses version 2. It
//! picks a probe from every face the artefact carries — stream-shaped and
//! datagram-shaped alike — and fails that probe over onto another face of
//! the SAME transport when the probe itself cannot connect. The version-1
//! fallback is [`EdgeSession::connect_first_available`] from index 0. A
//! version-3 refusal does not skip version 2.
//!
//! **Version 4 is the plane.** A QUIC face is dialled over the mono-protocol
//! QUIC transport ([`crate::dial::PlaneDial`]): ONE connection carries the
//! control wire (this hello and the welcome) AND the whole IP packets, in
//! QUIC datagrams. When the edge's welcome says the session carries whole
//! packets, the session is a plane session - there is no mux, and the
//! packets ride [`EdgeSession::packet_plane`]. Every other camouflage keeps
//! the mux transport of the older versions, byte for byte.
//!
//! **Any face, not only a TCP one.** The subscriber may be handed an
//! entrance wearing any outer camouflage — a stream one or a datagram one —
//! and the camouflage decides the wire transport. The probe is therefore
//! picked from EVERY entrance, and the faces the hello offers are the ones
//! riding the SAME transport as the probed face (the wire format carries
//! bare ports with no transport tag, so the two can never disagree). See
//! [`offered_ports_for_transport`] and `mirage_control_plane::edge_session`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mirage_client_core::relay::TunnelCompat;
use mirage_control_plane::{
    CountryCode, EdgeHello, EdgeOutcome, EdgeRefusal, EdgeSessionError as WireError, EdgeWelcome,
    ExitPreference, StreamKind, StreamOpen, StreamTarget,
};
use mirage_control_plane::edge_session::{
    face_transport, filter_offered_faces_by_transport, ClientAssignment,
};
use mirage_core::config::{AuthMode, MirageEndpointConfig, TransportCamouflage, TransportKind};
use mirage_core::error::MirageError;
use mirage_mux::tokio_compat::{SessionHandle, TokioMuxStream};
use mirage_mux::{MuxError, MuxStream};
use mirage_protocol::plane::{PacketPlane, PlaneConnector};
use mirage_protocol::traits::{TcpProtocol, Tunnel};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};

use crate::artefact::{ArtefactEntrance, ArtefactError, ArtefactMode};
use crate::dial::{quic_alpn, PlaneDial, StaticKeyProtocol};
use crate::status::{EdgeReason, EdgeStatusFile};
use crate::watchdog::{StreamHealth, WatchedStream, DEFAULT_STREAM_FAILURE_THRESHOLD};

/// The wire versions this client speaks. The numbers live in
/// `mirage_control_plane::peer` -- with every other implementation's -- so
/// that the client, the commercial box and the fleet box cannot drift apart.
///
/// Version 1 of this client's edge-session wire -- see [`EdgeHello::version`].
/// A version-2 dial is a separate path ([`EntranceDialer`]); this constant
/// stays the version an old edge still understands.
use mirage_control_plane::peer::{
    MUX_VERSION_V1 as EDGE_SESSION_WIRE_VERSION,
    MUX_VERSION_V2 as EDGE_SESSION_WIRE_VERSION_V2,
    MUX_VERSION_V3 as EDGE_SESSION_WIRE_VERSION_V3,
    PLANE_VERSION as EDGE_SESSION_WIRE_VERSION_V4,
};

/// How long a flow waits before asking for stream credit again.
///
/// The mux waits for the credit on its own (and wakes the flow the moment it
/// is back); this is only the pause between two asks, so a saturated session
/// does not turn into a spin.
const STREAM_CREDIT_RETRY: Duration = Duration::from_millis(250);

/// Mix input for the probe-face start. Not a `rand` source.
static PROBE_DRAW: AtomicU64 = AtomicU64::new(0);

/// A refusal reason with a human-readable [`std::fmt::Display`] impl —
/// `mirage_control_plane::EdgeRefusal` itself only derives `Debug`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefusalDisplay(pub EdgeRefusal);

impl std::fmt::Display for RefusalDisplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            EdgeRefusal::UnsupportedVersion { requested, supported } => write!(
                f,
                "unsupported_version(requested={requested}, supported={supported})"
            ),
            EdgeRefusal::CountryUnavailable { requested } => {
                write!(f, "country_unavailable(requested={requested})")
            }
        }
    }
}

/// Every way dialing an edge entrance fails.
#[derive(Debug, Error)]
pub enum EdgeDialError {
    #[error("edge session: entrance addr {addr:?} does not parse as host:port: {reason}")]
    BadAddr { addr: String, reason: String },

    #[error("edge session: entrance server_pubkey_hex is malformed: {0}")]
    BadServerPubkey(#[from] ArtefactError),

    #[error("edge session: artefact mode carries an invalid country code: {0}")]
    BadCountryCode(WireError),

    #[error(transparent)]
    Mirage(#[from] MirageError),

    #[error("edge session: EdgeWelcome did not parse: {0}")]
    MalformedWelcome(WireError),

    #[error("edge session: tunnel closed by the edge before EdgeWelcome arrived")]
    ClosedBeforeWelcome,

    #[error("edge session: refused by the edge: {0}")]
    Refused(RefusalDisplay),

    #[error("edge session: no entrances to dial")]
    NoEntrances,

    #[error("edge session: EdgeHello encode: {0}")]
    MalformedHello(WireError),

    #[error("edge session: edge assigned port {port} which is not an entrance on the artefact")]
    AssignedFaceMissing { port: u16 },

    #[error("edge session: {count} entrances of one transport exceeds the {max} a version-2 hello can offer")]
    TooManyFaces { count: usize, max: usize },

    #[error("edge session: the onion chain is unusable: {0}")]
    BadOnionChain(String),
}

/// Every way opening a mux stream on a live [`EdgeSession`] fails.
#[derive(Debug, Error)]
pub enum EdgeStreamError {
    #[error("edge stream: invalid target: {0}")]
    BadTarget(WireError),

    #[error("edge stream: mux session error: {0}")]
    Mux(#[from] mirage_mux::MuxError),

    #[error("edge stream: io error writing the StreamOpen header: {0}")]
    Io(#[from] std::io::Error),

    #[error("edge stream: udp datagram frame codec error: {0}")]
    Codec(WireError),

    #[error("edge stream: this exit does not carry datagrams")]
    DatagramsRefused,

    #[error("edge stream: target is {len} bytes - the length-prefixed frame carries at most 65535")]
    TargetTooLong { len: usize },
}

/// How a stream's destination is framed to the far end.
///
/// The two are not interchangeable: the edge session speaks the fleet's own
/// `StreamOpen` header, while an onion exit reads a bare length-prefixed
/// target. Sending the wrong one to the right end is a silent stall, so the
/// framing travels with the session instead of being decided at the call
/// site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamFraming {
    /// The fleet's own `StreamOpen` header — kind plus target. What the edge
    /// session speaks.
    EdgeHeader,
    /// `[2B BE len][target]` — the onion exit's own frame. What a chain's
    /// terminal hop reads on every stream.
    LengthPrefixedTarget,
}

impl StreamFraming {
    /// Write this framing's target frame onto `stream`.
    async fn write_target<W>(&self, stream: &mut W, target: &str) -> Result<(), EdgeStreamError>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        match self {
            Self::EdgeHeader => {
                let open = StreamOpen {
                    kind: StreamKind::Tcp,
                    target: StreamTarget::new(target).map_err(EdgeStreamError::BadTarget)?,
                };
                stream.write_all(&open.encode()).await?;
            }
            Self::LengthPrefixedTarget => {
                let bytes = target.as_bytes();
                if bytes.len() > u16::MAX as usize {
                    return Err(EdgeStreamError::TargetTooLong { len: bytes.len() });
                }
                let mut frame = Vec::with_capacity(2 + bytes.len());
                frame.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
                frame.extend_from_slice(bytes);
                stream.write_all(&frame).await?;
            }
        }
        Ok(())
    }
}

/// Map an artefact's [`ArtefactMode`] onto the wire's [`ExitPreference`].
/// `ArtefactMode::Country`'s code was already validated by
/// [`crate::artefact::EntranceArtefact::validate`], but this is the only
/// place that must actually construct a [`CountryCode`] from it, so the
/// (infallible in practice, still checked) conversion lives here.
pub fn exit_preference_from_mode(mode: &ArtefactMode) -> Result<ExitPreference, EdgeDialError> {
    match mode {
        ArtefactMode::FullAuto => Ok(ExitPreference::Auto),
        ArtefactMode::Country(code) => CountryCode::new(code)
            .map(ExitPreference::Country)
            .map_err(EdgeDialError::BadCountryCode),
    }
}

/// The mono-protocol transport of one session.
///
/// ONE QUIC connection carries both halves of the tunnel: the control wire
/// (the hello and the welcome) and - when the edge says so - the packet
/// plane (the whole IP packets, in QUIC datagrams). The packets do not
/// travel through the control wire, but they travel through the same
/// connection.
///
/// The connector owns the QUIC endpoint, and dropping the endpoint closes the
/// connection: the connector lives as long as the session does.
struct PlaneTransport {
    /// The packet plane - `None` when the edge's welcome does not say the
    /// session carries whole packets.
    plane: Option<Arc<dyn PacketPlane>>,
    /// The connector that owns the QUIC endpoint.
    _connector: PlaneConnector,
    /// Set when the control wire ends. The control wire is idle once the
    /// welcome arrived, so its end IS the end of the connection.
    control_closed: Arc<AtomicBool>,
    /// The task that watches the control wire.
    watcher: Option<tokio::task::JoinHandle<()>>,
}

/// One live session to the customer's own edge.
pub struct EdgeSession {
    /// The mux session handle, or `None` once this session has been torn
    /// down. It is an `Arc` so an in-flight stream open can keep using the
    /// handle while the watchdog takes it away to close the session — see
    /// [`Self::abort`].
    session: Mutex<Option<Arc<SessionHandle>>>,
    welcome: EdgeWelcome,
    /// Which entrance this session dialed — logs only.
    entrance_label: String,
    /// How this session frames a stream's destination — see
    /// [`StreamFraming`]. The edge session speaks the fleet's header; a chain
    /// session speaks the onion exit's own frame.
    framing: StreamFraming,
    /// Consecutive failed stream opens on THIS session — see
    /// [`crate::watchdog`].
    health: Arc<StreamHealth>,
    /// The mono-protocol transport of this session, when it rides one. A
    /// session over the older mux transport has none.
    plane: Mutex<Option<PlaneTransport>>,
    /// The userspace TCP/IP stack over this session's packet plane.
    ///
    /// `None` for the sessions of the older wire versions: those carry
    /// flows, not whole IP packets, so there is nothing to stack over. Also
    /// `None` when the tunnel device owns the plane's packets.
    netstack: Option<Arc<crate::plane_net::PlaneStack>>,
    /// Who owns the packets of this session's plane.
    plane_consumer: PlaneConsumer,
}

impl Drop for EdgeSession {
    /// The control-wire watcher must not outlive the session it watches.
    fn drop(&mut self) {
        if let Ok(transport) = self.plane.get_mut() {
            if let Some(transport) = transport.as_mut() {
                if let Some(watcher) = transport.watcher.as_ref() {
                    watcher.abort();
                }
            }
        }
    }
}

impl EdgeSession {
    /// Dial `entrance`, complete the V9 handshake with the subscriber's
    /// persistent static key, exchange `EdgeHello`/`EdgeWelcome`, and build
    /// the session the welcome asks for: a plane session when the edge's
    /// welcome says the session carries whole IP packets, a mux session
    /// otherwise. Refuses BY NAME (never a bare IO error) when the edge
    /// sends [`EdgeOutcome::Refused`].
    ///
    /// The hello version is offered newest first: version 4, then version 3,
    /// then version 2. A version-1 edge is reached through
    /// [`Self::connect_first_available`].
    pub async fn connect(
        entrance: &ArtefactEntrance,
        mesh_id: &[u8],
        client_static_priv: [u8; 32],
        exit: ExitPreference,
    ) -> Result<Self, EdgeDialError> {
        match Self::connect_at(
            entrance,
            mesh_id,
            client_static_priv,
            exit,
            EDGE_SESSION_WIRE_VERSION_V4,
        )
        .await
        {
            Err(EdgeDialError::Refused(RefusalDisplay(EdgeRefusal::UnsupportedVersion { .. }))) => {
                info!(
                    entrance = %entrance.addr,
                    "edge session: edge refused version 4, falling back to version 3"
                );
                match Self::connect_at(
                    entrance,
                    mesh_id,
                    client_static_priv,
                    exit,
                    EDGE_SESSION_WIRE_VERSION_V3,
                )
                .await
                {
                    Err(EdgeDialError::Refused(RefusalDisplay(EdgeRefusal::UnsupportedVersion {
                        ..
                    }))) => {
                        info!(
                            entrance = %entrance.addr,
                            "edge session: edge refused version 3, falling back to version 2"
                        );
                        Self::connect_at(
                            entrance,
                            mesh_id,
                            client_static_priv,
                            exit,
                            EDGE_SESSION_WIRE_VERSION_V2,
                        )
                        .await
                    }
                    other => other,
                }
            }
            other => other,
        }
    }

    /// One entrance at one wire version: the hello, the welcome, and the
    /// session the welcome asks for.
    async fn connect_at(
        entrance: &ArtefactEntrance,
        mesh_id: &[u8],
        client_static_priv: [u8; 32],
        exit: ExitPreference,
        version: u8,
    ) -> Result<Self, EdgeDialError> {
        let (opened, welcome) =
            dial_face(entrance, mesh_id, client_static_priv, exit, false, &[], version).await?;
        // The direct dial has no carry behind it: the userspace stack is the
        // consumer of the plane's packets.
        Ok(build_session(opened, welcome, entrance.addr.clone(), PlaneConsumer::Stack))
    }

    /// Version-1 walk across `entrances`, starting at `start_idx`. This is
    /// the fallback for an edge that does not speak version 2. It is not how
    /// a version-2 session picks its face — the edge assigns that. Returns
    /// the first entrance that accepts; on total failure, the LAST
    /// entrance's refusal/error.
    pub async fn connect_first_available(
        entrances: &[ArtefactEntrance],
        mesh_id: &[u8],
        client_static_priv: [u8; 32],
        exit: ExitPreference,
        start_idx: usize,
    ) -> Result<(usize, Self), EdgeDialError> {
        let mut last_err = EdgeDialError::NoEntrances;
        for offset in 0..entrances.len() {
            let idx = (start_idx + offset) % entrances.len();
            match Self::connect_v1(&entrances[idx], mesh_id, client_static_priv, exit).await {
                Ok(session) => return Ok((idx, session)),
                Err(e) => {
                    debug!(
                        entrance = %entrances[idx].addr,
                        error = %e,
                        "edge session: dial failed, trying next entrance"
                    );
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    /// The version-1 dial of one entrance: the old wire, the old transport,
    /// byte for byte. Only an edge that does not speak version 2 needs it.
    async fn connect_v1(
        entrance: &ArtefactEntrance,
        mesh_id: &[u8],
        client_static_priv: [u8; 32],
        exit: ExitPreference,
    ) -> Result<Self, EdgeDialError> {
        let mut tunnel = open_entrance_tunnel(entrance, mesh_id, client_static_priv).await?;
        let welcome = match send_hello_and_read_welcome(tunnel.as_mut(), exit).await {
            Ok(welcome) => welcome,
            Err(e) => {
                let _ = tunnel.close().await;
                return Err(e);
            }
        };
        let welcome = match interpret_welcome(welcome) {
            Ok(welcome) => welcome,
            Err(e) => {
                let _ = tunnel.close().await;
                return Err(e);
            }
        };

        info!(
            entrance = %entrance.addr,
            requested = ?exit,
            countries = welcome.countries.len(),
            "edge session: accepted, upgrading to mux"
        );

        Ok(upgrade_session(tunnel, welcome, entrance.addr.clone()))
    }

    /// Every country this edge can serve — never nodes, never topology.
    pub fn countries(&self) -> &[CountryCode] {
        &self.welcome.countries
    }

    /// The exit the edge actually fulfils for this session. Always
    /// `Some` — `connect` never returns `Ok` on a `Refused` outcome.
    pub fn accepted_exit(&self) -> Option<ExitPreference> {
        match self.welcome.outcome {
            EdgeOutcome::Accepted { exit } => Some(exit),
            EdgeOutcome::Refused(_) => None,
        }
    }

    /// Which entrance face this session actually dialed — logs only.
    pub fn entrance_label(&self) -> &str {
        &self.entrance_label
    }

    /// `true` once the underlying session has closed (peer GoAway, EOF,
    /// or a transport error) — the caller (`EdgeSessionSupervisor`) redials.
    /// A session that was torn down by the watchdog counts as closed.
    ///
    /// A plane session has no mux: its liveness is the control wire of the
    /// same QUIC connection, which ends when the connection ends.
    pub fn is_closed(&self) -> bool {
        let transport = self.plane.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(transport) = transport.as_ref() {
            if transport.plane.is_some() {
                return transport.control_closed.load(Ordering::Relaxed);
            }
        }
        match self.handle() {
            Some(handle) => handle.is_closed(),
            None => true,
        }
    }

    /// The packet plane of this session, when it has one.
    ///
    /// `None` for the mux sessions of the older wire versions: those carry
    /// flows, not whole IP packets. The plane is the datagram half of the
    /// mono-protocol QUIC connection - the same connection the control wire
    /// rides.
    pub fn packet_plane(&self) -> Option<Arc<dyn PacketPlane>> {
        let transport = self.plane.lock().unwrap_or_else(|e| e.into_inner());
        transport.as_ref().and_then(|t| t.plane.as_ref().map(Arc::clone))
    }

    /// The address the box assigned to this client, when the welcome carries
    /// one.
    ///
    /// The box is an L3 forwarder and every client behind it needs its own
    /// address: this is the one the box handed to this session, and the
    /// tunnel adapter must carry exactly it. `None` for the sessions of the
    /// older wire versions, and for a box that assigned nothing.
    pub fn assigned_client(&self) -> Option<ClientAssignment> {
        self.welcome.assigned_client
    }

    /// The userspace TCP/IP stack over this session's packet plane.
    ///
    /// `None` for the sessions of the older wire versions: those carry
    /// flows, not whole IP packets, so there is nothing to stack over.
    pub fn netstack(&self) -> Option<Arc<crate::plane_net::PlaneStack>> {
        self.netstack.clone()
    }

    /// Whether this session carries whole IP packets.
    ///
    /// `true` for the plane sessions of the newer wire version, `false` for
    /// the mux sessions of the older ones.
    pub fn has_plane(&self) -> bool {
        let transport = self.plane.lock().unwrap_or_else(|e| e.into_inner());
        transport.as_ref().and_then(|t| t.plane.as_ref()).is_some()
    }

    /// Whether this session's plane is owned by `consumer`.
    ///
    /// A session of the older wire versions carries no packets, so the owner
    /// of the plane does not matter and the session serves every consumer.
    pub fn serves_plane_consumer(&self, consumer: PlaneConsumer) -> bool {
        if !self.has_plane() {
            return true;
        }
        self.plane_consumer == consumer
    }

    /// Whether this session's data path can carry traffic RIGHT NOW.    ///
    /// A mux session is its own data path: the control wire and the flows
    /// ride the same connection. A plane session that the userspace stack
    /// owns is only ready once that stack has polled the plane - before
    /// that, the status must not say `connected`. A plane session that the
    /// tunnel device owns is ready as soon as the plane is up: the device
    /// brings its own stack.
    pub fn data_path_ready(&self) -> bool {
        if !self.has_plane() {
            return true;
        }
        match self.plane_consumer {
            PlaneConsumer::Stack => match &self.netstack {
                Some(stack) => stack.is_ready(),
                None => false,
            },
            PlaneConsumer::RawIp => true,
        }
    }

    /// The mux session handle, while this session is still live.
    fn handle(&self) -> Option<Arc<SessionHandle>> {
        self.session.lock().unwrap_or_else(|e| e.into_inner()).as_ref().cloned()
    }

    /// The watchdog's counter for THIS session — consecutive failed stream
    /// opens, reset by the first successful stream.
    pub fn health(&self) -> &Arc<StreamHealth> {
        &self.health
    }

    /// Tear this session down for real.
    ///
    /// Dropping the mux session handle closes the mux session: the handle
    /// carries the close signal the mux driver waits for, and dropping it
    /// drops that signal, so the driver closes the session (GoAway to the
    /// edge) and the transport with it — the TCP to the edge ends, and the
    /// EDGE records demand again when it accepts the next session. That is
    /// what heals the 2026-10-21 blackout: a session that stays up but
    /// cannot open streams never re-records demand.
    ///
    /// An in-flight stream open keeps its own reference to the handle, so
    /// the close lands as soon as that open returns — microseconds later in
    /// practice, because the mux driver answers an open immediately.
    ///
    /// The mono-protocol transport goes with it: dropping the QUIC endpoint
    /// closes the connection, so the packet plane stops with the session.
    pub fn abort(&self) {
        let taken = self.session.lock().unwrap_or_else(|e| e.into_inner()).take();
        drop(taken);
        let transport = self.plane.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(transport) = transport {
            if let Some(watcher) = transport.watcher.as_ref() {
                watcher.abort();
            }
            drop(transport);
        }
    }

    /// Open one mux stream for a TCP flow, writing this session's target
    /// frame as its first bytes. `target` is the raw `host:port` string the
    /// flow asked for — a domain name travels through unresolved so DNS
    /// resolves at the exit (see `crate::socks5`).
    ///
    /// The returned stream reports how the open ended to this session's
    /// watchdog — see [`crate::watchdog`].
    pub async fn open_tcp_stream(&self, target: &str) -> Result<WatchedStream, EdgeStreamError> {
        let stream = self.open_stream_with_backpressure().await?;
        let mut stream = WatchedStream::new(TokioMuxStream(stream), Arc::clone(&self.health));
        if let Err(e) = self.framing.write_target(&mut stream, target).await {
            self.health.record_failure();
            return Err(e);
        }
        Ok(stream)
    }

    /// Open one mux stream, waiting for stream credit instead of failing the
    /// flow.
    ///
    /// A session hands out a bounded number of concurrent streams — the mux's
    /// stream credit. A machine behind the tunnel asks for more than that all
    /// the time: every browser tab, every DNS lookup, every chat socket is a
    /// stream of its own. When the session is at its limit the extra flows
    /// have to WAIT, the way TCP waits for a slot in the accept queue and the
    /// way QUIC waits for `MAX_STREAMS` (RFC 9000, section 4.6).
    ///
    /// Failing them instead is what used to kill the client every minute:
    /// three failed opens in a row tear the session down (see
    /// [`crate::watchdog`]), every running flow dies with it, and the machine
    /// reconnects en masse into a fresh session that saturates again. The
    /// session stays; the flow waits.
    async fn open_stream_with_backpressure(&self) -> Result<MuxStream, EdgeStreamError> {
        loop {
            let handle = self.handle().ok_or(EdgeStreamError::Mux(MuxError::Closed))?;
            match handle.open_stream().await {
                Ok(stream) => return Ok(stream),
                Err(MuxError::TooManyStreams) => {
                    // The session is at its stream limit. This is
                    // backpressure, not a broken session: the credit comes
                    // back when a flow ends. NOT a failure — counting it
                    // would tear down a perfectly healthy session.
                    info!(
                        "edge session: no stream credit — this flow waits for a slot, the session stays"
                    );
                    tokio::time::sleep(STREAM_CREDIT_RETRY).await;
                    continue;
                }
                Err(e) => {
                    // The session itself could not give us a stream — that is a
                    // failed open like any other.
                    self.health.record_failure();
                    return Err(e.into());
                }
            }
        }
    }

    /// Open one mux stream for a UDP flow, writing the [`StreamOpen`] header
    /// as its first bytes — mirrors [`Self::open_tcp_stream`] exactly,
    /// `kind: StreamKind::Udp` instead, wrapped in
    /// [`crate::udp_flow::EdgeUdpFlow`]'s `[2B len][payload]` datagram
    /// framing. `target` is this flow's ONE fixed destination for its whole
    /// life — see `crate::udp_associate`. Returns
    /// [`EdgeStreamError::DatagramsRefused`] before any stream is opened
    /// when this session's welcome does not say the exit carries datagrams.
    pub async fn open_udp_flow(&self, target: &str) -> Result<crate::udp_flow::EdgeUdpFlow, EdgeStreamError> {
        if self.welcome.carries_datagrams != Some(true) {
            return Err(EdgeStreamError::DatagramsRefused);
        }
        let target = StreamTarget::new(target).map_err(EdgeStreamError::BadTarget)?;
        let stream = self.open_stream_with_backpressure().await?;
        let mut stream = WatchedStream::new(TokioMuxStream(stream), Arc::clone(&self.health));
        let open = StreamOpen { kind: StreamKind::Udp, target };
        if let Err(e) = stream.write_all(&open.encode()).await {
            self.health.record_failure();
            return Err(e.into());
        }
        if let Err(e) = stream.flush().await {
            self.health.record_failure();
            return Err(e.into());
        }
        Ok(crate::udp_flow::EdgeUdpFlow::new(stream))
    }

    /// Build a session over a mux handle this crate did not dial itself.
    ///
    /// The onion chain's session: the tunnel is already upgraded to the exit's
    /// mux server, so there is no edge hello to exchange and no entrance to
    /// name - the chain's id is the label.
    pub(crate) fn from_mux_session(
        session: SessionHandle,
        welcome: EdgeWelcome,
        entrance_label: String,
        framing: StreamFraming,
    ) -> Self {
        Self {
            session: Mutex::new(Some(Arc::new(session))),
            welcome,
            entrance_label,
            framing,
            health: Arc::new(StreamHealth::new()),
            plane: Mutex::new(None),
            netstack: None,
            plane_consumer: PlaneConsumer::Stack,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(session: SessionHandle, welcome: EdgeWelcome) -> Self {
        Self {
            session: Mutex::new(Some(Arc::new(session))),
            welcome,
            entrance_label: "test".to_owned(),
            framing: StreamFraming::EdgeHeader,
            health: Arc::new(StreamHealth::new()),
            plane: Mutex::new(None),
            netstack: None,
            plane_consumer: PlaneConsumer::Stack,
        }
    }
}

/// One opened entrance - the transport it rides.
enum OpenedEntrance {
    /// The mux transport of the older carriers: one V9 tunnel, upgraded to a
    /// mux session once the edge accepts.
    Mux(Box<dyn Tunnel>),
    /// The mono-protocol QUIC transport: ONE connection carries the control
    /// wire AND the packet plane.
    Plane(PlaneDial),
}

impl OpenedEntrance {
    /// The control wire of this entrance - the hello and the welcome ride
    /// it.
    fn tunnel(&mut self) -> &mut Box<dyn Tunnel> {
        match self {
            Self::Mux(tunnel) => tunnel,
            Self::Plane(dial) => dial.tunnel(),
        }
    }

    /// Close the control wire of this entrance.
    async fn close(&mut self) {
        let _ = self.tunnel().close().await;
    }
}

/// Open the V9 tunnel to one entrance. Camouflage stays on this dial; it is
/// not something the edge-session hello carries.
///
/// This is the mux transport of the older carriers. A QUIC entrance does not
/// come here - it is dialled over the mono-protocol plane, see
/// [`open_entrance`].
async fn open_entrance_tunnel(
    entrance: &ArtefactEntrance,
    mesh_id: &[u8],
    client_static_priv: [u8; 32],
) -> Result<Box<dyn Tunnel>, EdgeDialError> {
    let addr: SocketAddr = entrance.addr.parse().map_err(|e: std::net::AddrParseError| EdgeDialError::BadAddr {
        addr: entrance.addr.clone(),
        reason: e.to_string(),
    })?;
    let server_pubkey = entrance.server_pubkey()?;
    // The camouflage — not this crate — decides the wire transport: a stream
    // camouflage rides TCP, a datagram one rides datagrams. The config's
    // `transport` field is therefore DERIVED from the mask, never hardcoded,
    // so the tunnel this opens is dialled the way the face is published.
    let ep_config = MirageEndpointConfig {
        transport: face_transport(&entrance.camouflage),
        auth: AuthMode::Blake3,
        camouflage: entrance.camouflage.clone(),
        byte_cipher: None,
        byte_cipher_mode: None,
    };
    let proto = StaticKeyProtocol::new(ep_config, mesh_id.to_vec(), client_static_priv);
    Ok(proto
        .connect_with_credential(addr, &server_pubkey, "", Some(entrance.camouflage.clone()), None)
        .await?)
}

/// Open the transport of one entrance.
///
/// The camouflage decides: a QUIC entrance is the mono-protocol plane - ONE
/// connection carries the control wire AND the packet plane. Every other
/// camouflage keeps the mux transport of the older carriers, byte for byte.
async fn open_entrance(
    entrance: &ArtefactEntrance,
    mesh_id: &[u8],
    client_static_priv: [u8; 32],
) -> Result<OpenedEntrance, EdgeDialError> {
    if let Some(alpn) = quic_alpn(&entrance.camouflage) {
        let addr: SocketAddr = entrance.addr.parse().map_err(|e: std::net::AddrParseError| {
            EdgeDialError::BadAddr { addr: entrance.addr.clone(), reason: e.to_string() }
        })?;
        let server_pubkey = entrance.server_pubkey()?;
        let dial = PlaneDial::open(&alpn, addr, &server_pubkey, &client_static_priv, mesh_id).await?;
        return Ok(OpenedEntrance::Plane(dial));
    }
    Ok(OpenedEntrance::Mux(open_entrance_tunnel(entrance, mesh_id, client_static_priv).await?))
}

/// Build the session of an accepted hello.
///
/// The welcome decides what the session carries: whole IP packets (the
/// plane) or flows (the mux). The transport decides what it CAN carry: a
/// session over a transport without a plane is a mux session, whatever the
/// welcome says.
fn build_session(
    opened: OpenedEntrance,
    welcome: EdgeWelcome,
    entrance_label: String,
    consumer: PlaneConsumer,
) -> EdgeSession {
    match opened {
        OpenedEntrance::Mux(tunnel) => upgrade_session(tunnel, welcome, entrance_label),
        OpenedEntrance::Plane(dial) => {
            let (tunnel, plane, connector) = dial.into_parts();
            if welcome.carries_datagrams != Some(true) {
                // The edge did not offer whole packets: this is a mux
                // session over the same QUIC connection. The connector still
                // has to outlive it - dropping the QUIC endpoint closes the
                // connection.
                let mut session = upgrade_session(tunnel, welcome, entrance_label);
                session.plane = Mutex::new(Some(PlaneTransport {
                    plane: None,
                    _connector: connector,
                    control_closed: Arc::new(AtomicBool::new(false)),
                    watcher: None,
                }));
                return session;
            }
            plane_session(tunnel, plane, connector, welcome, entrance_label, consumer)
        }
    }
}

/// Build the plane session: the whole IP packets ride the datagrams of the
/// same QUIC connection the control wire rides.
///
/// The control wire is idle once the welcome arrived - it carried the hello
/// and the welcome, and nothing else. Its end IS the end of the
/// connection, so a task watches it and the session's liveness follows it.
fn plane_session(
    tunnel: Box<dyn Tunnel>,
    plane: Arc<dyn PacketPlane>,
    connector: PlaneConnector,
    welcome: EdgeWelcome,
    entrance_label: String,
    consumer: PlaneConsumer,
) -> EdgeSession {
    let control_closed = Arc::new(AtomicBool::new(false));
    // The userspace stack over this plane: the client opens its own TCP and
    // UDP flows THERE - no mux, no TUN, no routes, no helper process.
    //
    // Only the consumer that owns the plane's packets gets a stack: the
    // tunnel device wants the same packets raw, and two consumers would
    // steal them from each other.
    let netstack = match consumer {
        PlaneConsumer::Stack => welcome.assigned_client.map(|assignment| {
            crate::plane_net::PlaneStack::spawn(Arc::clone(&plane), assignment.addr)
        }),
        PlaneConsumer::RawIp => None,
    };
    let watcher = {
        let closed = Arc::clone(&control_closed);
        tokio::spawn(async move {
            let mut tunnel = tunnel;
            let mut buf = [0u8; 256];
            loop {
                match tunnel.recv(&mut buf).await {
                    // The peer closed the control wire: the connection is
                    // over, and the datagrams will never come again.
                    Ok(0) => break,
                    // Nothing rides the control wire after the welcome.
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
            closed.store(true, Ordering::Relaxed);
        })
    };
    EdgeSession {
        session: Mutex::new(None),
        welcome,
        entrance_label,
        framing: StreamFraming::EdgeHeader,
        health: Arc::new(StreamHealth::new()),
        plane: Mutex::new(Some(PlaneTransport {
            plane: Some(plane),
            _connector: connector,
            control_closed,
            watcher: Some(watcher),
        })),
        netstack,
        plane_consumer: consumer,
    }
}

fn upgrade_session(tunnel: Box<dyn Tunnel>, welcome: EdgeWelcome, entrance_label: String) -> EdgeSession {
    let compat = TunnelCompat::wrap(tunnel);
    let session = SessionHandle::client(compat);
    EdgeSession {
        session: Mutex::new(Some(Arc::new(session))),
        welcome,
        entrance_label,
        framing: StreamFraming::EdgeHeader,
        health: Arc::new(StreamHealth::new()),
        plane: Mutex::new(None),
        netstack: None,
        plane_consumer: PlaneConsumer::Stack,
    }
}

fn entrance_tcp_port(entrance: &ArtefactEntrance) -> Result<u16, EdgeDialError> {
    let Some((_, port)) = entrance.addr.rsplit_once(':') else {
        return Err(EdgeDialError::BadAddr {
            addr: entrance.addr.clone(),
            reason: "missing port".to_owned(),
        });
    };
    port.parse::<u16>().map_err(|e| EdgeDialError::BadAddr {
        addr: entrance.addr.clone(),
        reason: e.to_string(),
    })
}

/// The faces of `entrances` that ride `transport`, in artefact order, as
/// `(index, port)`.
///
/// The [`EdgeHello`] wire format carries bare port numbers and NO transport
/// tag, so the edge cannot tell a TCP port from a UDP one. The client
/// therefore offers ONLY the faces of the transport its session actually
/// rides: a session over datagrams must not also offer its stream faces, or
/// the edge may assign one of them back and the follow-up dial would be a
/// stream dial on a datagram session.
///
/// The rule itself lives in `mirage-control-plane`
/// ([`filter_offered_faces_by_transport`]) — the same one the edge and the
/// wire-format code use — so the client and the edge can never disagree
/// about which face rides what. A face whose mask has no port namespace
/// (ICMP) is never offered, whatever the transport is.
///
/// The result is NOT capped at [`EdgeHello::MAX_OFFERED_PORTS`]: that cap is
/// a wire-format limit the caller checks before encoding.
fn offered_ports_for_transport(
    entrances: &[ArtefactEntrance],
    transport: TransportKind,
) -> Result<Vec<(usize, u16)>, EdgeDialError> {
    let faces: Vec<(usize, &TransportCamouflage, u16)> = entrances
        .iter()
        .enumerate()
        .map(|(idx, e)| Ok((idx, &e.camouflage, entrance_tcp_port(e)?)))
        .collect::<Result<Vec<_>, EdgeDialError>>()?;
    Ok(filter_offered_faces_by_transport(&faces, transport))
}

/// Hello on one face at `version` (2, 3 or 4). Does not build the session:
/// the caller keeps the transport only when this face is the session.
async fn dial_face(
    entrance: &ArtefactEntrance,
    mesh_id: &[u8],
    client_static_priv: [u8; 32],
    exit: ExitPreference,
    commit: bool,
    offered: &[u16],
    version: u8,
) -> Result<(OpenedEntrance, EdgeWelcome), EdgeDialError> {
    let mut opened = open_entrance(entrance, mesh_id, client_static_priv).await?;
    // A client that speaks the plane can take the address the box assigns
    // it: it puts that address on its tunnel adapter. The flag is how the
    // box knows -- an older client has no such flag and keeps the address
    // it always had.
    let mut flags = if commit { EdgeHello::FLAG_COMMIT } else { 0 };
    if version == EDGE_SESSION_WIRE_VERSION_V4 {
        flags |= EdgeHello::FLAG_ACCEPTS_ASSIGNMENT;
    }
    let hello = EdgeHello {
        version,
        exit,
        flags,
        offered_ports: offered.to_vec(),
    };
    let bytes = match hello.encode() {
        Ok(bytes) => bytes,
        Err(e) => {
            opened.close().await;
            return Err(EdgeDialError::MalformedHello(e));
        }
    };
    if let Err(e) = opened.tunnel().send(&bytes).await {
        opened.close().await;
        return Err(e.into());
    }
    let welcome = match read_welcome(opened.tunnel().as_mut()).await {
        Ok(welcome) => welcome,
        Err(e) => {
            opened.close().await;
            return Err(e);
        }
    };
    match interpret_welcome(welcome) {
        Ok(welcome) => Ok((opened, welcome)),
        Err(e) => {
            opened.close().await;
            Err(e)
        }
    }
}

fn log_assigned(probe: &str, session: &EdgeSession) {
    info!(probe = %probe, entrance = %session.entrance_label(), "edge session: face assigned");
}

/// Send a version-1 [`EdgeHello`] and read back the (possibly
/// split-across-reads) [`EdgeWelcome`] — pure transport-level plumbing,
/// factored out so it is testable against a mock [`Tunnel`] with no live
/// V9 handshake.
async fn send_hello_and_read_welcome(
    tunnel: &mut dyn Tunnel,
    exit: ExitPreference,
) -> Result<EdgeWelcome, EdgeDialError> {
    let hello = EdgeHello {
        version: EDGE_SESSION_WIRE_VERSION,
        exit,
        flags: 0,
        offered_ports: Vec::new(),
    };
    let bytes = hello.encode().map_err(EdgeDialError::MalformedHello)?;
    tunnel.send(&bytes).await?;
    read_welcome(tunnel).await
}

/// Accumulate `recv()` chunks until a full [`EdgeWelcome`] decodes — the
/// underlying `Tunnel::recv` gives no guarantee the welcome arrives in one
/// call.
async fn read_welcome(tunnel: &mut dyn Tunnel) -> Result<EdgeWelcome, EdgeDialError> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match EdgeWelcome::decode(&buf) {
            Ok((welcome, _consumed)) => return Ok(welcome),
            Err(WireError::Truncated { .. }) => {}
            Err(other) => return Err(EdgeDialError::MalformedWelcome(other)),
        }
        let n = tunnel.recv(&mut chunk).await?;
        if n == 0 {
            return Err(EdgeDialError::ClosedBeforeWelcome);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Turn a parsed [`EdgeWelcome`] into `Ok` (accepted, keep going) or a named
/// `Err` (refused, by name — never a bare IO error).
fn interpret_welcome(welcome: EdgeWelcome) -> Result<EdgeWelcome, EdgeDialError> {
    match welcome.outcome {
        EdgeOutcome::Refused(refusal) => Err(EdgeDialError::Refused(RefusalDisplay(refusal))),
        EdgeOutcome::Accepted { .. } => Ok(welcome),
    }
}

// ── Supervisor: reconnect-with-backoff across the artefact's faces ────────

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
/// The longest the client waits between two dials.
///
/// The recovery after the box is back must fit into ONE of these intervals
/// (the acceptance says 35 seconds), so the ceiling stays well under that:
/// a dead box is noticed in about twenty seconds, and the next attempt
/// follows within this one.
const MAX_BACKOFF: Duration = Duration::from_secs(15);
/// `SessionHandle` exposes only a synchronous `is_closed()`, no async
/// "wait until closed" — the driver polls at this interval.
const LIVENESS_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How long the session's data path may take to come up before the session
/// is given up on.
///
/// The control session is up the moment the dial returns, but the userspace
/// stack over the plane needs a moment to start. Five seconds is far more
/// than it takes and far less than a user would wait.
const DATA_PATH_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Wait for the session's data path to be ready.
///
/// `true` when the data path can carry traffic, `false` when it did not come
/// up in time - the session is then not a session, and the caller tears it
/// down rather than publish a `connected` that is not true.
async fn wait_data_path(session: &Arc<EdgeSession>) -> bool {
    let deadline = tokio::time::Instant::now() + DATA_PATH_READY_TIMEOUT;
    loop {
        if session.data_path_ready() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// One way to obtain a fresh [`EdgeSession`] — the dial the supervisor loop
/// repeats. The production impl dials the artefact's entrances; the seam
/// exists so the loop's published state is testable without a live edge.
#[async_trait::async_trait]
pub(crate) trait EdgeDialer: Send + Sync + 'static {
    /// Dial a session. `start_idx` is not the session policy: the production
    /// dialer ignores it and lets the edge assign the face. Returns the
    /// index of the entrance that carries the live session.
    async fn dial(&self, start_idx: usize) -> Result<(usize, EdgeSession), EdgeDialError>;

    /// Bumped when the requested exit changes. The production dialer
    /// overrides this; a test dialer stays at 0 so a live session is not
    /// torn down.
    fn exit_generation(&self) -> u64 {
        0
    }

    /// Bumped when the owner of the plane's packets changes. The production
    /// dialer overrides this; a test dialer stays at 0 so a live session is
    /// not torn down.
    fn plane_generation(&self) -> u64 {
        0
    }
}

/// Requested exit, shared by the dialer and [`EdgeSessionSupervisor::set_exit`].
struct ExitCtrl {
    pref: Mutex<ExitPreference>,
    generation: AtomicU64,
}

impl ExitCtrl {
    fn new(exit: ExitPreference) -> Self {
        Self { pref: Mutex::new(exit), generation: AtomicU64::new(0) }
    }

    fn get(&self) -> ExitPreference {
        *self.pref.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set(&self, exit: ExitPreference) {
        {
            let mut pref = self.pref.lock().unwrap_or_else(|e| e.into_inner());
            if *pref == exit {
                // The same wish again is not a change. Tearing the live
                // session down for it is how a client that is asked for
                // "auto" on every frame ends up redialing forever.
                return;
            }
            *pref = exit;
        }
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }
}

/// Who owns the packets of a plane session.
///
/// A plane carries whole IP packets, and one address. The userspace stack
/// (the SOCKS5 flows) and the raw-IP consumer (the tunnel device) both need
/// every packet of that address - two consumers would steal the packets from
/// each other. So exactly ONE of them owns the plane, and the client's carry
/// decides which.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaneConsumer {
    /// The userspace TCP/IP stack: the SOCKS5 flows.
    #[default]
    Stack,
    /// The raw-IP consumer: the tunnel device.
    RawIp,
}

/// The plane consumer of the live session, shared by the dialer and
/// [`EdgeSessionSupervisor::set_plane_consumer`].
struct PlaneCtrl {
    consumer: Mutex<PlaneConsumer>,
    generation: AtomicU64,
}

impl PlaneCtrl {
    fn new(consumer: PlaneConsumer) -> Self {
        Self { consumer: Mutex::new(consumer), generation: AtomicU64::new(0) }
    }

    fn get(&self) -> PlaneConsumer {
        *self.consumer.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set(&self, consumer: PlaneConsumer) {
        {
            let mut current = self.consumer.lock().unwrap_or_else(|e| e.into_inner());
            if *current == consumer {
                return;
            }
            *current = consumer;
        }
        // A new consumer means a new owner of the plane's packets: the live
        // session goes away and the next dial builds the session the new
        // consumer needs.
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }
}

/// [`EdgeDialer`] over the artefact's own entrances — every one of them,
/// stream-shaped and datagram-shaped alike.
struct EntranceDialer {
    entrances: Vec<ArtefactEntrance>,
    mesh_id: Vec<u8>,
    client_static_priv: [u8; 32],
    exit: Arc<ExitCtrl>,
    /// Who owns the packets of the plane this dialer builds.
    plane: Arc<PlaneCtrl>,
    /// The entrance that carried the last live session.
    ///
    /// The next dial starts there. A face that worked is the face most
    /// likely to work again, and a random start lands on a datagram face
    /// half the time -- a face that cannot carry a mux session at all.
    last_good: AtomicUsize,
}

impl EntranceDialer {
    /// Probe faces at `version` (2, 3 or 4). A version-2 unsupported-version
    /// refusal falls back to version 1. A version-3 or version-4 refusal is
    /// returned so the caller can retry at the next version down - it does
    /// not skip a version.
    ///
    /// The probe face is picked from EVERY entrance — stream- and
    /// datagram-shaped alike — and the faces the hello offers are the ones
    /// riding the SAME transport as the probed face. The transport of the
    /// session is therefore known BEFORE the offer is assembled, never
    /// after.
    async fn dial_at(
        &self,
        version: u8,
        exit: ExitPreference,
        start: usize,
    ) -> Result<(usize, EdgeSession), EdgeDialError> {
        if self.entrances.is_empty() {
            return Err(EdgeDialError::NoEntrances);
        }
        let start = start % self.entrances.len();
        let mut last_err = EdgeDialError::NoEntrances;
        // A refusal kept for the report: a transport whose face count does
        // not fit in one hello is skipped, not fatal — another transport may
        // still carry the session.
        let mut too_many: Option<EdgeDialError> = None;
        for offset in 0..self.entrances.len() {
            let idx = (start + offset) % self.entrances.len();
            let entrance = &self.entrances[idx];
            let probe_addr = entrance.addr.clone();
            let session_transport = face_transport(&entrance.camouflage);
            let offered = offered_ports_for_transport(&self.entrances, session_transport)?;
            if offered.len() > EdgeHello::MAX_OFFERED_PORTS {
                too_many.get_or_insert(EdgeDialError::TooManyFaces {
                    count: offered.len(),
                    max: EdgeHello::MAX_OFFERED_PORTS,
                });
                debug!(
                    transport = %session_transport.as_str(),
                    count = offered.len(),
                    max = EdgeHello::MAX_OFFERED_PORTS,
                    "edge session: too many faces of this transport to offer — skipping it"
                );
                continue;
            }
            let ports: Vec<u16> = offered.iter().map(|(_, port)| *port).collect();
            let probe_port = entrance_tcp_port(entrance)?;
            match dial_face(
                entrance,
                &self.mesh_id,
                self.client_static_priv,
                exit,
                false,
                &ports,
                version,
            )
            .await
            {
                Ok((mut opened, welcome)) => {
                    if let Some(port) = welcome.assigned_face_port {
                        if port != probe_port {
                            opened.close().await;
                            let Some(target) = offered.iter().copied().find(|(_, face_port)| *face_port == port) else {
                                return Err(EdgeDialError::AssignedFaceMissing { port });
                            };
                            let (opened, welcome) = dial_face(
                                &self.entrances[target.0],
                                &self.mesh_id,
                                self.client_static_priv,
                                exit,
                                true,
                                &ports,
                                version,
                            )
                            .await?;
                            let session = build_session(
                                opened,
                                welcome,
                                self.entrances[target.0].addr.clone(),
                                self.plane.get(),
                            );
                            log_assigned(&probe_addr, &session);
                            return Ok((target.0, session));
                        }
                    }
                    let session =
                        build_session(opened, welcome, probe_addr.clone(), self.plane.get());
                    log_assigned(&probe_addr, &session);
                    return Ok((idx, session));
                }
                Err(e) => match e {
                    EdgeDialError::Refused(RefusalDisplay(EdgeRefusal::UnsupportedVersion { .. }))
                        if version == EDGE_SESSION_WIRE_VERSION_V2 =>
                    {
                        info!(probe = %probe_addr, "edge session: edge refused version 2, falling back to version 1");
                        let (fallback_idx, session) = EdgeSession::connect_first_available(
                            &self.entrances,
                            &self.mesh_id,
                            self.client_static_priv,
                            exit,
                            0,
                        )
                        .await?;
                        log_assigned(&probe_addr, &session);
                        return Ok((fallback_idx, session));
                    }
                    refused @ EdgeDialError::Refused(_) => return Err(refused),
                    other => {
                        debug!(
                            entrance = %probe_addr,
                            error = %other,
                            "edge session: probe failed, trying next face"
                        );
                        last_err = other;
                    }
                },
            }
        }
        // A face count that did not fit is the more specific refusal than
        // "no entrances" — report it when nothing else was tried.
        if let Some(err) = too_many {
            if matches!(last_err, EdgeDialError::NoEntrances) {
                return Err(err);
            }
        }
        Err(last_err)
    }
}

#[async_trait::async_trait]
impl EdgeDialer for EntranceDialer {
    async fn dial(&self, _start_idx: usize) -> Result<(usize, EdgeSession), EdgeDialError> {
        if self.entrances.is_empty() {
            return Err(EdgeDialError::NoEntrances);
        }
        let exit = self.exit.get();
        // Start where the last live session was. The face that carried it is
        // the face most likely to carry the next one, and a random start
        // lands on a datagram face half the time -- a face that cannot
        // carry a mux session at all.
        let start = self.last_good.load(Ordering::Relaxed);
        // Newest wire first: version 4 is the plane, version 3 and version 2
        // are the mux. A step down happens ONLY when the edge refuses the
        // version by name - a transport error never skips a version.
        let result = match self.dial_at(EDGE_SESSION_WIRE_VERSION_V4, exit, start).await {
            Err(EdgeDialError::Refused(RefusalDisplay(EdgeRefusal::UnsupportedVersion { .. }))) => {
                info!("edge session: edge refused version 4, falling back to version 3");
                match self.dial_at(EDGE_SESSION_WIRE_VERSION_V3, exit, start).await {
                    Err(EdgeDialError::Refused(RefusalDisplay(EdgeRefusal::UnsupportedVersion {
                        ..
                    }))) => {
                        info!("edge session: edge refused version 3, falling back to version 2");
                        self.dial_at(EDGE_SESSION_WIRE_VERSION_V2, exit, start).await
                    }
                    other => other,
                }
            }
            other => other,
        };
        if let Ok((idx, _)) = &result {
            self.last_good.store(*idx, Ordering::Relaxed);
        }
        result
    }

    fn exit_generation(&self) -> u64 {
        self.exit.generation()
    }

    fn plane_generation(&self) -> u64 {
        self.plane.generation()
    }
}

/// Reconnect-with-backoff wrapper around [`EdgeSession`]. Holds the current
/// live session (if any) behind a [`tokio::sync::watch`] channel so every
/// caller (the SOCKS5 listener, the TUN feed) sees the same session and the
/// same reconnects. A second `watch` publishes the connector's
/// [`EdgeStatusFile`] on every transition (see [`Self::status`]).
pub struct EdgeSessionSupervisor {
    slot: tokio::sync::watch::Sender<Option<Arc<EdgeSession>>>,
    status: tokio::sync::watch::Sender<EdgeStatusFile>,
    driver: tokio::task::JoinHandle<()>,
    /// Present on the production supervisor. Test dialers leave this empty.
    exit: Option<Arc<ExitCtrl>>,
    /// Who owns the packets of the live plane session.
    ///
    /// Present on the production supervisor. Test dialers leave this empty.
    plane: Option<Arc<PlaneCtrl>>,
}

impl Drop for EdgeSessionSupervisor {
    /// The dial loop lives exactly as long as its supervisor.
    fn drop(&mut self) {
        self.driver.abort();
    }
}

impl EdgeSessionSupervisor {
    /// Spawn the background dial/reconnect loop over `entrances` (every
    /// entrance the artefact carries — the camouflage decides the transport).
    ///
    /// The stream-failure watchdog uses its default threshold — see
    /// [`Self::spawn_with_threshold`].
    pub fn spawn(
        entrances: Vec<ArtefactEntrance>,
        mesh_id: Vec<u8>,
        client_static_priv: [u8; 32],
        exit: ExitPreference,
    ) -> Arc<Self> {
        Self::spawn_with_threshold(
            entrances,
            mesh_id,
            client_static_priv,
            exit,
            DEFAULT_STREAM_FAILURE_THRESHOLD,
        )
    }

    /// [`Self::spawn`] with an explicit stream-failure threshold: how many
    /// consecutive failed stream opens tear the session down and force a
    /// redial. A threshold below one is clamped to one — the watchdog never
    /// tears a session down without at least one failed open on record.
    pub fn spawn_with_threshold(
        entrances: Vec<ArtefactEntrance>,
        mesh_id: Vec<u8>,
        client_static_priv: [u8; 32],
        exit: ExitPreference,
        stream_failure_threshold: u32,
    ) -> Arc<Self> {
        Self::spawn_with_threshold_and_consumer(
            entrances,
            mesh_id,
            client_static_priv,
            exit,
            stream_failure_threshold,
            PlaneConsumer::Stack,
        )
    }

    /// [`Self::spawn_with_threshold`] with the owner of the plane's packets
    /// named: the userspace stack (the SOCKS5 flows) or the raw-IP consumer
    /// (the tunnel device).
    ///
    /// The two want the SAME packets, and one address: two consumers would
    /// steal the packets from each other. So the client says which one it
    /// is, and the session is built for that one.
    pub fn spawn_with_threshold_and_consumer(
        entrances: Vec<ArtefactEntrance>,
        mesh_id: Vec<u8>,
        client_static_priv: [u8; 32],
        exit: ExitPreference,
        stream_failure_threshold: u32,
        consumer: PlaneConsumer,
    ) -> Arc<Self> {
        let exit_ctrl = Arc::new(ExitCtrl::new(exit));
        let plane_ctrl = Arc::new(PlaneCtrl::new(consumer));
        Self::spawn_with_dialer_exit(
            EntranceDialer {
                entrances,
                mesh_id,
                client_static_priv,
                exit: Arc::clone(&exit_ctrl),
                plane: Arc::clone(&plane_ctrl),
                last_good: AtomicUsize::new(0),
            },
            exit,
            Some(exit_ctrl),
            Some(plane_ctrl),
            stream_failure_threshold.max(1),
        )
    }

    /// [`Self::spawn`] over any [`EdgeDialer`]; `requested` is the exit the
    /// status file reports as asked-for.
    #[cfg(test)]
    pub(crate) fn spawn_with_dialer(dialer: impl EdgeDialer, requested: ExitPreference) -> Arc<Self> {
        Self::spawn_with_dialer_exit(dialer, requested, None, None, DEFAULT_STREAM_FAILURE_THRESHOLD)
    }

    /// Spawn the supervisor over a dialer of this crate's own making - the
    /// onion chain's dialer. `requested` is the exit the status file reports
    /// as asked-for; a chain has no exit choice, so it is always auto.
    ///
    /// There is no live exit control behind this supervisor: a chain's exit
    /// is the chain's exit.
    pub(crate) fn spawn_with_custom_dialer(
        dialer: impl EdgeDialer,
        requested: ExitPreference,
        stream_failure_threshold: u32,
    ) -> Arc<Self> {
        Self::spawn_with_dialer_exit(dialer, requested, None, None, stream_failure_threshold.max(1))
    }

    /// [`Self::spawn_with_dialer`] with an explicit stream-failure threshold.
    #[cfg(test)]
    pub(crate) fn spawn_with_dialer_and_threshold(
        dialer: impl EdgeDialer,
        requested: ExitPreference,
        stream_failure_threshold: u32,
    ) -> Arc<Self> {
        Self::spawn_with_dialer_exit(
            dialer,
            requested,
            None,
            None,
            stream_failure_threshold.max(1),
        )
    }

    fn spawn_with_dialer_exit(
        dialer: impl EdgeDialer,
        requested: ExitPreference,
        exit: Option<Arc<ExitCtrl>>,
        plane: Option<Arc<PlaneCtrl>>,
        stream_failure_threshold: u32,
    ) -> Arc<Self> {
        let (slot, _slot_rx) = tokio::sync::watch::channel(None);
        let (status, _status_rx) =
            tokio::sync::watch::channel(EdgeStatusFile::connecting(&requested));
        let driver =
            tokio::spawn(drive(dialer, slot.clone(), status.clone(), stream_failure_threshold));
        Arc::new(Self { slot, status, driver, exit, plane })
    }

    /// Next dial uses `exit`. A live session is dropped so the loop redials.
    pub fn set_exit(&self, exit: ExitPreference) {
        let Some(ctrl) = &self.exit else { return };
        let label = crate::status::exit_label(&exit);
        ctrl.set(exit);
        self.status.send_modify(|status| status.exit.requested = label.clone());
    }

    /// Set the exit by its label: `auto` or an ISO alpha-2 country code.
    /// Returns `false` when the label is neither.
    pub fn set_exit_label(&self, label: &str) -> bool {
        use mirage_control_plane::CountryCode;
        let pref = if label == "auto" {
            ExitPreference::Auto
        } else {
            match CountryCode::new(label) {
                Ok(code) => ExitPreference::Country(code),
                Err(_) => return false,
            }
        };
        self.set_exit(pref);
        true
    }

    /// Name the owner of the plane's packets.
    ///
    /// The userspace stack (the SOCKS5 flows) and the tunnel device (the
    /// raw-IP consumer) want the same packets, and only one of them may have
    /// them. A change tears the live session down: the next dial builds the
    /// session the new owner needs.
    pub fn set_plane_consumer(&self, consumer: PlaneConsumer) {
        let Some(ctrl) = &self.plane else { return };
        ctrl.set(consumer);
    }

    /// Stop the dial loop. The live session, if any, is dropped with it and
    /// nothing is dialed again. Idempotent.
    ///
    /// This is what a target change uses: the old target's session goes away
    /// at once instead of being dialed behind the new one until its last
    /// `Arc` drops.
    pub fn stop(&self) {
        self.driver.abort();
    }

    /// A receiver for the connector's published state. Its current value is
    /// always the latest transition; see `crate::status`.
    pub fn status(&self) -> tokio::sync::watch::Receiver<EdgeStatusFile> {
        self.status.subscribe()
    }

    /// The current live session, if any — does not wait.
    pub fn current(&self) -> Option<Arc<EdgeSession>> {
        self.live_session()
    }

    /// The live session, if any -- does not wait.
    ///
    /// The non-blocking twin of [`Self::wait_for_session`]: it answers with
    /// the session that is up RIGHT NOW, and `None` when there is none.
    pub fn live_session(&self) -> Option<Arc<EdgeSession>> {
        self.slot.borrow().clone()
    }

    /// The address the box assigned to the live session, if any.
    ///
    /// `None` when no session is up, or when the box assigned nothing --
    /// the older wire versions have no such address.
    pub fn assigned_client(&self) -> Option<ClientAssignment> {
        self.live_session().and_then(|session| session.assigned_client())
    }

    /// The live session for `consumer`, waiting up to `timeout`.
    ///
    /// A plane session belongs to ONE owner of its packets: the userspace
    /// stack (the SOCKS5 flows) or the tunnel device (the raw-IP consumer).
    /// When the live session belongs to the other one, the owner is switched
    /// and the wait goes on: the supervisor tears that session down and dials
    /// a new one for the new owner.
    ///
    /// This is what makes the carry switch work without a race: whoever asks
    /// for the packets waits for a session that has them.
    pub async fn wait_for_session_with(
        &self,
        consumer: PlaneConsumer,
        timeout: Duration,
    ) -> Option<Arc<EdgeSession>> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut rx = self.slot.subscribe();
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let current = rx.borrow().clone();
            if let Some(session) = current {
                if !session.is_closed() {
                    if session.serves_plane_consumer(consumer) {
                        return Some(session);
                    }
                    // The live session's packets belong to the other owner:
                    // ask for the switch and wait for the session it brings.
                    self.set_plane_consumer(consumer);
                }
            }
            tokio::select! {
                changed = rx.changed() => {
                    if changed.is_err() {
                        return None;
                    }
                }
                _ = tokio::time::sleep(remaining) => return None,
            }
        }
    }

    /// The current live session, waiting up to `timeout` for one to appear
    /// (or reappear) if none is live right now.
    pub async fn wait_for_session(&self, timeout: Duration) -> Option<Arc<EdgeSession>> {
        if let Some(session) = self.current() {
            if !session.is_closed() {
                return Some(session);
            }
        }
        let mut rx = self.slot.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            tokio::select! {
                changed = rx.changed() => {
                    if changed.is_err() {
                        return None;
                    }
                    if let Some(session) = rx.borrow().clone() {
                        return Some(session);
                    }
                }
                _ = tokio::time::sleep(remaining) => return None,
            }
        }
    }
}

/// The supervisor's dial/reconnect loop: keep one session up, publish every
/// transition to `status`, back off exponentially between failed rounds.
///
/// The loop also runs the client-side watchdog
/// ([`crate::watchdog`]): while a session is up it watches that session's
/// consecutive failed stream opens, and once they reach
/// `stream_failure_threshold` it tears the session down and redials — the
/// only thing that makes the edge record demand again (see
/// `docs/mirage/audits/edge-idle-demand-drop-blackout-2026-10-21.md`).
async fn drive(
    dialer: impl EdgeDialer,
    slot: tokio::sync::watch::Sender<Option<Arc<EdgeSession>>>,
    status: tokio::sync::watch::Sender<EdgeStatusFile>,
    stream_failure_threshold: u32,
) {
    let mut backoff = INITIAL_BACKOFF;
    let mut ever_connected = false;
    loop {
        // The edge assigns the session face. Probe choice lives inside `dial`.
        match dialer.dial(0).await {
            Ok((_idx, session)) => {
                info!(entrance = %session.entrance_label(), "edge session: connected");
                backoff = INITIAL_BACKOFF;
                ever_connected = true;
                let session = Arc::new(session);
                // `send_replace`, not `send`: `send` drops the value when no
                // receiver is subscribed at that instant, which would lose a
                // session dialed before the first SOCKS5 client arrived.
                slot.send_replace(Some(Arc::clone(&session)));
                let exit_gen = dialer.exit_generation();
                let plane_gen = dialer.plane_generation();
                // The status may say `connected` only when the data path can
                // really carry traffic: the control session, the packet plane
                // AND the userspace stack. A control-only `connected` is a
                // lie - the SOCKS5 listener would be up and every flow would
                // fail.
                if wait_data_path(&session).await {
                    status.send_modify(|s| s.set_connected(session.accepted_exit(), session.countries()));
                } else {
                    warn!(
                        entrance = %session.entrance_label(),
                        "edge session: the data path did not come up - tearing the session down"
                    );
                    status.send_modify(EdgeStatusFile::set_reconnecting);
                    slot.send_replace(None);
                    session.abort();
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
                // The watchdog's counter: the loop wakes on every change
                // instead of waiting for its next liveness tick, so the
                // teardown starts the moment the threshold is reached.
                let mut failures = session.health().subscribe();
                let mut watchdog_tripped = false;
                while !session.is_closed() {
                    tokio::select! {
                        _ = tokio::time::sleep(LIVENESS_POLL_INTERVAL) => {}
                        changed = failures.changed() => {
                            if changed.is_err() {
                                break;
                            }
                        }
                    }
                    if dialer.exit_generation() != exit_gen {
                        info!("edge session: exit changed, redialing");
                        break;
                    }
                    if dialer.plane_generation() != plane_gen {
                        info!("edge session: the owner of the plane's packets changed, redialing");
                        break;
                    }
                    let failed = session.health().consecutive_failures();
                    if failed >= stream_failure_threshold {
                        warn!(
                            failures = failed,
                            threshold = stream_failure_threshold,
                            "edge session: consecutive stream-open failures — tearing the session down"
                        );
                        watchdog_tripped = true;
                        break;
                    }
                }
                if watchdog_tripped {
                    // The truth FIRST: this session is going away, so the
                    // published status must stop saying `connected` before
                    // anything else happens.
                    status.send_modify(EdgeStatusFile::set_reconnecting);
                    slot.send_replace(None);
                    // Close the session for real, so the EDGE sees the
                    // session end and records demand again on its next
                    // accept.
                    session.abort();
                } else {
                    warn!("edge session: session closed, reconnecting");
                    slot.send_replace(None);
                    status.send_modify(EdgeStatusFile::set_reconnecting);
                }
            }
            Err(e) => {
                warn!(
                    error = %e,
                    backoff_secs = backoff.as_secs(),
                    "edge session: every entrance refused or unreachable, backing off"
                );
                let reason = EdgeReason::from(&e);
                status.send_modify(|s| s.set_dial_failed(reason, ever_connected));
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

#[cfg(test)]
impl EdgeSessionSupervisor {
    /// Test-only: a supervisor whose slot already holds a live `session`,
    /// with no dial loop behind it — lets `crate::udp_associate`'s tests
    /// exercise a real mux pair without a real V9 handshake or entrance
    /// dial. The driver task never completes; the test process ending is
    /// the only cleanup this needs.
    pub(crate) fn for_test(session: Arc<EdgeSession>) -> Arc<Self> {
        let (slot, _rx) = tokio::sync::watch::channel(Some(session));
        let (status, _status_rx) =
            tokio::sync::watch::channel(EdgeStatusFile::connecting(&ExitPreference::Auto));
        let driver = tokio::spawn(std::future::pending());
        Arc::new(Self { slot, status, driver, exit: None, plane: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::EdgeState;
    use mirage_protocol::traits::TransportInfo;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex as StdMutex;
    use tokio::sync::mpsc;

    /// A hand-rolled [`Tunnel`] over pre-scripted inbound chunks, capturing
    /// every outbound `send()` call — lets `send_hello_and_read_welcome` be
    /// tested without a live V9 handshake.
    struct MockTunnel {
        sent: StdMutex<Vec<Vec<u8>>>,
        inbound: StdMutex<VecDeque<Vec<u8>>>,
        info: TransportInfo,
    }

    impl MockTunnel {
        fn with_inbound(chunks: Vec<Vec<u8>>) -> Self {
            Self {
                sent: StdMutex::new(Vec::new()),
                inbound: StdMutex::new(chunks.into()),
                info: TransportInfo::tcp_raw("127.0.0.1:0".parse().expect("valid addr")),
            }
        }

        fn sent_frames(&self) -> Vec<Vec<u8>> {
            self.sent.lock().expect("mock tunnel mutex not poisoned").clone()
        }
    }

    #[async_trait::async_trait]
    impl Tunnel for MockTunnel {
        async fn send(&mut self, data: &[u8]) -> Result<(), MirageError> {
            self.sent.lock().expect("mock tunnel mutex not poisoned").push(data.to_vec());
            Ok(())
        }

        async fn recv(&mut self, buf: &mut [u8]) -> Result<usize, MirageError> {
            let mut inbound = self.inbound.lock().expect("mock tunnel mutex not poisoned");
            match inbound.pop_front() {
                Some(chunk) => {
                    let n = chunk.len().min(buf.len());
                    buf[..n].copy_from_slice(&chunk[..n]);
                    Ok(n)
                }
                None => Ok(0),
            }
        }

        async fn close(&mut self) -> Result<(), MirageError> {
            Ok(())
        }

        fn transport_info(&self) -> &TransportInfo {
            &self.info
        }
    }

    fn sample_welcome(outcome: EdgeOutcome) -> EdgeWelcome {
        EdgeWelcome {
            version: 1,
            countries: vec![
                CountryCode::new("US").expect("valid"),
                CountryCode::new("DE").expect("valid"),
            ],
            outcome,
            assigned_face_port: None,
            carries_datagrams: None,
            assigned_client: None,
        }
    }

    #[tokio::test]
    async fn udp_flow_is_refused_when_welcome_does_not_carry_datagrams() {
        use mirage_mux::tokio_compat::TokioIoCompat;

        let (client_io, server_io) = tokio::io::duplex(64);
        let client = SessionHandle::client(TokioIoCompat(client_io));
        let server = SessionHandle::server(TokioIoCompat(server_io));
        let session = EdgeSession::for_test(
            client,
            sample_welcome(EdgeOutcome::Accepted { exit: ExitPreference::Auto }),
        );
        let opened = session.open_udp_flow("203.0.113.9:53").await;
        let err = match opened {
            Err(err) => err,
            Ok(_) => panic!("a welcome without datagrams must refuse before opening a stream"),
        };
        assert!(matches!(err, EdgeStreamError::DatagramsRefused));
        drop(server);
    }

    // ── send_hello_and_read_welcome ─────────────────────────────────────

    #[tokio::test]
    async fn sends_edge_hello_and_parses_a_welcome_split_across_reads() {
        let welcome = sample_welcome(EdgeOutcome::Accepted { exit: ExitPreference::Auto });
        let encoded = welcome.encode().expect("encodes");
        let mid = encoded.len() / 2;
        let mut tunnel = MockTunnel::with_inbound(vec![encoded[..mid].to_vec(), encoded[mid..].to_vec()]);

        let got = send_hello_and_read_welcome(&mut tunnel, ExitPreference::Auto)
            .await
            .expect("must parse a welcome split across two recv() calls");
        assert_eq!(got, welcome);

        let sent = tunnel.sent_frames();
        assert_eq!(sent.len(), 1, "exactly one EdgeHello must be sent");
        assert_eq!(
            sent[0],
            EdgeHello {
                version: EDGE_SESSION_WIRE_VERSION,
                exit: ExitPreference::Auto,
                flags: 0,
                offered_ports: Vec::new(),
            }
            .encode()
            .expect("version-1 hello encodes")
        );
    }

    #[tokio::test]
    async fn closed_before_welcome_is_a_named_error() {
        let mut tunnel = MockTunnel::with_inbound(vec![]);
        let err = send_hello_and_read_welcome(&mut tunnel, ExitPreference::Auto)
            .await
            .expect_err("empty inbound must refuse, not hang or panic");
        assert!(matches!(err, EdgeDialError::ClosedBeforeWelcome));
    }

    #[tokio::test]
    async fn malformed_welcome_bytes_are_a_named_error() {
        // Country count byte (255) exceeds EdgeWelcome::MAX_COUNTRIES (250).
        let mut tunnel = MockTunnel::with_inbound(vec![vec![1u8, 255]]);
        let err = send_hello_and_read_welcome(&mut tunnel, ExitPreference::Auto)
            .await
            .expect_err("must refuse a malformed welcome");
        assert!(matches!(err, EdgeDialError::MalformedWelcome(_)));
    }

    // ── interpret_welcome: refusal surfaced by name ─────────────────────

    #[test]
    fn accepted_welcome_passes_through() {
        let welcome = sample_welcome(EdgeOutcome::Accepted {
            exit: ExitPreference::Country(CountryCode::new("NL").expect("valid")),
        });
        let got = interpret_welcome(welcome.clone()).expect("accepted must pass through");
        assert_eq!(got, welcome);
    }

    #[test]
    fn refusal_is_surfaced_by_name_not_a_bare_io_error() {
        let welcome = sample_welcome(EdgeOutcome::Refused(EdgeRefusal::CountryUnavailable {
            requested: CountryCode::new("ZZ").expect("valid"),
        }));
        let err = interpret_welcome(welcome).expect_err("must refuse");
        match err {
            EdgeDialError::Refused(RefusalDisplay(EdgeRefusal::CountryUnavailable { requested })) => {
                assert_eq!(requested.as_str(), "ZZ");
            }
            other => panic!("expected Refused(CountryUnavailable), got {other:?}"),
        }
        let rendered = format!("{err}");
        assert!(
            rendered.contains("country_unavailable") && rendered.contains("ZZ"),
            "the refusal must be named in the error text, got: {rendered}"
        );
    }

    #[test]
    fn unsupported_version_refusal_is_surfaced_by_name() {
        let welcome = sample_welcome(EdgeOutcome::Refused(EdgeRefusal::UnsupportedVersion {
            requested: 9,
            supported: 1,
        }));
        let err = interpret_welcome(welcome).expect_err("must refuse");
        let rendered = format!("{err}");
        assert!(
            rendered.contains("unsupported_version") && rendered.contains("requested=9"),
            "got: {rendered}"
        );
    }

    // ── exit_preference_from_mode ───────────────────────────────────────

    #[test]
    fn exit_preference_from_mode_maps_full_auto() {
        assert_eq!(exit_preference_from_mode(&ArtefactMode::FullAuto).expect("ok"), ExitPreference::Auto);
    }

    /// The faces the offer tests share: two stream faces, two datagram faces
    /// and one ICMP face — in that order, so the offered list's order is
    /// observable.
    fn mixed_face_artefact() -> Vec<ArtefactEntrance> {
        use mirage_core::config::TransportCamouflage;
        let face = |addr: &str, camouflage: TransportCamouflage| ArtefactEntrance {
            addr: addr.to_owned(),
            server_pubkey_hex: "ab".repeat(32),
            camouflage,
        };
        vec![
            face(
                "203.0.113.10:80",
                TransportCamouflage::WebSocket { path: "/".to_owned(), host: "h".to_owned() },
            ),
            face(
                "203.0.113.10:5060",
                TransportCamouflage::RawUdp { bind_addr: "0.0.0.0:5060".parse().expect("addr") },
            ),
            face(
                "203.0.113.10:443",
                TransportCamouflage::TlsReal { sni: "ex".to_owned(), alpn: Vec::new() },
            ),
            face(
                "203.0.113.10:853",
                TransportCamouflage::RawUdp { bind_addr: "0.0.0.0:853".parse().expect("addr") },
            ),
            face(
                "203.0.113.10:8080",
                TransportCamouflage::Xhttp { path: "/".to_owned(), host: "h".to_owned() },
            ),
            // ICMP: no port namespace of its own — it must never be offered.
            face(
                "203.0.113.10:7",
                TransportCamouflage::Icmp {
                    peer_addr: "203.0.113.10".parse().expect("addr"),
                    identifier: 7,
                    mtu: 1400,
                },
            ),
        ]
    }

    /// A stream session offers the stream faces only; a datagram session
    /// offers the datagram faces only. The two sets never mix — the wire
    /// format carries bare ports with no transport tag.
    #[test]
    fn offered_ports_for_transport_keeps_only_the_faces_of_that_transport() {
        let entrances = mixed_face_artefact();

        let tcp = offered_ports_for_transport(&entrances, TransportKind::Tcp).expect("ports");
        assert_eq!(tcp, vec![(0, 80), (2, 443), (4, 8080)]);

        let udp = offered_ports_for_transport(&entrances, TransportKind::Udp).expect("ports");
        assert_eq!(udp, vec![(1, 5060), (3, 853)]);
    }

    /// The offer keeps the artefact's own order — the edge reads the list as
    /// "these are the faces I may be assigned", not as an unordered set.
    #[test]
    fn the_offered_faces_keep_the_artefact_order() {
        let entrances = mixed_face_artefact();
        for transport in [TransportKind::Tcp, TransportKind::Udp] {
            let offered = offered_ports_for_transport(&entrances, transport).expect("ports");
            let mut indices: Vec<usize> = offered.iter().map(|(idx, _)| *idx).collect();
            let artefact_order: Vec<usize> = indices.clone();
            indices.sort_unstable();
            assert_eq!(
                indices,
                artefact_order,
                "the offered faces must stay in the artefact's order (transport {})",
                transport.as_str()
            );
        }
    }

    /// A datagram face is offered by a datagram session and NEVER by a
    /// stream session — and the other way round. This is the whole point of
    /// the transport-aware offer: the subscriber reaches a box that publishes
    /// datagram faces, without ever offering a stream port on a datagram
    /// session.
    #[test]
    fn a_datagram_face_is_offered_by_a_datagram_session_only() {
        let entrances = mixed_face_artefact();
        let udp = offered_ports_for_transport(&entrances, TransportKind::Udp).expect("ports");
        assert!(
            udp.contains(&(1, 5060)) && udp.contains(&(3, 853)),
            "both datagram faces must be offered to a datagram session, got {udp:?}"
        );

        let tcp = offered_ports_for_transport(&entrances, TransportKind::Tcp).expect("ports");
        assert!(
            !tcp.contains(&(1, 5060)) && !tcp.contains(&(3, 853)),
            "a datagram face must never be offered to a stream session, got {tcp:?}"
        );
    }

    /// ICMP has no port namespace of its own — there is no port to put in
    /// the hello — so it is never offered, whatever the session's transport
    /// is.
    #[test]
    fn an_icmp_face_is_never_offered() {
        let entrances = mixed_face_artefact();
        let icmp_idx = 5;
        for transport in [TransportKind::Tcp, TransportKind::Udp] {
            let offered = offered_ports_for_transport(&entrances, transport).expect("ports");
            assert!(
                !offered.iter().any(|(idx, _)| *idx == icmp_idx),
                "an ICMP face must never be offered (transport {}), got {offered:?}",
                transport.as_str()
            );
        }
    }

    /// The transport a face rides is the one its own camouflage decides —
    /// the same rule the dial applies.
    #[test]
    fn the_transport_of_a_face_comes_from_its_camouflage() {
        let entrances = mixed_face_artefact();
        assert_eq!(face_transport(&entrances[0].camouflage), TransportKind::Tcp);
        assert_eq!(face_transport(&entrances[1].camouflage), TransportKind::Udp);
        assert_eq!(face_transport(&entrances[5].camouflage), TransportKind::Udp);
    }

    #[test]
    fn exit_preference_from_mode_maps_country() {
        let pref = exit_preference_from_mode(&ArtefactMode::Country("DE".to_owned())).expect("ok");
        assert_eq!(pref, ExitPreference::Country(CountryCode::new("DE").expect("valid")));
    }

    // ── open_tcp_stream: exact StreamOpen header bytes on the wire ──────

    #[tokio::test]
    async fn open_tcp_stream_writes_exact_stream_open_header_then_carries_raw_bytes() {
        let (client_io, server_io) = tokio::io::duplex(4096);
        let client_session =
            SessionHandle::client(mirage_mux::tokio_compat::TokioIoCompat(client_io));
        let server_session =
            SessionHandle::server(mirage_mux::tokio_compat::TokioIoCompat(server_io));

        let edge = EdgeSession::for_test(
            client_session,
            sample_welcome(EdgeOutcome::Accepted { exit: ExitPreference::Auto }),
        );

        let target = "93.184.216.34:443";
        let mut client_stream = edge.open_tcp_stream(target).await.expect("open must succeed");

        let server_stream = server_session.accept_stream().await.expect("server must see the new stream");
        let mut server_stream = TokioMuxStream(server_stream);

        let expected_header = StreamOpen {
            kind: StreamKind::Tcp,
            target: StreamTarget::new(target).expect("valid"),
        }
        .encode();
        let mut header = vec![0u8; expected_header.len()];
        tokio::io::AsyncReadExt::read_exact(&mut server_stream, &mut header)
            .await
            .expect("read header");
        assert_eq!(header, expected_header);
        let (decoded, consumed) = StreamOpen::decode(&header).expect("decodes");
        assert_eq!(consumed, header.len());
        assert_eq!(decoded.kind, StreamKind::Tcp);
        assert_eq!(decoded.target.as_str(), target);

        // Bytes after the header ride the stream raw, both directions.
        tokio::io::AsyncWriteExt::write_all(&mut client_stream, b"ping")
            .await
            .expect("client write");
        let mut ping = [0u8; 4];
        tokio::io::AsyncReadExt::read_exact(&mut server_stream, &mut ping)
            .await
            .expect("server read");
        assert_eq!(&ping, b"ping");

        tokio::io::AsyncWriteExt::write_all(&mut server_stream, b"pong")
            .await
            .expect("server write");
        let mut pong = [0u8; 4];
        tokio::io::AsyncReadExt::read_exact(&mut client_stream, &mut pong)
            .await
            .expect("client read");
        assert_eq!(&pong, b"pong");
    }

    // ── the stream-failure watchdog (BUG-3, 2026-10-21 blackout) ───────
    //
    // The edge does not refuse a stream with an error frame: it closes the
    // mux stream (`mirage2node/crates/mirage-edge/src/entrance.rs:733`).
    // The tests below play that edge exactly — a stream that is closed at
    // once, with nothing delivered on it.

    /// How the edge handles one stream: refuse it (close it at once, like
    /// the edge does when it holds no chain for this exit) or answer it.
    #[derive(Clone, Copy)]
    enum EdgeBehaviour {
        Refuse,
        Answer,
    }

    /// The edge's side of a test session: every accepted stream is handled
    /// per the behaviour the test scripted for it.
    async fn scripted_edge(server: SessionHandle, mut script: mpsc::Receiver<EdgeBehaviour>) {
        while let Ok(raw) = server.accept_stream().await {
            let mut stream = TokioMuxStream(raw);
            match script.recv().await {
                Some(EdgeBehaviour::Refuse) => {
                    // The refusal shape: the stream ends with nothing on it.
                    let _ = tokio::io::AsyncWriteExt::shutdown(&mut stream).await;
                }
                Some(EdgeBehaviour::Answer) => {
                    let mut header = [0u8; 256];
                    let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut header).await;
                    let _ = tokio::io::AsyncWriteExt::write_all(&mut stream, b"ok").await;
                    let _ = tokio::io::AsyncWriteExt::shutdown(&mut stream).await;
                }
                None => break,
            }
        }
    }

    /// Read one stream to its end, the way the proxy does — the EOF is what
    /// tells the watchdog how the open ended.
    async fn drain_to_end(stream: &mut WatchedStream) -> Vec<u8> {
        let mut buf = vec![0u8; 256];
        let mut out = Vec::new();
        loop {
            let n = tokio::io::AsyncReadExt::read(stream, &mut buf).await.expect("read to end");
            if n == 0 {
                return out;
            }
            out.extend_from_slice(&buf[..n]);
        }
    }

    /// Hands out one session and then parks, counting every dial — so a test
    /// can see that the watchdog forced a redial.
    struct OneShotDialer {
        session: StdMutex<Option<EdgeSession>>,
        dials: Arc<AtomicUsize>,
    }

    impl OneShotDialer {
        fn new(session: EdgeSession) -> (Self, Arc<AtomicUsize>) {
            let dials = Arc::new(AtomicUsize::new(0));
            (
                Self { session: StdMutex::new(Some(session)), dials: Arc::clone(&dials) },
                dials,
            )
        }
    }

    #[async_trait::async_trait]
    impl EdgeDialer for OneShotDialer {
        async fn dial(&self, _start_idx: usize) -> Result<(usize, EdgeSession), EdgeDialError> {
            self.dials.fetch_add(1, Ordering::SeqCst);
            // The guard must not live across the await below.
            let next = { self.session.lock().expect("dialer mutex not poisoned").take() };
            match next {
                Some(session) => Ok((0, session)),
                // Park: the supervisor must not spin redialing during a test.
                None => std::future::pending().await,
            }
        }
    }

    /// A live session whose edge refuses or answers streams per script, plus
    /// the supervisor over it.
    async fn watchdog_scenario(
        threshold: u32,
    ) -> (mpsc::Sender<EdgeBehaviour>, Arc<EdgeSessionSupervisor>, Arc<AtomicUsize>) {
        let (client_io, server_io) = tokio::io::duplex(4096);
        let client = SessionHandle::client(mirage_mux::tokio_compat::TokioIoCompat(client_io));
        let server = SessionHandle::server(mirage_mux::tokio_compat::TokioIoCompat(server_io));
        let (script_tx, script_rx) = mpsc::channel(16);
        tokio::spawn(scripted_edge(server, script_rx));

        let session = EdgeSession::for_test(
            client,
            sample_welcome(EdgeOutcome::Accepted { exit: ExitPreference::Auto }),
        );
        let (dialer, dials) = OneShotDialer::new(session);
        let supervisor = EdgeSessionSupervisor::spawn_with_dialer_and_threshold(
            dialer,
            ExitPreference::Auto,
            threshold,
        );
        (script_tx, supervisor, dials)
    }

    /// One refused stream: opened, then closed by the edge with nothing on
    /// it — the shape the 2026-10-21 blackout produced.
    async fn refused_stream(
        session: &EdgeSession,
        script: &mpsc::Sender<EdgeBehaviour>,
    ) -> WatchedStream {
        script.send(EdgeBehaviour::Refuse).await.expect("script");
        let mut stream = session.open_tcp_stream("203.0.113.7:443").await.expect("open");
        assert!(drain_to_end(&mut stream).await.is_empty(), "a refused stream carries nothing");
        stream
    }

    #[tokio::test]
    async fn n_consecutive_failed_stream_opens_tear_the_session_down_and_force_a_redial() {
        let (script, supervisor, dials) = watchdog_scenario(3).await;
        let mut status = supervisor.status();
        tokio::time::timeout(
            Duration::from_secs(5),
            status.wait_for(|s| s.state == EdgeState::Connected),
        )
        .await
        .expect("the supervisor must reach `connected`")
        .expect("status channel stays open");
        let session = supervisor.current().expect("a live session");

        for _ in 0..3 {
            refused_stream(&session, &script).await;
        }

        tokio::time::timeout(
            Duration::from_secs(5),
            status.wait_for(|s| s.state == EdgeState::Reconnecting),
        )
        .await
        .expect("the watchdog must publish `reconnecting`")
        .expect("status channel stays open");

        assert!(session.is_closed(), "the teardown must close the session for real");
        // The redial follows the backoff, so it lands a moment later.
        let redialed = tokio::time::timeout(Duration::from_secs(5), async {
            while dials.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            redialed.is_ok(),
            "the watchdog must force a redial — the edge records demand again on the next accept"
        );
    }

    #[tokio::test]
    async fn a_successful_stream_resets_the_failure_counter() {
        let (script, supervisor, _dials) = watchdog_scenario(3).await;
        let mut status = supervisor.status();
        tokio::time::timeout(
            Duration::from_secs(5),
            status.wait_for(|s| s.state == EdgeState::Connected),
        )
        .await
        .expect("the supervisor must reach `connected`")
        .expect("status channel stays open");
        let session = supervisor.current().expect("a live session");

        refused_stream(&session, &script).await;
        refused_stream(&session, &script).await;
        assert_eq!(session.health().consecutive_failures(), 2);

        // One answered stream: the edge delivered a payload, so the run of
        // failures ends here.
        script.send(EdgeBehaviour::Answer).await.expect("script");
        let mut stream = session.open_tcp_stream("203.0.113.8:443").await.expect("open");
        assert_eq!(drain_to_end(&mut stream).await, b"ok");
        assert_eq!(session.health().consecutive_failures(), 0, "a success must reset the counter");

        // Two more failures — still under the threshold, so the session
        // must survive them.
        refused_stream(&session, &script).await;
        refused_stream(&session, &script).await;
        assert_eq!(session.health().consecutive_failures(), 2);
        assert!(!session.is_closed(), "a reset counter must keep the session up");
        assert_eq!(status.borrow().state, EdgeState::Connected);
    }

    #[tokio::test]
    async fn the_published_status_is_not_connected_while_the_watchdog_tears_down() {
        let (script, supervisor, _dials) = watchdog_scenario(3).await;
        let mut status = supervisor.status();
        tokio::time::timeout(
            Duration::from_secs(5),
            status.wait_for(|s| s.state == EdgeState::Connected),
        )
        .await
        .expect("the supervisor must reach `connected`")
        .expect("status channel stays open");
        let session = supervisor.current().expect("a live session");

        for _ in 0..3 {
            refused_stream(&session, &script).await;
        }

        // The status must LEAVE `connected` — it must not keep claiming a
        // live session while the watchdog tears it down.
        let degraded = tokio::time::timeout(
            Duration::from_secs(5),
            status.wait_for(|s| s.state != EdgeState::Connected),
        )
        .await
        .expect("the status must leave `connected`")
        .expect("status channel stays open");
        assert_eq!(degraded.state, EdgeState::Reconnecting);
        assert_ne!(degraded.state, EdgeState::Connected);
        assert!(
            supervisor.current().is_none(),
            "the torn-down session must not stay in the slot"
        );
    }
}
