//! The window the tray keeps.
//!
//! The cross closes the window, not the application: the window goes away
//! and the application stays in the tray, with the link and the device as
//! they are. A click on the tray icon brings the window back.
//!
//! uzor has no "hide" command, and the native host leaves its loop when the
//! last window is closed -- so the window is hidden the Windows way. The
//! host keeps it, so the process stays alive, and the tray is there to call
//! it back.

/// Hide the window. The application stays alive in the tray.
pub fn hide() -> bool {
    imp::hide()
}

/// Bring the window back out of the tray.
pub fn show() -> bool {
    imp::show()
}

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicIsize, Ordering};

    /// The app window. Found once and kept: the handle stays good while the
    /// window lives, hidden or not.
    static HWND: AtomicIsize = AtomicIsize::new(0);

    const SW_HIDE: i32 = 0;
    const SW_SHOW: i32 = 5;
    const SW_RESTORE: i32 = 9;

    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(cb: unsafe extern "system" fn(isize, isize) -> i32, param: isize) -> i32;
        fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
        fn GetWindowTextW(hwnd: isize, buf: *mut u16, max: i32) -> i32;
        fn ShowWindow(hwnd: isize, cmd: i32) -> i32;
        fn SetForegroundWindow(hwnd: isize) -> i32;
        fn IsWindow(hwnd: isize) -> i32;
        fn IsIconic(hwnd: isize) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcessId() -> u32;
    }

    /// The one window of this process. Its title starts with "m1r4g3"; the
    /// tray's own hidden window carries no title.
    fn window() -> isize {
        let cached = HWND.load(Ordering::SeqCst);
        if cached != 0 && unsafe { IsWindow(cached) } != 0 {
            return cached;
        }
        HWND.store(0, Ordering::SeqCst);
        unsafe { EnumWindows(hunt, 0) };
        HWND.load(Ordering::SeqCst)
    }

    unsafe extern "system" fn hunt(hwnd: isize, _param: isize) -> i32 {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid != GetCurrentProcessId() {
            return 1;
        }
        let mut buf = [0u16; 64];
        let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        if n <= 0 {
            return 1;
        }
        let title = String::from_utf16_lossy(&buf[..n as usize]);
        if !title.starts_with("m1r4g3") {
            return 1;
        }
        HWND.store(hwnd, Ordering::SeqCst);
        0
    }

    pub fn hide() -> bool {
        let hwnd = window();
        if hwnd == 0 {
            tracing::warn!("window: no window to hide");
            return false;
        }
        unsafe { ShowWindow(hwnd, SW_HIDE) };
        tracing::info!("window: hidden -- the application stays in the tray");
        true
    }

    pub fn show() -> bool {
        let hwnd = window();
        if hwnd == 0 {
            tracing::warn!("window: no window to show");
            return false;
        }
        unsafe {
            let command = if IsIconic(hwnd) != 0 { SW_RESTORE } else { SW_SHOW };
            ShowWindow(hwnd, command);
            SetForegroundWindow(hwnd);
        }
        tracing::info!("window: back from the tray");
        true
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn hide() -> bool {
        false
    }

    pub fn show() -> bool {
        false
    }
}
