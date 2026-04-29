//! IPC protocol for supervisor ↔ worker communication.
//!
//! JSON-line framing over duplex byte stream.
//! Transport-agnostic: works over named pipe (Windows) and Unix socket (Linux/macOS).

use serde::{Deserialize, Serialize};

pub mod pipe;
pub mod transport;

pub use transport::{IpcReader, IpcWriter};

/// Messages sent from supervisor to worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SupervisorMsg {
    /// Start serving — worker should build pool, bind SOCKS5, signal Ready.
    Start {
        chain_id: String,
        socks5_bind: String,
        config_path: String,
    },
    /// Stop accepting new connections, drain existing.
    Drain { timeout_secs: u64 },
    /// Force exit immediately.
    Shutdown,
    /// Status ping.
    Ping,
    /// Supervisor acknowledges reattach — continue current state.
    Continue,
    /// Supervisor requests worker to drain and exit (will be respawned fresh).
    Restart { reason: String },
}

/// Messages sent from worker to supervisor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerMsg {
    /// Worker has bound its SOCKS5 listener and pool is ready.
    Ready { chain_id: String, pid: u32 },
    /// Worker is draining (after Drain message).
    Draining {
        chain_id: String,
        active_connections: u32,
    },
    /// Worker has fully drained and is about to exit.
    Drained { chain_id: String },
    /// Status snapshot (reply to Ping).
    Status {
        chain_id: String,
        active_connections: u32,
        total_connections: u64,
        state: WorkerState,
    },
    /// Worker encountered fatal error.
    Error { chain_id: String, message: String },
    /// First message after every supervisor connection (initial + reattach).
    Hello {
        chain_id: String,
        pid: u32,
        started_at_unix: u64,
        version: String,
        state: WorkerState,
    },
}

/// Worker lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    Starting,
    Ready,
    Draining,
    Exiting,
}

/// Read one JSON-line message from `reader`. Returns `None` on EOF.
pub async fn read_msg<R, T>(reader: &mut R) -> std::io::Result<Option<T>>
where
    R: tokio::io::AsyncBufReadExt + Unpin,
    T: serde::de::DeserializeOwned,
{
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Ok(None);
    }
    let trimmed = line.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        return Ok(None);
    }
    let msg = serde_json::from_str(trimmed).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("json parse error: {e}"),
        )
    })?;
    Ok(Some(msg))
}

/// Write one JSON-line message to `writer`.
pub async fn write_msg<W, T>(writer: &mut W, msg: &T) -> std::io::Result<()>
where
    W: tokio::io::AsyncWriteExt + Unpin,
    T: serde::Serialize,
{
    let json = serde_json::to_string(msg).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("json serialize error: {e}"),
        )
    })?;
    writer.write_all(json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

/// Returns `\\.\pipe\mirage-cli2-{chain_id}` on Windows,
/// `${TEMP}/mirage-cli2/{chain_id}.sock` on Unix.
pub fn pipe_path(chain_id: &str) -> String {
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\mirage-cli2-{chain_id}")
    }
    #[cfg(not(windows))]
    {
        let tmp = std::env::temp_dir();
        tmp.join("mirage-cli2")
            .join(format!("{chain_id}.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

/// Returns `${TEMP}/mirage-cli2/{chain_id}.pid`.
pub fn pid_file_path(chain_id: &str) -> std::path::PathBuf {
    std::env::temp_dir()
        .join("mirage-cli2")
        .join(format!("{chain_id}.pid"))
}
