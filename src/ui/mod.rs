pub mod components;
pub mod connect_screen;
pub mod main_screen;
pub mod protocol_detail_screen;
pub mod server_info_screen;
pub mod servers_screen;
pub mod settings_about;
pub mod settings_application;
pub mod settings_connection;
pub mod settings_dns;
pub mod settings_killswitch;
pub mod settings_logs;
pub mod settings_screen;
pub mod settings_servers_list;
pub mod settings_split_tunneling;

use uzor::render::RenderContext;
use uzor::input::InputState;
use uzor::types::Rect;

use crate::app::{AppState, Screen};

/// Top-level frame dispatch — draws background + current screen + nav bar.
pub fn draw_frame(
    ctx: &mut dyn RenderContext,
    input: InputState,
    viewport: Rect,
    state: &mut AppState,
) {
    // Advance animation, timers, and flush pending keyboard input every frame
    // regardless of which screen is active.
    state.tick(input.dt);

    let screen = state.current_screen().clone();

    match screen {
        Screen::Home => main_screen::draw_main_screen(ctx, &input, viewport, state),
        Screen::Servers => servers_screen::draw_servers_screen(ctx, &input, viewport, state),
        Screen::Settings => settings_screen::draw_settings_screen(ctx, &input, viewport, state),
        Screen::Connect => connect_screen::draw_connect_screen(ctx, &input, viewport, state),

        Screen::ServerInfo { server_idx } => {
            server_info_screen::draw_server_info_screen(ctx, &input, viewport, state, server_idx)
        }

        Screen::SettingsServersList => {
            settings_servers_list::draw(ctx, &input, viewport, state)
        }
        Screen::SettingsConnection => {
            settings_connection::draw(ctx, &input, viewport, state)
        }
        Screen::SettingsApplication => {
            settings_application::draw(ctx, &input, viewport, state)
        }
        Screen::SettingsLogs => {
            settings_logs::draw(ctx, &input, viewport, state)
        }
        Screen::SettingsAbout => {
            settings_about::draw(ctx, &input, viewport, state)
        }
        Screen::SettingsDns => {
            settings_dns::draw(ctx, &input, viewport, state)
        }
        Screen::SettingsSplitTunneling => {
            settings_split_tunneling::draw(ctx, &input, viewport, state)
        }
        Screen::SettingsKillSwitch => {
            settings_killswitch::draw(ctx, &input, viewport, state)
        }
        Screen::ProtocolDetail { chain_idx, protocol_idx } => {
            protocol_detail_screen::draw(ctx, &input, viewport, state, chain_idx, protocol_idx)
        }
    }

    // Confirmation drawer rendered above everything else (topmost layer).
    if state.confirm_drawer.is_some() {
        let w = viewport.width;
        let h = viewport.height;
        if let Some(action_id) =
            components::draw_confirm_drawer(ctx, &input, state, w, h)
        {
            state.handle_confirm_action(&action_id);
            state.dismiss_confirm();
        }
    }
}
