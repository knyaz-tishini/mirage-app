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
        Self { commands, status, bind_note: Arc::new(Mutex::new(None)) }
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
        // place, it does not dial — so the window never stands still. The
        // window's thread paints while it waits: a long wait here is a white
        // window, and a white window looks dead.
        let began = std::time::Instant::now();
        let result = answer
            .recv_timeout(std::time::Duration::from_millis(2500))
            .map_err(|error| format!("the engine did not answer: {error}"))?;
        tracing::info!(
            ms = began.elapsed().as_millis() as u64,
            ok = result.is_ok(),
            "engine: the configuration was taken in"
        );
        result
    }
}

impl EngineLink for LocalEngine {
    fn status(&self) -> EngineStatus {
        self.status.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn connect(&self) {
        self.send(Command::Connect);
    }

    fn disconnect(&self) {
        self.send(Command::Disconnect);
    }

    fn set_listeners(&self, socks: bool, _http: bool) {
        if socks {
            // The local port is the way out when the device is not used.
            self.send(Command::Carry { tun: false });
        }
    }

    fn set_tun(&self, enabled: bool) {
        self.send(Command::Carry { tun: enabled });
    }

    fn set_scope(
        &self,
        services: &[String],
        apps: &[String],
        bypass_services: &[String],
        bypass_apps: &[String],
    ) {
        self.send(Command::Scope {
            services: services.to_vec(),
            apps: apps.to_vec(),
            bypass_services: bypass_services.to_vec(),
            bypass_apps: bypass_apps.to_vec(),
        });
    }

    fn set_exit(&self, exit: &str) {
        self.send(Command::Exit(exit.to_owned()));
    }

    fn bind_claim(&self, text: &str) {
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
        self.disconnect();
        if let Ok(path) = crate::backend::bound_device_path() {
            let _ = std::fs::remove_file(path);
        }
    }

    fn relay_addrs(&self, addrs: &[String]) {
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
    /// The tunnel device. It has its own thread: it takes seconds to come up
    /// and seconds to go, and the engine must answer the window all the time.
    device: Device,
    /// The address the running device was configured with -- the one the
    /// box assigned to the live session.
    tun_addr: Option<String>,
    /// The address the box has assigned lately, and how long it has been
    /// that address. The device is only restarted when the new address has
    /// held still for a while: the box names one address while the session
    /// comes up and another one when it is live, and a device that is torn
    /// down for every word costs seconds of the machine's way out.
    addr_pending: Option<(String, std::time::Instant)>,
    /// The traffic leaves through the device, not through the local port.
    carry_tun: bool,
    /// The destinations the capture sends through.
    scope: EgressScope,
    /// The sites the capture sends through.
    services: Vec<String>,
    /// The local programs the capture sends through.
    apps: Vec<String>,
    /// The sites that stay DIRECT -- the device must never capture them.
    ///
    /// This is the "all except" side of the split: the capture carries
    /// everything else, and these destinations leave the machine the way
    /// they always did. The device reads the list when it starts, so the
    /// lists are written into its config.
    bypass_services: Vec<String>,
    /// The local programs that stay DIRECT.
    bypass_apps: Vec<String>,
    /// Full: everything through the chain. Split: only the listed sites.
    tun_mode: TunMode,
    /// The addresses the device must never capture.
    relay_addrs: Vec<String>,
    /// The status watcher for the live session.
    watch: Option<tokio::task::JoinHandle<()>>,
    /// The engine wants the tunnel up. The device's own answer is only
    /// believed when the engine wants it: a device that is going down still
    /// answers "the tunnel is up" for a moment.
    tun_wanted: Arc<std::sync::atomic::AtomicBool>,
    /// The probe's stop flag.
    probe_stop: Arc<std::sync::atomic::AtomicBool>,
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
        snapshot.session_phase = "off".into();
        snapshot.mode = "socks".into();
        *status.lock().unwrap_or_else(PoisonError::into_inner) = snapshot;
        let tun_wanted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let engine = Self {
            status,
            artefact: None,
            client: None,
            device: Device::start(),
            tun_addr: None,
            addr_pending: None,
            carry_tun: false,
            scope: EgressScope::all(),
            services: Vec::new(),
            apps: Vec::new(),
            bypass_services: Vec::new(),
            bypass_apps: Vec::new(),
            tun_mode: TunMode::Full,
            relay_addrs: Vec::new(),
            watch: None,
            tun_wanted: Arc::clone(&tun_wanted),
            probe_stop: Arc::clone(&probe_stop),
            wire_demoted: false,
        };
        engine.spawn_tun_probe(tun_wanted, probe_stop);
        engine
    }

    /// The device's own answer about the tunnel.
    ///
    /// The device owns the adapter, the routes and the switch, so it is the
    /// one that knows whether the traffic really goes through the tunnel. The
    /// process being up is not the tunnel being up: the adapter takes seconds
    /// to come up, and the window that says "connected" before that lies to
    /// the person who is waiting for it.
    ///
    /// The probe runs on its own thread and asks the device's management port.
    /// A device that is not there yet answers nothing, and that is "the tunnel
    /// is not up" -- but only after it keeps quiet: one question that comes
    /// back late is a busy device, not a dead tunnel, and the window that
    /// says "reconnecting" because of it flickers for nothing.
    fn spawn_tun_probe(
        &self,
        wanted: Arc<std::sync::atomic::AtomicBool>,
        stop: Arc<std::sync::atomic::AtomicBool>,
    ) {
        let status = Arc::clone(&self.status);
        let _ = std::thread::Builder::new()
            .name("mirage-tun-probe".into())
            .spawn(move || {
                // The misses in a row, and the word the window believes.
                let mut misses = 0u32;
                let mut believed = false;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let asked = wanted.load(std::sync::atomic::Ordering::Relaxed);
                    let answer = if asked {
                        device_tunnel_active()
                    } else {
                        Ok(false)
                    };
                    match answer {
                        Ok(true) => {
                            misses = 0;
                            believed = true;
                        }
                        Ok(false) => {
                            // The device answered, and it says the tunnel is
                            // not up.  That is a sure word.
                            misses = 0;
                            believed = false;
                        }
                        Err(reason) => {
                            misses += 1;
                            if misses == 1 {
                                tracing::debug!(reason = %reason, "engine: the device did not answer about the tunnel");
                            }
                            if misses >= TUN_PROBE_MISSES_BEFORE_DOWN {
                                if believed {
                                    tracing::warn!(reason = %reason, misses, "engine: the device stopped answering about the tunnel");
                                }
                                believed = false;
                            }
                        }
                    }
                    let active = believed;
                    let mut guard = status.lock().unwrap_or_else(PoisonError::into_inner);
                    if guard.tun != active {
                        // The device's own word, and the change of it: the
                        // window's "connected" is this line.
                        tracing::info!(tun = active, "engine: the device says the tunnel");
                    }
                    guard.tun = active;
                    let session = guard.session_phase.clone();
                    let tun_mode = guard.mode == "tun";
                    // The session can be up while the tunnel is still coming
                    // up. The window says "connecting" then, and "connected"
                    // only when the device says the tunnel carries.
                    guard.tun_alive = session == "connected" && (!tun_mode || active);
                    guard.phase = if tun_mode && session == "connected" && !active {
                        "connecting".to_owned()
                    } else {
                        session
                    };
                    drop(guard);
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
            });
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
                // The same scope must not cost the device its life.
                //
                // The window repeats its wish while the link is not up yet, and
                // the device takes a long time to come up (the adapter alone is
                // seconds).  Restarting it on every repeat means the device is
                // torn down while it is still starting -- and a device that is
                // killed mid-start leaves its routes behind: the machine loses
                // its way out.  So the device is only restarted when the lists
                // really changed.
                let unchanged = self.services == services
                    && self.apps == apps
                    && self.bypass_services == bypass_services
                    && self.bypass_apps == bypass_apps;
                self.scope = EgressScope::all();
                let _ = self.scope.set_lists(&services, &apps);
                if let Some(client) = &self.client {
                    let _ = client.scope.set_lists(&services, &apps);
                }
                self.services = services.clone();
                self.apps = apps.clone();
                self.bypass_services = bypass_services.clone();
                self.bypass_apps = bypass_apps.clone();
                self.tun_mode =
                    if services.is_empty() && apps.is_empty() { TunMode::Full } else { TunMode::Split };
                self.publish(|status| {
                    status.services = services.clone();
                    status.apps = apps.clone();
                    status.bypass_services = bypass_services.clone();
                    status.bypass_apps = bypass_apps.clone();
                });
                if unchanged {
                    return;
                }
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
            self.publish(|status| {
                status.phase = "connecting".into();
                status.session_phase = "connecting".into();
            });
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
            // The device comes up FIRST.
            //
            // The session takes seconds to build, and until it carries, the
            // traffic of the machine would go out through the physical
            // interface: the person asked for a tunnel and would get the
            // internet without it. The device brings the kill-switch with it,
            // so from this moment nothing that is not the bypass reaches the
            // internet -- not even the seconds the session needs.
            if self.carry_tun {
                self.start_tun();
            }
            // The session takes seconds to build: the status says so BEFORE
            // the work starts, or the window stands still with no word.
            tracing::info!("engine: the session is coming up");
            self.publish(|status| {
                status.phase = "connecting".into();
                status.session_phase = "connecting".into();
            });
            let began = std::time::Instant::now();
            match SubscriberClient::build_from_delivery(&artefact, options).await {
                Ok(client) => {
                    tracing::info!(
                        ms = began.elapsed().as_millis() as u64,
                        "engine: the session is up"
                    );
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
                    // The machine must not stay closed: the tunnel that was
                    // promised never came.
                    if self.carry_tun {
                        self.stop_tun();
                    }
                    self.publish(|status| {
                        status.error = Some(error.to_string());
                        status.phase = "refused".into();
                        status.session_phase = "refused".into();
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
            status.session_phase = "off".into();
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
        //
        // While the tunnel carries, the plane is PINNED: a SOCKS5 request
        // that arrives anyway must not take the plane away. The switch tears
        // the session down and dials a new one, and that costs the machine
        // its way out for half a minute -- and the box names another address
        // afterwards, so the device comes up again too. A SOCKS5 flow in
        // that state is served with an ordinary socket instead: the machine's
        // own routing sends it into the tunnel with everything else.
        if let Some(client) = &self.client {
            client.supervisor.current().set_plane_consumer(if self.carry_tun {
                PlaneConsumer::RawIp
            } else {
                PlaneConsumer::Stack
            });
            client.supervisor.current().pin_plane_consumer(self.carry_tun);
        }
        if self.carry_tun {
            if self.client.is_none() {
                // The device chains through the local port, so the port has to
                // be there first. The link comes up on its own.
                return;
            }
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
    /// QUIC face -- but the session that came up carries flows, not packets:
    /// the edge's welcome did not promise them. The raw-IP command would be
    /// refused every second, forever, and the machine would have no way
    /// out. So the datapath is demoted once, the config is written again,
    /// and the log says why.
    fn demote_wire_mode(&mut self) {
        if self.wire_demoted {
            return;
        }
        // Only a raw-IP device can be demoted.
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
        let tun_addr = self.assigned_tun_addr();
        if let Err(error) = self.write_device_config(&tun_addr) {
            tracing::error!(error = %error, "engine: the demoted device config was refused");
            return;
        }
        // The device reads its datapath when it starts: the config is
        // written, but the running device still speaks the old one. It comes
        // back and reads the new one.
        self.restart_tun();
    }

    /// The device goes up. It is a separate process because it needs the
    /// adapter; the window keeps it and brings it back when it dies.
    fn start_tun(&mut self) {
        if self.device.up || self.device.starting {
            return;
        }
        if self.device.stopping {
            // The old device is still going down: the new one comes up when
            // it is gone. Two devices on one adapter fight for it.
            self.device.restart_when_gone = true;
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
        if let Err(error) = self.write_device_config(&tun_addr) {
            tracing::error!(error = %error, "engine: the device config was refused");
            return;
        }
        tracing::info!(tun_addr = %tun_addr, "engine: the device config was written");
        // The device comes up on its own thread: it takes seconds, and the
        // engine must answer the window all the time.
        self.device.start_device(bin, config, tun_addr.clone());
        self.tun_wanted.store(true, std::sync::atomic::Ordering::Relaxed);
        self.tun_addr = Some(tun_addr);
        self.addr_pending = None;
        self.publish(|status| {
            status.tun = false;
            status.tun_alive = false;
        });
    }

    /// Write the device's configuration.
    ///
    /// The device reads this file when it starts, and it watches it while it
    /// runs: the address the box assigned is in there, and the device takes
    /// a new one on the fly. So writing this file is how a new address
    /// reaches a device that is already up.
    fn write_device_config(&self, tun_addr: &str) -> Result<(), String> {
        let config = tun_config_path().map_err(|error| error.to_string())?;
        tun_mode::write_config(
            &config,
            socks5_bind(),
            tun_addr,
            &[],
            self.tun_mode,
            self.wire_mode(),
            &self.services,
            &self.apps,
            &self.bypass_services,
            &self.bypass_apps,
            &self.relay_addrs,
        )
        .map_err(|error| error.to_string())
    }

    /// The device goes down.
    ///
    /// The pipe closes first: the device takes the routes back by itself. Only
    /// a device that ignores that is killed — and a killed device would leave
    /// its routes behind, which is what makes the machine's internet vanish.
    /// The wait happens on the device's thread; the engine only hears the
    /// news when it is over.
    fn stop_tun(&mut self) {
        if !self.device.up && !self.device.starting {
            self.publish(|status| {
                status.tun = false;
                status.tun_alive = false;
            });
            return;
        }
        tracing::info!("engine: the device is going down");
        self.tun_wanted.store(false, std::sync::atomic::Ordering::Relaxed);
        self.device.stop_device();
        self.tun_addr = None;
        self.addr_pending = None;
        self.publish(|status| {
            status.tun = false;
            status.tun_alive = false;
        });
    }

    /// Is the device still there?
    fn poll_tun(&mut self) {
        // What the device thread said.
        while let Some(event) = self.device.poll() {
            match event {
                DeviceEvent::Up { addr } => {
                    tracing::info!(tun_addr = %addr, "engine: the device is up");
                    self.device.up = true;
                    self.device.starting = false;
                    self.device.stopping = false;
                    self.tun_addr = Some(addr);
                    self.publish(|status| {
                        // The process is up -- that is not the tunnel yet. The
                        // adapter takes seconds more, and until it is there the
                        // traffic does not go through it. The probe asks the
                        // device, and the window waits for that answer.
                        status.tun = false;
                        status.tun_alive = false;
                    });
                }
                DeviceEvent::Failed { error } => {
                    tracing::error!(error = %error, "engine: the device refused to start");
                    self.device.up = false;
                    self.device.starting = false;
                    self.device.stopping = false;
                    self.publish(|status| {
                        status.tun = false;
                        status.tun_alive = false;
                        status.error = Some(error);
                    });
                }
                DeviceEvent::Gone { killed } => {
                    tracing::info!(killed, "engine: the device is gone");
                    self.device.up = false;
                    self.device.starting = false;
                    self.device.stopping = false;
                    self.tun_addr = None;
                    self.publish(|status| {
                        status.tun = false;
                        status.tun_alive = false;
                    });
                    let restart = std::mem::take(&mut self.device.restart_when_gone);
                    if restart && self.carry_tun {
                        // The device is gone: a new one takes its place.
                        self.start_tun();
                    } else if self.carry_tun && !killed {
                        // The device died on its own: it comes back.
                        self.start_tun();
                    }
                }
            }
        }
        // The address the box assigned can change under the device: the
        // session comes up after the device did, and a session that comes
        // back may get another address. The adapter must carry the address
        // of the LIVE session, or the box drops the packets as a mismatch.
        //
        // The box names one address while the session is coming up and
        // another one when it is live. A device that is torn down for every
        // word costs the machine its way out for seconds, so the new address
        // has to hold still first.
        // The box names one address while the session is coming up and
        // another one when it is live, and a session that comes back may get
        // another one again. Every word of that used to cost the machine its
        // way out: the device was torn down and brought up again, and the
        // tear-down broke the session, which named another address, which
        // tore the device down again. So the new address has to hold still
        // before the device follows it -- and the machine keeps its way out
        // while it waits.
        let assigned = self.assigned_tun_addr();
        if self.device.up && self.tun_addr.as_deref() != Some(assigned.as_str()) {
            let since = match &self.addr_pending {
                Some((known, since)) if known == &assigned => *since,
                _ => {
                    let now = std::time::Instant::now();
                    self.addr_pending = Some((assigned.clone(), now));
                    now
                }
            };
            if since.elapsed() >= ADDRESS_SETTLE {
                // The device takes the new address on the fly: the window
                // writes it into the config and the device gives the adapter
                // that address. A restart would cost the machine its way out
                // for the better part of a minute -- and the restart breaks
                // the session, which is how the address changed in the first
                // place.
                tracing::info!(
                    tun_addr = %assigned,
                    "engine: the box named another address -- the device takes it"
                );
                match self.write_device_config(&assigned) {
                    Ok(()) => {
                        self.tun_addr = Some(assigned.clone());
                        self.addr_pending = None;
                    }
                    Err(error) => {
                        tracing::error!(
                            error = %error,
                            "engine: the config could not be written -- the device comes back instead"
                        );
                        self.restart_tun();
                    }
                }
            }
        } else if self.tun_addr.as_deref() == Some(assigned.as_str()) {
            self.addr_pending = None;
        }
        // The datapath follows the live session: a session without a plane
        // cannot serve whole packets, and the device must stop asking for
        // them.
        self.demote_wire_mode();
    }

    /// The device's lists changed under it: write the config again and let it
    /// come back.
    fn restart_tun(&mut self) {
        if !self.device.up && !self.device.starting {
            return;
        }
        self.stop_tun();
        // The device has to be gone before a new one starts: two devices on
        // one adapter fight for it. The new one goes up when the old one is
        // reported gone.
        self.device.restart_when_gone = self.carry_tun;
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
                    // The session's phase is kept aside: the phase the window
                    // shows also depends on the tunnel, and the tunnel is the
                    // device's word -- the probe publishes it.
                    guard.session_phase = phase_of(snapshot.state);
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
        self.device.shutdown();
        self.probe_stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(watch) = self.watch.take() {
            watch.abort();
        }
        if let Some(client) = self.client.take() {
            client.shutdown();
        }
    }
}

/// Where the device answers about itself.
const DEVICE_API: &str = "127.0.0.1:19485";

/// How many questions in a row the device may leave unanswered before the
/// window calls the tunnel down.
///
/// The probe asks every quarter of a second.  The device is busy now and
/// then -- it drills holes, it resolves names -- and a question that comes
/// back late must not be read as a dead tunnel: the window would say
/// "reconnecting" every few seconds while the traffic keeps flowing.
const TUN_PROBE_MISSES_BEFORE_DOWN: u32 = 3;

/// How long the window waits for the device's answer.
const DEVICE_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(400);

/// Ask the device whether the tunnel carries.
///
/// The device owns the adapter and the routes, so it is the one that knows.
/// The answer comes from its management port.  A device that answers "the
/// tunnel is not up" is a sure word; a device that keeps quiet is only
/// unknown, and the difference between the two is the difference between a
/// dead tunnel and a busy one -- so the silence comes back as a reason, and
/// the caller decides what to do with it.
fn device_tunnel_active() -> Result<bool, String> {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(DEVICE_API)
        .map_err(|e| format!("the device did not open its port: {e}"))?;
    let _ = stream.set_read_timeout(Some(DEVICE_PROBE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(DEVICE_PROBE_TIMEOUT));
    let request = b"GET /status HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    stream
        .write_all(request)
        .map_err(|e| format!("the question did not reach the device: {e}"))?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| format!("the device did not finish its answer: {e}"))?;
    let text = String::from_utf8_lossy(&raw);
    let Some((_, body)) = text.split_once("\r\n\r\n") else {
        return Err("the answer of the device had no body".to_owned());
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body.trim()) else {
        return Err(format!("the answer of the device was not json: {}", body.trim()));
    };
    Ok(value.get("tun_phase").and_then(|item| item.as_str()) == Some("tun_active"))
}

/// What the device thread is told.
enum DeviceCommand {
    /// Bring the device up with this configuration.
    Start {
        bin: PathBuf,
        config: PathBuf,
        addr: String,
    },
    /// Give the routes back. The device goes by itself.
    Stop,
    /// The window is closing.
    Shutdown,
}

/// What the device thread says.
enum DeviceEvent {
    /// The device process is up. The adapter inside it takes seconds more.
    Up { addr: String },
    /// The device did not start.
    Failed { error: String },
    /// The device is gone.
    Gone { killed: bool },
}

/// How long the address the box names has to hold still before the device
/// follows it.
///
/// The tear-down of a device costs the machine its way out for half a minute
/// (the adapter alone takes seconds to come up), and it breaks the session,
/// which makes the box name another address -- and then the tear-down starts
/// all over again. Thirty seconds is longer than the box takes to settle.
const ADDRESS_SETTLE: std::time::Duration = std::time::Duration::from_secs(30);

/// The tunnel device and the thread that owns it.
///
/// The device takes seconds to come up (the adapter alone) and seconds to go
/// (it has to give the routes back). None of that may happen on the engine's
/// thread: the engine answers the window, and a window that waits on a
/// device is a window that looks dead. So the device lives here, on its own
/// thread, and the engine only hears what happened.
struct Device {
    tx: std::sync::mpsc::Sender<DeviceCommand>,
    rx: std::sync::mpsc::Receiver<DeviceEvent>,
    /// The address the running device carries.
    addr: Option<String>,
    /// The device is up (the process is running).
    up: bool,
    /// The device is being brought up.
    starting: bool,
    /// The device is being taken down.
    stopping: bool,
    /// A new device must come up as soon as this one is gone.
    restart_when_gone: bool,
}

impl Device {
    fn start() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let (out_tx, out_rx) = std::sync::mpsc::channel();
        let _ = std::thread::Builder::new()
            .name("mirage-device".into())
            .spawn(move || device_thread(rx, out_tx));
        Self { tx, rx: out_rx, addr: None, up: false, starting: false, stopping: false, restart_when_gone: false }
    }

    /// Bring the device up. The answer comes later, as an event.
    fn start_device(&mut self, bin: PathBuf, config: PathBuf, addr: String) {
        self.starting = true;
        self.stopping = false;
        self.restart_when_gone = false;
        if self.tx.send(DeviceCommand::Start { bin, config, addr }).is_err() {
            tracing::error!("engine: the device thread is gone");
            self.starting = false;
        }
    }

    /// Take the device down. The answer comes later, as an event.
    fn stop_device(&mut self) {
        if !self.up && !self.starting {
            return;
        }
        self.stopping = true;
        if self.tx.send(DeviceCommand::Stop).is_err() {
            tracing::error!("engine: the device thread is gone");
            self.stopping = false;
        }
    }

    /// What the device thread said, if it said anything.
    fn poll(&mut self) -> Option<DeviceEvent> {
        self.rx.try_recv().ok()
    }

    fn shutdown(&mut self) {
        let _ = self.tx.send(DeviceCommand::Shutdown);
    }
}

/// The device's own thread: it starts the process, watches it and takes it
/// down. Nothing here ever waits on the engine.
fn device_thread(
    rx: std::sync::mpsc::Receiver<DeviceCommand>,
    tx: std::sync::mpsc::Sender<DeviceEvent>,
) {
    let mut child: Option<std::process::Child> = None;
    let mut stdin: Option<std::process::ChildStdin> = None;
    loop {
        match rx.try_recv() {
            Ok(DeviceCommand::Start { bin, config, addr }) => {
                stop_child(&mut child, &mut stdin, &tx);
                let began = std::time::Instant::now();
                match start_child(&bin, &config) {
                    Ok((new_child, new_stdin)) => {
                        child = Some(new_child);
                        stdin = Some(new_stdin);
                        tracing::info!(
                            ms = began.elapsed().as_millis() as u64,
                            bin = %bin.display(),
                            "device: the process is up"
                        );
                        let _ = tx.send(DeviceEvent::Up { addr });
                    }
                    Err(error) => {
                        tracing::error!(error = %error, "device: the device refused to start");
                        let _ = tx.send(DeviceEvent::Failed { error });
                    }
                }
            }
            Ok(DeviceCommand::Stop) => {
                stop_child(&mut child, &mut stdin, &tx);
            }
            Ok(DeviceCommand::Shutdown) => {
                stop_child(&mut child, &mut stdin, &tx);
                break;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
        }
        // Did the device go on its own?
        if let Some(process) = child.as_mut() {
            match process.try_wait() {
                Ok(Some(status)) => {
                    tracing::warn!(code = status.code(), "device: the device died");
                    child = None;
                    stdin = None;
                    let _ = tx.send(DeviceEvent::Gone { killed: false });
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(error = %error, "device: the device cannot be asked");
                    child = None;
                    stdin = None;
                    let _ = tx.send(DeviceEvent::Gone { killed: false });
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Write the configuration and start the device process.
fn start_child(
    bin: &PathBuf,
    config: &PathBuf,
) -> Result<(std::process::Child, std::process::ChildStdin), String> {
    let mut child = tun_mode::spawn(bin, config)
        .map_err(|error| format!("{}: {error}", bin.display()))?;
    let stdin = child.stdin.take();
    Ok((child, stdin.ok_or_else(|| "the device has no pipe".to_owned())?))
}

/// Give the routes back and wait for the device to go.
///
/// The device has to take the routes back before it goes. A device whose
/// queue is full takes a while to notice the closed pipe and to drain, and
/// three seconds was not enough: the device was killed with its routes still
/// up, which is exactly the state where the machine's internet vanishes. So
/// the patience is the device's own startup time (the adapter alone takes
/// seconds): killing it earlier leaves the routes behind.
fn stop_child(
    child: &mut Option<std::process::Child>,
    stdin: &mut Option<std::process::ChildStdin>,
    tx: &std::sync::mpsc::Sender<DeviceEvent>,
) {
    let Some(mut process) = child.take() else {
        return;
    };
    // The pipe closes first: the device gives the routes back by itself.
    stdin.take();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut killed = false;
    loop {
        match process.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    tracing::warn!("device: the device did not go — killing it");
                    let _ = process.kill();
                    let _ = process.wait();
                    killed = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(error) => {
                tracing::error!(error = %error, "device: the device cannot be asked");
                let _ = process.kill();
                killed = true;
                break;
            }
        }
    }
    let _ = tx.send(DeviceEvent::Gone { killed });
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
