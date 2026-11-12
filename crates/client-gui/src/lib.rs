//! Library surface shared by the desktop window, the Apple bundle, and the
//! OTA publisher. The desktop binary is a thin call to [`run`].

use std::io::Write as _;
use std::sync::Arc;

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
///
/// The write happens on a thread of its own. A log that is written on the
/// thread that draws is a window that stands still: the disk's queue is not
/// the window's clock, and a build that saturates the disk used to freeze the
/// picture for minutes -- the window wrote one line and waited for the disk.
/// The queue is short on purpose: a log that cannot keep up drops lines
/// instead of holding the window.
struct LogSink {
    queue: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    /// The lines the queue refused. The writer counts them in the file.
    dropped: Arc<std::sync::atomic::AtomicU64>,
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
        let (queue, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(2048);
        let dropped = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counter = Arc::clone(&dropped);
        let _ = std::thread::Builder::new()
            .name("mirage-log".into())
            .spawn(move || {
                let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                else {
                    eprintln!("mirage-client-gui: the log file is not there: {}", path.display());
                    return;
                };
                while let Ok(bytes) = rx.recv() {
                    if let Err(error) = std::io::Write::write_all(&mut file, &bytes) {
                        eprintln!("mirage-client-gui: the log is not written: {error}");
                        break;
                    }
                    let lost = counter.swap(0, std::sync::atomic::Ordering::Relaxed);
                    if lost > 0 {
                        let _ = writeln!(
                            file,
                            "mirage-client-gui: {lost} lines were dropped -- the disk was busy"
                        );
                    }
                    let _ = file.flush();
                }
            });
        Self { queue: Some(queue), dropped }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter { queue: self.queue.clone(), dropped: Arc::clone(&self.dropped) }
    }
}

/// One event's bytes into the log file.
struct LogWriter {
    queue: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    dropped: Arc<std::sync::atomic::AtomicU64>,
}

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let Some(queue) = &self.queue else {
            return Ok(buf.len());
        };
        match queue.try_send(buf.to_vec()) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                // The disk is behind. The line goes: a window that waits for
                // its own log is a window that looks dead.
                self.dropped
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // The writer thread flushes the file. Waiting for it here would put
        // the disk back on the thread that draws.
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
