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
//! [`EdgeSessionSupervisor`] holds the session up across the artefact's
//! several entrance faces (round-robin failover, matching the face sharding
//! this crate already had) and reconnects with exponential backoff whenever
//! the live session closes.

use std::net::SocketAddr;
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

/// This client's own edge-session wire version — see [`EdgeHello::version`].
/// No edge box exists to negotiate against yet (`mirage-edge`, unbuilt per
/// the plan's PART ONE); `1` is the only version this client ever sends.
const EDGE_SESSION_WIRE_VERSION: u8 = 1;

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
        let addr: SocketAddr = entrance.addr.parse().map_err(|e: std::net::AddrParseError| {
            EdgeDialError::BadAddr { addr: entrance.addr.clone(), reason: e.to_string() }
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
        let mut tunnel: Box<dyn Tunnel> = proto
            .connect_with_credential(addr, &server_pubkey, "", Some(entrance.camouflage.clone()), None)
            .await?;

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

        let compat = TunnelCompat::wrap(tunnel);
        let session = SessionHandle::client(compat);

        Ok(Self { session, welcome, entrance_label: entrance.addr.clone() })
    }

    /// Dial across `entrances` round-robin, starting at `start_idx` — the
    /// face sharding this crate already had, applied to picking a dial path
    /// for the ONE persistent session rather than to spreading independent
    /// flows across independent chains. Returns the first entrance that
    /// accepts; on total failure, the LAST entrance's refusal/error (most
    /// informative for a single-entrance artefact, still useful for many).
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

/// Send [`EdgeHello`] and read back the (possibly split-across-reads)
/// [`EdgeWelcome`] — pure transport-level plumbing, factored out so it is
/// testable against a mock [`Tunnel`] with no live V9 handshake.
async fn send_hello_and_read_welcome(
    tunnel: &mut dyn Tunnel,
    exit: ExitPreference,
) -> Result<EdgeWelcome, EdgeDialError> {
    let hello = EdgeHello { version: EDGE_SESSION_WIRE_VERSION, exit };
    tunnel.send(&hello.encode()).await?;
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

/// Reconnect-with-backoff wrapper around [`EdgeSession`]. Holds the current
/// live session (if any) behind a [`tokio::sync::watch`] channel so every
/// caller (the SOCKS5 listener, the TUN feed) sees the same session and the
/// same reconnects.
pub struct EdgeSessionSupervisor {
    slot: tokio::sync::watch::Sender<Option<Arc<EdgeSession>>>,
    _driver: tokio::task::JoinHandle<()>,
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
        let (tx, _rx) = tokio::sync::watch::channel(None);
        let driver_tx = tx.clone();
        let driver = tokio::spawn(async move {
            let mut start_idx = 0usize;
            let mut backoff = INITIAL_BACKOFF;
            loop {
                match EdgeSession::connect_first_available(
                    &entrances,
                    &mesh_id,
                    client_static_priv,
                    exit,
                    start_idx,
                )
                .await
                {
                    Ok((idx, session)) => {
                        info!(entrance = %session.entrance_label(), "edge session: connected");
                        backoff = INITIAL_BACKOFF;
                        start_idx = (idx + 1) % entrances.len();
                        let session = Arc::new(session);
                        let _ = driver_tx.send(Some(Arc::clone(&session)));
                        while !session.is_closed() {
                            tokio::time::sleep(LIVENESS_POLL_INTERVAL).await;
                        }
                        warn!("edge session: session closed, reconnecting");
                        let _ = driver_tx.send(None);
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            backoff_secs = backoff.as_secs(),
                            "edge session: every entrance refused or unreachable, backing off"
                        );
                    }
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        });
        Arc::new(Self { slot: tx, _driver: driver })
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

#[cfg(test)]
impl EdgeSessionSupervisor {
    /// Test-only: a supervisor whose slot already holds a live `session`,
    /// with no dial loop behind it — lets `crate::udp_associate`'s tests
    /// exercise a real mux pair without a real V9 handshake or entrance
    /// dial. The driver task never completes; the test process ending is
    /// the only cleanup this needs.
    pub(crate) fn for_test(session: Arc<EdgeSession>) -> Arc<Self> {
        let (tx, _rx) = tokio::sync::watch::channel(Some(session));
        let driver = tokio::spawn(std::future::pending());
        Arc::new(Self { slot: tx, _driver: driver })
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
            EdgeHello { version: EDGE_SESSION_WIRE_VERSION, exit: ExitPreference::Auto }.encode()
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
