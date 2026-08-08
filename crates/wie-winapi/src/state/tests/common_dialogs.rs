//! Comdlg32 tests: PrintDlgW / PageSetupDlgW native-panel bridges, GetFileTitleA/W, and ChooseColorA.
use super::*;

// --- PrintDlgW (native print-panel bridge) ---

/// Write a `PRINTDLG` (Win64) into guest memory at `pd_va` (the typed view
/// zero-fills the untouched fields — the layout lives in guest_layout).
fn write_print_dlg(
    engine: &mut IcedCpu,
    pd_va: u64,
    h_dev_mode: u64,
    h_dev_names: u64,
    flags: u32,
) {
    crate::guest_memory::with_typed_write::<crate::guest_layout::PrintDlgW, _, _>(
        engine,
        pd_va,
        |pd| {
            pd.l_struct_size = 120;
            pd.hwnd_owner = 0;
            pd.h_dev_mode = h_dev_mode;
            pd.h_dev_names = h_dev_names;
            pd.flags = flags;
            pd.n_from_page = 1;
            pd.n_to_page = 0xFFFF;
            pd.n_min_page = 1;
            pd.n_max_page = 0xFFFF;
            pd.n_copies = 1;
            Ok(())
        },
    )
    .expect("write PRINTDLG");
}

/// Seed the test heap's guest bump cursor — the control block at 0x2000 was
/// attached in `default_winapi_state` (the LocalAlloc test precedent),
/// otherwise `alloc_coherent` sees bump=0 < base and refuses to allocate.
fn seed_test_heap_bump(engine: &mut IcedCpu) {
    engine
        .mem_write(0x2000, &0x2000_u64.to_le_bytes())
        .expect("write heap bump cursor");
}

/// Drive `PrintDlgW` with a scripted native print-panel bridge (Interactive
/// policy).
///
/// The real flow is two entries around the bridge: the handler's first entry
/// reads the `PRINTDLG`/DEVMODE, records [`PendingNativePrintDialog`] and
/// returns [`WinApiControlSignal::PrintDialogBridgeRequested`]; the runtime
/// runs the bridge WITHOUT the shared lock and records the pick; the engine's
/// re-execution of the fake API re-enters the handler, which allocates the
/// print DC and writes the pick back. This helper simulates exactly that (the
/// runtime is not involved in unit tests).
fn dispatch_print_dlg_with_bridge(
    engine: &mut IcedCpu,
    state: &mut WinApiState,
    bridge: crate::PrintDialogBridge,
) -> anyhow::Result<kernel32::WinApiHandlerResult> {
    seed_test_heap_bump(engine);
    state.window_state().print_dialog_policy = crate::PrintDialogPolicy::Interactive;
    state.window_state().print_dialog_bridge = Some(bridge);
    write_regs(engine, 0x5000, 0, 0, 0, 0);
    let first =
        comdlg32::handle_print_dlg_w(&mut HandlerContext::new(engine, test_environment(), state))
            .expect_err("the first entry parks the guest for the native print panel");
    let signal = first
        .downcast_ref::<WinApiControlSignal>()
        .expect("a control signal");
    let WinApiControlSignal::PrintDialogBridgeRequested { request } = signal else {
        panic!("expected a print-dialog bridge request");
    };
    // What the runtime does between the two entries: take the bridge out, run
    // it (no shared lock), restore it, record the pick.
    let bridge = state
        .window_state()
        .print_dialog_bridge
        .take()
        .expect("bridge registered");
    let picked = bridge(request);
    state.window_state().print_dialog_bridge = Some(bridge);
    state
        .window_state()
        .pending_native_print_dialog
        .as_mut()
        .expect("pending print dialog recorded")
        .pick = picked;
    // Re-entry: the handler writes the pick back.
    comdlg32::handle_print_dlg_w(&mut HandlerContext::new(engine, test_environment(), state))
}

/// The default `Cancel` policy (headless runs, `trace`): PrintDlgW returns
/// FALSE like a user canceling — no DC, no bridge, no write-back.
#[test]
fn test_print_dlg_cancel_policy_returns_false_without_machinery() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_print_dlg(&mut engine, 0x5000, 0, 0, 0x100); // PD_RETURNDC
    write_regs(&mut engine, 0x5000, 0, 0, 0, 0);

    let r = comdlg32::handle_print_dlg_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("PrintDlgW must dispatch under Cancel");

    assert_eq!(r.return_value, 0, "Cancel → FALSE");
    assert!(
        state.gdi_state().dcs.is_empty(),
        "no print DC is allocated on cancel"
    );
    assert!(
        state.window_state().pending_native_print_dialog.is_none(),
        "no pending record on cancel"
    );
}

/// `Interactive` policy but NO bridge registered (headless/trace sessions):
/// the handler cancels so a guest never hangs on a panel nobody can click.
#[test]
fn test_print_dlg_interactive_without_bridge_falls_back_to_cancel() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.window_state().print_dialog_policy = crate::PrintDialogPolicy::Interactive;
    write_print_dlg(&mut engine, 0x5000, 0, 0, 0x100); // PD_RETURNDC
    write_regs(&mut engine, 0x5000, 0, 0, 0, 0);

    let r = comdlg32::handle_print_dlg_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("PrintDlgW must dispatch without a bridge");

    assert_eq!(r.return_value, 0, "no bridge → cancel");
    assert!(state.gdi_state().dcs.is_empty());
    assert!(state.window_state().pending_native_print_dialog.is_none());
}

/// `PD_RETURNDEFAULT` with NULL handles: the handler allocates fresh
/// DEVMODE/DEVNAMES blocks, writes the handles back into the `PRINTDLG`, and
/// returns FALSE (the documented query semantics — no panel).
#[test]
fn test_print_dlg_return_default_allocates_and_writes_default_blocks() {
    use crate::guest_layout::{DevModeW, PrintDlgW};
    use crate::guest_memory::with_typed_read;

    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_print_dlg(&mut engine, 0x5000, 0, 0, 0x400); // PD_RETURNDEFAULT
    seed_test_heap_bump(&mut engine);
    write_regs(&mut engine, 0x5000, 0, 0, 0, 0);

    let r = comdlg32::handle_print_dlg_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("PD_RETURNDEFAULT must dispatch");

    assert_eq!(r.return_value, 0, "PD_RETURNDEFAULT returns FALSE");

    let pd = with_typed_read::<PrintDlgW, _, _>(&mut engine, 0x5000, |pd| Ok(*pd))
        .expect("read PRINTDLG");
    assert_ne!(pd.h_dev_mode, 0, "a fresh DEVMODE block was allocated");
    assert_ne!(pd.h_dev_names, 0, "a fresh DEVNAMES block was allocated");
    assert_eq!(pd.h_dc, 0, "PD_RETURNDEFAULT never creates a DC");

    let dm = with_typed_read::<DevModeW, _, _>(&mut engine, pd.h_dev_mode, |dm| Ok(*dm))
        .expect("read the default DEVMODE");
    assert_eq!(dm.dm_size, 220, "a full DEVMODEW");
    assert_eq!(dm.dm_copies, 1);
    assert_eq!(dm.dm_orientation, 1, "portrait");

    // The DEVNAMES header: four WORD offsets, driver string at offset 8.
    let (driver_off, device_off, output_off) = {
        let mut bytes = [0_u8; 8];
        engine
            .mem_read(pd.h_dev_names, &mut bytes)
            .expect("read DEVNAMES header");
        (
            u16::from_le_bytes([bytes[0], bytes[1]]),
            u16::from_le_bytes([bytes[2], bytes[3]]),
            u16::from_le_bytes([bytes[4], bytes[5]]),
        )
    };
    assert_eq!(driver_off, 8);
    assert!(device_off > driver_off && output_off > device_off);
}

/// `PD_RETURNDEFAULT` with caller-provided blocks: the blocks are filled IN
/// PLACE (the handles stay the same) and FALSE is returned.
#[test]
fn test_print_dlg_return_default_fills_caller_blocks_in_place() {
    use crate::guest_layout::{DevModeW, PrintDlgW};
    use crate::guest_memory::{with_typed_read, with_typed_write};

    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // The caller allocated DEVMODE + DEVNAMES blocks at 0x6000 / 0x6200.
    with_typed_write::<DevModeW, _, _>(&mut engine, 0x6000, |dm| {
        dm.dm_size = 220;
        Ok(())
    })
    .expect("write input DEVMODE block");
    engine
        .mem_write(0x6200, &[0_u8; 64])
        .expect("write input DEVNAMES block");
    write_print_dlg(&mut engine, 0x5000, 0x6000, 0x6200, 0x400); // PD_RETURNDEFAULT
    seed_test_heap_bump(&mut engine);
    write_regs(&mut engine, 0x5000, 0, 0, 0, 0);

    let r = comdlg32::handle_print_dlg_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("PD_RETURNDEFAULT must dispatch");
    assert_eq!(r.return_value, 0);

    let pd = with_typed_read::<PrintDlgW, _, _>(&mut engine, 0x5000, |pd| Ok(*pd))
        .expect("read PRINTDLG");
    assert_eq!(
        pd.h_dev_mode, 0x6000,
        "the caller's DEVMODE block is reused"
    );
    assert_eq!(
        pd.h_dev_names, 0x6200,
        "the caller's DEVNAMES block is reused"
    );
    let dm = with_typed_read::<DevModeW, _, _>(&mut engine, 0x6000, |dm| Ok(*dm))
        .expect("read the filled DEVMODE");
    assert_eq!(dm.dm_size, 220);
    assert_eq!(dm.dm_spec_version, 0x0401);
}

/// A bridge accept: PD_RETURNDC with no input DEVMODE → the pick's settings
/// are written back (hDC + nCopies + fresh DEVMODE/DEVNAMES blocks) and the
/// print job carries the paper/copies/print_info_id.
#[test]
fn test_print_dlg_bridge_accept_allocates_dc_and_writes_back() {
    use crate::guest_layout::{DevModeW, PrintDlgW};
    use crate::guest_memory::with_typed_read;
    use crate::handles::Hdc;

    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_print_dlg(&mut engine, 0x5000, 0, 0, 0x100); // PD_RETURNDC

    let bridge: crate::PrintDialogBridge = Box::new(|request| {
        // No input DEVMODE → the panel seeds from the letter defaults.
        assert_eq!(request.paper_size_mm, (216, 279));
        assert_eq!(request.orientation, 1, "portrait default");
        assert_eq!(request.copies, 1);
        assert_eq!(request.color, 2, "color default");
        assert_ne!(request.print_info_id, 0);
        Some(crate::PrintDialogPick {
            paper_size_mm: (210, 297), // A4
            orientation: 1,
            copies: 2,
            color: 2,
            print_info_id: request.print_info_id,
        })
    });

    let r = dispatch_print_dlg_with_bridge(&mut engine, &mut state, bridge)
        .expect("bridge accept must succeed");
    assert_eq!(r.return_value, 1, "an accepted pick → TRUE");

    let pd = with_typed_read::<PrintDlgW, _, _>(&mut engine, 0x5000, |pd| Ok(*pd))
        .expect("read PRINTDLG");
    let hdc = pd.h_dc;
    assert_ne!(hdc, 0, "PD_RETURNDC → a print DC is allocated");
    assert_eq!(pd.n_copies, 2, "the pick's copies reach the guest");
    assert_ne!(pd.h_dev_mode, 0, "a fresh DEVMODE block was allocated");
    assert_ne!(pd.h_dev_names, 0, "a fresh DEVNAMES block was allocated");

    let dm = with_typed_read::<DevModeW, _, _>(&mut engine, pd.h_dev_mode, |dm| Ok(*dm))
        .expect("read the DEVMODE write-back");
    assert_eq!(dm.dm_paper_width, 2100, "A4 width in tenths of mm");
    assert_eq!(dm.dm_paper_length, 2970, "A4 length in tenths of mm");
    assert_eq!(dm.dm_paper_size, 9, "DMPAPER_A4");
    assert_eq!(dm.dm_copies, 2);
    assert_eq!(dm.dm_orientation, 1);

    // The DEVNAMES header is valid (driver string at offset 8).
    let mut bytes = [0_u8; 8];
    engine
        .mem_read(pd.h_dev_names, &mut bytes)
        .expect("read DEVNAMES header");
    assert_eq!(u16::from_le_bytes([bytes[0], bytes[1]]), 8);

    // The print job carries the pick (GetDeviceCaps / StartPage / EndDoc and
    // the P3 NSPrintInfo handoff all read from it).
    let job = state
        .gdi_state()
        .find_print_job(Hdc::from(hdc))
        .expect("the print job exists");
    assert_eq!(job.copies, 2);
    assert_eq!(job.paper_mm, (210, 297));
    assert_eq!(job.print_info_id, u32::try_from(1).unwrap_or(0));
}

/// A bridge cancel (the user pressed Cancel on the panel): PrintDlgW returns
/// FALSE and the PRINTDLG stays untouched (hDC 0, no new blocks).
#[test]
fn test_print_dlg_bridge_cancel_returns_false_without_write_back() {
    use crate::guest_layout::PrintDlgW;
    use crate::guest_memory::with_typed_read;

    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_print_dlg(&mut engine, 0x5000, 0, 0, 0x100); // PD_RETURNDC

    let bridge: crate::PrintDialogBridge = Box::new(|_| None);
    let r = dispatch_print_dlg_with_bridge(&mut engine, &mut state, bridge)
        .expect("bridge cancel must dispatch");
    assert_eq!(r.return_value, 0, "a canceled pick → FALSE");

    let pd = with_typed_read::<PrintDlgW, _, _>(&mut engine, 0x5000, |pd| Ok(*pd))
        .expect("read PRINTDLG");
    assert_eq!(pd.h_dc, 0, "no DC on cancel");
    assert_eq!(pd.h_dev_mode, 0, "no DEVMODE allocation on cancel");
    assert_eq!(pd.h_dev_names, 0, "no DEVNAMES allocation on cancel");
    assert_eq!(pd.n_copies, 1, "nCopies untouched");
    assert!(state.gdi_state().dcs.is_empty());
}

/// A guest input DEVMODE seeds the panel (the bridge sees A4/landscape/3
/// copies from `dmPaperWidth`/`dmPaperLength`/`dmOrientation`/`dmCopies`) and
/// the accept reuses the SAME block for the write-back.
#[test]
fn test_print_dlg_bridge_seeds_from_guest_devmode_and_reuses_the_block() {
    use crate::guest_layout::{DevModeW, PrintDlgW};
    use crate::guest_memory::{with_typed_read, with_typed_write};
    use crate::handles::Hdc;

    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // A guest DEVMODE: A4 landscape, 3 copies, monochrome.
    with_typed_write::<DevModeW, _, _>(&mut engine, 0x6000, |dm| {
        dm.dm_size = 220;
        dm.dm_paper_width = 2100;
        dm.dm_paper_length = 2970;
        dm.dm_paper_size = 9;
        dm.dm_orientation = 2;
        dm.dm_copies = 3;
        dm.dm_color = 1;
        Ok(())
    })
    .expect("write input DEVMODE");
    write_print_dlg(&mut engine, 0x5000, 0x6000, 0, 0x100); // PD_RETURNDC

    let seed = Arc::new(Mutex::new(None));
    let seed_capture = Arc::clone(&seed);
    let bridge: crate::PrintDialogBridge = Box::new(move |request| {
        *seed_capture.lock().expect("seed capture lock") = Some((
            request.paper_size_mm,
            request.orientation,
            request.copies,
            request.color,
        ));
        Some(crate::PrintDialogPick {
            paper_size_mm: request.paper_size_mm,
            orientation: request.orientation,
            copies: request.copies,
            color: request.color,
            print_info_id: request.print_info_id,
        })
    });

    let r = dispatch_print_dlg_with_bridge(&mut engine, &mut state, bridge)
        .expect("bridge accept must succeed");
    assert_eq!(r.return_value, 1);
    let (paper, orientation, copies, color) = seed
        .lock()
        .expect("seed capture lock")
        .expect("the bridge saw a request");
    assert_eq!(paper, (210, 297), "the guest DEVMODE seeds the paper");
    assert_eq!(orientation, 2, "the guest DEVMODE seeds the orientation");
    assert_eq!(copies, 3);
    assert_eq!(color, 1);

    let pd = with_typed_read::<PrintDlgW, _, _>(&mut engine, 0x5000, |pd| Ok(*pd))
        .expect("read PRINTDLG");
    assert_eq!(pd.h_dev_mode, 0x6000, "the guest's DEVMODE block is reused");
    let dm = with_typed_read::<DevModeW, _, _>(&mut engine, 0x6000, |dm| Ok(*dm))
        .expect("read the rewritten DEVMODE");
    assert_eq!(dm.dm_copies, 3, "the round-trip keeps the copies");
    assert_eq!(dm.dm_paper_width, 2100);
    let job = state
        .gdi_state()
        .find_print_job(Hdc::from(pd.h_dc))
        .expect("the print job exists");
    assert_eq!(job.paper_mm, (210, 297));
    assert_eq!(job.copies, 3);
}

// --- PageSetupDlgW (native page-layout panel bridge) ---

/// Write a `PAGESETUPDLG` (Win64) into guest memory at `psd_va` (the typed
/// view zero-fills the untouched fields — the layout lives in guest_layout).
fn write_page_setup_dlg(
    engine: &mut IcedCpu,
    psd_va: u64,
    h_dev_mode: u64,
    h_dev_names: u64,
    flags: u32,
) {
    crate::guest_memory::with_typed_write::<crate::guest_layout::PageSetupDlgW, _, _>(
        engine,
        psd_va,
        |psd| {
            psd.l_struct_size = 128;
            psd.hwnd_owner = 0;
            psd.h_dev_mode = h_dev_mode;
            psd.h_dev_names = h_dev_names;
            psd.flags = flags;
            // Notepad's margin defaults (hundredths of mm) — the rtMargin the
            // write-back must preserve untouched (no margin UI in the panel).
            psd.rt_margin_left = 750;
            psd.rt_margin_top = 1000;
            psd.rt_margin_right = 750;
            psd.rt_margin_bottom = 1000;
            Ok(())
        },
    )
    .expect("write PAGESETUPDLG");
}

/// Drive `PageSetupDlgW` with a scripted native page-layout bridge
/// (Interactive policy). Mirrors [`dispatch_print_dlg_with_bridge`]: the
/// first entry returns [`WinApiControlSignal::PageSetupBridgeRequested`], the
/// bridge runs WITHOUT the shared lock (simulated here), the pending record's
/// pick is set, and the handler re-entry writes the pick back.
fn dispatch_page_setup_dlg_with_bridge(
    engine: &mut IcedCpu,
    state: &mut WinApiState,
    bridge: crate::PageSetupDialogBridge,
) -> anyhow::Result<kernel32::WinApiHandlerResult> {
    seed_test_heap_bump(engine);
    state.window_state().page_setup_dialog_policy = crate::PageSetupDialogPolicy::Interactive;
    state.window_state().page_setup_dialog_bridge = Some(bridge);
    write_regs(engine, 0x5000, 0, 0, 0, 0);
    let first = comdlg32::handle_page_setup_dlg_w(&mut HandlerContext::new(
        engine,
        test_environment(),
        state,
    ))
    .expect_err("the first entry parks the guest for the native page-layout panel");
    let signal = first
        .downcast_ref::<WinApiControlSignal>()
        .expect("a control signal");
    let WinApiControlSignal::PageSetupBridgeRequested { request } = signal else {
        panic!("expected a page-setup bridge request");
    };
    // What the runtime does between the two entries: take the bridge out, run
    // it (no shared lock), restore it, record the pick.
    let bridge = state
        .window_state()
        .page_setup_dialog_bridge
        .take()
        .expect("bridge registered");
    let picked = bridge(request);
    state.window_state().page_setup_dialog_bridge = Some(bridge);
    state
        .window_state()
        .pending_native_page_setup
        .as_mut()
        .expect("pending page setup recorded")
        .pick = picked;
    // Re-entry: the handler writes the pick back.
    comdlg32::handle_page_setup_dlg_w(&mut HandlerContext::new(engine, test_environment(), state))
}

/// The default `Cancel` policy (headless runs, `trace`): PageSetupDlgW
/// returns FALSE like a user canceling — no bridge, no write-back.
#[test]
fn test_page_setup_dlg_cancel_policy_returns_false_without_machinery() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_page_setup_dlg(&mut engine, 0x5000, 0, 0, 0x2); // PSD_MARGINS
    write_regs(&mut engine, 0x5000, 0, 0, 0, 0);

    let r = comdlg32::handle_page_setup_dlg_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("PageSetupDlgW must dispatch under Cancel");

    assert_eq!(r.return_value, 0, "Cancel → FALSE");
    assert!(
        state.window_state().pending_native_page_setup.is_none(),
        "no pending record on cancel"
    );
}

/// `Interactive` policy but NO bridge registered (headless/trace sessions):
/// the handler cancels so a guest never hangs on a panel nobody can click.
#[test]
fn test_page_setup_dlg_interactive_without_bridge_falls_back_to_cancel() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.window_state().page_setup_dialog_policy = crate::PageSetupDialogPolicy::Interactive;
    write_page_setup_dlg(&mut engine, 0x5000, 0, 0, 0x2); // PSD_MARGINS
    write_regs(&mut engine, 0x5000, 0, 0, 0, 0);

    let r = comdlg32::handle_page_setup_dlg_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("PageSetupDlgW must dispatch without a bridge");

    assert_eq!(r.return_value, 0, "no bridge → cancel");
    assert!(state.window_state().pending_native_page_setup.is_none());
}

/// `PSD_RETURNDEFAULT` with NULL handles: the handler allocates fresh
/// DEVMODE/DEVNAMES blocks, writes the handles back into the `PAGESETUPDLG`,
/// and returns FALSE (the documented query semantics — no panel).
#[test]
fn test_page_setup_dlg_return_default_allocates_and_writes_default_blocks() {
    use crate::guest_layout::{DevModeW, PageSetupDlgW};
    use crate::guest_memory::with_typed_read;

    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_page_setup_dlg(&mut engine, 0x5000, 0, 0, 0x400); // PSD_RETURNDEFAULT
    seed_test_heap_bump(&mut engine);
    write_regs(&mut engine, 0x5000, 0, 0, 0, 0);

    let r = comdlg32::handle_page_setup_dlg_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("PSD_RETURNDEFAULT must dispatch");

    assert_eq!(r.return_value, 0, "PSD_RETURNDEFAULT returns FALSE");

    let psd = with_typed_read::<PageSetupDlgW, _, _>(&mut engine, 0x5000, |psd| Ok(*psd))
        .expect("read PAGESETUPDLG");
    assert_ne!(psd.h_dev_mode, 0, "a fresh DEVMODE block was allocated");
    assert_ne!(psd.h_dev_names, 0, "a fresh DEVNAMES block was allocated");

    let dm = with_typed_read::<DevModeW, _, _>(&mut engine, psd.h_dev_mode, |dm| Ok(*dm))
        .expect("read the default DEVMODE");
    assert_eq!(dm.dm_size, 220, "a full DEVMODEW");
    assert_eq!(dm.dm_copies, 1);
    assert_eq!(dm.dm_orientation, 1, "portrait");
}

/// A bridge accept (hundredths-of-mm units): the pick's paper/orientation are
/// written back — `ptPaperSize` in 100ths of mm, the DEVMODE in tenths of mm
/// (the later `PrintDlgW` panel seeds from it) — while `rtMargin` passes
/// through unchanged and fresh DEVMODE/DEVNAMES blocks are allocated.
#[test]
fn test_page_setup_dlg_bridge_accept_writes_back_paper_and_devmode() {
    use crate::guest_layout::{DevModeW, PageSetupDlgW};
    use crate::guest_memory::with_typed_read;

    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_page_setup_dlg(&mut engine, 0x5000, 0, 0, 0x2); // PSD_MARGINS

    let bridge: crate::PageSetupDialogBridge = Box::new(|request| {
        // No input DEVMODE → the panel seeds from the letter defaults.
        assert_eq!(request.paper_size_mm, (216, 279));
        assert_eq!(request.orientation, 1, "portrait default");
        Some(crate::PageSetupDialogPick {
            paper_size_mm: (210, 297), // A4
            orientation: 2,            // landscape
        })
    });

    let r = dispatch_page_setup_dlg_with_bridge(&mut engine, &mut state, bridge)
        .expect("bridge accept must succeed");
    assert_eq!(r.return_value, 1, "an accepted pick → TRUE");

    let psd = with_typed_read::<PageSetupDlgW, _, _>(&mut engine, 0x5000, |psd| Ok(*psd))
        .expect("read PAGESETUPDLG");
    assert_eq!(psd.pt_paper_size_x, 21000, "A4 width in 100ths of mm");
    assert_eq!(psd.pt_paper_size_y, 29700, "A4 height in 100ths of mm");
    assert_eq!(psd.rt_margin_left, 750, "rtMargin passes through unchanged");
    assert_eq!(psd.rt_margin_top, 1000);
    assert_eq!(psd.rt_margin_right, 750);
    assert_eq!(psd.rt_margin_bottom, 1000);
    assert_ne!(psd.h_dev_mode, 0, "a fresh DEVMODE block was allocated");
    assert_ne!(psd.h_dev_names, 0, "a fresh DEVNAMES block was allocated");

    let dm = with_typed_read::<DevModeW, _, _>(&mut engine, psd.h_dev_mode, |dm| Ok(*dm))
        .expect("read the DEVMODE write-back");
    assert_eq!(dm.dm_paper_width, 2100, "A4 width in tenths of mm");
    assert_eq!(dm.dm_paper_length, 2970, "A4 length in tenths of mm");
    assert_eq!(dm.dm_paper_size, 9, "DMPAPER_A4");
    assert_eq!(dm.dm_orientation, 2, "the pick's landscape orientation");
}

/// `PSD_INTHOUSANDTHSOFINCHES` switches `ptPaperSize` to thousandths of an
/// inch (the DEVMODE stays tenths of mm — wingdi.h always uses those).
#[test]
fn test_page_setup_dlg_bridge_accept_writes_thousandths_of_inches_when_flag_set() {
    use crate::guest_layout::PageSetupDlgW;
    use crate::guest_memory::with_typed_read;

    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // PSD_MARGINS | PSD_INTHOUSANDTHSOFINCHES.
    write_page_setup_dlg(&mut engine, 0x5000, 0, 0, 0x2 | 0x4);

    let bridge: crate::PageSetupDialogBridge = Box::new(|_| {
        Some(crate::PageSetupDialogPick {
            paper_size_mm: (210, 297), // A4
            orientation: 1,
        })
    });

    let r = dispatch_page_setup_dlg_with_bridge(&mut engine, &mut state, bridge)
        .expect("bridge accept must succeed");
    assert_eq!(r.return_value, 1);

    let psd = with_typed_read::<PageSetupDlgW, _, _>(&mut engine, 0x5000, |psd| Ok(*psd))
        .expect("read PAGESETUPDLG");
    // 210 mm = 8268 thousandths of an inch; 297 mm = 11693.
    assert_eq!(psd.pt_paper_size_x, 8268);
    assert_eq!(psd.pt_paper_size_y, 11693);
}

/// A bridge cancel (the user pressed Cancel on the panel): PageSetupDlgW
/// returns FALSE and the PAGESETUPDLG stays untouched (ptPaperSize 0, the
/// caller's handles unchanged, rtMargin as written).
#[test]
fn test_page_setup_dlg_bridge_cancel_returns_false_without_write_back() {
    use crate::guest_layout::PageSetupDlgW;
    use crate::guest_memory::with_typed_read;

    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_page_setup_dlg(&mut engine, 0x5000, 0x6000, 0x6200, 0x2); // PSD_MARGINS

    let bridge: crate::PageSetupDialogBridge = Box::new(|_| None);
    let r = dispatch_page_setup_dlg_with_bridge(&mut engine, &mut state, bridge)
        .expect("bridge cancel must dispatch");
    assert_eq!(r.return_value, 0, "a canceled pick → FALSE");

    let psd = with_typed_read::<PageSetupDlgW, _, _>(&mut engine, 0x5000, |psd| Ok(*psd))
        .expect("read PAGESETUPDLG");
    assert_eq!(psd.h_dev_mode, 0x6000, "the caller's DEVMODE handle stays");
    assert_eq!(
        psd.h_dev_names, 0x6200,
        "the caller's DEVNAMES handle stays"
    );
    assert_eq!(psd.pt_paper_size_x, 0, "ptPaperSize untouched");
    assert_eq!(psd.pt_paper_size_y, 0);
    assert_eq!(psd.rt_margin_left, 750, "rtMargin untouched");
}

/// A guest input DEVMODE seeds the page-layout panel (the bridge sees
/// A4/landscape from `dmPaperWidth`/`dmPaperLength`/`dmOrientation`) and the
/// accept reuses the SAME block for the write-back.
#[test]
fn test_page_setup_dlg_bridge_seeds_from_guest_devmode_and_reuses_the_block() {
    use crate::guest_layout::{DevModeW, PageSetupDlgW};
    use crate::guest_memory::{with_typed_read, with_typed_write};

    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // A guest DEVMODE: A4 landscape.
    with_typed_write::<DevModeW, _, _>(&mut engine, 0x6000, |dm| {
        dm.dm_size = 220;
        dm.dm_paper_width = 2100;
        dm.dm_paper_length = 2970;
        dm.dm_paper_size = 9;
        dm.dm_orientation = 2;
        Ok(())
    })
    .expect("write input DEVMODE");
    write_page_setup_dlg(&mut engine, 0x5000, 0x6000, 0, 0x2); // PSD_MARGINS

    let seed = Arc::new(Mutex::new(None));
    let seed_capture = Arc::clone(&seed);
    let bridge: crate::PageSetupDialogBridge = Box::new(move |request| {
        *seed_capture.lock().expect("seed capture lock") =
            Some((request.paper_size_mm, request.orientation));
        Some(crate::PageSetupDialogPick {
            paper_size_mm: request.paper_size_mm,
            orientation: request.orientation,
        })
    });

    let r = dispatch_page_setup_dlg_with_bridge(&mut engine, &mut state, bridge)
        .expect("bridge accept must succeed");
    assert_eq!(r.return_value, 1);
    let (paper, orientation) = seed
        .lock()
        .expect("seed capture lock")
        .expect("the bridge saw a request");
    assert_eq!(paper, (210, 297), "the guest DEVMODE seeds the paper");
    assert_eq!(orientation, 2, "the guest DEVMODE seeds the orientation");

    let psd = with_typed_read::<PageSetupDlgW, _, _>(&mut engine, 0x5000, |psd| Ok(*psd))
        .expect("read PAGESETUPDLG");
    assert_eq!(
        psd.h_dev_mode, 0x6000,
        "the guest's DEVMODE block is reused"
    );
    let dm = with_typed_read::<DevModeW, _, _>(&mut engine, 0x6000, |dm| Ok(*dm))
        .expect("read the rewritten DEVMODE");
    assert_eq!(dm.dm_orientation, 2, "the round-trip keeps the orientation");
    assert_eq!(dm.dm_paper_width, 2100);
}

// --- Comdlg32 ---

#[test]
fn test_get_file_title_w_basename() {
    // Full dispatch path: name resolution (names.rs) → dense id → handler arm.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let path_addr = 0x5000;
    let title_addr = 0x6000;
    write_guest_utf16(&mut engine, path_addr, r"C:\foo\bar.txt");
    // Pre-fill so the handler's write is observable.
    engine
        .mem_write(title_addr, &[0xAA_u8; 128])
        .expect("prefill title buffer");
    write_regs(&mut engine, path_addr, title_addr, 64, 0, 0);
    engine
        .mem_write(STACK_TOP, &0x1234_5678_u64.to_le_bytes())
        .expect("write sentinel return address");
    let id = crate::resolve_winapi_id("comdlg32.dll", "GetFileTitleW")
        .expect("GetFileTitleW must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetFileTitleW must dispatch");
    assert_eq!(
        r.return_address, 0x1234_5678,
        "handler must return past the call"
    );
    assert_eq!(r.return_value, 0, "GetFileTitleW must succeed");
    assert_eq!(
        read_guest_utf16_raw(&mut engine, title_addr, 64),
        "bar.txt",
        "basename after the last separator must be copied"
    );
}

#[test]
fn test_get_file_title_w_buffer_too_small() {
    // Truncated copy plus the MSDN negative return: abs = required size
    // including the terminating NUL ("bar.txt" is 7 chars → 8, returned -8).
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let path_addr = 0x5000;
    let title_addr = 0x6000;
    write_guest_utf16(&mut engine, path_addr, r"C:\foo\bar.txt");
    engine
        .mem_write(title_addr, &[0xAA_u8; 32])
        .expect("prefill title buffer");
    write_regs(&mut engine, path_addr, title_addr, 4, 0, 0);
    engine
        .mem_write(STACK_TOP, &0x1234_5678_u64.to_le_bytes())
        .expect("write sentinel return address");
    let id = crate::resolve_winapi_id("comdlg32.dll", "GetFileTitleW")
        .expect("GetFileTitleW must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetFileTitleW must dispatch");
    assert_eq!(
        r.return_value, 0xFFFF_FFF8,
        "too-small buffer must return -(required size incl. NUL)"
    );
    assert_eq!(
        read_guest_utf16_raw(&mut engine, title_addr, 4),
        "bar",
        "buffer must hold a truncated NUL-terminated copy"
    );
}

#[test]
fn test_get_file_title_w_no_separators() {
    // No `\` or `/` in the path → the whole string is the title.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let path_addr = 0x5000;
    let title_addr = 0x6000;
    write_guest_utf16(&mut engine, path_addr, "report.md");
    write_regs(&mut engine, path_addr, title_addr, 64, 0, 0);
    let id = crate::resolve_winapi_id("comdlg32.dll", "GetFileTitleW")
        .expect("GetFileTitleW must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetFileTitleW must dispatch");
    assert_eq!(r.return_value, 0, "GetFileTitleW must succeed");
    assert_eq!(
        read_guest_utf16_raw(&mut engine, title_addr, 64),
        "report.md",
        "a separator-free path must be copied whole"
    );
}

#[test]
fn test_get_file_title_w_trailing_separator_is_invalid() {
    // "C:\foo\" has no basename; GetFileTitle reports an invalid file name
    // (1) and the buffer still comes back NUL-terminated.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let path_addr = 0x5000;
    let title_addr = 0x6000;
    write_guest_utf16(&mut engine, path_addr, r"C:\foo\");
    write_regs(&mut engine, path_addr, title_addr, 64, 0, 0);
    let id = crate::resolve_winapi_id("comdlg32.dll", "GetFileTitleW")
        .expect("GetFileTitleW must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetFileTitleW must dispatch");
    assert_eq!(r.return_value, 1, "trailing separator must be invalid");
    assert_eq!(
        read_guest_utf16_raw(&mut engine, title_addr, 64),
        "",
        "buffer must be NUL-terminated"
    );
}

#[test]
fn test_get_file_title_w_empty_path_is_success() {
    // A genuinely empty path has no basename, but real GetFileTitle treats
    // it as success: 0 return with an empty NUL-terminated title (only a
    // trailing-separator path is an invalid file name).
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let path_addr = 0x5000;
    let title_addr = 0x6000;
    write_guest_utf16(&mut engine, path_addr, "");
    // Pre-fill so the handler's NUL write is observable.
    engine
        .mem_write(title_addr, &[0xAA_u8; 64])
        .expect("prefill title buffer");
    write_regs(&mut engine, path_addr, title_addr, 64, 0, 0);
    let id = crate::resolve_winapi_id("comdlg32.dll", "GetFileTitleW")
        .expect("GetFileTitleW must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetFileTitleW must dispatch");
    assert_eq!(r.return_value, 0, "empty path must succeed with 0");
    assert_eq!(
        read_guest_utf16_raw(&mut engine, title_addr, 64),
        "",
        "title buffer must be NUL-terminated"
    );
}

#[test]
fn test_get_file_title_a_basename_ansi() {
    // ANSI mirror: reads an A-string path and writes an A-string title.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let path_addr = 0x5000;
    let title_addr = 0x6000;
    write_guest_ansi(&mut engine, path_addr, r"C:\foo\bar.txt");
    write_regs(&mut engine, path_addr, title_addr, 64, 0, 0);
    let id = crate::resolve_winapi_id("comdlg32.dll", "GetFileTitleA")
        .expect("GetFileTitleA must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetFileTitleA must dispatch");
    assert_eq!(r.return_value, 0, "GetFileTitleA must succeed");
    assert_eq!(
        read_guest_ansi_raw(&mut engine, title_addr, 64),
        "bar.txt",
        "ANSI basename must be copied"
    );
}

#[test]
fn test_choose_color_a_writes_color() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let cc_va = 0x5000;
    engine
        .mem_map(cc_va, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map CHOOSECOLOR");
    write_regs(&mut engine, cc_va, 0, 0, 0, 0);
    assert_return_value!(
        comdlg32::handle_choose_color_a(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        1
    );
    // rgbResult is at offset 0x10 in CHOOSECOLOR — should be RGB black (0).
    let mut rgb = [0_u8; 4];
    engine.mem_read(cc_va + 0x10, &mut rgb).ok();
    assert_eq!(u32::from_le_bytes(rgb), 0x00_00_00);
}
