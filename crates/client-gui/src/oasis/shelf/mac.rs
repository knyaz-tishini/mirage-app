//! macOS dock menu. AppKit asks the application delegate for it.
//! winit owns that delegate, so the method is added onto its class.
//! One item flips the link. Quit stays on the status item.

use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use super::{post, Cmd};

static READY: AtomicBool = AtomicBool::new(false);
static HANDLER: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
static UP: AtomicBool = AtomicBool::new(false);

#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const u8) -> *mut std::ffi::c_void;
    fn sel_registerName(name: *const u8) -> *mut std::ffi::c_void;
    fn class_addMethod(
        cls: *mut std::ffi::c_void,
        name: *mut std::ffi::c_void,
        imp: *const std::ffi::c_void,
        types: *const u8,
    ) -> u8;
    fn objc_allocateClassPair(
        super_cls: *mut std::ffi::c_void,
        name: *const u8,
        extra: usize,
    ) -> *mut std::ffi::c_void;
    fn objc_registerClassPair(cls: *mut std::ffi::c_void);
    fn objc_msgSend();
}

pub fn tick(up: bool) {
    UP.store(up, Ordering::SeqCst);
    if READY.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe { install() };
}

unsafe fn install() {
    let handler = handler();
    if handler.is_null() {
        tracing::warn!("dock shelf: no handler");
        return;
    }
    HANDLER.store(handler, Ordering::SeqCst);
    set_dock_icon();
    let class = objc_getClass(b"WinitApplicationDelegate\0".as_ptr());
    if class.is_null() {
        tracing::warn!("dock shelf: no application delegate");
        return;
    }
    let added = class_addMethod(
        class,
        sel_registerName(b"applicationDockMenu:\0".as_ptr()),
        dock_menu as *const std::ffi::c_void,
        b"@@:@\0".as_ptr(),
    );
    if added == 0 {
        tracing::warn!("dock shelf: dock menu was not added");
    }
}

unsafe fn handler() -> *mut std::ffi::c_void {
    let name = b"M1r4g3Shelf\0".as_ptr();
    let mut class = objc_allocateClassPair(objc_getClass(b"NSObject\0".as_ptr()), name, 0);
    let fresh = !class.is_null();
    if class.is_null() {
        class = objc_getClass(name);
    }
    if class.is_null() {
        return std::ptr::null_mut();
    }
    class_addMethod(
        class,
        sel_registerName(b"toggle:\0".as_ptr()),
        on_toggle as *const std::ffi::c_void,
        b"v@:@\0".as_ptr(),
    );
    if fresh {
        objc_registerClassPair(class);
    }
    msg0(msg0(class, b"alloc\0"), b"init\0")
}

extern "C" fn on_toggle(_this: *mut std::ffi::c_void, _cmd: *mut std::ffi::c_void, _sender: *mut std::ffi::c_void) {
    post(Cmd::Toggle);
}

extern "C" fn dock_menu(_this: *mut std::ffi::c_void, _cmd: *mut std::ffi::c_void, _app: *mut std::ffi::c_void) -> *mut std::ffi::c_void {
    unsafe {
        let handler = HANDLER.load(Ordering::SeqCst);
        let menu = msg0(msg0(objc_getClass(b"NSMenu\0".as_ptr()), b"alloc\0"), b"init\0");
        if menu.is_null() {
            return std::ptr::null_mut();
        }
        msg_bool(menu, b"setAutoenablesItems:\0", 0);
        let up = UP.load(Ordering::SeqCst);
        let label = if up {
            crate::text::tr("shelf.disconnect")
        } else {
            crate::text::tr("shelf.connect")
        };
        add_item(menu, handler, label, b"toggle:\0", true);
        msg0(menu, b"autorelease\0");
        menu
    }
}

unsafe fn add_item(menu: *mut std::ffi::c_void, handler: *mut std::ffi::c_void, title: &str, action: &[u8], enabled: bool) {
    let mut title0 = title.as_bytes().to_vec();
    title0.push(0);
    let label = msg1(
        objc_getClass(b"NSString\0".as_ptr()),
        b"stringWithUTF8String:\0",
        title0.as_ptr() as *mut std::ffi::c_void,
    );
    let empty = msg1(
        objc_getClass(b"NSString\0".as_ptr()),
        b"stringWithUTF8String:\0",
        b"\0".as_ptr() as *mut std::ffi::c_void,
    );
    let item = msg3(
        msg0(objc_getClass(b"NSMenuItem\0".as_ptr()), b"alloc\0"),
        b"initWithTitle:action:keyEquivalent:\0",
        label,
        sel_registerName(action.as_ptr()),
        empty,
    );
    if item.is_null() {
        return;
    }
    msg1(item, b"setTarget:\0", handler);
    msg_bool(item, b"setEnabled:\0", if enabled { 1 } else { 0 });
    msg1(menu, b"addItem:\0", item);
}

unsafe fn msg0(obj: *mut std::ffi::c_void, sel: &[u8]) -> *mut std::ffi::c_void {
    if obj.is_null() {
        return std::ptr::null_mut();
    }
    let f: unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void) -> *mut std::ffi::c_void =
        std::mem::transmute(objc_msgSend as *const ());
    f(obj, sel_registerName(sel.as_ptr()))
}

unsafe fn msg1(obj: *mut std::ffi::c_void, sel: &[u8], a: *mut std::ffi::c_void) -> *mut std::ffi::c_void {
    if obj.is_null() {
        return std::ptr::null_mut();
    }
    let f: unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void, *mut std::ffi::c_void) -> *mut std::ffi::c_void =
        std::mem::transmute(objc_msgSend as *const ());
    f(obj, sel_registerName(sel.as_ptr()), a)
}

unsafe fn msg3(
    obj: *mut std::ffi::c_void,
    sel: &[u8],
    a: *mut std::ffi::c_void,
    b: *mut std::ffi::c_void,
    c: *mut std::ffi::c_void,
) -> *mut std::ffi::c_void {
    if obj.is_null() {
        return std::ptr::null_mut();
    }
    let f: unsafe extern "C" fn(
        *mut std::ffi::c_void,
        *mut std::ffi::c_void,
        *mut std::ffi::c_void,
        *mut std::ffi::c_void,
        *mut std::ffi::c_void,
    ) -> *mut std::ffi::c_void = std::mem::transmute(objc_msgSend as *const ());
    f(obj, sel_registerName(sel.as_ptr()), a, b, c)
}

/// The Dock reads the application icon. winit drops `set_window_icon`
/// on macOS, so the palm is set here. The menu-bar palm is the tray.
unsafe fn set_dock_icon() {
    const PALM: &[u8] = include_bytes!("../palm64.rgba");
    let Some(image) = image::RgbaImage::from_raw(64, 64, PALM.to_vec()) else {
        return;
    };
    let image = image::DynamicImage::ImageRgba8(image);
    let mut png = std::io::Cursor::new(Vec::new());
    if image.write_to(&mut png, image::ImageFormat::Png).is_err() {
        return;
    }
    let bytes = png.into_inner();
    let data = msg_bytes(
        objc_getClass(b"NSData\0".as_ptr()),
        b"dataWithBytes:length:\0",
        bytes.as_ptr() as *mut std::ffi::c_void,
        bytes.len(),
    );
    let icon = msg1(
        msg0(objc_getClass(b"NSImage\0".as_ptr()), b"alloc\0"),
        b"initWithData:\0",
        data,
    );
    let app = msg0(objc_getClass(b"NSApplication\0".as_ptr()), b"sharedApplication\0");
    msg1(app, b"setApplicationIconImage:\0", icon);
    tracing::info!("dock palm");
}

unsafe fn msg_bytes(
    obj: *mut std::ffi::c_void,
    sel: &[u8],
    bytes: *mut std::ffi::c_void,
    len: usize,
) -> *mut std::ffi::c_void {
    if obj.is_null() {
        return std::ptr::null_mut();
    }
    let f: unsafe extern "C" fn(
        *mut std::ffi::c_void,
        *mut std::ffi::c_void,
        *mut std::ffi::c_void,
        usize,
    ) -> *mut std::ffi::c_void = std::mem::transmute(objc_msgSend as *const ());
    f(obj, sel_registerName(sel.as_ptr()), bytes, len)
}

unsafe fn msg_bool(obj: *mut std::ffi::c_void, sel: &[u8], value: i8) {
    if obj.is_null() {
        return;
    }
    let f: unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void, i8) =
        std::mem::transmute(objc_msgSend as *const ());
    f(obj, sel_registerName(sel.as_ptr()), value);
}
