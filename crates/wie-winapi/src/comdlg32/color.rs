//! `ChooseColorA` handler (simulated accept with black).

use super::CDERR_NONE;
use crate::{HandlerContext, WinApiHandlerResult};
use anyhow::{Context, Result};

/// Handles `comdlg32.dll!ChooseColorA` (simulates accept with black color).
pub fn handle_choose_color_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let choose_color_va = engine
        .read_rcx()
        .context("failed to read RCX for ChooseColorA")?;

    state.window_state().comm_dlg_extended_error = CDERR_NONE;

    // CHOOSECOLORA has rgbResult at offset 0x10 (after lStructSize + hwndOwner + hInstance).
    if choose_color_va != 0 {
        // Write default RGB color (black) into rgbResult field.
        let rgb_field = choose_color_va.wrapping_add(0x10);
        drop(crate::guest_memory::write_u32(
            engine, rgb_field, 0x00_00_00,
        ));
    }

    ctx.finish(1)
}
