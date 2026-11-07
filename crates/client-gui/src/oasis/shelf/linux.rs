//! Linux launcher action. A desktop entry's toggle starts this exe again
//! with `--shelf toggle`. That process writes one word to the running
//! window's socket and exits. The window applies it on the next tick.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{desktop_entry, post, Cmd};

static SERVED: AtomicBool = AtomicBool::new(false);

pub fn tick(up: bool) {
    publish(up);
    if SERVED.swap(true, Ordering::SeqCst) {
        return;
    }
    serve();
}

pub fn deliver() -> bool {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("--shelf") {
        return false;
    }
    let word = match args.next().as_deref() {
        Some("connect") => "connect\n",
        Some("disconnect") => "disconnect\n",
        Some("toggle") => "toggle\n",
        _ => return true,
    };
    match UnixStream::connect(socket_path()) {
        Ok(mut stream) => {
            let _ = stream.write_all(word.as_bytes());
        }
        Err(err) => tracing::warn!("shelf: the window is not listening: {err}"),
    }
    true
}

fn publish(up: bool) {
    let Some(dir) = applications_dir() else {
        return;
    };
    let Some(exe) = std::env::current_exe().ok() else {
        return;
    };
    let Some(exe) = exe.to_str() else {
        return;
    };
    let text = desktop_entry(exe, up);
    let path = dir.join("m1r4g3.desktop");
    if std::fs::read_to_string(&path).ok().as_deref() == Some(text.as_str()) {
        return;
    }
    if let Err(err) = std::fs::create_dir_all(&dir) {
        tracing::warn!("shelf: applications dir: {err}");
        return;
    }
    let tmp = dir.join("m1r4g3.desktop.tmp");
    if let Err(err) = std::fs::write(&tmp, &text) {
        tracing::warn!("shelf: desktop entry: {err}");
        return;
    }
    if let Err(err) = std::fs::rename(&tmp, &path) {
        tracing::warn!("shelf: desktop entry: {err}");
    }
}

fn applications_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        return Some(PathBuf::from(xdg).join("applications"));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".local/share/applications"))
}

fn socket_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("m1r4g3-shelf.sock");
    }
    let uid = unsafe { getuid() };
    std::env::temp_dir().join(format!("m1r4g3-shelf-{uid}.sock"))
}

fn serve() {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(err) => {
            tracing::warn!("shelf: socket: {err}");
            return;
        }
    };
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    let _ = std::thread::Builder::new().name("shelf".into()).spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut stream) = conn else {
                continue;
            };
            let mut buf = [0u8; 32];
            let Ok(n) = stream.read(&mut buf) else {
                continue;
            };
            match &buf[..n] {
                b"connect" | b"connect\n" => post(Cmd::Connect),
                b"disconnect" | b"disconnect\n" => post(Cmd::Disconnect),
                b"toggle" | b"toggle\n" => post(Cmd::Toggle),
                _ => {}
            }
        }
    });
}

#[link(name = "c")]
extern "C" {
    fn getuid() -> u32;
}
