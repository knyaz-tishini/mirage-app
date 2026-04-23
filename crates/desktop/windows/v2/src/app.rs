use std::sync::Arc;

use mirage_core::config::ClientConfig;

use crate::runtime::{find_client_exe, ClientProcess, PollInfo, ProcessStats, ProcessStatus};

pub enum AgentCmd {
    SetConfig(ClientConfig),
    Connect,
    Disconnect,
}

enum StatsUpdate {
    Stats(ProcessStats),
    ProcessDied,
}

pub struct AppState {
    // Connection
    pub status: ProcessStatus,
    pub config: Option<ClientConfig>,
    pub process: Option<ClientProcess>,
    pub stats: Option<ProcessStats>,

    // Pending process startup (oneshot from async spawn)
    pub process_pending:
        Option<tokio::sync::oneshot::Receiver<Result<ClientProcess, String>>>,

    // Animation time for UI
    pub animation_time: f64,

    // Async runtime handle
    pub tokio_rt: Arc<tokio::runtime::Runtime>,

    // Agent commands
    pub agent_cmd_rx: tokio::sync::mpsc::Receiver<AgentCmd>,
    pub agent_cmd_tx: tokio::sync::mpsc::Sender<AgentCmd>,

    // Stats polling
    stats_rx: tokio::sync::mpsc::Receiver<StatsUpdate>,
    stats_tx: tokio::sync::mpsc::Sender<StatsUpdate>,

    // Input
    pub pending_chars: Vec<char>,
    pub pending_backspaces: u32,

    // Client exe path
    pub client_exe: Option<std::path::PathBuf>,

    // Shared log buffer for agent API (GET /logs) and UI
    pub shared_logs: Arc<tokio::sync::Mutex<Vec<String>>>,

    // Whether mirage2tun service is alive (used by TUN mode)
    pub tun_service_alive: bool,

    // Selector state (indices into the selector item arrays in draw.rs)
    // Chain: 0=classic-direct, 1=byte-cipher, 2=crypto-relay, 3=three-hop-onion, 4=telescopic
    pub sel_chain: usize,
    // Camouflage: 0=raw, 1=json-api, 2=grpc, 3=h2data
    pub sel_camouflage: usize,
    // Shaping: 0=none, 1=web-browsing, 2=cloud-api, 3=streaming
    pub sel_shaping: usize,
    // Client mode: 0=socks5, 1=http-connect, 2=tun
    pub sel_client_mode: usize,

    // Input coordination
    pub coordinator: uzor::InputCoordinator,
}

impl AppState {
    pub fn new(rt: Arc<tokio::runtime::Runtime>) -> Self {
        let (agent_tx, agent_rx) = tokio::sync::mpsc::channel(32);
        let (stats_tx, stats_rx) = tokio::sync::mpsc::channel(32);

        let client_exe = find_client_exe();

        let config = Self::load_default_config();

        Self {
            status: ProcessStatus::Stopped,
            config,
            process: None,
            stats: None,
            process_pending: None,
            animation_time: 0.0,
            tokio_rt: rt,
            agent_cmd_rx: agent_rx,
            agent_cmd_tx: agent_tx,
            stats_rx,
            stats_tx,
            pending_chars: Vec::new(),
            pending_backspaces: 0,
            client_exe,
            sel_chain: 0,
            sel_camouflage: 0,
            sel_shaping: 0,
            sel_client_mode: 0,
            shared_logs: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            tun_service_alive: false,
            coordinator: uzor::InputCoordinator::new(),
        }
    }

    fn apply_selectors_to_config(&mut self) {
        let Some(ref mut config) = self.config else { return };

        // Chain IDs in same order as draw.rs selectors
        let chain_ids = [
            "classic-direct",
            "byte-cipher-classic",
            "crypto-relay-classic",
            "three-hop-onion",
            "three-hop-telescopic",
        ];

        // Set priority: selected chain gets highest priority
        let selected_chain_id = chain_ids.get(self.sel_chain).unwrap_or(&"classic-direct");
        for chain in &mut config.chains {
            if chain.chain_id == *selected_chain_id {
                chain.priority = 100;
            } else {
                chain.priority = 0;
            }
        }

        // Apply camouflage to all chains
        let camouflage = match self.sel_camouflage {
            0 => mirage_core::config::CamouflageMode::Raw,
            1 => mirage_core::config::CamouflageMode::JsonApi,
            2 => mirage_core::config::CamouflageMode::Grpc,
            3 => mirage_core::config::CamouflageMode::H2Data,
            _ => mirage_core::config::CamouflageMode::Raw,
        };
        for chain in &mut config.chains {
            chain.camouflage = camouflage.clone();
        }

        // Apply shaping to all chains
        let shaping = match self.sel_shaping {
            0 => mirage_core::config::ShapingPreset::None,
            1 => mirage_core::config::ShapingPreset::WebBrowsing,
            2 => mirage_core::config::ShapingPreset::CloudApi,
            3 => mirage_core::config::ShapingPreset::Streaming,
            _ => mirage_core::config::ShapingPreset::None,
        };
        for chain in &mut config.chains {
            chain.shaping = shaping.clone();
        }

        // Apply client mode
        match self.sel_client_mode {
            0 => {
                // SOCKS5 — also start HTTP CONNECT for system proxy
                // (WinINET doesn't support SOCKS5, only SOCKS4)
                config.tun.enabled = false;
                config.http_proxy.enabled = true;
            }
            1 => {
                // HTTP-CONNECT
                config.tun.enabled = false;
                config.http_proxy.enabled = true;
            }
            2 => {
                // TUN — managed by mirage2tun service
                config.tun.enabled = false;
                config.http_proxy.enabled = false;
            }
            _ => {}
        }
    }

    pub fn connect(&mut self) {
        if self.status == ProcessStatus::Running || self.status == ProcessStatus::Starting {
            return;
        }
        self.apply_selectors_to_config();
        let chain_names = ["classic-direct", "byte-cipher", "crypto-relay", "three-hop-onion", "telescopic"];
        let camo_names = ["raw", "json-api", "grpc", "h2data"];
        let shape_names = ["none", "web-browse", "cloud-api", "streaming"];
        let mode_names = ["socks5", "http-connect", "tun"];
        self.add_log("CFG", &format!(
            "chain={} camo={} shape={} mode={}",
            chain_names.get(self.sel_chain).unwrap_or(&"?"),
            camo_names.get(self.sel_camouflage).unwrap_or(&"?"),
            shape_names.get(self.sel_shaping).unwrap_or(&"?"),
            mode_names.get(self.sel_client_mode).unwrap_or(&"?"),
        ));
        let Some(config) = self.config.clone() else {
            self.add_log("ERR", "no config loaded");
            return;
        };
        let Some(exe) = self.client_exe.clone() else {
            self.add_log("ERR", "mirage-client.exe not found");
            return;
        };

        self.status = ProcessStatus::Starting;

        if self.sel_client_mode == 2 {
            // TUN mode: check mirage2tun service first, then spawn non-elevated
            self.add_log("OK", "TUN mode: checking mirage2tun service...");
            let rt = self.tokio_rt.clone();
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.process_pending = Some(rx);
            rt.spawn(async move {
                let http = reqwest::Client::new();
                match http.get("http://127.0.0.1:19485/health").send().await {
                    Ok(r) if r.status().is_success() => {}
                    _ => {
                        let _ = tx.send(Err(
                            "mirage2tun service not running (run: mirage2tun.exe install)".into(),
                        ));
                        return;
                    }
                }
                let result = ClientProcess::start_simple(config, &exe, "127.0.0.1:19480").await;
                let _ = tx.send(result);
            });
        } else {
            // SOCKS5 / HTTP-CONNECT mode: spawn elevated (UAC)
            self.add_log("OK", "starting mirage-client (UAC prompt will appear)...");
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.process_pending = Some(rx);
            self.tokio_rt.spawn(async move {
                let result = ClientProcess::start(config, &exe, "127.0.0.1:19480").await;
                let _ = tx.send(result);
            });
        }
    }

    pub fn disconnect(&mut self) {
        let is_tun = self.sel_client_mode == 2;

        if is_tun {
            // Tell mirage2tun to stop TUN before killing client
            let rt = self.tokio_rt.clone();
            rt.spawn(async move {
                let client = reqwest::Client::new();
                let _ = client
                    .post("http://127.0.0.1:19485/tun/stop")
                    .send()
                    .await;
            });
        }

        // Clear system proxy BEFORE killing child — child's cleanup won't fire on kill.
        crate::system_proxy::clear_system_proxy();

        if let Some(mut proc) = self.process.take() {
            let rt = self.tokio_rt.clone();
            rt.spawn(async move {
                proc.stop().await;
            });
        }
        self.process_pending = None;
        self.status = ProcessStatus::Stopped;
        self.stats = None;
        self.tun_service_alive = false;
        self.add_log("OK", "disconnected");
    }

    /// Start a background task that polls stats every 2 seconds.
    fn start_stats_poller(&self, poll_info: PollInfo) {
        let stats_tx = self.stats_tx.clone();
        let shared_logs = self.shared_logs.clone();
        self.tokio_rt.spawn(async move {
            let mut last_log_count: usize = 0;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                match Self::poll_once(&poll_info).await {
                    Ok(stats) => {
                        if stats_tx.send(StatsUpdate::Stats(stats)).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        let _ = stats_tx.send(StatsUpdate::ProcessDied).await;
                        break;
                    }
                }

                // Poll client logs
                if let Ok(logs) = Self::poll_client_logs(&poll_info).await {
                    if logs.len() > last_log_count {
                        let new_logs = &logs[last_log_count..];
                        if let Ok(mut shared) = shared_logs.try_lock() {
                            for log in new_logs {
                                shared.push(format!("[CLIENT] {}", log));
                            }
                            if shared.len() > 2000 {
                                shared.drain(..1000);
                            }
                        }
                        last_log_count = logs.len();
                    }
                }
            }
        });
    }

    async fn poll_client_logs(info: &PollInfo) -> Result<Vec<String>, String> {
        let url = format!("{}/admin/logs", info.management_url);
        let resp = info
            .http_client
            .get(&url)
            .header("Authorization", format!("Bearer {}", info.management_key))
            .send()
            .await
            .map_err(|e| format!("poll logs: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("poll logs HTTP {}", resp.status()));
        }

        resp.json::<Vec<String>>()
            .await
            .map_err(|e| format!("parse logs: {e}"))
    }

    async fn poll_once(info: &PollInfo) -> Result<ProcessStats, String> {
        let url = format!("{}/admin/status", info.management_url);
        let resp = info
            .http_client
            .get(&url)
            .header("Authorization", format!("Bearer {}", info.management_key))
            .send()
            .await
            .map_err(|e| format!("poll stats: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("poll stats HTTP {}", resp.status()));
        }

        resp.json::<ProcessStats>()
            .await
            .map_err(|e| format!("parse stats: {e}"))
    }

    pub fn tick(&mut self, dt: f64) {
        self.animation_time += dt * 1000.0;

        // Drain agent commands.
        while let Ok(cmd) = self.agent_cmd_rx.try_recv() {
            match cmd {
                AgentCmd::SetConfig(cfg) => {
                    self.config = Some(cfg);
                    self.add_log("OK", "config updated via agent API");
                }
                AgentCmd::Connect => self.connect(),
                AgentCmd::Disconnect => self.disconnect(),
            }
        }

        // Check if process startup completed.
        if let Some(rx) = &mut self.process_pending {
            match rx.try_recv() {
                Ok(result) => {
                    self.process_pending = None;
                    match result {
                        Ok(proc) => {
                            let poll_info = proc.poll_info();
                            self.process = Some(proc);
                            self.status = ProcessStatus::Running;
                            self.add_log("OK", "connected");
                            self.add_log("INFO", &format!(
                                "management API at {} (key {}...)",
                                poll_info.management_url,
                                &poll_info.management_key[..8.min(poll_info.management_key.len())]
                            ));
                            self.start_stats_poller(poll_info);

                            // Set system proxy from parent process so it is always
                            // cleaned up on disconnect (child kill won't run its cleanup).
                            // TUN mode (sel_client_mode == 2) captures all traffic at
                            // the network layer — no system proxy needed; mirage2tun handles it.
                            match self.sel_client_mode {
                                1 => {
                                    // HTTP-CONNECT proxy on 127.0.0.1:19481
                                    crate::system_proxy::set_system_proxy("127.0.0.1:19481");
                                    self.add_log("OK", "system proxy -> http 127.0.0.1:19481");
                                }
                                2 => {
                                    // TUN mode — tell mirage2tun to activate TUN adapter
                                    self.tun_service_alive = true;
                                    let relay_addrs: Vec<String> = self
                                        .config
                                        .as_ref()
                                        .map(|c| {
                                            c.chains
                                                .iter()
                                                .filter_map(|ch| {
                                                    ch.l1.as_ref().map(|n| n.addr.ip().to_string())
                                                })
                                                .collect()
                                        })
                                        .unwrap_or_default();
                                    let rt = self.tokio_rt.clone();
                                    let shared_logs = self.shared_logs.clone();
                                    rt.spawn(async move {
                                        let client = reqwest::Client::new();
                                        let body = serde_json::json!({
                                            "socks5_addr": "127.0.0.1:19480",
                                            "relay_addrs": relay_addrs,
                                        });
                                        match client
                                            .post("http://127.0.0.1:19485/tun/start")
                                            .json(&body)
                                            .send()
                                            .await
                                        {
                                            Ok(resp) if resp.status().is_success() => {
                                                if let Ok(mut logs) = shared_logs.try_lock() {
                                                    logs.push(
                                                        "[TUN] mirage2tun /tun/start OK".into(),
                                                    );
                                                }
                                            }
                                            Ok(resp) => {
                                                let status = resp.status();
                                                let body =
                                                    resp.text().await.unwrap_or_default();
                                                if let Ok(mut logs) = shared_logs.try_lock() {
                                                    logs.push(format!(
                                                        "[TUN] /tun/start failed: {status} {body}"
                                                    ));
                                                }
                                            }
                                            Err(e) => {
                                                if let Ok(mut logs) = shared_logs.try_lock() {
                                                    logs.push(format!(
                                                        "[TUN] /tun/start error: {e}"
                                                    ));
                                                }
                                            }
                                        }
                                    });
                                    self.add_log("OK", "TUN mode -> mirage2tun /tun/start");
                                }
                                _ => {
                                    // SOCKS5 mode — set system proxy via HTTP CONNECT
                                    // (WinINET "socks=" is SOCKS4 only, not SOCKS5)
                                    // HTTP CONNECT proxy is always started alongside SOCKS5
                                    crate::system_proxy::set_system_proxy("127.0.0.1:19481");
                                    self.add_log("OK", "system proxy -> http 127.0.0.1:19481 (socks5 on 19480)");
                                }
                            }
                        }
                        Err(e) => {
                            self.status = ProcessStatus::Error(e.clone());
                            self.add_log("ERR", &e);
                        }
                    }
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    self.process_pending = None;
                    self.status = ProcessStatus::Error("spawn task dropped".into());
                    self.add_log("ERR", "spawn task dropped unexpectedly");
                }
            }
        }

        // Drain stats updates.
        while let Ok(update) = self.stats_rx.try_recv() {
            match update {
                StatsUpdate::Stats(s) => self.stats = Some(s),
                StatsUpdate::ProcessDied => {
                    self.status = ProcessStatus::Stopped;
                    self.process = None;
                    self.stats = None;
                    self.add_log("WARN", "mirage-client process exited");
                }
            }
        }

        // Check if child process is still alive.
        if let Some(proc) = &mut self.process {
            if !proc.is_alive() {
                let _ = self.stats_tx.try_send(StatsUpdate::ProcessDied);
            }
        }

        self.pending_chars.clear();
        self.pending_backspaces = 0;
    }

    /// Append a log line to the shared buffer (visible via agent API GET /logs).
    pub fn add_log(&mut self, tag: &str, msg: &str) {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mm = (secs / 60) % 60;
        let ss = secs % 60;
        let line = format!("{mm:02}:{ss:02} [{tag}] {msg}");
        if let Ok(mut logs) = self.shared_logs.try_lock() {
            logs.push(line);
            if logs.len() > 1000 {
                logs.drain(..500);
            }
        }
    }

    /// Current status as a short string for the agent API.
    pub fn status_str(&self) -> &'static str {
        match &self.status {
            ProcessStatus::Stopped => "stopped",
            ProcessStatus::Starting => "starting",
            ProcessStatus::Running => "running",
            ProcessStatus::Error(_) => "error",
        }
    }

    fn load_default_config() -> Option<ClientConfig> {
        let mut paths: Vec<std::path::PathBuf> = vec![
            "mirage-testbed-persistent-0.json".into(),
            "../mirage2monitor/mirage-testbed-persistent-0.json".into(),
        ];
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                paths.insert(0, dir.join("mirage-testbed-persistent-0.json"));
            }
        }
        for p in &paths {
            if let Ok(data) = std::fs::read_to_string(p) {
                if let Ok(mut cfg) = serde_json::from_str::<ClientConfig>(&data) {
                    Self::patch_config_for_windows(&mut cfg);
                    tracing::info!("loaded config from {}", p.display());
                    return Some(cfg);
                }
            }
        }
        tracing::error!("no config found in any path");
        None
    }

    fn patch_config_for_windows(cfg: &mut ClientConfig) {
        cfg.tun.tun_name = "mirage0".into();
        cfg.tun.tun_addr = "10.85.0.1/24".into();
        cfg.tun.socks_port = 19480;
        cfg.dns.listen_addr = "127.0.0.1:19482".into();
        cfg.dns.socks_addr = "socks5h://127.0.0.1:19480".into();
        cfg.http_proxy.bind_addr = "127.0.0.1:19481".into();
        cfg.management_addr = Some("127.0.0.1:19483".into());
        cfg.kill_switch = false;
    }
}
