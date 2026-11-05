//! Store-build C ABI. Swift is the packet-flow interpreter. This module
//! redeems a claim, runs the subscriber, and drives the packet stack.

use std::ffi::{CStr, CString, c_char, c_void};
use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use mirage_subscriber_client::{
    redeem_artefact_json, EgressScope, EntranceArtefact, PlaneConsumer, SubscriberClient,
    SubscriberClientOptions,
};
use mirage_tun_core::policy::{PolicyConfigRequest, PolicyDecision};
use mirage_tun_core::stack_config::{ChainEntry, StackConfig};
use mirage_tun_core::traits::TunStack;
use mirage_tun_ios::packet_tunnel::{WriteCallback, WriteFn};
use mirage_tun_ios::stack::{clone_inbound_tx, IosTunStack};

const PANEL_ORIGIN: &str = "https://panel.m1r4g3.bid";

struct Session {
    stop_tx: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

static SESSION: OnceLock<Mutex<Option<usize>>> = OnceLock::new();

fn session_slot() -> &'static Mutex<Option<usize>> {
    SESSION.get_or_init(|| Mutex::new(None))
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
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(_) => return std::ptr::null_mut(),
    };
    let json = match rt.block_on(redeem_artefact_json(PANEL_ORIGIN, &code)) {
        Ok(json) => json,
        Err(_) => return std::ptr::null_mut(),
    };
    CString::new(json)
        .map(CString::into_raw)
        .unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub unsafe extern "C" fn mirage_store_free(ptr: *mut c_char) {
    if !ptr.is_null() {
        drop(unsafe { CString::from_raw(ptr) });
    }
}

#[no_mangle]
pub unsafe extern "C" fn mirage_tun_start_artefact(
    json: *const u8,
    json_len: usize,
    write_fn: WriteFn,
    write_ctx: *mut c_void,
) -> *mut c_void {
    if json.is_null() || json_len == 0 {
        return std::ptr::null_mut();
    }
    let bytes = unsafe { std::slice::from_raw_parts(json, json_len) };
    let artefact = match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => return std::ptr::null_mut(),
    };
    let write_cb = std::sync::Arc::new(unsafe { WriteCallback::new(write_fn, write_ctx) });
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let thread = match std::thread::Builder::new().name("mirage-tun".to_owned()).spawn(move || {
        let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).try_init();
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(err) => {
                let _ = ready_tx.send(Err(err.to_string()));
                return;
            }
        };
        rt.block_on(async move {
            match start_session(&artefact, write_cb).await {
                Ok(client) => {
                    let _ = ready_tx.send(Ok(()));
                    let _ = stop_rx.await;
                    client.shutdown();
                }
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                }
            }
        });
    }) {
        Ok(thread) => thread,
        Err(_) => return std::ptr::null_mut(),
    };
    match ready_rx.recv() {
        Ok(Ok(())) => {}
        _ => {
            let _ = thread.join();
            return std::ptr::null_mut();
        }
    }
    let session = Box::new(Session { stop_tx: Some(stop_tx), thread: Some(thread) });
    let ptr = Box::into_raw(session);
    if let Ok(mut slot) = session_slot().lock() {
        *slot = Some(ptr as usize);
    }
    ptr.cast()
}

#[no_mangle]
pub unsafe extern "C" fn mirage_tun_process_packet(ctx: *mut c_void, data: *const u8, data_len: usize) {
    if ctx.is_null() || data.is_null() || data_len == 0 {
        return;
    }
    let packet = unsafe { std::slice::from_raw_parts(data, data_len) }.to_vec();
    if let Some(tx) = clone_inbound_tx() {
        let _ = tx.try_send(packet);
    }
}

#[no_mangle]
pub unsafe extern "C" fn mirage_tun_stop(ctx: *mut c_void) {
    if ctx.is_null() {
        return;
    }
    if let Ok(mut slot) = session_slot().lock() {
        if *slot == Some(ctx as usize) {
            *slot = None;
        }
    }
    let mut session = unsafe { Box::from_raw(ctx.cast::<Session>()) };
    session.stop();
}

async fn start_session(
    artefact_json: &str,
    write_cb: std::sync::Arc<WriteCallback>,
) -> Result<SubscriberClient, String> {
    let artefact = EntranceArtefact::from_json(artefact_json).map_err(|_| "artefact".to_owned())?;
    let (client, socks) = bind_subscriber(&artefact).await?;
    if !wait_for_socks(socks).await {
        client.shutdown();
        return Err("socks".to_owned());
    }
    IosTunStack::set_write_callback(write_cb).await;
    let cancel = tokio_util::sync::CancellationToken::new();
    if let Err(err) = IosTunStack::start(stack_config(socks), cancel).await {
        let _ = err;
        client.shutdown();
        return Err("stack".to_owned());
    }
    Ok(client)
}

async fn bind_subscriber(artefact: &EntranceArtefact) -> Result<(SubscriberClient, SocketAddr), String> {
    for _ in 0..3 {
        let probe = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(probe) => probe,
            Err(_) => continue,
        };
        let socks = match probe.local_addr() {
            Ok(addr) => addr,
            Err(_) => continue,
        };
        drop(probe);
        let options = SubscriberClientOptions {
            socks5_bind: socks,
            tun_api_url: None,
            status_file: None,
            scope: EgressScope::all(),
            start_socks: true,
            // The packet tunnel reads this SOCKS listener. The session
            // feeds the userspace stack, not a raw-IP device.
            plane_consumer: PlaneConsumer::Stack,
        };
        if let Ok(client) = SubscriberClient::build(artefact, options).await {
            return Ok((client, socks));
        }
    }
    Err("subscriber".to_owned())
}

async fn wait_for_socks(socks: SocketAddr) -> bool {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(socks).await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

fn stack_config(socks: SocketAddr) -> StackConfig {
    StackConfig {
        tun_name: "utun0".to_owned(),
        tun_addr: "10.0.85.1/24".to_owned(),
        tun_addr_v6: Some("fd00:7::2/64".to_owned()),
        mtu: 1200,
        chains: vec![ChainEntry {
            name: "default".to_owned(),
            socks5_addr: socks,
            group: None,
            nature: Default::default(),
        }],
        policy: PolicyConfigRequest {
            rules: Vec::new(),
            default_action: PolicyDecision::default_chain(),
        },
        chain_dns: Vec::new(),
        relay_addrs: vec![std::net::IpAddr::from([127, 0, 0, 1])],
        dns_upstream: "1.1.1.1:53".parse().expect("dns upstream"),
        dns_cache_ttl_secs: 60,
        dns_cache_max: 256,
        dns_concurrency: 64,
        socks5_concurrency: 64,
        per_chain_tun: false,
    }
}
