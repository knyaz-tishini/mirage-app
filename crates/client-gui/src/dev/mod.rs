//! Developer-only surface — compiled only with the `dev-tools` feature, which
//! is OFF by default. A customer build contains none of this: no standalone
//! control server, no fps HUD, no mock engine. The source-scan gate
//! (`tests/customer_sources.rs`) scans everything under `src/` EXCEPT this
//! directory.
//!
//! What lives here:
//!   * the standalone control server (`stand_api`): a loopback HTTP API that
//!     drives the standalone install with no window at all,
//!   * the fps HUD drawn over the window,
//!   * the mock engine (UI without the L0 stack; `--live` opts into the real one),
//!   * `--connected` (start with the tunnel up, for screenshot capture).

pub mod mock;
pub mod stand_api;

use uzor::render::{RenderContext, TextAlign, TextBaseline};

use crate::backend::{EngineLink, LocalHttp};

/// Per-window hooks the app calls into each frame.
pub struct DevHooks {
    start_connected: bool,
}

impl DevHooks {
    /// Whether the window opens in the Connected phase (`--connected`).
    pub fn start_connected(&self) -> bool {
        self.start_connected
    }
}

/// Draw the fps HUD (top-right, just under the chrome strip).
pub fn draw_hud(render: &mut dyn RenderContext, w: f64, chrome_h: f64, text: &str) {
    render.set_fill_color("#6B6F7A");
    render.set_font("11px monospace");
    render.set_text_align(TextAlign::Right);
    render.set_text_baseline(TextBaseline::Top);
    render.fill_text(text, w - 8.0, chrome_h + 5.0);
}

/// Dev startup: choose the engine (`--live` → the real L0 stack, else the
/// mock) and honour `--connected`.
pub fn launch(args: &[String]) -> (Box<dyn EngineLink>, DevHooks) {
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let connected = has("--connected");
    let engine: Box<dyn EngineLink> = if has("--live") {
        Box::new(LocalHttp::start())
    } else {
        Box::new(mock::MockEngine::new(connected))
    };
    (
        engine,
        DevHooks {
            start_connected: connected,
        },
    )
}
