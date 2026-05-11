//! Plain QR encoder for vless:// / hysteria2:// / vmess:// share URIs.
//! Single QR per URI — v2rayTun and friends read it as text.
//!
//! Usage:
//!   v2ray-share-gen --uri 'vless://...' --out ./vless.png
//!   v2ray-share-gen --uri-file uri.txt   --out ./qr.png

use anyhow::{Context, Result};
use clap::Parser;
use image::{ImageBuffer, Luma};
use qrcode::QrCode;
use std::{fs, path::PathBuf};

#[derive(Parser, Debug)]
#[command(version, about = "Plain-text QR generator for VPN share URIs")]
struct Args {
    /// URI string to encode (mutually exclusive with --uri-file).
    #[arg(long)]
    uri: Option<String>,

    /// Read URI from a text file (trimmed). Mutually exclusive with --uri.
    #[arg(long)]
    uri_file: Option<PathBuf>,

    /// Output PNG path.
    #[arg(long)]
    out: PathBuf,

    /// Pixels per QR module.
    #[arg(long, default_value_t = 8)]
    module: u32,
}

fn render_qr_png(payload: &[u8], path: &PathBuf, module: u32) -> Result<()> {
    let code = QrCode::new(payload).context("QR encode")?;
    let width = code.width();
    let quiet = 4 * module;
    let dim = (width as u32) * module + 2 * quiet;
    let mut img = ImageBuffer::<Luma<u8>, _>::new(dim, dim);
    for x in 0..dim {
        for y in 0..dim {
            img.put_pixel(x, y, Luma([255]));
        }
    }
    for y in 0..width {
        for x in 0..width {
            if code[(x, y)] == qrcode::Color::Dark {
                let ox = quiet + (x as u32) * module;
                let oy = quiet + (y as u32) * module;
                for dx in 0..module {
                    for dy in 0..module {
                        img.put_pixel(ox + dx, oy + dy, Luma([0]));
                    }
                }
            }
        }
    }
    img.save(path).context("write png")?;
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();

    let uri = match (&args.uri, &args.uri_file) {
        (Some(u), None) => u.trim().to_string(),
        (None, Some(p)) => fs::read_to_string(p)
            .with_context(|| format!("read {:?}", p))?
            .trim()
            .to_string(),
        (Some(_), Some(_)) => anyhow::bail!("--uri and --uri-file are mutually exclusive"),
        (None, None) => anyhow::bail!("need --uri or --uri-file"),
    };

    if let Some(parent) = args.out.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).ok();
        }
    }

    render_qr_png(uri.as_bytes(), &args.out, args.module)?;
    eprintln!(
        "[v2ray-share-gen] wrote {} ({} chars)",
        args.out.display(),
        uri.len()
    );
    Ok(())
}
