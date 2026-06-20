//! mirage-client-gui — Mirage client GUI.
//!
//! Entry point: drives the app via the uzor AppBuilder runtime, which gives
//! the agent-API + `/screenshot` endpoint for free.

// No console window on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod centerpiece;
mod prof;
mod shell;

use std::sync::{Arc, Mutex};

use uzor::framework::builder::AppBuilder;
use uzor::framework::multi_window::{WindowKey, WindowSpec};
use uzor::platform::types::CornerStyle;
use uzor_desktop::AppRun as _;

use app::MirageClientApp;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mirage_client_gui=info".parse().unwrap_or_default()),
        )
        .init();

    // Panic hook — log + exit(1) so the parent supervisor can respawn.
    std::panic::set_hook(Box::new(|info| {
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .unwrap_or("(non-string panic)");
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "(unknown location)".into());
        tracing::error!("PANIC at {}: {}", loc, msg);
        std::thread::sleep(std::time::Duration::from_millis(100));
        std::process::exit(1);
    }));

    // --skin bloom|planet|radar + --connected (deterministic screenshot capture).
    let args: Vec<String> = std::env::args().collect();
    let skin = match args
        .iter()
        .position(|a| a == "--skin")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
    {
        Some("planet") => centerpiece::SkinKind::Planet,
        Some("radar") => centerpiece::SkinKind::Radar,
        Some("aurora") => centerpiece::SkinKind::Aurora,
        _ => centerpiece::SkinKind::Bloom,
    };
    let connected = args.iter().any(|a| a == "--connected");

    // --backend <name> for the initial backend; absent → autodetect (VelloGpu).
    let backend = args
        .iter()
        .position(|a| a == "--backend")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| prof::parse_backend(s));

    // Runtime profiling control: hot-switch backend + scrape fps over HTTP.
    let prof_state = Arc::new(Mutex::new(prof::ProfState::default()));
    prof::serve(prof_state.clone(), 17472);

    let mut builder = AppBuilder::new(MirageClientApp::with_skin(skin, connected, prof_state))
        .agent_api(17471)
        // render-hub builds vello area-only AA; default msaa_samples=8 would
        // request the absent Msaa8 shader and panic on the first frame.
        .msaa(0);
    if let Some(b) = backend {
        builder = builder.backend(b);
    }
    builder
        .window(
            WindowSpec::new(WindowKey::new("main"), "Mirage")
                .size(380, 540)
                .min_size(280, 400)
                .decorations(false)
                .background(0xFF_0B_0B_0E)
                .corner_style(CornerStyle::Rounded),
        )
        .run()?;

    Ok(())
}
