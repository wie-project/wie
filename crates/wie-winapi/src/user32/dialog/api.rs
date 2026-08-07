//! The dialog item accessors: `GetDlgItemA/W`, `GetDlgItemTextA/W`,
//! `SetDlgItemTextA/W`, `GetDlgItemInt`, `SetDlgItemInt` and
//! `SendDlgItemMessageW`.

use anyhow::{Context, Result};

use crate::user32::{
    HandlerContext, WinApiHandlerResult, WinApiState, WindowRecord, find_window_mut,
    message::handle_send_message, read_arg_string, read_u64, write_guest_u32, write_out_string,
};

/// Handles `USER32.dll!GetDlgItemA`.
pub fn handle_get_dlg_item_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_dlg_item_impl(ctx, "GetDlgItemA")
}
/// Handles `USER32.dll!GetDlgItemW`.
pub fn handle_get_dlg_item_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_dlg_item_impl(ctx, "GetDlgItemW")
}

fn handle_get_dlg_item_impl(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let dialog_hwnd = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;
    let id_raw = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;
    let id = u16::try_from(id_raw & u64::from(u32::MAX)).unwrap_or(0);

    let child = get_dlg_item(state, dialog_hwnd, id);

    ctx.finish(child)
}

/// Find a dialog's child control by id (control ids live in `menu_handle`).
fn get_dlg_item(state: &WinApiState, dialog_hwnd: u64, id: u16) -> u64 {
    state
        .try_window_state()
        .and_then(|ws| {
            ws.windows.iter().find(|w| {
                w.parent_handle == crate::handles::Hwnd::from(dialog_hwnd)
                    && w.menu_handle == u64::from(id)
            })
        })
        .map_or(0, |w| w.handle.as_u64())
}

/// Handles `USER32.dll!GetDlgItemTextA`.
pub fn handle_get_dlg_item_text_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_dlg_item_text_impl(ctx, false, "GetDlgItemTextA")
}
/// Handles `USER32.dll!GetDlgItemTextW`.
pub fn handle_get_dlg_item_text_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_dlg_item_text_impl(ctx, true, "GetDlgItemTextW")
}

fn handle_get_dlg_item_text_impl(
    ctx: &mut HandlerContext<'_>,
    unicode: bool,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let dialog_hwnd = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;
    let id_raw = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;
    let buffer_va = engine
        .read_r8()
        .with_context(|| format!("failed to read R8 for {api_name}"))?;
    let max_characters = engine
        .read_r9()
        .with_context(|| format!("failed to read R9 for {api_name}"))?;
    let id = u16::try_from(id_raw & u64::from(u32::MAX)).unwrap_or(0);

    let child = get_dlg_item(state, dialog_hwnd, id);
    let text = if child == 0 {
        String::new()
    } else {
        find_window_ref(state, child).map_or_else(String::new, |w| {
            if w.control_kind.is_some() {
                w.control_text.clone()
            } else {
                w.title.clone()
            }
        })
    };

    let return_value = if text.is_empty() || buffer_va == 0 {
        0
    } else {
        write_out_string(engine, buffer_va, max_characters, &text, unicode)?
    };

    ctx.finish(return_value)
}

/// Handles `USER32.dll!SetDlgItemTextA`.
pub fn handle_set_dlg_item_text_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_set_dlg_item_text_impl(ctx, false, "SetDlgItemTextA")
}
/// Handles `USER32.dll!SetDlgItemTextW`.
pub fn handle_set_dlg_item_text_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_set_dlg_item_text_impl(ctx, true, "SetDlgItemTextW")
}

fn handle_set_dlg_item_text_impl(
    ctx: &mut HandlerContext<'_>,
    unicode: bool,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let dialog_hwnd = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;
    let id_raw = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;
    let text_va = engine
        .read_r8()
        .with_context(|| format!("failed to read R8 for {api_name}"))?;
    let id = u16::try_from(id_raw & u64::from(u32::MAX)).unwrap_or(0);

    let child = get_dlg_item(state, dialog_hwnd, id);
    let success = child != 0 && text_va != 0;
    if success {
        let text = read_arg_string(engine, text_va, unicode)?;
        if let Some(window) = find_window_mut(state, child) {
            window.control_text = text;
            window.invalidated = true;
        }
    }

    let return_value = u64::from(success);
    ctx.finish(return_value)
}

/// Handles `USER32.dll!SetDlgItemInt`.
///
/// Converts the integer to a decimal string (a leading `-` when `b_signed`)
/// and sets it as the control's text — the same record write `SetDlgItemTextW`
/// performs. Returns FALSE when the item does not exist.
pub fn handle_set_dlg_item_int(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let dialog_hwnd = engine
        .read_rcx()
        .context("failed to read RCX for SetDlgItemInt")?;
    let id_raw = engine
        .read_rdx()
        .context("failed to read RDX for SetDlgItemInt")?;
    let value_raw = engine
        .read_r8()
        .context("failed to read R8 for SetDlgItemInt")?;
    let signed_raw = engine
        .read_r9()
        .context("failed to read R9 for SetDlgItemInt")?;
    let id = u16::try_from(id_raw & u64::from(u32::MAX)).unwrap_or(0);

    let child = get_dlg_item(state, dialog_hwnd, id);
    let success = child != 0;
    if success {
        // The value is a UINT; `b_signed` interprets it as a two's-complement
        // LONG (Windows formats with the locale's digit characters; the en-US
        // digits are the faithful WIE default).
        let value = value_raw & u64::from(u32::MAX);
        let text = if signed_raw != 0 {
            let as_i32 = i32::from_le_bytes(u32::try_from(value).unwrap_or(0).to_le_bytes());
            as_i32.to_string()
        } else {
            value.to_string()
        };
        if let Some(window) = find_window_mut(state, child) {
            window.control_text = text;
            window.invalidated = true;
        }
        // Real Windows `SetDlgItemInt` sends `WM_SETTEXT` to the child, so an
        // EDIT's caret lands at the END of the new value (typing appends to a
        // prefilled dialog field). The direct write above bypasses that
        // dispatch, so mirror the WM_SETTEXT arm's caret placement here.
        if let Some(kind) = find_window_mut(state, child).map(|w| w.control_kind)
            && kind == Some(crate::user32::controls::ControlClassKind::Edit)
        {
            crate::user32::controls::edit_set_selection(state, child, -1, -1);
            crate::user32::controls::edit_reset_invalid_rows(state, child);
        }
    }

    let return_value = u64::from(success);
    ctx.finish(return_value)
}

/// Handles `USER32.dll!GetDlgItemInt`.
///
/// Parses the child's text like Windows: skip leading whitespace, an optional
/// `-` (only when `b_signed`), then decimal digits, stopping at the first
/// non-digit. `lpTranslated` receives FALSE when the text has no convertible
/// digits or the value overflows 32 bits; a negative signed value is returned
/// as its two's-complement UINT.
pub fn handle_get_dlg_item_int(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let dialog_hwnd = engine
        .read_rcx()
        .context("failed to read RCX for GetDlgItemInt")?;
    let id_raw = engine
        .read_rdx()
        .context("failed to read RDX for GetDlgItemInt")?;
    let translated_va = engine
        .read_r8()
        .context("failed to read R8 for GetDlgItemInt")?;
    let signed_raw = engine
        .read_r9()
        .context("failed to read R9 for GetDlgItemInt")?;
    let id = u16::try_from(id_raw & u64::from(u32::MAX)).unwrap_or(0);

    let child = get_dlg_item(state, dialog_hwnd, id);
    let text = if child == 0 {
        String::new()
    } else {
        find_window_ref(state, child).map_or_else(String::new, |window| {
            if window.control_kind.is_some() {
                window.control_text.clone()
            } else {
                window.title.clone()
            }
        })
    };

    let (value, translated) = parse_dlg_item_int(&text, signed_raw != 0);

    if translated_va != 0 {
        write_guest_u32(engine, translated_va, u32::from(translated))?;
    }

    let return_value = u64::from(value);
    ctx.finish(return_value)
}

/// Parse a control's text the way `GetDlgItemInt` does.
///
/// Returns `(value, translated)`: `translated` is FALSE when the text yields
/// no digits (or the value overflows 32 bits) — Windows then returns 0 and
/// sets `*lpTranslated` FALSE.
#[must_use]
fn parse_dlg_item_int(text: &str, signed: bool) -> (u32, bool) {
    let trimmed = text.trim_start();
    let (negative, digits) = if signed {
        trimmed
            .strip_prefix('-')
            .map_or((false, trimmed), |rest| (true, rest))
    } else {
        (false, trimmed)
    };

    let mut value: u64 = 0;
    let mut any_digit = false;
    let mut overflow = false;
    for ch in digits.chars() {
        let Some(digit) = ch.to_digit(10) else { break };
        any_digit = true;
        value = value.saturating_mul(10).saturating_add(u64::from(digit));
        if value > u64::from(u32::MAX) {
            overflow = true;
        }
    }
    if !any_digit || overflow {
        return (0, false);
    }
    let magnitude = u32::try_from(value).unwrap_or(0);
    // Negative signed values wrap to their two's-complement UINT (the return
    // type is UINT even when `b_signed` is set).
    (
        if negative {
            magnitude.wrapping_neg()
        } else {
            magnitude
        },
        true,
    )
}

/// Handles `USER32.dll!SendDlgItemMessageW`.
///
/// Resolves the child by id and forwards the message to it through the same
/// dispatch as `SendMessageW` (guest WndProc bridge or host-side control
/// handling). Returns 0 when the item does not exist.
pub fn handle_send_dlg_item_message_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let dialog_hwnd = engine
        .read_rcx()
        .context("failed to read RCX for SendDlgItemMessageW")?;
    let id_raw = engine
        .read_rdx()
        .context("failed to read RDX for SendDlgItemMessageW")?;
    let message_raw = engine
        .read_r8()
        .context("failed to read R8 for SendDlgItemMessageW")?;
    let word_parameter = engine
        .read_r9()
        .context("failed to read R9 for SendDlgItemMessageW")?;
    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for SendDlgItemMessageW")?;
    let long_parameter = read_u64(engine, rsp.wrapping_add(0x28))
        .context("failed to read 5th arg for SendDlgItemMessageW")?;
    let id = u16::try_from(id_raw & u64::from(u32::MAX)).unwrap_or(0);

    let child = get_dlg_item(state, dialog_hwnd, id);
    if child == 0 {
        return ctx.finish(0);
    }

    // Rewrite the registers to address the child, then run the shared
    // SendMessage path (it reads the four message arguments from registers).
    engine
        .write_rcx(child)
        .context("failed to write RCX for SendDlgItemMessageW")?;
    engine
        .write_rdx(message_raw)
        .context("failed to write RDX for SendDlgItemMessageW")?;
    engine
        .write_r8(word_parameter)
        .context("failed to write R8 for SendDlgItemMessageW")?;
    engine
        .write_r9(long_parameter)
        .context("failed to write R9 for SendDlgItemMessageW")?;
    handle_send_message(ctx, true, "SendDlgItemMessageW")
}

/// Read-only window lookup (for helpers holding `&WinApiState`).
fn find_window_ref(state: &WinApiState, handle: u64) -> Option<&WindowRecord> {
    state.try_window_state().and_then(|ws| {
        ws.windows
            .iter()
            .find(|window| window.handle == crate::handles::Hwnd::from(handle))
    })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::sync_obj::SyncState;
    use crate::user32::{
        CreateWindowRequest, WS_CHILD, WS_CLIPCHILDREN, WS_VISIBLE, WindowClassIdentifier,
        create_window_record,
    };
    use crate::vfs::VolumeConfig;
    use crate::{
        FileHandle, FindFileHandle, GuestHeap, GuestStdinMode, HeapState, KernelState,
        ModuleHandle, ModuleState, ProcessState, RegistryKeyHandle, ResourceHandle, ThreadState,
        WinApiEnvironment,
    };
    use ahash::HashMap;
    use ahash::HashMapExt;
    use std::sync::{Arc, Mutex};
    use wie_cpu::{CpuEngine, IcedCpu, RwxPerms};

    const STACK_VA: u64 = 0x100_0000;
    const STACK_SIZE: usize = 0x1_0000;
    const STACK_TOP: u64 = 0x100_FF00;

    fn test_engine() -> IcedCpu {
        let mut cpu = IcedCpu::open_x86_64();
        cpu.mem_map(0x1000, 0x10_0000, RwxPerms::ALL)
            .expect("map test memory");
        cpu.mem_map(STACK_VA, STACK_SIZE, RwxPerms::ALL)
            .expect("map test stack");
        cpu.mem_write(STACK_TOP, &0_u64.to_le_bytes())
            .expect("write return address");
        cpu.write_rsp(STACK_TOP).ok();
        cpu
    }

    fn test_state() -> WinApiState {
        let mut heap = GuestHeap::new(0x2000, 0x10000);
        heap.attach_guest_control(0x2000);
        WinApiState {
            heap_state: HeapState {
                heap,
                next_fls_index: 0,
                fls_slots: Vec::new(),
                guest_fls_table_va: 0,
            },
            file_io: crate::FileIoState {
                executable_file_size: 0,
                executable_file_bytes: Arc::new(Vec::new()),
                executable_file_cursor: 0,
                next_find_handle: FindFileHandle::from(0),
                find_handles: Vec::new(),
                host_file_mounts: Vec::new(),
                virtual_files: Vec::new(),
                open_files: HashMap::new(),
                next_file_handle: FileHandle::from(0),
                next_resource_handle: ResourceHandle::from(0),
                resources: Vec::new(),
                current_directory_wide: Vec::new(),
                bottle_root: None,
                volumes: VolumeConfig::default(),
                guest_file_data_next: 0,
                guest_io: None,
                stdin_bytes: Vec::new(),
                stdin_cursor: 0,
                stdin_mode: GuestStdinMode::InjectOnly,
                ucrt_files: HashMap::new(),
                ucrt_next_file_va: 0x0000_0000_6900_0000,
                cached_streams: HashMap::new(),
            },
            process: ProcessState {
                last_error: 0,
                next_registry_key_handle: RegistryKeyHandle::from(0),
                registry_keys: Vec::new(),
                main_module_file_name: String::new(),
                main_module_path: String::new(),
                main_module_host_dir: None,
                error_mode: 0,
                suspended_threads: HashMap::new(),
                environment: crate::DEFAULT_ENVIRONMENT
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                    .collect(),
                main_module_dialogs: Vec::new(),
                main_module_menus: Vec::new(),
                main_module_strings: Vec::new(),
                main_module_accelerators: Vec::new(),
            },
            kernel: KernelState {
                threads: ThreadState::primary(),
                sync: SyncState::new(),
                seh_pending: HashMap::new(),
            },
            dll_states: crate::DllStateMap::new(),
            message_queue: Arc::new(Mutex::new(crate::present::MessageQueue::default())),
            module_state: ModuleState {
                loaded_modules: HashMap::new(),
                import_resolver: None,
                get_proc_address_cache: HashMap::new(),
                next_module_handle: ModuleHandle::from(crate::dll_loader::REAL_MODULE_HANDLE_BASE),
            },
        }
    }

    fn test_environment() -> WinApiEnvironment {
        WinApiEnvironment {
            image_base: 0,
            command_line_a_ptr: 0,
            command_line_w_ptr: 0,
            environment_strings_w_ptr: 0,
            module_file_name_a_ptr: 0,
            module_file_name_w_ptr: 0,
            process_heap_handle: 1,
        }
    }

    fn write_regs(cpu: &mut IcedCpu, rcx: u64, rdx: u64, r8: u64, r9: u64) {
        cpu.write_rcx(rcx).ok();
        cpu.write_rdx(rdx).ok();
        cpu.write_r8(r8).ok();
        cpu.write_r9(r9).ok();
        cpu.write_rsp(STACK_TOP).ok();
    }

    /// Set registers plus trailing stack arguments (5th arg at rsp+0x28, …).
    fn write_regs_stack(cpu: &mut IcedCpu, rcx: u64, rdx: u64, r8: u64, r9: u64, stack: &[u64]) {
        write_regs(cpu, rcx, rdx, r8, r9);
        for (index, arg) in stack.iter().enumerate() {
            let va = STACK_TOP
                .wrapping_add(0x28)
                .wrapping_add(u64::try_from(index).unwrap_or(0).wrapping_mul(8));
            cpu.mem_write(va, &arg.to_le_bytes())
                .expect("write stack arg");
        }
    }

    /// A dialog window with one EDIT child at `edit_id` (control ids live in
    /// the child's `menu_handle`, mirroring `CreateWindowEx`).
    fn build_dialog_with_edit(state: &mut WinApiState, edit_id: u16) -> (u64, u64) {
        let (dialog_hwnd, _, _) = create_window_record(
            state,
            CreateWindowRequest {
                class_identifier: WindowClassIdentifier::Name("Dialog".to_owned()),
                title: "GoTo".to_owned(),
                style: WS_VISIBLE | WS_CLIPCHILDREN,
                extended_style: 0,
                parent_handle: 0,
                menu_handle: 0,
                instance_handle: 0,
                x: 0,
                y: 0,
                width: 300,
                height: 100,
            },
            true,
        )
        .expect("create dialog");
        let (edit_hwnd, _, _) = create_window_record(
            state,
            CreateWindowRequest {
                class_identifier: WindowClassIdentifier::Atom(0x0081), // EDIT
                title: String::new(),
                style: WS_CHILD | WS_VISIBLE,
                extended_style: 0,
                parent_handle: dialog_hwnd,
                menu_handle: u64::from(edit_id),
                instance_handle: 0,
                x: 0,
                y: 0,
                width: 100,
                height: 20,
            },
            true,
        )
        .expect("create edit");
        (dialog_hwnd, edit_hwnd)
    }

    // ── SetDlgItemInt / GetDlgItemInt / SendDlgItemMessageW ──────────────

    fn control_text(state: &WinApiState, hwnd: u64) -> String {
        find_window_ref(state, hwnd).map_or_else(String::new, |window| window.control_text.clone())
    }

    // ── GetDlgItemTextA/W / SetDlgItemTextA/W (the A/W arg pair family) ──

    /// The guest UTF-16LE bytes (NUL-terminated) for `text`.
    fn utf16_c_string_bytes(text: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes
    }

    /// Read a NUL-terminated UTF-16LE guest buffer at `ptr` (test-side).
    fn read_guest_utf16(engine: &mut IcedCpu, ptr: u64) -> String {
        let mut raw = [0_u8; 128];
        engine.mem_read(ptr, &mut raw).expect("read guest buffer");
        let mut units = Vec::new();
        for pair in raw.chunks_exact(2) {
            let unit = u16::from_le_bytes([pair[0], pair[1]]);
            if unit == 0 {
                break;
            }
            units.push(unit);
        }
        String::from_utf16_lossy(&units)
    }

    #[test]
    fn dlg_item_text_w_round_trips_via_wide_boundary() {
        // The demo W-path pin: SetDlgItemTextW with "dialog text — ✓" then
        // GetDlgItemTextW must echo the identical text back — em dash and
        // U+2713 included (the A path would degrade ✓ to '?').
        let mut engine = test_engine();
        let mut state = test_state();
        let (dialog, _edit) = build_dialog_with_edit(&mut state, 0x10);

        let text_va = 0x4000;
        engine
            .mem_write(text_va, &utf16_c_string_bytes("dialog text — ✓"))
            .expect("write guest text");
        write_regs(&mut engine, dialog, 0x10, text_va, 0);
        let result = handle_set_dlg_item_text_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("SetDlgItemTextW should succeed");
        assert_eq!(result.return_value, 1, "existing item sets text");

        let out_va = 0x5000;
        write_regs(&mut engine, dialog, 0x10, out_va, 64);
        let result = handle_get_dlg_item_text_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("GetDlgItemTextW should succeed");
        assert_eq!(
            result.return_value, 15,
            "15 UTF-16 units copied excluding the NUL"
        );
        assert_eq!(
            read_guest_utf16(&mut engine, out_va),
            "dialog text — ✓",
            "the W round-trip must be lossless"
        );
    }

    #[test]
    fn dlg_item_text_a_reads_utf8_first_and_writes_cp1252() {
        // The A-path asymmetry through the converted glue: the guest stores
        // mingw A-string literals as UTF-8 ("café" = 63 61 66 C3 A9), and the
        // write side re-encodes as the cp1252 bytes Windows writes (é → E9).
        let mut engine = test_engine();
        let mut state = test_state();
        let (dialog, _edit) = build_dialog_with_edit(&mut state, 0x10);

        let text_va = 0x4000;
        engine
            .mem_write(text_va, b"caf\xC3\xA9\0")
            .expect("write UTF-8 literal");
        write_regs(&mut engine, dialog, 0x10, text_va, 0);
        let result = handle_set_dlg_item_text_a(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("SetDlgItemTextA should succeed");
        assert_eq!(result.return_value, 1, "existing item sets text");

        let out_va = 0x5000;
        write_regs(&mut engine, dialog, 0x10, out_va, 8);
        let result = handle_get_dlg_item_text_a(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("GetDlgItemTextA should succeed");
        assert_eq!(result.return_value, 4, "4 CP1252 chars, not 5 UTF-8 bytes");
        let mut raw = [0_u8; 8];
        engine.mem_read(out_va, &mut raw).expect("read out buffer");
        assert_eq!(
            &raw[..5],
            &[0x63, 0x61, 0x66, 0xE9, 0x00],
            "UTF-8-first read, cp1252 write, NUL-terminated"
        );
    }

    #[test]
    fn set_dlg_item_int_writes_decimal_text() {
        let mut engine = test_engine();
        let mut state = test_state();
        let (dialog, edit) = build_dialog_with_edit(&mut state, 0x10);

        write_regs(&mut engine, dialog, 0x10, 42, 1);
        let result = handle_set_dlg_item_int(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("SetDlgItemInt should succeed");
        assert_eq!(result.return_value, 1, "existing item sets text");
        assert_eq!(control_text(&state, edit), "42");
    }

    #[test]
    fn set_dlg_item_int_signed_negative_prefixes_minus() {
        let mut engine = test_engine();
        let mut state = test_state();
        let (dialog, edit) = build_dialog_with_edit(&mut state, 0x10);

        // 0xFFFF_FFFF as a signed LONG is -1.
        write_regs(&mut engine, dialog, 0x10, 0xFFFF_FFFF, 1);
        handle_set_dlg_item_int(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("SetDlgItemInt should succeed");
        assert_eq!(control_text(&state, edit), "-1");
        // Unsigned mode formats the raw u32 (no sign).
        write_regs(&mut engine, dialog, 0x10, 0xFFFF_FFFF, 0);
        handle_set_dlg_item_int(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("SetDlgItemInt should succeed");
        assert_eq!(control_text(&state, edit), "4294967295");
    }

    #[test]
    fn set_dlg_item_int_missing_item_returns_false() {
        let mut engine = test_engine();
        let mut state = test_state();
        let (dialog, _edit) = build_dialog_with_edit(&mut state, 0x10);

        write_regs(&mut engine, dialog, 0x99, 7, 1);
        let result = handle_set_dlg_item_int(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("SetDlgItemInt should succeed");
        assert_eq!(result.return_value, 0, "unknown id returns FALSE");
    }

    #[test]
    fn get_dlg_item_int_round_trip() {
        let mut engine = test_engine();
        let mut state = test_state();
        let (dialog, _edit) = build_dialog_with_edit(&mut state, 0x10);
        let translated_va = 0x4000_u64;

        write_regs(&mut engine, dialog, 0x10, 123, 0);
        handle_set_dlg_item_int(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("SetDlgItemInt should succeed");

        write_regs(&mut engine, dialog, 0x10, translated_va, 1);
        let result = handle_get_dlg_item_int(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("GetDlgItemInt should succeed");
        assert_eq!(result.return_value, 123);
        let mut flag = [0_u8; 4];
        engine
            .mem_read(translated_va, &mut flag)
            .expect("read translated flag");
        assert_eq!(u32::from_le_bytes(flag), 1, "translated flag is TRUE");
    }

    #[test]
    fn get_dlg_item_int_leading_space_and_sign() {
        let mut engine = test_engine();
        let mut state = test_state();
        let (dialog, edit) = build_dialog_with_edit(&mut state, 0x10);

        // Leading whitespace is skipped.
        if let Some(window) = find_window_mut(&mut state, edit) {
            window.control_text = "  42".to_owned();
        }
        write_regs(&mut engine, dialog, 0x10, 0, 0);
        let result = handle_get_dlg_item_int(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("GetDlgItemInt should succeed");
        assert_eq!(result.return_value, 42);

        // Signed "-5" wraps to its two's-complement UINT (0xFFFF_FFFB).
        if let Some(window) = find_window_mut(&mut state, edit) {
            window.control_text = "-5".to_owned();
        }
        write_regs(&mut engine, dialog, 0x10, 0, 1);
        let result = handle_get_dlg_item_int(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("GetDlgItemInt should succeed");
        assert_eq!(result.return_value, 0xFFFF_FFFB);

        // Unsigned mode rejects a sign character (no digits parse).
        write_regs(&mut engine, dialog, 0x10, 0, 0);
        let result = handle_get_dlg_item_int(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("GetDlgItemInt should succeed");
        assert_eq!(result.return_value, 0);
    }

    #[test]
    fn get_dlg_item_int_parse_failure_sets_flag_false() {
        let mut engine = test_engine();
        let mut state = test_state();
        let (dialog, edit) = build_dialog_with_edit(&mut state, 0x10);
        let translated_va = 0x4000_u64;

        if let Some(window) = find_window_mut(&mut state, edit) {
            window.control_text = "abc".to_owned();
        }
        write_regs(&mut engine, dialog, 0x10, translated_va, 1);
        let result = handle_get_dlg_item_int(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("GetDlgItemInt should succeed");
        assert_eq!(result.return_value, 0);
        let mut flag = [0_u8; 4];
        engine
            .mem_read(translated_va, &mut flag)
            .expect("read translated flag");
        assert_eq!(u32::from_le_bytes(flag), 0, "translated flag is FALSE");
    }

    #[test]
    fn get_dlg_item_int_overflow_fails_but_max_ok() {
        let mut engine = test_engine();
        let mut state = test_state();
        let (dialog, edit) = build_dialog_with_edit(&mut state, 0x10);
        let translated_va = 0x4000_u64;

        // u32::MAX parses fine.
        if let Some(window) = find_window_mut(&mut state, edit) {
            window.control_text = "4294967295".to_owned();
        }
        write_regs(&mut engine, dialog, 0x10, translated_va, 0);
        let result = handle_get_dlg_item_int(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("GetDlgItemInt should succeed");
        assert_eq!(result.return_value, 0xFFFF_FFFF);

        // One past u32::MAX overflows → 0 + FALSE.
        if let Some(window) = find_window_mut(&mut state, edit) {
            window.control_text = "4294967296".to_owned();
        }
        write_regs(&mut engine, dialog, 0x10, translated_va, 0);
        let result = handle_get_dlg_item_int(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("GetDlgItemInt should succeed");
        assert_eq!(result.return_value, 0);
        let mut flag = [0_u8; 4];
        engine
            .mem_read(translated_va, &mut flag)
            .expect("read translated flag");
        assert_eq!(u32::from_le_bytes(flag), 0, "overflow reports FALSE");
    }

    #[test]
    fn send_dlg_item_message_w_forwards_to_child() {
        let mut engine = test_engine();
        let mut state = test_state();
        let (dialog, _edit) = build_dialog_with_edit(&mut state, 0x10);

        // EM_SETSEL(0, -1) on the child edit: returns TRUE through the
        // host-side control dispatch (proves the message reached the child).
        write_regs_stack(
            &mut engine,
            dialog,
            0x10,
            u64::from(crate::user32::wm::WinMsg::EM_SETSEL.as_u32()),
            0,
            &[0xFFFF_FFFF, 0],
        );
        let result = handle_send_dlg_item_message_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("SendDlgItemMessageW should succeed");
        assert_eq!(result.return_value, 1, "EM_SETSEL returns TRUE");
    }

    #[test]
    fn send_dlg_item_message_w_missing_item_returns_zero() {
        let mut engine = test_engine();
        let mut state = test_state();
        let (dialog, _edit) = build_dialog_with_edit(&mut state, 0x10);

        write_regs_stack(
            &mut engine,
            dialog,
            0x99,
            u64::from(crate::user32::wm::WinMsg::EM_SETSEL.as_u32()),
            0,
            &[0],
        );
        let result = handle_send_dlg_item_message_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("SendDlgItemMessageW should succeed");
        assert_eq!(result.return_value, 0, "unknown id returns 0");
    }
}
