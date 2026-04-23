//! iOS TUN device implementation.
//!
//! On iOS, `NEPacketTunnelProvider` exposes a `NEPacketFlow` object rather
//! than a raw file descriptor.  There is no `/dev/tun` and no fd to `dup()`.
//!
//! # Architecture
//!
//! ```text
//! Swift (NEPacketFlow)          Rust (IosTun)
//!   readPackets callback  ──→   IosTunHandle::push (→ inbound channel)
//!   writePackets call     ←──   IosTunHandle::pull (← outbound channel)
//! ```
//!
//! `IosTun::new` returns a `(IosTun, IosTunHandle)` pair:
//! - `IosTun` is given to the VPN stack and implements [`PlatformTun`].
//! - `IosTunHandle` is stored in global state (see `mirage-ffi`) and accessed
//!   by the Swift layer through C FFI.
//!
//! [`PlatformTun::create`] is a no-op because the iOS interface is already
//! active when `NEPacketTunnelProvider.startTunnel` fires.

use mirage_core::error::MirageError;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use mirage_client_core::tun::traits::PlatformTun;

/// Channel capacity: how many packets may be buffered between Swift and Rust.
const CHANNEL_CAPACITY: usize = 256;

// ── IosTunHandle ────────────────────────────────────────────────────────────

/// Handle held by the Swift side (via C FFI) for exchanging raw IP packets.
///
/// - Call [`IosTunHandle::push`] with each packet received from `NEPacketFlow`
///   (inbound: network → VPN stack).
/// - Call [`IosTunHandle::pull`] to retrieve packets that the VPN stack wants
///   to send back out through `NEPacketFlow` (outbound: VPN stack → network).
pub struct IosTunHandle {
    /// Swift pushes inbound packets here (network interface → Rust VPN stack).
    pub inbound_tx: mpsc::Sender<Vec<u8>>,
    /// Swift reads outbound packets from here (Rust VPN stack → network interface).
    pub outbound_rx: mpsc::Receiver<Vec<u8>>,
}

impl IosTunHandle {
    /// Push one raw IP packet into the Rust VPN stack.
    ///
    /// Returns `false` if the VPN stack has shut down (channel closed).
    pub fn push(&mut self, packet: Vec<u8>) -> bool {
        self.inbound_tx.try_send(packet).is_ok()
    }

    /// Pull one raw IP packet from the Rust VPN stack (non-blocking).
    ///
    /// Returns `None` if no packet is ready.
    pub fn pull(&mut self) -> Option<Vec<u8>> {
        self.outbound_rx.try_recv().ok()
    }
}

// ── IosTun ──────────────────────────────────────────────────────────────────

/// iOS TUN device backed by a pair of async channels.
///
/// Implements [`PlatformTun`] so it can be used in the same VPN pipeline as
/// other platforms.  All I/O is non-blocking at the OS level; blocking happens
/// at the Tokio task level via `await`.
pub struct IosTun {
    /// Virtual interface name reported to the rest of the stack.
    name: String,
    /// MTU agreed with the Swift layer.
    mtu: u16,
    /// Inbound packets arriving from the iOS network interface.
    inbound_rx: mpsc::Receiver<Vec<u8>>,
    /// Outbound packets to be forwarded to the iOS network interface.
    outbound_tx: mpsc::Sender<Vec<u8>>,
}

impl IosTun {
    /// Create a new `IosTun` and the paired [`IosTunHandle`] for the Swift side.
    ///
    /// # Arguments
    ///
    /// * `name` — logical interface name (e.g. `"utun0"`).  On iOS the actual
    ///   kernel interface is controlled by `NEPacketTunnelProvider`; this name
    ///   is informational only.
    /// * `mtu`  — maximum transmission unit negotiated with the VPN server.
    pub fn new(name: String, mtu: u16) -> (Self, IosTunHandle) {
        let (inbound_tx, inbound_rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);
        let (outbound_tx, outbound_rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);

        let tun = Self {
            name,
            mtu,
            inbound_rx,
            outbound_tx,
        };

        let handle = IosTunHandle {
            inbound_tx,
            outbound_rx,
        };

        (tun, handle)
    }

    /// Returns the MTU this device was configured with.
    pub fn mtu(&self) -> u16 {
        self.mtu
    }
}

#[async_trait::async_trait]
impl PlatformTun for IosTun {
    /// No-op: `NEPacketTunnelProvider` brings up the interface before Rust
    /// receives control.
    async fn create(&mut self) -> Result<(), MirageError> {
        info!(
            name = %self.name,
            mtu  = self.mtu,
            "IosTun: interface already established by NEPacketTunnelProvider",
        );
        Ok(())
    }

    /// Read one raw IP packet from the iOS network interface.
    ///
    /// Waits until the Swift side pushes a packet via [`IosTunHandle::push`].
    /// Returns [`MirageError::TunnelClosed`] if the Swift handle has been
    /// dropped.
    async fn read_packet(&mut self, buf: &mut [u8]) -> Result<usize, MirageError> {
        let packet = self
            .inbound_rx
            .recv()
            .await
            .ok_or(MirageError::TunnelClosed)?;

        let n = packet.len();
        if n > buf.len() {
            return Err(MirageError::Config(format!(
                "IosTun: packet ({n} bytes) exceeds buffer ({} bytes)",
                buf.len()
            )));
        }

        buf[..n].copy_from_slice(&packet);
        debug!(bytes = n, "IosTun: read_packet");
        Ok(n)
    }

    /// Write one raw IP packet to the iOS network interface.
    ///
    /// The packet is queued for the Swift side to pick up via
    /// [`IosTunHandle::pull`].  Returns [`MirageError::TunnelClosed`] if
    /// the Swift handle has been dropped.
    async fn write_packet(&mut self, packet: &[u8]) -> Result<(), MirageError> {
        self.outbound_tx
            .send(packet.to_vec())
            .await
            .map_err(|_| MirageError::TunnelClosed)?;
        debug!(bytes = packet.len(), "IosTun: write_packet");
        Ok(())
    }

    fn name(&self) -> &str {
        &self.name
    }

    /// Closing the `IosTun` is sufficient — dropping the channel senders
    /// signals the Swift handle that no more packets are coming.
    fn destroy(&mut self) {
        info!(name = %self.name, "IosTun: destroyed");
    }
}

impl Drop for IosTun {
    fn drop(&mut self) {
        warn!(
            name = %self.name,
            "IosTun dropped; inbound/outbound channels will close"
        );
    }
}
