//! Supervisor persistence: peer_table snapshot on disk.

use std::io;
use std::path::{Path, PathBuf};

const FILENAME: &str = "last-peer-table.json";

fn path(state_dir: &Path) -> PathBuf {
    state_dir.join(FILENAME)
}

/// Atomically write peer_table JSON to `${state_dir}/last-peer-table.json`.
pub fn save_peer_table(state_dir: &Path, json: &str) -> io::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let target = path(state_dir);
    let tmp = target.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &target)
}

/// Load the last-known peer_table JSON. Returns None on missing/IO err.
pub fn load_peer_table(state_dir: &Path) -> Option<String> {
    std::fs::read_to_string(path(state_dir)).ok()
}
