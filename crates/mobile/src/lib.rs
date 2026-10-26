//! `mirage-client-mobile` — C/JNI FFI layer for the Mirage VPN engine.
//!
//! This crate compiles as both a `cdylib` (Android `.so`, iOS static library)
//! and a `staticlib` for embedding in mobile apps.  All platform-specific
//! glue is gated behind `cfg` attributes so that the crate builds cleanly on
//! Windows/macOS/Linux during development.
//!
//! # Architecture
//!
//! ```text
//!  Kotlin / Swift
//!       │
//!  ┌────┴────┐
//!  │mirage-client-mobile │   ← this crate
//!  │ android  │   JNI bridge (cfg android)
//!  │ ios      │   C FFI stubs (cfg ios)
//!  │ common   │   shared engine state + tokio runtime
//!  └────┬────┘
//!  mirage-client-core / mirage-core / mirage-protocol
//! ```

#[cfg(any(target_os = "android", target_os = "ios"))]
mod common;

#[cfg(target_os = "android")]
mod android;

#[cfg(target_os = "ios")]
mod ios;

#[cfg(any(target_os = "ios", target_os = "macos"))]
mod store;

mod tun;
