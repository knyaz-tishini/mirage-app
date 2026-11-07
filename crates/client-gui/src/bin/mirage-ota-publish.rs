//! Sign one desktop binary and write the stable-track manifest.
//!
//! The private key is a 32-byte seed file. The window only has the matching
//! public key. Output layout, under `--out`:
//!
//! ```text
//! <version>/<payload>
//! stable/<platform>.json
//! ```

use std::env;
use std::fs;
use std::path::PathBuf;

use base64::Engine;
use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey;
use mirage_client_gui::ota;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help") {
        eprintln!("mirage-ota-publish --key <seed> --bin <file> --version <semver> --out <dir> [--platform win64]");
        std::process::exit(2);
    }
    let key_path = required(&args, "--key");
    let bin_path = required(&args, "--bin");
    let version = required(&args, "--version");
    let out = PathBuf::from(required(&args, "--out"));
    let platform = optional(&args, "--platform").unwrap_or_else(|| ota::current_platform().to_owned());
    if platform == "unknown" {
        eprintln!("platform is unknown; pass --platform");
        std::process::exit(2);
    }

    let seed = match ota::read_signing_seed(PathBuf::from(&key_path).as_path()) {
        Ok(seed) => seed,
        Err(err) => {
            eprintln!("key: {err}");
            std::process::exit(1);
        }
    };
    let bytes = match fs::read(&bin_path) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("bin: {err}");
            std::process::exit(1);
        }
    };
    let digest = ota::sha256_raw(&bytes);
    let signing = SigningKey::from_bytes(&seed);
    let signature = signing.sign(&digest);
    let signature_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
    match ota::verify_binary_signature(&bytes, &signature_b64) {
        ota::VerifyResult::Valid => {}
        other => {
            eprintln!("self-check failed: {other:?}");
            std::process::exit(1);
        }
    }

    let payload = ota::payload_name(&platform);
    let stored = out.join(&version).join(&payload);
    if let Some(parent) = stored.parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            eprintln!("out: {err}");
            std::process::exit(1);
        }
    }
    if let Err(err) = fs::write(&stored, &bytes) {
        eprintln!("write payload: {err}");
        std::process::exit(1);
    }
    let download_url = format!("https://m1r4g3.bid/ota/desktop/{version}/{payload}");
    let manifest = ota::VersionManifest::signed(
        version.clone(),
        ota::sha256_hex(&bytes),
        download_url,
        bytes.len() as u64,
        signature_b64,
    );
    let manifest_path = out.join("stable").join(format!("{platform}.json"));
    if let Err(err) = ota::write_manifest(&manifest_path, &manifest) {
        eprintln!("manifest: {err}");
        std::process::exit(1);
    }
    match ota::retain_releases(&out, ota::RELEASES_KEPT) {
        Ok(removed) => {
            for name in removed {
                println!("dropped {name}");
            }
        }
        Err(err) => {
            eprintln!("retain: {err}");
            std::process::exit(1);
        }
    }
    println!("published {version} {platform} bytes {}", bytes.len());
    println!("payload {}", stored.display());
    println!("manifest {}", manifest_path.display());
}

fn required(args: &[String], name: &str) -> String {
    match optional(args, name) {
        Some(value) => value,
        None => {
            eprintln!("missing {name}");
            std::process::exit(2);
        }
    }
}

fn optional(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|arg| arg == name).and_then(|index| args.get(index + 1)).cloned()
}
