//! Config import — load a Mirage ClientConfig from a source, validate it, and
//! return a short summary. The full schema lives in mirage2app/CLAUDE.md; this
//! crate is isolated (no client-core dep), so we validate via serde_json and
//! keep the raw text to hand to sentinel later.
//!
//! Sources: file, clipboard text, clipboard-QR-image (real); camera-QR is
//! stubbed in the UI for now (camera comes next).

/// A validated import: the raw config JSON + a one-line summary.
pub struct Imported {
    pub raw: String,
    pub summary: String,
}

/// Parse + validate a config blob.
pub fn parse(raw: &str) -> Result<Imported, String> {
    let v: serde_json::Value = serde_json::from_str(raw).map_err(|e| format!("не JSON: {e}"))?;
    let mesh = v.get("mesh_id").and_then(|x| x.as_str());
    let chains = v.get("chains").and_then(|x| x.as_array());
    match (mesh, chains) {
        (Some(m), Some(c)) => {
            let exit = c
                .first()
                .and_then(|ch| ch.get("hops"))
                .and_then(|h| h.as_array())
                .and_then(|h| h.last())
                .and_then(|hop| hop.get("node_id").or_else(|| hop.get("addr")))
                .and_then(|x| x.as_str())
                .unwrap_or("?");
            Ok(Imported {
                raw: raw.to_string(),
                summary: format!("mesh {m} · chains {} · exit {exit}", c.len()),
            })
        }
        _ => Err("в конфиге нет mesh_id / chains".into()),
    }
}

/// Pick a file via the native dialog and parse it. Blocking (modal dialog).
pub fn from_file() -> Result<Imported, String> {
    let path = rfd::FileDialog::new()
        .add_filter("Mirage config", &["json", "txt", "mirage"])
        .set_title("Import Mirage config")
        .pick_file()
        .ok_or_else(|| "отменено".to_string())?;
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("чтение файла: {e}"))?;
    parse(&raw)
}

/// Read the clipboard text and parse it.
pub fn from_clipboard_text() -> Result<Imported, String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| format!("буфер недоступен: {e}"))?;
    let raw = cb.get_text().map_err(|e| format!("в буфере нет текста: {e}"))?;
    parse(&raw)
}

/// Read a QR-code image from the clipboard, decode it, and parse the payload.
pub fn from_clipboard_qr() -> Result<Imported, String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| format!("буфер недоступен: {e}"))?;
    let img = cb
        .get_image()
        .map_err(|e| format!("в буфере нет картинки: {e}"))?;
    let raw = decode_qr_rgba(&img.bytes, img.width, img.height)?;
    parse(&raw)
}

/// Decode the first QR code found in an RGBA8 buffer to its text payload.
/// `rqrr` works straight off a grayscale closure — no `image` codec dep needed.
pub fn decode_qr_rgba(rgba: &[u8], w: usize, h: usize) -> Result<String, String> {
    if w == 0 || h == 0 || rgba.len() < w * h * 4 {
        return Err("битый буфер картинки".into());
    }
    let mut prep = rqrr::PreparedImage::prepare_from_greyscale(w, h, |x, y| {
        let i = (y * w + x) * 4;
        // BT.601 luma from RGB; ignore alpha.
        let (r, g, b) = (rgba[i] as u32, rgba[i + 1] as u32, rgba[i + 2] as u32);
        ((r * 299 + g * 587 + b * 114) / 1000) as u8
    });
    let grids = prep.detect_grids();
    let grid = grids.first().ok_or("QR-код не найден на картинке")?;
    let (_meta, content) = grid.decode().map_err(|e| format!("QR не читается: {e}"))?;
    Ok(content)
}
