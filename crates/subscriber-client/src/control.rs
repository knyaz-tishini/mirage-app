//! Loopback control API. The desktop parent is a client of this process.
//!
//! One HTTP/1.1 request per accepted connection. No cabinet, account, or
//! subscriber routes. A TUN wish starts `mirage2tun` when that binary was
//! passed in and the edge session is up. Bypass names are not copied into
//! the SOCKS scope. Site and app lists are the SOCKS and HTTP tunnel filter.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::process::Child;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tracing::{debug, error, info};

use mirage_control_plane::{CountryCode, ExitPreference};

use crate::scope::EgressScope;
use crate::status::EdgeState;
use crate::supervisor_slot::SupervisorSlot;
use crate::tun_mode::{self, TunMode, WireMode};

const READ_CAP: usize = 16 * 1024;

/// What the parent asked for. Listeners run only while `session_up` is set.
#[derive(Clone, Debug)]
pub struct Wish {
    pub session_up: bool,
    pub socks: bool,
    pub http: bool,
    pub tun: bool,
    /// Only `"full"` or `"split"`.
    pub tun_mode: Option<String>,
    pub bypass: Vec<String>,
    pub services: Vec<String>,
    pub apps: Vec<String>,
    /// TUN only. These names stay direct. SOCKS and HTTP ignore them.
    pub bypass_services: Vec<String>,
    pub bypass_apps: Vec<String>,
    pub tun_bin_set: bool,
    /// `"auto"` or an ISO alpha-2 country. The dialer reads this on the next session.
    pub exit: String,
}

impl Default for Wish {
    fn default() -> Self {
        Self {
            session_up: false,
            socks: true,
            http: false,
            tun: false,
            tun_mode: None,
            bypass: Vec::new(),
            services: Vec::new(),
            apps: Vec::new(),
            bypass_services: Vec::new(),
            bypass_apps: Vec::new(),
            tun_bin_set: false,
            exit: "auto".to_owned(),
        }
    }
}

/// Inputs to [`status_document`]. The live server fills this from real tasks.
pub struct View {
    pub session_up: bool,
    pub edge_state: Option<String>,
    pub socks_running: bool,
    pub http_running: bool,
    pub tun: bool,
    pub tun_mode: Option<String>,
    pub bypass: Vec<String>,
    pub services: Vec<String>,
    pub apps: Vec<String>,
    /// TUN names that stay direct. Not a SOCKS filter.
    pub bypass_services: Vec<String>,
    pub bypass_apps: Vec<String>,
    /// `socks`, `http`, or `tun`. The selection, not proof that it is carrying.
    pub mode: String,
    /// Set when TUN is selected and this process did not spawn mirage2tun.
    pub tun_block: Option<String>,
    /// Why the selected tun process is not up. Empty when it is up or not selected.
    pub error: Option<String>,
    /// Countries the edge last reported. Empty until a session is accepted.
    pub countries: Vec<String>,
    /// Requested exit: `"auto"` or an ISO alpha-2 code.
    pub exit: String,
}

/// Wish change a route asks the server to store. `None` stores nothing.
#[derive(Debug)]
pub enum Patch {
    None,
    Session(bool),
    Listeners { socks: bool, http: bool },
    /// `enabled` is the value to store. The switch is the wish, not a live tunnel.
    Tun {
        enabled: bool,
        mode: Option<String>,
        bypass: Option<Vec<String>>,
        /// `None` keeps the list already stored.
        services: Option<Vec<String>>,
        apps: Option<Vec<String>>,
    },
    /// Replace the tunnel lists. `None` keeps the TUN bypass list already stored.
    Scope {
        services: Vec<String>,
        apps: Vec<String>,
        bypass_services: Option<Vec<String>>,
        bypass_apps: Option<Vec<String>>,
    },
    /// `"auto"` or an ISO alpha-2 code.
    Exit(String),
}

/// Pure route result. The live server replaces a 200 body with [`status_document`].
#[derive(Debug)]
pub struct Decision {
    pub status: u16,
    pub body: serde_json::Value,
    pub patch: Patch,
}

/// True only for `127.0.0.1` and `::1`.
pub fn ensure_loopback(addr: SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(ip) => ip == Ipv4Addr::LOCALHOST,
        IpAddr::V6(ip) => ip == Ipv6Addr::LOCALHOST,
    }
}

/// Status the parent polls. No key, entrance, cabinet, or account fields.
pub fn status_document(view: &View) -> serde_json::Value {
    let state = if view.session_up {
        view.edge_state.as_deref().unwrap_or("connecting")
    } else {
        "off"
    };
    serde_json::json!({
        "v": 1,
        "state": state,
        "socks": view.socks_running,
        "http": view.http_running,
        "tun": view.tun,
        "tun_mode": view.tun_mode,
        "bypass": view.bypass,
        "services": view.services,
        "apps": view.apps,
        "bypass_services": view.bypass_services,
        "bypass_apps": view.bypass_apps,
        "mode": view.mode,
        "tun_block": view.tun_block,
        "error": view.error,
        "countries": view.countries,
        "exit": view.exit,
    })
}

/// Route one request. `path` may carry a query; there is no trailing-slash alias.
///
/// GET `/v1/status` projects `wish` (the live server overwrites that body).
pub fn dispatch(method: &str, path: &str, body: &str, wish: &Wish) -> Decision {
    let path = path.split_once('?').map(|(p, _)| p).unwrap_or(path);
    // Unknown paths include /subscribers, /account, /cabinet, and /v1/delete.
    if !known_path(path) {
        return reject(404, "not_found");
    }
    match (method, path) {
        ("GET", "/v1/status") => Decision {
            status: 200,
            body: status_document(&view_from_wish(wish)),
            patch: Patch::None,
        },
        ("POST", "/v1/session") => post_session(body),
        ("POST", "/v1/listeners") => post_listeners(body),
        ("POST", "/v1/tun") => post_tun(body, wish),
        ("POST", "/v1/scope") => post_scope(body),
        ("POST", "/v1/exit") => post_exit(body),
        _ => reject(405, "method"),
    }
}

/// Serve the control API until the listener fails. `bind` must be loopback.
pub async fn serve(
    bind: SocketAddr,
    supervisor: Arc<SupervisorSlot>,
    scope: Arc<EgressScope>,
    tun_bin: Option<PathBuf>,
    relay_addrs: Vec<String>,
    wire: WireMode,
) -> std::io::Result<()> {
    if !ensure_loopback(bind) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "control bind must be loopback",
        ));
    }
    let listener = TcpListener::bind(bind).await?;
    info!(%bind, "subscriber-client: control listener ready");
    let tun_bin_set = tun_bin.is_some();
    let mut runtime = Runtime {
        wish: Wish { tun_bin_set, ..Wish::default() },
        socks_task: None,
        http_task: None,
        supervisor,
        scope,
        tun_bin,
        relay_addrs,
        wire,
        tun_child: None,
        tun_stamp: String::new(),
        tun_retry_at: None,
        tun_fault: None,
        tun_ready: false,
        tun_born: None,
    };
    loop {
        let (mut stream, _) = listener.accept().await?;
        if let Err(e) = handle_client(&mut stream, &mut runtime).await {
            debug!(error = %e, "control: connection ended");
        }
    }
}

fn known_path(path: &str) -> bool {
    matches!(path, "/v1/status" | "/v1/session" | "/v1/listeners" | "/v1/tun" | "/v1/scope" | "/v1/exit")
}

fn carry_mode(wish: &Wish) -> &'static str {
    if wish.tun {
        "tun"
    } else if wish.http {
        "http"
    } else {
        "socks"
    }
}

fn tun_block(_wish: &Wish) -> Option<String> {
    None
}

fn view_from_wish(wish: &Wish) -> View {
    View {
        session_up: wish.session_up,
        edge_state: None,
        socks_running: false,
        http_running: false,
        tun: false,
        tun_mode: wish.tun_mode.clone(),
        bypass: wish.bypass.clone(),
        services: wish.services.clone(),
        apps: wish.apps.clone(),
        bypass_services: wish.bypass_services.clone(),
        bypass_apps: wish.bypass_apps.clone(),
        mode: carry_mode(wish).to_owned(),
        tun_block: tun_block(wish),
        error: None,
        countries: Vec::new(),
        exit: wish.exit.clone(),
    }
}

fn post_exit(body: &str) -> Decision {
    let Some(map) = json_object(body) else {
        return reject(400, "bad_json");
    };
    let Some(exit) = map.get("exit").and_then(serde_json::Value::as_str) else {
        return reject(400, "bad_json");
    };
    if !valid_exit(exit) {
        return reject(400, "bad_json");
    }
    Decision { status: 200, body: serde_json::json!({}), patch: Patch::Exit(exit.to_owned()) }
}

fn valid_exit(label: &str) -> bool {
    label == "auto" || (label.len() == 2 && label.bytes().all(|byte| byte.is_ascii_uppercase()))
}

fn exit_preference(label: &str) -> Option<ExitPreference> {
    if label == "auto" {
        return Some(ExitPreference::Auto);
    }
    CountryCode::new(label).ok().map(ExitPreference::Country)
}

fn reject(status: u16, error: &str) -> Decision {
    Decision {
        status,
        body: serde_json::json!({ "error": error }),
        patch: Patch::None,
    }
}

fn json_object(body: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    match serde_json::from_str(body) {
        Ok(serde_json::Value::Object(map)) => Some(map),
        _ => None,
    }
}

fn require_bool(map: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<bool> {
    map.get(key).and_then(serde_json::Value::as_bool)
}

fn lists_changed(patch: &Patch) -> bool {
    match patch {
        Patch::Scope { .. } => true,
        Patch::Tun { services, apps, .. } => services.is_some() || apps.is_some(),
        _ => false,
    }
}

/// `None` when the key is absent. `Some` when it is an array of strings.
fn present_strings(
    map: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<Option<Vec<String>>, ()> {
    match map.get(key) {
        None => Ok(None),
        Some(value) => optional_strings(Some(value)).map(Some).ok_or(()),
    }
}

fn optional_strings(value: Option<&serde_json::Value>) -> Option<Vec<String>> {
    match value {
        None => Some(Vec::new()),
        Some(serde_json::Value::Array(items)) => {
            items.iter().map(|item| item.as_str().map(str::to_owned)).collect()
        }
        Some(_) => None,
    }
}

fn post_session(body: &str) -> Decision {
    let Some(map) = json_object(body) else {
        return reject(400, "bad_json");
    };
    let Some(up) = require_bool(&map, "up") else {
        return reject(400, "bad_json");
    };
    Decision { status: 200, body: serde_json::json!({}), patch: Patch::Session(up) }
}

fn post_listeners(body: &str) -> Decision {
    let Some(map) = json_object(body) else {
        return reject(400, "bad_json");
    };
    let Some(socks) = require_bool(&map, "socks") else {
        return reject(400, "bad_json");
    };
    let Some(http) = require_bool(&map, "http") else {
        return reject(400, "bad_json");
    };
    Decision {
        status: 200,
        body: serde_json::json!({}),
        patch: Patch::Listeners { socks, http },
    }
}

fn post_tun(body: &str, _wish: &Wish) -> Decision {
    let Some(map) = json_object(body) else {
        return reject(400, "bad_json");
    };
    let Some(enabled) = require_bool(&map, "enabled") else {
        return reject(400, "bad_json");
    };
    let mode = match map.get("mode") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(mode)) if mode == "full" || mode == "split" => {
            Some(mode.clone())
        }
        _ => return reject(400, "bad_json"),
    };
    let Ok(bypass) = present_strings(&map, "bypass") else {
        return reject(400, "bad_json");
    };
    let Ok(services) = present_strings(&map, "services") else {
        return reject(400, "bad_json");
    };
    let Ok(apps) = present_strings(&map, "apps") else {
        return reject(400, "bad_json");
    };
    if let Some(list) = &services {
        if EgressScope::from_lists(list, &[]).is_err() {
            return reject(400, "bad_json");
        }
    }
    if let Some(list) = &apps {
        if EgressScope::from_lists(&[], list).is_err() {
            return reject(400, "bad_json");
        }
    }
    Decision {
        status: 200,
        body: serde_json::json!({}),
        patch: Patch::Tun { enabled, mode, bypass, services, apps },
    }
}

fn post_scope(body: &str) -> Decision {
    let Some(map) = json_object(body) else {
        return reject(400, "bad_json");
    };
    let Some(services) = optional_strings(map.get("services")) else {
        return reject(400, "bad_json");
    };
    let Some(apps) = optional_strings(map.get("apps")) else {
        return reject(400, "bad_json");
    };
    if EgressScope::from_lists(&services, &apps).is_err() {
        return reject(400, "bad_json");
    }
    let Ok(bypass_services) = present_strings(&map, "bypass_services") else {
        return reject(400, "bad_json");
    };
    let Ok(bypass_apps) = present_strings(&map, "bypass_apps") else {
        return reject(400, "bad_json");
    };
    if let Some(list) = &bypass_services {
        if EgressScope::from_lists(list, &[]).is_err() {
            return reject(400, "bad_json");
        }
    }
    if let Some(list) = &bypass_apps {
        if EgressScope::from_lists(&[], list).is_err() {
            return reject(400, "bad_json");
        }
    }
    Decision {
        status: 200,
        body: serde_json::json!({}),
        patch: Patch::Scope { services, apps, bypass_services, bypass_apps },
    }
}

fn apply_wish(wish: &mut Wish, patch: &Patch) {
    match patch {
        Patch::None => {}
        Patch::Session(up) => wish.session_up = *up,
        Patch::Listeners { socks, http } => {
            // One carry: SOCKS or HTTP or TUN. Both listener flags true keeps SOCKS.
            let pick_socks = *socks;
            let pick_http = *http && !pick_socks;
            if pick_socks || pick_http {
                wish.socks = pick_socks;
                wish.http = pick_http;
                wish.tun = false;
            } else if wish.tun {
                wish.socks = false;
                wish.http = false;
            } else {
                wish.socks = true;
                wish.http = false;
            }
        }
        // Stored only. Nothing is spawned. Site and app lists, when present,
        // are pushed into the shared scope by the caller after this match.
        // Enabling TUN clears SOCKS and HTTP. Disabling it with nothing else
        // selected falls back to SOCKS.
        Patch::Tun { enabled, mode, bypass, services, apps } => {
            if *enabled {
                wish.tun = true;
                wish.socks = false;
                wish.http = false;
            } else {
                wish.tun = false;
                if !wish.socks && !wish.http {
                    wish.socks = true;
                }
            }
            wish.tun_mode = mode.clone();
            if let Some(bypass) = bypass {
                wish.bypass = bypass.clone();
            }
            if let Some(services) = services {
                wish.services = services.clone();
            }
            if let Some(apps) = apps {
                wish.apps = apps.clone();
            }
        }
        Patch::Scope { services, apps, bypass_services, bypass_apps } => {
            wish.services = services.clone();
            wish.apps = apps.clone();
            if let Some(list) = bypass_services {
                wish.bypass_services = list.clone();
            }
            if let Some(list) = bypass_apps {
                wish.bypass_apps = list.clone();
            }
        }
        Patch::Exit(exit) => wish.exit = exit.clone(),
    }
}

struct Runtime {
    wish: Wish,
    socks_task: Option<JoinHandle<()>>,
    http_task: Option<JoinHandle<()>>,
    supervisor: Arc<SupervisorSlot>,
    scope: Arc<EgressScope>,
    tun_bin: Option<PathBuf>,
    /// Entrance addresses the tun routes around itself. Not a status field.
    relay_addrs: Vec<String>,
    /// The datapath the device speaks: the raw-IP path carries the whole IP
    /// packets, the stack path carries byte streams.
    wire: WireMode,
    tun_child: Option<Child>,
    tun_stamp: String,
    tun_retry_at: Option<Instant>,
    /// Shown when tun is selected and the process is not up. Cleared when it is up.
    tun_fault: Option<String>,
    /// Process is up and the adapter answered. A live process with no adapter is not up.
    tun_ready: bool,
    tun_born: Option<Instant>,
}

impl Drop for Runtime {
    /// Listeners and the tun process are children of this loop.
    fn drop(&mut self) {
        if let Some(task) = self.socks_task.take() {
            task.abort();
        }
        if let Some(task) = self.http_task.take() {
            task.abort();
        }
        stop_tun(&mut self.tun_child);
    }
}

async fn handle_client(stream: &mut TcpStream, rt: &mut Runtime) -> std::io::Result<()> {
    let incoming = match read_incoming(stream).await {
        Ok(incoming) => incoming,
        Err(ReadFail::TooLarge) => {
            return write_response(stream, 413, &serde_json::json!({ "error": "too_large" })).await;
        }
        Err(ReadFail::Malformed) => {
            return write_response(stream, 400, &serde_json::json!({ "error": "bad_json" })).await;
        }
        Err(ReadFail::Io(err)) => return Err(err),
    };
    let Decision { status, body, patch } =
        dispatch(&incoming.method, &incoming.path, &incoming.body, &rt.wish);
    if status == 200 || status == 409 {
        apply_wish(&mut rt.wish, &patch);
        if let Patch::Exit(label) = &patch {
            if let Some(pref) = exit_preference(label) {
                rt.supervisor.current().set_exit(pref);
            }
        }
        if !matches!(patch, Patch::None) {
            if lists_changed(&patch) {
                if let Err(e) = rt.scope.set_lists(&rt.wish.services, &rt.wish.apps) {
                    error!(error = %e, "control: scope rejected");
                }
            }
            sync_listeners(rt);
        }
        if status == 200 {
            sync_tun(rt);
        }
    }
    let body = if status == 200 {
        status_document(&live_view(rt))
    } else {
        body
    };
    write_response(stream, status, &body).await
}

fn live_view(rt: &mut Runtime) -> View {
    let rx = rt.supervisor.current().status();
    let snap = rx.borrow().clone();
    let tun_up = tun_child_alive(&mut rt.tun_child);
    if !tun_up {
        rt.tun_ready = false;
    }
    View {
        session_up: rt.wish.session_up,
        edge_state: Some(edge_label(snap.state).to_owned()),
        socks_running: task_running(&rt.socks_task),
        http_running: task_running(&rt.http_task),
        tun: tun_up && rt.tun_ready,
        tun_mode: rt.wish.tun_mode.clone(),
        bypass: rt.wish.bypass.clone(),
        services: rt.wish.services.clone(),
        apps: rt.wish.apps.clone(),
        bypass_services: rt.wish.bypass_services.clone(),
        bypass_apps: rt.wish.bypass_apps.clone(),
        mode: carry_mode(&rt.wish).to_owned(),
        tun_block: tun_block(&rt.wish),
        error: if rt.wish.tun { rt.tun_fault.clone() } else { None },
        countries: snap.countries,
        exit: snap.exit.requested,
    }
}

fn edge_label(state: EdgeState) -> &'static str {
    match state {
        EdgeState::Connecting => "connecting",
        EdgeState::Connected => "connected",
        EdgeState::Reconnecting => "reconnecting",
        EdgeState::Refused => "refused",
        EdgeState::Fatal => "fatal",
    }
}

fn task_running(task: &Option<JoinHandle<()>>) -> bool {
    task.as_ref().is_some_and(|handle| !handle.is_finished())
}

fn sync_listeners(rt: &mut Runtime) {
    // TUN's chain is this SOCKS listener. The menu still shows one mode.
    let run_socks = rt.wish.session_up && (rt.wish.socks || rt.wish.tun);
    let run_http = rt.wish.session_up && rt.wish.http;
    let socks_supervisor = Arc::clone(&rt.supervisor);
    let socks_scope = Arc::clone(&rt.scope);
    let http_supervisor = Arc::clone(&rt.supervisor);
    let http_scope = Arc::clone(&rt.scope);
    ensure_task(&mut rt.socks_task, run_socks, move || {
        tokio::spawn(async move {
            let bind = SocketAddr::from(([127, 0, 0, 1], 1080));
            if let Err(e) = crate::socks5::serve_slot(bind, socks_supervisor, socks_scope).await {
                error!(error = %e, "subscriber-client: SOCKS5 listener exited");
            }
        })
    });
    ensure_task(&mut rt.http_task, run_http, move || {
        tokio::spawn(async move {
            let bind = SocketAddr::from(([127, 0, 0, 1], 8080));
            if let Err(e) = crate::http_connect::serve(bind, http_supervisor, http_scope).await {
                error!(error = %e, "subscriber-client: HTTP CONNECT listener exited");
            }
        })
    });
}

fn tun_child_alive(slot: &mut Option<Child>) -> bool {
    let Some(child) = slot.as_mut() else {
        return false;
    };
    match child.try_wait() {
        Ok(None) => true,
        _ => {
            let _ = slot.take().map(|mut child| child.wait());
            false
        }
    }
}

fn stop_tun(slot: &mut Option<Child>) {
    if let Some(mut child) = slot.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn tun_stamp(wish: &Wish) -> String {
    format!(
        "{}\n{}\n{}\n{}",
        wish.services.join("\n"),
        wish.apps.join("\n"),
        wish.bypass_services.join("\n"),
        wish.bypass_apps.join("\n"),
    )
}

/// Start or stop `mirage2tun` to match the wish. A status poll retries the
/// start once the edge session is up. A missing binary, a spawn error, or a
/// dead child is stored on the runtime and returned in status. It is not silent.
fn sync_tun(rt: &mut Runtime) {
    if rt.wish.tun && rt.tun_bin.is_none() {
        rt.tun_fault = Some("mirage2tun binary is not set".to_owned());
    }
    // The adapter follows the WISH, not the live session: the tunnel comes up
    // when the connection starts, the way Amnezia raises its adapter, and it
    // stays up when the session drops. While the session is not live the
    // chain refuses every packet, so the traffic is blocked — the kill
    // switch — and nothing leaks outside.
    let want = rt.wish.session_up && rt.wish.tun;
    if !want {
        stop_tun(&mut rt.tun_child);
        rt.tun_stamp.clear();
        rt.tun_retry_at = None;
        rt.tun_ready = false;
        rt.tun_born = None;
        if !rt.wish.tun {
            rt.tun_fault = None;
        }
        return;
    }
    let Some(bin) = rt.tun_bin.clone() else {
        return;
    };
    let stamp = tun_stamp(&rt.wish);
    match observe_tun(&mut rt.tun_child) {
        Some(None) if rt.tun_stamp == stamp => {
            note_adapter(rt);
            return;
        }
        Some(None) => {
            stop_tun(&mut rt.tun_child);
            rt.tun_ready = false;
            rt.tun_born = None;
        }
        Some(Some(code)) => {
            rt.tun_fault = Some(tun_exit_fault(code));
            rt.tun_stamp.clear();
            rt.tun_ready = false;
            rt.tun_born = None;
            rt.tun_retry_at = Some(Instant::now());
            return;
        }
        None if !rt.tun_stamp.is_empty() => {
            rt.tun_fault = Some("mirage2tun exited".to_owned());
            rt.tun_stamp.clear();
            rt.tun_ready = false;
            rt.tun_born = None;
            rt.tun_retry_at = Some(Instant::now());
            return;
        }
        None => {}
    }
    if rt.tun_retry_at.is_some_and(|at| at.elapsed() < Duration::from_secs(3)) {
        return;
    }
    let snap = rt.supervisor.current().status().borrow().clone();
    if snap.state != EdgeState::Connected || !task_running(&rt.socks_task) {
        if matches!(snap.state, EdgeState::Refused | EdgeState::Fatal) {
            rt.tun_fault = Some(format!("mirage2tun did not start: dial {}", edge_label(snap.state)));
        }
        return;
    }
    let mode = if rt.wish.services.is_empty() && rt.wish.apps.is_empty() {
        TunMode::Full
    } else {
        TunMode::Split
    };
    let config = std::env::temp_dir().join("mirage-gui-tun.json");
    if let Err(e) = tun_mode::write_config(
        &config,
        SocketAddr::from(([127, 0, 0, 1], 1080)),
        &rt.wish.bypass,
        mode,
        rt.wire,
        &rt.wish.services,
        &rt.wish.apps,
        &rt.wish.bypass_services,
        &rt.wish.bypass_apps,
        &rt.relay_addrs,
    ) {
        error!(error = %e, "control: tun config was not written");
        rt.tun_fault = Some("mirage2tun config was not written".to_owned());
        rt.tun_retry_at = Some(Instant::now());
        return;
    }
    match tun_mode::spawn(&bin, &config) {
        Ok(child) => {
            info!(pid = child.id(), "mirage2tun started");
            rt.tun_child = Some(child);
            rt.tun_stamp = stamp;
            rt.tun_retry_at = None;
            rt.tun_fault = None;
            rt.tun_ready = !cfg!(windows);
            rt.tun_born = Some(Instant::now());
        }
        Err(e) => {
            error!(error = %e, "control: mirage2tun did not start");
            rt.tun_fault = Some(public_fault(&format!("mirage2tun did not start: {e}")));
            rt.tun_retry_at = Some(Instant::now());
        }
    }
}

/// `None` — no child. `Some(None)` — still running. `Some(Some(code))` — reaped.
fn observe_tun(slot: &mut Option<Child>) -> Option<Option<i32>> {
    let child = slot.as_mut()?;
    match child.try_wait() {
        Ok(None) => Some(None),
        Ok(Some(status)) => {
            let code = status.code().unwrap_or(-1);
            let _ = slot.take();
            Some(Some(code))
        }
        Err(_) => {
            let _ = slot.take();
            Some(Some(-1))
        }
    }
}

/// A live process is not enough. On Windows the adapter has its own status.
fn note_adapter(rt: &mut Runtime) {
    #[cfg(not(windows))]
    {
        rt.tun_ready = true;
        rt.tun_fault = None;
    }
    #[cfg(windows)]
    {
        let born = rt.tun_born.unwrap_or_else(Instant::now);
        match probe_adapter_alive() {
            Some(true) => {
                rt.tun_ready = true;
                rt.tun_fault = None;
            }
            other => {
                rt.tun_ready = false;
                if born.elapsed() >= Duration::from_secs(4) {
                    let why = tun_log_fault_line().unwrap_or_else(|| {
                        if other == Some(false) {
                            "mirage2tun did not take the adapter".to_owned()
                        } else {
                            "mirage2tun control did not answer".to_owned()
                        }
                    });
                    rt.tun_fault = Some(public_fault(&why));
                }
            }
        }
    }
}

#[cfg(windows)]
fn probe_adapter_alive() -> Option<bool> {
    use std::io::{Read, Write};
    let addr: std::net::SocketAddr = "127.0.0.1:19485".parse().ok()?;
    let mut stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)).ok()?;
    stream.set_read_timeout(Some(Duration::from_millis(400))).ok()?;
    stream.set_write_timeout(Some(Duration::from_millis(200))).ok()?;
    stream
        .write_all(b"GET /status HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("");
    let value: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    value.get("adapter_alive").and_then(serde_json::Value::as_bool)
}

#[cfg(windows)]
fn tun_log_fault_line() -> Option<String> {
    let bytes = std::fs::read(tun_log_path()).ok()?;
    let start = bytes.len().saturating_sub(8192);
    let text = String::from_utf8_lossy(&bytes[start..]);
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| {
            let lower = line.to_ascii_lowercase();
            !line.is_empty()
                && (lower.contains("fail")
                    || lower.contains("error")
                    || lower.contains("denied")
                    || lower.contains("wintun"))
        })
        .map(str::to_owned)
}

fn tun_log_path() -> std::path::PathBuf {
    std::env::temp_dir().join("mirage-tun.log")
}

fn tun_log_tail() -> Option<String> {
    let bytes = std::fs::read(tun_log_path()).ok()?;
    let start = bytes.len().saturating_sub(4096);
    let text = String::from_utf8_lossy(&bytes[start..]);
    text.lines().rev().map(str::trim).find(|line| !line.is_empty()).map(str::to_owned)
}

fn tun_exit_fault(code: i32) -> String {
    // STATUS_DLL_NOT_FOUND. The loader never reached main, so the log is empty.
    if code == -1_073_741_515 {
        return "mirage2tun exited: wintun.dll was not next to the process".to_owned();
    }
    if let Some(line) = tun_log_tail() {
        return public_fault(&format!("mirage2tun exited {code}: {line}"));
    }
    format!("mirage2tun exited {code}")
}

/// Status text. Drops addresses and words the status document must not carry.
fn public_fault(raw: &str) -> String {
    let mut kept = String::new();
    for token in raw.split_whitespace() {
        let lower = token.to_ascii_lowercase();
        if ["private", "entrance", "cabinet", "pubkey", "artefact"].iter().any(|needle| lower.contains(needle))
        {
            continue;
        }
        let host = token.trim_matches(|c: char| !c.is_ascii_hexdigit() && c != '.' && c != ':');
        if host.parse::<IpAddr>().is_ok() {
            continue;
        }
        if !kept.is_empty() {
            kept.push(' ');
        }
        kept.push_str(token);
        if kept.len() > 180 {
            break;
        }
    }
    if kept.is_empty() { "mirage2tun failed".to_owned() } else { kept }
}

fn ensure_task(slot: &mut Option<JoinHandle<()>>, should_run: bool, spawn: impl FnOnce() -> JoinHandle<()>) {
    if !should_run {
        if let Some(task) = slot.take() {
            task.abort();
        }
        return;
    }
    if slot.as_ref().is_some_and(|task| !task.is_finished()) {
        return;
    }
    if let Some(task) = slot.take() {
        task.abort();
    }
    *slot = Some(spawn());
}

enum ReadFail {
    Malformed,
    TooLarge,
    Io(std::io::Error),
}

struct Incoming {
    method: String,
    path: String,
    body: String,
}

struct Header {
    method: String,
    path: String,
    content_length: Option<usize>,
}

async fn read_incoming(stream: &mut TcpStream) -> Result<Incoming, ReadFail> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    let header_end = loop {
        if let Some(end) = find_header_end(&buf) {
            break end;
        }
        if buf.len() >= READ_CAP {
            return Err(ReadFail::TooLarge);
        }
        let room = (READ_CAP - buf.len()).min(tmp.len());
        let n = stream.read(&mut tmp[..room]).await.map_err(ReadFail::Io)?;
        if n == 0 {
            return Err(ReadFail::Malformed);
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let header = parse_header(&buf[..header_end])?;
    let body_at = header_end + 4;
    let Some(len) = header.content_length else {
        return Ok(Incoming { method: header.method, path: header.path, body: String::new() });
    };
    if len > READ_CAP {
        return Err(ReadFail::TooLarge);
    }
    let Some(end) = body_at.checked_add(len) else {
        return Err(ReadFail::TooLarge);
    };
    while buf.len() < end {
        let need = end - buf.len();
        let want = need.min(tmp.len());
        let n = stream.read(&mut tmp[..want]).await.map_err(ReadFail::Io)?;
        if n == 0 {
            return Err(ReadFail::Malformed);
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = std::str::from_utf8(&buf[body_at..end])
        .map_err(|_| ReadFail::Malformed)?
        .to_owned();
    Ok(Incoming { method: header.method, path: header.path, body })
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_header(bytes: &[u8]) -> Result<Header, ReadFail> {
    let text = std::str::from_utf8(bytes).map_err(|_| ReadFail::Malformed)?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("");
    if method.is_empty() || path.is_empty() || !version.starts_with("HTTP/") || parts.next().is_some()
    {
        return Err(ReadFail::Malformed);
    }
    let mut content_length = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(ReadFail::Malformed);
        };
        if !name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        if content_length.is_some() {
            return Err(ReadFail::Malformed);
        }
        let len = value.trim().parse::<usize>().map_err(|_| ReadFail::Malformed)?;
        content_length = Some(len);
    }
    Ok(Header {
        method: method.to_owned(),
        path: path.to_owned(),
        content_length,
    })
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        _ => "Error",
    }
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    body: &serde_json::Value,
) -> std::io::Result<()> {
    let payload = serde_json::to_vec(body).unwrap_or_else(|_| br#"{"error":"bad_json"}"#.to_vec());
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reason_phrase(status),
        payload.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&payload).await?;
    stream.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn keys(value: &serde_json::Value) -> BTreeSet<&str> {
        value.as_object().expect("object").keys().map(String::as_str).collect()
    }

    fn expect_keys() -> BTreeSet<&'static str> {
        [
            "v", "state", "socks", "http", "tun", "tun_mode", "bypass", "services", "apps",
            "bypass_services", "bypass_apps", "mode", "tun_block", "error", "countries", "exit",
        ]
            .into_iter()
            .collect()
    }

    fn assert_no_secret(value: &serde_json::Value) {
        let text = serde_json::to_string(value).expect("json");
        for needle in ["private", "entrance", "cabinet", "pubkey", "artefact"] {
            assert!(!text.contains(needle), "{needle} in {text}");
        }
    }

    #[test]
    fn status_document_shape_has_no_secret_fields() {
        let off = View {
            session_up: false,
            edge_state: Some("connected".to_owned()),
            socks_running: false,
            http_running: false,
            tun: false,
            tun_mode: None,
            bypass: Vec::new(),
            services: Vec::new(),
            apps: Vec::new(),
            bypass_services: Vec::new(),
            bypass_apps: Vec::new(),
            mode: "socks".to_owned(),
            tun_block: None,
            error: None,
            countries: Vec::new(),
            exit: "auto".to_owned(),
        };
        let doc = status_document(&off);
        assert_eq!(keys(&doc), expect_keys());
        assert_eq!(
            doc,
            serde_json::json!({
                "v": 1,
                "state": "off",
                "socks": false,
                "http": false,
                "tun": false,
                "tun_mode": null,
                "bypass": [],
                "services": [],
                "apps": [],
                "bypass_services": [],
                "bypass_apps": [],
                "mode": "socks",
                "tun_block": null,
                "error": null,
                "countries": [],
                "exit": "auto"
            })
        );
        assert_no_secret(&doc);

        let connecting = status_document(&View {
            session_up: true,
            edge_state: None,
            socks_running: true,
            http_running: false,
            tun: false,
            tun_mode: None,
            bypass: vec!["lan".to_owned()],
            services: vec!["dns.example".to_owned()],
            apps: Vec::new(),
            bypass_services: Vec::new(),
            bypass_apps: Vec::new(),
            mode: "socks".to_owned(),
            tun_block: None,
            error: None,
            countries: vec!["NL".to_owned()],
            exit: "auto".to_owned(),
        });
        assert_eq!(keys(&connecting), expect_keys());
        assert_eq!(connecting["state"], "connecting");
        assert_eq!(connecting["socks"], true);
        assert_no_secret(&connecting);

        let connected = status_document(&View {
            session_up: true,
            edge_state: Some("connected".to_owned()),
            socks_running: false,
            http_running: true,
            tun: true,
            tun_mode: Some("split".to_owned()),
            bypass: Vec::new(),
            services: Vec::new(),
            apps: Vec::new(),
            bypass_services: Vec::new(),
            bypass_apps: Vec::new(),
            mode: "tun".to_owned(),
            tun_block: Some("no_tun_process".to_owned()),
            error: Some("mirage2tun binary is not set".to_owned()),
            countries: Vec::new(),
            exit: "DE".to_owned(),
        });
        assert_eq!(connected["state"], "connected");
        assert_eq!(connected["exit"], "DE");
        assert_eq!(connected["http"], true);
        assert_eq!(connected["tun"], true);
        assert_eq!(connected["tun_mode"], "split");
        assert_eq!(connected["error"], "mirage2tun binary is not set");
        assert_no_secret(&connected);
    }

    #[test]
    fn listeners_patch_flips_wish() {
        let mut wish = Wish::default();
        assert!(wish.socks);
        assert!(!wish.http);
        let decision = dispatch(
            "POST",
            "/v1/listeners",
            r#"{"socks":false,"http":true}"#,
            &wish,
        );
        assert_eq!(decision.status, 200);
        apply_wish(&mut wish, &decision.patch);
        assert!(!wish.socks);
        assert!(wish.http);
        assert!(!wish.session_up);
        assert!(!wish.tun);
    }

    #[test]
    fn tun_without_bin_stores_the_switch() {
        let mut wish = Wish::default();
        assert!(!wish.tun_bin_set);
        let decision = dispatch(
            "POST",
            "/v1/tun",
            r#"{"enabled":true,"mode":"split","bypass":["lan"],"services":["dns.example"]}"#,
            &wish,
        );
        assert_eq!(decision.status, 200);
        let Patch::Tun { enabled, mode, bypass, services, apps } = &decision.patch else {
            panic!("expected tun patch");
        };
        assert!(*enabled);
        assert_eq!(mode.as_deref(), Some("split"));
        assert_eq!(bypass, &Some(vec!["lan".to_owned()]));
        assert_eq!(services, &Some(vec!["dns.example".to_owned()]));
        assert!(apps.is_none());
        apply_wish(&mut wish, &decision.patch);
        assert!(wish.tun);
        assert!(!wish.socks);
        assert!(!wish.http);
        assert_eq!(wish.bypass, vec!["lan".to_owned()]);
        assert_eq!(wish.services, vec!["dns.example".to_owned()]);
        assert!(wish.apps.is_empty());
        assert_eq!(wish.tun_mode.as_deref(), Some("split"));
    }

    #[test]
    fn one_carry_at_a_time() {
        let mut wish = Wish::default();
        let http = dispatch("POST", "/v1/listeners", r#"{"socks":false,"http":true}"#, &wish);
        apply_wish(&mut wish, &http.patch);
        assert!(wish.http && !wish.socks && !wish.tun);
        let tun = dispatch("POST", "/v1/tun", r#"{"enabled":true}"#, &wish);
        apply_wish(&mut wish, &tun.patch);
        assert!(wish.tun && !wish.socks && !wish.http);
        let socks = dispatch("POST", "/v1/listeners", r#"{"socks":true,"http":false}"#, &wish);
        apply_wish(&mut wish, &socks.patch);
        assert!(wish.socks && !wish.http && !wish.tun);
        let both = dispatch("POST", "/v1/listeners", r#"{"socks":true,"http":true}"#, &wish);
        apply_wish(&mut wish, &both.patch);
        assert!(wish.socks && !wish.http && !wish.tun);
    }

    #[test]
    fn hidden_routes_are_404() {
        let wish = Wish::default();
        for (method, path) in [
            ("GET", "/subscribers"),
            ("POST", "/v1/account"),
            ("GET", "/nope"),
            ("GET", "/v1/subscribers"),
            ("POST", "/account"),
            ("GET", "/cabinet"),
            ("POST", "/v1/cabinet"),
            ("POST", "/v1/delete"),
            ("GET", "/v1/status/"),
        ] {
            let decision = dispatch(method, path, "", &wish);
            assert_eq!(decision.status, 404, "{method} {path}");
            assert_eq!(decision.body["error"], "not_found");
            assert!(matches!(decision.patch, Patch::None));
        }
    }

    #[test]
    fn session_without_up_is_400() {
        let wish = Wish::default();
        for body in ["{}", "", "null", r#"{"down":true}"#, r#"{"up":"yes"}"#] {
            let decision = dispatch("POST", "/v1/session", body, &wish);
            assert_eq!(decision.status, 400, "{body}");
            assert_eq!(decision.body["error"], "bad_json");
            assert!(matches!(decision.patch, Patch::None));
        }
    }

    #[test]
    fn exit_patch_accepts_auto_or_country() {
        let wish = Wish::default();
        let auto = dispatch("POST", "/v1/exit", r#"{"exit":"auto"}"#, &wish);
        assert_eq!(auto.status, 200);
        assert!(matches!(auto.patch, Patch::Exit(ref label) if label == "auto"));
        let country = dispatch("POST", "/v1/exit", r#"{"exit":"DE"}"#, &wish);
        assert_eq!(country.status, 200);
        assert!(matches!(country.patch, Patch::Exit(ref label) if label == "DE"));
        for body in [r#"{"exit":"de"}"#, r#"{"exit":"DEU"}"#, r#"{}"#, r#"{"exit":""}"#] {
            let decision = dispatch("POST", "/v1/exit", body, &wish);
            assert_eq!(decision.status, 400, "{body}");
            assert!(matches!(decision.patch, Patch::None));
        }
    }

    #[test]
    fn loopback_bind_only() {
        let ok: SocketAddr = "127.0.0.1:19421".parse().expect("addr");
        assert!(ensure_loopback(ok));
        let v6: SocketAddr = "[::1]:19421".parse().expect("addr");
        assert!(ensure_loopback(v6));
        let any: SocketAddr = "0.0.0.0:1".parse().expect("addr");
        assert!(!ensure_loopback(any));
        let public: SocketAddr = "8.8.8.8:1".parse().expect("addr");
        assert!(!ensure_loopback(public));
    }
}
