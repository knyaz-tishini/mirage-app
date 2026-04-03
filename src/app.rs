use std::collections::HashMap;
use std::time::Instant;
use mirage_core::config::{ClientConfig, ProtocolId};
use uzor::input::InputCoordinator;

use crate::api::{ApiClient, ApiRequest, ApiResponse};

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

/// Split tunneling mode — which sites go through VPN.
#[derive(Clone, Debug, PartialEq)]
pub enum SplitTunnelingMode {
    /// Only listed sites go through VPN.
    ForwardOnly,
    /// Listed sites bypass VPN.
    BlockListed,
}

/// A modal confirmation drawer blocking the current screen.
#[derive(Clone, Debug)]
pub struct ConfirmDrawer {
    pub title: String,
    pub description: String,
    pub confirm_label: String,
    pub cancel_label: String,
    /// Discriminant string for identifying which action to execute on confirm.
    pub action_id: String,
}

/// All possible screens — matches Amnezia's PageEnum.
#[derive(Clone, Debug, PartialEq)]
pub enum Screen {
    // Tab roots
    Home,
    Servers,
    Settings,
    Connect,

    // Server sub-screens
    ServerInfo { server_idx: usize },

    // Settings sub-screens
    SettingsServersList,
    SettingsConnection,
    SettingsApplication,
    SettingsLogs,
    SettingsAbout,

    // Connection sub-screens
    SettingsDns,
    SettingsSplitTunneling,

    // New screens (Phase 1)
    SettingsKillSwitch,
    ProtocolDetail { chain_idx: usize, protocol_idx: usize },
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

    /// Navigation stack (like Amnezia's StackView).
    pub nav_stack: Vec<Screen>,
    /// Active bottom tab (0=Home, 1=Servers, 2=Settings, 3=Connect).
    pub active_tab: usize,

    pub log_lines: Vec<String>,
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
    /// Server info active tab (0=Protocols, 1=Services, 2=Management).
    pub server_info_tab: usize,

    // ---- Toggle state -------------------------------------------------------
    /// Persistent on/off values keyed by a stable string ID.
    pub toggles: HashMap<String, bool>,

    // ---- Text input state ---------------------------------------------------
    /// Which input field currently has focus (None = none focused).
    pub focused_field: Option<String>,
    /// Text buffer per field ID.
    pub text_fields: HashMap<String, String>,
    /// Cursor byte position within the focused field's buffer.
    pub cursor_pos: usize,

    // ---- Pending keyboard input (accumulated between frames) ----------------
    /// Characters typed this frame (filled by main.rs event loop).
    pub pending_chars: Vec<char>,
    /// Backspace presses this frame.
    pub pending_backspaces: u32,

    // ---- Confirmation drawer ------------------------------------------------
    /// When Some, a modal confirmation drawer is shown above the current screen.
    pub confirm_drawer: Option<ConfirmDrawer>,

    // ---- Connection stats (displayed on Home) --------------------------------
    /// Seconds elapsed since entering Connected state, incremented in tick().
    pub connection_duration_secs: u64,
    /// Monotonic instant of last duration increment.
    pub duration_tick: Option<Instant>,

    // ---- Scroll offsets (pixel offset, clamped per-screen) ------------------
    /// Key = screen discriminant string (e.g. "split_tunneling", "logs").
    pub scroll_offsets: HashMap<String, f64>,

    // ---- Split tunneling site list ------------------------------------------
    pub split_tunneling_enabled: bool,
    pub split_tunneling_mode: SplitTunnelingMode,
    pub split_tunneling_sites: Vec<String>,

    // ---- KillSwitch ---------------------------------------------------------
    pub killswitch_enabled: bool,
    pub killswitch_strict: bool,

    // ---- DNS settings -------------------------------------------------------
    pub dns_primary: String,
    pub dns_secondary: String,

    // ---- API client (mirage2server) -----------------------------------------
    pub api: ApiClient,
    pub api_token: Option<String>,
    pub providers_l1: Vec<crate::api::Provider>,
    pub providers_l2: Vec<crate::api::Provider>,
    pub nodes: Vec<crate::api::Node>,
    pub chains: Vec<crate::api::Chain>,
    pub server_online: bool,
    pub api_error: Option<String>,
}

impl AppState {
    /// Create initial state.
    pub fn new() -> Self {
        let mut text_fields = HashMap::new();
        text_fields.insert("dns_primary".into(), "1.1.1.1".into());
        text_fields.insert("dns_secondary".into(), "1.0.0.1".into());

        let api = ApiClient::new("http://localhost:19400");
        // Fire initial health check and providers fetch immediately on startup.
        api.send(ApiRequest::Health);
        api.send(ApiRequest::FetchProviders);

        Self {
            connection: ConnectionState::Disconnected,
            config: None,
            config_path: default_config_path(),
            selected_chain: 0,
            socks5_addr: "127.0.0.1:1080".to_string(),
            bytes_up: 0,
            bytes_down: 0,
            status_message: "Ready".to_string(),
            nav_stack: vec![Screen::Home],
            active_tab: 0,
            log_lines: vec!["Mirage started".to_string()],
            animation_phase: 0.0,
            connect_start: None,
            sheet_progress: 0.0,
            sheet_open: false,
            coordinator: InputCoordinator::new(),
            server_info_tab: 0,
            toggles: HashMap::new(),
            focused_field: None,
            text_fields,
            cursor_pos: 0,
            pending_chars: Vec::new(),
            pending_backspaces: 0,
            confirm_drawer: None,
            connection_duration_secs: 0,
            duration_tick: None,
            scroll_offsets: HashMap::new(),
            split_tunneling_enabled: false,
            split_tunneling_mode: SplitTunnelingMode::ForwardOnly,
            split_tunneling_sites: Vec::new(),
            killswitch_enabled: false,
            killswitch_strict: false,
            dns_primary: "1.1.1.1".into(),
            dns_secondary: "1.0.0.1".into(),
            api,
            api_token: None,
            providers_l1: Vec::new(),
            providers_l2: Vec::new(),
            nodes: Vec::new(),
            chains: Vec::new(),
            server_online: false,
            api_error: None,
        }
    }

    /// Current screen (top of nav stack).
    pub fn current_screen(&self) -> &Screen {
        self.nav_stack.last().unwrap_or(&Screen::Home)
    }

    /// Push a screen onto the nav stack.
    pub fn push_screen(&mut self, screen: Screen) {
        self.nav_stack.push(screen);
    }

    /// Pop the current screen, staying at root if already at root.
    pub fn pop_screen(&mut self) {
        if self.nav_stack.len() > 1 {
            self.nav_stack.pop();
        }
    }

    /// Switch to a root tab — clears the nav stack and pushes the tab root.
    pub fn go_to_tab(&mut self, tab: usize) {
        self.active_tab = tab;
        let root = match tab {
            0 => Screen::Home,
            1 => Screen::Servers,
            2 => Screen::Settings,
            3 => Screen::Connect,
            _ => Screen::Home,
        };
        self.nav_stack.clear();
        self.nav_stack.push(root);
    }

    /// Drain all pending API responses and apply them to state.
    pub fn process_api_responses(&mut self) {
        while let Some(resp) = self.api.try_recv() {
            match resp {
                ApiResponse::Health(ok) => {
                    self.server_online = ok;
                }
                ApiResponse::Providers { l1, l2 } => {
                    self.providers_l1 = l1;
                    self.providers_l2 = l2;
                }
                ApiResponse::TokenSet => {
                    // Token is now stored in the worker; fetch authenticated data.
                    self.api.send(ApiRequest::FetchNodes);
                    self.api.send(ApiRequest::FetchChains);
                }
                ApiResponse::Nodes(nodes) => {
                    self.nodes = nodes;
                }
                ApiResponse::Chains(chains) => {
                    self.chains = chains;
                }
                ApiResponse::ChainActivated { chain_id } => {
                    if let Some(c) = self.chains.iter_mut().find(|c| c.id == chain_id) {
                        c.status = "active".into();
                    }
                }
                ApiResponse::ChainDeactivated { chain_id } => {
                    if let Some(c) = self.chains.iter_mut().find(|c| c.id == chain_id) {
                        c.status = "inactive".into();
                    }
                }
                ApiResponse::Error(msg) => {
                    tracing::warn!(error = %msg, "API error");
                    self.api_error = Some(msg);
                }
            }
        }
    }

    /// Advance animation state and connection timer.
    ///
    /// Call once per frame, passing `input.dt` (seconds since last frame).
    pub fn tick(&mut self, dt: f64) {
        // Drain any pending API responses first.
        self.process_api_responses();

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
                    // Prefer server chain if available (find first active or first chain).
                    let server_chain = self
                        .chains
                        .iter()
                        .find(|c| c.status == "active")
                        .or_else(|| self.chains.first())
                        .map(|c| (c.id.to_string(), "VLESS + XHTTP".to_string()));

                    if let Some((chain_id, protocol_name)) = server_chain {
                        self.push_log(
                            format!("Подключено через {chain_id}").as_str(),
                        );
                        self.connection = ConnectionState::Connected {
                            chain_id,
                            protocol: ProtocolId::Mirage,
                            duration_secs: 0,
                        };
                        // Store protocol name for display elsewhere if needed.
                        let _ = protocol_name;
                    } else {
                        self.on_connected();
                    }
                }
            }
        }

        // Increment connection duration every second while connected.
        if matches!(self.connection, ConnectionState::Connected { .. }) {
            match self.duration_tick {
                None => {
                    self.duration_tick = Some(Instant::now());
                }
                Some(t) if t.elapsed().as_secs() >= 1 => {
                    self.connection_duration_secs += 1;
                    self.duration_tick = Some(Instant::now());
                }
                _ => {}
            }
        } else {
            self.duration_tick = None;
            self.connection_duration_secs = 0;
        }

        // Apply pending keyboard input to focused field.
        let chars: Vec<char> = self.pending_chars.drain(..).collect();
        let backspaces = self.pending_backspaces;
        self.pending_backspaces = 0;

        for c in chars {
            self.field_type_char(c);
        }
        for _ in 0..backspaces {
            self.field_backspace();
        }
    }

    /// Start connection.
    ///
    /// If chains are available from the server, activates the first chain via API.
    /// Falls back to legacy config-based flow if no server chains are available.
    /// Transitions to Connecting; `tick()` drives the Connecting→Connected transition
    /// after 1.5 s.
    pub fn connect(&mut self) {
        if !matches!(
            self.connection,
            ConnectionState::Disconnected | ConnectionState::Error(_)
        ) {
            return;
        }

        if self.chains.is_empty() && self.config.is_none() {
            self.on_error("Конфигурация не загружена — откройте Настройки".to_string());
            return;
        }

        self.connect_start = Some(Instant::now());
        self.connection = ConnectionState::Connecting;
        self.push_log("Подключение...");

        // Request chain activation if we have server-managed chains.
        if let Some(chain) = self.chains.first() {
            self.api.send(ApiRequest::ActivateChain { chain_id: chain.id });
        }
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

        self.push_log(format!("Подключено через {chain_id}").as_str());
        self.connection = ConnectionState::Connected {
            chain_id,
            protocol,
            duration_secs: 0,
        };
    }

    /// Disconnect.
    pub fn disconnect(&mut self) {
        // Notify server if we have an active server-managed chain.
        if let ConnectionState::Connected { ref chain_id, .. } = self.connection {
            // chain_id may be a numeric server chain ID or a legacy string ID.
            if let Ok(id) = chain_id.parse::<i64>() {
                self.api.send(ApiRequest::DeactivateChain { chain_id: id });
            }
        }

        self.connect_start = None;
        self.connection = ConnectionState::Disconnected;
        self.bytes_up = 0;
        self.bytes_down = 0;
        self.push_log("Отключено");
    }

    /// Set connection error state.
    pub fn on_error(&mut self, reason: String) {
        self.push_log(format!("Ошибка: {reason}").as_str());
        self.connection = ConnectionState::Error(reason.clone());
        self.status_message = reason;
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

    // ---- Toggle helpers -----------------------------------------------------

    /// Get or default a toggle value.
    pub fn toggle_get(&self, id: &str) -> bool {
        self.toggles.get(id).copied().unwrap_or(false)
    }

    /// Flip a toggle and return the new value.
    pub fn toggle_flip(&mut self, id: &str) -> bool {
        let v = !self.toggles.get(id).copied().unwrap_or(false);
        self.toggles.insert(id.to_string(), v);
        v
    }

    // ---- Text field helpers -------------------------------------------------

    /// Focus a text field. Initialises buffer to `initial` if not yet present.
    pub fn focus_field(&mut self, id: &str, initial: &str) {
        if !self.text_fields.contains_key(id) {
            self.text_fields.insert(id.to_string(), initial.to_string());
        }
        self.focused_field = Some(id.to_string());
        let len = self.text_fields[id].len();
        self.cursor_pos = len;
    }

    /// Remove focus from any field.
    pub fn blur_field(&mut self) {
        self.focused_field = None;
    }

    /// Read a text field value (empty string if not found).
    pub fn field_value(&self, id: &str) -> &str {
        self.text_fields.get(id).map(|s| s.as_str()).unwrap_or("")
    }

    /// Set a text field value directly.
    pub fn field_set_text(&mut self, id: &str, text: String) {
        self.text_fields.insert(id.to_string(), text);
    }

    /// Insert a character at `cursor_pos` in the focused field.
    pub fn field_type_char(&mut self, c: char) {
        let id = match &self.focused_field {
            Some(id) => id.clone(),
            None => return,
        };
        let buf = self.text_fields.entry(id).or_default();
        // Clamp cursor_pos to valid byte boundary.
        let pos = self.cursor_pos.min(buf.len());
        buf.insert(pos, c);
        self.cursor_pos = pos + c.len_utf8();
    }

    /// Remove the character before `cursor_pos` in the focused field.
    pub fn field_backspace(&mut self) {
        let id = match &self.focused_field {
            Some(id) => id.clone(),
            None => return,
        };
        let buf = self.text_fields.entry(id).or_default();
        if self.cursor_pos == 0 || buf.is_empty() {
            return;
        }
        // Find the start of the previous character (UTF-8 safe).
        let mut start = self.cursor_pos.min(buf.len());
        while start > 0 && !buf.is_char_boundary(start - 1) {
            start -= 1;
        }
        if start == 0 {
            return;
        }
        let prev_char_start = buf[..start]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        buf.remove(prev_char_start);
        self.cursor_pos = prev_char_start;
    }

    // ---- Confirmation drawer helpers ----------------------------------------

    /// Show a modal confirmation drawer.
    pub fn show_confirm(&mut self, drawer: ConfirmDrawer) {
        self.confirm_drawer = Some(drawer);
    }

    /// Dismiss the confirmation drawer.
    pub fn dismiss_confirm(&mut self) {
        self.confirm_drawer = None;
    }

    // ---- Scroll helpers -----------------------------------------------------

    /// Apply a scroll delta for a screen key, clamped to [0, max_scroll].
    pub fn scroll_by(&mut self, key: &str, delta: f64, max_scroll: f64) {
        let current = self.scroll_offsets.get(key).copied().unwrap_or(0.0);
        let new_val = (current - delta).clamp(0.0, max_scroll.max(0.0));
        self.scroll_offsets.insert(key.to_string(), new_val);
    }

    // ---- Confirm action dispatcher ------------------------------------------

    /// Execute the action identified by `action_id` after user confirmation.
    pub fn handle_confirm_action(&mut self, action_id: &str) {
        if let Some(rest) = action_id.strip_prefix("delete_site_") {
            if let Ok(idx) = rest.parse::<usize>() {
                if idx < self.split_tunneling_sites.len() {
                    self.split_tunneling_sites.remove(idx);
                }
            }
        } else if let Some(rest) = action_id.strip_prefix("remove_chain_") {
            if let Ok(idx) = rest.parse::<usize>() {
                if let Some(cfg) = &mut self.config {
                    if idx < cfg.chains.len() {
                        cfg.chains.remove(idx);
                        if self.selected_chain >= cfg.chains.len() {
                            self.selected_chain = cfg.chains.len().saturating_sub(1);
                        }
                    }
                }
            }
        } else if let Some(rest) = action_id.strip_prefix("clear_chain_") {
            if let Ok(_idx) = rest.parse::<usize>() {
                self.push_log("Данные цепи очищены");
            }
        } else if action_id == "clear_logs" {
            self.log_lines.clear();
        } else if action_id == "reset_all_settings" {
            self.toggles.clear();
            self.text_fields.clear();
            self.text_fields.insert("dns_primary".into(), "1.1.1.1".into());
            self.text_fields.insert("dns_secondary".into(), "1.0.0.1".into());
            self.dns_primary = "1.1.1.1".into();
            self.dns_secondary = "1.0.0.1".into();
            self.split_tunneling_sites.clear();
            self.split_tunneling_enabled = false;
            self.killswitch_enabled = false;
            self.killswitch_strict = false;
            self.push_log("Настройки сброшены");
        } else if action_id == "ks_strict_enable" {
            self.killswitch_strict = true;
            self.killswitch_enabled = true;
            self.toggles.insert("killswitch_strict".into(), true);
            self.toggles.insert("killswitch_enabled".into(), true);
        }
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_flip_round_trip() {
        let mut state = AppState::new();
        assert!(!state.toggle_get("test"));
        state.toggle_flip("test");
        assert!(state.toggle_get("test"));
        state.toggle_flip("test");
        assert!(!state.toggle_get("test"));
    }

    #[test]
    fn field_type_char_inserts_at_cursor() {
        let mut state = AppState::new();
        state.focus_field("f", "ab");
        state.cursor_pos = 1; // between 'a' and 'b'
        state.field_type_char('X');
        assert_eq!(state.field_value("f"), "aXb");
        assert_eq!(state.cursor_pos, 2);
    }

    #[test]
    fn field_backspace_removes_char() {
        let mut state = AppState::new();
        state.focus_field("f", "abc");
        state.field_backspace();
        assert_eq!(state.field_value("f"), "ab");
        assert_eq!(state.cursor_pos, 2);
    }

    #[test]
    fn scroll_by_clamps_to_range() {
        let mut state = AppState::new();
        state.scroll_by("s", -100.0, 50.0); // scroll up past 0
        assert_eq!(state.scroll_offsets["s"], 0.0);
        state.scroll_by("s", 200.0, 50.0); // scroll down past max
        assert_eq!(state.scroll_offsets["s"], 50.0);
    }

    #[test]
    fn handle_confirm_action_delete_site() {
        let mut state = AppState::new();
        state.split_tunneling_sites.push("example.com".into());
        state.split_tunneling_sites.push("test.com".into());
        state.handle_confirm_action("delete_site_0");
        assert_eq!(state.split_tunneling_sites.len(), 1);
        assert_eq!(state.split_tunneling_sites[0], "test.com");
    }
}
