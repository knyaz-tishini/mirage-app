//! Centerpiece: pluggable skin abstraction for the Mirage client GUI.
//!
//! The shell calls `Skin::render` every frame and routes single-tap events
//! through `Skin::hit_center`. No skin-specific code lives outside this
//! module tree or the shell dispatcher.

pub mod bloom;
pub mod planet;
pub mod radar;

use uzor::render::RenderContext;

// ── Connection phase ──────────────────────────────────────────────────────────

/// Connection phase the skin must animate toward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnPhase {
    Disconnected,
    Connecting,
    Connected,
    Disconnecting,
}

// ── Paint area ────────────────────────────────────────────────────────────────

/// Available paint area in logical pixels (below titlebar / above bottom sheet).
#[derive(Debug, Clone, Copy)]
pub struct SkinRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl SkinRect {
    pub fn cx(&self) -> f64 {
        self.x + self.w * 0.5
    }
    pub fn cy(&self) -> f64 {
        self.y + self.h * 0.5
    }
    /// Radius of the skin's center hit area (38 % of the smaller dimension).
    pub fn radius(&self) -> f64 {
        self.w.min(self.h) * 0.34
    }
}

// ── Per-frame context ─────────────────────────────────────────────────────────

/// Immutable per-frame context the shell passes to the active skin.
#[derive(Debug, Clone, Copy)]
pub struct CenterpieceState {
    /// Frame delta in seconds (typically ≈ 0.016).
    pub dt: f64,
    /// Current connection phase.
    pub phase: ConnPhase,
    /// Available paint rect (below titlebar / above bottom sheet).
    pub rect: SkinRect,
}

// ── Skin trait ────────────────────────────────────────────────────────────────

/// Every visual skin implements this trait.
///
/// The shell calls these methods; no skin-specific code lives in the shell.
/// Skins own their internal eased state and animate toward `st.phase` each
/// frame.
pub trait Skin: Send + 'static {
    /// Called every frame. Draw to `ctx` using `st`.
    fn render(&mut self, ctx: &mut dyn RenderContext, st: CenterpieceState);

    /// Returns `true` if `(x, y)` (logical pixels) falls within the skin's
    /// center hit area — the connect/disconnect toggle zone.
    ///
    /// Default: circular hit area at `rect.radius() * 0.38`.
    fn hit_center(&self, x: f64, y: f64, rect: SkinRect) -> bool {
        let dx = x - rect.cx();
        let dy = y - rect.cy();
        dx.hypot(dy) < rect.radius() * 0.38
    }
}

// ── Skin kind + registry ──────────────────────────────────────────────────────

/// The three available visual skins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkinKind {
    Bloom,
    Planet,
    Radar,
}

impl SkinKind {
    /// Cycle to the next skin (Bloom → Planet → Radar → Bloom).
    pub fn next(self) -> Self {
        match self {
            Self::Bloom  => Self::Planet,
            Self::Planet => Self::Radar,
            Self::Radar  => Self::Bloom,
        }
    }
}

/// Instantiate the requested skin.
pub fn make_skin(kind: SkinKind) -> Box<dyn Skin> {
    match kind {
        SkinKind::Bloom  => Box::new(bloom::BloomSkin::new()),
        SkinKind::Planet => Box::new(planet::PlanetSkin::new()),
        SkinKind::Radar  => Box::new(radar::RadarSkin::new()),
    }
}
