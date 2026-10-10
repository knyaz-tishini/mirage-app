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
use std::time::Duration;

use clap::{Parser, Subcommand};
use mirage_control_plane::ExitPreference;
use mirage_subscriber_client::{
    exit_preference_from_mode, http_connect, publish_fatal, tun_mode, EdgeReason, EdgeSession,
    EdgeState, EgressScope, EntranceArtefact, SubscriberClient, SubscriberClientError,
    SubscriberClientOptions, TunMode,
};
use tracing::{error, info};

#[derive(Parser)]
#[command(
    name = "mirage-subscriber-client",
    version,
    about = "Mirage commercial subscriber client — local proxy over one persistent edge session"
)]
struct Args {
    /// Path to the delivery artefact (JSON) issued by the operator.
    /// This file decides where traffic goes. Nothing below does.
    #[arg(long)]
    artefact: PathBuf,

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

    /// How this machine enters Mirage: `socks`, `http`, or `tun`.
    #[arg(long, default_value = "socks")]
    transport: String,

    /// Local HTTP CONNECT bind. Loopback only. Used when `--transport http`.
    #[arg(long, default_value = "127.0.0.1:8080")]
    http_bind: SocketAddr,

    /// Destination the proxy (or split TUN) may send. Repeat for several.
    /// `HOST` or `HOST:PORT`. Absent means every destination, except TUN
    /// split which requires at least one.
    #[arg(long = "service")]
    service: Vec<String>,

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

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Connect to the edge, print the countries it can serve (one per
    /// line), and exit. Never starts the local SOCKS5 listener.
    Countries,
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

fn read_and_validate_artefact(path: &PathBuf) -> Result<EntranceArtefact, ExitCode> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        error!(path = %path.display(), error = %e, "refusing: cannot read artefact file");
        ExitCode::FAILURE
    })?;

    // Step 1: parse — never validates (`EntranceArtefact::from_json`).
    let artefact = EntranceArtefact::from_json(&raw).map_err(|e| {
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

    let artefact = match read_and_validate_artefact(&args.artefact) {
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

    match args.command {
        Some(Command::Countries) => run_countries(&artefact).await,
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

/// Connect to the edge over its TCP-shaped entrances, print every country it
/// can serve, and exit — no SOCKS5 listener, no supervisor, no reconnect
/// loop; a single dial attempt is enough for this one-shot query.
async fn run_countries(artefact: &EntranceArtefact) -> ExitCode {
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
    let tcp_entrances: Vec<_> =
        artefact.entrances.iter().filter(|e| !e.camouflage.is_udp()).cloned().collect();
    if tcp_entrances.is_empty() {
        error!("refusing: every entrance in the artefact is UDP-shaped, nothing to dial");
        return ExitCode::FAILURE;
    }

    match EdgeSession::connect_first_available(&tcp_entrances, &mesh_id, client_static_priv, exit, 0)
        .await
    {
        Ok((idx, session)) => {
            info!(entrance = %tcp_entrances[idx].addr, "mirage-subscriber-client: connected to edge");
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
    let mut rx = client.supervisor.status();
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
async fn run_proxy(artefact: &EntranceArtefact, args: &Args) -> ExitCode {
    // `validate()` already accepted the mode, so the fallback is unreachable;
    // it only keeps the fatal document well-formed if that ever changes.
    let requested = exit_preference_from_mode(&artefact.mode).unwrap_or(ExitPreference::Auto);
    let fail = |reason: Option<EdgeReason>| {
        report_fatal(args.status_file.as_deref(), &requested, reason);
        ExitCode::FAILURE
    };

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

    let scope = match EgressScope::from_services(&args.service) {
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
            TunMode::Full if !args.service.is_empty() => {
                error!("refusing: --tun-mode full does not take --service");
                return fail(None);
            }
            TunMode::Split if args.service.is_empty() => {
                error!("refusing: --tun-mode split requires --service");
                return fail(None);
            }
            _ => {}
        }
        Some(mode)
    } else {
        None
    };

    if transport == Transport::Tun {
        match default_iface() {
            Ok(iface) => {
                info!(iface = %iface, "pinning the edge carrier to the physical NIC");
                mirage_protocol::driver::set_carrier_interface(Some(iface));
            }
            Err(e) => {
                error!(error = %e, "refusing: cannot find the physical NIC for the edge carrier");
                return fail(None);
            }
        }
    }

    info!(
        entrances = artefact.entrances.len(),
        mode = ?artefact.mode,
        transport = %args.transport,
        services = args.service.len(),
        "mirage-subscriber-client: artefact validated, assembling local proxy"
    );

    let options = SubscriberClientOptions {
        socks5_bind: args.bind,
        status_file: args.status_file.clone(),
        scope: scope.clone(),
        start_socks: transport != Transport::Http,
        ..Default::default()
    };

    let client = match SubscriberClient::build(artefact, options).await {
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
        if let Err(e) = tun_mode::write_config(&config_path, args.bind, &args.bypass_cidr, mode, &args.service)
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
