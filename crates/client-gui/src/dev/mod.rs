//! Developer-only surface — compiled only with the `dev-tools` feature, which
//! is OFF by default. A customer build contains none of this: no agent API
//! server, no profiling control server, no fps HUD, no mock engine. The
//! source-scan gate (`tests/customer_sources.rs`) scans everything under
//! `src/` EXCEPT this directory.
//!
//! What lives here:
//!   * the uzor agent API (`:17471`, screenshots + widget tree over loopback),
//!   * the profiling control server (`:17472`, backend hot-switch + fps),
//!   * the fps HUD drawn over the window,
//!   * the mock engine (UI without the L0 stack; `--live` opts into the real one),
//!   * `--connected` (start with the tunnel up, for screenshot capture).

pub mod mock;
pub mod prof;

use std::sync::{Arc, Mutex};

use uzor::framework::builder::AppBuilder;
use uzor::framework::render_control::RenderControl;
use uzor::render::{RenderContext, TextAlign, TextBaseline};

use crate::app::MirageClientApp;
use crate::backend::{EngineLink, LocalHttp};

/// Port of the uzor agent API (screenshots, widget tree).
const AGENT_API_PORT: u16 = 17471;
/// Port of the profiling control server.
const PROF_PORT: u16 = 17472;

/// Per-window hooks the app calls into each frame.
pub struct DevHooks {
    prof: prof::ProfHandle,
    start_connected: bool,
}

impl DevHooks {
    /// Whether the window opens in the Connected phase (`--connected`).
    pub fn start_connected(&self) -> bool {
        self.start_connected
    }

    /// Per-frame profiling sync; returns the HUD readout.
    pub fn frame(&self, render_control: &mut dyn RenderControl) -> String {
        prof::tick(&self.prof, render_control)
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
/// mock), honour `--connected`, and start the profiling server.
pub fn launch(args: &[String]) -> (Box<dyn EngineLink>, DevHooks) {
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let connected = has("--connected");
    let engine: Box<dyn EngineLink> = if has("--live") {
        Box::new(LocalHttp::start())
    } else {
        Box::new(mock::MockEngine::new(connected))
    };
    let prof = Arc::new(Mutex::new(prof::ProfState::default()));
    prof::serve(prof.clone(), PROF_PORT);
    (
        engine,
        DevHooks {
            prof,
            start_connected: connected,
        },
    )
}

/// Enable the uzor agent API on the builder.
pub fn attach(builder: AppBuilder<MirageClientApp>) -> AppBuilder<MirageClientApp> {
    builder.agent_api(AGENT_API_PORT)
}
