//! Exit-node catalog for the planet scene.
//!
//! The customer build ships no baked-in inventory. The Connect menu does not
//! read this: it lists countries from the cabinet catalog. The planet draws
//! only what is in [`CATALOG`].

/// One exit the customer can route through.
#[derive(Clone, Copy)]
pub struct ExitNode {
    /// ISO 3166-1 alpha-2 country code — the only label a customer ever sees.
    pub code: &'static str,
    pub lat: f64,
    pub lon: f64,
}

/// Exits known to the client. Empty: nothing is reported by the engine yet.
pub static CATALOG: &[ExitNode] = &[];
