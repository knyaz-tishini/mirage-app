//! `mirage-subscriber-client` — the runnable binary.
//!
//! The commercial customer's own process: reads his delivery artefact,
//! parses it, refuses it BY NAME if it is malformed
//! (`mirage_subscriber_client::artefact`'s three-step discipline — parse,
//! then validate, then assemble), and otherwise stands up the local,
//! loopback-only SOCKS5 listener carrying every flow over one persistent
//! mux session to the customer's edge
//! (`mirage_subscriber_client::session::EdgeSession`). The
//! `countries` subcommand instead connects, prints the edge's country list,
//! and exits — it never starts the SOCKS5 listener.
//!
//! This binary is NOT an admin surface: its only inputs are the artefact
//! path and the local bind address, and the bind address is refused
//! outright if it is not loopback — the customer has no flag that picks a
//! hop, a node, or a chain composition; that is all the edge's own job.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitCode};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use mirage_control_plane::ExitPreference;
use mirage_client_chain::chain_install::{self, ChainRequest};
use mirage_client_chain::oss_install::OssCarrier;
use mirage_client_chain::provision::InstallBus;
use mirage_subscriber_client::{
    chain_artefact, control, exit_preference_from_mode, http_connect, multi::ClientSet,
    oss_setup, publish_fatal, tun_mode, DeliveryArtefact, EdgeReason, EdgeSession, EdgeState,
    EgressScope, OssSetup, PlaneConsumer, SubscriberClient, SubscriberClientError,
    SubscriberClientOptions, TunMode,
};
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(
    name = "mirage-subscriber-client",
    version,
    about = "Mirage commercial subscriber client — local proxy over one persistent edge session"
)]
struct Args {
    /// Path to the delivery artefact (JSON). This file decides where traffic
    /// goes. Required except for `claim`, which writes the file and exits.
    #[arg(long)]
    artefact: Option<PathBuf>,

    /// A configuration to serve: `LABEL=ARTEFACT.json`. Repeat for several.
    ///
    /// One customer may hold several configurations against one box: each
    /// with its own key, its own address inside the plane and its own
    /// SOCKS5 port. Mutually exclusive with `--artefact`.
    #[arg(long = "config", value_name = "LABEL=ARTEFACT")]
    config: Vec<String>,

    /// The SOCKS5 bind of one configuration: `LABEL=ADDR`. Repeat for
    /// several. A configuration without one gets `--bind` with the port
    /// counted up from the first configuration that has none.
    #[arg(long = "config-bind", value_name = "LABEL=ADDR")]
    config_bind: Vec<String>,

    /// Local SOCKS5 bind address. Must be a loopback address — this
    /// listener must never be reachable off this machine. Ignored by the
    /// `countries` subcommand. Also the address TUN feeds.
    #[arg(long, default_value = "127.0.0.1:1080")]
    bind: SocketAddr,

    /// Keep this JSON file current with the connector's state (connecting /
    /// connected / reconnecting / refused / fatal, a closed reason code, the
    /// requested and granted exit, the edge's country list). Rewritten
    /// atomically on every transition and every 5 s; removed on a clean
    /// shutdown. Ignored by the `countries` subcommand.
    #[arg(long)]
    status_file: Option<PathBuf>,

    /// Loopback control API. When set, `--transport` is ignored.
    /// Listeners stay down until POST /v1/session and POST /v1/listeners.
    #[arg(long)]
    control: Option<SocketAddr>,

    /// How this machine enters Mirage: `socks`, `http`, or `tun`.
    #[arg(long, default_value = "socks")]
    transport: String,

    /// Physical interface for the entrance socket when another VPN owns the
    /// default route. Linux and macOS only; no host routes are changed.
    #[arg(long)]
    carrier_iface: Option<String>,

    /// Local HTTP CONNECT bind. Loopback only. Used when `--transport http`.
    #[arg(long, default_value = "127.0.0.1:8080")]
    http_bind: SocketAddr,

    /// Destination the proxy (or split TUN) may send. Repeat for several.
    /// `HOST` or `HOST:PORT`. Absent means every destination, except TUN
    /// split which requires at least one site or one app.
    #[arg(long = "service")]
    service: Vec<String>,

    /// Local program the proxy (or split TUN) may carry. Repeat for several.
    /// A file name (`chrome.exe`) or a full path. Absent means every program.
    #[arg(long = "app")]
    app: Vec<String>,

    /// `full` sends every non-bypass destination into Mirage. `split` sends
    /// only `--service`. Required for `--transport tun`.
    #[arg(long)]
    tun_mode: Option<String>,

    /// Path to the `mirage2tun` binary. Required for `--transport tun`.
    #[arg(long)]
    tun_bin: Option<PathBuf>,

    /// CIDR that stays on the physical NIC (the local subnet). Repeatable.
    /// TUN only. Not an entrance address.
    #[arg(long = "bypass-cidr")]
    bypass_cidr: Vec<String>,

    /// How many consecutive failed stream opens tear the edge session down
    /// and force a redial. This is the watchdog that notices the edge
    /// refusing streams (it holds no chain for this exit) — without it the
    /// connector sits at `connected` through a blackout until the session
    /// TCP dies on its own.
    #[arg(long, default_value_t = mirage_subscriber_client::DEFAULT_STREAM_FAILURE_THRESHOLD)]
    stream_failure_threshold: u32,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Connect to the edge, print the countries it can serve (one per
    /// line), and exit. Never starts the local SOCKS5 listener.
    Countries,
    /// Redeem a cabinet connection claim, write the artefact, and exit.
    /// Does not open a session and does not choose a port, a face, or an exit.
    Claim {
        /// Panel origin, for example `https://panel.m1r4g3.bid`. One trailing
        /// slash is stripped.
        #[arg(long)]
        panel: String,
        /// Connection code or `/c/` link. One successful redeem burns it.
        #[arg(long)]
        claim: Option<String>,
        /// PNG or JPEG of the same claim's QR. A photograph of the code is
        /// enough. The text inside the picture is redeemed, not the file.
        #[arg(long)]
        qr: Option<PathBuf>,
        /// Poll code returned by a redeem that has not finished yet.
        #[arg(long)]
        poll: Option<String>,
        /// Artefact JSON destination. Created mode 0600 on unix.
        #[arg(long)]
        out: PathBuf,
    },
    /// Install a self-assembled box and write the artefact it publishes.
    ///
    /// The client makes its own key, installs the box with that key's public
    /// half in the box's allowlist, reads back what the box published, and
    /// writes the artefact — the same artefact the fleet's panel issues. No
    /// session is opened and no traffic is carried: the artefact is written
    /// for the client that dials it.
    Oss {
        /// The box, as `user@host:port`. The port defaults to 22.
        #[arg(long = "box", value_name = "USER@HOST:PORT")]
        box_addr: String,
        /// The password of the box's ssh door. Used once, to plant the key.
        #[arg(long)]
        password: Option<String>,
        /// A file with the password, for when the password must not sit in
        /// the shell's history.
        #[arg(long)]
        password_file: Option<PathBuf>,
        /// Read the password from the standard input, one line.
        ///
        /// The password sits neither in the command line nor on the disk: the
        /// caller writes it into the pipe and the installer takes it from
        /// there. This is the way a windowed caller hands a secret over.
        #[arg(long)]
        password_stdin: bool,
        /// The `mirage-vpn` binary that goes on the box.
        #[arg(long)]
        binary: Option<PathBuf>,
        /// The MirageVG port the box opens.
        #[arg(long)]
        mwg_port: Option<u16>,
        /// The MirageKVIC port the box opens.
        #[arg(long)]
        mkvik_port: Option<u16>,
        /// The id both ends bake into the handshake.
        #[arg(long, default_value = "mirage-vpn")]
        mesh_id: String,
        /// The name the client goes under in the box's peer table.
        #[arg(long, default_value = "customer")]
        client_name: String,
        /// The label the artefact carries.
        #[arg(long, default_value = "self-built")]
        label: String,
        /// Where the client's own key pair is kept between runs.
        #[arg(long)]
        key_out: Option<PathBuf>,
        /// This box's own state directory: the ssh key and the host-key pin.
        #[arg(long)]
        state: Option<PathBuf>,
        /// The ssh key the client uses for this box's door — the operator's
        /// own key, the one the box already knows. The public half is the
        /// same path with `.pub` beside it.
        #[arg(long)]
        ssh_key: Option<PathBuf>,
        /// Print the client's own key for this box and stop.
        ///
        /// The line goes into the box's own list of keys — through the
        /// hoster's panel, or by hand — and the client comes in by key from
        /// then on, with no password at all. The private half stays on this
        /// machine.
        #[arg(long)]
        print_key: bool,
        /// Where the artefact is written.
        #[arg(long, default_value = "artefact.json")]
        out: PathBuf,
    },
    /// Build a chain of boxes: install every box, wire them into one relay
    /// path, and write the chain down.
    ///
    /// No session is opened and no traffic is carried — the chain is written
    /// for the client that dials it. The FIRST box is the edge the customer
    /// talks to; the boxes after it are the hops the traffic walks.
    Chain {
        /// The chain's id. Written into every box of the chain.
        #[arg(long, default_value = "stand")]
        chain_id: String,
        /// The boxes of the chain, edge first: `user@host:port`. One flag per
        /// box, in the order the traffic walks them.
        #[arg(long = "box", value_name = "USER@HOST:PORT")]
        boxes: Vec<String>,
        /// The password of the boxes' SSH door. Used once per box to plant
        /// the key, then dropped — it is not written anywhere.
        #[arg(long)]
        password: Option<String>,
        /// A file with the password, for when the password must not sit in
        /// the shell's history.
        #[arg(long)]
        password_file: Option<PathBuf>,
        /// The base `server.env` every box needs for itself — the stack, the
        /// protocols, the ports, the masks. The chain is appended to it.
        #[arg(long)]
        base_env: Option<PathBuf>,
        /// Where the chain is written: `chain.json` and `link.txt`.
        #[arg(long, default_value = ".")]
        out: PathBuf,
    },
}

/// Standard L0 panic catcher (matches every other binary in this
/// workspace, e.g. `crates/debug-shell/src/main.rs`): log the location and
/// message through `tracing`, then a clean `exit(1)` — never the default
/// Rust abort, and never a raw backtrace in the customer's face.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "<unknown>".into());
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".into()
        };
        error!(location = %location, message = %msg, "panic — process exiting");
        std::thread::sleep(std::time::Duration::from_millis(100));
        std::process::exit(1);
    }));
}

fn read_and_validate_artefact(path: &PathBuf) -> Result<DeliveryArtefact, ExitCode> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        error!(path = %path.display(), error = %e, "refusing: cannot read artefact file");
        ExitCode::FAILURE
    })?;

    // Step 1: parse — never validates. The artefact's own `kind` says which
    // of the two it is; an artefact without one is the VPN one.
    let artefact = DeliveryArtefact::from_json(&raw).map_err(|e| {
        error!(error = %e, "refusing: artefact did not parse");
        ExitCode::FAILURE
    })?;

    // Step 2: validate — refuses BY NAME, touches nothing else.
    artefact.validate().map_err(|e| {
        error!(error = %e, "refusing: artefact failed validation");
        ExitCode::FAILURE
    })?;

    Ok(artefact)
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    install_panic_hook();

    let args = Args::parse();

    // A chain is built before anything is dialled: it needs no artefact and
    // opens no session, so it is answered before the artefact is demanded.
    if let Some(Command::Oss {
        box_addr,
        password,
        password_file,
        password_stdin,
        binary,
        mwg_port,
        mkvik_port,
        mesh_id,
        client_name,
        label,
        key_out,
        state,
        ssh_key,
        print_key,
        out,
    }) = &args.command
    {
        return run_oss(
            box_addr,
            password.as_deref(),
            password_file.as_deref(),
            *password_stdin,
            binary.as_deref(),
            *mwg_port,
            *mkvik_port,
            mesh_id,
            client_name,
            label,
            key_out.as_deref(),
            state.as_deref(),
            ssh_key.as_deref(),
            *print_key,
            out,
        )
        .await;
    }

    if let Some(Command::Chain {
        chain_id,
        boxes,
        password,
        password_file,
        base_env,
        out,
    }) = &args.command
    {
        return run_chain(
            chain_id,
            boxes,
            password.as_deref(),
            password_file.as_deref(),
            base_env.as_deref(),
            out,
        )
        .await;
    }

    if let Some(Command::Claim { panel, claim, qr, poll, out }) = &args.command {
        if qr.is_some() && claim.is_some() {
            error!("refusing: pass a code, a link, or one qr image");
            return ExitCode::FAILURE;
        }
        let from_image = if let Some(path) = qr {
            match read_qr_claim(path) {
                Ok(text) => Some(text),
                Err(err) => {
                    error!(error = %err, "refusing: qr image");
                    return ExitCode::FAILURE;
                }
            }
        } else {
            None
        };
        let claim_text = claim.clone().or(from_image);
        return run_claim(panel, claim_text.as_deref(), poll.as_deref(), out).await;
    }

    let Some(artefact_path) = args.artefact.as_ref() else {
        // Several configurations in one process: each with its own key, its
        // own address inside the plane and its own local surface.
        if !args.config.is_empty() {
            return run_multi(&args).await;
        }
        error!("refusing: --artefact is required");
        return ExitCode::FAILURE;
    };
    if !args.config.is_empty() {
        error!("refusing: --artefact and --config are two ways to say the same thing");
        return ExitCode::FAILURE;
    }
    let artefact = match read_and_validate_artefact(artefact_path) {
        Ok(artefact) => artefact,
        Err(code) => {
            if args.command.is_none() {
                report_fatal(
                    args.status_file.as_deref(),
                    &ExitPreference::Auto,
                    Some(EdgeReason::BadArtefact),
                );
            }
            return code;
        }
    };

    match &args.command {
        Some(Command::Countries) => run_countries(&artefact).await,
        Some(Command::Claim { .. }) => ExitCode::FAILURE,
        Some(Command::Chain { .. }) => ExitCode::FAILURE,
        Some(Command::Oss { .. }) => ExitCode::FAILURE,
        None => run_proxy(&artefact, &args).await,
    }
}

/// Best effort: leave a `fatal` status document for a connector that cannot
/// start, so its supervisor learns why without parsing logs. A failure to
/// write it is logged and otherwise ignored — the process is exiting anyway.
fn report_fatal(status_file: Option<&Path>, requested: &ExitPreference, reason: Option<EdgeReason>) {
    let Some(path) = status_file else { return };
    if let Err(e) = publish_fatal(path, requested, reason) {
        error!(path = %path.display(), error = %e, "cannot write the fatal status file");
    }
}

/// Connect to the edge over its entrances, print every country it can serve,
/// and exit — no SOCKS5 listener, no supervisor, no reconnect loop; a single
/// dial attempt is enough for this one-shot query.
///
/// An onion artefact has no such list: a chain's exit is where it is, and
/// there is no edge to ask.
async fn run_countries(artefact: &DeliveryArtefact) -> ExitCode {
    let artefact = match artefact {
        DeliveryArtefact::Vpn(vpn) => vpn,
        DeliveryArtefact::Onion(_) => {
            error!("refusing: an onion artefact names a chain, not an edge - there is no country list");
            return ExitCode::FAILURE;
        }
    };
    let exit = match exit_preference_from_mode(&artefact.mode) {
        Ok(exit) => exit,
        Err(e) => {
            error!(error = %e, "refusing: artefact mode does not map to a valid exit preference");
            return ExitCode::FAILURE;
        }
    };
    let client_static_priv = match artefact.client_static_priv() {
        Ok(key) => key,
        Err(e) => {
            error!(error = %e, "refusing: cannot decode the subscriber's private key");
            return ExitCode::FAILURE;
        }
    };
    let mesh_id = artefact.mesh_id.as_bytes().to_vec();
    // EVERY entrance is dialable — the outer camouflage decides the wire
    // transport, so a datagram-shaped face is dialled over datagrams. The
    // query must reach a box that publishes only those.
    let entrances = &artefact.entrances;
    if entrances.is_empty() {
        error!("refusing: the artefact carries no entrance, nothing to dial");
        return ExitCode::FAILURE;
    }

    match EdgeSession::connect_first_available(entrances, &mesh_id, client_static_priv, exit, 0).await {
        Ok((idx, session)) => {
            info!(entrance = %entrances[idx].addr, "mirage-subscriber-client: connected to edge");
            for cc in session.countries() {
                println!("{cc}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!(error = %e, "refusing: could not reach the edge");
            ExitCode::FAILURE
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Transport {
    Socks,
    Http,
    Tun,
}

fn parse_transport(raw: &str) -> Result<Transport, String> {
    match raw {
        "socks" => Ok(Transport::Socks),
        "http" => Ok(Transport::Http),
        "tun" => Ok(Transport::Tun),
        other => Err(format!("unknown --transport {other} (expected socks, http, or tun)")),
    }
}

fn parse_tun_mode(raw: &str) -> Result<TunMode, String> {
    match raw {
        "full" => Ok(TunMode::Full),
        "split" => Ok(TunMode::Split),
        other => Err(format!("unknown --tun-mode {other} (expected full or split)")),
    }
}

/// Default-route interface, captured before TUN moves that route.
fn default_iface() -> Result<String, String> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/net/route")
            .map_err(|e| format!("read /proc/net/route: {e}"))?;
        for line in text.lines().skip(1) {
            let mut fields = line.split_whitespace();
            let iface = fields.next().unwrap_or("");
            let dest = fields.next().unwrap_or("");
            if dest == "00000000" && !iface.is_empty() {
                return Ok(iface.to_string());
            }
        }
        Err("no default route in /proc/net/route".into())
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("route")
            .args(["-n", "get", "default"])
            .output()
            .map_err(|e| format!("route -n get default: {e}"))?;
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let line = line.trim();
            if let Some(iface) = line.strip_prefix("interface:") {
                let iface = iface.trim();
                if !iface.is_empty() {
                    return Ok(iface.to_string());
                }
            }
        }
        Err("route -n get default did not name an interface".into())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err("carrier interface discovery is implemented for linux and macos".into())
    }
}

async fn wait_for_stop() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(sig) => sig,
            Err(e) => {
                error!(error = %e, "failed to install SIGTERM handler");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            r = tokio::signal::ctrl_c() => {
                if r.is_err() {
                    error!("failed to install Ctrl-C handler — shutting down anyway");
                }
            }
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        if tokio::signal::ctrl_c().await.is_err() {
            error!("failed to install Ctrl-C handler — shutting down anyway");
        }
    }
}

/// SIGTERM first so mirage2tun can remove its routes. SIGKILL is the fallback
/// and leaves those routes in place.
fn stop_tun_child(child: &mut Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        unsafe {
            extern "C" {
                fn kill(pid: i32, sig: i32) -> i32;
            }
            let _ = kill(pid, 15);
        }
        let start = std::time::Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if start.elapsed() < Duration::from_secs(3) => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                _ => break,
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// macOS has no fwmark. `IP_BOUND_IF` only helps if en0 still has a route
/// after mirage2tun installs the /1 overrides. An ifscope default is that
/// route: unbound sockets still follow the tun, the pinned carrier does not.
/// The entrance address stays out of `relay_addrs`.
#[cfg(target_os = "macos")]
fn install_ifscope_default() -> Result<(String, String), String> {
    let out = std::process::Command::new("route")
        .args(["-n", "get", "default"])
        .output()
        .map_err(|e| format!("route -n get default: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut iface = String::new();
    let mut gateway = String::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("interface:") {
            iface = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("gateway:") {
            gateway = v.trim().to_string();
        }
    }
    if iface.is_empty() || gateway.is_empty() || iface.starts_with("utun") {
        return Err("default route is not a physical NIC".into());
    }
    let add = std::process::Command::new("route")
        .args(["-n", "add", "-ifscope", &iface, "default", &gateway])
        .output()
        .map_err(|e| format!("route add -ifscope: {e}"))?;
    if !add.status.success() {
        let both = format!(
            "{}{}",
            String::from_utf8_lossy(&add.stderr),
            String::from_utf8_lossy(&add.stdout)
        );
        if !both.contains("File exists") {
            return Err(format!("route add -ifscope {iface} default {gateway}: {both}"));
        }
    }
    Ok((iface, gateway))
}

#[cfg(target_os = "macos")]
fn remove_ifscope_default(iface: &str, gateway: &str) {
    let _ = std::process::Command::new("route")
        .args(["-n", "delete", "-ifscope", iface, "default", gateway])
        .status();
}

async fn wait_connected(client: &SubscriberClient) -> bool {
    let mut rx = client.supervisor.current().status();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
    loop {
        {
            let state = rx.borrow().state;
            if state == EdgeState::Connected {
                return true;
            }
            if state == EdgeState::Refused || state == EdgeState::Fatal {
                return false;
            }
        }
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    return false;
                }
            }
            _ = tokio::time::sleep_until(deadline) => return false,
        }
    }
}

async fn wait_for_socks(bind: SocketAddr) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if tokio::net::TcpStream::connect(bind).await.is_ok() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Stand up the local proxy the user asked for, over one persistent edge
/// session. The artefact picks the edge. These flags only pick the local
/// capture.
///
/// The hosts the local capture must not swallow: the boxes the traffic leaves
/// through. For the VPN artefact that is the edge's faces; for the onion
/// artefact it is the chain's hops - the first one above all, since that is
/// the box the traffic leaves by.
fn relay_hosts(artefact: &DeliveryArtefact) -> Vec<String> {
    let mut out = Vec::new();
    let addrs: Vec<String> = match artefact {
        DeliveryArtefact::Vpn(vpn) => vpn.entrances.iter().map(|e| e.addr.clone()).collect(),
        DeliveryArtefact::Onion(onion) => onion.hops.iter().map(|h| h.addr.clone()).collect(),
    };
    for addr in addrs {
        let host = addr
            .rsplit_once(':')
            .map(|(host, _)| host)
            .unwrap_or(addr.as_str());
        let host = host.trim_matches(|ch| ch == '[' || ch == ']');
        if host.parse::<std::net::IpAddr>().is_ok() && !out.iter().any(|have| have == host) {
            out.push(host.to_owned());
        }
    }
    out
}

/// Which kind of artefact this is — for the log line.
fn artefact_kind(artefact: &DeliveryArtefact) -> &'static str {
    match artefact {
        DeliveryArtefact::Vpn(_) => "vpn",
        DeliveryArtefact::Onion(_) => "onion",
    }
}

fn discover_tun_bin() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os("MIRAGE_TUN_BIN") {
        let path = PathBuf::from(raw);
        if path.is_file() {
            return Some(path);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let name = format!("mirage2tun{}", std::env::consts::EXE_SUFFIX);
    let sibling = dir.join(&name);
    if sibling.is_file() {
        return Some(sibling);
    }
    let nested = dir.join("../../../mirage2tun/target/release").join(name);
    if nested.is_file() {
        return Some(nested);
    }
    None
}

async fn run_proxy(artefact: &DeliveryArtefact, args: &Args) -> ExitCode {
    // The requested exit: the VPN artefact names one, a chain does not - its
    // exit is where it is.
    let requested = match artefact {
        DeliveryArtefact::Vpn(vpn) => exit_preference_from_mode(&vpn.mode).unwrap_or(ExitPreference::Auto),
        DeliveryArtefact::Onion(_) => ExitPreference::Auto,
    };
    let fail = |reason: Option<EdgeReason>| {
        report_fatal(args.status_file.as_deref(), &requested, reason);
        ExitCode::FAILURE
    };

    // `--transport` is ignored. The control API starts the selected listener,
    // and starts mirage2tun when that mode is selected and the binary is set.
    if let Some(addr) = args.control {
        if !control::ensure_loopback(addr) {
            error!("refusing: --control must be loopback");
            return fail(None);
        }
        let scope = match EgressScope::from_lists(&args.service, &args.app) {
            Ok(scope) => scope,
            Err(e) => {
                error!(error = %e, "refusing");
                return fail(None);
            }
        };
        info!(
            kind = artefact_kind(artefact),
            control = %addr,
            "mirage-subscriber-client: artefact validated, control api"
        );
        let options = SubscriberClientOptions {
            socks5_bind: args.bind,
            status_file: args.status_file.clone(),
            scope: scope.clone(),
            start_socks: false,
            ..Default::default()
        };
        let client = match SubscriberClient::build_from_delivery_with_threshold(
            artefact,
            options,
            args.stream_failure_threshold,
        )
        .await
        {
            Ok(client) => client,
            Err(e) => {
                error!(error = %e, "refusing: failed to assemble subscriber client");
                if !matches!(e, SubscriberClientError::StatusFile { .. }) {
                    report_fatal(args.status_file.as_deref(), &requested, e.status_reason());
                }
                return ExitCode::FAILURE;
            }
        };
        info!(control = %addr, "mirage-subscriber-client: control api ready — Ctrl-C to stop");
        tokio::select! {
            result = control::serve(
                addr,
                client.supervisor.clone(),
                std::sync::Arc::new(scope),
                args.tun_bin.clone().or_else(discover_tun_bin),
                relay_hosts(artefact),
                tun_mode::wire_mode_for(artefact),
            ) => {
                if let Err(e) = result {
                    error!(error = %e, "control listener exited");
                }
            }
            _ = wait_for_stop() => {}
        }
        client.shutdown();
        return ExitCode::SUCCESS;
    }

    let transport = match parse_transport(&args.transport) {
        Ok(t) => t,
        Err(e) => {
            error!(error = %e, "refusing");
            return fail(None);
        }
    };
    if !args.bind.ip().is_loopback() {
        error!(
            bind = %args.bind,
            "refusing: --bind must be a loopback address — this listener must never be reachable off this machine"
        );
        return fail(None);
    }
    if transport == Transport::Http && !args.http_bind.ip().is_loopback() {
        error!(bind = %args.http_bind, "refusing: --http-bind must be a loopback address");
        return fail(None);
    }

    let scope = match EgressScope::from_lists(&args.service, &args.app) {
        Ok(scope) => scope,
        Err(e) => {
            error!(error = %e, "refusing");
            return fail(None);
        }
    };
    let tun = if transport == Transport::Tun {
        let mode = match args.tun_mode.as_deref().map(parse_tun_mode) {
            Some(Ok(mode)) => mode,
            Some(Err(e)) => {
                error!(error = %e, "refusing");
                return fail(None);
            }
            None => {
                error!("refusing: --transport tun requires --tun-mode full|split");
                return fail(None);
            }
        };
        if args.tun_bin.is_none() {
            error!("refusing: --transport tun requires --tun-bin");
            return fail(None);
        }
        match mode {
            TunMode::Full if !args.service.is_empty() || !args.app.is_empty() => {
                error!("refusing: --tun-mode full does not take --service or --app");
                return fail(None);
            }
            TunMode::Split if args.service.is_empty() && args.app.is_empty() => {
                error!("refusing: --tun-mode split requires --service or --app");
                return fail(None);
            }
            _ => {}
        }
        Some(mode)
    } else {
        None
    };

    if let Some(iface) = args.carrier_iface.as_deref() {
        if iface.is_empty() {
            error!("refusing: --carrier-iface must not be empty");
            return fail(None);
        }
        info!(iface = %iface, "pinning the edge carrier to the physical NIC");
        mirage_protocol::driver::set_carrier_interface(Some(iface.to_owned()));
    } else if transport == Transport::Tun {
        match default_iface() {
            Ok(iface) => {
                info!(iface = %iface, "pinning the edge carrier to the physical NIC");
                mirage_protocol::driver::set_carrier_interface(Some(iface));
            }
            Err(_) if cfg!(windows) => {
                info!("windows keeps the edge off the tun with relay routes");
            }
            Err(e) => {
                error!(error = %e, "refusing: cannot find the physical NIC for the edge carrier");
                return fail(None);
            }
        }
    }

    info!(
        kind = artefact_kind(artefact),
        transport = %args.transport,
        services = args.service.len(),
        "mirage-subscriber-client: artefact validated, assembling local proxy"
    );

    let options = SubscriberClientOptions {
        socks5_bind: args.bind,
        status_file: args.status_file.clone(),
        scope: scope.clone(),
        start_socks: transport != Transport::Http,
        // The tunnel device wants the plane's packets raw; the SOCKS5 and
        // HTTP listeners want them in the client's own userspace stack. One
        // owner per plane.
        plane_consumer: match transport {
            Transport::Tun => PlaneConsumer::RawIp,
            _ => PlaneConsumer::Stack,
        },
        ..Default::default()
    };

    let client = match SubscriberClient::build_from_delivery_with_threshold(
        artefact,
        options,
        args.stream_failure_threshold,
    )
    .await
    {
        Ok(client) => client,
        Err(e) => {
            error!(error = %e, "refusing: failed to assemble subscriber client");
            if !matches!(e, SubscriberClientError::StatusFile { .. }) {
                report_fatal(args.status_file.as_deref(), &requested, e.status_reason());
            }
            return ExitCode::FAILURE;
        }
    };

    let http_task = if transport == Transport::Http {
        let supervisor = std::sync::Arc::clone(&client.supervisor);
        let http_scope = std::sync::Arc::new(scope);
        let bind = args.http_bind;
        Some(tokio::spawn(async move {
            if let Err(e) = http_connect::serve(bind, supervisor, http_scope).await {
                error!(error = %e, "http connect listener exited");
            }
        }))
    } else {
        None
    };

    let mut tun_child: Option<Child> = None;
    #[cfg(target_os = "macos")]
    let mut ifscope: Option<(String, String)> = None;
    if let Some(mode) = tun {
        if !wait_connected(&client).await {
            error!("refusing: edge session did not connect before TUN");
            client.shutdown();
            return fail(None);
        }
        if !wait_for_socks(args.bind).await {
            error!(bind = %args.bind, "refusing: SOCKS listener did not come up before TUN");
            client.shutdown();
            return fail(None);
        }
        #[cfg(target_os = "macos")]
        {
            match install_ifscope_default() {
                Ok(route) => {
                    info!(iface = %route.0, gateway = %route.1, "carrier ifscope route installed");
                    ifscope = Some(route);
                }
                Err(e) => {
                    error!(error = %e, "refusing: cannot keep a physical route for the edge carrier");
                    client.shutdown();
                    return fail(None);
                }
            }
        }
        let config_path = args
            .status_file
            .as_ref()
            .map(|p| p.with_file_name("mirage-tun.json"))
            .unwrap_or_else(|| PathBuf::from("mirage-tun.json"));
        let relays = if cfg!(windows) { relay_hosts(artefact) } else { Vec::new() };
        // The address the box assigned to this client: the adapter must
        // carry exactly it, or the box drops the packets of this peer as a
        // mismatch.
        let tun_addr = client
            .assigned_client_address()
            .unwrap_or_else(|| tun_mode::DEFAULT_TUN_ADDR.to_owned());
        if let Err(e) = tun_mode::write_config(
            &config_path,
            args.bind,
            &tun_addr,
            &args.bypass_cidr,
            mode,
            tun_mode::wire_mode_for(artefact),
            &args.service,
            &args.app,
            &[],
            &[],
            &relays,
        )
        {
            error!(error = %e, "refusing: cannot write the tun config");
            client.shutdown();
            #[cfg(target_os = "macos")]
            if let Some((iface, gateway)) = ifscope.take() {
                remove_ifscope_default(&iface, &gateway);
            }
            return fail(None);
        }
        match tun_mode::spawn(args.tun_bin.as_ref().expect("checked"), &config_path) {
            Ok(child) => {
                info!(pid = child.id(), config = %config_path.display(), "mirage2tun started");
                tun_child = Some(child);
            }
            Err(e) => {
                error!(error = %e, "refusing: cannot spawn mirage2tun");
                client.shutdown();
                #[cfg(target_os = "macos")]
                if let Some((iface, gateway)) = ifscope.take() {
                    remove_ifscope_default(&iface, &gateway);
                }
                return fail(None);
            }
        }
    }

    match transport {
        Transport::Socks | Transport::Tun => {
            info!(bind = %client.socks5_bind, "mirage-subscriber-client: ready — Ctrl-C to stop");
        }
        Transport::Http => {
            info!(bind = %args.http_bind, "mirage-subscriber-client: HTTP CONNECT ready — Ctrl-C to stop");
        }
    }

    wait_for_stop().await;

    info!("mirage-subscriber-client: shutdown requested");
    if let Some(task) = http_task {
        task.abort();
    }
    client.shutdown();
    if let Some(mut child) = tun_child {
        stop_tun_child(&mut child);
    }
    #[cfg(target_os = "macos")]
    if let Some((iface, gateway)) = ifscope {
        remove_ifscope_default(&iface, &gateway);
    }

    ExitCode::SUCCESS
}

/// Serve several configurations in one process.
///
/// Each configuration is a whole client: its own key, its own session, its
/// own SOCKS5 port, its own status file. Nothing is shared but the process.
///
/// The machine's tunnel device belongs to ONE configuration - it carries
/// exactly the address the box assigned to that client - so the device is
/// not started here.
async fn run_multi(args: &Args) -> ExitCode {
    let transport = match parse_transport(&args.transport) {
        Ok(transport) => transport,
        Err(e) => {
            error!(error = %e, "refusing");
            return ExitCode::FAILURE;
        }
    };
    if transport == Transport::Tun {
        error!("refusing: the tunnel device belongs to one configuration - run it with --artefact");
        return ExitCode::FAILURE;
    }
    let scope = match EgressScope::from_lists(&args.service, &args.app) {
        Ok(scope) => scope,
        Err(e) => {
            error!(error = %e, "refusing");
            return ExitCode::FAILURE;
        }
    };
    // The binds the operator named.
    let mut binds: std::collections::BTreeMap<String, SocketAddr> = Default::default();
    for spec in &args.config_bind {
        let Some((label, addr)) = spec.split_once('=') else {
            error!(spec = %spec, "refusing: a bind is LABEL=ADDR");
            return ExitCode::FAILURE;
        };
        let Ok(addr) = addr.parse::<SocketAddr>() else {
            error!(spec = %spec, "refusing: the bind is not an address");
            return ExitCode::FAILURE;
        };
        if !control::ensure_loopback(addr) {
            error!(bind = %addr, "refusing: a listener must be loopback");
            return ExitCode::FAILURE;
        }
        binds.insert(label.to_owned(), addr);
    }
    // The configurations themselves.
    let mut specs: Vec<(String, std::path::PathBuf)> = Vec::new();
    for spec in &args.config {
        let Some((label, path)) = spec.split_once('=') else {
            error!(spec = %spec, "refusing: a configuration is LABEL=ARTEFACT.json");
            return ExitCode::FAILURE;
        };
        if label.is_empty() {
            error!(spec = %spec, "refusing: the label is empty");
            return ExitCode::FAILURE;
        }
        specs.push((label.to_owned(), std::path::PathBuf::from(path)));
    }
    if specs.is_empty() {
        error!("refusing: --config wants at least one configuration");
        return ExitCode::FAILURE;
    }

    let mut set = ClientSet::new();
    // The configurations without a bind of their own take `--bind`, with the
    // port counted up from the first one that has none.
    let mut next_port = args.bind.port();
    for (label, path) in &specs {
        let artefact = match read_and_validate_artefact(path) {
            Ok(artefact) => artefact,
            Err(code) => {
                error!(label = %label, path = %path.display(), "refusing: the artefact is not usable");
                return code;
            }
        };
        let bind = match binds.get(label) {
            Some(addr) => *addr,
            None => {
                let port = next_port;
                next_port = next_port.saturating_add(1);
                SocketAddr::new(args.bind.ip(), port)
            }
        };
        let status_file = args.status_file.as_ref().map(|path| labelled_status_file(path, label));
        let options = SubscriberClientOptions {
            socks5_bind: bind,
            status_file,
            scope: scope.clone(),
            start_socks: transport != Transport::Http,
            plane_consumer: PlaneConsumer::Stack,
            ..Default::default()
        };
        if let Err(e) = set.add(label, &artefact, options).await {
            error!(label = %label, error = %e, "refusing: failed to assemble the subscriber client");
            return ExitCode::FAILURE;
        }
        info!(
            label = %label,
            bind = %bind,
            "mirage-subscriber-client: configuration ready"
        );
    }

    // The HTTP listener belongs to the first configuration.
    let http_task = if transport == Transport::Http {
        let first = specs[0].0.clone();
        let Some(client) = set.get(&first) else {
            error!("refusing: the first configuration is gone");
            return ExitCode::FAILURE;
        };
        let supervisor = std::sync::Arc::clone(&client.supervisor);
        let http_scope = std::sync::Arc::new(scope);
        let bind = args.http_bind;
        Some(tokio::spawn(async move {
            if let Err(e) = http_connect::serve(bind, supervisor, http_scope).await {
                error!(error = %e, "http connect listener exited");
            }
        }))
    } else {
        None
    };

    info!(
        configurations = set.len(),
        "mirage-subscriber-client: ready - Ctrl-C to stop"
    );
    wait_for_stop().await;

    info!("mirage-subscriber-client: shutdown requested");
    if let Some(task) = http_task {
        task.abort();
    }
    set.shutdown();
    ExitCode::SUCCESS
}

/// The status file of one configuration.
///
/// The label goes in front of the extension, so the files of several
/// configurations sit next to each other and none of them overwrites
/// another: `edge-status.json` + `consumer-1` -> `edge-status.consumer-1.json`.
fn labelled_status_file(path: &std::path::Path, label: &str) -> std::path::PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "status".to_owned());
    let extension = path.extension().map(|s| s.to_string_lossy().to_string());
    let name = match extension {
        Some(extension) => format!("{stem}.{label}.{extension}"),
        None => format!("{stem}.{label}"),
    };
    path.with_file_name(name)
}

fn panel_origin(panel: &str) -> &str {
    panel.trim().strip_suffix('/').unwrap_or(panel.trim())
}

fn exit_for_http(status: u16) -> Option<ExitCode> {
    match status {
        404 => Some(ExitCode::from(2)),
        409 => Some(ExitCode::from(3)),
        200 | 202 => None,
        _ => Some(ExitCode::FAILURE),
    }
}

async fn post_panel(client: &reqwest::Client, url: &str, body: serde_json::Value) -> Result<(u16, serde_json::Value), String> {
    let response = client.post(url).json(&body).send().await.map_err(|err| err.to_string())?;
    let status = response.status().as_u16();
    let text = response.text().await.map_err(|err| err.to_string())?;
    let value = if text.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null)
    };
    Ok((status, value))
}

fn write_credential(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    use std::io::Write;
    file.write_all(bytes)?;
    file.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn read_qr_claim(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|err| err.to_string())?;
    mirage_subscriber_client::claim_image::claim_text_from_image(&bytes)
}

/// Redeem `--claim` and/or poll until the artefact is ready, then write it.
/// Prints the path and the device id. Never prints the private key.
async fn run_claim(panel: &str, claim: Option<&str>, poll: Option<&str>, out: &Path) -> ExitCode {
    let origin = panel_origin(panel);
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(20)).build() {
        Ok(client) => client,
        Err(err) => {
            error!(error = %err, "refusing: cannot build the panel client");
            return ExitCode::FAILURE;
        }
    };

    let mut poll_code = poll.map(str::to_string);
    if let Some(code) = claim {
        let (status, body) = match post_panel(
            &client,
            &format!("{origin}/public/claims/redeem"),
            serde_json::json!({ "claim": code }),
        )
        .await
        {
            Ok(pair) => pair,
            Err(err) => {
                error!(error = %err, "redeem failed");
                return ExitCode::FAILURE;
            }
        };
        if let Some(code) = exit_for_http(status) {
            if status != 404 && status != 409 {
                error!(status, "redeem refused");
            }
            return code;
        }
        let Some(returned) = body.get("poll").and_then(|value| value.as_str()) else {
            error!(status, "redeem did not return a poll code");
            return ExitCode::FAILURE;
        };
        poll_code = Some(returned.to_string());
    }
    let Some(poll_code) = poll_code else {
        error!("refusing: claim needs --claim, --qr, or --poll");
        return ExitCode::FAILURE;
    };

    let started = std::time::Instant::now();
    let budget = Duration::from_secs(90);
    loop {
        if started.elapsed() >= budget {
            println!("{poll_code}");
            return ExitCode::from(4);
        }
        let (status, body) = match post_panel(
            &client,
            &format!("{origin}/public/claims/poll"),
            serde_json::json!({ "poll": poll_code }),
        )
        .await
        {
            Ok(pair) => pair,
            Err(err) => {
                error!(error = %err, "poll failed");
                return ExitCode::FAILURE;
            }
        };
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let rest = budget.saturating_sub(started.elapsed());
            if rest.is_zero() {
                println!("{poll_code}");
                return ExitCode::from(4);
            }
            tokio::time::sleep(Duration::from_secs(2).min(rest)).await;
            continue;
        }
        if let Some(code) = exit_for_http(status) {
            if status != 404 && status != 409 {
                error!(status, "poll refused");
            }
            return code;
        }
        let state = body.get("status").and_then(|value| value.as_str()).unwrap_or("");
        if state == "ready" {
            let document = body.get("artefact").cloned().filter(|value| !value.is_null()).or_else(|| {
                body.get("packet").cloned().filter(|value| !value.is_null())
            });
            let Some(document) = document else {
                error!("ready response carried no credential");
                return ExitCode::FAILURE;
            };
            let Some(device_id) = body.get("device_id").and_then(|value| value.as_str()) else {
                error!("ready response carried no device id");
                return ExitCode::FAILURE;
            };
            let mut bytes = match serde_json::to_vec(&document) {
                Ok(bytes) => bytes,
                Err(err) => {
                    error!(error = %err, "cannot encode the credential");
                    return ExitCode::FAILURE;
                }
            };
            bytes.push(b'\n');
            if let Err(err) = write_credential(out, &bytes) {
                error!(path = %out.display(), error = %err, "cannot write the credential");
                return ExitCode::FAILURE;
            }
            println!("wrote {} device {device_id}", out.display());
            return ExitCode::SUCCESS;
        }
        if state == "pending" {
            let rest = budget.saturating_sub(started.elapsed());
            if rest.is_zero() {
                println!("{poll_code}");
                return ExitCode::from(4);
            }
            tokio::time::sleep(Duration::from_secs(1).min(rest)).await;
            continue;
        }
        if state == "failed" {
            error!(status = state, "issuance failed");
            return ExitCode::FAILURE;
        }
        error!(status = state, "credential was already delivered");
        return ExitCode::FAILURE;
    }
}

/// The line a windowed caller reads when the install fails.
///
/// The GUI cannot read this process's log, so the reason travels back on the
/// standard output, marked. The headless run prints the same reason twice:
/// once for the operator's eyes, once for the caller's.
fn refuse(message: &str) -> ExitCode {
    println!("mirage-oss-error: {message}");
    error!(error = %message, "the self-assembly did not come up");
    ExitCode::FAILURE
}

/// The box's password, from wherever the caller left it.
///
/// Three ways in, and only one at a time: the command line, a file, or the
/// standard input. The pipe is the one a windowed caller uses — the secret
/// travels inside it and dies with it, and never sits in the process list
/// where every other process on the machine can read it.
fn take_password(
    password: Option<&str>,
    password_file: Option<&Path>,
    password_stdin: bool,
) -> Result<String, String> {
    let given = [
        password.is_some(),
        password_file.is_some(),
        password_stdin,
    ]
    .iter()
    .filter(|given| **given)
    .count();
    if given > 1 {
        return Err(
            "pass --password, --password-file or --password-stdin, not more than one".to_owned(),
        );
    }
    if let Some(password) = password {
        return Ok(password.to_owned());
    }
    if let Some(path) = password_file {
        let text = std::fs::read_to_string(path).map_err(|err| {
            format!("cannot read the password file {}: {err}", path.display())
        })?;
        return Ok(text.trim_end_matches(['\r', '\n']).to_owned());
    }
    if password_stdin {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
            .map_err(|err| format!("cannot read the password from the standard input: {err}"))?;
        return Ok(text.trim_end_matches(['\r', '\n']).to_owned());
    }
    Ok(String::new())
}

/// Install a self-assembled box and write the artefact it published.
///
/// The client makes its own key, the box is installed with that key's public
/// half in its allowlist, the box publishes itself, and the artefact is built
/// from the publication — the same artefact the fleet's panel issues.
#[allow(clippy::too_many_arguments)]
async fn run_oss(
    box_addr: &str,
    password: Option<&str>,
    password_file: Option<&Path>,
    password_stdin: bool,
    binary: Option<&Path>,
    mwg_port: Option<u16>,
    mkvik_port: Option<u16>,
    mesh_id: &str,
    client_name: &str,
    label: &str,
    key_out: Option<&Path>,
    state: Option<&Path>,
    ssh_key: Option<&Path>,
    print_key: bool,
    out: &Path,
) -> ExitCode {
    // The key the client comes in by. The person can put its public half into
    // the box by hand and be done with passwords.
    if print_key {
        let key = match mirage_client_chain::oss_install::oss_key(state) {
            Ok(key) => key,
            Err(err) => return refuse(&err),
        };
        if let Err(err) = mirage_client_chain::provision::ensure_key(&key.priv_path, &key.pub_path) {
            return refuse(&err);
        }
        match std::fs::read_to_string(&key.pub_path) {
            Ok(line) => {
                println!("{}", line.trim());
                return ExitCode::SUCCESS;
            }
            Err(err) => return refuse(&format!("cannot read the public key: {err}")),
        }
    }
    let password = match take_password(password, password_file, password_stdin) {
        Ok(password) => password,
        Err(err) => return refuse(&err),
    };

    let Some(binary) = binary else {
        return refuse("name the box's binary: --binary <path>");
    };
    let mut carriers = Vec::new();
    if let Some(port) = mwg_port {
        carriers.push(OssCarrier::mwg(port));
    }
    if let Some(port) = mkvik_port {
        carriers.push(OssCarrier::mkvik(port));
    }
    if carriers.is_empty() {
        return refuse("name at least one port for the box (--mwg-port or --mkvik-port)");
    }
    if !binary.is_file() {
        return refuse(&format!(
            "the mirage-vpn binary is not there: {}",
            binary.display()
        ));
    }

    let bus = Arc::new(InstallBus::new());
    let stop = watch_progress(Arc::clone(&bus));
    let outcome = oss_setup::setup(
        OssSetup {
            box_addr: box_addr.to_owned(),
            password,
            binary: binary.to_owned(),
            carriers,
            mesh_id: mesh_id.to_owned(),
            client_name: client_name.to_owned(),
            label: label.to_owned(),
            state: state.map(Path::to_owned),
            ssh_key: ssh_key.map(Path::to_owned),
            key_path: key_out.map(Path::to_owned),
        },
        bus.as_ref(),
    )
    .await;
    stop.store(true, Ordering::Relaxed);

    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(err) => return refuse(&err),
    };
    if let Some(warn) = &outcome.warn {
        warn!(warning = %warn, "the box is up, with a warning");
    }
    if let Err(err) = std::fs::write(out, &outcome.artefact_json) {
        return refuse(&format!("cannot write the artefact {}: {err}", out.display()));
    }
    println!("the box is up, the artefact is written: {}", out.display());
    for entrance in &outcome.artefact.entrances {
        println!("  face {}", entrance.addr);
    }
    ExitCode::SUCCESS
}

/// Build a chain of boxes and write it down.
///
/// The boxes are installed first, each on its own, and only then is the chain
/// written into them: a hop cannot be told about a neighbour that nobody has
/// installed yet. The chain's key material is minted here, and the mesh id is
/// the EDGE's own — taken from the edge's link — so the chain's legs match
/// the link the customer dials.
async fn run_chain(
    chain_id: &str,
    boxes: &[String],
    password: Option<&str>,
    password_file: Option<&Path>,
    base_env_file: Option<&Path>,
    out: &Path,
) -> ExitCode {
    if boxes.is_empty() {
        error!("refusing: a chain needs at least one box (--box)");
        return ExitCode::FAILURE;
    }
    let password = match (password, password_file) {
        (Some(_), Some(_)) => {
            error!("refusing: pass --password or --password-file, not both");
            return ExitCode::FAILURE;
        }
        (Some(password), None) => password.to_owned(),
        (None, Some(path)) => match std::fs::read_to_string(path) {
            Ok(text) => text.trim_end_matches(['\r', '\n']).to_owned(),
            Err(err) => {
                error!(path = %path.display(), error = %err, "cannot read the password file");
                return ExitCode::FAILURE;
            }
        },
        (None, None) => String::new(),
    };
    let mut requests: Vec<ChainRequest> = Vec::with_capacity(boxes.len());
    for spec in boxes {
        match parse_box(spec) {
            Ok(mut request) => {
                request.password = password.clone();
                requests.push(request);
            }
            Err(err) => {
                error!(box = %spec, error = %err, "refusing: bad box");
                return ExitCode::FAILURE;
            }
        }
    }
    let base_env = match base_env_file {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) => {
                error!(path = %path.display(), error = %err, "cannot read the base env");
                return ExitCode::FAILURE;
            }
        },
        None => String::new(),
    };

    let bus = Arc::new(InstallBus::new());
    let stop = watch_progress(Arc::clone(&bus));
    let outcome = chain_install::install_chain(chain_id, &requests, &base_env, bus.as_ref()).await;
    stop.store(true, Ordering::Relaxed);

    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(err) => {
            error!(error = %err, "the chain did not come up");
            return ExitCode::FAILURE;
        }
    };

    println!("chain {} is up", outcome.chain.chain_id);
    for (index, box_) in outcome.boxes.iter().enumerate() {
        println!(
            "  {}. {} {} door {} ({} doors)",
            index + 1,
            box_.role.as_env(),
            box_.host,
            box_.face.addr,
            box_.faces
        );
    }
    if let Err(err) = chain_install::save_chain(out, &outcome) {
        error!(dir = %out.display(), error = %err, "cannot write the chain");
        return ExitCode::FAILURE;
    }
    if let Err(err) = std::fs::write(out.join("link.txt"), outcome.link.as_bytes()) {
        error!(dir = %out.display(), error = %err, "cannot write the link");
        return ExitCode::FAILURE;
    }
    println!(
        "wrote {} and {}",
        out.join("chain.json").display(),
        out.join("link.txt").display()
    );

    // The onion artefact: the circuit the client walks itself. A chain of one
    // box has no hops, and then there is nothing to walk — the link above is
    // the whole story of that chain.
    let keypair = match oss_setup::load_or_make_keypair(Some(&out.join("client.key.json"))) {
        Ok(keypair) => keypair,
        Err(err) => {
            error!(dir = %out.display(), error = %err, "cannot keep the client's key");
            return ExitCode::FAILURE;
        }
    };
    match chain_artefact::build_onion_artefact(
        &outcome.chain,
        &keypair,
        &outcome.chain.chain_id,
        chain_artefact::now_ms(),
    ) {
        Ok(artefact) => match artefact.to_json() {
            Ok(json) => {
                let path = out.join("onion-artefact.json");
                if let Err(err) = std::fs::write(&path, json.as_bytes()) {
                    error!(path = %path.display(), error = %err, "cannot write the onion artefact");
                    return ExitCode::FAILURE;
                }
                println!("wrote {}", path.display());
            }
            Err(err) => {
                error!(error = %err, "the onion artefact did not serialize");
                return ExitCode::FAILURE;
            }
        },
        Err(chain_artefact::ChainArtefactError::NoHops) => {
            println!("the chain has no hops — there is no onion artefact to write");
        }
        Err(err) => {
            error!(error = %err, "the onion artefact did not build");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

/// Print the installer's status line as it changes.
///
/// The installer writes its progress into the bus, and a headless run has no
/// window to show it in — so the line is printed as it changes. The operator
/// sees that the box is checking its ports instead of a silent minute.
fn watch_progress(bus: Arc<InstallBus>) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    std::thread::spawn(move || {
        let mut last = String::new();
        while !flag.load(Ordering::Relaxed) {
            let line = bus.text.lock().unwrap_or_else(|p| p.into_inner()).clone();
            if !line.is_empty() && line != last {
                println!("... {line}");
                last = line;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    });
    stop
}

/// One box of the chain, as it was named on the command line.
///
/// `user@host:port`, or `host:port` for the root login on the standard SSH
/// port. An IPv6 address goes in brackets, the way a URL does.
fn parse_box(spec: &str) -> Result<ChainRequest, String> {
    let (user, rest) = match spec.split_once('@') {
        Some((user, rest)) => (user.to_owned(), rest.to_owned()),
        None => ("root".to_owned(), spec.to_owned()),
    };
    if user.trim().is_empty() {
        return Err("no login".to_owned());
    }
    let (host, port) = match rest.rsplit_once(':') {
        Some((host, port)) => {
            let port: u16 = port
                .trim()
                .parse()
                .map_err(|_| format!("bad port {port}"))?;
            let host = host.trim().trim_start_matches('[').trim_end_matches(']');
            (host.to_owned(), port)
        }
        None => (rest.trim().to_owned(), 22),
    };
    if host.trim().is_empty() {
        return Err("no host".to_owned());
    }
    Ok(ChainRequest {
        host: host.trim().to_owned(),
        port,
        user: user.trim().to_owned(),
        password: String::new(),
    })
}
