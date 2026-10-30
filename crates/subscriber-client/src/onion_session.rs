//! The onion plane inside the client: an artefact that names a chain, walked
//! by the client itself.
//!
//! The VPN artefact names one box - the edge - and the client dials it. The
//! onion artefact names a chain, and there is no edge to hide behind: the
//! client IS the edge, so it walks the hops itself, signs the chain leg with
//! the chain's operator key, and ends up on the exit's mux session.
//!
//! Everything the rest of the client needs from a session is unchanged - the
//! same mux stream per flow, the same watchdog, the same SOCKS5 listener. The
//! only thing that differs is how a stream's destination is framed, and that
//! travels with the session ([`crate::session::StreamFraming`]).

use mirage_client_core::relay::TunnelCompat;
use mirage_control_plane::{EdgeOutcome, EdgeWelcome, ExitPreference};
use mirage_mux::tokio_compat::SessionHandle;
use mirage_onion_client::{OnionChainSpec, OnionDialError};
use tracing::info;

use crate::session::{EdgeDialError, EdgeDialer, EdgeSession, EdgeSessionSupervisor, StreamFraming};

/// Spawn the supervisor that keeps the chain walked.
///
/// One chain, one session: the hops are fixed by the artefact, so there is
/// nothing to fail over to. The supervisor's dial loop still wraps it - a
/// dropped chain is redialled with the same backoff the edge session uses.
pub(crate) fn spawn_onion(
    spec: OnionChainSpec,
    stream_failure_threshold: u32,
) -> std::sync::Arc<EdgeSessionSupervisor> {
    EdgeSessionSupervisor::spawn_with_custom_dialer(
        OnionDialer::new(spec),
        ExitPreference::Auto,
        stream_failure_threshold,
    )
}

/// Dials the onion chain the artefact names.
///
/// The chain's hops are fixed by the artefact, so there is nothing to fail
/// over to: one chain, one session. The supervisor's dial loop still wraps it
/// — a dropped chain is redialled with the same backoff the edge session uses.
pub(crate) struct OnionDialer {
    spec: OnionChainSpec,
}

impl OnionDialer {
    pub(crate) fn new(spec: OnionChainSpec) -> Self {
        Self { spec }
    }
}

#[async_trait::async_trait]
impl EdgeDialer for OnionDialer {
    async fn dial(&self, _start_idx: usize) -> Result<(usize, EdgeSession), EdgeDialError> {
        let tunnel = mirage_onion_client::dial(&self.spec).await.map_err(onion_dial_error)?;
        let session = SessionHandle::client(TunnelCompat::wrap(tunnel));

        // There is no edge hello to exchange: the chain's exit already
        // admitted this client when it verified the chain leg. The welcome is
        // the shape the rest of the crate expects — an accepted session with
        // no country list (the chain's exit is where it is) and no datagram
        // support (the onion plane carries streams only).
        let welcome = EdgeWelcome {
            version: 3,
            countries: Vec::new(),
            outcome: EdgeOutcome::Accepted { exit: ExitPreference::Auto },
            assigned_face_port: None,
            carries_datagrams: None,
        };
        let label = format!("chain {}", self.spec.chain_id);
        info!(
            chain_id = %self.spec.chain_id,
            hops = self.spec.hops.len(),
            "onion session: chain walked, mux session up"
        );
        Ok((
            0,
            EdgeSession::from_mux_session(
                session,
                welcome,
                label,
                StreamFraming::LengthPrefixedTarget,
            ),
        ))
    }
}

/// Collapse an onion dial failure into the crate's own error shape.
///
/// A malformed chain is an artefact problem; a chain that could not be walked
/// is a transport problem, and it keeps the ordinary reason mapping.
fn onion_dial_error(error: OnionDialError) -> EdgeDialError {
    match error {
        OnionDialError::Spec(_) | OnionDialError::Credential(_) => {
            EdgeDialError::BadOnionChain(error.to_string())
        }
        OnionDialError::Mirage(e) => EdgeDialError::Mirage(e),
    }
}
