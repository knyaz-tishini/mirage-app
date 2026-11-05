//! Taskbar glyph and tray palm colors from the Windows theme.
//! The taskbar glyph is the taskbar's own foreground.
//! The tray palm is the accent, unless that accent would disappear.

use std::sync::atomic::{AtomicBool, Ordering};

const HKEY_CURRENT_USER: isize = 0x8000_0001u32 as isize;
const RRF_RT_REG_DWORD: u32 = 0x10;

#[link(name = "advapi32")]
extern "system" {
    fn RegGetValueW(
        key: isize,
        sub: *const u16,
        value: *const u16,
        flags: u32,
        typ: *mut u32,
        data: *mut u8,
        len: *mut u32,
    ) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryW(name: *const u16) -> isize;
    fn GetProcAddress(module: isize, name: *const u8) -> *const std::ffi::c_void;
}

static MENUS: AtomicBool = AtomicBool::new(false);

/// Foreground that stays visible on the taskbar. Light on a dark bar.
pub fn neutral() -> [u8; 3] {
    if system_dark() {
        [0xE6, 0xE6, 0xE6]
    } else {
        [0x20, 0x20, 0x20]
    }
}

/// Let the tray menu follow the system theme instead of a white card.
/// AllowDark follows the setting. It does not force a theme.
pub fn allow_dark_menus() {
    if MENUS.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        let lib = LoadLibraryW(wide("uxtheme.dll").as_ptr());
        if lib == 0 {
            return;
        }
        // SetPreferredAppMode is ordinal 135. FlushMenuThemes is 136.
        // Neither name is exported.
        let set = GetProcAddress(lib, 135 as *const u8);
        let flush = GetProcAddress(lib, 136 as *const u8);
        if !set.is_null() {
            let f: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(set);
            f(1);
        }
        if !flush.is_null() {
            let f: unsafe extern "system" fn() = std::mem::transmute(flush);
            f();
        }
    }
}

pub(super) fn system_dark() -> bool {
    // 0 means the taskbar is dark. A missing value is the old light bar.
    dword(
        "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize",
        "SystemUsesLightTheme",
    ) == Some(0)
}

fn dword(sub: &str, value: &str) -> Option<u32> {
    let sub = wide(sub);
    let value = wide(value);
    let mut data = 0u32;
    let mut len = 4u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            sub.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut data as *mut u32 as *mut u8,
            &mut len,
        )
    };
    if rc == 0 {
        Some(data)
    } else {
        None
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
