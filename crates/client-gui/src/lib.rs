//! Library surface shared by the desktop window, the Apple bundle, and the
//! OTA publisher. The desktop binary is a thin call to [`run`].

pub mod ota;

// One picture: the oasis scene. Desktop and iOS both enter through `run`.
mod backend;
mod camera;
mod configs;
mod text;
mod theme;
#[cfg(target_os = "ios")]
mod ios_host;
pub(crate) mod oasis;
mod shell;

/// Dev-tools only. The customer build compiles none of this.
#[cfg(feature = "dev-tools")]
pub mod dev;

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

    // The headless mode opens no window at all: the control API is the whole
    // surface, so an install on a box can be driven from a script or an agent.
    #[cfg(feature = "dev-tools")]
    {
        if let Some(requested) = dev::stand_api::headless_port(std::env::args().skip(1)) {
            let port = dev::stand_api::serve(requested)?;
            tracing::info!("[stand-api] http://127.0.0.1:{port}");
            // The process stays alive: the API thread serves, this one waits.
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
    }

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
