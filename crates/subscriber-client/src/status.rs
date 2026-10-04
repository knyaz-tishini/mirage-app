//! `edge-status.json` — the connector's machine-readable state, published for
//! whoever supervises it (the sentinel, and through it the GUI).
//!
//! The file is the whole contract; there is no listener. It is rewritten
//! atomically (temp file in the same directory, then rename) on every
//! supervisor transition and every [`STATUS_REFRESH_INTERVAL`], so a reader
//! never sees a torn document and can judge liveness by `updated_at_ms`:
//!
//! ```json
//! {"v":1,"updated_at_ms":1727700000000,"state":"connected","reason":null,
//!  "exit":{"requested":"auto","accepted":"NL"},"countries":["NL","DE"]}
//! ```
//!
//! `reason` is a closed enum ([`EdgeReason`]) — never free text and never an
//! address. Dial errors carry the entrance address in their `Display`; the
//! only path from an [`EdgeDialError`] into this file is
//! [`EdgeReason::from`], which cannot forward any of it. On unix the file is
//! created `0600`; on Windows it relies on the default ACL of the per-user
//! state directory it lives in.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mirage_control_plane::{CountryCode, EdgeRefusal, ExitPreference};
use mirage_core::error::MirageError;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::session::{EdgeDialError, RefusalDisplay};

/// Schema version written into every status file (`"v"`).
pub const STATUS_SCHEMA_VERSION: u32 = 1;

/// How often the writer re-stamps and rewrites the file even when nothing
/// changed — the heartbeat a reader uses to tell a live connector from a
/// stale file.
pub const STATUS_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Where the connector is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeState {
    /// No session has been established yet; dialing.
    Connecting,
    /// A live session to the edge is up.
    Connected,
    /// A session existed and was lost (or a redial failed); dialing again.
    Reconnecting,
    /// The edge answered and refused this client by name.
    Refused,
    /// The connector cannot work without outside action (bad artefact).
    Fatal,
}

/// Why the connector is not (yet) connected. A closed set: a reader can
/// switch on it and the file can never leak an address or an error string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeReason {
    /// The entrance could not be reached (connect failed, timed out, dropped).
    Unreachable,
    /// The entrance answered but the V9 handshake did not complete.
    HandshakeFailed,
    /// The edge closed the session before sending its `EdgeWelcome`.
    ClosedBeforeWelcome,
    /// The `EdgeWelcome` bytes did not decode.
    MalformedWelcome,
    /// The edge cannot serve the requested exit country.
    RefusedCountryUnavailable,
    /// The edge does not speak the wire version this client sent.
    RefusedUnsupportedVersion,
    /// The artefact is unusable (bad entrance address, key, country, or no
    /// dialable entrance).
    BadArtefact,
}

impl EdgeReason {
    /// The state a reason forces, if any: a named refusal is `refused`, an
    /// unusable artefact is `fatal`. Transport-level reasons force nothing —
    /// the state stays `connecting` / `reconnecting`.
    pub fn terminal_state(self) -> Option<EdgeState> {
        match self {
            Self::RefusedCountryUnavailable | Self::RefusedUnsupportedVersion => {
                Some(EdgeState::Refused)
            }
            Self::BadArtefact => Some(EdgeState::Fatal),
            Self::Unreachable
            | Self::HandshakeFailed
            | Self::ClosedBeforeWelcome
            | Self::MalformedWelcome => None,
        }
    }
}

impl From<&EdgeDialError> for EdgeReason {
    /// Collapse a dial error to its closed reason code. Nothing from the
    /// error's payload (entrance address, io text, requested country) is
    /// carried across.
    fn from(err: &EdgeDialError) -> Self {
        match err {
            EdgeDialError::BadAddr { .. }
            | EdgeDialError::BadServerPubkey(_)
            | EdgeDialError::BadCountryCode(_)
            | EdgeDialError::NoEntrances => Self::BadArtefact,
            EdgeDialError::Mirage(e) => match e {
                MirageError::Io(_)
                | MirageError::NodeUnreachable(_)
                | MirageError::Timeout
                | MirageError::HeartbeatTimeout => Self::Unreachable,
                // Every other failure inside the dial is a handshake that did
                // not complete (crypto, auth, protocol, handshake timeout, …).
                _ => Self::HandshakeFailed,
            },
            EdgeDialError::MalformedWelcome(_) => Self::MalformedWelcome,
            EdgeDialError::ClosedBeforeWelcome => Self::ClosedBeforeWelcome,
            EdgeDialError::Refused(RefusalDisplay(EdgeRefusal::CountryUnavailable { .. })) => {
                Self::RefusedCountryUnavailable
            }
            EdgeDialError::Refused(RefusalDisplay(EdgeRefusal::UnsupportedVersion { .. })) => {
                Self::RefusedUnsupportedVersion
            }
        }
    }
}

/// The exit the client asked for and the exit the edge fulfils.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitStatus {
    /// `"auto"` or an ISO alpha-2 code — what the artefact asked for.
    pub requested: String,
    /// `"auto"` or an ISO alpha-2 code — what the live session was granted;
    /// `null` whenever no session is live.
    pub accepted: Option<String>,
}

/// One `edge-status.json` document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeStatusFile {
    /// Schema version, [`STATUS_SCHEMA_VERSION`].
    pub v: u32,
    /// Unix milliseconds of the last (re)write — the heartbeat.
    pub updated_at_ms: u64,
    /// Where the connector is in its life.
    pub state: EdgeState,
    /// Why it is not connected, when there is a named cause.
    pub reason: Option<EdgeReason>,
    /// Requested vs granted exit.
    pub exit: ExitStatus,
    /// Every country the edge said it can serve on its last accepted
    /// session, in the edge's own order. Kept across a reconnect.
    pub countries: Vec<String>,
}

/// The label an [`ExitPreference`] has on the wire of this file.
pub fn exit_label(exit: &ExitPreference) -> String {
    match exit {
        ExitPreference::Auto => "auto".to_owned(),
        ExitPreference::Country(code) => code.as_str().to_owned(),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

impl EdgeStatusFile {
    fn new(state: EdgeState, reason: Option<EdgeReason>, requested: &ExitPreference) -> Self {
        Self {
            v: STATUS_SCHEMA_VERSION,
            updated_at_ms: now_ms(),
            state,
            reason,
            exit: ExitStatus { requested: exit_label(requested), accepted: None },
            countries: Vec::new(),
        }
    }

    /// The state before any dial has finished.
    pub fn connecting(requested: &ExitPreference) -> Self {
        Self::new(EdgeState::Connecting, None, requested)
    }

    /// A terminal state for a connector that cannot start. `reason` is
    /// `None` when the cause is not one of the closed [`EdgeReason`]s (for
    /// example a non-loopback bind address).
    pub fn fatal(requested: &ExitPreference, reason: Option<EdgeReason>) -> Self {
        Self::new(EdgeState::Fatal, reason, requested)
    }

    /// A session is live: record the granted exit and the country list.
    pub(crate) fn set_connected(
        &mut self,
        accepted: Option<ExitPreference>,
        countries: &[CountryCode],
    ) {
        self.updated_at_ms = now_ms();
        self.state = EdgeState::Connected;
        self.reason = None;
        self.exit.accepted = accepted.as_ref().map(exit_label);
        self.countries = countries.iter().map(|c| c.as_str().to_owned()).collect();
    }

    /// The live session closed; a redial is next. Countries are kept.
    pub(crate) fn set_reconnecting(&mut self) {
        self.updated_at_ms = now_ms();
        self.state = EdgeState::Reconnecting;
        self.reason = None;
        self.exit.accepted = None;
    }

    /// A dial attempt failed. A named refusal is `refused`, an unusable
    /// artefact is `fatal`, anything else keeps the connector in
    /// `connecting` (never connected) or `reconnecting` (had a session).
    pub(crate) fn set_dial_failed(&mut self, reason: EdgeReason, ever_connected: bool) {
        self.updated_at_ms = now_ms();
        self.state = reason.terminal_state().unwrap_or(if ever_connected {
            EdgeState::Reconnecting
        } else {
            EdgeState::Connecting
        });
        self.reason = Some(reason);
        self.exit.accepted = None;
    }
}

/// Write `status` to `path` atomically: the JSON goes to a temp file in the
/// same directory (created `0600` on unix), then renames over `path`. A
/// reader sees the old document or the new one, never a partial one. Missing
/// parent directories are created.
pub fn write_atomic(path: &Path, status: &EdgeStatusFile) -> io::Result<()> {
    let json = serde_json::to_vec(status).map_err(io::Error::other)?;
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "status file path has no file name")
    })?;
    fs::create_dir_all(dir)?;

    let tmp = dir.join(format!("{}.{}.tmp", file_name.to_string_lossy(), std::process::id()));
    let result = write_tmp_then_rename(&tmp, path, &json);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn write_tmp_then_rename(tmp: &Path, dest: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.flush()?;
    drop(file);
    fs::rename(tmp, dest)
}

/// Publish a terminal `fatal` document for a connector that could not start
/// (bad artefact, refused bind). Best effort for the caller: the connector is
/// about to exit either way.
pub fn publish_fatal(
    path: &Path,
    requested: &ExitPreference,
    reason: Option<EdgeReason>,
) -> io::Result<()> {
    write_atomic(path, &EdgeStatusFile::fatal(requested, reason))
}

/// The background task that keeps `edge-status.json` current.
pub struct StatusWriter {
    path: PathBuf,
    task: JoinHandle<()>,
}

impl StatusWriter {
    /// Write the current status once, synchronously (so a stale file from a
    /// previous run is replaced before this returns and an unwritable path
    /// fails loudly), then spawn the task that rewrites it on every change of
    /// `rx` and every [`STATUS_REFRESH_INTERVAL`]. Must be called inside a
    /// tokio runtime.
    pub fn start(path: PathBuf, mut rx: watch::Receiver<EdgeStatusFile>) -> io::Result<Self> {
        let mut first = rx.borrow_and_update().clone();
        first.updated_at_ms = now_ms();
        write_atomic(&path, &first)?;

        let task_path = path.clone();
        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(STATUS_REFRESH_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick fires immediately; the initial write above
            // already covered it.
            ticker.tick().await;
            let mut healthy = true;
            loop {
                tokio::select! {
                    changed = rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                    }
                    _ = ticker.tick() => {}
                }
                let mut snapshot = rx.borrow_and_update().clone();
                snapshot.updated_at_ms = now_ms();
                // A ~1 KiB write and rename: short enough to do inline, and
                // inline means `abort()` can only land between whole writes.
                match write_atomic(&task_path, &snapshot) {
                    Ok(()) => {
                        if !healthy {
                            info!(path = %task_path.display(), "status file writable again");
                            healthy = true;
                        }
                    }
                    Err(e) => {
                        if healthy {
                            warn!(path = %task_path.display(), error = %e, "cannot write status file");
                            healthy = false;
                        }
                    }
                }
            }
        });
        Ok(Self { path, task })
    }

    /// Stop rewriting and remove the file, so a clean shutdown never leaves a
    /// stale `connected` behind. A killed process still leaves the file; that
    /// is what `updated_at_ms` is for.
    pub fn stop(&self) {
        self.task.abort();
        if let Err(e) = fs::remove_file(&self.path) {
            if e.kind() != io::ErrorKind::NotFound {
                warn!(path = %self.path.display(), error = %e, "cannot remove status file on shutdown");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};

    use mirage_control_plane::{EdgeOutcome, EdgeWelcome};
    use mirage_mux::tokio_compat::{SessionHandle, TokioIoCompat};

    use super::*;
    use crate::artefact::ArtefactError;
    use crate::session::{EdgeDialer, EdgeSession, EdgeSessionSupervisor};

    fn cc(code: &str) -> CountryCode {
        CountryCode::new(code).expect("test country code is two uppercase letters")
    }

    fn connected_sample() -> EdgeStatusFile {
        EdgeStatusFile {
            v: STATUS_SCHEMA_VERSION,
            updated_at_ms: 1_727_700_000_000,
            state: EdgeState::Connected,
            reason: None,
            exit: ExitStatus { requested: "auto".to_owned(), accepted: Some("NL".to_owned()) },
            countries: vec!["NL".to_owned(), "DE".to_owned()],
        }
    }

    fn refused_sample() -> EdgeStatusFile {
        EdgeStatusFile {
            v: STATUS_SCHEMA_VERSION,
            updated_at_ms: 1_727_700_005_000,
            state: EdgeState::Refused,
            reason: Some(EdgeReason::RefusedCountryUnavailable),
            exit: ExitStatus { requested: "ZZ".to_owned(), accepted: None },
            countries: Vec::new(),
        }
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir()
            .join(format!("mirage-e1-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn cleanup(dir: &Path, path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_dir(dir);
    }

    // ── contract snapshot ───────────────────────────────────────────────

    #[test]
    fn status_json_matches_contract_snapshot() {
        assert_eq!(
            serde_json::to_string(&connected_sample()).expect("serializes"),
            r#"{"v":1,"updated_at_ms":1727700000000,"state":"connected","reason":null,"exit":{"requested":"auto","accepted":"NL"},"countries":["NL","DE"]}"#
        );
        assert_eq!(
            serde_json::to_string(&refused_sample()).expect("serializes"),
            r#"{"v":1,"updated_at_ms":1727700005000,"state":"refused","reason":"refused_country_unavailable","exit":{"requested":"ZZ","accepted":null},"countries":[]}"#
        );

        // The closed vocabularies, spelled exactly as the contract does.
        let states = [
            (EdgeState::Connecting, "connecting"),
            (EdgeState::Connected, "connected"),
            (EdgeState::Reconnecting, "reconnecting"),
            (EdgeState::Refused, "refused"),
            (EdgeState::Fatal, "fatal"),
        ];
        for (state, name) in states {
            assert_eq!(serde_json::to_string(&state).expect("serializes"), format!("\"{name}\""));
        }
        let reasons = [
            (EdgeReason::Unreachable, "unreachable"),
            (EdgeReason::HandshakeFailed, "handshake_failed"),
            (EdgeReason::ClosedBeforeWelcome, "closed_before_welcome"),
            (EdgeReason::MalformedWelcome, "malformed_welcome"),
            (EdgeReason::RefusedCountryUnavailable, "refused_country_unavailable"),
            (EdgeReason::RefusedUnsupportedVersion, "refused_unsupported_version"),
            (EdgeReason::BadArtefact, "bad_artefact"),
        ];
        for (reason, name) in reasons {
            assert_eq!(serde_json::to_string(&reason).expect("serializes"), format!("\"{name}\""));
        }

        // A reader can parse what the writer emits.
        let round: EdgeStatusFile =
            serde_json::from_str(&serde_json::to_string(&connected_sample()).expect("serializes"))
                .expect("round-trips");
        assert_eq!(round, connected_sample());
    }

    // ── EdgeDialError -> EdgeReason, never an address ───────────────────

    #[test]
    fn dial_errors_map_to_reason_codes_without_addresses() {
        let bad_country = CountryCode::new("zz").expect_err("lowercase is refused");
        let cases: Vec<(EdgeDialError, EdgeReason)> = vec![
            (
                EdgeDialError::BadAddr {
                    addr: "203.0.113.9:443".to_owned(),
                    reason: "invalid socket address syntax".to_owned(),
                },
                EdgeReason::BadArtefact,
            ),
            (
                EdgeDialError::BadServerPubkey(ArtefactError::MalformedHex {
                    field: "entrances[0].server_pubkey_hex".to_owned(),
                    reason: "odd length".to_owned(),
                }),
                EdgeReason::BadArtefact,
            ),
            (EdgeDialError::BadCountryCode(bad_country), EdgeReason::BadArtefact),
            (EdgeDialError::NoEntrances, EdgeReason::BadArtefact),
            (
                EdgeDialError::Mirage(MirageError::Io(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "connect to 198.51.100.7:443 refused",
                ))),
                EdgeReason::Unreachable,
            ),
            (
                EdgeDialError::Mirage(MirageError::NodeUnreachable("198.51.100.7:443".to_owned())),
                EdgeReason::Unreachable,
            ),
            (EdgeDialError::Mirage(MirageError::Timeout), EdgeReason::Unreachable),
            (
                EdgeDialError::Mirage(MirageError::Handshake("bad hello from 198.51.100.7".to_owned())),
                EdgeReason::HandshakeFailed,
            ),
            (EdgeDialError::Mirage(MirageError::AuthFailed), EdgeReason::HandshakeFailed),
            (
                EdgeDialError::Mirage(MirageError::HandshakeTimedOut("198.51.100.7:443".to_owned())),
                EdgeReason::HandshakeFailed,
            ),
            (
                EdgeDialError::MalformedWelcome(CountryCode::new("1x").expect_err("digit is refused")),
                EdgeReason::MalformedWelcome,
            ),
            (EdgeDialError::ClosedBeforeWelcome, EdgeReason::ClosedBeforeWelcome),
            (
                EdgeDialError::Refused(RefusalDisplay(EdgeRefusal::CountryUnavailable {
                    requested: cc("ZZ"),
                })),
                EdgeReason::RefusedCountryUnavailable,
            ),
            (
                EdgeDialError::Refused(RefusalDisplay(EdgeRefusal::UnsupportedVersion {
                    requested: 9,
                    supported: 1,
                })),
                EdgeReason::RefusedUnsupportedVersion,
            ),
        ];

        for (err, expected) in cases {
            let rendered_err = err.to_string();
            let reason = EdgeReason::from(&err);
            assert_eq!(reason, expected, "wrong reason for: {rendered_err}");

            // Push the reason through the real state machine and serialize the
            // whole document: no address fragment, no error text.
            let mut status = EdgeStatusFile::connecting(&ExitPreference::Auto);
            status.set_dial_failed(reason, false);
            let json = serde_json::to_string(&status).expect("serializes");
            for fragment in ["203.0.113", "198.51.100", "io error", "bad hello", "odd length"] {
                assert!(!json.contains(fragment), "status leaked {fragment:?}: {json}");
            }
            let reason_json = serde_json::to_string(&reason).expect("serializes");
            assert!(
                reason_json.trim_matches('"').chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "reason must be a bare snake_case code, got {reason_json}"
            );
        }
    }

    #[test]
    fn dial_failure_picks_state_from_reason_and_history() {
        let mut status = EdgeStatusFile::connecting(&ExitPreference::Auto);

        status.set_dial_failed(EdgeReason::Unreachable, false);
        assert_eq!((status.state, status.reason), (EdgeState::Connecting, Some(EdgeReason::Unreachable)));

        status.set_dial_failed(EdgeReason::Unreachable, true);
        assert_eq!(status.state, EdgeState::Reconnecting);

        status.set_dial_failed(EdgeReason::RefusedCountryUnavailable, true);
        assert_eq!(status.state, EdgeState::Refused);

        status.set_dial_failed(EdgeReason::BadArtefact, false);
        assert_eq!(status.state, EdgeState::Fatal);

        status.set_connected(Some(ExitPreference::Country(cc("NL"))), &[cc("NL"), cc("DE")]);
        assert_eq!(status.state, EdgeState::Connected);
        assert_eq!(status.reason, None);
        assert_eq!(status.exit.accepted.as_deref(), Some("NL"));
        assert_eq!(status.countries, ["NL", "DE"]);

        status.set_reconnecting();
        assert_eq!((status.state, status.reason), (EdgeState::Reconnecting, None));
        assert_eq!(status.exit.accepted, None, "no live session, no granted exit");
        assert_eq!(status.countries, ["NL", "DE"], "the last known list survives a reconnect");
    }

    // ── atomic writer ───────────────────────────────────────────────────

    #[test]
    fn status_writer_is_atomic() {
        let dir = scratch_dir("atomic");
        let path = dir.join("edge-status.json");

        write_atomic(&path, &connected_sample()).expect("initial write");
        assert_eq!(
            fs::read_to_string(&path).expect("read back"),
            serde_json::to_string(&connected_sample()).expect("serializes"),
            "the file holds exactly the compact contract JSON"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "status file must be owner-only on unix");
        }

        // A reader hammers the file while the writer alternates between a
        // ~1.5 KiB document and a tiny one. An in-place truncate+write would
        // expose an empty or partial file; rename must not.
        let stop = Arc::new(AtomicBool::new(false));
        let good_reads = Arc::new(AtomicUsize::new(0));
        let torn_reads = Arc::new(AtomicUsize::new(0));
        let reader = {
            let (stop, good_reads, torn_reads, path) =
                (Arc::clone(&stop), Arc::clone(&good_reads), Arc::clone(&torn_reads), path.clone());
            std::thread::spawn(move || loop {
                // A transient open error (Windows mid-rename) is not a torn
                // document; only bytes that fail to parse are.
                if let Ok(bytes) = fs::read(&path) {
                    if serde_json::from_slice::<EdgeStatusFile>(&bytes).is_ok() {
                        good_reads.fetch_add(1, Ordering::Relaxed);
                    } else {
                        torn_reads.fetch_add(1, Ordering::Relaxed);
                    }
                }
                if stop.load(Ordering::Relaxed) {
                    break;
                }
            })
        };

        let big: Vec<String> = ('A'..='Z')
            .flat_map(|a| ('A'..='Z').map(move |b| format!("{a}{b}")))
            .take(250)
            .collect();
        let mut last = connected_sample();
        for i in 0..300u64 {
            last.updated_at_ms = 1_727_700_000_000 + i;
            last.countries = if i % 2 == 0 { big.clone() } else { Vec::new() };
            write_atomic(&path, &last).expect("rewrite");
        }
        stop.store(true, Ordering::Relaxed);
        reader.join().expect("reader thread must not panic");

        assert_eq!(torn_reads.load(Ordering::Relaxed), 0, "a reader saw a torn status file");
        assert!(good_reads.load(Ordering::Relaxed) > 0, "the reader never saw a document");
        assert_eq!(
            serde_json::from_slice::<EdgeStatusFile>(&fs::read(&path).expect("final read"))
                .expect("final document parses"),
            last
        );
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .expect("list scratch dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(leftovers.len(), 1, "no temp file may survive a write: {leftovers:?}");

        cleanup(&dir, &path);
    }

    #[tokio::test]
    async fn writer_task_publishes_every_transition() {
        let dir = scratch_dir("task");
        let path = dir.join("nested").join("edge-status.json");
        let (tx, rx) = watch::channel(EdgeStatusFile::connecting(&ExitPreference::Auto));

        let writer = StatusWriter::start(path.clone(), rx).expect("initial write creates the parent dir");
        let first: EdgeStatusFile =
            serde_json::from_slice(&fs::read(&path).expect("file exists after start")).expect("parses");
        assert_eq!(first.state, EdgeState::Connecting);

        tx.send_modify(|s| s.set_connected(Some(ExitPreference::Auto), &[cc("NL")]));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
        loop {
            let seen: Option<EdgeStatusFile> =
                fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok());
            if seen.as_ref().map(|s| s.state) == Some(EdgeState::Connected) {
                assert_eq!(seen.map(|s| s.countries), Some(vec!["NL".to_owned()]));
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "the writer never published `connected`");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        writer.stop();
        assert!(!path.exists(), "a clean shutdown removes the status file");
        let _ = fs::remove_dir(path.parent().expect("has parent"));
        let _ = fs::remove_dir(&dir);
    }

    // ── supervisor publishes its state ──────────────────────────────────

    /// Hands out pre-scripted dial results; once they run out it parks.
    struct ScriptedDialer {
        script: StdMutex<VecDeque<Result<(usize, EdgeSession), EdgeDialError>>>,
    }

    impl ScriptedDialer {
        fn new(script: Vec<Result<(usize, EdgeSession), EdgeDialError>>) -> Self {
            Self { script: StdMutex::new(script.into()) }
        }
    }

    #[async_trait::async_trait]
    impl EdgeDialer for ScriptedDialer {
        async fn dial(&self, _start_idx: usize) -> Result<(usize, EdgeSession), EdgeDialError> {
            let next = self.script.lock().expect("script mutex not poisoned").pop_front();
            match next {
                Some(result) => result,
                None => std::future::pending().await,
            }
        }
    }

    /// A live in-memory session; the returned server handle must be kept
    /// alive or the session reads as closed.
    fn live_session(countries: &[&str], granted: ExitPreference) -> (EdgeSession, SessionHandle) {
        let (client_io, server_io) = tokio::io::duplex(4096);
        let client = SessionHandle::client(TokioIoCompat(client_io));
        let server = SessionHandle::server(TokioIoCompat(server_io));
        let welcome = EdgeWelcome {
            version: 1,
            countries: countries.iter().map(|c| cc(c)).collect(),
            outcome: EdgeOutcome::Accepted { exit: granted },
        };
        (EdgeSession::for_test(client, welcome), server)
    }

    #[tokio::test]
    async fn supervisor_publishes_connecting_then_connected() {
        let (session, _server) = live_session(&["NL", "DE"], ExitPreference::Country(cc("NL")));
        let dialer = ScriptedDialer::new(vec![Ok((0, session))]);
        let supervisor =
            EdgeSessionSupervisor::spawn_with_dialer(dialer, ExitPreference::Country(cc("NL")));
        let mut rx = supervisor.status();

        // The driver task has not run yet on this single-threaded runtime: the
        // published value is the initial one.
        {
            let initial = rx.borrow();
            assert_eq!(initial.state, EdgeState::Connecting);
            assert_eq!(initial.reason, None);
            assert_eq!(initial.exit.requested, "NL");
            assert_eq!(initial.exit.accepted, None);
            assert!(initial.countries.is_empty());
        }

        let connected = tokio::time::timeout(
            Duration::from_secs(5),
            rx.wait_for(|s| s.state == EdgeState::Connected),
        )
        .await
        .expect("supervisor must reach `connected`")
        .expect("status channel stays open")
        .clone();
        assert_eq!(connected.reason, None);
        assert_eq!(connected.exit.requested, "NL");
        assert_eq!(connected.exit.accepted.as_deref(), Some("NL"));
        assert_eq!(connected.countries, ["NL", "DE"]);

        // Nobody was subscribed to the session slot while the dial finished;
        // the session must still be retrievable afterwards.
        assert!(supervisor.current().is_some(), "a session dialed before any waiter must be kept");
    }

    #[tokio::test]
    async fn supervisor_names_a_refusal_and_keeps_retrying() {
        let refusal = EdgeDialError::Refused(RefusalDisplay(EdgeRefusal::CountryUnavailable {
            requested: cc("ZZ"),
        }));
        let dialer = ScriptedDialer::new(vec![Err(refusal)]);
        let supervisor =
            EdgeSessionSupervisor::spawn_with_dialer(dialer, ExitPreference::Country(cc("ZZ")));
        let mut rx = supervisor.status();

        let refused = tokio::time::timeout(
            Duration::from_secs(5),
            rx.wait_for(|s| s.state == EdgeState::Refused),
        )
        .await
        .expect("supervisor must publish `refused`")
        .expect("status channel stays open")
        .clone();
        assert_eq!(refused.reason, Some(EdgeReason::RefusedCountryUnavailable));
        assert_eq!(refused.exit.accepted, None);
        assert!(supervisor.current().is_none());
    }
}
