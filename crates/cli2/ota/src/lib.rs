//! OTA update logic: manifest fetching, download, sha256 verification,
//! Minisign signature verification, and atomic binary swap.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, info};

/// OTA update manifest (Tauri-inspired JSON format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtaManifest {
    pub version: String,
    pub channel: String,
    #[serde(default)]
    pub criticality: Option<String>,
    pub min_compat_version: String,
    pub pub_date: String,
    #[serde(default)]
    pub notes: Option<String>,
    pub platforms: HashMap<String, OtaPlatform>,
}

/// Per-platform artifact descriptor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtaPlatform {
    /// HTTPS URL to the (zip-compressed) binary.
    pub url: String,
    /// Lowercase hex SHA-256 of the compressed artifact.
    pub sha256: String,
    /// Full Minisign `.sig` file contents (the signature string).
    pub signature: String,
    /// Compressed artifact size in bytes (for progress reporting).
    pub size: u64,
}

/// Fetch the OTA manifest from `monitor_url`.
///
/// `monitor_url` should be the base URL; this function appends `/ota/client/latest`.
pub async fn fetch_manifest(monitor_url: &str) -> anyhow::Result<OtaManifest> {
    let url = format!("{}/ota/client/latest", monitor_url.trim_end_matches('/'));
    debug!(url = %url, "fetching OTA manifest");
    let manifest: OtaManifest = reqwest::get(&url)
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("HTTP error fetching {url}"))?
        .json()
        .await
        .with_context(|| format!("deserializing manifest from {url}"))?;
    info!(version = %manifest.version, channel = %manifest.channel, "manifest fetched");
    Ok(manifest)
}

/// Download the artifact for the current platform, verify sha256 and Minisign signature.
/// Returns the path to the verified uncompressed binary in the system temp directory.
pub async fn download_and_verify(
    platform: &OtaPlatform,
    pubkey_b64: &str,
) -> anyhow::Result<PathBuf> {
    let temp_dir = std::env::temp_dir();
    // Derive a stable temp filename from the URL tail.
    let file_stem = platform
        .url
        .rsplit('/')
        .next()
        .unwrap_or("mirage-update.bin");
    let compressed_path = temp_dir.join(file_stem);

    // Download.
    info!(url = %platform.url, "downloading OTA artifact");
    download_file(&platform.url, &compressed_path).await?;

    // Verify SHA-256.
    let computed = sha256_hex(&compressed_path)
        .with_context(|| format!("sha256 of {:?}", compressed_path))?;
    if computed != platform.sha256.to_lowercase() {
        bail!(
            "sha256 mismatch: expected {} got {}",
            platform.sha256,
            computed
        );
    }
    debug!("sha256 OK");

    // Verify Minisign signature.
    verify_minisign(&compressed_path, &platform.signature, pubkey_b64)
        .with_context(|| "Minisign verification failed")?;
    debug!("Minisign signature OK");

    // Decompress.
    let binary_path = decompress(&compressed_path, &temp_dir)
        .with_context(|| format!("decompression of {:?}", compressed_path))?;

    info!(path = ?binary_path, "artifact verified and ready");
    Ok(binary_path)
}

/// Return the platform key string for the current target, e.g. `"windows-x86_64"`.
pub fn platform_key() -> &'static str {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    return "windows-x86_64";
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    return "linux-x86_64";
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    return "linux-aarch64";
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    return "darwin-x86_64";
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return "darwin-aarch64";
    #[cfg(not(any(
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64"),
    )))]
    return "unknown";
}

/// The version of the supervisor binary (from `Cargo.toml` at compile time).
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Returns `true` if `candidate` semver is strictly newer than `current`.
pub fn is_newer(current: &str, candidate: &str) -> bool {
    let parse = |s: &str| semver::Version::parse(s).ok();
    match (parse(current), parse(candidate)) {
        (Some(c), Some(n)) => n > c,
        _ => false,
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

async fn download_file(url: &str, dest: &Path) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;

    let response = reqwest::get(url)
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("HTTP error from {url}"))?;

    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("creating {:?}", dest))?;

    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("reading body from {url}"))?;

    file.write_all(&bytes)
        .await
        .with_context(|| format!("writing to {:?}", dest))?;

    Ok(())
}

fn sha256_hex(path: &Path) -> anyhow::Result<String> {
    let data = std::fs::read(path).with_context(|| format!("reading {:?}", path))?;
    let digest = Sha256::digest(&data);
    Ok(hex::encode(digest))
}

fn verify_minisign(path: &Path, sig_str: &str, pubkey_b64: &str) -> anyhow::Result<()> {
    use minisign_verify::{PublicKey, Signature};

    let pubkey =
        PublicKey::from_base64(pubkey_b64).with_context(|| "parsing Minisign public key")?;
    let sig =
        Signature::decode(sig_str).with_context(|| "decoding Minisign signature")?;
    let data = std::fs::read(path).with_context(|| format!("reading {:?} for verify", path))?;
    pubkey
        .verify(&data, &sig, false)
        .with_context(|| "Minisign signature invalid")?;
    Ok(())
}

/// Decompress the downloaded artifact.
/// - `.zip` → extracts first entry with `.exe` or matching name.
/// - Other → treat as raw binary, just return as-is (caller can rename).
fn decompress(src: &Path, dest_dir: &Path) -> anyhow::Result<PathBuf> {
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    if ext == "zip" {
        decompress_zip(src, dest_dir)
    } else {
        // Not compressed — return the path as-is.
        Ok(src.to_path_buf())
    }
}

fn decompress_zip(src: &Path, dest_dir: &Path) -> anyhow::Result<PathBuf> {
    let data = std::fs::read(src).with_context(|| format!("reading zip {:?}", src))?;
    let cursor = std::io::Cursor::new(data);
    let mut archive = zip::ZipArchive::new(cursor).with_context(|| "parsing zip archive")?;

    // Extract the first file entry (skip directories).
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .with_context(|| format!("zip entry {i}"))?;

        if entry.is_dir() {
            continue;
        }

        let name = entry.name().to_string();
        let out_path = dest_dir.join(
            Path::new(&name)
                .file_name()
                .unwrap_or(std::ffi::OsStr::new(&name)),
        );

        let mut out = std::fs::File::create(&out_path)
            .with_context(|| format!("creating {:?}", out_path))?;
        std::io::copy(&mut entry, &mut out)
            .with_context(|| format!("extracting {name} to {:?}", out_path))?;

        info!(entry = %name, dest = ?out_path, "extracted from zip");
        return Ok(out_path);
    }

    bail!("zip archive is empty or contains only directories");
}
