//! Exit-node catalog — the single source the planet scene and the menu both
//! read.
//!
//! The customer build ships NO baked-in inventory: the catalog is empty until
//! the engine reports the exits the customer's own subscription can reach.
//! Consumers (`shell::menu`, `centerpiece::planet`) render only what is in
//! [`CATALOG`], sized by its length — never a fixed node count.

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
