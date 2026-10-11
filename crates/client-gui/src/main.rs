//! mirage-client-gui — Mirage client GUI.
//!
//! Entry point: drives the app via the uzor AppBuilder runtime. The customer
//! build always owns and drives the real L0 backend; everything developer-only
//! (agent API, profiling server, fps HUD, mock engine) lives in `src/dev/`
//! behind the `dev-tools` feature, which is off by default.

// No console window on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod backend;
mod centerpiece;
mod config_import;
#[cfg(feature = "dev-tools")]
mod dev;
mod nodes;
mod render_backend;
mod shell;

use uzor::framework::builder::AppBuilder;
use uzor::framework::multi_window::{WindowKey, WindowSpec};
use uzor::platform::types::CornerStyle;
use uzor_desktop::AppRun as _;

use app::MirageClientApp;
use mirage_client_gui::ota;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    ota::wait_for_parent_exit();
    ota::clear_rollback_copy();

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

    // --skin bloom|planet picks the initial skin. Default is planet.
    let args: Vec<String> = std::env::args().collect();
    let skin = match args
        .iter()
        .position(|a| a == "--skin")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
    {
        Some("bloom") => centerpiece::SkinKind::Bloom,
        _ => centerpiece::SkinKind::Planet,
    };

    // --backend <name> for the initial render backend (support fallback for a
    // broken GPU driver: `--backend tiny_skia`); absent → autodetect.
    let initial_backend = args
        .iter()
        .position(|a| a == "--backend")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| render_backend::parse_backend(s));

    // The customer build spawns one subscriber-client child and polls 127.0.0.1:19421.
    #[cfg(not(feature = "dev-tools"))]
    let engine: Box<dyn backend::EngineLink> = Box::new(backend::LocalHttp::start());
    #[cfg(feature = "dev-tools")]
    let (engine, dev_hooks) = dev::launch(&args);

    ota::start();
    let app = MirageClientApp::new(skin, engine);
    #[cfg(feature = "dev-tools")]
    let app = app.with_dev(dev_hooks);

    // render-hub builds vello area-only AA; default msaa_samples=8 would
    // request the absent Msaa8 shader and panic on the first frame.
    let mut builder = AppBuilder::new(app).msaa(0);
    #[cfg(feature = "dev-tools")]
    {
        builder = dev::attach(builder);
    }
    if let Some(b) = initial_backend {
        builder = builder.backend(b);
    }
    builder
        .window(
            WindowSpec::new(WindowKey::new("main"), concat!("Mirage ", env!("CARGO_PKG_VERSION")))
                .size(380, 540)
                .min_size(280, 400)
                .decorations(false)
                .background(0xFF_0B_0B_0E)
                .corner_style(CornerStyle::Rounded),
        )
        .run()?;

    Ok(())
}
