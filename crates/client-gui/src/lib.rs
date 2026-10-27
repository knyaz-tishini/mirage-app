//! Library surface shared by the desktop window, the Apple bundle, and the
//! OTA publisher. The desktop binary is a thin call to [`run`].

pub mod ota;

// One picture: the oasis scene. Desktop and iOS both enter through `run`.
mod backend;
mod camera;
mod text;
#[cfg(target_os = "ios")]
mod ios_host;
mod oasis;
mod shell;

/// Dev-tools only. `MIRAGE_SHOT_SIZE=1206x2622` sets the window the screenshot
/// API captures. The customer build does not compile this and stays 380×540.
#[cfg(feature = "dev-tools")]
fn dev_shot_size() -> (u32, u32) {
    let Ok(raw) = std::env::var("MIRAGE_SHOT_SIZE") else {
        return (380, 540);
    };
    let Some((w, h)) = raw.split_once('x') else {
        return (380, 540);
    };
    match (w.parse::<u32>(), h.parse::<u32>()) {
        (Ok(w), Ok(h)) if w >= 280 && h >= 400 => (w, h),
        _ => (380, 540),
    }
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    ota::wait_for_parent_exit();
    ota::clear_rollback_copy();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mirage_client_gui=info".parse().unwrap_or_default()),
        )
        .init();

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

    ota::start();
    oasis::launch()?;
    Ok(())
}

#[no_mangle]
pub extern "C" fn mirage_gui_run() -> i32 {
    match run() {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("mirage gui: {err}");
            1
        }
    }
}
