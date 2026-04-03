pub mod chains_screen;
pub mod components;
pub mod connect_screen;
pub mod main_screen;
pub mod settings_screen;

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
    match state.screen {
        Screen::Main => main_screen::draw_main_screen(ctx, &input, viewport, state),
        Screen::Chains => chains_screen::draw_chains_screen(ctx, &input, viewport, state),
        Screen::Settings => settings_screen::draw_settings_screen(ctx, &input, viewport, state),
        Screen::Connect => connect_screen::draw_connect_screen(ctx, &input, viewport, state),
    }
}
