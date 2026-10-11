//! Library surface shared by the desktop window and the OTA publisher.
//! The window is still `src/main.rs`. This crate exists so the publisher
//! signs with the same hash and the same compiled-in public key the window verifies.

pub mod ota;
