//! mirage-client-gui — Mirage client GUI.
//!
//! The window lives in the library so the Apple bundle links the same app.
//! The Windows subsystem does not allocate a console. A parent console is
//! attached so a terminal launch still sees `eprintln`. The same lines are
//! appended next to the exe.

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::io::Write;
use std::path::PathBuf;

/// Attach the parent console when one exists. Explorer has none, so a
/// normal launch stays quiet. Failure is that no-console case.
#[cfg(target_os = "windows")]
fn attach_parent_console() {
    #[link(name = "kernel32")]
    extern "system" {
        fn AttachConsole(pid: u32) -> i32;
    }
    // ATTACH_PARENT_PROCESS
    const PARENT: u32 = 0xFFFF_FFFF;
    unsafe {
        let _ = AttachConsole(PARENT);
    }
}

#[cfg(not(target_os = "windows"))]
fn attach_parent_console() {}

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
    attach_parent_console();
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
