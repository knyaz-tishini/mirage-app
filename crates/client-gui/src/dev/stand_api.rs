//! Standalone control API — a loopback HTTP server that drives the standalone
//! install with no window at all. Dev-tools only.
//!
//! The install is a two-step, human decision: the box is asked what it could
//! publish, the person looks at the proposal, and the second step brings the
//! server up with the faces that were chosen. The window is the usual way to
//! do that; this API is the same two steps over HTTP, so an install on a box
//! can be driven from a script or an agent with no desktop session.
//!
//!   GET  /health        → {"ok":true}
//!   GET  /stand         → the stand as it stands
//!   POST /stand/ask     → step one: ask the box what it could publish
//!   POST /stand/install → step two: bring it up with the faces that were chosen
//!   POST /stand/remove  → take the container down
//!   GET  /stand/status  → what the running job is doing
//!   POST /stand/chain   → build a chain of boxes: install them, then wire them
//!                         together, and hand back the edge box's link
//!
//! The listener binds `127.0.0.1` only: this drives a box over SSH, it must
//! not become a way into this machine. The password arrives in the POST body,
//! goes straight into the stand, and is never echoed back or logged.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::{json, Value};

use crate::oasis::chain_install;
use crate::oasis::provision::ProposedFace;
use crate::oasis::stand::{split_host, Proto, Stand};

/// Port the control API listens on when nothing else is asked for.
pub const DEFAULT_PORT: u16 = 17500;

/// Environment variable that turns the headless mode on and picks the port.
pub const PORT_ENV: &str = "MIRAGE_STAND_API_PORT";

/// Command line flag that turns the headless mode on.
pub const HEADLESS_FLAG: &str = "--headless-stand";

/// Largest request head the API reads.
const MAX_HEAD: usize = 16 * 1024;
/// Largest request body the API reads.
const MAX_BODY: usize = 64 * 1024;
/// How long a client may take to send its request.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// The stand the API drives, plus the last proposal the box made.
///
/// The bus hands the proposal over exactly once — the stand takes it and
/// spreads it over its own protocol list — so a copy is kept here: the API
/// has to be able to answer "what did the box offer?" after that.
struct Api {
    stand: Mutex<Stand>,
    offered: Mutex<Vec<ProposedFace>>,
}

impl Api {
    fn new() -> Self {
        Self {
            stand: Mutex::new(Stand::load()),
            offered: Mutex::new(Vec::new()),
        }
    }

    fn lock_stand(&self) -> MutexGuard<'_, Stand> {
        self.stand.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_offered(&self) -> MutexGuard<'_, Vec<ProposedFace>> {
        self.offered.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Pull the bus into the stand and keep a copy of the proposal.
    ///
    /// The window pulls the bus every frame, so the proposal is in the stand
    /// before the second press of "install". The API has no frames, so it
    /// pulls the bus itself before every step.
    ///
    /// The kept copy is what the status reports: the bus hands the proposal
    /// over exactly once, and the answer to "what did the box offer" must
    /// not depend on who asked first.
    fn sync_bus(&self, stand: &mut Stand) -> Vec<ProposedFace> {
        let from_bus = {
            let guard = stand.bus.faces.lock().unwrap_or_else(PoisonError::into_inner);
            (*guard).clone().unwrap_or_default()
        };
        if !from_bus.is_empty() {
            *self.lock_offered() = from_bus;
        }
        stand.pull_bus();
        self.lock_offered().clone()
    }
}

/// Start the control API on `127.0.0.1:port` and return the port it took.
///
/// The port comes back before the accept loop starts, so the caller can log
/// the address that works even when the port was left to the system.
pub fn serve(port: u16) -> std::io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let bound = listener.local_addr().map(|addr| addr.port()).unwrap_or(port);
    let api = Arc::new(Api::new());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let api = Arc::clone(&api);
            std::thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
                let _ = stream.set_write_timeout(Some(READ_TIMEOUT));
                handle(&api, &mut stream);
            });
        }
    });
    Ok(bound)
}

/// The port the headless mode should use, or `None` when the mode is off.
///
/// The mode is on when `--headless-stand` is on the command line or when
/// `MIRAGE_STAND_API_PORT` is set. The flag may carry the port itself, as
/// the next argument or after an `=`.
pub fn headless_port(args: impl Iterator<Item = String>) -> Option<u16> {
    let args: Vec<String> = args.collect();
    let prefix = format!("{}=", HEADLESS_FLAG);
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].clone();
        index += 1;
        if arg == HEADLESS_FLAG {
            // The port may be the argument right after the flag.
            if let Some(next) = args.get(index) {
                if let Some(port) = port_of(next) {
                    return Some(port);
                }
            }
            return Some(DEFAULT_PORT);
        }
        if let Some(value) = arg.strip_prefix(&prefix) {
            return Some(port_of(value).unwrap_or(DEFAULT_PORT));
        }
    }
    // The environment variable turns the mode on as well, so a service
    // manager does not have to rewrite the command line.
    match std::env::var(PORT_ENV) {
        Ok(raw) => Some(port_of(raw.trim()).unwrap_or(DEFAULT_PORT)),
        Err(_) => None,
    }
}

/// A port from text, if the text is one.
fn port_of(raw: &str) -> Option<u16> {
    match raw.trim().parse::<u16>() {
        Ok(port) if port > 0 => Some(port),
        _ => None,
    }
}

/// What the API answers.
#[derive(Clone, Copy)]
enum Route {
    Health,
    Stand,
    Ask,
    Install,
    Remove,
    Status,
    Chain,
}

fn route(path: &str) -> Option<Route> {
    match path {
        "/health" => Some(Route::Health),
        "/stand" => Some(Route::Stand),
        "/stand/ask" => Some(Route::Ask),
        "/stand/install" => Some(Route::Install),
        "/stand/remove" => Some(Route::Remove),
        "/stand/status" => Some(Route::Status),
        "/stand/chain" => Some(Route::Chain),
        _ => None,
    }
}

/// One request, read whole.
struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

/// Answer one request.
fn handle(api: &Api, stream: &mut TcpStream) {
    let request = match read_request(stream) {
        Ok(request) => request,
        Err(err) => {
            tracing::warn!("[stand-api] {err}");
            respond(stream, 400, &error_body(&err));
            return;
        }
    };
    // The body carries the password, so only the line itself is logged.
    tracing::info!("[stand-api] {} {}", request.method, request.path);

    let Some(route) = route(&request.path) else {
        respond(stream, 404, &error_body("not found"));
        return;
    };
    let wants_post = matches!(
        route,
        Route::Ask | Route::Install | Route::Remove | Route::Chain
    );
    if wants_post != (request.method == "POST") {
        respond(stream, 405, &error_body("method not allowed"));
        return;
    }

    match route {
        Route::Health => respond(stream, 200, &json!({"ok": true}).to_string()),
        Route::Stand => respond(stream, 200, &stand_view(api)),
        Route::Status => respond(stream, 200, &status_view(api)),
        Route::Ask => match credentials(&request.body, true) {
            Ok(creds) => {
                let (code, body) = install(api, &creds, true);
                respond(stream, code, &body);
            }
            Err(err) => respond(stream, 400, &error_body(&err)),
        },
        Route::Install => match credentials(&request.body, true) {
            Ok(creds) => {
                let (code, body) = install(api, &creds, false);
                respond(stream, code, &body);
            }
            Err(err) => respond(stream, 400, &error_body(&err)),
        },
        Route::Remove => match credentials(&request.body, false) {
            Ok(creds) => {
                let (code, body) = remove(api, &creds);
                respond(stream, code, &body);
            }
            Err(err) => respond(stream, 400, &error_body(&err)),
        },
        Route::Chain => match chain_request(&request.body) {
            Ok(chain) => {
                let (code, body) = start_chain(api, chain);
                respond(stream, code, &body);
            }
            Err(err) => respond(stream, 400, &error_body(&err)),
        },
    }
}

/// Read one request: the head first, then the body the head announces.
fn read_request(stream: &mut TcpStream) -> Result<Request, String> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = find_head_end(&buf) {
            break pos;
        }
        let read = stream.read(&mut chunk).map_err(|err| format!("read: {err}"))?;
        if read == 0 {
            return Err("no request head".to_owned());
        }
        buf.extend_from_slice(&chunk[..read]);
        if buf.len() > MAX_HEAD + MAX_BODY {
            return Err("request too large".to_owned());
        }
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_ascii_uppercase();
    let target = parts.next().unwrap_or_default().to_owned();
    if method.is_empty() || target.is_empty() {
        return Err("bad request line".to_owned());
    }
    let path = target.split('?').next().unwrap_or_default().to_owned();

    let mut length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            length = value
                .trim()
                .parse::<usize>()
                .map_err(|_| "bad content-length".to_owned())?;
        }
    }
    if length > MAX_BODY {
        return Err("body too large".to_owned());
    }

    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).map_err(|err| format!("read body: {err}"))?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    if body.len() < length {
        return Err("body shorter than announced".to_owned());
    }
    body.truncate(length);
    Ok(Request { method, path, body })
}

/// Where the head ends, if it has ended.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Write the answer: JSON, one request, close.
fn respond(stream: &mut TcpStream, code: u16, body: &str) {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn error_body(message: &str) -> String {
    json!({"ok": false, "error": message}).to_string()
}

/// What a POST body carries: where to go and how to get in.
struct Credentials {
    host: String,
    user: String,
    password: String,
}

/// Read the credentials out of a request body.
///
/// The password is only read where it is needed: the remove step is key-only
/// and has no password at all.
fn credentials(body: &[u8], with_password: bool) -> Result<Credentials, String> {
    let text = std::str::from_utf8(body).map_err(|_| "body is not text".to_owned())?;
    if text.trim().is_empty() {
        return Err("empty body".to_owned());
    }
    let value: Value = serde_json::from_str(text).map_err(|err| format!("bad json: {err}"))?;
    let host = value
        .get("host")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    check_host(&host)?;
    let user = value
        .get("user")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    if user.is_empty() {
        return Err("user is required".to_owned());
    }
    let password = if with_password {
        value
            .get("password")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    } else {
        String::new()
    };
    Ok(Credentials {
        host,
        user,
        password,
    })
}

/// Check the host before it goes anywhere near an SSH session.
///
/// A trailing `:digits` is the port; everything else means the default SSH
/// port, which is what the stand does with the same text.
fn check_host(host: &str) -> Result<(), String> {
    if host.is_empty() {
        return Err("host is required".to_owned());
    }
    if host.chars().any(char::is_whitespace) {
        return Err("host must not contain spaces".to_owned());
    }
    if let Some((_, port)) = host.rsplit_once(':') {
        if port.is_empty() {
            return Err("host has no port".to_owned());
        }
        if !port.chars().all(|c| c.is_ascii_digit()) {
            return Err("host port is not a number".to_owned());
        }
        if port.parse::<u16>().map_or(true, |port| port == 0) {
            return Err("host port is out of range".to_owned());
        }
    }
    Ok(())
}

/// Run one install step and answer with what happened.
fn install(api: &Api, creds: &Credentials, ask_faces: bool) -> (u16, String) {
    let mut stand = api.lock_stand();
    // The proposal has to be in the stand before the install step reads the
    // protocol list: the box answered, and that answer is the face list.
    api.sync_bus(&mut stand);
    if stand.bus.running.load(Ordering::Relaxed) {
        return (409, error_body("busy"));
    }
    stand.host = creds.host.clone();
    stand.user = creds.user.clone();
    // The stand hands the password to the worker and forgets it: it is never
    // written down and never comes back out.
    stand.pass = creds.password.clone();
    stand.install_with(ask_faces);
    if !stand.bus.running.load(Ordering::Relaxed) {
        // The stand refused to start and said why in its status line.
        let why = stand.status();
        let error = if why.is_empty() {
            "the install did not start".to_owned()
        } else {
            why
        };
        return (400, error_body(&error));
    }
    let mode = if ask_faces { "ask" } else { "install" };
    (
        200,
        json!({"ok": true, "started": true, "mode": mode}).to_string(),
    )
}

/// Take the container down and answer with what happened.
/// One box of a chain, as the request names it.
struct ChainBoxBody {
    host: String,
    port: u16,
    user: String,
    password: String,
}

/// A chain to build, as the request names it.
struct ChainBody {
    chain_id: String,
    boxes: Vec<ChainBoxBody>,
}

/// Read the chain out of a request body.
///
/// The password of every box is read here and goes straight into the worker's
/// memory: it is never written down and never comes back out.
fn chain_request(body: &[u8]) -> Result<ChainBody, String> {
    let text = std::str::from_utf8(body).map_err(|_| "body is not text".to_owned())?;
    let value: Value = serde_json::from_str(text).map_err(|err| format!("bad json: {err}"))?;
    let chain_id = value
        .get("chain_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .unwrap_or("stand")
        .to_owned();
    let raw = value
        .get("boxes")
        .and_then(Value::as_array)
        .ok_or("boxes is required")?;
    if raw.is_empty() {
        return Err("a chain needs at least one box".to_owned());
    }
    let mut boxes = Vec::with_capacity(raw.len());
    for item in raw {
        let host = item
            .get("host")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
        check_host(&host)?;
        let (host, port) = split_host(&host).ok_or("host is required")?;
        let user = item
            .get("user")
            .and_then(Value::as_str)
            .unwrap_or("root")
            .trim()
            .to_owned();
        let password = item
            .get("password")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        boxes.push(ChainBoxBody {
            host,
            port,
            user,
            password,
        });
    }
    Ok(ChainBody { chain_id, boxes })
}

/// Build the chain, and answer with what the build is doing.
///
/// The build is long — every box is installed over SSH — so it runs in a
/// worker and the answer comes through `/stand/status`, exactly like a
/// single box's install does.
fn start_chain(api: &Api, body: ChainBody) -> (u16, String) {
    let mut stand = api.lock_stand();
    api.sync_bus(&mut stand);
    if stand.bus.running.load(Ordering::Relaxed) {
        return (409, error_body("busy"));
    }
    let base_env = stand.server_env();
    let bus = Arc::clone(&stand.bus);
    let chain_id = body.chain_id.clone();
    bus.running.store(true, Ordering::Relaxed);
    std::thread::spawn(move || {
        let boxes: Vec<chain_install::ChainRequest> = body
            .boxes
            .iter()
            .map(|box_| chain_install::ChainRequest {
                host: box_.host.clone(),
                port: box_.port,
                user: box_.user.clone(),
                password: box_.password.clone(),
            })
            .collect();
        let outcome = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime.block_on(chain_install::install_chain(
                &chain_id,
                &boxes,
                &base_env,
                &bus,
            )),
            Err(err) => Err(format!("runtime: {err}")),
        };
        match outcome {
            Ok(outcome) => {
                *bus.link.lock().unwrap_or_else(|p| p.into_inner()) = Some(outcome.link.clone());
                // The first box is the edge — the customer-facing one — and the
                // exit is the chain's LAST HOP, not the last box of the chain.
                let boxes: Vec<String> = outcome
                    .boxes
                    .iter()
                    .map(|box_| format!("{} {}", box_.host, box_.role.as_env()))
                    .collect();
                *bus.text.lock().unwrap_or_else(|p| p.into_inner()) =
                    format!("chain {chain_id}: {}", boxes.join(", "));
            }
            Err(err) => *bus.text.lock().unwrap_or_else(|p| p.into_inner()) = err,
        }
        bus.running.store(false, Ordering::Relaxed);
    });
    (
        200,
        json!({"ok": true, "started": true, "chain_id": body.chain_id}).to_string(),
    )
}

fn remove(api: &Api, creds: &Credentials) -> (u16, String) {
    let mut stand = api.lock_stand();
    api.sync_bus(&mut stand);
    if stand.bus.running.load(Ordering::Relaxed) {
        return (409, error_body("busy"));
    }
    stand.host = creds.host.clone();
    stand.user = creds.user.clone();
    stand.remove_remote();
    if !stand.bus.running.load(Ordering::Relaxed) {
        let why = stand.status();
        let error = if why.is_empty() {
            "the removal did not start".to_owned()
        } else {
            why
        };
        return (400, error_body(&error));
    }
    (
        200,
        json!({"ok": true, "started": true, "mode": "remove"}).to_string(),
    )
}

/// The stand as it stands. The password is not in it and never comes out.
fn stand_view(api: &Api) -> String {
    let mut stand = api.lock_stand();
    api.sync_bus(&mut stand);
    let protocols: Vec<Value> = stand.protocols.iter().map(protocol_view).collect();
    json!({
        "ok": true,
        "host": stand.host,
        "user": stand.user,
        "stack": stand.stack,
        "protocols": protocols,
        "link": stand.link,
        "status": stand.status(),
        "faces_from_box": stand.faces_from_box,
    })
    .to_string()
}

/// What the running job is doing right now.
fn status_view(api: &Api) -> String {
    let mut stand = api.lock_stand();
    let offered = api.sync_bus(&mut stand);
    let running = stand.bus.running.load(Ordering::Relaxed);
    let status = stand.status();
    let link = stand.link.clone();
    let faces: Vec<Value> = offered.iter().map(face_view).collect();
    json!({
        "ok": true,
        "running": running,
        "status": status,
        "link": link,
        "faces": faces,
        "has_faces": !faces.is_empty(),
    })
    .to_string()
}

fn protocol_view(proto: &Proto) -> Value {
    json!({
        "id": proto.id,
        "on": proto.on,
        "port": proto.port,
        "mtu": proto.mtu,
        "mask": proto.mask,
        "why": proto.why,
    })
}

fn face_view(face: &ProposedFace) -> Value {
    json!({
        "port": face.port,
        "transport": face.transport,
        "mask": face.mask,
        "why": face.why,
    })
}
