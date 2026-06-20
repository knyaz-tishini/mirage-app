//! Centerpiece: pluggable skin abstraction for the Mirage client GUI.
//!
//! The shell calls `Skin::render` every frame and routes single-tap events
//! via `App::on_event`. No skin-specific code lives outside this module tree
//! or the shell dispatcher.

pub mod aurora;
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
    /// Current pointer position in window-logical px, if known. Skins that
    /// react to the cursor (radar) read it; others ignore it.
    pub pointer: Option<(f64, f64)>,
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

    /// Pointer drag delta (logical px) while the centerpiece is held.
    /// Skins that rotate (planet) consume this; others ignore it.
    fn drag(&mut self, _dx: f64, _dy: f64) {}

    /// Scroll-wheel delta (logical units). Planet zooms; others ignore.
    fn zoom(&mut self, _dy: f64) {}

    /// A tap (press+release without dragging) at logical window coords.
    /// Returns `true` if the skin consumed it (e.g. selected a node and opened
    /// its info popup), `false` if the shell should treat it as a connect tap.
    fn tap(&mut self, _x: f64, _y: f64) -> bool {
        false
    }

    /// Drop any transient selection (e.g. node popup) — called on disconnect.
    fn clear_selection(&mut self) {}
}

// ── Skin kind + registry ──────────────────────────────────────────────────────

/// The three available visual skins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkinKind {
    Bloom,
    Planet,
    Radar,
    Aurora,
}

impl SkinKind {
    /// Cycle to the next skin (Bloom → Planet → Radar → Aurora → Bloom).
    pub fn next(self) -> Self {
        match self {
            Self::Bloom  => Self::Planet,
            Self::Planet => Self::Radar,
            Self::Radar  => Self::Aurora,
            Self::Aurora => Self::Bloom,
        }
    }
}

/// Instantiate the requested skin.
pub fn make_skin(kind: SkinKind) -> Box<dyn Skin> {
    match kind {
        SkinKind::Bloom  => Box::new(bloom::BloomSkin::new()),
        SkinKind::Planet => Box::new(planet::PlanetSkin::new()),
        SkinKind::Radar  => Box::new(radar::RadarSkin::new()),
        SkinKind::Aurora => Box::new(aurora::AuroraSkin::new()),
    }
}
