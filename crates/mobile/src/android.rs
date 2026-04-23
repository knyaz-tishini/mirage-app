//! Android JNI bridge.
//!
//! Provides the `Java_com_mirage_vpn_MirageEngine_*` entry points called by
//! the Kotlin `MirageEngine` object via JNI.
//!
//! The Kotlin side is responsible for:
//! 1. Calling `VpnService.Builder.establish()` to obtain a `ParcelFileDescriptor`.
//! 2. Calling `startVpn(configJson, tunFd)` where `tunFd` is the raw int fd
//!    extracted from the `ParcelFileDescriptor`.
//! 3. Calling `stopVpn()` when the VPN should be torn down.
//! 4. Calling `getStatus()` to poll the engine state.
//!
//! # Thread safety
//!
//! The JNI functions may be called from any thread.  All mutable state is
//! protected by the `Mutex` in [`crate::common`] and the global tokio
//! `Runtime`.

use jni::{
    objects::{JClass, JString},
    sys::{jboolean, jint, jstring, JNI_FALSE, JNI_TRUE},
    JNIEnv,
};
use tracing::error;

use crate::common::{
    build_tun_stack, get_status, init_runtime, start_vpn_with_stack, stop_vpn,
};
use crate::tun::android::{AndroidFirewall, AndroidRouting, AndroidTun};
use mirage_client_core::tun::config::TunConfig;

/// JNI constant: default MTU for the Android TUN interface.
const DEFAULT_MTU: u16 = 1500;

/// Start the VPN engine.
///
/// Called from Kotlin:
/// ```kotlin
/// MirageEngine.startVpn(configJson: String, tunFd: Int): Boolean
/// ```
///
/// # Parameters
/// - `config_json` — serialised [`mirage_core::config::ClientConfig`] JSON.
/// - `tun_fd`      — file descriptor returned by `VpnService.Builder.establish()`.
///
/// # Returns
/// `JNI_TRUE` on success, `JNI_FALSE` on error (error is logged via Android log).
#[no_mangle]
pub extern "system" fn Java_com_mirage_vpn_MirageEngine_startVpn(
    mut env: JNIEnv,
    _class: JClass,
    config_json: JString,
    tun_fd: jint,
) -> jboolean {
    // Initialise the tokio runtime and Android logger on first call.
    init_runtime();
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Debug)
            .with_tag("mirage-ffi"),
    );

    let config_str: String = match env.get_string(&config_json) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("startVpn: failed to read config_json from JNI: {e}");
            return JNI_FALSE;
        }
    };

    // Build platform handles.
    let tun = AndroidTun::from_fd("vpn0".to_owned(), tun_fd, DEFAULT_MTU);
    let firewall = AndroidFirewall::new();
    let routing = AndroidRouting::new();

    // TunConfig with usespace NAT (Android cannot use kernel iptables REDIRECT).
    let tun_config = TunConfig {
        enabled: true,
        intercept: mirage_core::config::InterceptStrategy::UserspaceNat,
        mtu: DEFAULT_MTU,
        ..TunConfig::default()
    };

    let stack = build_tun_stack(
        Box::new(tun),
        Box::new(firewall),
        Box::new(routing),
        tun_config,
    );

    match start_vpn_with_stack(&config_str, stack) {
        Ok(()) => JNI_TRUE,
        Err(e) => {
            error!("startVpn failed: {e}");
            JNI_FALSE
        }
    }
}

/// Stop the VPN engine.
///
/// Called from Kotlin:
/// ```kotlin
/// MirageEngine.stopVpn(): Boolean
/// ```
///
/// # Returns
/// `JNI_TRUE` on success, `JNI_FALSE` if the engine mutex is poisoned.
#[no_mangle]
pub extern "system" fn Java_com_mirage_vpn_MirageEngine_stopVpn(
    _env: JNIEnv,
    _class: JClass,
) -> jboolean {
    match stop_vpn() {
        Ok(()) => JNI_TRUE,
        Err(e) => {
            error!("stopVpn failed: {e}");
            JNI_FALSE
        }
    }
}

/// Query the current VPN status.
///
/// Called from Kotlin:
/// ```kotlin
/// MirageEngine.getStatus(): String
/// ```
///
/// # Returns
/// A JSON string: `{"status":"connected"}` or `{"status":"disconnected"}`.
#[no_mangle]
pub extern "system" fn Java_com_mirage_vpn_MirageEngine_getStatus(
    mut env: JNIEnv,
    _class: JClass,
) -> jstring {
    let json = get_status().to_json();
    match env.new_string(json) {
        Ok(s) => s.into_raw(),
        Err(e) => {
            error!("getStatus: failed to create JNI string: {e}");
            // Return a null jstring — Kotlin side must handle null.
            std::ptr::null_mut()
        }
    }
}
