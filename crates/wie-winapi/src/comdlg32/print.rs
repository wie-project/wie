//! Print / page-setup dialogs (`PrintDlgW`, `PageSetupDlgW`) with the native
//! panel bridges.

use super::state_comm_dlg_none;
use crate::gdi32::{paper_tenths_mm_to_mm, paper_tenths_mm_to_px};
use crate::guest_layout::{DevModeW, PageSetupDlgW, PrintDlgW};
use crate::guest_memory::{with_typed_read, with_typed_write};
use crate::state::{
    PageSetupDialogRequest, PendingNativePageSetup, PendingNativePrintDialog, PrintDialogPick,
    PrintDialogRequest,
};
use crate::user32::{ModalFrame, ModalResult, finish_modal};
use crate::{
    HandlerContext, PageSetupDialogPolicy, PrintDialogPolicy, WinApiControlSignal,
    WinApiHandlerResult, WinApiState,
};
use anyhow::{Context, Result};

// ── PrintDlgW (commdlg.h / wingdi.h constants) ────────────────────────────

/// `PRINTDLG.Flags`: return the print DC in `hDC` (RNotepad's flag).
const PD_RETURNDC: u32 = 0x0000_0100;
/// `PRINTDLG.Flags`: query the defaults without showing a dialog.
const PD_RETURNDEFAULT: u32 = 0x0000_0400;

/// `DEVMODE.dmFields` bits the emulated driver fills (wingdi.h).
const DM_ORIENTATION: u32 = 0x0000_0001;
const DM_PAPERSIZE: u32 = 0x0000_0002;
const DM_PAPERLENGTH: u32 = 0x0000_0004;
const DM_PAPERWIDTH: u32 = 0x0000_0008;
const DM_COPIES: u32 = 0x0000_0100;
const DM_COLOR: u32 = 0x0000_0800;

/// `DEVMODE.dmOrientation` values.
const DMORIENT_PORTRAIT: i16 = 1;
/// `DEVMODE.dmColor` values.
const DMCOLOR_COLOR: i16 = 2;
/// `DEVMODE.dmPaperSize` approximations (the length/width fields carry the
/// exact tenths-of-mm size; the code only needs to be plausible).
const DMPAPER_LETTER: i16 = 1;
const DMPAPER_A4: i16 = 9;
const DMPAPER_USER: i16 = 256;
/// `DEVMODE.dmDefaultSource` — DMBIN_AUTO.
const DMBIN_AUTO: i16 = 7;
/// `DEVMODE.dmDuplex` — DMDUP_SIMPLEX.
const DMDUP_SIMPLEX: i16 = 1;
/// `DEVMODE.dmSpecVersion` / `dmDriverVersion` (the Win32 0x0401 line).
const DM_SPEC_VERSION: u16 = 0x0401;

/// `DEVNAMES.wDefault` — the strings name the default printer.
const DN_DEFAULTPRN: u16 = 0x0001;
/// The emulated driver/device names and output port. The device name is what
/// a guest would pass to `CreateDCW`; the FILE: port matches the emulated
/// model (pages land as files under `WIE_PRINT_TO`).
const PRINT_DRIVER_NAME: &str = "winspool";
const PRINT_DEVICE_NAME: &str = "WIE Printer";
const PRINT_OUTPUT_NAME: &str = "FILE:";

/// The default paper: US Letter, whole millimetres (215.9 × 279.4 mm).
const DEFAULT_PAPER_MM: (u32, u32) = (216, 279);

/// Fill a `[u16; N]` with `text`'s UTF-16 units (zero-padded) — the
/// `DEVMODE.dmDeviceName` / `dmFormName` layout.
fn utf16_array<const N: usize>(text: &str) -> [u16; N] {
    let mut units = [0_u16; N];
    for (slot, unit) in units.iter_mut().zip(text.encode_utf16()) {
        *slot = unit;
    }
    units
}

/// The canonical emulated `DEVMODEW`: letter/A4/custom paper, portrait, one
/// copy, color, 300 DPI — overridden by the pick's values when one is given.
fn default_dev_mode(pick: Option<&PrintDialogPick>) -> DevModeW {
    let (paper_w_mm, paper_h_mm) = pick.map_or(DEFAULT_PAPER_MM, |pick| pick.paper_size_mm);
    let width_tenths = paper_w_mm.saturating_mul(10);
    let length_tenths = paper_h_mm.saturating_mul(10);
    let paper_size = match (paper_w_mm, paper_h_mm) {
        (216, 279) => DMPAPER_LETTER,
        (210, 297) => DMPAPER_A4,
        _ => DMPAPER_USER,
    };
    let copies = i16::try_from(pick.map_or(1, |pick| u32::from(pick.copies).max(1)))
        .unwrap_or(1)
        .max(1);
    let orientation = i16::try_from(
        pick.map_or(u16::try_from(DMORIENT_PORTRAIT).unwrap_or(1), |pick| {
            pick.orientation.max(1)
        }),
    )
    .unwrap_or(DMORIENT_PORTRAIT);
    let color = i16::try_from(
        pick.map_or(u16::try_from(DMCOLOR_COLOR).unwrap_or(2), |pick| {
            pick.color.max(1)
        }),
    )
    .unwrap_or(DMCOLOR_COLOR);
    let dpi = 300;
    DevModeW {
        dm_device_name: utf16_array(PRINT_DRIVER_NAME),
        dm_spec_version: DM_SPEC_VERSION,
        dm_driver_version: DM_SPEC_VERSION,
        dm_size: u16::try_from(std::mem::size_of::<DevModeW>()).unwrap_or(220),
        dm_driver_extra: 0,
        dm_fields: DM_ORIENTATION
            | DM_PAPERSIZE
            | DM_PAPERLENGTH
            | DM_PAPERWIDTH
            | DM_COPIES
            | DM_COLOR,
        dm_orientation: orientation,
        dm_paper_size: paper_size,
        dm_paper_length: i16::try_from(length_tenths).unwrap_or(2794),
        dm_paper_width: i16::try_from(width_tenths).unwrap_or(2159),
        dm_scale: 100,
        dm_copies: copies,
        dm_default_source: DMBIN_AUTO,
        dm_print_quality: i16::try_from(dpi).unwrap_or(300),
        dm_color: color,
        dm_duplex: DMDUP_SIMPLEX,
        dm_y_resolution: 0,
        dm_ttoption: 0,
        dm_collate: 0,
        dm_form_name: utf16_array(match paper_size {
            DMPAPER_LETTER => "Letter",
            DMPAPER_A4 => "A4",
            _ => "Custom",
        }),
        dm_log_pixels: u16::try_from(dpi).unwrap_or(300),
        dm_bits_per_pel: 32,
        dm_pels_width: paper_tenths_mm_to_px(width_tenths, dpi),
        dm_pels_height: paper_tenths_mm_to_px(length_tenths, dpi),
        dm_display_flags: 0,
        dm_display_frequency: 0,
        dm_icm_method: 0,
        dm_icm_intent: 0,
        dm_media_type: 0,
        dm_dither_type: 0,
        dm_reserved1: 0,
        dm_reserved2: 0,
        dm_panning_width: 0,
        dm_panning_height: 0,
    }
}

/// Read the guest's input DEVMODE (the `hDevMode` HGLOBAL — a guest VA under
/// the GMEM_FIXED semantics). `None` for a NULL handle or an unreadable
/// block (the panel then uses the defaults).
fn read_print_dev_mode_seed(
    engine: &mut dyn wie_cpu::CpuEngine,
    h_dev_mode: u64,
) -> Option<DevModeW> {
    if h_dev_mode == 0 {
        return None;
    }
    with_typed_read::<DevModeW, _, _>(engine, h_dev_mode, |dev_mode| Ok(*dev_mode)).ok()
}

/// The panel-seed values from the guest DEVMODE:
/// `(paper_size_mm, orientation, copies, color)`.
///
/// `dmPaperWidth`/`dmPaperLength` are tenths of a millimetre; invalid
/// (non-positive) dimensions fall back to Letter. The copies seed defaults to
/// 1; orientation/color default to portrait/color when the DEVMODE did not
/// set them.
fn dev_mode_seed_ui(dev_mode: Option<&DevModeW>) -> ((u32, u32), u16, u16, u16) {
    let Some(dev_mode) = dev_mode else {
        return (
            DEFAULT_PAPER_MM,
            u16::try_from(DMORIENT_PORTRAIT).unwrap_or(1),
            1,
            u16::try_from(DMCOLOR_COLOR).unwrap_or(2),
        );
    };
    let width_tenths = u32::try_from(dev_mode.dm_paper_width.max(0)).unwrap_or(0);
    let length_tenths = u32::try_from(dev_mode.dm_paper_length.max(0)).unwrap_or(0);
    let paper_mm = if width_tenths > 0 && length_tenths > 0 {
        (
            paper_tenths_mm_to_mm(width_tenths),
            paper_tenths_mm_to_mm(length_tenths),
        )
    } else {
        DEFAULT_PAPER_MM
    };
    let orientation = u16::try_from(dev_mode.dm_orientation.max(0)).unwrap_or(0);
    let copies = u16::try_from(dev_mode.dm_copies.max(1)).unwrap_or(1).max(1);
    let color = u16::try_from(dev_mode.dm_color.max(0)).unwrap_or(0);
    (paper_mm, orientation, copies, color)
}

/// Write a full canonical `DEVMODEW` (overridden by the pick's values) into
/// the guest's original `hDevMode` block when one was passed, else into a
/// freshly allocated guest-heap block (the GMEM_FIXED semantics — the handle
/// IS the address). Returns the block address; 0 on allocation failure.
fn write_print_dev_mode(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    h_dev_mode_in: u64,
    pick: Option<&PrintDialogPick>,
) -> Result<u64> {
    let dev_mode = default_dev_mode(pick);
    let block = if h_dev_mode_in != 0
        && with_typed_read::<DevModeW, _, _>(engine, h_dev_mode_in, |_| Ok(())).is_ok()
    {
        h_dev_mode_in
    } else {
        let size = u64::try_from(std::mem::size_of::<DevModeW>()).unwrap_or(0);
        state.heap_state.heap.alloc_coherent(engine, size)
    };
    if block == 0 {
        return Ok(0);
    }
    with_typed_write::<DevModeW, _, _>(engine, block, |view| {
        *view = dev_mode;
        Ok(())
    })
    .context("failed to write DEVMODE block")?;
    Ok(block)
}

/// The canonical `DEVNAMES` block: the 8-byte header (four WORD offsets) plus
/// the driver/device/output UTF-16 strings, the last double-NUL-terminated.
fn build_dev_names_bytes() -> Vec<u8> {
    let driver = PRINT_DRIVER_NAME;
    let device = PRINT_DEVICE_NAME;
    let output = PRINT_OUTPUT_NAME;
    let mut bytes = Vec::with_capacity(8 + 64);
    let device_offset = 8 + (driver.encode_utf16().count() + 1) * 2;
    let output_offset = device_offset + (device.encode_utf16().count() + 1) * 2;
    bytes.extend_from_slice(&8_u16.to_le_bytes());
    bytes.extend_from_slice(
        &u16::try_from(device_offset)
            .unwrap_or(u16::MAX)
            .to_le_bytes(),
    );
    bytes.extend_from_slice(
        &u16::try_from(output_offset)
            .unwrap_or(u16::MAX)
            .to_le_bytes(),
    );
    bytes.extend_from_slice(&DN_DEFAULTPRN.to_le_bytes());
    for text in [driver, device, output] {
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&[0, 0]);
    }
    // Final NUL so the output string is double-terminated (the multi-string
    // convention DEVNAMES uses).
    bytes.extend_from_slice(&[0, 0]);
    bytes
}

/// Write the canonical `DEVNAMES` block into the guest's original
/// `hDevNames` block when one was passed (and is writable), else into a
/// freshly allocated guest-heap block. Returns the block address; 0 on
/// allocation failure.
fn write_print_dev_names(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    h_dev_names_in: u64,
) -> Result<u64> {
    let bytes = build_dev_names_bytes();
    if h_dev_names_in != 0 && engine.mem_write(h_dev_names_in, &bytes).is_ok() {
        return Ok(h_dev_names_in);
    }
    let size = u64::try_from(bytes.len()).unwrap_or(0);
    let block = state.heap_state.heap.alloc_coherent(engine, size);
    if block == 0 {
        return Ok(0);
    }
    engine
        .mem_write(block, &bytes)
        .context("failed to write DEVNAMES block")?;
    Ok(block)
}

/// Handles `comdlg32.dll!PrintDlgW` — the interactive host print dialog.
///
/// Reads the guest `PRINTDLG` (the typed view in `guest_layout`), dispatches
/// on `Flags` and [`PrintDialogPolicy`]:
///
/// - `PD_RETURNDEFAULT` — fill the caller's `hDevMode`/`hDevNames` blocks with
///   the host defaults (allocating fresh blocks when the handles are NULL, per
///   the documented behavior) and return FALSE. No panel is shown.
/// - `PD_RETURNDC` — the guest (RNotepad) wants a print DC. Under
///   [`PrintDialogPolicy::Interactive`] with a registered print-dialog bridge
///   the handler runs the native panel (macOS NSPrintPanel) through the
///   two-entry bridge flow (see [`open_native_print_dialog`]); on accept the
///   re-entry allocates the print DC and writes `hDC` / `nCopies` /
///   `hDevMode` / `hDevNames` back into the `PRINTDLG`. Under `Cancel` (or
///   without a bridge) it returns FALSE exactly like a user canceling.
/// - Anything else — no DC requested: return FALSE (a program that needs the
///   print setup would have set `PD_RETURNDC`/`PD_RETURNDEFAULT`).
///
/// `PD_SELECTION` / `PD_PAGENUMS` and the hooks/templates are ignored (the
/// panel always offers the whole document). `hDevMode`/`hDevNames` are the
/// GMEM_FIXED kind of handle — the handle value IS the block address, so the
/// handler reads and writes them directly.
pub fn handle_print_dlg_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let pd_ptr = engine
        .read_rcx()
        .context("failed to read RCX for PrintDlgW")?;

    if pd_ptr == 0 {
        state_comm_dlg_none(state);
        return print_dialog_return(engine, 0);
    }

    // Re-entry: the native panel ran; take the pending record and write its
    // pick back into the guest PRINTDLG.
    if let Some(pending) = state.window_state().pending_native_print_dialog.take() {
        return finish_native_print_dialog(engine, state, pending);
    }

    let pd = with_typed_read::<PrintDlgW, _, _>(engine, pd_ptr, |pd| Ok(*pd))
        .context("failed to read PRINTDLG for PrintDlgW")?;
    let flags = pd.flags;

    // PD_RETURNDEFAULT is a query, not a dialog: fill the caller's blocks
    // with the default device mode/names and return FALSE (documented).
    if flags & PD_RETURNDEFAULT != 0 {
        return handle_print_dlg_return_default(engine, state, pd_ptr, &pd);
    }

    // Without PD_RETURNDC there is no DC to hand back; Windows requires it
    // (or PD_RETURNDEFAULT/PD_PRINTSETUP) for a successful return.
    if flags & PD_RETURNDC == 0 {
        state_comm_dlg_none(state);
        tracing::debug!(flags, "PrintDlgW without PD_RETURNDC; cancelling");
        return print_dialog_return(engine, 0);
    }

    // Clone the policy to avoid borrowing window_state() across the match.
    let policy = state.window_state().print_dialog_policy.clone();
    match policy {
        PrintDialogPolicy::Cancel => {
            state_comm_dlg_none(state);
            tracing::debug!("PrintDlgW cancelled by policy");
            print_dialog_return(engine, 0)
        }
        PrintDialogPolicy::Interactive => {
            let bridge_registered = state
                .try_window_state()
                .is_some_and(|window_state| window_state.print_dialog_bridge.is_some());
            if !bridge_registered {
                // Without a bridge (headless/trace sessions) a guest must
                // never hang on a panel nobody can click.
                state_comm_dlg_none(state);
                tracing::warn!("PrintDlgW interactive but no print-dialog bridge; cancelling");
                return print_dialog_return(engine, 0);
            }
            open_native_print_dialog(ctx, pd_ptr, &pd)
        }
    }
}

/// `PD_RETURNDEFAULT`: fill the guest's `hDevMode`/`hDevNames` blocks with the
/// host defaults and return FALSE (no panel — the documented query semantics).
///
/// A NULL handle means "give me a freshly allocated default block": the block
/// is allocated from the guest heap and the handle written back into the
/// `PRINTDLG`. A non-NULL handle is used as the caller-provided buffer (the
/// GMEM_FIXED handle IS the block address).
fn handle_print_dlg_return_default(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    pd_ptr: u64,
    pd: &PrintDlgW,
) -> Result<WinApiHandlerResult> {
    let mut updated = *pd;
    let default_pick = None;

    let dev_mode_va = write_print_dev_mode(engine, state, pd.h_dev_mode, default_pick)?;
    if dev_mode_va != 0 {
        updated.h_dev_mode = dev_mode_va;
    }
    let dev_names_va = write_print_dev_names(engine, state, pd.h_dev_names)?;
    if dev_names_va != 0 {
        updated.h_dev_names = dev_names_va;
    }
    if dev_mode_va == 0 || dev_names_va == 0 {
        state_comm_dlg_none(state);
        tracing::warn!("PrintDlgW PD_RETURNDEFAULT: allocation failed; cancelling");
        return print_dialog_return(engine, 0);
    }

    with_typed_write::<PrintDlgW, _, _>(engine, pd_ptr, |view| {
        *view = updated;
        Ok(())
    })
    .context("failed to write PRINTDLG handles back for PD_RETURNDEFAULT")?;

    state_comm_dlg_none(state);
    tracing::debug!("PrintDlgW PD_RETURNDEFAULT: default DEVMODE/DEVNAMES written");
    print_dialog_return(engine, 0)
}

/// `PrintDialogPolicy::Interactive` with a host bridge registered: show the
/// native print panel (macOS NSPrintPanel behind the
/// `GuestHandle::set_print_dialog_bridge` seam) and write its pick back.
///
/// The handler runs in TWO entries, split around the bridge:
///
/// - **First entry** (state lock held): read the guest's `PRINTDLG` +
///   DEVMODE, allocate a `print_info_id`, record everything the write-back
///   needs in [`PendingNativePrintDialog`], and return
///   [`WinApiControlSignal::PrintDialogBridgeRequested`]. The runtime then
///   DROPS the shared state lock and runs the bridge on the guest thread —
///   the native panel blocks the main thread for the whole session, and the
///   winit event loop needs the SAME lock to service frame/user events while
///   the panel is up, so holding it across the bridge would deadlock into the
///   macOS beachball.
/// - **Re-entry** (the engine re-executes the fake API after the bridge
///   returns): take the pending record, allocate the print DC, and write
///   `hDC` / `nCopies` / `hDevMode` / `hDevNames` back into the guest
///   `PRINTDLG`.
fn open_native_print_dialog(
    ctx: &mut HandlerContext<'_>,
    pd_ptr: u64,
    pd: &PrintDlgW,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;

    // Seed the panel from the guest DEVMODE (when one was passed): paper
    // size, orientation, copies. The color has no panel control, so the seed
    // value round-trips through the pick unchanged.
    let seed = read_print_dev_mode_seed(engine, pd.h_dev_mode);
    let (paper_size_mm, orientation, copies, color) = dev_mode_seed_ui(seed.as_ref());

    let print_info_id = {
        let id = state.window_state().next_print_info_id;
        state.window_state().next_print_info_id = id.wrapping_add(1).max(1);
        u64::from(id)
    };

    let request = PrintDialogRequest {
        paper_size_mm,
        orientation,
        copies,
        color,
        print_info_id,
    };

    state.window_state().pending_native_print_dialog = Some(PendingNativePrintDialog {
        print_dlg_ptr: pd_ptr,
        h_dev_mode_in: pd.h_dev_mode,
        h_dev_names_in: pd.h_dev_names,
        flags: pd.flags,
        print_info_id,
        pick: None,
        // The native panel is a modal session: open the frame (depth up, the
        // active window captured) so the re-entry's finish_modal restores it.
        frame: Some(open_native_bridge_frame(state, engine)?),
    });

    tracing::info!(
        target: "wiegui",
        paper_mm = ?paper_size_mm,
        orientation,
        copies,
        print_info_id,
        "PrintDlgW: native print panel requested"
    );

    Err(WinApiControlSignal::PrintDialogBridgeRequested { request }.into())
}

/// Open the modal frame for a native panel launch: the panel is a modal
/// session (the queue stays modal while the guest is parked), keyed by the
/// window that was active when it opened — restored + invalidated by the
/// re-entry's `finish_modal`.
fn open_native_bridge_frame(
    state: &mut WinApiState,
    engine: &mut dyn wie_cpu::CpuEngine,
) -> anyhow::Result<ModalFrame> {
    let owner = state.window_state().active_window_handle.as_u64();
    let (frame, _signal) = ModalFrame::activate(state, engine, owner, None, &[])?;
    Ok(frame)
}

/// Write the native panel's pick back into the guest `PRINTDLG`.
///
/// Runs on the handler's re-entry (after the runtime ran the bridge WITHOUT
/// the shared state lock). `None` pick = the user cancelled (or the bridge
/// vanished mid-call — a racing teardown must not hang the guest): return
/// FALSE with the struct untouched. On accept, allocate the print DC
/// (`PD_RETURNDC`), apply the pick to its job (paper, copies, `print_info_id`
/// — the `NSPrintInfo` table key the P3 handoff consumes), and write
/// `hDC`/`nCopies`/`hDevMode`/`hDevNames` back.
fn finish_native_print_dialog(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    pending: PendingNativePrintDialog,
) -> Result<WinApiHandlerResult> {
    let frame = pending.frame;
    let Some(pick) = pending.pick else {
        state_comm_dlg_none(state);
        tracing::info!("native print dialog cancelled");
        return finish_print_bridge(engine, state, frame, 0);
    };

    if pending.flags & PD_RETURNDC == 0 {
        state_comm_dlg_none(state);
        tracing::warn!("PrintDlgW bridge accepted but PD_RETURNDC not set; cancelling");
        return finish_print_bridge(engine, state, frame, 0);
    }

    // PD_RETURNDC: allocate the print DC and apply the pick to its job, so
    // GetDeviceCaps/StartPage/EndDoc see the chosen paper and the P3 handoff
    // finds the NSPrintInfo by id.
    let dc = state.gdi_state().alloc_print_dc();
    let hdc = dc.as_u64();
    if let Some(job) = state.gdi_state().find_print_job_mut(dc) {
        job.copies = u32::from(pick.copies).max(1);
        job.print_info_id = u32::try_from(pick.print_info_id).unwrap_or(0);
        let (width_mm, height_mm) = pick.paper_size_mm;
        let (width_tenths, height_tenths) =
            (width_mm.saturating_mul(10), height_mm.saturating_mul(10));
        let dpi = job.dpi;
        job.paper_px = (
            paper_tenths_mm_to_px(width_tenths, dpi).max(1),
            paper_tenths_mm_to_px(height_tenths, dpi).max(1),
        );
        job.paper_mm = (width_mm.max(1), height_mm.max(1));
    }

    // The DEVMODE/DEVNAMES round-trip: write a full valid DEVMODEW (and the
    // DEVNAMES string block) into the guest's original blocks, or freshly
    // allocated ones when the guest passed none.
    let dev_mode_va = write_print_dev_mode(engine, state, pending.h_dev_mode_in, Some(&pick))?;
    let dev_names_va = write_print_dev_names(engine, state, pending.h_dev_names_in)?;
    if dev_mode_va == 0 || dev_names_va == 0 {
        state_comm_dlg_none(state);
        tracing::warn!("PrintDlgW: DEVMODE/DEVNAMES allocation failed; cancelling");
        return finish_print_bridge(engine, state, frame, 0);
    }

    // Snapshot the PRINTDLG, edit the write-back fields (hDC, nCopies, the
    // hDevMode/hDevNames handles), write it back untouched otherwise — the
    // MENUITEMINFO pattern (two shared-lock borrows).
    let mut pd = with_typed_read::<PrintDlgW, _, _>(engine, pending.print_dlg_ptr, |pd| Ok(*pd))
        .context("failed to read PRINTDLG on PrintDlgW re-entry")?;
    pd.h_dc = hdc;
    pd.n_copies = pick.copies;
    pd.h_dev_mode = dev_mode_va;
    pd.h_dev_names = dev_names_va;
    with_typed_write::<PrintDlgW, _, _>(engine, pending.print_dlg_ptr, |view| {
        *view = pd;
        Ok(())
    })
    .context("failed to write PRINTDLG back for PrintDlgW")?;

    state_comm_dlg_none(state);
    tracing::info!(
        target: "wiegui",
        hdc = hdc,
        copies = pick.copies,
        paper_mm = ?pick.paper_size_mm,
        orientation = pick.orientation,
        print_info_id = pick.print_info_id,
        "PrintDlgW accepted; print DC allocated"
    );

    finish_print_bridge(engine, state, frame, 1)
}

/// Finish the native bridge's modal frame (opened at the first entry) and
/// return `value` from the print/page-setup dialog — every re-entry tail,
/// accept or cancel, closes the frame the same way.
fn finish_print_bridge(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    frame: Option<ModalFrame>,
    value: u64,
) -> Result<WinApiHandlerResult> {
    if let Some(frame) = frame {
        let result = if value == 0 {
            ModalResult::Cancel
        } else {
            ModalResult::Ok(value)
        };
        if let Some(signal) = finish_modal(state, engine, frame, result)? {
            return Err(signal.into());
        }
    }
    print_dialog_return(engine, value)
}

/// Build a `WinApiHandlerResult` that returns `value` from PrintDlgW.
fn print_dialog_return(
    engine: &mut dyn wie_cpu::CpuEngine,
    value: u64,
) -> Result<WinApiHandlerResult> {
    let return_address = engine
        .return_from_win64_api(value)
        .context("failed to return from PrintDlgW")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: value,
    })
}

/// `PAGESETUPDLG.Flags`: query the defaults without showing a dialog.
const PSD_RETURNDEFAULT: u32 = 0x0000_0400;
/// `PAGESETUPDLG.Flags`: `ptPaperSize`/`rtMargin` are in thousandths of an
/// inch; without it they are in hundredths of a millimetre.
const PSD_INTHOUSANDTHSOFINCHES: u32 = 0x0000_0004;

/// Handles `comdlg32.dll!PageSetupDlgW` — the interactive host page-setup
/// dialog.
///
/// Reads the guest `PAGESETUPDLG` (the typed view in `guest_layout`),
/// dispatches on `Flags` and [`PageSetupDialogPolicy`]:
///
/// - `PSD_RETURNDEFAULT` — fill the caller's `hDevMode`/`hDevNames` blocks
///   with the host defaults (allocating fresh blocks when the handles are
///   NULL) and return FALSE. No panel is shown.
/// - Under [`PageSetupDialogPolicy::Interactive`] with a registered
///   page-setup bridge the handler runs the native panel (macOS NSPageLayout)
///   through the two-entry bridge flow (see [`open_native_page_setup_dialog`]);
///   on accept the re-entry writes `ptPaperSize`, `hDevMode`, and `hDevNames`
///   back into the `PAGESETUPDLG`. `rtMargin` passes through unchanged — the
///   documented deviation: NSPageLayout has no margin UI, so the guest's
///   margins survive the dialog. Under `Cancel` (or without a bridge) it
///   returns FALSE exactly like a user canceling.
/// - Anything else — no panel requested by policy: return FALSE.
///
/// The custom page-setup template and hook (`lpPageSetupTemplateName` /
/// `lpfnPageSetupHook` / `lpfnPagePaintHook`) are ignored — WIE renders no
/// guest dialog templates, so the header/footer strings are not editable
/// in-session (the second documented deviation). `hDevMode`/`hDevNames` are
/// the GMEM_FIXED kind of handle — the handle value IS the block address, so
/// the handler reads and writes them directly.
pub fn handle_page_setup_dlg_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let psd_ptr = engine
        .read_rcx()
        .context("failed to read RCX for PageSetupDlgW")?;

    if psd_ptr == 0 {
        state_comm_dlg_none(state);
        return print_dialog_return(engine, 0);
    }

    // Re-entry: the native panel ran; take the pending record and write its
    // pick back into the guest PAGESETUPDLG.
    if let Some(pending) = state.window_state().pending_native_page_setup.take() {
        return finish_native_page_setup(engine, state, pending);
    }

    let psd = with_typed_read::<PageSetupDlgW, _, _>(engine, psd_ptr, |psd| Ok(*psd))
        .context("failed to read PAGESETUPDLG for PageSetupDlgW")?;
    let flags = psd.flags;

    // PSD_RETURNDEFAULT is a query, not a dialog: fill the caller's blocks
    // with the default device mode/names and return FALSE (documented).
    if flags & PSD_RETURNDEFAULT != 0 {
        return handle_page_setup_return_default(engine, state, psd_ptr, &psd);
    }

    // Clone the policy to avoid borrowing window_state() across the match.
    let policy = state.window_state().page_setup_dialog_policy.clone();
    match policy {
        PageSetupDialogPolicy::Cancel => {
            state_comm_dlg_none(state);
            tracing::debug!("PageSetupDlgW cancelled by policy");
            print_dialog_return(engine, 0)
        }
        PageSetupDialogPolicy::Interactive => {
            let bridge_registered = state
                .try_window_state()
                .is_some_and(|window_state| window_state.page_setup_dialog_bridge.is_some());
            if !bridge_registered {
                // Without a bridge (headless/trace sessions) a guest must
                // never hang on a panel nobody can click.
                state_comm_dlg_none(state);
                tracing::warn!("PageSetupDlgW interactive but no page-setup bridge; cancelling");
                return print_dialog_return(engine, 0);
            }
            open_native_page_setup_dialog(ctx, psd_ptr, &psd)
        }
    }
}

/// `PSD_RETURNDEFAULT`: fill the guest's `hDevMode`/`hDevNames` blocks with
/// the host defaults and return FALSE (no panel — the documented query
/// semantics).
///
/// A NULL handle means "give me a freshly allocated default block": the block
/// is allocated from the guest heap and the handle written back into the
/// `PAGESETUPDLG`. A non-NULL handle is used as the caller-provided buffer
/// (the GMEM_FIXED handle IS the block address).
fn handle_page_setup_return_default(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    psd_ptr: u64,
    psd: &PageSetupDlgW,
) -> Result<WinApiHandlerResult> {
    let mut updated = *psd;
    let default_pick = None;

    let dev_mode_va = write_print_dev_mode(engine, state, psd.h_dev_mode, default_pick)?;
    if dev_mode_va != 0 {
        updated.h_dev_mode = dev_mode_va;
    }
    let dev_names_va = write_print_dev_names(engine, state, psd.h_dev_names)?;
    if dev_names_va != 0 {
        updated.h_dev_names = dev_names_va;
    }
    if dev_mode_va == 0 || dev_names_va == 0 {
        state_comm_dlg_none(state);
        tracing::warn!("PageSetupDlgW PSD_RETURNDEFAULT: allocation failed; cancelling");
        return print_dialog_return(engine, 0);
    }

    with_typed_write::<PageSetupDlgW, _, _>(engine, psd_ptr, |view| {
        *view = updated;
        Ok(())
    })
    .context("failed to write PAGESETUPDLG handles back for PSD_RETURNDEFAULT")?;

    state_comm_dlg_none(state);
    tracing::debug!("PageSetupDlgW PSD_RETURNDEFAULT: default DEVMODE/DEVNAMES written");
    print_dialog_return(engine, 0)
}

/// `PageSetupDialogPolicy::Interactive` with a host bridge registered: show
/// the native page-layout panel (macOS NSPageLayout behind the
/// `GuestHandle::set_page_setup_dialog_bridge` seam) and write its pick back.
///
/// The handler runs in TWO entries, split around the bridge, exactly like the
/// print panel (see [`open_native_print_dialog`]):
///
/// - **First entry** (state lock held): read the guest's `PAGESETUPDLG` +
///   DEVMODE, record everything the write-back needs in
///   [`PendingNativePageSetup`], and return
///   [`WinApiControlSignal::PageSetupBridgeRequested`]. The runtime then
///   DROPS the shared state lock and runs the bridge on the guest thread —
///   the native panel blocks the main thread for the whole session, and the
///   winit event loop needs the SAME lock to service frame/user events while
///   the panel is up, so holding it across the bridge would deadlock into the
///   macOS beachball.
/// - **Re-entry** (the engine re-executes the fake API after the bridge
///   returns): take the pending record, write `ptPaperSize` / `hDevMode` /
///   `hDevNames` back into the guest `PAGESETUPDLG`, and return TRUE.
fn open_native_page_setup_dialog(
    ctx: &mut HandlerContext<'_>,
    psd_ptr: u64,
    psd: &PageSetupDlgW,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;

    // Seed the panel from the guest DEVMODE (when one was passed): paper size
    // and orientation. The margins have no panel control — rtMargin is only
    // read back unchanged.
    let seed = read_print_dev_mode_seed(engine, psd.h_dev_mode);
    let (paper_size_mm, orientation, _, _) = dev_mode_seed_ui(seed.as_ref());

    let request = PageSetupDialogRequest {
        paper_size_mm,
        orientation,
    };

    state.window_state().pending_native_page_setup = Some(PendingNativePageSetup {
        page_setup_dlg_ptr: psd_ptr,
        h_dev_mode_in: psd.h_dev_mode,
        h_dev_names_in: psd.h_dev_names,
        flags: psd.flags,
        pick: None,
        // The native panel is a modal session: open the frame (depth up, the
        // active window captured) so the re-entry's finish_modal restores it.
        frame: Some(open_native_bridge_frame(state, engine)?),
    });

    tracing::info!(
        target: "wiegui",
        paper_mm = ?paper_size_mm,
        orientation,
        "PageSetupDlgW: native page-layout panel requested"
    );

    Err(WinApiControlSignal::PageSetupBridgeRequested { request }.into())
}

/// Write the native panel's pick back into the guest `PAGESETUPDLG`.
///
/// Runs on the handler's re-entry (after the runtime ran the bridge WITHOUT
/// the shared state lock). `None` pick = the user cancelled (or the bridge
/// vanished mid-call — a racing teardown must not hang the guest): return
/// FALSE with the struct untouched. On accept, write the DEVMODE (paper +
/// orientation — the later `PrintDlgW` panel seeds from it) and DEVNAMES into
/// the guest's original blocks or freshly allocated ones, and write
/// `ptPaperSize` / `hDevMode` / `hDevNames` back. `rtMargin` and everything
/// else passes through unchanged (the deviations: no margin UI, no guest
/// template/hook).
fn finish_native_page_setup(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    pending: PendingNativePageSetup,
) -> Result<WinApiHandlerResult> {
    let frame = pending.frame;
    let Some(pick) = pending.pick else {
        state_comm_dlg_none(state);
        tracing::info!("native page-setup dialog cancelled");
        return finish_print_bridge(engine, state, frame, 0);
    };

    // The DEVMODE/DEVNAMES round-trip (the same helpers the print panel
    // uses): the pick's paper/orientation land in the guest DEVMODE so the
    // LATER PrintDlgW panel seeds from them (the guest stores the handle).
    let print_pick = PrintDialogPick {
        paper_size_mm: pick.paper_size_mm,
        orientation: pick.orientation,
        copies: 1,
        color: u16::try_from(DMCOLOR_COLOR).unwrap_or(2),
        print_info_id: 0,
    };
    let dev_mode_va =
        write_print_dev_mode(engine, state, pending.h_dev_mode_in, Some(&print_pick))?;
    let dev_names_va = write_print_dev_names(engine, state, pending.h_dev_names_in)?;
    if dev_mode_va == 0 || dev_names_va == 0 {
        state_comm_dlg_none(state);
        tracing::warn!("PageSetupDlgW: DEVMODE/DEVNAMES allocation failed; cancelling");
        return finish_print_bridge(engine, state, frame, 0);
    }

    // Snapshot the PAGESETUPDLG, edit the write-back fields (ptPaperSize in
    // the units the flags request, the hDevMode/hDevNames handles), write it
    // back untouched otherwise — the MENUITEMINFO pattern (two shared-lock
    // borrows).
    let mut psd =
        with_typed_read::<PageSetupDlgW, _, _>(engine, pending.page_setup_dlg_ptr, |psd| Ok(*psd))
            .context("failed to read PAGESETUPDLG on PageSetupDlgW re-entry")?;
    let (width_tenths_mm, height_tenths_mm) =
        paper_size_in_dialog_units(pending.flags, pick.paper_size_mm);
    psd.pt_paper_size_x = i32::try_from(width_tenths_mm).unwrap_or(0);
    psd.pt_paper_size_y = i32::try_from(height_tenths_mm).unwrap_or(0);
    psd.h_dev_mode = dev_mode_va;
    psd.h_dev_names = dev_names_va;
    with_typed_write::<PageSetupDlgW, _, _>(engine, pending.page_setup_dlg_ptr, |view| {
        *view = psd;
        Ok(())
    })
    .context("failed to write PAGESETUPDLG back for PageSetupDlgW")?;

    state_comm_dlg_none(state);
    tracing::info!(
        target: "wiegui",
        paper_mm = ?pick.paper_size_mm,
        orientation = pick.orientation,
        "PageSetupDlgW accepted; paper/orientation written back"
    );

    finish_print_bridge(engine, state, frame, 1)
}

/// The paper size in the units `PAGESETUPDLG.ptPaperSize` uses: hundredths of
/// a millimetre, or thousandths of an inch when `PSD_INTHOUSANDTHSOFINCHES`
/// is set (commdlg.h). Returns `(width, height)`.
fn paper_size_in_dialog_units(flags: u32, paper_mm: (u32, u32)) -> (u32, u32) {
    let (width_mm, height_mm) = paper_mm;
    if flags & PSD_INTHOUSANDTHSOFINCHES != 0 {
        // 1 inch = 25.4 mm = 1000 thousandths of an inch; pure integer math
        // (×10000/254 = ×(1000/25.4) scaled, +127 rounds half-up).
        let to_thousandths = |mm: u32| mm.saturating_mul(10_000).saturating_add(127) / 254;
        (to_thousandths(width_mm), to_thousandths(height_mm))
    } else {
        (width_mm.saturating_mul(100), height_mm.saturating_mul(100))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DMCOLOR_COLOR, DMORIENT_PORTRAIT, build_dev_names_bytes, default_dev_mode,
        dev_mode_seed_ui, handle_page_setup_dlg_w, handle_print_dlg_w, read_print_dev_mode_seed,
    };
    use crate::comdlg32::test_support::{test_engine, test_environment, test_state, write_regs};
    use crate::guest_layout::DevModeW;
    use crate::{HandlerContext, PrintDialogPick};

    // Test fixtures: the landscape/monochrome DEVMODE values the pick
    // round-trips (the implementation itself only names the portrait/color
    // defaults).
    const DMORIENT_LANDSCAPE: i16 = 2;
    const DMCOLOR_MONOCHROME: i16 = 1;

    #[test]
    fn print_dlg_and_page_setup_dlg_return_false() {
        let mut engine = test_engine();
        let mut state = test_state();
        write_regs(&mut engine, 0x5000, 0, 0, 0);

        let print_result = handle_print_dlg_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("PrintDlgW succeeds");
        assert_eq!(
            print_result.return_value, 0,
            "PrintDlgW simulates user-cancel"
        );

        let setup_result = handle_page_setup_dlg_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("PageSetupDlgW succeeds");
        assert_eq!(
            setup_result.return_value, 0,
            "PageSetupDlgW simulates user-cancel"
        );
    }

    // ── PrintDlgW helpers (pure) ───────────────────────────────────────────

    #[test]
    fn default_dev_mode_is_letter_portrait_one_copy_color() {
        let dm = default_dev_mode(None);
        assert_eq!(dm.dm_size, 220, "the full DEVMODEW size");
        assert_eq!(dm.dm_spec_version, 0x0401);
        assert_eq!(dm.dm_orientation, DMORIENT_PORTRAIT);
        assert_eq!(dm.dm_paper_size, 1, "DMPAPER_LETTER");
        assert_eq!(dm.dm_paper_width, 2160, "letter width in tenths of mm");
        assert_eq!(dm.dm_paper_length, 2790, "letter length in tenths of mm");
        assert_eq!(dm.dm_copies, 1);
        assert_eq!(dm.dm_color, DMCOLOR_COLOR);
        assert_eq!(
            dm.dm_fields & 0x0000_000F,
            0x0000_000F,
            "orientation+paper fields"
        );
        assert_eq!(dm.dm_fields & 0x0000_0100, 0x0000_0100, "copies field");
        assert_eq!(dm.dm_log_pixels, 300);
    }

    #[test]
    fn default_dev_mode_applies_the_pick() {
        let dm = default_dev_mode(Some(&PrintDialogPick {
            paper_size_mm: (210, 297),
            orientation: u16::try_from(DMORIENT_LANDSCAPE).unwrap_or(2),
            copies: 3,
            color: u16::try_from(DMCOLOR_MONOCHROME).unwrap_or(1),
            print_info_id: 7,
        }));
        assert_eq!(dm.dm_orientation, DMORIENT_LANDSCAPE);
        assert_eq!(dm.dm_paper_size, 9, "DMPAPER_A4");
        assert_eq!(dm.dm_paper_width, 2100, "A4 width in tenths of mm");
        assert_eq!(dm.dm_paper_length, 2970, "A4 length in tenths of mm");
        assert_eq!(dm.dm_copies, 3);
        assert_eq!(dm.dm_color, DMCOLOR_MONOCHROME);
    }

    #[test]
    fn dev_mode_seed_ui_parses_the_guest_devmode() {
        // No input DEVMODE → the letter/portrait/1-copy/color defaults.
        let (paper, orientation, copies, color) = dev_mode_seed_ui(None);
        assert_eq!(paper, (216, 279));
        assert_eq!(orientation, u16::try_from(DMORIENT_PORTRAIT).unwrap_or(1));
        assert_eq!(copies, 1);
        assert_eq!(color, u16::try_from(DMCOLOR_COLOR).unwrap_or(2));

        // A guest A4-landscape-3-copies-mono DEVMODE seeds the panel.
        let mut dm = default_dev_mode(None);
        dm.dm_paper_size = 9;
        dm.dm_paper_width = 2100;
        dm.dm_paper_length = 2970;
        dm.dm_orientation = DMORIENT_LANDSCAPE;
        dm.dm_copies = 3;
        dm.dm_color = DMCOLOR_MONOCHROME;
        let (paper, orientation, copies, color) = dev_mode_seed_ui(Some(&dm));
        assert_eq!(paper, (210, 297), "tenths of mm → whole mm");
        assert_eq!(orientation, u16::try_from(DMORIENT_LANDSCAPE).unwrap_or(2));
        assert_eq!(copies, 3);
        assert_eq!(color, u16::try_from(DMCOLOR_MONOCHROME).unwrap_or(1));
    }

    #[test]
    fn dev_names_block_has_valid_word_offsets_and_strings() {
        let bytes = build_dev_names_bytes();
        let header = |off: usize| -> u16 { u16::from_le_bytes([bytes[off], bytes[off + 1]]) };
        let driver_offset = usize::from(header(0));
        let device_offset = usize::from(header(2));
        let output_offset = usize::from(header(4));
        assert_eq!(driver_offset, 8, "driver string follows the 8-byte header");
        assert!(
            device_offset > driver_offset && output_offset > device_offset,
            "offsets increase: driver < device < output"
        );
        assert_eq!(header(6), 0x0001, "DN_DEFAULTPRN set");

        let read_c_string = |start: usize| -> String {
            let mut units = Vec::new();
            for pair in bytes[start..].as_chunks::<2>().0 {
                let unit = u16::from_le_bytes([pair[0], pair[1]]);
                if unit == 0 {
                    break;
                }
                units.push(unit);
            }
            String::from_utf16_lossy(&units)
        };
        assert_eq!(read_c_string(driver_offset), "winspool");
        assert_eq!(read_c_string(device_offset), "WIE Printer");
        assert_eq!(read_c_string(output_offset), "FILE:");
        // The output string is double-NUL-terminated (the DEVNAMES
        // convention): "FILE:" is 5 units (10 bytes) + the NUL + the final NUL.
        assert_eq!(bytes.get(output_offset + 10), Some(&0), "output string NUL");
        assert_eq!(bytes.get(output_offset + 12), Some(&0), "final NUL");
    }

    #[test]
    fn read_print_dev_mode_seed_returns_none_for_null_or_garbage() {
        let mut engine = test_engine();
        let mut state = test_state();
        assert!(read_print_dev_mode_seed(&mut engine, 0).is_none());
        // The engine's heap lives at [0x2000, 0x10000) — a valid block reads
        // back; an unmapped address is None (the panel then uses defaults).
        let dm = default_dev_mode(None);
        crate::guest_memory::with_typed_write::<DevModeW, _, _>(&mut engine, 0x7000, |view| {
            *view = dm;
            Ok(())
        })
        .expect("write seed DEVMODE");
        assert_eq!(
            read_print_dev_mode_seed(&mut engine, 0x7000)
                .expect("mapped block reads")
                .dm_copies,
            1
        );
        assert!(
            read_print_dev_mode_seed(&mut engine, 0xFFFF_0000).is_none(),
            "an unmapped address is treated as no DEVMODE"
        );
        let _ = &mut state;
    }
}
