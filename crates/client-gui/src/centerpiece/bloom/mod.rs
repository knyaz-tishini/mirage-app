//! Bloom skin — vector flower that animates on connect/disconnect.

pub mod paint;
pub mod state;

use uzor::render::RenderContext;

use super::{CenterpieceState, Skin, SkinRect};
use state::BloomState;

/// Bloom skin: 6-ring petal flower, radial glow, stem, sparkles.
pub struct BloomSkin {
    state: BloomState,
}

impl BloomSkin {
    pub fn new() -> Self {
        Self {
            state: BloomState::new(),
        }
    }
}

impl Default for BloomSkin {
    fn default() -> Self {
        Self::new()
    }
}

impl Skin for BloomSkin {
    fn render(&mut self, ctx: &mut dyn RenderContext, st: CenterpieceState) {
        // Get local hour for time-of-day tint.
        let hour_frac = {
            // Use a fixed value; full time-of-day hooks can be wired later
            // without changing this interface.
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();
            // seconds within day → hours
            (t % 86400.0) / 3600.0
        };

        self.state.tick(st.dt, st.phase, hour_frac);
        paint::render_bloom(ctx, &self.state, st);
    }

    fn hit_center(&self, x: f64, y: f64, rect: SkinRect) -> bool {
        let dx = x - rect.cx();
        let dy = y - rect.cy();
        dx.hypot(dy) < rect.radius() * 0.38
    }
}
