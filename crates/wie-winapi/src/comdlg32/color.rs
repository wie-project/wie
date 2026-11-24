//! `ChooseColorA` handler (simulated accept with black).

use super::CDERR_NONE;
use crate::gdi32::{ArgReg, read_arg};
use crate::{HandlerContext, WinApiHandlerResult};
use anyhow::Result;

/// Handles `comdlg32.dll!ChooseColorA` (simulates accept with black color).
///
/// Reaches exactly one capability — the dialog's extended error — so it takes
/// it from the context per call and never binds the whole state (ADR-0005
/// stage 2). Each `&mut *ctx.engine` is its own short reborrow, which is what
/// keeps the original read-then-set-then-write order without a wide `state`.
pub fn handle_choose_color_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let choose_color_va = read_arg(&mut *ctx.engine, ArgReg::Rcx, "ChooseColorA")?;

    ctx.dialogs().set_comm_dlg_extended_error(CDERR_NONE);

    // CHOOSECOLORA has rgbResult at offset 0x10 (after lStructSize + hwndOwner + hInstance).
    if choose_color_va != 0 {
        // Write default RGB color (black) into rgbResult field.
        let rgb_field = choose_color_va.wrapping_add(0x10);
        drop(crate::guest_memory::write_u32(
            &mut *ctx.engine,
            rgb_field,
            0x00_00_00,
        ));
    }

    ctx.finish(1)
}
