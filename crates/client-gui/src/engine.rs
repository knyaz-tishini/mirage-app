//! The engine inside the window.
//!
//! One application, one process. The window owns the edge session, the local
//! SOCKS5 listener and the tunnel device. The device itself is a separate
//! process (`mirage2tun`) — it needs the adapter, the way Amnezia's tunnel
//! carrier does. There is no child client process here and nothing speaks
//! HTTP to one.
//!
//! The configuration decides the target and the protocol. The engine follows
//! it: a new configuration replaces the session in place, so the listener,
//! the window and the tunnel device all stay up — a target change is a
//! change of peer, not a rebuild of the machine.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use mirage_subscriber_client::artefact::DeliveryArtefact;
use mirage_subscriber_client::assembly::{SubscriberClient, SubscriberClientOptions};
use mirage_subscriber_client::scope::EgressScope;
use mirage_subscriber_client::session::PlaneConsumer;
use mirage_subscriber_client::status::EdgeState;
use mirage_subscriber_client::supervisor_slot::SupervisorSlot;
use mirage_subscriber_client::tun_mode::{self, TunMode, WireMode};

use crate::backend::{EngineLink, EngineStatus};

/// The local SOCKS5 port the tunnel device chains through.
fn socks5_bind() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 1080))
}

/// How many stream failures tear the session down.
fn stream_failure_threshold() -> u32 {
    3
}

/// What the engine thread is told to do. The window never touches the session
/// itself: it tells the engine, the engine does it.
enum Command {
    /// A new configuration: the target and the protocol.
    Adopt {
        json: String,
        reply: std::sync::mpsc::Sender<Result<(), String>>,
    },
    /// The link goes up.
    Connect,
    /// The link goes down.
    Disconnect,
    /// The exit: `auto` or an ISO alpha-2 code.
    Exit(String),
    /// Sites and apps for the capture.
    Scope {
        services: Vec<String>,
        apps: Vec<String>,
        bypass_services: Vec<String>,
        bypass_apps: Vec<String>,
    },
    /// How the traffic leaves this machine: through the device, or through the
    /// local port.
    Carry { tun: bool },
    /// The addresses the device must never capture — every entrance we know.
    RelayAddrs(Vec<String>),
    /// Stop the engine.
    Stop,
}

/// The engine inside this process.
pub struct LocalEngine {
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
    status: Arc<Mutex<EngineStatus>>,
    /// The line from the last bind attempt. The window takes it once.
    bind_note: Arc<Mutex<Option<String>>>,
    /// Picture only. No session, no listener, no tunnel device, no device file.
    picture: bool,
}

impl LocalEngine {
    /// Start the engine thread. It comes up with the session off; the window
    /// tells it what to do.
    pub fn start() -> Self {
        let (commands, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let status = Arc::new(Mutex::new(EngineStatus::default()));
        let thread_status = Arc::clone(&status);
        let spawned = std::thread::Builder::new()
            .name("mirage-engine".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        tracing::error!(error = %error, "engine: no runtime");
                        return;
                    }
                };
                runtime.block_on(async move {
                    let mut engine = Engine::new(thread_status);
                    loop {
                        // A tick without a command: the device is watched here,
                        // so a device that dies is noticed even when the
                        // window is idle.
                        match tokio::time::timeout(
                            std::time::Duration::from_millis(500),
                            receiver.recv(),
                        )
                        .await
                        {
                            Ok(Some(command)) => engine.handle(command).await,
                            Ok(None) => break,
                            Err(_) => engine.poll_tun(),
                        }
                    }
                    engine.shutdown();
                });
            });
        if let Err(error) = spawned {
            tracing::error!(error = %error, "engine: thread refused");
        }
        Self { commands, status, bind_note: Arc::new(Mutex::new(None)), picture: false }
    }

    /// The window and nothing else. Clicks move the picture. Nothing is dialed,
    /// spawned, or written.
    pub fn picture() -> Self {
        let (commands, receiver) = tokio::sync::mpsc::unbounded_channel();
        drop(receiver);
        let mut snapshot = EngineStatus::default();
        snapshot.online = true;
        snapshot.phase = "off".into();
        snapshot.mode = "socks".into();
        Self {
            commands,
            status: Arc::new(Mutex::new(snapshot)),
            bind_note: Arc::new(Mutex::new(None)),
            picture: true,
        }
    }

    fn paint_link(&self, up: bool) {
        let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        status.online = true;
        status.tun = up;
        status.tun_alive = up;
        status.phase = if up { "connected".into() } else { "off".into() };
    }

    fn send(&self, command: Command) {
        if self.commands.send(command).is_err() {
            tracing::warn!("engine: the engine thread is gone");
        }
    }

    /// Put the client back on the cabinet device.
    ///
    /// The cabinet and a configuration are two different entrances, and the
    /// session changes from one to the other in place — nothing is respawned.
    pub fn adopt_device(&self) -> Result<(), String> {
        if self.picture {
            return Ok(());
        }
        let path = crate::backend::bound_device_path().map_err(|error| error.to_string())?;
        let text = std::fs::read_to_string(&path)
            .map_err(|_| crate::text::tr("bind.not_started").to_owned())?;
        self.adopt_artefact(&text)
    }

    /// Take a configuration document: the target and the protocol.
    ///
    /// The document is written down, so the next start of the window knows
    /// where it was. The session takes it in place — the listener, the window
    /// and the tunnel device stay up.
    pub fn adopt_artefact(&self, json: &str) -> Result<(), String> {
        if self.picture {
            return Ok(());
        }
        let json = json.trim();
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(|_| crate::text::tr("bind.not_artefact").to_owned())?;
        if !value.is_object() {
            return Err(crate::text::tr("bind.not_artefact").into());
        }
        if let Ok(dir) = crate::backend::exe_dir() {
            let dir = dir.join("standalone");
            if std::fs::create_dir_all(&dir).is_ok() {
                let _ = std::fs::write(dir.join("artefact.json"), json);
            }
        }
        let (reply, answer) = std::sync::mpsc::channel();
        self.send(Command::Adopt { json: json.to_owned(), reply });
        // The window asks once, at the moment of the change, and waits for the
        // answer: a configuration that was not accepted must not look like a
        // live one. The wait is short — the engine swaps the session in
        // place, it does not dial — so the window never stands still.
        answer
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| format!("the engine did not answer: {error}"))?
    }
}

impl EngineLink for LocalEngine {
    fn status(&self) -> EngineStatus {
        self.status.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn connect(&self) {
        if self.picture {
            self.paint_link(true);
            return;
        }
        self.send(Command::Connect);
    }

    fn disconnect(&self) {
        if self.picture {
            self.paint_link(false);
            return;
        }
        self.send(Command::Disconnect);
    }

    fn set_listeners(&self, socks: bool, _http: bool) {
        if self.picture {
            return;
        }
        if socks {
            // The local port is the way out when the device is not used.
            self.send(Command::Carry { tun: false });
        }
    }

    fn set_tun(&self, enabled: bool) {
        if self.picture {
            self.paint_link(enabled);
            return;
        }
        self.send(Command::Carry { tun: enabled });
    }

    fn set_scope(
        &self,
        services: &[String],
        apps: &[String],
        bypass_services: &[String],
        bypass_apps: &[String],
    ) {
        if self.picture {
            return;
        }
        self.send(Command::Scope {
            services: services.to_vec(),
            apps: apps.to_vec(),
            bypass_services: bypass_services.to_vec(),
            bypass_apps: bypass_apps.to_vec(),
        });
    }

    fn set_exit(&self, exit: &str) {
        if self.picture {
            return;
        }
        self.send(Command::Exit(exit.to_owned()));
    }

    fn bind_claim(&self, text: &str) {
        if self.picture {
            return;
        }
        let text = text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        let note = Arc::clone(&self.bind_note);
        let commands = self.commands.clone();
        std::thread::spawn(move || {
            let line = match redeem(&text) {
                Ok(json) => {
                    // The device file is the credential the client keeps.
                    if let Ok(path) = crate::backend::bound_device_path() {
                        let _ = std::fs::write(&path, &json);
                    }
                    // The session takes it at once: no respawn, no child.
                    let (reply, answer) = std::sync::mpsc::channel();
                    if commands.send(Command::Adopt { json, reply }).is_ok() {
                        let _ = answer.recv_timeout(std::time::Duration::from_secs(20));
                    }
                    crate::text::tr("bind.ok").to_owned()
                }
                Err(error) => bind_error(&error),
            };
            *note.lock().unwrap_or_else(PoisonError::into_inner) = Some(line);
        });
    }

    fn take_bind_note(&self) -> Option<String> {
        self.bind_note.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    fn is_bound(&self) -> bool {
        crate::backend::bound_device_path().map(|path| path.is_file()).unwrap_or(false)
    }

    fn cabinet_open(&self) -> bool {
        self.is_bound() && crate::backend::cabinet_marker_present()
    }

    fn bound_name(&self) -> Option<String> {
        if !self.is_bound() {
            return None;
        }
        let path = crate::backend::bound_device_path().ok()?;
        let text = std::fs::read_to_string(path).ok()?;
        let value: serde_json::Value = serde_json::from_str(&text).ok()?;
        let label = value.get("label")?.as_str()?.trim();
        if label.is_empty()
            || label.len() > 48
            || !label.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return None;
        }
        Some(label.to_owned())
    }

    fn unlink(&self) {
        if self.picture {
            return;
        }
        self.disconnect();
        if let Ok(path) = crate::backend::bound_device_path() {
            let _ = std::fs::remove_file(path);
        }
    }

    fn relay_addrs(&self, addrs: &[String]) {
        if self.picture {
            return;
        }
        self.send(Command::RelayAddrs(addrs.to_vec()));
    }
}

/// The state the engine thread owns.
struct Engine {
    status: Arc<Mutex<EngineStatus>>,
    /// The current configuration: the target and the protocol.
    artefact: Option<DeliveryArtefact>,
    /// The live session and the local listener.
    client: Option<SubscriberClient>,
    /// The tunnel device, when it is up. The window owns the process.
    tun: Option<TunDevice>,
    /// The address the running device was configured with -- the one the
    /// box assigned to the live session.
    tun_addr: Option<String>,
    /// The traffic leaves through the device, not through the local port.
    carry_tun: bool,
    /// The destinations the capture sends through.
    scope: EgressScope,
    /// The sites the capture sends through.
    services: Vec<String>,
    /// The local programs the capture sends through.
    apps: Vec<String>,
    /// Full: everything through the chain. Split: only the listed sites.
    tun_mode: TunMode,
    /// The addresses the device must never capture.
    relay_addrs: Vec<String>,
    /// The status watcher for the live session.
    watch: Option<tokio::task::JoinHandle<()>>,
    /// The device was put back on the byte-stream path.
    ///
    /// The artefact named a QUIC face, so the device asked for whole IP
    /// packets -- but the live session carries flows, not packets, and the
    /// raw-IP command would be refused every second. The datapath is
    /// demoted once, and the log says why.
    wire_demoted: bool,
}

impl Engine {
    fn new(status: Arc<Mutex<EngineStatus>>) -> Self {
        let mut snapshot = EngineStatus::default();
        snapshot.online = true;
        snapshot.phase = "off".into();
        snapshot.mode = "socks".into();
        *status.lock().unwrap_or_else(PoisonError::into_inner) = snapshot;
        Self {
            status,
            artefact: None,
            client: None,
            tun: None,
            tun_addr: None,
            carry_tun: false,
            scope: EgressScope::all(),
            services: Vec::new(),
            apps: Vec::new(),
            tun_mode: TunMode::Full,
            relay_addrs: Vec::new(),
            watch: None,
            wire_demoted: false,
        }
    }

    async fn handle(&mut self, command: Command) {
        match command {
            Command::Adopt { json, reply } => {
                let result = self.adopt(&json).await;
                let _ = reply.send(result);
            }
            Command::Connect => self.connect().await,
            Command::Disconnect => self.disconnect(),
            Command::Exit(label) => {
                if let Some(client) = &self.client {
                    if !client.supervisor.current().set_exit_label(&label) {
                        tracing::warn!(exit = %label, "engine: unknown exit label");
                    }
                }
                self.publish(|status| status.exit = label);
            }
            Command::Scope { services, apps, bypass_services, bypass_apps } => {
                self.scope = EgressScope::all();
                let _ = self.scope.set_lists(&services, &apps);
                if let Some(client) = &self.client {
                    let _ = client.scope.set_lists(&services, &apps);
                }
                self.services = services.clone();
                self.apps = apps.clone();
                self.tun_mode =
                    if services.is_empty() && apps.is_empty() { TunMode::Full } else { TunMode::Split };
                self.publish(|status| {
                    status.services = services.clone();
                    status.apps = apps.clone();
                    status.bypass_services = bypass_services.clone();
                    status.bypass_apps = bypass_apps.clone();
                });
                // The device reads the lists when it starts. If it is up, the
                // lists changed under it — write the config again and let it
                // come back.
                self.restart_tun();
            }
            Command::Carry { tun } => {
                self.carry_tun = tun;
                self.apply_carry();
            }
            Command::RelayAddrs(addrs) => {
                if addrs == self.relay_addrs {
                    // Nothing changed: the device keeps running.
                    return;
                }
                self.relay_addrs = addrs;
                self.restart_tun();
            }
            Command::Stop => self.shutdown(),
        }
    }

    /// A new configuration: the target and the protocol.
    ///
    /// The session is replaced in place. The listener, the window and the
    /// device stay up — the peer changes, the machine does not.
    async fn adopt(&mut self, json: &str) -> Result<(), String> {
        let artefact = DeliveryArtefact::from_json(json).map_err(|error| error.to_string())?;
        self.artefact = Some(artefact.clone());
        // A new configuration gets a fresh verdict about the datapath: the
        // demotion belonged to the old one.
        self.wire_demoted = false;
        if let Some(client) = &self.client {
            client
                .adopt_artefact(&artefact, stream_failure_threshold())
                .await
                .map_err(|error| error.to_string())?;
            self.watch = Some(self.spawn_watch(Arc::clone(&client.supervisor)));
            self.publish(|status| status.phase = "connecting".into());
        }
        Ok(())
    }

    /// The link goes up: the session starts and the local listener comes with
    /// it.
    async fn connect(&mut self) {
        let Some(artefact) = self.artefact.clone() else {
            tracing::warn!("engine: connect without a configuration");
            return;
        };
        if self.client.is_none() {
            let options = SubscriberClientOptions {
                socks5_bind: socks5_bind(),
                start_socks: true,
                scope: self.scope.clone(),
                ..Default::default()
            };
            match SubscriberClient::build_from_delivery(&artefact, options).await {
                Ok(client) => {
                    self.watch = Some(self.spawn_watch(Arc::clone(&client.supervisor)));
                    self.client = Some(client);
                    self.publish(|status| {
                        status.socks = true;
                        status.mode =
                            if self.carry_tun { "tun".into() } else { "socks".into() };
                    });
                }
                Err(error) => {
                    tracing::error!(error = %error, "engine: the session refused to start");
                    self.publish(|status| {
                        status.error = Some(error.to_string());
                        status.phase = "refused".into();
                    });
                    return;
                }
            }
        }
        self.apply_carry();
    }

    /// The link goes down: the device and the session stop.
    fn disconnect(&mut self) {
        self.stop_tun();
        if let Some(watch) = self.watch.take() {
            watch.abort();
        }
        if let Some(client) = self.client.take() {
            client.shutdown();
        }
        self.publish(|status| {
            status.phase = "off".into();
            status.socks = false;
            status.tun = false;
            status.tun_alive = false;
            status.error = None;
        });
    }

    /// How the traffic leaves this machine.
    fn apply_carry(&mut self) {
        // Who owns the plane's packets follows the carry: the tunnel device
        // wants them raw, the SOCKS5 flows want them in the client's own
        // userspace stack. One owner per plane - so the switch redials.
        if let Some(client) = &self.client {
            client.supervisor.current().set_plane_consumer(if self.carry_tun {
                PlaneConsumer::RawIp
            } else {
                PlaneConsumer::Stack
            });
        }
        if self.carry_tun {
            // The device comes up BEFORE the link.
            //
            // The device chains through the local port, and the local port is
            // the link itself.  While the link is not there, the device waits
            // for it: the machine keeps its own way out, the switch is not
            // up, and the driver -- the thing that lets the applications out
            // of the tunnel -- comes up with the device.
            self.start_tun();
        } else {
            self.stop_tun();
        }
        let mode = if self.carry_tun { "tun" } else { "socks" };
        self.publish(|status| status.mode = mode.into());
    }

    /// The datapath the device speaks to the client.
    ///
    /// A QUIC face is the mono-protocol plane: the whole IP packets ride the
    /// QUIC datagrams of the edge session, and the device hands them over
    /// the raw-IP command. Every other face keeps the byte-stream path.
    ///
    /// A live session without a plane demotes the path: the device must not
    /// ask for whole packets that nobody will carry.
    fn wire_mode(&self) -> WireMode {
        if self.wire_demoted {
            return WireMode::Stack;
        }
        match &self.artefact {
            Some(artefact) => tun_mode::wire_mode_for(artefact),
            // No configuration yet: the byte-stream path.
            None => WireMode::Stack,
        }
    }

    /// The address the tunnel adapter must carry right now.
    ///
    /// The address the box assigned to the live session, in CIDR form. A box
    /// that assigned nothing -- no session yet, or a box of the older wire
    /// versions -- gets the address the client always had.
    fn assigned_tun_addr(&self) -> String {
        self.client
            .as_ref()
            .and_then(|client| client.assigned_client_address())
            .unwrap_or_else(|| tun_mode::DEFAULT_TUN_ADDR.to_owned())
    }

    /// The live session carries no packet plane: put the device back on the
    /// byte-stream path.
    ///
    /// The device asked for whole IP packets because the artefact named a
    /// QUIC face, but the session that came up carries flows. The raw-IP
    /// command would be refused every second. The datapath is demoted once
    /// and the device comes back and reads the new config.
    fn demote_wire_mode(&mut self) {
        if self.wire_demoted {
            return;
        }
        if self.wire_mode() != WireMode::RawIp {
            return;
        }
        let Some(client) = &self.client else {
            return;
        };
        let Some(session) = client.supervisor.current().current() else {
            return;
        };
        if session.has_plane() {
            return;
        }
        self.wire_demoted = true;
        tracing::warn!(
            entrance = %session.entrance_label(),
            "engine: the live session carries no packet plane - the device drops to the byte-stream path"
        );
        self.restart_tun();
    }

    /// The device goes up. It is a separate process because it needs the
    /// adapter; the window keeps it and brings it back when it dies.
    fn start_tun(&mut self) {
        if self.tun.is_some() {
            return;
        }
        let config = match tun_config_path() {
            Ok(path) => path,
            Err(error) => {
                tracing::error!(error = %error, "engine: no place for the device config");
                return;
            }
        };
        let bin = match tun_binary() {
            Ok(path) => path,
            Err(error) => {
                tracing::error!(error = %error, "engine: no device binary");
                return;
            }
        };
        // The address the box assigned to this client: the adapter must
        // carry exactly it, or the box drops the packets of this peer as a
        // mismatch. A box that assigns nothing (an old one) gets the address
        // the client always had.
        let tun_addr = self.assigned_tun_addr();
        let write = tun_mode::write_config(
            &config,
            socks5_bind(),
            &tun_addr,
            &[],
            self.tun_mode,
            self.wire_mode(),
            &self.services,
            &self.apps,
            &[],
            &[],
            &self.relay_addrs,
        );
        if let Err(error) = write {
            tracing::error!(error = %error, "engine: the device config was refused");
            return;
        }
        tracing::info!(tun_addr = %tun_addr, "engine: the device config was written");
        let mut child = match tun_mode::spawn(&bin, &config) {
            Ok(child) => child,
            Err(error) => {
                tracing::error!(error = %error, "engine: the device refused to start");
                return;
            }
        };
        // The pipe to the device: when it closes, the device gives the routes
        // back and exits by itself. A device that does not go is killed.
        let stdin = child.stdin.take();
        tracing::info!(bin = %bin.display(), "engine: the device is up");
        self.tun = Some(TunDevice { child, stdin });
        self.tun_addr = Some(tun_addr);
        self.publish(|status| {
            status.tun = true;
            status.tun_alive = true;
        });
    }

    /// The device goes down.
    ///
    /// The pipe closes first: the device takes the routes back by itself. Only
    /// a device that ignores that is killed — and a killed device would leave
    /// its routes behind, which is what makes the machine's internet vanish.
    fn stop_tun(&mut self) {
        if let Some(mut device) = self.tun.take() {
            device.stdin.take();
            self.tun_addr = None;
            // The device has to take the routes back before it goes. A device
            // whose queue is full takes a while to notice the closed pipe and
            // to drain, and three seconds was not enough: the device was
            // killed with its routes still up, which is exactly the state
            // where the machine's internet vanishes. Ten seconds is still
            // shorter than the time a person waits for the window to answer.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                match device.child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => {
                        if std::time::Instant::now() >= deadline {
                            tracing::warn!("engine: the device did not go — killing it");
                            let _ = device.child.kill();
                            let _ = device.child.wait();
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    Err(error) => {
                        tracing::error!(error = %error, "engine: the device cannot be asked");
                        let _ = device.child.kill();
                        break;
                    }
                }
            }
        }
        self.publish(|status| {
            status.tun = false;
            status.tun_alive = false;
        });
    }

    /// Is the device still there?
    fn poll_tun(&mut self) {
        // The datapath follows the live session: a session without a plane
        // cannot serve whole packets, and the device must stop asking for
        // them.
        self.demote_wire_mode();
        // The address the box assigned can change under the device: the
        // session comes up after the device did, and a session that comes
        // back may get another address. The adapter must carry the address
        // of the LIVE session, or the box drops the packets as a mismatch.
        let assigned = self.assigned_tun_addr();
        if self.tun.is_some() && self.tun_addr.as_deref() != Some(assigned.as_str()) {
            tracing::info!(
                tun_addr = %assigned,
                "engine: the assigned address changed -- the device comes back"
            );
            self.restart_tun();
            return;
        }
        let Some(device) = self.tun.as_mut() else {
            return;
        };
        match device.child.try_wait() {
            Ok(Some(_)) => {
                tracing::warn!("engine: the device died");
                self.tun = None;
                self.publish(|status| {
                    status.tun = false;
                    status.tun_alive = false;
                });
                if self.carry_tun {
                    self.start_tun();
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::error!(error = %error, "engine: the device cannot be asked");
            }
        }
    }

    /// The device's lists changed under it: write the config again and let it
    /// come back.
    fn restart_tun(&mut self) {
        if self.tun.is_none() {
            return;
        }
        self.stop_tun();
        if self.carry_tun {
            self.start_tun();
        }
    }

    /// Watch the live session and paint what it says.
    fn spawn_watch(&self, slot: Arc<SupervisorSlot>) -> tokio::task::JoinHandle<()> {
        let status = Arc::clone(&self.status);
        tokio::spawn(async move {
            let mut rx = slot.current().status();
            loop {
                let snapshot = rx.borrow().clone();
                {
                    let mut guard = status.lock().unwrap_or_else(PoisonError::into_inner);
                    guard.phase = phase_of(snapshot.state);
                    guard.tun_alive = snapshot.state == EdgeState::Connected;
                    guard.error = snapshot.reason.as_ref().map(|reason| format!("{reason:?}"));
                    guard.exit = snapshot.exit.requested.clone();
                }
                if rx.changed().await.is_err() {
                    break;
                }
            }
        })
    }

    fn publish(&self, edit: impl FnOnce(&mut EngineStatus)) {
        let mut guard = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        edit(&mut guard);
    }

    fn shutdown(&mut self) {
        self.stop_tun();
        if let Some(watch) = self.watch.take() {
            watch.abort();
        }
        if let Some(client) = self.client.take() {
            client.shutdown();
        }
    }
}

/// The device process. The window keeps it; it is not the client's child.
///
/// The pipe in `stdin` is the way out: the window closes it when the link
/// goes down, and the device takes the routes back by itself. A device that
/// ignores that is killed — and a killed device leaves its routes behind,
/// which is what makes the machine's internet vanish.
struct TunDevice {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
}

/// Where the device config lives: next to the window's own files.
fn tun_config_path() -> Result<PathBuf, String> {
    let dir = crate::backend::exe_dir().map_err(|error| error.to_string())?;
    Ok(dir.join("tun.json"))
}

/// The device binary: next to the window, by name.
fn tun_binary() -> Result<PathBuf, String> {
    let dir = crate::backend::exe_dir().map_err(|error| error.to_string())?;
    let bin = dir.join("mirage2tun.exe");
    if bin.is_file() {
        Ok(bin)
    } else {
        Err(format!("{} is not there", bin.display()))
    }
}

/// The phase string the window reads.
fn phase_of(state: EdgeState) -> String {
    match state {
        EdgeState::Connecting => "connecting".into(),
        EdgeState::Connected => "connected".into(),
        EdgeState::Reconnecting => "reconnecting".into(),
        EdgeState::Refused => "refused".into(),
        EdgeState::Fatal => "fatal".into(),
    }
}

/// Redeem a claim at the panel and return the artefact document.
///
/// The panel is over the network, so this runs on its own thread with its own
/// runtime: the window never waits on it.
fn redeem(text: &str) -> Result<String, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "transport".to_owned())?;
    runtime.block_on(mirage_subscriber_client::claim::redeem_artefact_json(
        crate::backend::PANEL_ORIGIN,
        text,
    ))
}

/// The window's line for a refused claim.
fn bind_error(token: &str) -> String {
    match token {
        "spent" => crate::text::tr("bind.used").to_owned(),
        "pending" => crate::text::tr("bind.soon").to_owned(),
        "missing" | "unknown" | "empty" => crate::text::tr("bind.missing").to_owned(),
        "refused" => crate::text::tr("bind.refused").to_owned(),
        _ => format!("{}: {token}", crate::text::tr("bind.fail")),
    }
}
