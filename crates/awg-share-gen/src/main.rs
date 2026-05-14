//! AmneziaWG share-config generator.
//!
//! `awg-share-gen --endpoint 91.218.245.247:51820
//!                --server-pubkey ulfEp606EI7rGkJh634SfZPn2EyUn69z8k4KTRkTywE=
//!                --output-dir ./out`
//!
//! Emits these files in `./out`:
//!   • `phone.conf`           — wg-quick text config with AmneziaWG extras.
//!   • `vpn-uri.txt`          — `vpn://...` URI, Amnezia "paste from text" format.
//!   • `phone-qr-<N>.png`     — multi-chunk QR series for camera import (1-indexed).
//!                              Number of chunks depends on payload size (~850 B each).
//!   • `phone-qr.png`         — symlink-style copy of `phone-qr-1.png` for back-compat
//!                              when the payload fits in a single chunk.
//!
//! Defaults match the lm-ru-1 entry / lm-ch-1 terminator deployed in
//! `mirage2node/configs/amnezia-test/`.

use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use clap::Parser;
use flate2::{write::ZlibEncoder, Compression};
use image::{ImageBuffer, Luma};
use qrcode::QrCode;
use serde::Serialize;
use std::{fs, io::Write, path::PathBuf};
use x25519_dalek::{PublicKey, StaticSecret};

#[derive(Parser, Debug)]
#[command(version, about = "AmneziaWG share-config generator")]
struct Args {
    /// Public endpoint foreign clients connect to (entry node).
    #[arg(long)]
    endpoint: String,

    /// Server public key (base64). The matching priv lives on the exit
    /// terminator — operator-side, NEVER shared.
    #[arg(long)]
    server_pubkey: String,

    /// Client private key (base64). If absent, a fresh keypair is minted.
    #[arg(long)]
    client_priv: Option<String>,

    /// Client tunnel address (CIDR). Default `10.66.66.2/32`.
    #[arg(long, default_value = "10.66.66.2/32")]
    client_ip: String,

    /// DNS for the WireGuard interface side. Default `1.1.1.1`.
    #[arg(long, default_value = "1.1.1.1")]
    dns1: String,
    #[arg(long, default_value = "1.0.0.1")]
    dns2: String,

    /// AllowedIPs (comma-separated). Default `0.0.0.0/0, ::/0`.
    #[arg(long, default_value = "0.0.0.0/0, ::/0")]
    allowed_ips: String,

    /// PersistentKeepalive seconds. Default 25.
    #[arg(long, default_value_t = 25)]
    keepalive: u32,

    /// Human label for the profile. Shows up in Amnezia.
    #[arg(long, default_value = "mirage-awg-test")]
    description: String,

    // ── AmneziaWG obfuscation params — MUST match terminator config ────
    #[arg(long, default_value_t = 4)]    jc: u32,
    #[arg(long, default_value_t = 50)]   jmin: u32,
    #[arg(long, default_value_t = 1000)] jmax: u32,
    #[arg(long, default_value_t = 1)]    s1: u32,
    #[arg(long, default_value_t = 2)]    s2: u32,
    #[arg(long, default_value_t = 1247591134)] h1: u32,
    #[arg(long, default_value_t = 1797685375)] h2: u32,
    #[arg(long, default_value_t = 2635015212)] h3: u32,
    #[arg(long, default_value_t = 3237568913)] h4: u32,

    /// Output directory. Files: phone.conf, vpn-uri.txt, phone-qr.png.
    #[arg(long, default_value = "./out")]
    output_dir: PathBuf,
}

fn parse_endpoint(s: &str) -> Result<(String, u16)> {
    let (host, port) = s
        .rsplit_once(':')
        .with_context(|| format!("endpoint must be host:port, got {s}"))?;
    let port: u16 = port.parse().with_context(|| format!("bad port {port}"))?;
    Ok((host.to_string(), port))
}

fn x25519_keypair_b64() -> (String, String) {
    use base64::engine::general_purpose::STANDARD;
    let priv_key = StaticSecret::random_from_rng(rand_core::OsRng);
    let pub_key = PublicKey::from(&priv_key);
    (
        STANDARD.encode(priv_key.to_bytes()),
        STANDARD.encode(pub_key.as_bytes()),
    )
}

fn derive_pubkey_b64(priv_b64: &str) -> Result<String> {
    use base64::engine::general_purpose::STANDARD;
    let raw = STANDARD
        .decode(priv_b64.trim())
        .context("client_priv is not base64")?;
    let arr: [u8; 32] = raw
        .as_slice()
        .try_into()
        .context("client_priv must decode to 32 bytes")?;
    let sk = StaticSecret::from(arr);
    Ok(STANDARD.encode(PublicKey::from(&sk).as_bytes()))
}

fn render_conf(args: &Args, priv_b64: &str, host: &str, port: u16) -> String {
    let mut out = String::new();
    out.push_str("[Interface]\n");
    out.push_str(&format!("Address = {}\n", args.client_ip));
    out.push_str(&format!("DNS = {}, {}\n", args.dns1, args.dns2));
    out.push_str(&format!("PrivateKey = {}\n", priv_b64));
    out.push_str(&format!("Jc = {}\n", args.jc));
    out.push_str(&format!("Jmin = {}\n", args.jmin));
    out.push_str(&format!("Jmax = {}\n", args.jmax));
    out.push_str(&format!("S1 = {}\n", args.s1));
    out.push_str(&format!("S2 = {}\n", args.s2));
    out.push_str(&format!("H1 = {}\n", args.h1));
    out.push_str(&format!("H2 = {}\n", args.h2));
    out.push_str(&format!("H3 = {}\n", args.h3));
    out.push_str(&format!("H4 = {}\n\n", args.h4));
    out.push_str("[Peer]\n");
    out.push_str(&format!("PublicKey = {}\n", args.server_pubkey));
    out.push_str(&format!("AllowedIPs = {}\n", args.allowed_ips));
    out.push_str(&format!("Endpoint = {}:{}\n", host, port));
    out.push_str(&format!("PersistentKeepalive = {}\n", args.keepalive));
    out
}

#[allow(non_snake_case)]
#[derive(Serialize)]
struct AwgInnerConfig<'a> {
    config: String,
    #[serde(rename = "hostName")]
    host_name: &'a str,
    port: u16,
    client_priv_key: &'a str,
    client_ip: &'a str,
    server_pub_key: &'a str,
    allowed_ips: Vec<String>,
    junkPacketCount: String,
    junkPacketMinSize: String,
    junkPacketMaxSize: String,
    initPacketJunkSize: String,
    responsePacketJunkSize: String,
    initPacketMagicHeader: String,
    responsePacketMagicHeader: String,
    underloadPacketMagicHeader: String,
    transportPacketMagicHeader: String,
}

#[allow(non_snake_case)]
#[derive(Serialize)]
struct AwgContainerConfig {
    last_config: String,
    isThirdPartyConfig: bool,
    port: String,
    transport_proto: &'static str,
}

#[derive(Serialize)]
struct ContainerEntry {
    container: &'static str,
    awg: AwgContainerConfig,
}

#[derive(Serialize)]
struct ShareableConfig<'a> {
    containers: Vec<ContainerEntry>,
    #[serde(rename = "defaultContainer")]
    default_container: &'static str,
    description: &'a str,
    dns1: &'a str,
    dns2: &'a str,
    #[serde(rename = "hostName")]
    host_name: &'a str,
    /// Marks this as a shared (not self-managed-via-SSH) config. Amnezia
    /// skips the SSH connectivity probe when `configVersion > 0`.
    #[serde(rename = "configVersion")]
    config_version: u32,
}

/// Output of [`render_vpn_uri`]: the `vpn://...` URI ready for text-paste import
/// AND the raw qCompress bytes (4-byte BE length prefix + zlib) that feed the
/// multi-QR camera-import path.  The QR path uses the qCompress bytes directly
/// (no `vpn://` prefix, no outer base64) and frames them per Amnezia's
/// `ExportController::generateQrCodeImageSeries`.
struct Payload {
    uri: String,
    qcompress_bytes: Vec<u8>,
}

fn render_vpn_uri(
    args: &Args,
    conf_text: &str,
    priv_b64: &str,
    host: &str,
    port: u16,
) -> Result<Payload> {
    let allowed = args
        .allowed_ips
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();

    let inner = AwgInnerConfig {
        config: conf_text.to_string(),
        host_name: host,
        port,
        client_priv_key: priv_b64,
        client_ip: &args.client_ip,
        server_pub_key: &args.server_pubkey,
        allowed_ips: allowed,
        junkPacketCount: args.jc.to_string(),
        junkPacketMinSize: args.jmin.to_string(),
        junkPacketMaxSize: args.jmax.to_string(),
        initPacketJunkSize: args.s1.to_string(),
        responsePacketJunkSize: args.s2.to_string(),
        initPacketMagicHeader: args.h1.to_string(),
        responsePacketMagicHeader: args.h2.to_string(),
        underloadPacketMagicHeader: args.h3.to_string(),
        transportPacketMagicHeader: args.h4.to_string(),
    };
    let inner_str = serde_json::to_string(&inner)?;

    let outer = ShareableConfig {
        containers: vec![ContainerEntry {
            container: "amnezia-awg",
            awg: AwgContainerConfig {
                last_config: inner_str,
                isThirdPartyConfig: true,
                port: port.to_string(),
                transport_proto: "udp",
            },
        }],
        default_container: "amnezia-awg",
        description: &args.description,
        dns1: &args.dns1,
        dns2: &args.dns2,
        host_name: host,
        config_version: 1,
    };

    let json_bytes = serde_json::to_vec(&outer)?;

    // Qt qCompress wire: 4-byte BE uncompressed length, then zlib(level=6).
    let mut compressed: Vec<u8> = Vec::with_capacity(json_bytes.len());
    {
        let mut enc = ZlibEncoder::new(&mut compressed, Compression::new(6));
        enc.write_all(&json_bytes)?;
        enc.finish()?;
    }
    let mut buf = Vec::with_capacity(4 + compressed.len());
    buf.extend_from_slice(&(json_bytes.len() as u32).to_be_bytes());
    buf.extend_from_slice(&compressed);

    let uri = format!("vpn://{}", URL_SAFE_NO_PAD.encode(&buf));
    Ok(Payload {
        uri,
        qcompress_bytes: buf,
    })
}

/// Render an arbitrary text payload as a single QR PNG.  Used both for the
/// legacy single-URI mode (kept for diagnostics) and for each chunk in the
/// multi-QR camera-import series.
fn render_qr_png(payload: &[u8], path: &PathBuf) -> Result<()> {
    let code = QrCode::new(payload).context("QR encode")?;
    let width = code.width();
    const MODULE: u32 = 8;
    const QUIET: u32 = 4 * MODULE;
    let dim = (width as u32) * MODULE + 2 * QUIET;
    let mut img = ImageBuffer::<Luma<u8>, _>::new(dim, dim);
    for x in 0..dim {
        for y in 0..dim {
            img.put_pixel(x, y, Luma([255]));
        }
    }
    for y in 0..width {
        for x in 0..width {
            if code[(x, y)] == qrcode::Color::Dark {
                let ox = QUIET + (x as u32) * MODULE;
                let oy = QUIET + (y as u32) * MODULE;
                for dx in 0..MODULE {
                    for dy in 0..MODULE {
                        img.put_pixel(ox + dx, oy + dy, Luma([0]));
                    }
                }
            }
        }
    }
    img.save(path).context("write png")?;
    Ok(())
}

/// Amnezia per-chunk QR payload (one PNG per chunk).
///
/// Mirror of `client/ui/controllers/exportController.cpp`
/// `ExportController::generateQrCodeImageSeries` (Amnezia client master), and
/// inverse of `ImportController::parseQrCodeChunk` in the same repo:
///
/// ```cpp
/// // exportController.cpp:344
/// double k = 850;
/// quint8 chunksCount = std::ceil(data.size() / k);
/// for (int i = 0; i < data.size(); i = i + k) {
///     QByteArray chunk;
///     QDataStream s(&chunk, QIODevice::WriteOnly);
///     s << amnezia::qrMagicCode    // qint16 = 1984, big-endian
///       << chunksCount             // quint8
///       << (quint8)std::round(i / k)
///       << data.mid(i, k);         // QByteArray: quint32 BE length + raw bytes
///     QByteArray ba = chunk.toBase64(QByteArray::Base64UrlEncoding
///                                    | QByteArray::OmitTrailingEquals);
///     // QR-encode `ba` as text, ECC LOW
/// }
/// ```
///
/// `data` is the qCompress'd JSON: `[4-byte BE uncompressed length][zlib data]`
/// — i.e. the **same bytes that get base64-url-encoded for the `vpn://` URI**,
/// minus the URI base64 and prefix.  `QDataStream` defaults to big-endian and
/// length-prefixes `QByteArray` with a 4-byte BE length.
const QR_CHUNK_SIZE: usize = 850;
const QR_MAGIC_CODE: i16 = 1984;

fn build_qr_chunk_frame(magic: i16, total: u8, index: u8, payload: &[u8]) -> Vec<u8> {
    // QDataStream default = big-endian, version 12 (Qt 5.x). QByteArray on the
    // wire is `quint32 BE length` followed by raw bytes (only special case is
    // null arrays which use 0xFFFFFFFF; we never hit that here).
    let mut buf = Vec::with_capacity(2 + 1 + 1 + 4 + payload.len());
    buf.extend_from_slice(&magic.to_be_bytes());
    buf.push(total);
    buf.push(index);
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Write one PNG per Amnezia QR chunk into `out_dir`.  Filenames are 1-indexed
/// (`phone-qr-1.png`, `phone-qr-2.png`, ...).  Returns the number of chunks.
fn render_qr_chunks(qcompress_bytes: &[u8], out_dir: &PathBuf) -> Result<usize> {
    let total_chunks = qcompress_bytes.len().div_ceil(QR_CHUNK_SIZE);
    if total_chunks > u8::MAX as usize {
        anyhow::bail!(
            "payload too large: {} chunks > {} (Amnezia chunksCount is quint8)",
            total_chunks,
            u8::MAX
        );
    }
    let total_u8 = total_chunks as u8;

    for (idx, chunk) in qcompress_bytes.chunks(QR_CHUNK_SIZE).enumerate() {
        let framed = build_qr_chunk_frame(QR_MAGIC_CODE, total_u8, idx as u8, chunk);
        let b64 = URL_SAFE_NO_PAD.encode(&framed);
        let file_name = format!("phone-qr-{}.png", idx + 1);
        render_qr_png(b64.as_bytes(), &out_dir.join(&file_name))
            .with_context(|| format!("write chunk {} ({})", idx + 1, file_name))?;
    }

    Ok(total_chunks)
}

fn main() -> Result<()> {
    let args = Args::parse();
    let (host, port) = parse_endpoint(&args.endpoint)?;

    let (priv_b64, generated_pub) = match &args.client_priv {
        Some(p) => (p.clone(), derive_pubkey_b64(p)?),
        None => x25519_keypair_b64(),
    };

    fs::create_dir_all(&args.output_dir).context("mkdir output_dir")?;

    let conf = render_conf(&args, &priv_b64, &host, port);
    fs::write(args.output_dir.join("phone.conf"), &conf).context("write .conf")?;

    let payload = render_vpn_uri(&args, &conf, &priv_b64, &host, port)?;
    fs::write(args.output_dir.join("vpn-uri.txt"), &payload.uri).context("write uri")?;

    let chunk_count = render_qr_chunks(&payload.qcompress_bytes, &args.output_dir)
        .context("write QR chunk series")?;

    // Back-compat: keep a `phone-qr.png` alongside `phone-qr-1.png` so older
    // tooling that points at the single filename still finds the first chunk.
    fs::copy(
        args.output_dir.join("phone-qr-1.png"),
        args.output_dir.join("phone-qr.png"),
    )
    .context("copy phone-qr-1.png -> phone-qr.png")?;

    eprintln!("[awg-share-gen] wrote files to {:?}", args.output_dir);
    eprintln!("[awg-share-gen] client pub (for terminator peer table): {generated_pub}");
    eprintln!("[awg-share-gen] URI length: {} chars", payload.uri.len());
    eprintln!(
        "[awg-share-gen] QR chunks: {} (phone-qr-1.png .. phone-qr-{}.png, {} bytes each, {} qCompress total)",
        chunk_count,
        chunk_count,
        QR_CHUNK_SIZE,
        payload.qcompress_bytes.len()
    );
    Ok(())
}
