//! The client-side watchdog over one edge session's stream opens.
//!
//! WHY this exists: the 2026-10-21 edge blackout
//! (`docs/mirage/audits/edge-idle-demand-drop-blackout-2026-10-21.md`,
//! root cause 3) left the subscriber client saying `connected` for the whole
//! life of a session TCP while the edge refused EVERY new stream with
//! `no chain currently held for exit choice ...`. The edge records demand
//! only when it ACCEPTS a session
//! (`mirage2node/crates/mirage-edge/src/entrance.rs:466`), so a session that
//! stays up but cannot open a stream never re-records demand — the blackout
//! heals only when a NEW session is accepted.
//!
//! What the client can actually SEE: the edge does not refuse a stream with
//! an error frame. `mirage2node/crates/mirage-edge/src/entrance.rs:733`
//! closes the mux stream instead, so the refusal reaches the client as a
//! stream that ends without the edge ever delivering a payload byte on it.
//! [`WatchedStream`] turns that shape into a counted failure and
//! [`StreamHealth`] counts the consecutive ones; the supervisor tears the
//! session down and re-dials once the count reaches the threshold, which
//! makes the edge accept a new session — and record demand again.
//!
//! The same shape covers the edge's other way of refusing: a chain dial
//! that times out also ends the mux stream with nothing delivered on it.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use mirage_mux::tokio_compat::TokioMuxStream;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// How many consecutive failed stream opens tear the edge session down and
/// force a redial.
///
/// Three is deliberate: one refusal can be a single unlucky destination
/// (a port the exit refuses to reach), two can be a coincidence, but a
/// third one in a row on a healthy session means the edge is no longer
/// serving this exit — which is exactly the blackout's shape. Overridable
/// through `SubscriberClientOptions::stream_failure_threshold`.
pub const DEFAULT_STREAM_FAILURE_THRESHOLD: u32 = 3;

/// Consecutive failed stream opens on ONE edge session.
///
/// A fresh session starts at zero. Every failed open adds one; the first
/// successful stream resets the count to zero. The count belongs to the
/// session, not to the client: a redial starts a new session with a new
/// counter, because the new session is a new chance for the edge to serve.
#[derive(Debug)]
pub struct StreamHealth {
    consecutive_failures: AtomicU32,
    /// Publishes the count so the supervisor's dial loop can react the
    /// moment the threshold is reached, instead of on its next liveness
    /// poll (which is [`crate::session`]'s two-second tick).
    failures: tokio::sync::watch::Sender<u32>,
}

impl StreamHealth {
    /// A fresh counter, at zero.
    pub fn new() -> Self {
        let (failures, _rx) = tokio::sync::watch::channel(0);
        Self { consecutive_failures: AtomicU32::new(0), failures }
    }

    /// One successful stream: the run of failures ends here.
    pub fn record_success(&self) {
        // Publish only when the count actually changes. A busy session ends
        // streams constantly, and the supervisor must not wake for each one.
        if self.consecutive_failures.swap(0, Ordering::Relaxed) != 0 {
            self.failures.send_replace(0);
        }
    }

    /// One failed stream open. Returns the new consecutive count.
    pub fn record_failure(&self) -> u32 {
        let count = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        self.failures.send_replace(count);
        count
    }

    /// How many stream opens have failed in a row.
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures.load(Ordering::Relaxed)
    }

    /// A receiver for the failure count. The supervisor watches it so the
    /// teardown starts the moment the threshold is reached, not on the next
    /// liveness tick.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u32> {
        self.failures.subscribe()
    }
}

impl Default for StreamHealth {
    fn default() -> Self {
        Self::new()
    }
}

/// One mux stream, wrapped so the session's watchdog learns how the open
/// ended.
///
/// The mux protocol has no "stream refused" frame — the edge closes the
/// stream instead — so the only thing the client can count is the shape:
/// the stream ended and the edge never delivered a payload byte on it.
pub struct WatchedStream {
    inner: TokioMuxStream,
    health: Arc<StreamHealth>,
    /// Payload bytes the edge delivered on this stream.
    from_edge: u64,
    /// `true` once this stream's outcome has been reported to the counter.
    reported: bool,
    /// `true` once the LOCAL side half-closed the stream.
    local_closed: bool,
}

impl WatchedStream {
    /// Wrap a freshly opened mux stream so its outcome reaches `health`.
    pub fn new(inner: TokioMuxStream, health: Arc<StreamHealth>) -> Self {
        Self { inner, health, from_edge: 0, reported: false, local_closed: false }
    }

    /// Report this stream's outcome to the counter — once.
    ///
    /// A stream the edge closed without ever delivering a payload is the
    /// refusal shape: the edge had no chain for the requested exit (or its
    /// chain dial timed out) and closed the stream instead of relaying. A
    /// stream the LOCAL side half-closed first is the local application
    /// giving up — the edge never refused anything, so that is not a
    /// failure.
    fn finish(&mut self) {
        if self.reported {
            return;
        }
        self.reported = true;
        if self.from_edge == 0 && !self.local_closed {
            self.health.record_failure();
        } else {
            self.health.record_success();
        }
    }
}

impl AsyncRead for WatchedStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        match std::pin::Pin::new(&mut self.inner).poll_read(cx, buf) {
            std::task::Poll::Ready(Ok(())) => {
                let read = buf.filled().len() - before;
                if read == 0 {
                    // EOF: the peer closed this stream.
                    self.finish();
                } else {
                    self.from_edge += read as u64;
                }
                std::task::Poll::Ready(Ok(()))
            }
            std::task::Poll::Ready(Err(e)) => {
                self.finish();
                std::task::Poll::Ready(Err(e))
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

impl AsyncWrite for WatchedStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match std::pin::Pin::new(&mut self.inner).poll_write(cx, buf) {
            std::task::Poll::Ready(Ok(n)) => std::task::Poll::Ready(Ok(n)),
            std::task::Poll::Ready(Err(e)) => {
                self.finish();
                std::task::Poll::Ready(Err(e))
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        // The LOCAL side gave up on this flow. Whatever the edge does with
        // the stream afterwards is not a refusal.
        self.local_closed = true;
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consecutive_failures_count_up_and_a_success_resets_them() {
        let health = StreamHealth::new();
        assert_eq!(health.consecutive_failures(), 0);

        assert_eq!(health.record_failure(), 1);
        assert_eq!(health.record_failure(), 2);
        assert_eq!(health.consecutive_failures(), 2);

        health.record_success();
        assert_eq!(health.consecutive_failures(), 0, "a success must reset the run");

        assert_eq!(health.record_failure(), 1, "the count restarts after a success");
    }

    #[test]
    fn the_failure_count_is_published_to_the_supervisor() {
        let health = StreamHealth::new();
        let mut rx = health.subscribe();
        assert_eq!(*rx.borrow_and_update(), 0);

        health.record_failure();
        assert_eq!(*rx.borrow_and_update(), 1);

        health.record_failure();
        assert_eq!(*rx.borrow_and_update(), 2);

        health.record_success();
        assert_eq!(*rx.borrow_and_update(), 0);
    }

    #[tokio::test]
    async fn a_success_that_changes_nothing_does_not_wake_the_supervisor() {
        let health = StreamHealth::new();
        let mut rx = health.subscribe();
        let _ = rx.borrow_and_update();

        // Already zero: no publish, so the supervisor's `changed()` stays put.
        health.record_success();
        let woken = tokio::time::timeout(std::time::Duration::from_millis(50), rx.changed()).await;
        assert!(woken.is_err(), "an idle success must not publish");

        // A real failure does publish.
        health.record_failure();
        let woken = tokio::time::timeout(std::time::Duration::from_secs(1), rx.changed()).await;
        assert!(woken.is_ok(), "a failure must publish the new count");
    }
}
