//! Transparent proxy helpers (Linux-only).
//!
//! When the TUN stack is active with the `KernelRedirect` intercept strategy,
//! iptables REDIRECT steers all outbound TCP into our SOCKS5 port without the
//! application performing a SOCKS5 handshake.  The kernel records the original
//! destination in `SO_ORIGINAL_DST` so we can retrieve it here.
//!
//! On Linux, [`peek_connection_kind`] peeks at the first byte of an accepted
//! connection:
//! * `0x05` → normal SOCKS5 handshake.
//! * anything else → transparently redirected connection; call
//!   [`get_original_dst`] to recover the destination.
//!
//! All items in this module are gated to `#[cfg(target_os = "linux")]`.

// ── Linux ─────────────────────────────────────────────────────────────────────

/// Whether an accepted connection looks like a SOCKS5 client greeting.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionKind {
    /// First byte is `0x05` — standard SOCKS5 connection.
    Socks5,
    /// First byte is NOT `0x05` — connection was redirected by iptables.
    Transparent,
}

/// Peek at the first byte of `stream` without consuming it.
///
/// Returns the [`ConnectionKind`] so the caller can dispatch to the correct
/// handler without re-reading the byte.
#[cfg(target_os = "linux")]
pub async fn peek_connection_kind(
    stream: &tokio::net::TcpStream,
) -> Result<ConnectionKind, mirage_core::error::MirageError> {
    let mut buf = [0u8; 1];
    // peek() does not advance the read position.
    let n = stream.peek(&mut buf).await?;
    if n == 0 {
        return Err(mirage_core::error::MirageError::Protocol(
            "transparent: peer closed before sending any data".to_owned(),
        ));
    }
    if buf[0] == 0x05 {
        Ok(ConnectionKind::Socks5)
    } else {
        Ok(ConnectionKind::Transparent)
    }
}

/// Retrieve the original destination of a connection that was redirected by
/// an `iptables REDIRECT` rule.
///
/// Calls `getsockopt(fd, SOL_IP, SO_ORIGINAL_DST)` on the underlying socket.
#[cfg(target_os = "linux")]
pub fn get_original_dst(
    stream: &tokio::net::TcpStream,
) -> Result<std::net::SocketAddr, mirage_core::error::MirageError> {
    use std::os::unix::io::AsRawFd;

    // SOL_IP = 0, SO_ORIGINAL_DST = 80
    const SOL_IP: libc::c_int = 0;
    const SO_ORIGINAL_DST: libc::c_int = 80;

    let fd = stream.as_raw_fd();

    let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut addr_len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;

    let ret = unsafe {
        libc::getsockopt(
            fd,
            SOL_IP,
            SO_ORIGINAL_DST,
            &mut addr as *mut _ as *mut libc::c_void,
            &mut addr_len,
        )
    };

    if ret != 0 {
        let os_err = std::io::Error::last_os_error();
        return Err(mirage_core::error::MirageError::Io(os_err));
    }

    // addr.sin_addr.s_addr is in network byte order.
    let ip = std::net::Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
    let port = u16::from_be(addr.sin_port);
    Ok(std::net::SocketAddr::from((ip, port)))
}
