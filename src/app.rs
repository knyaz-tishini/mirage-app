use mirage_core::config::{ClientConfig, ProtocolId};

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
    Logs,
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
    /// Scroll offset for the logs screen (pixels from top).
    pub logs_scroll: f64,
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
            logs_scroll: 0.0,
        }
    }

    /// Start connection.
    ///
    /// If no config is loaded, immediately transitions to Error state.
    /// Otherwise transitions to Connecting then calls `on_connected()`.
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

        self.connection = ConnectionState::Connecting;
        self.push_log("Connecting...");
    }

    /// Simulate successful connect (called from async task result, or directly in demo).
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

    /// Returns true if currently connected or connecting.
    pub fn is_connected(&self) -> bool {
        matches!(
            self.connection,
            ConnectionState::Connected { .. } | ConnectionState::Connecting
        )
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
