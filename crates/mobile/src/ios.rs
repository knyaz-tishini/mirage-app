//! iOS C FFI — called by Swift via a bridging header.
//!
//! # Packet flow
//!
//! ```text
//! NEPacketFlow.readPackets callback
//!     └─► mirage_push_packet(data, len)   ──→  IosTunHandle::push
//!                                                   │
//!                                               Rust VPN stack
//!                                                   │
//! NEPacketFlow.writePackets                 ←──  IosTunHandle::pull
//!     └─► mirage_pull_packet(buf, buf_len) ──→  returns bytes copied
//! ```
//!
//! # Thread safety
//!
//! All state is protected by a `std::sync::Mutex` that is **never** held
//! across an `.await` point (it wraps synchronous access only).  The tokio
//! runtime lives in a `OnceLock` and is never dropped.
//!
//! # Safety contract
//!
//! All `*const`/`*mut` pointer parameters must be valid and non-null for the
//! stated number of bytes.  NUL-terminated strings must be valid UTF-8.
//! Ownership of heap-allocated strings returned by [`mirage_get_status`] is
//! transferred to the caller and **must** be freed with [`mirage_free_string`].

use std::{
    ffi::{CStr, CString, c_char, c_int},
    sync::{Mutex, OnceLock},
};

use mirage_core::config::ClientConfig;
use mirage_client_core::tun::config::TunConfig;
use crate::tun::ios::{IosFirewall, IosRouting, IosTun, IosTunHandle};

use crate::common::{build_tun_stack, get_status, init_runtime, start_vpn_with_stack, stop_vpn};

// ── Return codes ────────────────────────────────────────────────────────────

const MIRAGE_OK: c_int = 0;
const MIRAGE_ERR_NULL_PTR: c_int = -1;
const MIRAGE_ERR_INVALID_UTF8: c_int = -2;
const MIRAGE_ERR_ENGINE: c_int = -3;
const MIRAGE_ERR_NO_PACKET: c_int = 0; // pull returns 0 when nothing is ready
const MIRAGE_ERR_BUF_TOO_SMALL: c_int = -4;
const MIRAGE_ERR_NOT_RUNNING: c_int = -5;

// ── Global TUN handle (Swift side) ──────────────────────────────────────────

/// The [`IosTunHandle`] is created in [`mirage_start`] and stored here so that
/// [`mirage_push_packet`] / [`mirage_pull_packet`] can reach it from any
/// thread without passing a raw pointer to Swift.
static IOS_TUN_HANDLE: OnceLock<Mutex<Option<IosTunHandle>>> = OnceLock::new();

fn tun_handle_lock() -> &'static Mutex<Option<IosTunHandle>> {
    IOS_TUN_HANDLE.get_or_init(|| Mutex::new(None))
}

// ── Stored config JSON (for mirage_start after mirage_init) ─────────────────

static CONFIG_JSON: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn config_lock() -> &'static Mutex<Option<String>> {
    CONFIG_JSON.get_or_init(|| Mutex::new(None))
}

// ── FFI functions ───────────────────────────────────────────────────────────

/// Initialise the VPN engine with a JSON configuration string.
///
/// Must be called before [`mirage_start`].  May be called again to update the
/// configuration while the engine is stopped.
///
/// Returns `0` on success, a negative error code on failure.
///
/// # Safety
///
/// `config_json` must be a valid, non-null, NUL-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn mirage_init(config_json: *const c_char) -> c_int {
    if config_json.is_null() {
        return MIRAGE_ERR_NULL_PTR;
    }

    let json = match unsafe { CStr::from_ptr(config_json) }.to_str() {
        Ok(s) => s.to_owned(),
        Err(_) => return MIRAGE_ERR_INVALID_UTF8,
    };

    // Validate that the JSON parses as a ClientConfig.
    if let Err(e) = serde_json::from_str::<ClientConfig>(&json) {
        tracing::error!("mirage_init: invalid config JSON: {e}");
        return MIRAGE_ERR_ENGINE;
    }

    match config_lock().lock() {
        Ok(mut guard) => *guard = Some(json),
        Err(_) => return MIRAGE_ERR_ENGINE,
    }

    tracing::info!("mirage_init: config stored");
    MIRAGE_OK
}

/// Start the VPN engine.
///
/// Creates the channel-based [`IosTun`], builds the [`TunStack`], and spawns
/// the engine task.  The [`IosTunHandle`] is stored globally so that
/// [`mirage_push_packet`] / [`mirage_pull_packet`] can access it.
///
/// Returns `0` on success, a negative error code on failure.
#[no_mangle]
pub extern "C" fn mirage_start() -> c_int {
    // Retrieve stored config.
    let config_json = match config_lock().lock() {
        Ok(guard) => match guard.as_ref() {
            Some(j) => j.clone(),
            None => {
                tracing::error!("mirage_start: mirage_init not called");
                return MIRAGE_ERR_ENGINE;
            }
        },
        Err(_) => return MIRAGE_ERR_ENGINE,
    };

    // Parse the TunConfig (embedded in ClientConfig).
    let client_config: ClientConfig = match serde_json::from_str(&config_json) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("mirage_start: config parse error: {e}");
            return MIRAGE_ERR_ENGINE;
        }
    };
    let tun_config: TunConfig = client_config.tun.clone();

    // Create the channel TUN and store the handle for Swift.
    let (ios_tun, ios_handle) = IosTun::new("utun0".to_owned(), tun_config.mtu as u16);

    match tun_handle_lock().lock() {
        Ok(mut guard) => *guard = Some(ios_handle),
        Err(_) => return MIRAGE_ERR_ENGINE,
    }

    // Build the TUN stack.
    let tun_stack = build_tun_stack(
        Box::new(ios_tun),
        Box::new(IosFirewall::new()),
        Box::new(IosRouting::new()),
        tun_config,
    );

    // Hand off to the common engine runner.
    match start_vpn_with_stack(&config_json, tun_stack) {
        Ok(()) => {
            tracing::info!("mirage_start: VPN engine started");
            MIRAGE_OK
        }
        Err(e) => {
            tracing::error!("mirage_start: {e}");
            MIRAGE_ERR_ENGINE
        }
    }
}

/// Stop the VPN engine.
///
/// Signals the background task to shut down and clears the [`IosTunHandle`].
/// Safe to call when the engine is already stopped.
///
/// Returns `0` on success.
#[no_mangle]
pub extern "C" fn mirage_stop() -> c_int {
    // Clear the TUN handle so Swift-side callbacks can detect shutdown.
    if let Ok(mut guard) = tun_handle_lock().lock() {
        *guard = None;
    }

    match stop_vpn() {
        Ok(()) => {
            tracing::info!("mirage_stop: VPN engine stopped");
            MIRAGE_OK
        }
        Err(e) => {
            tracing::error!("mirage_stop: {e}");
            MIRAGE_ERR_ENGINE
        }
    }
}

/// Push one raw IP packet from the iOS network interface into the Rust VPN
/// stack.
///
/// Called from the `NEPacketFlow.readPackets` completion handler in Swift.
///
/// Returns `0` on success, a negative error code on failure.
///
/// # Safety
///
/// `data` must be a valid pointer to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn mirage_push_packet(data: *const u8, len: usize) -> c_int {
    if data.is_null() {
        return MIRAGE_ERR_NULL_PTR;
    }

    let packet = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();

    match tun_handle_lock().lock() {
        Ok(mut guard) => match guard.as_mut() {
            Some(handle) => {
                if handle.push(packet) {
                    MIRAGE_OK
                } else {
                    MIRAGE_ERR_NOT_RUNNING
                }
            }
            None => MIRAGE_ERR_NOT_RUNNING,
        },
        Err(_) => MIRAGE_ERR_ENGINE,
    }
}

/// Pull one raw IP packet from the Rust VPN stack to send to the iOS network
/// interface.
///
/// Called by Swift to retrieve packets for `NEPacketFlow.writePackets`.
///
/// Returns the number of bytes written into `buf`, `0` if no packet is
/// available (non-blocking), or a negative error code.
///
/// # Safety
///
/// `buf` must be a valid pointer to at least `buf_len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn mirage_pull_packet(buf: *mut u8, buf_len: usize) -> c_int {
    if buf.is_null() {
        return MIRAGE_ERR_NULL_PTR;
    }

    match tun_handle_lock().lock() {
        Ok(mut guard) => match guard.as_mut() {
            Some(handle) => match handle.pull() {
                Some(packet) => {
                    if packet.len() > buf_len {
                        return MIRAGE_ERR_BUF_TOO_SMALL;
                    }
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            packet.as_ptr(),
                            buf,
                            packet.len(),
                        );
                    }
                    packet.len() as c_int
                }
                None => MIRAGE_ERR_NO_PACKET,
            },
            None => MIRAGE_ERR_NOT_RUNNING,
        },
        Err(_) => MIRAGE_ERR_ENGINE,
    }
}

/// Return the VPN status as a heap-allocated, NUL-terminated JSON C string.
///
/// The caller takes ownership of the returned pointer and **must** free it
/// with [`mirage_free_string`].  Never pass the returned pointer to `free()`
/// directly.
///
/// Returns a null pointer only if memory allocation fails (extremely unlikely).
#[no_mangle]
pub extern "C" fn mirage_get_status() -> *mut c_char {
    let json = get_status().to_json();
    match CString::new(json) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Free a string previously returned by [`mirage_get_status`].
///
/// Passing any other pointer (including null) is undefined behaviour.
///
/// # Safety
///
/// `s` must be a pointer previously returned by [`mirage_get_status`] and not
/// yet freed.
#[no_mangle]
pub unsafe extern "C" fn mirage_free_string(s: *mut c_char) {
    if !s.is_null() {
        // SAFETY: s was allocated by CString::into_raw in mirage_get_status.
        drop(unsafe { CString::from_raw(s) });
    }
}
