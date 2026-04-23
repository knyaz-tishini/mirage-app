//! Android TUN device implementation.
//!
//! On Android the `VpnService.Builder.establish()` call returns a
//! `ParcelFileDescriptor` (a file descriptor) that behaves as a raw IP packet
//! stream.  Unlike macOS utun there is **no** protocol-family prefix — packets
//! are raw Layer-3 IP datagrams starting at the IP version nibble.
//!
//! The fd is created on the Kotlin/JNI side and handed to Rust via
//! [`AndroidTun::from_fd`].  The `create()` method is a no-op because the
//! interface already exists by the time Rust receives control.

use std::os::unix::io::RawFd;

use mirage_core::error::MirageError;
use tracing::{debug, info, warn};

use mirage_client_core::tun::traits::PlatformTun;

/// Android TUN device backed by a `VpnService`-provided file descriptor.
pub struct AndroidTun {
    /// Interface name as reported to the rest of the stack (e.g. `"vpn0"`).
    name: String,
    /// File descriptor obtained from `VpnService.Builder.establish()` via JNI.
    fd: RawFd,
    /// MTU negotiated with the Kotlin side.
    mtu: u16,
}

impl AndroidTun {
    /// Create an [`AndroidTun`] from an fd passed by the Kotlin `VpnService`.
    ///
    /// # Safety
    ///
    /// `fd` must be a valid, open file descriptor for the duration of this
    /// struct's lifetime.  Ownership of the fd transfers to this struct;
    /// [`destroy`] will close it.
    pub fn from_fd(name: String, fd: i32, mtu: u16) -> Self {
        Self { name, fd: fd as RawFd, mtu }
    }

    /// Returns the MTU this device was configured with.
    pub fn mtu(&self) -> u16 {
        self.mtu
    }
}

#[async_trait::async_trait]
impl PlatformTun for AndroidTun {
    /// No-op: the fd is already open when `from_fd` is called.
    async fn create(&mut self) -> Result<(), MirageError> {
        info!(
            name = %self.name,
            fd = self.fd,
            mtu = self.mtu,
            "AndroidTun: fd already established by VpnService",
        );
        Ok(())
    }

    async fn read_packet(&mut self, buf: &mut [u8]) -> Result<usize, MirageError> {
        // SAFETY: fd is valid (upheld by contract of from_fd); buf is a valid
        // writable slice for the given length.
        let n = unsafe {
            libc::read(
                self.fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };
        if n < 0 {
            return Err(MirageError::Io(std::io::Error::last_os_error()));
        }
        debug!(bytes = n, "AndroidTun: read");
        Ok(n as usize)
    }

    async fn write_packet(&mut self, packet: &[u8]) -> Result<(), MirageError> {
        // SAFETY: fd is valid; packet is a valid readable slice.
        let n = unsafe {
            libc::write(
                self.fd,
                packet.as_ptr() as *const libc::c_void,
                packet.len(),
            )
        };
        if n < 0 {
            return Err(MirageError::Io(std::io::Error::last_os_error()));
        }
        debug!(bytes = n, "AndroidTun: write");
        Ok(())
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn destroy(&mut self) {
        if self.fd >= 0 {
            // SAFETY: fd is open and owned by this struct.
            unsafe { libc::close(self.fd) };
            info!(name = %self.name, fd = self.fd, "AndroidTun: fd closed");
            self.fd = -1;
        }
    }
}

impl Drop for AndroidTun {
    fn drop(&mut self) {
        if self.fd >= 0 {
            warn!(
                name = %self.name,
                fd = self.fd,
                "AndroidTun dropped without calling destroy()"
            );
            self.destroy();
        }
    }
}
