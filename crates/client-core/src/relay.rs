use mirage_core::error::MirageError;
use mirage_protocol::traits::Tunnel;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::warn;

pub async fn relay_loop<C, R>(
    client: &mut C,
    remote: &mut R,
) -> Result<(), MirageError>
where
    C: AsyncRead + AsyncWrite + Unpin,
    R: AsyncRead + AsyncWrite + Unpin,
{
    tokio::io::copy_bidirectional(client, remote)
        .await
        .map_err(MirageError::Io)?;
    Ok(())
}

pub struct TunnelCompat {
    stream: tokio::io::DuplexStream,
    _relay_task: tokio::task::JoinHandle<()>,
}

impl TunnelCompat {
    pub fn wrap(tunnel: Box<dyn Tunnel>) -> Self {
        let (app_side, tunnel_side) = tokio::io::duplex(1048576);
        let handle = tokio::spawn(async move {
            let mut tunnel = tunnel;
            let (mut rd, mut wr) = tokio::io::split(tunnel_side);
            let mut rbuf = vec![0u8; 16384];
            let mut wbuf = vec![0u8; 16384];
            loop {
                tokio::select! {
                    result = tunnel.recv(&mut rbuf) => {
                        match result {
                            Ok(0) => { warn!("tunnel_compat: recv EOF"); break; }
                            Err(e) => { warn!("tunnel_compat: recv error: {e}"); break; }
                            Ok(n) => {
                                if let Err(e) = wr.write_all(&rbuf[..n]).await {
                                    warn!("tunnel_compat: wr error: {e}");
                                    break;
                                }
                            }
                        }
                    }
                    result = rd.read(&mut wbuf) => {
                        match result {
                            Ok(0) => { let _ = tunnel.close().await; break; }
                            Err(e) => { warn!("tunnel_compat: rd error: {e}"); let _ = tunnel.close().await; break; }
                            Ok(n) => {
                                if let Err(e) = tunnel.send(&wbuf[..n]).await {
                                    warn!("tunnel_compat: send error: {e}");
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        });
        Self {
            stream: app_side,
            _relay_task: handle,
        }
    }
}

// ── tokio::io traits (used by relay_loop and copy_bidirectional) ──────────────

impl AsyncRead for TunnelCompat {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for TunnelCompat {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

// ── futures::io traits (used by MuxSession transport) ────────────────────────

impl futures::io::AsyncRead for TunnelCompat {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let mut read_buf = tokio::io::ReadBuf::new(buf);
        match std::pin::Pin::new(&mut self.stream).poll_read(cx, &mut read_buf) {
            std::task::Poll::Ready(Ok(())) => {
                std::task::Poll::Ready(Ok(read_buf.filled().len()))
            }
            std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Err(e)),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

impl futures::io::AsyncWrite for TunnelCompat {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
