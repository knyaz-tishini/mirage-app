//! The relay that carries one flow between the local side and the edge.
//!
//! `tokio::io::copy_bidirectional` keeps a flow open as long as both ends
//! stay readable. A peer that never sends its FIN leaves the relay blocked
//! on its read forever, and the flow keeps its slot in the edge session's
//! stream budget for good. When enough flows are stuck that way, the
//! session has no credit left and every new flow has to wait -- which is
//! what a customer sees as "the VPN stopped working".
//!
//! The edge box reaps its own side of a relay after 300 idle seconds
//! (`mirage-node`'s `copy_bidirectional_with_idle_timeout`). The client
//! needs the same net: a flow that carries nothing in either direction for
//! that long is dead, whether the peer is silent because it died or because
//! it is holding the connection open on purpose.
//!
//! The timer restarts on every byte in either direction, so a slow but
//! live flow is never touched.

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// How long a flow may carry nothing in either direction before it is
/// considered dead.
///
/// Matches the edge box's own relay timeout, so both ends of a flow give up
/// at the same point.
pub const FLOW_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Copy bytes between `a` and `b` until one side is done, and give up on a
/// flow that carries nothing in either direction for `idle_timeout`.
///
/// Returns the number of bytes moved in each direction.
pub async fn copy_bidirectional_with_idle_timeout<A, B>(
    a: &mut A,
    b: &mut B,
    idle_timeout: Duration,
) -> std::io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf_a = vec![0u8; 16 * 1024];
    let mut buf_b = vec![0u8; 16 * 1024];
    let mut a_to_b: u64 = 0;
    let mut b_to_a: u64 = 0;
    let mut a_eof = false;
    let mut b_eof = false;

    loop {
        if a_eof && b_eof {
            break;
        }

        let timer = tokio::time::sleep(idle_timeout);
        tokio::pin!(timer);

        tokio::select! {
            biased;

            _ = &mut timer => {
                tracing::debug!(
                    a_to_b,
                    b_to_a,
                    idle_secs = idle_timeout.as_secs(),
                    "flow idle timeout -- closing the relay"
                );
                return Ok((a_to_b, b_to_a));
            }

            res = a.read(&mut buf_a), if !a_eof => {
                match res? {
                    0 => {
                        a_eof = true;
                        let _ = b.shutdown().await;
                    }
                    n => {
                        b.write_all(&buf_a[..n]).await?;
                        a_to_b += n as u64;
                    }
                }
            }

            res = b.read(&mut buf_b), if !b_eof => {
                match res? {
                    0 => {
                        b_eof = true;
                        let _ = a.shutdown().await;
                    }
                    n => {
                        a.write_all(&buf_b[..n]).await?;
                        b_to_a += n as u64;
                    }
                }
            }
        }
    }

    Ok((a_to_b, b_to_a))
}

/// The same, with the standard flow idle timeout.
pub async fn copy_bidirectional_with_flow_idle_timeout<A, B>(
    a: &mut A,
    b: &mut B,
) -> std::io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    copy_bidirectional_with_idle_timeout(a, b, FLOW_IDLE_TIMEOUT).await
}
