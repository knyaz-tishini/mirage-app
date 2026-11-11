//! Library surface shared by the desktop window, the Apple bundle, and the
//! OTA publisher. The desktop binary is a thin call to [`run`].

pub mod ota;

// One picture: the oasis scene. Desktop and iOS both enter through `run`.
mod backend;
mod camera;
mod configs;
mod engine;
mod render_backend;
mod text;
mod theme;
#[cfg(target_os = "ios")]
mod ios_host;
pub(crate) mod oasis;
mod shell;

/// Dev-tools only. The customer build compiles none of this.
#[cfg(feature = "dev-tools")]
pub mod dev;

/// The window's log file, beside the window.
struct LogSink {
    file: Option<std::sync::Arc<std::sync::Mutex<std::fs::File>>>,
}

/// Where the logs of the window live.
///
/// Not beside the binary: the release folder is on a mechanical disk that
/// other builds saturate, and a log written there is a window that never
/// opens -- the write waits in the disk's queue for tens of seconds.  The
/// logs are the fast disk's business; the targets and the presets stay where
/// they are.
pub fn log_dir() -> std::path::PathBuf {
    let dir = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::path::PathBuf::new)
        .join("mirage2app")
        .join("logs");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

impl LogSink {
    fn open() -> Self {
        let path = log_dir().join("mirage-client-gui.log");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok();
        Self { file: file.map(|file| std::sync::Arc::new(std::sync::Mutex::new(file))) }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter { file: self.file.clone() }
    }
}

/// One event's bytes into the log file.
struct LogWriter {
    file: Option<std::sync::Arc<std::sync::Mutex<std::fs::File>>>,
}

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let Some(file) = &self.file else {
            return Ok(buf.len());
        };
        let mut guard = file.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        std::io::Write::write_all(&mut *guard, buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(file) = &self.file {
            let mut guard = file.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            std::io::Write::flush(&mut *guard)?;
        }
        Ok(())
    }
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    ota::wait_for_parent_exit();
    ota::clear_rollback_copy();

    // The window's own log, in a file beside the window. The window has no
    // console of its own: without this file the engine's own words are lost.
    let sink = LogSink::open();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mirage_client_gui=info,mirage_subscriber_client=info,mirage_mux=info,mirage_protocol=info,mirage_core=info".parse().unwrap_or_default()),
        )
        .with_writer(sink)
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

    oasis::launch(crate::render_backend::from_args())?;
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
