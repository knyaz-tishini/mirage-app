//! GUI↔engine link.
//!
//! Desktop parent of one `mirage-subscriber-client` child. Loopback control
//! at `http://127.0.0.1:19421`. `TaskMode` later for the phone.
//!
//! The customer build always constructs it. A `dev-tools` build constructs it
//! only with `--live`; without that flag a mock engine stands in (see
//! `crate::dev`).

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// Lab and older installs listen here. A second window must not adopt that process.
const DEFAULT_CONTROL_PORT: u16 = 19421;
/// Cabinet catalog. Same route the cabinet order form uses for exit countries.
/// The edge welcome is not this list.
const CABINET_CATALOG: &str = "https://m1r4g3.bid/api/public/catalog";
/// Claim redeem origin. The catalog host is a different service.
const PANEL_ORIGIN: &str = "https://panel.m1r4g3.bid";

/// Snapshot the GUI reflects. `online` is false when the control API did not answer.
/// `tun_alive` is true only when `state` is `"connected"` — the name is a leftover
/// the Bloom phase still reads.
#[allow(dead_code)]
#[derive(Clone, Default)]
pub struct EngineStatus {
    pub online: bool,
    /// Control API reported `state == "connected"`.
    pub tun_alive: bool,
    pub cli_alive: bool,
    /// Control `state` string (`off`, `connecting`, `connected`, …).
    pub phase: String,
    pub monitor_reachable: bool,
    pub error: Option<String>,
    /// Listener or tun process is actually up. Not the selected mode.
    pub socks: bool,
    pub http: bool,
    pub tun: bool,
    /// `socks`, `http`, or `tun`.
    pub mode: String,
    /// Why TUN is not running. `None` when another mode is selected.
    pub tun_block: Option<String>,
    /// Sites the proxy or the TUN split sends through. Empty means every destination.
    pub services: Vec<String>,
    /// Apps the proxy or the TUN split sends through. Empty means every local program.
    pub apps: Vec<String>,
    /// TUN names that stay direct. SOCKS and HTTP do not use these.
    pub bypass_services: Vec<String>,
    pub bypass_apps: Vec<String>,
    /// Exit countries from the cabinet catalog. Empty until that GET answers.
    pub countries: Vec<String>,
    /// Requested exit: `auto` or an ISO alpha-2 code.
    pub exit: String,
}

/// GUI↔engine seam. `LocalHttp` is the desktop parent; `TaskMode` later
/// (in-process, for the phone) will implement the same trait.
pub trait EngineLink: Send + Sync {
    fn status(&self) -> EngineStatus;
    fn connect(&self);
    fn disconnect(&self);
    fn set_listeners(&self, _socks: bool, _http: bool) {}
    fn set_tun(&self, _enabled: bool) {}
    /// Sites and apps for SOCKS, HTTP, and TUN split. Empty on a side means unconstrained.
    fn set_scope(
        &self,
        _services: &[String],
        _apps: &[String],
        _bypass_services: &[String],
        _bypass_apps: &[String],
    ) {
    }
    /// `auto` or an ISO alpha-2 country. The engine redials on the next session.
    fn set_exit(&self, _exit: &str) {}
    /// Redeem a claim code or `/c/` link, write a new file, and respawn only this child.
    fn bind_claim(&self, _text: &str) {}
    /// True only after a cabinet claim has been redeemed into the device file.
    fn is_bound(&self) -> bool {
        false
    }
    /// Human tag stored on the device. Not a cabinet login.
    fn bound_name(&self) -> Option<String> {
        None
    }
    /// The cabinet page may open only on an install explicitly marked as the
    /// admin client. A device redeemed from the cabinet does not get that mark.
    fn cabinet_open(&self) -> bool {
        false
    }
    /// Stop the child and delete the device file. The cabinet is not called.
    fn unlink(&self) {}
    /// One-shot line from the last bind attempt. Empty until that attempt finishes.
    fn take_bind_note(&self) -> Option<String> {
        None
    }
}

/// Desktop parent: one subscriber-client child, driven over the loopback control API.
pub struct LocalHttp {
    status: Arc<Mutex<EngineStatus>>,
    /// The one child this process spawned. Drop kills only this handle.
    child: Arc<Mutex<Option<Child>>>,
    /// Bind attempt result. The UI takes it once.
    bind_note: Arc<Mutex<Option<String>>>,
    /// Set when the child was started from the redeemed device file.
    bound: Arc<AtomicBool>,
    /// `http://127.0.0.1:<port>` of the child this process spawned. `None` until then,
    /// so a foreign subscriber already on 19421 cannot paint this window as up.
    control: Arc<Mutex<Option<String>>>,
}

impl LocalHttp {
    /// Open the window. The subscriber child starts only from a redeemed device
    /// file. A launch script cannot supply that file.
    pub fn start() -> Self {
        let bound = Arc::new(AtomicBool::new(false));
        let control = Arc::new(Mutex::new(None));
        let child = match bound_device_path() {
            Ok(path) if path.is_file() => {
                let url = pick_control_url();
                match spawn_child_with(&path, &url) {
                    Ok(child) => {
                        *control.lock().unwrap_or_else(PoisonError::into_inner) = Some(url);
                        bound.store(true, Ordering::Relaxed);
                        Some(child)
                    }
                    Err(e) => {
                        tracing::warn!("subscriber-client spawn failed: {e}");
                        None
                    }
                }
            }
            _ => None,
        };
        let status = Arc::new(Mutex::new(EngineStatus::default()));
        spawn_poller(status.clone(), Arc::clone(&control));
        spawn_cabinet_countries(status.clone());
        Self {
            status,
            child: Arc::new(Mutex::new(child)),
            bind_note: Arc::new(Mutex::new(None)),
            bound,
            control,
        }
    }

    fn fire_here(&self, path: &'static str, body: serde_json::Value) {
        let base = self.control.lock().unwrap_or_else(PoisonError::into_inner).clone();
        if let Some(base) = base {
            fire(&base, path, body);
        }
    }
}

impl EngineLink for LocalHttp {
    fn status(&self) -> EngineStatus {
        self.status.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
    fn connect(&self) {
        if !self.is_bound() {
            return;
        }
        self.fire_here("/v1/session", serde_json::json!({"up": true}));
    }
    fn disconnect(&self) {
        self.fire_here("/v1/session", serde_json::json!({"up": false}));
    }
    fn set_listeners(&self, socks: bool, http: bool) {
        let pick_socks = socks;
        let pick_http = http && !pick_socks;
        {
            let mut guard = self.status.lock().unwrap_or_else(PoisonError::into_inner);
            guard.socks = false;
            guard.http = false;
            guard.tun = false;
            guard.tun_block = None;
            if pick_http {
                guard.mode = "http".to_owned();
            } else {
                guard.mode = "socks".to_owned();
            }
        }
        self.fire_here("/v1/listeners", serde_json::json!({"socks": pick_socks, "http": pick_http}));
    }
    fn set_exit(&self, exit: &str) {
        self.fire_here("/v1/exit", serde_json::json!({"exit": exit}));
    }
    fn set_tun(&self, enabled: bool) {
        {
            let mut guard = self.status.lock().unwrap_or_else(PoisonError::into_inner);
            guard.socks = false;
            guard.http = false;
            guard.tun = false;
            if enabled {
                guard.mode = "tun".to_owned();
            } else {
                guard.mode = "socks".to_owned();
            }
            guard.tun_block = None;
            guard.error = None;
        }
        // Lists stay on /v1/scope. An absent key must not clear them.
        self.fire_here("/v1/tun", serde_json::json!({"enabled": enabled, "mode": "split"}));
    }
    fn set_scope(
        &self,
        services: &[String],
        apps: &[String],
        bypass_services: &[String],
        bypass_apps: &[String],
    ) {
        {
            let mut guard = self.status.lock().unwrap_or_else(PoisonError::into_inner);
            guard.services = services.to_vec();
            guard.apps = apps.to_vec();
            guard.bypass_services = bypass_services.to_vec();
            guard.bypass_apps = bypass_apps.to_vec();
        }
        self.fire_here(
            "/v1/scope",
            serde_json::json!({
                "services": services,
                "apps": apps,
                "bypass_services": bypass_services,
                "bypass_apps": bypass_apps,
            }),
        );
    }

    fn bind_claim(&self, text: &str) {
        let text = text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        let note = Arc::clone(&self.bind_note);
        let child = Arc::clone(&self.child);
        let bound = Arc::clone(&self.bound);
        let control = Arc::clone(&self.control);
        std::thread::spawn(move || {
            let line = redeem_and_respawn(&text, &child, &bound, &control);
            *note.lock().unwrap_or_else(PoisonError::into_inner) = Some(line);
        });
    }

    fn take_bind_note(&self) -> Option<String> {
        self.bind_note.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    fn is_bound(&self) -> bool {
        self.bound.load(Ordering::Relaxed)
    }

    fn cabinet_open(&self) -> bool {
        self.is_bound() && cabinet_marker_present()
    }

    fn bound_name(&self) -> Option<String> {
        if !self.is_bound() {
            return None;
        }
        let path = bound_device_path().ok()?;
        let text = std::fs::read_to_string(path).ok()?;
        let value: serde_json::Value = serde_json::from_str(&text).ok()?;
        let label = value.get("label")?.as_str()?.trim();
        if label.is_empty()
            || label.len() > 48
            || !label.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return None;
        }
        Some(label.to_owned())
    }

    fn unlink(&self) {
        if let Some(mut child) = self.child.lock().unwrap_or_else(PoisonError::into_inner).take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Ok(path) = bound_device_path() {
            let _ = std::fs::remove_file(path);
        }
        self.bound.store(false, Ordering::Relaxed);
    }
}

impl LocalHttp {
    /// Spawn the subscriber on a standalone entrance artefact. The cabinet
    /// device file is not read and not written. `json` must be the artefact
    /// document the server put in `client.link`.
    pub fn adopt_artefact(&self, json: &str) -> Result<(), String> {
        let json = json.trim();
        let value: serde_json::Value = serde_json::from_str(json).map_err(|_| crate::text::tr("bind.not_artefact"))?;
        if !value.is_object() {
            return Err(crate::text::tr("bind.not_artefact").into());
        }
        let dir = exe_dir().map_err(|e| e.to_string())?.join("standalone");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join("artefact.json");
        std::fs::write(&path, json).map_err(|e| e.to_string())?;
        let url = {
            let mut slot = self.control.lock().unwrap_or_else(PoisonError::into_inner);
            if slot.is_none() {
                *slot = Some(pick_control_url());
            }
            slot.clone().unwrap_or_else(|| format!("http://127.0.0.1:{DEFAULT_CONTROL_PORT}"))
        };
        replace_child(&self.child, &path, &url).map_err(|e| format!("{}: {e}", crate::text::tr("bind.client")))?;
        self.bound.store(true, Ordering::Relaxed);
        Ok(())
    }
}

/// Cabinet page in the machine browser. No token. Only an install that has
/// the empty `cabinet-open` marker beside the exe.
pub(crate) fn open_marked_cabinet() {
    if !cabinet_marker_present() {
        return;
    }
    const CABINET_URL: &str = "https://m1r4g3.bid/cabinet/";
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let _ = Command::new("cmd")
            .args(["/C", "start", "", CABINET_URL])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg(CABINET_URL).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = Command::new("xdg-open").arg(CABINET_URL).spawn();
    }
}

impl Drop for LocalHttp {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.lock().unwrap_or_else(PoisonError::into_inner).take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn spawn_poller(status: Arc<Mutex<EngineStatus>>, control: Arc<Mutex<Option<String>>>) {
    std::thread::Builder::new()
        .name("mirage-engine-poll".into())
        .spawn(move || {
            let Ok(client) = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
            else {
                return;
            };
            loop {
                let base = control.lock().unwrap_or_else(PoisonError::into_inner).clone();
                let Some(base) = base else {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                };
                let fetched = match client.get(format!("{base}/v1/status")).send() {
                    Ok(response) if response.status().is_success() => response
                        .text()
                        .map(|body| parse_status(&body))
                        .unwrap_or_default(),
                    _ => {
                        let prev = status.lock().unwrap_or_else(PoisonError::into_inner).clone();
                        EngineStatus {
                            countries: prev.countries,
                            exit: prev.exit,
                            services: prev.services,
                            apps: prev.apps,
                            bypass_services: prev.bypass_services,
                            bypass_apps: prev.bypass_apps,
                            mode: prev.mode,
                            tun_block: prev.tun_block,
                            error: prev.error,
                            socks: prev.socks,
                            http: prev.http,
                            tun: prev.tun,
                            ..EngineStatus::default()
                        }
                    }
                };
                let mut guard = status.lock().unwrap_or_else(PoisonError::into_inner);
                // The cabinet thread owns `countries`. Control status must not
                // replace that list with the edge welcome.
                let countries = guard.countries.clone();
                *guard = EngineStatus { countries, ..fetched };
                drop(guard);
                std::thread::sleep(Duration::from_millis(700));
            }
        })
        .ok();
}

pub(crate) fn spawn_cabinet_countries(status: Arc<Mutex<EngineStatus>>) {
    std::thread::Builder::new()
        .name("mirage-cabinet-countries".into())
        .spawn(move || {
            let Ok(client) = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(8))
                .build()
            else {
                return;
            };
            loop {
                if let Some(codes) = fetch_cabinet_countries(&client) {
                    status.lock().unwrap_or_else(PoisonError::into_inner).countries = codes;
                }
                std::thread::sleep(Duration::from_secs(60));
            }
        })
        .ok();
}

/// `egress_countries[].code` from the cabinet catalog. Two uppercase letters
/// only. `None` leaves the last good list in place.
fn fetch_cabinet_countries(client: &reqwest::blocking::Client) -> Option<Vec<String>> {
    let body = client.get(CABINET_CATALOG).send().ok()?.text().ok()?;
    let value: serde_json::Value = serde_json::from_str(&body).ok()?;
    let items = value.get("egress_countries")?.as_array()?;
    let codes: Vec<String> = items
        .iter()
        .filter_map(|item| item.get("code").and_then(|code| code.as_str()))
        .filter(|code| code.len() == 2 && code.bytes().all(|byte| byte.is_ascii_uppercase()))
        .map(str::to_owned)
        .collect();
    if codes.is_empty() { None } else { Some(codes) }
}

fn control_client() -> Result<reqwest::blocking::Client, reqwest::Error> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
}

fn fire(base: &str, path: &'static str, body: serde_json::Value) {
    let base = base.to_owned();
    std::thread::spawn(move || {
        if let Ok(client) = control_client() {
            post_json(&client, &base, path, body);
        }
    });
}

fn post_json(client: &reqwest::blocking::Client, base: &str, path: &str, body: serde_json::Value) {
    let Ok(bytes) = serde_json::to_vec(&body) else { return };
    let _ = client
        .post(format!("{base}{path}"))
        .header("content-type", "application/json")
        .body(bytes)
        .send();
}

fn string_list(value: &serde_json::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(|item| item.as_array())
        .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

fn parse_status(body: &str) -> EngineStatus {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return EngineStatus::default();
    };
    let state = value.get("state").and_then(|item| item.as_str()).unwrap_or("");
    EngineStatus {
        online: true,
        tun_alive: state == "connected",
        cli_alive: false,
        phase: state.to_string(),
        monitor_reachable: false,
        error: value
            .get("error")
            .and_then(|item| item.as_str())
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                if state == "refused" || state == "fatal" {
                    Some(state.to_string())
                } else {
                    None
                }
            }),
        socks: value.get("socks").and_then(|item| item.as_bool()).unwrap_or(false),
        http: value.get("http").and_then(|item| item.as_bool()).unwrap_or(false),
        tun: value.get("tun").and_then(|item| item.as_bool()).unwrap_or(false),
        mode: value.get("mode").and_then(|item| item.as_str()).unwrap_or("socks").to_owned(),
        tun_block: value.get("tun_block").and_then(|item| item.as_str()).map(str::to_owned),
        services: string_list(&value, "services"),
        apps: string_list(&value, "apps"),
        bypass_services: string_list(&value, "bypass_services"),
        bypass_apps: string_list(&value, "bypass_apps"),
        countries: Vec::new(),
        exit: value.get("exit").and_then(|item| item.as_str()).unwrap_or("auto").to_owned(),
    }
}

fn exe_dir() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    exe.parent()
        .map(|path| path.to_path_buf())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "exe has no parent"))
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var(key).ok().filter(|value| !value.is_empty()).map(PathBuf::from)
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn find_tun_bin(subscriber: &std::path::Path) -> Option<PathBuf> {
    if let Some(path) = env_path("MIRAGE_TUN_BIN") {
        if path.is_file() {
            return Some(path);
        }
    }
    let dir = subscriber.parent()?;
    let name = format!("mirage2tun{}", std::env::consts::EXE_SUFFIX);
    let sibling = dir.join(&name);
    if sibling.is_file() {
        return Some(sibling);
    }
    let nested = dir.join("../../../mirage2tun/target/release").join(&name);
    if nested.is_file() {
        return Some(nested);
    }
    None
}

fn subscriber_bin() -> std::io::Result<PathBuf> {
    let name = format!("mirage-subscriber-client{}", std::env::consts::EXE_SUFFIX);
    if let Some(path) = env_path("MIRAGE_SUBSCRIBER_CLIENT") {
        if path.is_file() {
            return Ok(path);
        }
    }
    let dir = exe_dir()?;
    let sibling = dir.join(&name);
    if sibling.is_file() {
        return Ok(sibling);
    }
    // The GUI binary sits in crates/client-gui/target/release. The subscriber
    // build is the workspace release, four levels up.
    let nested = dir.join("../../../../target/release").join(&name);
    if nested.is_file() {
        return Ok(nested);
    }
    Ok(sibling)
}

/// Redeemed device file next to the GUI. This is the only credential the child gets.
fn bound_device_path() -> std::io::Result<PathBuf> {
    Ok(exe_dir()?.join("bound-device.json"))
}

/// Empty marker beside the device file. Presence is the whole signal: this
/// install may open the cabinet page. Customer devices never receive it.
pub(crate) fn cabinet_marker_present() -> bool {
    bound_device_path().map(|path| path.with_file_name("cabinet-open").is_file()).unwrap_or(false)
}

/// First loopback port that is not already taken. Skips the lab subscriber on 19421.
fn pick_control_url() -> String {
    for port in DEFAULT_CONTROL_PORT..DEFAULT_CONTROL_PORT + 20 {
        let address = format!("127.0.0.1:{port}");
        let occupied = std::net::TcpStream::connect_timeout(
            &address.parse().expect("loopback address"),
            Duration::from_millis(150),
        )
        .is_ok();
        if !occupied {
            return format!("http://{address}");
        }
    }
    format!("http://127.0.0.1:{DEFAULT_CONTROL_PORT}")
}

fn spawn_child_with(artefact: &std::path::Path, control_url: &str) -> std::io::Result<Child> {
    let dir = exe_dir()?;
    let bin = subscriber_bin()?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("mirage-subscriber-client.log"))?;
    let log_err = log.try_clone()?;
    let tun = find_tun_bin(&bin);
    let host_port = control_url.trim_start_matches("http://");
    let mut cmd = Command::new(bin);
    cmd.arg("--artefact")
        .arg(artefact)
        .arg("--control")
        .arg(host_port);
    if let Some(tun) = tun {
        cmd.arg("--tun-bin").arg(tun);
    }
    cmd.current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        return cmd.creation_flags(CREATE_NO_WINDOW).spawn();
    }
    #[cfg(not(windows))]
    {
        cmd.spawn()
    }
}

fn redeem_and_respawn(
    text: &str,
    child: &Mutex<Option<Child>>,
    bound: &AtomicBool,
    control: &Mutex<Option<String>>,
) -> String {
    let bin = match subscriber_bin() {
        Ok(bin) => bin,
        Err(err) => return format!("{} ({err})", crate::text::tr("bind.no_client")),
    };
    let out = match bound_device_path() {
        Ok(path) => path,
        Err(err) => return format!("{}: {err}", crate::text::tr("bind.fail")),
    };
    let mut cmd = Command::new(&bin);
    cmd.arg("claim")
        .arg("--panel")
        .arg(PANEL_ORIGIN)
        .arg("--claim")
        .arg(text)
        .arg("--out")
        .arg(&out)
        .stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let output = match cmd.output() {
        Ok(output) => output,
        Err(err) => return format!("{} ({err})", crate::text::tr("bind.not_started")),
    };
    if !output.status.success() || !out.is_file() {
        return bind_failure(&output, text);
    }
    // An existing cabinet-open beside this exe stays. A customer directory has
    // none, so redeeming a claim there still does not grow the button.
    let url = {
        let mut slot = control.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(pick_control_url());
        }
        slot.clone().unwrap_or_else(|| format!("http://127.0.0.1:{DEFAULT_CONTROL_PORT}"))
    };
    match replace_child(child, &out, &url) {
        Ok(()) => {
            bound.store(true, Ordering::Relaxed);
            bind_ok(&output.stdout)
        }
        Err(err) => format!("{}: {err}", crate::text::tr("bind.ok_stale")),
    }
}

fn replace_child(
    slot: &Mutex<Option<Child>>,
    artefact: &std::path::Path,
    control_url: &str,
) -> std::io::Result<()> {
    let mut guard = slot.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(mut old) = guard.take() {
        let _ = old.kill();
        let _ = old.wait();
    }
    *guard = Some(spawn_child_with(artefact, control_url)?);
    Ok(())
}

fn bind_ok(stdout: &[u8]) -> String {
    let text = String::from_utf8_lossy(stdout);
    let id = text.split_whitespace().skip_while(|word| *word != "device").nth(1).filter(|id| {
        id.len() < 80 && id.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    });
    match id {
        Some(id) => format!("{} · {id}", crate::text::tr("bind.ok")),
        None => crate::text::tr("bind.ok").to_owned(),
    }
}

fn bind_failure(output: &std::process::Output, claim: &str) -> String {
    match output.status.code().unwrap_or(-1) {
        2 => return crate::text::tr("bind.missing").to_owned(),
        3 => return crate::text::tr("bind.used").to_owned(),
        4 => return crate::text::tr("bind.soon").to_owned(),
        code => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let last = stderr.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("");
            let cleaned = scrub_claim(last, claim);
            if cleaned.is_empty() {
                format!("{} {code}", crate::text::tr("bind.refused"))
            } else {
                format!("{}: {cleaned}", crate::text::tr("bind.fail"))
            }
        }
    }
}

fn scrub_claim(line: &str, claim: &str) -> String {
    let mut kept = String::new();
    for token in line.split_whitespace() {
        if !claim.is_empty() && token.contains(claim) {
            continue;
        }
        let lower = token.to_ascii_lowercase();
        if lower.contains("private") || lower.contains("pubkey") {
            continue;
        }
        if !kept.is_empty() {
            kept.push(' ');
        }
        kept.push_str(token);
        if kept.len() > 160 {
            break;
        }
    }
    kept
}
