//! Connect, disconnect, and quit from outside the picture.
//!
//! The tray menu is one word that flips with the link, plus quit.
//! The taskbar control is one button that flips the link:
//! the thumbnail button under the Windows preview,
//! the dock menu on macOS,
//! and one launcher action on Linux (a second process of this exe
//! hands the word to the running one over a user socket).
//! Quit stays on the tray.

use std::sync::atomic::{AtomicU8, Ordering};

/// Tray id. The taskbar button is the toggle. Quit is tray-only.
pub const CONNECT: u32 = 1;
pub const DISCONNECT: u32 = 2;
pub const QUIT: u32 = 3;
pub const TOGGLE: u32 = 4;
/// The tray calls the window back.
pub const SHOW: u32 = 5;

const NONE: u8 = 0;
const POST_CONNECT: u8 = 1;
const POST_DISCONNECT: u8 = 2;
const POST_QUIT: u8 = 3;
const POST_TOGGLE: u8 = 4;
const POST_SHOW: u8 = 5;

static PENDING: AtomicU8 = AtomicU8::new(NONE);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmd {
    Connect,
    Disconnect,
    Quit,
    Toggle,
    /// The window was closed to the tray and is called back.
    Show,
}

pub fn from_id(id: u32) -> Option<Cmd> {
    match id {
        CONNECT => Some(Cmd::Connect),
        DISCONNECT => Some(Cmd::Disconnect),
        QUIT => Some(Cmd::Quit),
        TOGGLE => Some(Cmd::Toggle),
        SHOW => Some(Cmd::Show),
        _ => None,
    }
}

/// A click that did not arrive on the UI thread (thumbnail, dock, socket).
pub fn post(cmd: Cmd) {
    let value = match cmd {
        Cmd::Connect => POST_CONNECT,
        Cmd::Disconnect => POST_DISCONNECT,
        Cmd::Quit => POST_QUIT,
        Cmd::Toggle => POST_TOGGLE,
        Cmd::Show => POST_SHOW,
    };
    PENDING.store(value, Ordering::SeqCst);
}

pub fn take() -> Option<Cmd> {
    match PENDING.swap(NONE, Ordering::SeqCst) {
        POST_CONNECT => Some(Cmd::Connect),
        POST_DISCONNECT => Some(Cmd::Disconnect),
        POST_QUIT => Some(Cmd::Quit),
        POST_TOGGLE => Some(Cmd::Toggle),
        POST_SHOW => Some(Cmd::Show),
        _ => None,
    }
}

/// Keep the platform control in step with the link. `up` is the picture's link.
pub fn tick(up: bool) {
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    tray::tick(up);
    #[cfg(windows)]
    win::tick(up);
    #[cfg(target_os = "macos")]
    mac::tick(up);
    #[cfg(target_os = "linux")]
    linux::tick(up);
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = up;
    }
}

/// A Linux launcher action is a second process. It delivers the word and
/// does not open a window. Other platforms ignore the flag.
pub fn deliver_and_exit() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux::deliver()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Freedesktop launcher entry. One action, whose name is the next flip.
/// Linux writes it. The tests read it. Other builds keep it for that text.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub fn desktop_entry(exe: &str, up: bool) -> String {
    let exe = exe.replace('"', "");
    let (name, name_ru, name_en) = if up {
        ("disconnect", "отключиться", "disconnect")
    } else {
        ("connect", "подключиться", "connect")
    };
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=m1r4g3\n\
         StartupWMClass=mirage-client-gui\n\
         Exec=\"{exe}\"\n\
         Actions=toggle;\n\
         \n\
         [Desktop Action toggle]\n\
         Name={name}\n\
         Name[ru]={name_ru}\n\
         Name[en]={name_en}\n\
         Exec=\"{exe}\" --shelf toggle\n"
    )
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod tray;

#[cfg(windows)]
mod win;

#[cfg(windows)]
mod wincolor;

#[cfg(target_os = "macos")]
mod mac;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shelf_ids_map_to_commands() {
        assert_eq!(from_id(CONNECT), Some(Cmd::Connect));
        assert_eq!(from_id(DISCONNECT), Some(Cmd::Disconnect));
        assert_eq!(from_id(QUIT), Some(Cmd::Quit));
        assert_eq!(from_id(TOGGLE), Some(Cmd::Toggle));
        assert_eq!(from_id(0), None);
    }

    #[test]
    fn desktop_entry_names_the_toggle() {
        let down = desktop_entry("/opt/m1r4g3/mirage-client-gui", false);
        assert!(down.contains("Actions=toggle;"));
        assert!(down.contains("StartupWMClass=mirage-client-gui"));
        assert!(down.contains("Name[ru]=подключиться"));
        assert!(down.contains("Exec=\"/opt/m1r4g3/mirage-client-gui\" --shelf toggle"));
        assert!(!down.contains("quit"));
        let up = desktop_entry("/opt/m1r4g3/mirage-client-gui", true);
        assert!(up.contains("Name[ru]=отключиться"));
        assert!(!up.contains("Actions=connect;"));
    }
}
