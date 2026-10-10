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
//! face (hello version 2). This client only picks a TCP probe, and fails
//! that probe over onto another TCP face when the probe itself cannot
//! connect. An edge that refuses version 2 falls back to version-1
//! [`EdgeSession::connect_first_available`] from index 0.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mirage_client_core::relay::TunnelCompat;
use mirage_control_plane::{
    CountryCode, EdgeHello, EdgeOutcome, EdgeRefusal, EdgeSessionError as WireError, EdgeWelcome,
    ExitPreference, StreamKind, StreamOpen, StreamTarget,
};
use mirage_core::config::{AuthMode, MirageEndpointConfig, TransportKind};
use mirage_core::error::MirageError;
use mirage_mux::tokio_compat::{SessionHandle, TokioMuxStream};
use mirage_protocol::traits::{TcpProtocol, Tunnel};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};

use crate::artefact::{ArtefactEntrance, ArtefactError, ArtefactMode};
use crate::dial::StaticKeyProtocol;
use crate::status::{EdgeReason, EdgeStatusFile};

/// Version 1 of this client's edge-session wire — see [`EdgeHello::version`].
/// A version-2 dial is a separate path ([`EntranceDialer`]); this constant
/// stays the version an old edge still understands.
const EDGE_SESSION_WIRE_VERSION: u8 = 1;
const EDGE_SESSION_WIRE_VERSION_V2: u8 = 2;

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

    #[error("edge session: edge assigned port {port} which is not a tcp entrance on the artefact")]
    AssignedFaceMissing { port: u16 },

    #[error("edge session: {count} tcp entrances exceeds the {max} a version-2 hello can offer")]
    TooManyFaces { count: usize, max: usize },
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

/// One live session to the customer's own edge.
pub struct EdgeSession {
    session: SessionHandle,
    welcome: EdgeWelcome,
    /// Which entrance this session dialed — logs only.
    entrance_label: String,
}

impl EdgeSession {
    /// Dial `entrance`, complete the V9 handshake with the subscriber's
    /// persistent static key, exchange `EdgeHello`/`EdgeWelcome`, and — on
    /// [`EdgeOutcome::Accepted`] — upgrade to a mux session. Refuses BY NAME
    /// (never a bare IO error) when the edge sends
    /// [`EdgeOutcome::Refused`].
    pub async fn connect(
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
            match Self::connect(&entrances[idx], mesh_id, client_static_priv, exit).await {
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

    /// `true` once the underlying mux session has closed (peer GoAway, EOF,
    /// or a transport error) — the caller (`EdgeSessionSupervisor`) redials.
    pub fn is_closed(&self) -> bool {
        self.session.is_closed()
    }

    /// Open one mux stream for a TCP flow, writing the [`StreamOpen`] header
    /// as its first bytes. `target` is the raw `host:port` string the flow
    /// asked for — a domain name travels through unresolved so DNS resolves
    /// at the exit (see `crate::socks5`).
    pub async fn open_tcp_stream(&self, target: &str) -> Result<TokioMuxStream, EdgeStreamError> {
        let target = StreamTarget::new(target).map_err(EdgeStreamError::BadTarget)?;
        let stream = self.session.open_stream().await?;
        let mut stream = TokioMuxStream(stream);
        let open = StreamOpen { kind: StreamKind::Tcp, target };
        stream.write_all(&open.encode()).await?;
        Ok(stream)
    }

    /// Open one mux stream for a UDP flow, writing the [`StreamOpen`] header
    /// as its first bytes — mirrors [`Self::open_tcp_stream`] exactly,
    /// `kind: StreamKind::Udp` instead, wrapped in
    /// [`crate::udp_flow::EdgeUdpFlow`]'s `[2B len][payload]` datagram
    /// framing. `target` is this flow's ONE fixed destination for its whole
    /// life — see `crate::udp_associate`.
    pub async fn open_udp_flow(&self, target: &str) -> Result<crate::udp_flow::EdgeUdpFlow, EdgeStreamError> {
        let target = StreamTarget::new(target).map_err(EdgeStreamError::BadTarget)?;
        let stream = self.session.open_stream().await?;
        let mut stream = TokioMuxStream(stream);
        let open = StreamOpen { kind: StreamKind::Udp, target };
        stream.write_all(&open.encode()).await?;
        Ok(crate::udp_flow::EdgeUdpFlow::new(stream))
    }

    #[cfg(test)]
    pub(crate) fn for_test(session: SessionHandle, welcome: EdgeWelcome) -> Self {
        Self { session, welcome, entrance_label: "test".to_owned() }
    }
}

/// Open the V9 tunnel to one entrance. Camouflage stays on this dial; it is
/// not something the edge-session hello carries.
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
    let ep_config = MirageEndpointConfig {
        transport: TransportKind::Tcp,
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

fn upgrade_session(tunnel: Box<dyn Tunnel>, welcome: EdgeWelcome, entrance_label: String) -> EdgeSession {
    let compat = TunnelCompat::wrap(tunnel);
    let session = SessionHandle::client(compat);
    EdgeSession { session, welcome, entrance_label }
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

/// TCP faces only, in artefact order, as `(index, port)`. UDP-shaped faces,
/// including `raw_udp`, are not offered and not probed.
fn offered_tcp_ports(entrances: &[ArtefactEntrance]) -> Result<Vec<(usize, u16)>, EdgeDialError> {
    let mut offered = Vec::new();
    for (idx, entrance) in entrances.iter().enumerate() {
        if entrance.camouflage.is_udp() {
            continue;
        }
        offered.push((idx, entrance_tcp_port(entrance)?));
    }
    Ok(offered)
}

fn probe_origin(len: usize) -> usize {
    let tick = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = PROBE_DRAW.fetch_add(1, Ordering::Relaxed);
    let mixed = tick.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(seq);
    (mixed as usize) % len
}

/// Version-2 hello on one face. Does not upgrade to mux: the caller keeps
/// the tunnel only when this face is the session.
async fn dial_v2_face(
    entrance: &ArtefactEntrance,
    mesh_id: &[u8],
    client_static_priv: [u8; 32],
    exit: ExitPreference,
    commit: bool,
    offered: &[u16],
) -> Result<(Box<dyn Tunnel>, EdgeWelcome), EdgeDialError> {
    let mut tunnel = open_entrance_tunnel(entrance, mesh_id, client_static_priv).await?;
    let hello = EdgeHello {
        version: EDGE_SESSION_WIRE_VERSION_V2,
        exit,
        flags: if commit { EdgeHello::FLAG_COMMIT } else { 0 },
        offered_ports: offered.to_vec(),
    };
    let bytes = match hello.encode() {
        Ok(bytes) => bytes,
        Err(e) => {
            let _ = tunnel.close().await;
            return Err(EdgeDialError::MalformedHello(e));
        }
    };
    if let Err(e) = tunnel.send(&bytes).await {
        let _ = tunnel.close().await;
        return Err(e.into());
    }
    let welcome = match read_welcome(tunnel.as_mut()).await {
        Ok(welcome) => welcome,
        Err(e) => {
            let _ = tunnel.close().await;
            return Err(e);
        }
    };
    match interpret_welcome(welcome) {
        Ok(welcome) => Ok((tunnel, welcome)),
        Err(e) => {
            let _ = tunnel.close().await;
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
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// `SessionHandle` exposes only a synchronous `is_closed()`, no async
/// "wait until closed" — the driver polls at this interval.
const LIVENESS_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// One way to obtain a fresh [`EdgeSession`] — the dial the supervisor loop
/// repeats. The production impl dials the artefact's entrances; the seam
/// exists so the loop's published state is testable without a live edge.
#[async_trait::async_trait]
pub(crate) trait EdgeDialer: Send + Sync + 'static {
    /// Dial a session. `start_idx` is not the session policy: the production
    /// dialer ignores it and lets the edge assign the face. Returns the
    /// index of the entrance that carries the live session.
    async fn dial(&self, start_idx: usize) -> Result<(usize, EdgeSession), EdgeDialError>;
}

/// [`EdgeDialer`] over the artefact's own TCP-shaped entrances.
struct EntranceDialer {
    entrances: Vec<ArtefactEntrance>,
    mesh_id: Vec<u8>,
    client_static_priv: [u8; 32],
    exit: ExitPreference,
}

#[async_trait::async_trait]
impl EdgeDialer for EntranceDialer {
    async fn dial(&self, _start_idx: usize) -> Result<(usize, EdgeSession), EdgeDialError> {
        let offered = offered_tcp_ports(&self.entrances)?;
        if offered.is_empty() {
            return Err(EdgeDialError::NoEntrances);
        }
        if offered.len() > EdgeHello::MAX_OFFERED_PORTS {
            return Err(EdgeDialError::TooManyFaces {
                count: offered.len(),
                max: EdgeHello::MAX_OFFERED_PORTS,
            });
        }
        let ports: Vec<u16> = offered.iter().map(|(_, port)| *port).collect();
        let start = probe_origin(offered.len());
        let mut last_err = EdgeDialError::NoEntrances;
        for offset in 0..offered.len() {
            let slot = (start + offset) % offered.len();
            let (idx, probe_port) = offered[slot];
            let probe_addr = self.entrances[idx].addr.clone();
            match dial_v2_face(
                &self.entrances[idx],
                &self.mesh_id,
                self.client_static_priv,
                self.exit,
                false,
                &ports,
            )
            .await
            {
                Ok((mut tunnel, welcome)) => {
                    if let Some(port) = welcome.assigned_face_port {
                        if port != probe_port {
                            let _ = tunnel.close().await;
                            let Some(target) = offered.iter().copied().find(|(_, face_port)| *face_port == port) else {
                                return Err(EdgeDialError::AssignedFaceMissing { port });
                            };
                            let (tunnel, welcome) = dial_v2_face(
                                &self.entrances[target.0],
                                &self.mesh_id,
                                self.client_static_priv,
                                self.exit,
                                true,
                                &ports,
                            )
                            .await?;
                            let session = upgrade_session(tunnel, welcome, self.entrances[target.0].addr.clone());
                            log_assigned(&probe_addr, &session);
                            return Ok((target.0, session));
                        }
                    }
                    let session = upgrade_session(tunnel, welcome, probe_addr.clone());
                    log_assigned(&probe_addr, &session);
                    return Ok((idx, session));
                }
                Err(e) => match e {
                    EdgeDialError::Refused(RefusalDisplay(EdgeRefusal::UnsupportedVersion { .. })) => {
                        info!(probe = %probe_addr, "edge session: edge refused version 2, falling back to version 1");
                        let (fallback_idx, session) = EdgeSession::connect_first_available(
                            &self.entrances,
                            &self.mesh_id,
                            self.client_static_priv,
                            self.exit,
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
                            "edge session: probe failed, trying next tcp face"
                        );
                        last_err = other;
                    }
                },
            }
        }
        Err(last_err)
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
}

impl Drop for EdgeSessionSupervisor {
    /// The dial loop lives exactly as long as its supervisor.
    fn drop(&mut self) {
        self.driver.abort();
    }
}

impl EdgeSessionSupervisor {
    /// Spawn the background dial/reconnect loop over `entrances` (already
    /// filtered to TCP-shaped faces by the caller — see `crate::assembly`).
    pub fn spawn(
        entrances: Vec<ArtefactEntrance>,
        mesh_id: Vec<u8>,
        client_static_priv: [u8; 32],
        exit: ExitPreference,
    ) -> Arc<Self> {
        Self::spawn_with_dialer(EntranceDialer { entrances, mesh_id, client_static_priv, exit }, exit)
    }

    /// [`Self::spawn`] over any [`EdgeDialer`]; `requested` is the exit the
    /// status file reports as asked-for.
    pub(crate) fn spawn_with_dialer(dialer: impl EdgeDialer, requested: ExitPreference) -> Arc<Self> {
        let (slot, _slot_rx) = tokio::sync::watch::channel(None);
        let (status, _status_rx) =
            tokio::sync::watch::channel(EdgeStatusFile::connecting(&requested));
        let driver = tokio::spawn(drive(dialer, slot.clone(), status.clone()));
        Arc::new(Self { slot, status, driver })
    }

    /// A receiver for the connector's published state. Its current value is
    /// always the latest transition; see `crate::status`.
    pub fn status(&self) -> tokio::sync::watch::Receiver<EdgeStatusFile> {
        self.status.subscribe()
    }

    /// The current live session, if any — does not wait.
    pub fn current(&self) -> Option<Arc<EdgeSession>> {
        self.slot.borrow().clone()
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
async fn drive(
    dialer: impl EdgeDialer,
    slot: tokio::sync::watch::Sender<Option<Arc<EdgeSession>>>,
    status: tokio::sync::watch::Sender<EdgeStatusFile>,
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
                status.send_modify(|s| s.set_connected(session.accepted_exit(), session.countries()));
                while !session.is_closed() {
                    tokio::time::sleep(LIVENESS_POLL_INTERVAL).await;
                }
                warn!("edge session: session closed, reconnecting");
                slot.send_replace(None);
                status.send_modify(EdgeStatusFile::set_reconnecting);
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
        Arc::new(Self { slot, status, driver })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mirage_protocol::traits::TransportInfo;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;

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
        }
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

    #[test]
    fn offered_tcp_ports_skip_udp_faces() {
        use mirage_core::config::TransportCamouflage;
        let face = |addr: &str, camouflage: TransportCamouflage| ArtefactEntrance {
            addr: addr.to_owned(),
            server_pubkey_hex: "ab".repeat(32),
            camouflage,
        };
        let entrances = vec![
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
        ];
        let offered = offered_tcp_ports(&entrances).expect("ports");
        assert_eq!(offered, vec![(0, 80), (2, 443), (4, 8080)]);
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
}
