use std::time::Instant;
use mirage_core::config::{ClientConfig, ProtocolId};
use uzor::types::ScrollState;
use uzor::input::InputCoordinator;

/// Connection status.
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected {
        chain_id: String,
        protocol: ProtocolId,
        duration_secs: u64,
    },
    Error(String),
}

/// Current active screen/tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Main,
    Chains,
    Settings,
    Connect,
}

/// Main mutable application state.
///
/// Uzor is immediate-mode — no Message/Elm. Screens mutate this struct directly
/// each frame based on interaction results.
pub struct AppState {
    pub connection: ConnectionState,
    pub config: Option<ClientConfig>,
    pub config_path: String,
    pub selected_chain: usize,
    pub socks5_addr: String,
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub status_message: String,
    pub screen: Screen,
    pub log_lines: Vec<String>,
    /// Scroll state for the chains screen.
    pub chains_scroll: ScrollState,
    /// Animation phase 0.0–1.0 (wraps), updated each tick for pulse effects.
    pub animation_phase: f64,
    /// Timestamp when connection attempt started; drives the Connecting→Connected delay.
    pub connect_start: Option<Instant>,
    /// Bottom sheet animation progress (0.0 = hidden, 1.0 = fully open).
    pub sheet_progress: f64,
    /// Whether the server detail bottom sheet is open.
    pub sheet_open: bool,
    /// Z-order-aware input coordinator — single winner per widget ID per frame.
    pub coordinator: InputCoordinator,
}

impl AppState {
    /// Create initial state.
    pub fn new() -> Self {
        Self {
            connection: ConnectionState::Disconnected,
            config: None,
            config_path: default_config_path(),
            selected_chain: 0,
            socks5_addr: "127.0.0.1:1080".to_string(),
            bytes_up: 0,
            bytes_down: 0,
            status_message: "Ready".to_string(),
            screen: Screen::Main,
            log_lines: vec!["Mirage started".to_string()],
            chains_scroll: ScrollState::new(),
            animation_phase: 0.0,
            connect_start: None,
            sheet_progress: 0.0,
            sheet_open: false,
            coordinator: InputCoordinator::new(),
        }
    }

    /// Advance animation state and connection timer.
    ///
    /// Call once per frame, passing `input.dt` (seconds since last frame).
    pub fn tick(&mut self, dt: f64) {
        // Advance animation phase for pulse/spin effects.
        self.animation_phase = (self.animation_phase + dt * 1.5) % 1.0;

        // Animate bottom sheet open/close.
        if self.sheet_open && self.sheet_progress < 1.0 {
            self.sheet_progress = (self.sheet_progress + dt * 4.0).min(1.0);
        } else if !self.sheet_open && self.sheet_progress > 0.0 {
            self.sheet_progress = (self.sheet_progress - dt * 4.0).max(0.0);
        }

        // Connecting → Connected after 1.5 s delay.
        if matches!(self.connection, ConnectionState::Connecting) {
            if let Some(start) = self.connect_start {
                if start.elapsed().as_secs_f64() >= 1.5 {
                    self.connect_start = None;
                    self.on_connected();
                }
            }
        }
    }

    /// Start connection.
    ///
    /// If no config is loaded, immediately transitions to Error state.
    /// Otherwise transitions to Connecting; `tick()` drives the transition after 1.5 s.
    pub fn connect(&mut self) {
        if !matches!(
            self.connection,
            ConnectionState::Disconnected | ConnectionState::Error(_)
        ) {
            return;
        }

        if self.config.is_none() {
            self.on_error("No config loaded — open Settings to load a config file".to_string());
            return;
        }

        self.connect_start = Some(Instant::now());
        self.connection = ConnectionState::Connecting;
        self.push_log("Connecting...");
    }

    /// Simulate successful connect (called from tick() after delay, or directly in demo).
    pub fn on_connected(&mut self) {
        let chain_id = self
            .config
            .as_ref()
            .and_then(|c| c.chains.get(self.selected_chain))
            .map(|ch| ch.chain_id.clone())
            .unwrap_or_else(|| "default".to_string());

        let protocol = self
            .config
            .as_ref()
            .and_then(|c| c.chains.get(self.selected_chain))
            .map(|ch| ch.protocol)
            .unwrap_or(ProtocolId::Mirage);

        self.push_log(format!("Connected via {chain_id}").as_str());
        self.connection = ConnectionState::Connected {
            chain_id,
            protocol,
            duration_secs: 0,
        };
    }

    /// Disconnect.
    pub fn disconnect(&mut self) {
        self.connect_start = None;
        self.connection = ConnectionState::Disconnected;
        self.bytes_up = 0;
        self.bytes_down = 0;
        self.push_log("Disconnected");
    }

    /// Set connection error state.
    pub fn on_error(&mut self, reason: String) {
        self.push_log(format!("Error: {reason}").as_str());
        self.connection = ConnectionState::Error(reason.clone());
        self.status_message = reason;
    }

    /// Navigate to a screen.
    pub fn go_to(&mut self, screen: Screen) {
        self.screen = screen;
    }

    /// Select a chain by index.
    pub fn select_chain(&mut self, idx: usize) {
        self.selected_chain = idx;
    }

    pub fn push_log(&mut self, line: &str) {
        self.log_lines.push(line.to_string());
        if self.log_lines.len() > 200 {
            self.log_lines.remove(0);
        }
        self.status_message = line.to_string();
    }

}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn default_config_path() -> String {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    format!("{home}/mirage2web/client.json")
}
