//! SOCKS5 UDP ASSOCIATE (RFC 1928 §7) — the local relay half of one UDP
//! association: binds a loopback UDP relay socket, replies with its
//! `BND.ADDR`/`BND.PORT` on the control TCP stream, then pumps each
//! per-destination flow into one `StreamKind::Udp` mux stream on the live
//! edge session ([`crate::udp_flow::EdgeUdpFlow`]).
//!
//! One association = one client-facing relay socket + a destination-keyed
//! table of flows — RFC 1928 §7 lets one ASSOCIATE serve any number of
//! distinct destinations, each carrying its own `DST.ADDR`/`DST.PORT` in its
//! own datagram. A flow is created lazily on the first datagram to a new
//! destination and torn down after [`UDP_FLOW_IDLE_TIMEOUT`] of silence in
//! BOTH directions — matching the edge/exit's own `UDP_SESSION_TIMEOUT`
//! (`docs/mirage/plans/2026-09-25-edge-udp.md` §1 "NAT-style flow
//! lifetime"). The WHOLE association dies the moment its control TCP
//! connection closes or errors, per RFC 1928 §7's own lifetime rule ("the
//! UDP association terminates when the TCP connection that the UDP
//! ASSOCIATE request arrived on terminates") — driven by one
//! `tokio::select!` between reading the control stream to EOF and the relay
//! loop.
//!
//! **Fragmentation (`FRAG != 0`) is refused, not reassembled** — dropped and
//! counted, logged once process-wide. **MTU-safe payload cap**
//! ([`UDP_PAYLOAD_MTU_SAFE_LEN`] = 1200, matching
//! `mirage_protocol::core::udp_handshake::SAFE_UDP_HANDSHAKE_PAYLOAD_LEN`'s
//! own reasoning) is enforced the same way. Both are UDP already having no
//! delivery guarantee — a dropped malformed/oversized datagram is
//! indistinguishable from ordinary loss to any correctly-behaving peer.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{mpsc, watch, Mutex as AsyncMutex};
use tracing::debug;

use crate::scope::EgressScope;
use crate::session::{EdgeSession, EdgeSessionSupervisor, EdgeStreamError};
use crate::socks5;
use crate::udp_flow::EdgeUdpFlow;

/// Idle timeout for one client-facing UDP flow (one destination inside one
/// association) — matches the edge/exit's own `UDP_SESSION_TIMEOUT`.
pub const UDP_FLOW_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// MTU-safe payload cap enforced at this SOCKS5 ingestion boundary — see the
/// module doc.
pub const UDP_PAYLOAD_MTU_SAFE_LEN: usize = 1200;

/// Cap on distinct destinations one association will track — bounds one
/// SOCKS5 client's worst case (e.g. a UDP port scan through one ASSOCIATE)
/// to a bounded number of concurrently open mux streams.
pub const MAX_DESTINATIONS_PER_ASSOCIATION: usize = 32;

/// How long routing a datagram to a brand-new destination waits for a live
/// edge session before giving up on that one datagram — matches
/// `crate::socks5::CONNECT_SESSION_WAIT`'s own value for the same reasoning.
const FLOW_SESSION_WAIT: Duration = Duration::from_secs(5);

/// Largest single `recv_from` this relay reads — the largest UDP datagram
/// representable at all over IPv4, matching
/// `mirage_control_plane::MAX_UDP_DATAGRAM_PAYLOAD`'s own header-inclusive
/// ceiling with headroom for the SOCKS5 header itself.
const RELAY_RECV_BUF_LEN: usize = 65535;

/// Bound on one flow's outbound queue depth — a slow edge session applies
/// backpressure by dropping, exactly like ordinary UDP loss, never by
/// blocking the relay's single receive loop.
const FLOW_OUTBOUND_QUEUE_LEN: usize = 64;

/// How often to repeat one unanswered payload on the same stream.
/// The first send is immediate. The edge drops any datagram bytes that
/// share the `StreamOpen` read, and it does not read the stream again
/// until the datagram dial returns. A copy written after that read is
/// what the pump relays. The payload stays UDP.
const RESEND_EVERY: Duration = Duration::from_millis(1000);

/// Copies with no reply before the flow dials one replacement stream.
/// A dial that never returns keeps the first stream silent, and every
/// copy already queued on it is discarded with the header when that
/// read finally happens.
const SILENCE_REOPEN_AFTER: u32 = 4;

/// Stop repeating after this many copies of the same unanswered payload.
const MAX_RESENDS: u32 = 16;

/// After a failed stream is replaced, wait this long before the first
/// payload so it is not sitting in the new `StreamOpen` buffer.
const REOPEN_GAP: Duration = Duration::from_millis(200);

/// A dead mux stream (send or recv error) may be dialled again this many
/// times. Silence is not a dead stream: the copies above stay on it.
const MAX_STREAM_REOPENS: u8 = 1;

/// Every way serving one UDP association fails at the control-stream level.
#[derive(Debug, Error)]
pub enum UdpAssociateError {
    #[error("socks5 udp: no live edge session")]
    NoLiveSession,
    #[error(transparent)]
    Stream(#[from] EdgeStreamError),
}

#[derive(Default)]
struct UdpAssociationStats {
    frag_dropped: AtomicU64,
    oversized_dropped: AtomicU64,
}

/// Logged once, process-wide, per drop reason — a standing policy, not a
/// per-datagram event worth repeating in the log.
static FRAG_DROPPED_LOGGED: std::sync::Once = std::sync::Once::new();
static OVERSIZED_DROPPED_LOGGED: std::sync::Once = std::sync::Once::new();

fn log_frag_dropped_once() {
    FRAG_DROPPED_LOGGED.call_once(|| {
        tracing::warn!(
            "socks5 udp: dropping a fragmented datagram (FRAG != 0) — fragmentation is not \
             supported, see docs/mirage/plans/2026-09-25-edge-udp.md §4"
        );
    });
}

fn log_oversized_dropped_once() {
    OVERSIZED_DROPPED_LOGGED.call_once(|| {
        tracing::warn!(
            max = UDP_PAYLOAD_MTU_SAFE_LEN,
            "socks5 udp: dropping a datagram whose payload exceeds the MTU-safe cap"
        );
    });
}

/// One SOCKS5 UDP ASSOCIATE. Owns the bound local relay [`UdpSocket`] and a
/// destination-keyed flow table, each entry one [`EdgeUdpFlow`] driven by
/// its own [`run_flow`] task.
struct UdpAssociation {
    relay_socket: Arc<UdpSocket>,
    supervisor: Arc<EdgeSessionSupervisor>,
    flows: AsyncMutex<HashMap<String, mpsc::Sender<Vec<u8>>>>,
    /// The most recently seen client source address — every reply goes
    /// back there. In practice one association serves exactly one local
    /// client socket for its whole life.
    client_addr: watch::Sender<Option<SocketAddr>>,
    stats: UdpAssociationStats,
    idle_timeout: Duration,
    scope: Arc<EgressScope>,
    /// Process that opened the TCP control connection. `None` if it could not be named.
    exe: Option<String>,
}

impl UdpAssociation {
    #[cfg(test)]
    fn new(relay_socket: Arc<UdpSocket>, supervisor: Arc<EdgeSessionSupervisor>) -> Self {
        Self::with_idle_timeout(relay_socket, supervisor, UDP_FLOW_IDLE_TIMEOUT)
    }

    #[cfg(test)]
    fn with_idle_timeout(
        relay_socket: Arc<UdpSocket>,
        supervisor: Arc<EdgeSessionSupervisor>,
        idle_timeout: Duration,
    ) -> Self {
        Self::with_scope(relay_socket, supervisor, idle_timeout, Arc::new(EgressScope::all()), None)
    }

    fn with_scope(
        relay_socket: Arc<UdpSocket>,
        supervisor: Arc<EdgeSessionSupervisor>,
        idle_timeout: Duration,
        scope: Arc<EgressScope>,
        exe: Option<String>,
    ) -> Self {
        let (client_addr, _rx) = watch::channel(None);
        Self {
            relay_socket,
            supervisor,
            flows: AsyncMutex::new(HashMap::new()),
            client_addr,
            stats: UdpAssociationStats::default(),
            idle_timeout,
            scope,
            exe,
        }
    }

    /// Count of datagrams dropped for `FRAG != 0` since this association was
    /// created — test surface only, never wire-exposed.
    #[cfg(test)]
    fn frag_dropped_count(&self) -> u64 {
        self.stats.frag_dropped.load(Ordering::Relaxed)
    }

    /// Count of datagrams dropped for exceeding [`UDP_PAYLOAD_MTU_SAFE_LEN`]
    /// since this association was created — test surface only.
    #[cfg(test)]
    fn oversized_dropped_count(&self) -> u64 {
        self.stats.oversized_dropped.load(Ordering::Relaxed)
    }

    /// Read datagrams off `relay_socket` forever, routing each to its flow
    /// (opening a new one on first sight of a destination) until the relay
    /// socket errors. Runs until the caller's `tokio::select!` drops it (the
    /// control stream closing wins the race).
    async fn run_relay_loop(self: &Arc<Self>) {
        let mut buf = vec![0u8; RELAY_RECV_BUF_LEN];
        loop {
            let (n, peer) = match self.relay_socket.recv_from(&mut buf).await {
                Ok(v) => v,
                Err(e) => {
                    debug!(error = %e, "socks5 udp: relay socket recv failed — association ending");
                    return;
                }
            };
            // `send` (not `send_replace`) would silently no-op whenever
            // there are zero live receivers — which is exactly the window
            // between construction and the first `subscribe()` a brand-new
            // flow's `route_to_flow` performs. `send_replace` always stores
            // the value regardless of receiver count, so a receiver
            // subscribed AFTER this call still sees it.
            self.client_addr.send_replace(Some(peer));

            let request = match socks5::parse_socks5_udp_request(&buf[..n]) {
                Ok(req) => req,
                Err(e) => {
                    debug!(error = %e, "socks5 udp: dropping malformed datagram");
                    continue;
                }
            };
            if request.frag != 0 {
                self.stats.frag_dropped.fetch_add(1, Ordering::Relaxed);
                log_frag_dropped_once();
                continue;
            }
            if request.data.len() > UDP_PAYLOAD_MTU_SAFE_LEN {
                self.stats.oversized_dropped.fetch_add(1, Ordering::Relaxed);
                log_oversized_dropped_once();
                continue;
            }

            if let Err(e) = self.route_to_flow(request).await {
                debug!(error = %e, "socks5 udp: failed to route datagram to its flow");
            }
        }
    }

    /// Forward one already-validated request to its destination's flow,
    /// opening a fresh mux stream on first sight of that destination.
    async fn route_to_flow(self: &Arc<Self>, request: socks5::Socks5UdpRequest) -> Result<(), UdpAssociateError> {
        if !self.scope.allows_flow(&request.target, self.exe.as_deref()) {
            debug!(
                target = %request.target,
                "socks5 udp: destination outside the selected services — dropping"
            );
            return Ok(());
        }
        let mut flows = self.flows.lock().await;
        if let Some(sender) = flows.get(&request.target) {
            let _ = sender.try_send(request.data);
            return Ok(());
        }
        if flows.len() >= MAX_DESTINATIONS_PER_ASSOCIATION {
            tracing::warn!(
                target = %request.target,
                max = MAX_DESTINATIONS_PER_ASSOCIATION,
                "socks5 udp: too many distinct destinations for one association — dropping"
            );
            return Ok(());
        }

        let session = self
            .supervisor
            .wait_for_session(FLOW_SESSION_WAIT)
            .await
            .ok_or(UdpAssociateError::NoLiveSession)?;
        let flow = session.open_udp_flow(&request.target).await?;

        let (tx, rx) = mpsc::channel::<Vec<u8>>(FLOW_OUTBOUND_QUEUE_LEN);
        let _ = tx.try_send(request.data);
        flows.insert(request.target.clone(), tx);
        drop(flows);

        let relay_socket = Arc::clone(&self.relay_socket);
        let client_addr = self.client_addr.subscribe();
        let association = Arc::clone(self);
        let target = request.target;
        let idle_timeout = self.idle_timeout;
        let session_for_flow = Arc::clone(&session);
        tokio::spawn(async move {
            run_flow(
                flow,
                rx,
                relay_socket,
                client_addr,
                request.header_echo,
                idle_timeout,
                session_for_flow,
                target.clone(),
            )
            .await;
            association.flows.lock().await.remove(&target);
        });

        Ok(())
    }
}

/// Pump one destination's flow both directions until the edge stream ends,
/// errors, or [`UDP_FLOW_IDLE_TIMEOUT`] elapses with no traffic in either
/// direction. Every reply frame from the edge is wrapped back into a SOCKS5
/// UDP §7 header — `header_echo` (the original request's `ATYP+ADDR+PORT`)
/// echoed verbatim — and sent to the association's current client address.
async fn run_flow(
    mut flow: EdgeUdpFlow,
    mut outbound_rx: mpsc::Receiver<Vec<u8>>,
    relay_socket: Arc<UdpSocket>,
    client_addr: watch::Receiver<Option<SocketAddr>>,
    header_echo: Vec<u8>,
    idle_timeout: Duration,
    session: Arc<EdgeSession>,
    target: String,
) {
    // `opening` is the latest datagram with no reply yet. `copies` counts
    // repeats already sent. A reply clears both.
    let mut opening: Option<Vec<u8>> = None;
    let mut retry_at: Option<tokio::time::Instant> = None;
    let mut copies: u32 = 0;
    let mut silence_reopened = false;
    let mut reopens_left = MAX_STREAM_REOPENS;
    // A second dial races the first. One held chain can sit in the datagram
    // handshake for its whole timeout while the next chain answers at once.
    // The edge picks the chain when the stream is accepted, so the spare has
    // to be open while the first dial is still in flight.
    let (spare_tx, mut spare_rx) = mpsc::channel::<(EdgeUdpFlow, Vec<u8>)>(1);
    let mut spare_task: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        let retry_wait = async {
            match retry_at {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            biased;
            maybe_payload = outbound_rx.recv() => {
                match maybe_payload {
                    Some(payload) => {
                        let first = opening.is_none();
                        opening = Some(payload.clone());
                        if first {
                            copies = 0;
                            silence_reopened = false;
                            retry_at = Some(tokio::time::Instant::now() + RESEND_EVERY);
                        }
                        if (first || copies > 0) && !send_payload(&mut flow, &payload).await {
                            if !reopen_stream(
                                &mut flow,
                                &session,
                                &target,
                                &mut reopens_left,
                                &mut retry_at,
                            )
                            .await
                            {
                                return;
                            }
                        }
                        if first && spare_task.is_none() {
                            spare_task = Some(spawn_spare(
                                Arc::clone(&session),
                                target.clone(),
                                payload,
                                spare_tx.clone(),
                            ));
                        }
                    }
                    None => return, // Sender dropped — association is shutting down.
                }
            }
            _ = retry_wait => {
                retry_at = None;
                let Some(payload) = opening.clone() else {
                    continue;
                };
                if !silence_reopened && copies >= SILENCE_REOPEN_AFTER && reopens_left > 0 {
                    debug!(copies, "socks5 udp: no reply, opening another stream");
                    silence_reopened = true;
                    copies = 0;
                    if !reopen_stream(
                        &mut flow,
                        &session,
                        &target,
                        &mut reopens_left,
                        &mut retry_at,
                    )
                    .await
                    {
                        return;
                    }
                    continue;
                }
                if !send_payload(&mut flow, &payload).await {
                    if !reopen_stream(
                        &mut flow,
                        &session,
                        &target,
                        &mut reopens_left,
                        &mut retry_at,
                    )
                    .await
                    {
                        return;
                    }
                    continue;
                }
                copies += 1;
                debug!(copies, "socks5 udp: resend unanswered datagram");
                if copies < MAX_RESENDS {
                    retry_at = Some(tokio::time::Instant::now() + RESEND_EVERY);
                }
            }
            recv_result = flow.recv() => {
                match recv_result {
                    Ok(payload) => {
                        if let Some(task) = spare_task.take() {
                            task.abort();
                        }
                        opening = None;
                        retry_at = None;
                        copies = 0;
                        silence_reopened = false;
                        reopens_left = MAX_STREAM_REOPENS;
                        deliver_reply(&payload, &header_echo, &client_addr, &relay_socket).await;
                    }
                    Err(e) => {
                        debug!(error = %e, "socks5 udp: flow recv from edge failed");
                        if opening.is_none() {
                            return;
                        }
                        if !reopen_stream(
                            &mut flow,
                            &session,
                            &target,
                            &mut reopens_left,
                            &mut retry_at,
                        )
                        .await
                        {
                            return;
                        }
                    }
                }
            }
            won = spare_rx.recv() => {
                let Some((winner, payload)) = won else {
                    continue;
                };
                spare_task.take();
                flow = winner;
                opening = None;
                retry_at = None;
                copies = 0;
                silence_reopened = false;
                reopens_left = MAX_STREAM_REOPENS;
                debug!("socks5 udp: spare stream answered");
                deliver_reply(&payload, &header_echo, &client_addr, &relay_socket).await;
            }
            _ = tokio::time::sleep(idle_timeout) => {
                debug!("socks5 udp: flow idle timeout — ending flow");
                return;
            }
        }
    }
}

fn spawn_spare(
    session: Arc<EdgeSession>,
    target: String,
    payload: Vec<u8>,
    win_tx: mpsc::Sender<(EdgeUdpFlow, Vec<u8>)>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut spare = match session.open_udp_flow(&target).await {
            Ok(flow) => flow,
            Err(e) => {
                debug!(error = %e, "socks5 udp: spare stream failed");
                return;
            }
        };
        tokio::time::sleep(REOPEN_GAP).await;
        debug!("socks5 udp: spare stream armed");
        for copies in 1..=8 {
            if !send_payload(&mut spare, &payload).await {
                debug!("socks5 udp: spare send failed");
                return;
            }
            debug!(copies, "socks5 udp: spare datagram");
            tokio::select! {
                result = spare.recv() => {
                    match result {
                        Ok(reply) => {
                            let _ = win_tx.send((spare, reply)).await;
                        }
                        Err(e) => debug!(error = %e, "socks5 udp: spare recv failed"),
                    }
                    return;
                }
                _ = tokio::time::sleep(RESEND_EVERY) => {}
            }
        }
    })
}

async fn deliver_reply(
    payload: &[u8],
    header_echo: &[u8],
    client_addr: &watch::Receiver<Option<SocketAddr>>,
    relay_socket: &UdpSocket,
) {
    let addr = *client_addr.borrow();
    let Some(addr) = addr else {
        return;
    };
    let mut reply = Vec::with_capacity(3 + header_echo.len() + payload.len());
    reply.extend_from_slice(&[0x00, 0x00, 0x00]); // RSV RSV FRAG=0
    reply.extend_from_slice(header_echo);
    reply.extend_from_slice(payload);
    let _ = relay_socket.send_to(&reply, addr).await;
}

async fn send_payload(flow: &mut EdgeUdpFlow, payload: &[u8]) -> bool {
    match flow.send(payload).await {
        Ok(()) => true,
        Err(e) => {
            debug!(error = %e, "socks5 udp: flow send to edge failed");
            false
        }
    }
}

/// The mux stream failed. Dial another and restart the header gap. Returns
/// false when the flow should end. Does not send: the gap timer does.
async fn reopen_stream(
    flow: &mut EdgeUdpFlow,
    session: &EdgeSession,
    target: &str,
    reopens_left: &mut u8,
    retry_at: &mut Option<tokio::time::Instant>,
) -> bool {
    if *reopens_left == 0 {
        debug!("socks5 udp: edge stream failed, no reopen left");
        return false;
    }
    *reopens_left -= 1;
    debug!("socks5 udp: edge stream failed, opening another");
    match session.open_udp_flow(target).await {
        Ok(new_flow) => {
            *flow = new_flow;
            *retry_at = Some(tokio::time::Instant::now() + REOPEN_GAP);
            true
        }
        Err(e) => {
            debug!(error = %e, "socks5 udp: replacement stream failed — ending flow");
            false
        }
    }
}

/// Drives one association end to end: binds the relay socket, replies
/// success on `control_stream` with its `BND.ADDR`/`BND.PORT`, then runs
/// until `control_stream` closes/errors or the relay loop itself ends (RFC
/// 1928 §7's own association lifetime rule).
pub async fn serve_udp_association(
    mut control_stream: TcpStream,
    supervisor: Arc<EdgeSessionSupervisor>,
    scope: Arc<EgressScope>,
) -> std::io::Result<()> {
    let relay_socket = match UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await {
        Ok(socket) => socket,
        Err(e) => {
            let _ = socks5::write_reply(&mut control_stream, socks5::REP_GENERAL_FAILURE).await;
            return Err(e);
        }
    };
    let bound = relay_socket.local_addr()?;
    socks5::write_udp_associate_reply(&mut control_stream, socks5::REP_SUCCESS, bound).await?;

    let exe = crate::peer::owning_exe(&control_stream);
    let association = Arc::new(UdpAssociation::with_scope(
        Arc::new(relay_socket),
        supervisor,
        UDP_FLOW_IDLE_TIMEOUT,
        scope,
        exe,
    ));

    tokio::select! {
        _ = read_control_stream_to_close(&mut control_stream) => {
            debug!("socks5 udp: control connection closed — association ending");
        }
        _ = association.run_relay_loop() => {
            debug!("socks5 udp: relay loop ended on its own — association ending");
        }
    }
    Ok(())
}

/// Read `stream` to EOF or error — RFC 1928 §7's own signal that the
/// association driving it must end. The bytes themselves are discarded: a
/// UDP ASSOCIATE's control connection carries no further requests.
async fn read_control_stream_to_close(stream: &mut TcpStream) {
    let mut discard = [0u8; 256];
    loop {
        match stream.read(&mut discard).await {
            Ok(0) | Err(_) => return,
            Ok(_) => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mirage_control_plane::{encode_udp_datagram_frame, CountryCode, EdgeOutcome, EdgeWelcome, ExitPreference, StreamKind};
    use mirage_mux::tokio_compat::{SessionHandle, TokioIoCompat, TokioMuxStream};
    use tokio::io::AsyncWriteExt as _;

    fn sample_welcome() -> EdgeWelcome {
        EdgeWelcome {
            version: 3,
            countries: vec![CountryCode::new("US").expect("valid")],
            outcome: EdgeOutcome::Accepted { exit: ExitPreference::Auto },
            assigned_face_port: Some(1),
            carries_datagrams: Some(true),
        }
    }

    /// Poll `cond` until it is true or `timeout` elapses — avoids a fixed
    /// sleep racing against the async flow-open/idle-timeout machinery.
    async fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if cond() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    // ── FRAG refusal and oversized-payload refusal — no live session needed,
    // both are refused before any flow is ever opened ─────────────────────

    fn no_session_supervisor() -> Arc<EdgeSessionSupervisor> {
        EdgeSessionSupervisor::spawn(Vec::new(), b"MESH2".to_vec(), [0x11u8; 32], ExitPreference::Auto)
    }

    #[tokio::test]
    async fn fragmented_datagrams_are_dropped_and_counted() {
        let relay_socket = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.expect("bind relay"));
        let association = Arc::new(UdpAssociation::new(Arc::clone(&relay_socket), no_session_supervisor()));
        let assoc_for_loop = Arc::clone(&association);
        tokio::spawn(async move { assoc_for_loop.run_relay_loop().await });

        let client_socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.expect("bind client");
        let mut request = vec![0x00, 0x00, 0x01, 0x01]; // FRAG=1, ATYP=IPv4.
        request.extend_from_slice(&Ipv4Addr::LOCALHOST.octets());
        request.extend_from_slice(&1234u16.to_be_bytes());
        request.extend_from_slice(b"payload");
        client_socket
            .send_to(&request, relay_socket.local_addr().expect("addr"))
            .await
            .expect("send");

        assert!(
            wait_until(|| association.frag_dropped_count() == 1, Duration::from_secs(2)).await,
            "a FRAG != 0 datagram must be dropped and counted"
        );
        assert_eq!(association.oversized_dropped_count(), 0);
        assert!(
            association.flows.lock().await.is_empty(),
            "a refused datagram must never open a flow"
        );
    }

    #[tokio::test]
    async fn oversized_payloads_are_dropped_and_counted() {
        let relay_socket = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.expect("bind relay"));
        let association = Arc::new(UdpAssociation::new(Arc::clone(&relay_socket), no_session_supervisor()));
        let assoc_for_loop = Arc::clone(&association);
        tokio::spawn(async move { assoc_for_loop.run_relay_loop().await });

        let client_socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.expect("bind client");
        let mut request = vec![0x00, 0x00, 0x00, 0x01]; // FRAG=0, ATYP=IPv4.
        request.extend_from_slice(&Ipv4Addr::LOCALHOST.octets());
        request.extend_from_slice(&1234u16.to_be_bytes());
        request.extend_from_slice(&vec![0xABu8; UDP_PAYLOAD_MTU_SAFE_LEN + 1]);
        client_socket
            .send_to(&request, relay_socket.local_addr().expect("addr"))
            .await
            .expect("send");

        assert!(
            wait_until(|| association.oversized_dropped_count() == 1, Duration::from_secs(2)).await,
            "a payload over the MTU-safe cap must be dropped and counted"
        );
        assert_eq!(association.frag_dropped_count(), 0);
        assert!(association.flows.lock().await.is_empty());
    }

    // ── Round trip over a real mux pair ─────────────────────────────────

    #[tokio::test]
    async fn round_trip_datagram_through_a_real_mux_pair_and_back_as_a_socks5_udp_reply() {
        let (client_io, server_io) = tokio::io::duplex(65536);
        let client_session = SessionHandle::client(TokioIoCompat(client_io));
        let server_session = SessionHandle::server(TokioIoCompat(server_io));
        let edge_session =
            Arc::new(crate::session::EdgeSession::for_test(client_session, sample_welcome()));
        let supervisor = EdgeSessionSupervisor::for_test(edge_session);

        let relay_socket = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.expect("bind relay"));
        let association = Arc::new(UdpAssociation::new(Arc::clone(&relay_socket), supervisor));
        let assoc_for_loop = Arc::clone(&association);
        tokio::spawn(async move { assoc_for_loop.run_relay_loop().await });

        // Simulate the edge's own accept side of the mux stream: read the
        // StreamOpen header, then the one framed datagram, then echo a
        // reply frame back.
        tokio::spawn(async move {
            let stream = server_session.accept_stream().await.expect("edge sees the new stream");
            let mut stream = TokioMuxStream(stream);

            let mut kind_and_len = [0u8; 3];
            stream.read_exact(&mut kind_and_len).await.expect("read StreamOpen kind+len");
            assert_eq!(kind_and_len[0], StreamKind::Udp.encode());
            let target_len = u16::from_be_bytes([kind_and_len[1], kind_and_len[2]]) as usize;
            let mut target_buf = vec![0u8; target_len];
            stream.read_exact(&mut target_buf).await.expect("read StreamOpen target");
            assert_eq!(std::str::from_utf8(&target_buf).expect("utf8"), "203.0.113.9:9999");

            let mut frame_len_buf = [0u8; 2];
            stream.read_exact(&mut frame_len_buf).await.expect("read datagram frame len");
            let payload_len = u16::from_be_bytes(frame_len_buf) as usize;
            let mut payload = vec![0u8; payload_len];
            stream.read_exact(&mut payload).await.expect("read datagram payload");
            assert_eq!(payload, b"hello-edge");

            let reply_frame = encode_udp_datagram_frame(b"hello-client").expect("encodes");
            stream.write_all(&reply_frame).await.expect("write reply frame");
        });

        // A fake SOCKS5-UDP peer sending one datagram to our relay.
        let client_socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.expect("bind client");
        let mut request = vec![0x00, 0x00, 0x00, 0x01]; // RSV RSV FRAG=0 ATYP=IPv4.
        request.extend_from_slice(&Ipv4Addr::new(203, 0, 113, 9).octets());
        request.extend_from_slice(&9999u16.to_be_bytes());
        request.extend_from_slice(b"hello-edge");
        client_socket
            .send_to(&request, relay_socket.local_addr().expect("addr"))
            .await
            .expect("send");

        let mut buf = [0u8; 512];
        let (n, _from) = tokio::time::timeout(Duration::from_secs(5), client_socket.recv_from(&mut buf))
            .await
            .expect("reply must arrive before the timeout")
            .expect("recv");
        assert_eq!(&buf[..3], &[0x00, 0x00, 0x00], "RSV RSV FRAG=0");
        assert_eq!(buf[3], 0x01, "ATYP must echo IPv4, matching the request");
        assert_eq!(&buf[4..8], &Ipv4Addr::new(203, 0, 113, 9).octets());
        assert_eq!(u16::from_be_bytes([buf[8], buf[9]]), 9999);
        assert_eq!(&buf[10..n], b"hello-client");
    }

    // ── Idle expiry ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_flow_expires_and_is_evicted_after_its_idle_timeout() {
        let (client_io, server_io) = tokio::io::duplex(65536);
        let client_session = SessionHandle::client(TokioIoCompat(client_io));
        let server_session = SessionHandle::server(TokioIoCompat(server_io));
        let edge_session =
            Arc::new(crate::session::EdgeSession::for_test(client_session, sample_welcome()));
        let supervisor = EdgeSessionSupervisor::for_test(edge_session);

        // Keep the edge side of the mux stream alive for the test's life —
        // otherwise the flow's own `flow.recv()` would end the flow on EOF
        // before the idle timeout ever gets a chance to fire.
        tokio::spawn(async move {
            let _stream = server_session.accept_stream().await.expect("edge sees the new stream");
            std::future::pending::<()>().await;
        });

        let relay_socket = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.expect("bind relay"));
        let association = Arc::new(UdpAssociation::with_idle_timeout(
            Arc::clone(&relay_socket),
            supervisor,
            Duration::from_millis(150),
        ));
        let assoc_for_loop = Arc::clone(&association);
        tokio::spawn(async move { assoc_for_loop.run_relay_loop().await });

        let client_socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.expect("bind client");
        let mut request = vec![0x00, 0x00, 0x00, 0x01]; // RSV RSV FRAG=0 ATYP=IPv4.
        request.extend_from_slice(&Ipv4Addr::new(198, 51, 100, 7).octets());
        request.extend_from_slice(&53u16.to_be_bytes());
        request.extend_from_slice(b"query");
        client_socket
            .send_to(&request, relay_socket.local_addr().expect("addr"))
            .await
            .expect("send");

        // The flow must exist right after the first datagram...
        assert!(
            wait_until(
                || {
                    // A best-effort non-blocking peek: `try_lock` avoids
                    // deadlocking against the relay loop's own async lock.
                    association
                        .flows
                        .try_lock()
                        .map(|flows| flows.contains_key("198.51.100.7:53"))
                        .unwrap_or(false)
                },
                Duration::from_secs(2)
            )
            .await,
            "the flow must be opened after the first datagram to a new destination"
        );

        // ...and gone again once its idle timeout has elapsed with no
        // further traffic in either direction.
        assert!(
            wait_until(
                || {
                    association
                        .flows
                        .try_lock()
                        .map(|flows| flows.is_empty())
                        .unwrap_or(false)
                },
                Duration::from_secs(2)
            )
            .await,
            "an idle flow must be evicted from the association's table"
        );
    }
}
