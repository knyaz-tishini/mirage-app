pub mod admin_screen;
pub mod components;
pub mod connect_screen;
pub mod main_screen;
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

/// Returns the nav bar tab index for the given screen.
/// Sub-screens inherit the tab index of their parent.
fn nav_tab_for_screen(screen: &Screen) -> usize {
    match screen {
        Screen::Home => 0,
        Screen::Servers => 1,
        Screen::ServerInfo { .. } => 1,
        Screen::SettingsServersList => 1,
        Screen::Settings => 2,
        Screen::SettingsConnection => 2,
        Screen::SettingsApplication => 2,
        Screen::SettingsLogs => 2,
        Screen::SettingsAbout => 2,
        Screen::SettingsDns => 2,
        Screen::SettingsSplitTunneling => 2,
        Screen::SettingsKillSwitch => 2,
        Screen::Connect => 3,
        Screen::Admin => 4,
    }
}

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
    let active_tab = nav_tab_for_screen(&screen);

    match screen.clone() {
        Screen::Home => main_screen::draw_main_screen(ctx, &input, viewport, state),
        Screen::Servers => servers_screen::draw_servers_screen(ctx, &input, viewport, state),
        Screen::Settings => settings_screen::draw_settings_screen(ctx, &input, viewport, state),
        Screen::Connect => connect_screen::draw_connect_screen(ctx, &input, viewport, state),
        Screen::Admin => admin_screen::draw_admin_screen(ctx, &input, viewport, state),

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
    }

    // Nav bar is drawn AFTER screen content so it always appears on top.
    // Shown on ALL screens — sub-screens inherit parent tab highlight.
    {
        let w = viewport.width;
        let h = viewport.height;
        if let Some(new_tab) = components::draw_nav_bar(ctx, &input, state, w, h, active_tab) {
            state.go_to_tab(new_tab);
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
