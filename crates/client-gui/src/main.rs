//! mirage-client-gui — Mirage client GUI.
//!
//! The window lives in the library so the Apple bundle links the same app.
//! Console subsystem: a panic and `eprintln` show in the terminal that
//! launched the process. The same lines are appended next to the exe.

use std::io::Write;
use std::path::PathBuf;

fn log_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("mirage-client-gui.log"))
}

fn note(line: &str) {
    eprintln!("{line}");
    let Some(path) = log_path() else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{line}");
    }
}

fn main() {
    let path = log_path();
    std::panic::set_hook(Box::new(move |info| {
        let line = format!("panic: {info}");
        eprintln!("{line}");
        if let Some(path) = &path {
            if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(file, "{line}");
            }
        }
    }));
    note("mirage-client-gui start");
    if let Err(err) = mirage_client_gui::run() {
        note(&format!("mirage-client-gui: {err}"));
        std::process::exit(1);
    }
}
