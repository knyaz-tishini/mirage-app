//! Windows taskbar preview. One button, always enabled.
//!
//! A flags mask of zero marks the button disabled on this taskbar, so the
//! mask carries the icon and the tooltip only. The window is elevated, so
//! Explorer's click is dropped by UIPI unless WM_COMMAND is allowed through.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicPtr, AtomicU32, AtomicU8, Ordering};

use super::{post, Cmd, CONNECT, DISCONNECT, TOGGLE};

const WM_COMMAND: u32 = 0x0111;
const THB_ICON: u32 = 0x2;
const THB_TOOLTIP: u32 = 0x4;
const MSGFLT_ALLOW: u32 = 1;
const MSGFLT_ADD: u32 = 1;
const CLSCTX_INPROC_SERVER: u32 = 1;
const COINIT_APARTMENTTHREADED: u32 = 2;
const SUBCLASS_ID: usize = 0x6D31_7234;

static HWND: AtomicIsize = AtomicIsize::new(0);
static TASKBAR: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
static TB_MSG: AtomicU32 = AtomicU32::new(0);
static READD: AtomicBool = AtomicBool::new(false);
static ADDED: AtomicBool = AtomicBool::new(false);
static SUBCLASSED: AtomicBool = AtomicBool::new(false);
static LAST_UP: AtomicU8 = AtomicU8::new(0);
static LAST_LANG: AtomicU8 = AtomicU8::new(255);
static INK: AtomicU32 = AtomicU32::new(0);
static ICON_ON: AtomicIsize = AtomicIsize::new(0);
static ICON_OFF: AtomicIsize = AtomicIsize::new(0);
static WARNED: AtomicBool = AtomicBool::new(false);
static FILTER_WARNED: AtomicBool = AtomicBool::new(false);

#[repr(C)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

#[repr(C)]
struct ThumbButton {
    mask: u32,
    id: u32,
    bitmap: u32,
    icon: isize,
    tip: [u16; 260],
    flags: u32,
}

#[repr(C)]
struct ChangeFilter {
    size: u32,
    ext_status: u32,
}

#[repr(C)]
struct BitmapInfoHeader {
    size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bit_count: u16,
    compression: u32,
    size_image: u32,
    x_ppm: i32,
    y_ppm: i32,
    clr_used: u32,
    clr_important: u32,
}

#[repr(C)]
struct IconInfo {
    icon: i32,
    x_hot: u32,
    y_hot: u32,
    mask: isize,
    color: isize,
}

#[link(name = "ole32")]
extern "system" {
    fn CoInitializeEx(reserved: *mut std::ffi::c_void, model: u32) -> i32;
    fn CoCreateInstance(
        clsid: *const Guid,
        outer: *mut std::ffi::c_void,
        ctx: u32,
        iid: *const Guid,
        out: *mut *mut std::ffi::c_void,
    ) -> i32;
}

#[link(name = "user32")]
extern "system" {
    fn EnumWindows(cb: unsafe extern "system" fn(isize, isize) -> i32, param: isize) -> i32;
    fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
    fn IsWindowVisible(hwnd: isize) -> i32;
    fn GetWindowTextW(hwnd: isize, buf: *mut u16, max: i32) -> i32;
    fn ChangeWindowMessageFilterEx(hwnd: isize, msg: u32, action: u32, change: *mut ChangeFilter) -> i32;
    fn ChangeWindowMessageFilter(msg: u32, action: u32) -> i32;
    fn RegisterWindowMessageW(name: *const u16) -> u32;
    fn CreateIconIndirect(info: *const IconInfo) -> isize;
    fn DestroyIcon(icon: isize) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcessId() -> u32;
}

#[link(name = "gdi32")]
extern "system" {
    fn CreateDIBSection(
        hdc: isize,
        bmi: *const BitmapInfoHeader,
        usage: u32,
        bits: *mut *mut u8,
        section: isize,
        offset: u32,
    ) -> isize;
    fn CreateBitmap(w: i32, h: i32, planes: u32, bits: u32, data: *const u8) -> isize;
    fn DeleteObject(obj: isize) -> i32;
}

#[link(name = "comctl32")]
extern "system" {
    fn SetWindowSubclass(
        hwnd: isize,
        proc: unsafe extern "system" fn(isize, u32, usize, isize, usize, usize) -> isize,
        id: usize,
        data: usize,
    ) -> i32;
    fn DefSubclassProc(hwnd: isize, msg: u32, wp: usize, lp: isize) -> isize;
}

pub fn tick(up: bool) {
    ensure(up);
}

fn ensure(up: bool) {
    if HWND.load(Ordering::SeqCst) == 0 {
        unsafe { EnumWindows(find_phone, 0) };
    }
    let hwnd = HWND.load(Ordering::SeqCst);
    if hwnd == 0 {
        return;
    }
    if !SUBCLASSED.load(Ordering::SeqCst) {
        let tb = wide("TaskbarButtonCreated");
        let msg = unsafe { RegisterWindowMessageW(tb.as_ptr()) };
        TB_MSG.store(msg, Ordering::SeqCst);
        let ok = unsafe { SetWindowSubclass(hwnd, subclass, SUBCLASS_ID, 0) };
        if ok == 0 {
            warn("taskbar shelf: subclass failed");
            return;
        }
        SUBCLASSED.store(true, Ordering::SeqCst);
    }
    permit(hwnd);
    if TASKBAR.load(Ordering::SeqCst).is_null() && !open_taskbar() {
        return;
    }
    let recolor = sync_ink();
    let readd = READD.swap(false, Ordering::SeqCst);
    if readd {
        ADDED.store(false, Ordering::SeqCst);
        LAST_UP.store(0, Ordering::SeqCst);
        LAST_LANG.store(255, Ordering::SeqCst);
    }
    let want = if up { 2 } else { 1 };
    if !ADDED.load(Ordering::SeqCst) {
        if add_button(hwnd, up) {
            ADDED.store(true, Ordering::SeqCst);
            LAST_UP.store(want, Ordering::SeqCst);
            LAST_LANG.store(crate::text::current() as u8, Ordering::SeqCst);
            tracing::info!("taskbar shelf button");
        }
        return;
    }
    let lang = crate::text::current() as u8;
    let lang_changed = LAST_LANG.swap(lang, Ordering::SeqCst) != lang;
    let state_changed = LAST_UP.swap(want, Ordering::SeqCst) != want;
    if lang_changed || state_changed || recolor {
        update_button(hwnd, up);
    }
}

fn permit(hwnd: isize) {
    unsafe {
        let mut change = ChangeFilter {
            size: std::mem::size_of::<ChangeFilter>() as u32,
            ext_status: 0,
        };
        let cmd_ok = ChangeWindowMessageFilterEx(hwnd, WM_COMMAND, MSGFLT_ALLOW, &mut change);
        let _ = ChangeWindowMessageFilter(WM_COMMAND, MSGFLT_ADD);
        let msg = TB_MSG.load(Ordering::SeqCst);
        if msg != 0 {
            let _ = ChangeWindowMessageFilterEx(hwnd, msg, MSGFLT_ALLOW, &mut change);
            let _ = ChangeWindowMessageFilter(msg, MSGFLT_ADD);
        }
        if cmd_ok == 0 && !FILTER_WARNED.swap(true, Ordering::SeqCst) {
            tracing::warn!("taskbar shelf: command filter failed");
        }
    }
}

fn sync_ink() -> bool {
    let rgb = super::wincolor::neutral();
    let next = (rgb[0] as u32) << 16 | (rgb[1] as u32) << 8 | rgb[2] as u32;
    if INK.swap(next, Ordering::SeqCst) == next {
        return false;
    }
    let on = ICON_ON.swap(0, Ordering::SeqCst);
    let off = ICON_OFF.swap(0, Ordering::SeqCst);
    unsafe {
        if on != 0 {
            DestroyIcon(on);
        }
        if off != 0 {
            DestroyIcon(off);
        }
    }
    true
}

fn open_taskbar() -> bool {
    const CLSID: Guid = Guid {
        data1: 0x56FDF344,
        data2: 0xFD6D,
        data3: 0x11d0,
        data4: [0x95, 0x8A, 0x00, 0x60, 0x97, 0xC9, 0xA0, 0x90],
    };
    const IID: Guid = Guid {
        data1: 0xEA1AFB91,
        data2: 0x9E28,
        data3: 0x4B86,
        data4: [0x90, 0xE9, 0x9E, 0x9F, 0x8A, 0x5E, 0xEF, 0xAF],
    };
    unsafe {
        // Already-initialized is fine. A failed init still tries the create:
        // the host may have brought the thread up itself.
        let _ = CoInitializeEx(std::ptr::null_mut(), COINIT_APARTMENTTHREADED);
        let mut obj = std::ptr::null_mut();
        let hr = CoCreateInstance(&CLSID, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, &IID, &mut obj);
        if hr < 0 || obj.is_null() {
            warn("taskbar shelf: taskbar list unavailable");
            return false;
        }
        let hr_init = vcall0(obj, 3);
        if hr_init < 0 {
            warn("taskbar shelf: HrInit failed");
            return false;
        }
        TASKBAR.store(obj, Ordering::SeqCst);
    }
    true
}

fn add_button(hwnd: isize, up: bool) -> bool {
    if icons().is_none() {
        warn("taskbar shelf: icon failed");
        return false;
    }
    let button = [one(up)];
    let obj = TASKBAR.load(Ordering::SeqCst);
    let hr = unsafe { vcall_buttons(obj, 15, hwnd, button.as_ptr(), 1) };
    if hr < 0 {
        warn("taskbar shelf: add button failed");
        return false;
    }
    true
}

fn update_button(hwnd: isize, up: bool) {
    if icons().is_none() {
        return;
    }
    let button = [one(up)];
    let obj = TASKBAR.load(Ordering::SeqCst);
    if obj.is_null() {
        return;
    }
    unsafe { vcall_buttons(obj, 16, hwnd, button.as_ptr(), 1) };
}

fn icons() -> Option<()> {
    if ICON_ON.load(Ordering::SeqCst) != 0 && ICON_OFF.load(Ordering::SeqCst) != 0 {
        return Some(());
    }
    let on = icon(false);
    let off = icon(true);
    if on == 0 || off == 0 {
        if on != 0 {
            unsafe { DestroyIcon(on) };
        }
        if off != 0 {
            unsafe { DestroyIcon(off) };
        }
        return None;
    }
    ICON_ON.store(on, Ordering::SeqCst);
    ICON_OFF.store(off, Ordering::SeqCst);
    Some(())
}

fn one(up: bool) -> ThumbButton {
    let icon = if up { ICON_ON.load(Ordering::SeqCst) } else { ICON_OFF.load(Ordering::SeqCst) };
    let tip = if up {
        crate::text::tr("shelf.disconnect")
    } else {
        crate::text::tr("shelf.connect")
    };
    button(TOGGLE, icon, tip)
}

fn button(id: u32, icon: isize, tip: &str) -> ThumbButton {
    // No THB_FLAGS. A zero flags word in the mask disables the button,
    // and a disabled thumbnail button draws but does not click.
    let mut button = ThumbButton {
        mask: THB_ICON | THB_TOOLTIP,
        id,
        bitmap: 0,
        icon,
        tip: [0; 260],
        flags: 0,
    };
    let mut n = 0;
    for unit in tip.encode_utf16() {
        if n + 1 >= button.tip.len() {
            break;
        }
        button.tip[n] = unit;
        n += 1;
    }
    button
}

unsafe fn vcall0(obj: *mut std::ffi::c_void, slot: usize) -> i32 {
    let vtbl = *(obj as *const *const usize);
    let f: unsafe extern "system" fn(*mut std::ffi::c_void) -> i32 = std::mem::transmute(*vtbl.add(slot));
    f(obj)
}

unsafe fn vcall_buttons(obj: *mut std::ffi::c_void, slot: usize, hwnd: isize, buttons: *const ThumbButton, count: u32) -> i32 {
    let vtbl = *(obj as *const *const usize);
    let f: unsafe extern "system" fn(*mut std::ffi::c_void, isize, u32, *const ThumbButton) -> i32 =
        std::mem::transmute(*vtbl.add(slot));
    f(obj, hwnd, count, buttons)
}

/// `play` is the triangle (link is down). The square is the link up.
/// Both use the taskbar foreground.
fn icon(play: bool) -> isize {
    let ink = INK.load(Ordering::SeqCst);
    let r = ((ink >> 16) & 0xFF) as u8;
    let g = ((ink >> 8) & 0xFF) as u8;
    let b = (ink & 0xFF) as u8;
    let mut header = BitmapInfoHeader {
        size: std::mem::size_of::<BitmapInfoHeader>() as u32,
        width: 16,
        height: -16,
        planes: 1,
        bit_count: 32,
        compression: 0,
        size_image: 0,
        x_ppm: 0,
        y_ppm: 0,
        clr_used: 0,
        clr_important: 0,
    };
    let mut bits = std::ptr::null_mut();
    let color = unsafe { CreateDIBSection(0, &mut header, 0, &mut bits, 0, 0) };
    if color == 0 || bits.is_null() {
        return 0;
    }
    unsafe {
        let px = std::slice::from_raw_parts_mut(bits, 16 * 16 * 4);
        for i in 0..(16 * 16) {
            let x = i % 16;
            let y = i / 16;
            let on = if play {
                let rel = (y as i32) - 8;
                let half = 6 - rel.abs();
                (x as i32) >= 4 && (x as i32) < 4 + half
            } else {
                (4..12).contains(&x) && (4..12).contains(&y)
            };
            let o = i * 4;
            if on {
                px[o] = b;
                px[o + 1] = g;
                px[o + 2] = r;
                px[o + 3] = 0xFF;
            } else {
                px[o] = 0;
                px[o + 1] = 0;
                px[o + 2] = 0;
                px[o + 3] = 0;
            }
        }
    }
    let mask_bits = [0xFFu8; 32];
    let mask = unsafe { CreateBitmap(16, 16, 1, 1, mask_bits.as_ptr()) };
    if mask == 0 {
        unsafe { DeleteObject(color) };
        return 0;
    }
    let info = IconInfo { icon: 1, x_hot: 0, y_hot: 0, mask, color };
    let icon = unsafe { CreateIconIndirect(&info) };
    unsafe {
        DeleteObject(color);
        DeleteObject(mask);
    }
    icon
}

fn warn(msg: &str) {
    if !WARNED.swap(true, Ordering::SeqCst) {
        tracing::warn!("{msg}");
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe extern "system" fn find_phone(hwnd: isize, _param: isize) -> i32 {
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid != GetCurrentProcessId() || IsWindowVisible(hwnd) == 0 {
        return 1;
    }
    let mut buf = [0u16; 64];
    let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    if n <= 0 {
        return 1;
    }
    let title = String::from_utf16_lossy(&buf[..n as usize]);
    if title.starts_with("m1r4g3") {
        HWND.store(hwnd, Ordering::SeqCst);
        return 0;
    }
    1
}

unsafe extern "system" fn subclass(hwnd: isize, msg: u32, wp: usize, lp: isize, _id: usize, _data: usize) -> isize {
    let tb = TB_MSG.load(Ordering::SeqCst);
    if tb != 0 && msg == tb {
        READD.store(true, Ordering::SeqCst);
    }
    if msg == WM_COMMAND {
        let note = (wp >> 16) & 0xFFFF;
        let id = (wp & 0xFFFF) as u32;
        if id == TOGGLE || id == CONNECT || id == DISCONNECT {
            tracing::info!("shelf thumb {id} note {note:#x}");
            post(Cmd::Toggle);
            return 0;
        }
    }
    DefSubclassProc(hwnd, msg, wp, lp)
}
