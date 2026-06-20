//! Config import — load a Mirage ClientConfig from a source, validate it, and
//! return a short summary. The full schema lives in mirage2app/CLAUDE.md; this
//! crate is isolated (no client-core dep), so we validate via serde_json and
//! keep the raw text to hand to sentinel later.
//!
//! Sources: file, clipboard text (real); clipboard-QR-image and camera-QR are
//! stubbed in the UI for now (QR decode + camera come next).

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
