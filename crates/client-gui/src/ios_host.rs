//! iOS symbols the Swift bundle already links. The picture is the oasis
//! window (`mirage_gui_run`). These hooks start the packet tunnel. Nothing
//! here paints.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

struct Hooks {
    start: Option<extern "C" fn(*const c_char)>,
    stop: Option<extern "C" fn()>,
    status: Option<extern "C" fn() -> i32>,
    bind: Option<extern "C" fn(*const c_char)>,
    unlink: Option<extern "C" fn()>,
}

static CABINET_BOUND: AtomicBool = AtomicBool::new(false);

/// Cabinet claim is in the keychain. Swift reports it through `mirage_gui_set_bound`.
pub fn cabinet_bound() -> bool {
    CABINET_BOUND.load(Ordering::Relaxed)
}

/// Start the packet tunnel. `artefact` is the standalone document; cabinet passes
/// none and the extension reads the keychain device.
pub fn tunnel_start(artefact: Option<&str>) -> bool {
    let start = hooks().lock().unwrap_or_else(PoisonError::into_inner).start;
    let Some(start) = start else {
        return false;
    };
    match artefact {
        Some(text) => {
            let Ok(raw) = CString::new(text) else {
                return false;
            };
            start(raw.as_ptr());
        }
        None => start(std::ptr::null()),
    }
    true
}

pub fn tunnel_stop() {
    if let Some(stop) = hooks().lock().unwrap_or_else(PoisonError::into_inner).stop {
        stop();
    }
}

fn hooks() -> &'static Mutex<Hooks> {
    static HOOKS: OnceLock<Mutex<Hooks>> = OnceLock::new();
    HOOKS.get_or_init(|| {
        Mutex::new(Hooks {
            start: None,
            stop: None,
            status: None,
            bind: None,
            unlink: None,
        })
    })
}

/// The old bitmap pump. The oasis window does not use it.
#[no_mangle]
pub extern "C" fn mirage_gui_frame(_width_px: u32, _height_px: u32, _dpr: f64, _pixels: *mut u8) -> i32 {
    1
}

#[no_mangle]
pub extern "C" fn mirage_gui_pointer(_phase: i32, _x: f64, _y: f64) {}

#[no_mangle]
pub extern "C" fn mirage_gui_text(_utf8: *const c_char) {}

#[no_mangle]
pub extern "C" fn mirage_gui_key(_key: i32) {}

#[no_mangle]
pub extern "C" fn mirage_gui_wants_keyboard() -> i32 {
    0
}

#[no_mangle]
pub extern "C" fn mirage_gui_bind_result(_utf8: *const c_char) {}

#[no_mangle]
pub extern "C" fn mirage_gui_set_bound(bound: i32, _name: *const c_char) {
    CABINET_BOUND.store(bound != 0, Ordering::Relaxed);
}

#[no_mangle]
pub extern "C" fn mirage_gui_set_hooks(
    start: Option<extern "C" fn(*const c_char)>,
    stop: Option<extern "C" fn()>,
    status: Option<extern "C" fn() -> i32>,
    bind: Option<extern "C" fn(*const c_char)>,
) {
    let mut guard = hooks().lock().unwrap_or_else(PoisonError::into_inner);
    guard.start = start;
    guard.stop = stop;
    guard.status = status;
    guard.bind = bind;
}

#[no_mangle]
pub extern "C" fn mirage_gui_set_unlink(unlink: Option<extern "C" fn()>) {
    hooks().lock().unwrap_or_else(PoisonError::into_inner).unlink = unlink;
}

fn redeem_claim(code: &str) -> Result<String, String> {
    let code = code.trim();
    if code.is_empty() || code.len() > 512 {
        return Err("empty".to_owned());
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|_| "transport".to_owned())?;
    let origin = "https://panel.m1r4g3.bid";
    let (status, body) = post_json(
        &client,
        &format!("{origin}/public/claims/redeem"),
        serde_json::json!({ "claim": code }),
    )?;
    if status != 200 && status != 202 {
        return Err("refused".to_owned());
    }
    let poll = body
        .get("poll")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "refused".to_owned())?
        .to_owned();
    let started = std::time::Instant::now();
    let budget = std::time::Duration::from_secs(90);
    loop {
        if started.elapsed() >= budget {
            return Err("pending".to_owned());
        }
        let (status, body) = post_json(
            &client,
            &format!("{origin}/public/claims/poll"),
            serde_json::json!({ "poll": poll }),
        )?;
        if status == 429 {
            std::thread::sleep(std::time::Duration::from_secs(2));
            continue;
        }
        if status != 200 && status != 202 {
            return Err("refused".to_owned());
        }
        let state = body.get("status").and_then(|value| value.as_str()).unwrap_or("");
        if state == "ready" {
            let document = body
                .get("artefact")
                .cloned()
                .filter(|value| !value.is_null())
                .or_else(|| body.get("packet").cloned().filter(|value| !value.is_null()))
                .ok_or_else(|| "empty-artefact".to_owned())?;
            return serde_json::to_string(&document).map_err(|_| "refused".to_owned());
        }
        if state == "pending" {
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        }
        return Err("refused".to_owned());
    }
}

fn post_json(
    client: &reqwest::blocking::Client,
    url: &str,
    body: serde_json::Value,
) -> Result<(u16, serde_json::Value), String> {
    let response = client
        .post(url)
        .json(&body)
        .send()
        .map_err(|_| "transport".to_owned())?;
    let status = response.status().as_u16();
    let text = response.text().map_err(|_| "transport".to_owned())?;
    let value = if text.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null)
    };
    Ok((status, value))
}

#[no_mangle]
pub unsafe extern "C" fn mirage_claim_redeem(code: *const c_char) -> *mut c_char {
    if code.is_null() {
        return std::ptr::null_mut();
    }
    let code = match unsafe { CStr::from_ptr(code) }.to_str() {
        Ok(code) => code.to_owned(),
        Err(_) => return std::ptr::null_mut(),
    };
    let json = match redeem_claim(&code) {
        Ok(json) => json,
        Err(_) => return std::ptr::null_mut(),
    };
    CString::new(json).map(CString::into_raw).unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub unsafe extern "C" fn mirage_store_free(ptr: *mut c_char) {
    if !ptr.is_null() {
        drop(unsafe { CString::from_raw(ptr) });
    }
}
