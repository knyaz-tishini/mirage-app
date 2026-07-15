//! PT-v1 managed-transport IPC — env parsing + stdout protocol lines.
//!
//! Grounded in `docs/mirage/audits/arc3-deep-tor-in-wire-2026-07-15.md` §3.2
//! and the arc3 Phase-0 contract §2/§11. No external PT library: the
//! protocol is ~10 env vars + ~4 stdout line formats, hand-rolled here.
//!
//! stdout is reserved for these protocol lines — Tor parses it line by
//! line. Nothing else in this crate may write to stdout; `main.rs` routes
//! all logging to stderr for exactly this reason.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use tokio::io::{AsyncWrite, AsyncWriteExt};

/// The only transport name this adapter recognizes in
/// `TOR_PT_CLIENT_TRANSPORTS` / `TOR_PT_SERVER_TRANSPORTS`.
pub const TRANSPORT_NAME: &str = "mirage";

/// Parsed PT-v1 managed-transport environment (contract §2).
#[derive(Debug, Clone)]
pub struct ManagedTransportEnv {
    /// `TOR_PT_MANAGED_TRANSPORT_VER` — validated to contain `"1"`.
    pub version: String,
    /// `TOR_PT_STATE_LOCATION` — this PT's private state directory.
    pub state_location: PathBuf,
    /// `TOR_PT_EXIT_ON_STDIN_CLOSE` — defaults to `false` if unset.
    pub exit_on_stdin_close: bool,
    pub mode: PtMode,
}

/// Which side Tor launched this adapter as, plus that side's env vars.
#[derive(Debug, Clone)]
pub enum PtMode {
    Client {
        /// `TOR_PT_CLIENT_TRANSPORTS`, comma-split, untrimmed of unknown
        /// names — [`validate_transports`] is the fallible filter, kept
        /// separate so both `main.rs` and unit tests can exercise it
        /// without needing a full env parse.
        transports: Vec<String>,
        /// `TOR_PT_PROXY` — spike: logged, never dialled through.
        proxy: Option<String>,
    },
    Server {
        /// `TOR_PT_SERVER_TRANSPORTS`, comma-split (see `Client::transports`).
        transports: Vec<String>,
        /// `TOR_PT_ORPORT` — the real (loopback) Tor ORPort.
        or_port: SocketAddr,
        /// `TOR_PT_EXTENDED_SERVER_PORT` — spike: unused (contract §4).
        ext_or_port: Option<SocketAddr>,
        /// `TOR_PT_AUTH_COOKIE_FILE` — spike: unused (only meaningful with
        /// `ext_or_port`).
        auth_cookie_file: Option<PathBuf>,
        /// `TOR_PT_SERVER_BINDADDR`, `"name-addr,name-addr"` — spike:
        /// overridden by the adapter's own fixed forwarder bind addr
        /// (contract §4 note), kept here for visibility/logging only.
        bind_addrs: HashMap<String, SocketAddr>,
        /// `TOR_PT_SERVER_TRANSPORT_OPTIONS` — spike: parsed, unused.
        transport_options: Option<String>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum PtIpcError {
    #[error("missing required env var {0}")]
    MissingEnv(&'static str),
    #[error("unsupported managed-transport version, want \"1\", got {0:?}")]
    UnsupportedVersion(String),
    #[error("invalid socket address in {0}: {1}")]
    BadAddr(&'static str, std::net::AddrParseError),
    #[error("neither TOR_PT_CLIENT_TRANSPORTS nor TOR_PT_SERVER_TRANSPORTS set — not launched by Tor")]
    NoMode,
    #[error("transport {0:?} not recognized (this adapter only speaks \"mirage\")")]
    UnknownTransport(String),
    #[error("malformed TOR_PT_SERVER_BINDADDR entry {0:?}")]
    BadBindAddr(String),
    #[error("stdout io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Parse the real process environment. The actual field-by-field logic
/// lives in [`parse_from`] so it can be unit-tested against a fixed map
/// instead of mutating global process env (flaky under parallel tests).
pub fn parse_env() -> Result<ManagedTransportEnv, PtIpcError> {
    parse_from(|name| std::env::var(name).ok())
}

/// Core parse logic, parameterised over an env-var lookup closure.
fn parse_from(get: impl Fn(&str) -> Option<String>) -> Result<ManagedTransportEnv, PtIpcError> {
    let version = get("TOR_PT_MANAGED_TRANSPORT_VER")
        .ok_or(PtIpcError::MissingEnv("TOR_PT_MANAGED_TRANSPORT_VER"))?;
    if !version.split(',').any(|v| v.trim() == "1") {
        return Err(PtIpcError::UnsupportedVersion(version));
    }

    let state_location = PathBuf::from(
        get("TOR_PT_STATE_LOCATION").ok_or(PtIpcError::MissingEnv("TOR_PT_STATE_LOCATION"))?,
    );

    let exit_on_stdin_close = get("TOR_PT_EXIT_ON_STDIN_CLOSE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let mode = if let Some(raw) = get("TOR_PT_CLIENT_TRANSPORTS") {
        PtMode::Client {
            transports: parse_transport_list(&raw),
            proxy: get("TOR_PT_PROXY"),
        }
    } else if let Some(raw) = get("TOR_PT_SERVER_TRANSPORTS") {
        let or_port_raw = get("TOR_PT_ORPORT").ok_or(PtIpcError::MissingEnv("TOR_PT_ORPORT"))?;
        let or_port: SocketAddr = or_port_raw
            .parse()
            .map_err(|e| PtIpcError::BadAddr("TOR_PT_ORPORT", e))?;

        let ext_or_port = match get("TOR_PT_EXTENDED_SERVER_PORT") {
            Some(v) if !v.is_empty() => Some(
                v.parse::<SocketAddr>()
                    .map_err(|e| PtIpcError::BadAddr("TOR_PT_EXTENDED_SERVER_PORT", e))?,
            ),
            _ => None,
        };

        let auth_cookie_file = get("TOR_PT_AUTH_COOKIE_FILE").map(PathBuf::from);
        let bind_addrs = parse_bind_addrs(get("TOR_PT_SERVER_BINDADDR").as_deref().unwrap_or(""))?;
        let transport_options = get("TOR_PT_SERVER_TRANSPORT_OPTIONS");

        PtMode::Server {
            transports: parse_transport_list(&raw),
            or_port,
            ext_or_port,
            auth_cookie_file,
            bind_addrs,
            transport_options,
        }
    } else {
        return Err(PtIpcError::NoMode);
    };

    Ok(ManagedTransportEnv {
        version,
        state_location,
        exit_on_stdin_close,
        mode,
    })
}

/// Pure comma-split, trimmed, empties dropped. No name filtering here —
/// that is [`validate_transports`]'s job, kept separate so a caller can
/// report the SPECIFIC bad name back to Tor via `CMETHOD-ERROR`/
/// `SMETHOD-ERROR` instead of failing the whole env parse blind.
fn parse_transport_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// `"name-addr,name-addr"` → `{name: addr}`. Address itself may contain
/// `:` (host:port) but never `-`, so splitting on the first `-` is safe.
fn parse_bind_addrs(raw: &str) -> Result<HashMap<String, SocketAddr>, PtIpcError> {
    let mut out = HashMap::new();
    if raw.is_empty() {
        return Ok(out);
    }
    for entry in raw.split(',') {
        let (name, addr) = entry
            .split_once('-')
            .ok_or_else(|| PtIpcError::BadBindAddr(entry.to_string()))?;
        let addr: SocketAddr = addr
            .parse()
            .map_err(|_| PtIpcError::BadBindAddr(entry.to_string()))?;
        out.insert(name.to_string(), addr);
    }
    Ok(out)
}

/// Reject any transport name this adapter does not speak. Per contract
/// §2: "Any other name in the comma-list → `CMETHOD-ERROR <name>
/// no-such-transport` / equivalent server-side, never silently accepted."
/// Callers use the returned name to format that exact line.
pub fn validate_transports(transports: &[String]) -> Result<(), PtIpcError> {
    for t in transports {
        if t != TRANSPORT_NAME {
            return Err(PtIpcError::UnknownTransport(t.clone()));
        }
    }
    Ok(())
}

/// Line-buffered PT-v1 stdout protocol writer. Every method writes exactly
/// one `\n`-terminated line and flushes immediately — Tor reads stdout
/// line-by-line and a buffered-but-unflushed line looks identical to a
/// hung PT from Tor's side.
pub struct PtStdout<W: AsyncWrite + Unpin> {
    out: W,
}

impl<W: AsyncWrite + Unpin> PtStdout<W> {
    pub fn new(out: W) -> Self {
        Self { out }
    }

    async fn write_line(&mut self, line: &str) -> Result<(), PtIpcError> {
        self.out.write_all(line.as_bytes()).await?;
        self.out.write_all(b"\n").await?;
        self.out.flush().await?;
        Ok(())
    }

    pub async fn version(&mut self, v: &str) -> Result<(), PtIpcError> {
        self.write_line(&format!("VERSION {v}")).await
    }

    pub async fn cmethod(&mut self, name: &str, addr: SocketAddr) -> Result<(), PtIpcError> {
        self.write_line(&format!("CMETHOD {name} socks5 {addr}")).await
    }

    pub async fn cmethod_error(&mut self, name: &str, reason: &str) -> Result<(), PtIpcError> {
        self.write_line(&format!("CMETHOD-ERROR {name} {reason}")).await
    }

    pub async fn cmethods_done(&mut self) -> Result<(), PtIpcError> {
        self.write_line("CMETHODS DONE").await
    }

    pub async fn smethod(&mut self, name: &str, addr: SocketAddr) -> Result<(), PtIpcError> {
        self.write_line(&format!("SMETHOD {name} {addr}")).await
    }

    pub async fn smethod_error(&mut self, name: &str, reason: &str) -> Result<(), PtIpcError> {
        self.write_line(&format!("SMETHOD-ERROR {name} {reason}")).await
    }

    pub async fn smethods_done(&mut self) -> Result<(), PtIpcError> {
        self.write_line("SMETHODS DONE").await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(vars: &HashMap<&'static str, &'static str>) -> impl Fn(&str) -> Option<String> + '_ {
        move |name| vars.get(name).map(|v| v.to_string())
    }

    #[test]
    fn env_parse_round_trip_client() {
        let mut vars = HashMap::new();
        vars.insert("TOR_PT_MANAGED_TRANSPORT_VER", "1");
        vars.insert("TOR_PT_STATE_LOCATION", "/tmp/pt-state");
        vars.insert("TOR_PT_CLIENT_TRANSPORTS", "mirage");
        vars.insert("TOR_PT_EXIT_ON_STDIN_CLOSE", "1");

        let env = parse_from(lookup(&vars)).expect("parse ok");
        assert_eq!(env.version, "1");
        assert_eq!(env.state_location, PathBuf::from("/tmp/pt-state"));
        assert!(env.exit_on_stdin_close);
        match env.mode {
            PtMode::Client { transports, proxy } => {
                assert_eq!(transports, vec!["mirage".to_string()]);
                assert_eq!(proxy, None);
            }
            PtMode::Server { .. } => panic!("expected client mode"),
        }
    }

    #[test]
    fn env_parse_round_trip_server() {
        let mut vars = HashMap::new();
        vars.insert("TOR_PT_MANAGED_TRANSPORT_VER", "1");
        vars.insert("TOR_PT_STATE_LOCATION", "/tmp/pt-state");
        vars.insert("TOR_PT_SERVER_TRANSPORTS", "mirage");
        vars.insert("TOR_PT_ORPORT", "127.0.0.1:9001");
        vars.insert("TOR_PT_SERVER_BINDADDR", "mirage-203.0.113.10:38001");

        let env = parse_from(lookup(&vars)).expect("parse ok");
        match env.mode {
            PtMode::Server {
                transports,
                or_port,
                bind_addrs,
                ..
            } => {
                assert_eq!(transports, vec!["mirage".to_string()]);
                assert_eq!(or_port, "127.0.0.1:9001".parse::<SocketAddr>().expect("literal addr"));
                assert_eq!(
                    bind_addrs.get("mirage"),
                    Some(&"203.0.113.10:38001".parse::<SocketAddr>().expect("literal addr"))
                );
            }
            PtMode::Client { .. } => panic!("expected server mode"),
        }
    }

    #[test]
    fn env_parse_no_mode_env_set() {
        let vars = HashMap::new();
        let err = parse_from(lookup(&vars)).expect_err("must fail with no mode env set");
        assert!(matches!(err, PtIpcError::NoMode));
    }

    #[test]
    fn env_parse_unsupported_version() {
        let mut vars = HashMap::new();
        vars.insert("TOR_PT_MANAGED_TRANSPORT_VER", "2");
        vars.insert("TOR_PT_STATE_LOCATION", "/tmp/pt-state");
        vars.insert("TOR_PT_CLIENT_TRANSPORTS", "mirage");

        let err = parse_from(lookup(&vars)).expect_err("must reject unsupported version");
        assert!(matches!(err, PtIpcError::UnsupportedVersion(v) if v == "2"));
    }

    #[test]
    fn validate_transports_rejects_unknown_name() {
        let transports = vec!["obfs4".to_string()];
        let err = validate_transports(&transports).expect_err("must reject unknown transport");
        match err {
            PtIpcError::UnknownTransport(name) => assert_eq!(name, "obfs4"),
            other => panic!("expected UnknownTransport, got {other:?}"),
        }
    }

    #[test]
    fn validate_transports_accepts_mirage() {
        let transports = vec!["mirage".to_string()];
        validate_transports(&transports).expect("mirage must be accepted");
    }

    #[tokio::test]
    async fn stdout_line_formatting_client() {
        let (mut reader, writer) = tokio::io::duplex(4096);
        let mut ipc = PtStdout::new(writer);

        ipc.version("1").await.expect("version line");
        let addr: SocketAddr = "127.0.0.1:9050".parse().expect("literal addr");
        ipc.cmethod(TRANSPORT_NAME, addr).await.expect("cmethod line");
        ipc.cmethods_done().await.expect("cmethods done line");
        drop(ipc);

        let mut buf = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut buf)
            .await
            .expect("read back duplex");
        let text = String::from_utf8(buf).expect("utf8 output");
        assert_eq!(
            text,
            "VERSION 1\nCMETHOD mirage socks5 127.0.0.1:9050\nCMETHODS DONE\n"
        );
    }

    #[tokio::test]
    async fn stdout_line_formatting_server_error() {
        let (mut reader, writer) = tokio::io::duplex(4096);
        let mut ipc = PtStdout::new(writer);

        ipc.smethod_error(TRANSPORT_NAME, "bind failed: address in use")
            .await
            .expect("smethod-error line");
        ipc.smethods_done().await.expect("smethods done line");
        drop(ipc);

        let mut buf = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut buf)
            .await
            .expect("read back duplex");
        let text = String::from_utf8(buf).expect("utf8 output");
        assert_eq!(
            text,
            "SMETHOD-ERROR mirage bind failed: address in use\nSMETHODS DONE\n"
        );
    }
}
