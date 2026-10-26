//! Desktop self-update. Same sequence as the chart client:
//! check the stable manifest, download in the background, require SHA-256
//! and an Ed25519 signature over that digest, write the new binary beside
//! the running one, then wait. Nothing restarts until the window's
//! «Перезагрузить» control is pressed.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Public origin that already serves `/ota/` for this product.
const UPDATE_ORIGIN: &str = "https://m1r4g3.bid";

/// First look, after the window has had a moment to draw.
const CHECK_AFTER: Duration = Duration::from_secs(5);
/// How often a running build looks again. Short enough that a build
/// published after launch is noticed without a restart.
const CHECK_EVERY: Duration = Duration::from_secs(15);

/// Ed25519 public key. The private half lives only on the signing machine
/// (`env/mirage-desktop-ota/signing_key.bin`) and is not in this binary.
const PRIMARY_PUBLIC_KEY: &[u8; 32] = &[
    0x92, 0xc3, 0xa0, 0x90, 0x82, 0xaa, 0xce, 0x3d, 0x50, 0xde, 0x54, 0xbf, 0x50, 0xa1, 0x9a, 0xea,
    0x18, 0xe8, 0x6d, 0x0d, 0x1e, 0x29, 0xec, 0x59, 0x7c, 0xff, 0x7f, 0x5d, 0xed, 0x43, 0xb3, 0x3d,
];

/// What the object store returns for one platform on the stable track.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionManifest {
    pub version: String,
    pub sha256: String,
    pub download_url: String,
    pub release_notes: String,
    pub file_size: u64,
    #[serde(default)]
    pub signature: Option<String>,
}

#[derive(Debug, Clone)]
struct UpdateInfo {
    version: String,
    sha256: String,
    download_url: String,
    signature: Option<String>,
}

/// What the window reads each frame.
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateStatus {
    Idle,
    Checking,
    Downloading { percent: u8 },
    Verifying,
    Installing,
    RestartPending { version: String },
    Error(String),
}

impl Default for UpdateStatus {
    fn default() -> Self {
        Self::Idle
    }
}

/// Signature check over `SHA256(binary)`.
#[derive(Debug)]
pub enum VerifyResult {
    Valid,
    Unsigned,
    Invalid(String),
    FormatError(String),
}

fn state() -> &'static Mutex<UpdateStatus> {
    static STATUS: Mutex<UpdateStatus> = Mutex::new(UpdateStatus::Idle);
    &STATUS
}

fn set_status(next: UpdateStatus) {
    *state().lock().unwrap_or_else(|p| p.into_inner()) = next;
}

pub fn status() -> UpdateStatus {
    state().lock().unwrap_or_else(|p| p.into_inner()).clone()
}

pub fn current_platform() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", _) => "win64",
        ("linux", _) => "linux64",
        ("macos", "aarch64") => "macos-arm64",
        ("macos", _) => "macos-x64",
        _ => "unknown",
    }
}

pub fn payload_name(platform: &str) -> String {
    if platform == "win64" {
        "win64.exe".to_owned()
    } else {
        platform.to_owned()
    }
}

pub fn manifest_url(platform: &str) -> String {
    format!("{UPDATE_ORIGIN}/ota/desktop/stable/{platform}.json")
}

/// SHA-256 of `bytes`, lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Raw SHA-256 digest. This is the message the signature covers.
pub fn sha256_raw(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

pub fn verify_binary_signature(binary: &[u8], signature_b64: &str) -> VerifyResult {
    let trimmed = signature_b64.trim();
    if trimmed.is_empty() {
        return VerifyResult::Unsigned;
    }
    let cleaned: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    let sig_bytes = match base64::engine::general_purpose::STANDARD.decode(cleaned) {
        Ok(bytes) => bytes,
        Err(err) => return VerifyResult::FormatError(format!("base64: {err}")),
    };
    if sig_bytes.len() != 64 {
        return VerifyResult::FormatError(format!("signature is {} bytes", sig_bytes.len()));
    }
    let mut sig_array = [0u8; 64];
    sig_array.copy_from_slice(&sig_bytes);
    let signature = Signature::from_bytes(&sig_array);
    let hash = sha256_raw(binary);
    let key = match VerifyingKey::from_bytes(PRIMARY_PUBLIC_KEY) {
        Ok(key) => key,
        Err(err) => return VerifyResult::FormatError(format!("public key: {err}")),
    };
    if key.verify_strict(&hash, &signature).is_ok() {
        VerifyResult::Valid
    } else {
        VerifyResult::Invalid("signature does not match the compiled-in key".to_owned())
    }
}

pub fn is_newer(current: &str, proposed: &str) -> bool {
    match (semver::Version::parse(current), semver::Version::parse(proposed)) {
        (Ok(current), Ok(proposed)) => proposed > current,
        _ => false,
    }
}

pub fn is_downgrade(current: &str, proposed: &str) -> bool {
    match (semver::Version::parse(current), semver::Version::parse(proposed)) {
        (Ok(current), Ok(proposed)) => proposed <= current,
        _ => false,
    }
}

fn note(message: &str) {
    tracing::info!("{message}");
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else {
        return;
    };
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(dir.join("ota.log")) {
        let _ = writeln!(file, "{message}");
    }
}

/// Background check. One thread for the life of the process.
pub fn start() {
    if cfg!(target_os = "ios") {
        return;
    }
    thread::Builder::new()
        .name("mirage-ota".into())
        .spawn(|| {
            thread::sleep(CHECK_AFTER);
            loop {
                if !install_in_progress() {
                    match fetch_latest() {
                        Ok(Some(info)) => install(&info),
                        Ok(None) => {
                            if !matches!(status(), UpdateStatus::RestartPending { .. }) {
                                set_status(UpdateStatus::Idle);
                            }
                        }
                        Err(err) => {
                            note(&format!("ota check failed: {err}"));
                            if !matches!(status(), UpdateStatus::RestartPending { .. }) {
                                set_status(UpdateStatus::Idle);
                            }
                        }
                    }
                }
                thread::sleep(CHECK_EVERY);
            }
        })
        .ok();
}

fn install_in_progress() -> bool {
    matches!(
        status(),
        UpdateStatus::Downloading { .. }
            | UpdateStatus::Verifying
            | UpdateStatus::Installing
            | UpdateStatus::RestartPending { .. }
    )
}

fn fetch_latest() -> Result<Option<UpdateInfo>, String> {
    let current = env!("CARGO_PKG_VERSION");
    set_status(UpdateStatus::Checking);
    let url = manifest_url(current_platform());
    let client = http_client(Duration::from_secs(10))?;
    let response = client.get(&url).send().map_err(|err| err.to_string())?;
    if !response.status().is_success() {
        return Err(format!("manifest {}", response.status()));
    }
    let manifest: VersionManifest = response.json().map_err(|err| err.to_string())?;
    if !is_newer(current, &manifest.version) {
        note(&format!("ota: running {current}, store {}", manifest.version));
        return Ok(None);
    }
    note(&format!("ota: {current} -> {}", manifest.version));
    Ok(Some(UpdateInfo {
        version: manifest.version,
        sha256: manifest.sha256,
        download_url: manifest.download_url,
        signature: manifest.signature,
    }))
}

fn install(info: &UpdateInfo) {
    let current = env!("CARGO_PKG_VERSION");
    if is_downgrade(current, &info.version) {
        set_status(UpdateStatus::Error(format!("refusing downgrade {}", info.version)));
        return;
    }
    set_status(UpdateStatus::Downloading { percent: 0 });
    let binary = match download(&info.download_url, &info.sha256) {
        Ok(bytes) => bytes,
        Err(err) => {
            note(&format!("ota download failed: {err}"));
            set_status(UpdateStatus::Error(err));
            return;
        }
    };
    set_status(UpdateStatus::Verifying);
    let signature = info.signature.as_deref().unwrap_or("");
    match verify_binary_signature(&binary, signature) {
        VerifyResult::Valid => note(&format!("ota: signature ok {}", info.version)),
        VerifyResult::Unsigned => {
            set_status(UpdateStatus::Error("update has no signature".to_owned()));
            return;
        }
        VerifyResult::Invalid(reason) | VerifyResult::FormatError(reason) => {
            note(&format!("ota: rejected signature: {reason}"));
            set_status(UpdateStatus::Error(reason));
            return;
        }
    }
    set_status(UpdateStatus::Installing);
    match self_replace(&binary) {
        Ok(()) => {
            note(&format!("ota: installed {}, waiting for restart", info.version));
            set_status(UpdateStatus::RestartPending { version: info.version.clone() });
        }
        Err(err) => {
            note(&format!("ota install failed: {err}"));
            set_status(UpdateStatus::Error(err));
        }
    }
}

fn download(url: &str, expected_sha256: &str) -> Result<Vec<u8>, String> {
    let client = http_client(Duration::from_secs(600))?;
    let mut response = client.get(url).send().map_err(|err| err.to_string())?;
    if !response.status().is_success() {
        return Err(format!("download {}", response.status()));
    }
    let total = response.content_length().unwrap_or(0);
    let mut data = Vec::with_capacity(total as usize);
    let mut buf = [0u8; 64 * 1024];
    let mut last_pct = 0u8;
    loop {
        let read = response.read(&mut buf).map_err(|err| err.to_string())?;
        if read == 0 {
            break;
        }
        data.extend_from_slice(&buf[..read]);
        if total > 0 {
            let pct = ((data.len() as u64 * 100) / total).min(100) as u8;
            if pct != last_pct {
                last_pct = pct;
                set_status(UpdateStatus::Downloading { percent: pct });
            }
        }
    }
    let got = sha256_hex(&data);
    if got != expected_sha256.to_ascii_lowercase() {
        return Err(format!("sha256 mismatch"));
    }
    Ok(data)
}

fn http_client(timeout: Duration) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|err| err.to_string())
}

/// `{exe}.new`, then rename the running file to `{exe}.old`, then rename
/// `.new` into its place. On Windows a running image can be renamed.
pub fn self_replace(binary: &[u8]) -> Result<(), String> {
    let current = std::env::current_exe().map_err(|err| err.to_string())?;
    let new_path = current.with_extension("exe.new");
    let old_path = current.with_extension("exe.old");
    fs::write(&new_path, binary).map_err(|err| format!("write new: {err}"))?;
    let _ = fs::remove_file(&old_path);
    if let Err(err) = fs::rename(&current, &old_path) {
        let _ = fs::remove_file(&new_path);
        return Err(format!("rename current: {err}"));
    }
    if let Err(err) = fs::rename(&new_path, &current) {
        let _ = fs::rename(&old_path, &current);
        return Err(format!("rename new: {err}"));
    }
    Ok(())
}

/// Drop the previous binary once this process has actually started.
pub fn clear_rollback_copy() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = fs::remove_file(exe.with_extension("exe.old"));
    }
}

/// Press of «Перезагрузить». Spawn this same path (now the new bytes) and leave.
pub fn restart() {
    if cfg!(target_os = "ios") {
        return;
    }
    if let Err(err) = spawn_and_exit() {
        note(&format!("ota restart failed: {err}"));
        set_status(UpdateStatus::Error(err));
    }
}

fn spawn_and_exit() -> Result<(), String> {
    let current = std::env::current_exe().map_err(|err| err.to_string())?;
    let my_pid = std::process::id();
    let mut skip_next = false;
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|arg| {
            if skip_next {
                skip_next = false;
                return false;
            }
            if arg == "--wait-pid" || arg == "--wait-port" {
                skip_next = true;
                return false;
            }
            if arg.starts_with("--wait-pid=") || arg.starts_with("--wait-port=") {
                return false;
            }
            true
        })
        .collect();
    note(&format!("ota: spawning {} --wait-pid {my_pid}", current.display()));
    std::process::Command::new(&current)
        .args(&args)
        .arg("--wait-pid")
        .arg(my_pid.to_string())
        .spawn()
        .map_err(|err| err.to_string())?;
    std::process::exit(0);
}

/// If this process was spawned to replace a parent, wait until that parent
/// has released the binary. No-op on a normal launch.
pub fn wait_for_parent_exit() {
    let args: Vec<String> = std::env::args().collect();
    let Some(pid) = flag_value(&args, "--wait-pid") else {
        return;
    };
    note(&format!("ota: waiting for parent {pid}"));
    wait_for_pid(&pid);
    if let Some(port) = flag_value(&args, "--wait-port").and_then(|value| value.parse::<u16>().ok()) {
        wait_for_port(port);
    }
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|arg| arg == name).and_then(|index| args.get(index + 1)).cloned()
}

fn wait_for_pid(pid: &str) {
    #[cfg(target_os = "windows")]
    {
        extern "system" {
            fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> isize;
            fn WaitForSingleObject(handle: isize, milliseconds: u32) -> u32;
            fn CloseHandle(handle: isize) -> i32;
        }
        const SYNCHRONIZE: u32 = 0x0010_0000;
        let Ok(pid) = pid.parse::<u32>() else {
            return;
        };
        let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
        if handle != 0 {
            unsafe {
                WaitForSingleObject(handle, 30_000);
                CloseHandle(handle);
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        let Ok(pid) = pid.parse::<i32>() else {
            return;
        };
        for _ in 0..60 {
            let alive = unsafe { kill(pid, 0) } == 0;
            if !alive {
                return;
            }
            thread::sleep(Duration::from_millis(500));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for _ in 0..60 {
            if !Path::new(&format!("/proc/{pid}")).exists() {
                return;
            }
            thread::sleep(Duration::from_millis(500));
        }
    }
}

fn wait_for_port(port: u16) {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    for _ in 0..120 {
        if std::net::TcpListener::bind(addr).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(500));
    }
}

/// Read a signing seed. Used by the publisher, never by the window.
pub fn read_signing_seed(path: &Path) -> Result<[u8; 32], String> {
    let bytes = fs::read(path).map_err(|err| err.to_string())?;
    if bytes.len() != 32 {
        return Err(format!("signing key is {} bytes", bytes.len()));
    }
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&bytes);
    Ok(seed)
}

/// Release directories kept on the desktop track. The cap counts versions,
/// not files: every platform payload inside a kept version stays. Older
/// versions are removed when a newer one is published. `stable/` is the
/// track pointer and is not a release.
pub const RELEASES_KEPT: usize = 3;

/// Delete release directories under `root` beyond the newest `keep`.
/// A directory counts only when its name is a semver version. Returns the
/// names that were removed, oldest last.
pub fn retain_releases(root: &Path, keep: usize) -> Result<Vec<String>, String> {
    let mut found: Vec<(semver::Version, String)> = Vec::new();
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.to_string()),
    };
    for entry in entries {
        let entry = entry.map_err(|err| err.to_string())?;
        let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        if !is_dir {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Ok(version) = semver::Version::parse(&name) {
            found.push((version, name));
        }
    }
    found.sort_by(|left, right| right.0.cmp(&left.0));
    let mut removed = Vec::new();
    for (_, name) in found.into_iter().skip(keep) {
        fs::remove_dir_all(root.join(&name)).map_err(|err| format!("remove {name}: {err}"))?;
        removed.push(name);
    }
    Ok(removed)
}

pub fn write_manifest(path: &PathBuf, manifest: &VersionManifest) -> Result<(), String> {
    let body = serde_json::to_vec_pretty(manifest).map_err(|err| err.to_string())?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::write(path, body).map_err(|err| err.to_string())
}

impl VersionManifest {
    pub fn signed(version: String, sha256: String, download_url: String, file_size: u64, signature: String) -> Self {
        Self {
            version,
            sha256,
            download_url,
            release_notes: String::new(),
            file_size,
            signature: Some(signature),
        }
    }
}
