//! `RT_ACCELERATOR` loading and `TranslateAccelerator` key translation.
//!
//! `LoadAcceleratorsA/W` resolves a parsed table by resource id (main EXE
//! module only — the same lifecycle as dialogs/menus/strings) and caches one
//! fake `HACCEL` per (module, id) pair, so repeated loads return the same
//! handle. `TranslateAcceleratorA/W` matches a `WM_KEYDOWN`/`WM_CHAR` message
//! against the table entries and posts a `WM_COMMAND` on a hit.
//!
//! Matching follows the documented `ACCEL` semantics: `FVIRTKEY` entries key
//! on the VK code and require the SHIFT/CTRL/ALT state to equal their
//! `FSHIFT`/`FCONTROL`/`FALT` bits exactly; non-`FVIRTKEY` entries key on the
//! character of a `WM_CHAR`. `FNOINVERT` (2) is accepted and ignored, as is
//! `FLASTENTRY` (0x80). The extended `FVIRTKEY`+character "keyboard language"
//! behavior is not modeled (YAGNI); non-`FVIRTKEY` entries translate from the
//! message's wParam, not from the keyboard layout.

use super::{
    Context, KEY_STATE_DOWN, Msg, Result, WM_CHAR, WM_COMMAND, WM_KEYDOWN, WM_SYSCHAR,
    WM_SYSKEYDOWN, WinApiHandlerResult, WinApiState, make_command_wparam, with_typed_read,
};
use crate::HandlerContext;
use crate::handles::{Haccel, Hwnd};
use crate::state::KeyboardState;
use wie_pe::resources::AccelEntry;

/// `ACCEL` `fFlags` bits (winuser.h).
const FVIRTKEY: u16 = 0x01;
const FSHIFT: u16 = 0x04;
const FCONTROL: u16 = 0x08;
const FALT: u16 = 0x10;

/// Virtual-key codes consulted while matching modifiers.
const VK_SHIFT: usize = 0x10;
const VK_CONTROL: usize = 0x11;
const VK_MENU: usize = 0x12;

/// One loaded fake accelerator table.
///
/// The parsed entries live on `ProcessState::main_module_accelerators`, so
/// the record only carries the (module, resource id) pair needed to resolve
/// them — and that pair doubles as the cache key for `LoadAcceleratorsA/W`.
#[derive(Debug, Clone)]
pub struct AccelRecord {
    /// Fake `HACCEL` handle returned to the guest.
    pub handle: u64,
    /// Module instance the table was loaded from.
    pub instance_handle: u64,
    /// Resource id the table was loaded under.
    pub table_id: u16,
}

/// Allocate the next fake `HACCEL` handle (mirrors `allocate_menu_handle`).
pub(crate) fn allocate_accel_handle(state: &mut WinApiState) -> Result<u64> {
    let handle = state.window_state().next_accel_handle.as_u64();
    state.window_state().next_accel_handle = Haccel::from(
        state
            .window_state()
            .next_accel_handle
            .as_u64()
            .checked_add(1)
            .context("accelerator handle allocator overflow")?,
    );
    Ok(handle)
}

/// Handles `USER32.dll!LoadAcceleratorsW`.
pub fn handle_load_accelerators_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_load_accelerators(ctx, "LoadAcceleratorsW")
}
/// Handles `USER32.dll!LoadAcceleratorsA`.
pub fn handle_load_accelerators_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_load_accelerators(ctx, "LoadAcceleratorsA")
}

/// Shared `LoadAcceleratorsA/W` implementation.
///
/// Win64 ABI: `rcx` = hinst, `rdx` = `lpTableName`. Only `MAKEINTRESOURCE`
/// ids are resolvable — a string-named table has no parsed counterpart and
/// returns `NULL`, mirroring how `LoadStringA/W` resolves ids only.
fn handle_load_accelerators(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let instance_handle = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;
    let table_name_raw = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;

    let table_id = if table_name_raw >> 16 == 0 {
        u16::try_from(table_name_raw & u64::from(u16::MAX)).unwrap_or(0)
    } else {
        0
    };

    let return_value = if table_id == 0 || instance_handle != ctx.environment.image_base {
        // Not the main EXE (or a named table): no parsed table to resolve.
        0
    } else {
        load_accel_handle(state, instance_handle, table_id)?
    };

    tracing::debug!(
        target: "wiegui",
        instance_handle,
        table_id,
        haccel = return_value,
        "{api_name}"
    );

    ctx.finish(return_value)
}

/// Resolve `(instance_handle, table_id)` to a fake `HACCEL`, caching it.
///
/// An id that does not match any parsed table resolves to `NULL`; a repeat
/// load of the same pair returns the previously allocated handle.
fn load_accel_handle(state: &mut WinApiState, instance_handle: u64, table_id: u16) -> Result<u64> {
    // A cached record proves the id validated at load time, so the cache
    // lookup runs first and a repeat load skips the parsed-table scan.
    if let Some(record) = state
        .window_state()
        .accel_tables
        .iter()
        .find(|record| record.instance_handle == instance_handle && record.table_id == table_id)
    {
        return Ok(record.handle);
    }

    // Single `find` over the parsed tables doubles as the existence check:
    // an id matching no table resolves to NULL and is never cached.
    let Some(_) = state
        .process
        .main_module_accelerators
        .iter()
        .find(|table| table.id == u32::from(table_id))
    else {
        return Ok(0);
    };

    let handle = allocate_accel_handle(state)?;
    state.window_state().accel_tables.push(AccelRecord {
        handle,
        instance_handle,
        table_id,
    });
    Ok(handle)
}

/// Handles `USER32.dll!DestroyAcceleratorTable`.
///
/// Win64 ABI: `rcx` = haccel. The matching `AccelRecord` is removed so a
/// subsequent `LoadAcceleratorsA/W` of the same (module, id) pair allocates a
/// fresh handle. `TRUE` is returned only when a record was actually removed —
/// an unknown handle yields `FALSE`, matching the Windows API spec. This
/// intentionally contrasts with `DestroyMenu`, which always succeeds here.
pub fn handle_destroy_accelerator_table(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let accel_handle = engine
        .read_rcx()
        .context("failed to read RCX for DestroyAcceleratorTable")?;

    let ws = state.window_state();
    let before = ws.accel_tables.len();
    ws.accel_tables
        .retain(|record| record.handle != accel_handle);
    let return_value = u64::from(ws.accel_tables.len() != before);

    ctx.finish(return_value)
}

/// Handles `USER32.dll!TranslateAcceleratorW`.
pub fn handle_translate_accelerator_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_translate_accelerator(ctx, "TranslateAcceleratorW")
}
/// Handles `USER32.dll!TranslateAcceleratorA`.
pub fn handle_translate_accelerator_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_translate_accelerator(ctx, "TranslateAcceleratorA")
}

/// Shared `TranslateAcceleratorA/W` implementation.
///
/// Win64 ABI: `rcx` = hwnd, `rdx` = haccel, `r8` = `LPMSG`. On a match the
/// `WM_COMMAND` (wParam = the entry's command id, lParam = 0) is posted to
/// `hwnd` and `TRUE` is returned; otherwise `FALSE` and the message queue is
/// untouched.
fn handle_translate_accelerator(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;
    let accel_handle = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;
    let message_ptr = engine
        .read_r8()
        .with_context(|| format!("failed to read R8 for {api_name}"))?;

    // One shared-lock borrow instead of two per-field reads; the MSG layout
    // + pinned offsets (message @8, wParam @16) live in
    // `crate::guest_layout::Msg`.
    let (message, word_parameter) =
        with_typed_read::<Msg, _, _>(engine, message_ptr, |msg| Ok((msg.message, msg.wparam)))
            .with_context(|| format!("failed to read {api_name} MSG"))?;

    // Resolve handle → table id → parsed entries, then match.
    let table_id = state
        .window_state()
        .accel_tables
        .iter()
        .find(|record| record.handle == accel_handle)
        .map(|record| record.table_id);
    // Borrow the keyboard state rather than clone 256 bytes per keystroke.
    // All borrows in this span are shared — `try_window_state` takes `&self`
    // and `.iter()` reborrows `state.process` immutably — so the reference
    // coexists with the iterator, and NLL ends `keyboard`'s borrow at
    // `keyboard?`, before any `&mut state` access. The `?` None arm is
    // unreachable: the window-state slot exists by the dispatch model's
    // runtime invariant (`window_state()` get_or_init's it on demand).
    let keyboard: Option<&KeyboardState> = state.try_window_state().map(|ws| &ws.keyboard_state);
    let command_id: Option<u16> = state
        .process
        .main_module_accelerators
        .iter()
        .find(|table| table_id.is_some_and(|id| table.id == u32::from(id)))
        .and_then(|table| {
            find_matching_command(&table.entries, message, word_parameter, keyboard?)
        });

    let return_value = if let Some(command_id) = command_id {
        let mut queue = state.lock_message_queue();
        queue.push(
            Hwnd::from(window_handle),
            WM_COMMAND,
            // MAKEWPARAM(id, 0): accelerator command ids are u16, so the
            // high word of wParam is 0 (no notification code).
            make_command_wparam(u64::from(command_id), 0),
            0,
        )?;
        1
    } else {
        0
    };

    ctx.finish(return_value)
}

/// First table entry that translates `(message, wParam)`.
///
/// `FVIRTKEY` entries match `WM_KEYDOWN`/`WM_SYSKEYDOWN` on the VK code and
/// require the current SHIFT/CTRL/ALT state to equal the entry's modifier
/// bits exactly. Non-`FVIRTKEY` entries match `WM_CHAR`/`WM_SYSCHAR` on the
/// ANSI character. `FNOINVERT` is decorative here and ignored.
fn find_matching_command(
    entries: &[AccelEntry],
    message: u32,
    word_parameter: u64,
    keyboard: &KeyboardState,
) -> Option<u16> {
    let wparam = u16::try_from(word_parameter & u64::from(u16::MAX)).unwrap_or(0);
    entries.iter().find_map(|entry| {
        let matches = if entry.flags & FVIRTKEY != 0 {
            (message == WM_KEYDOWN || message == WM_SYSKEYDOWN)
                && entry.key == wparam
                && modifiers_match(entry.flags, keyboard)
        } else {
            (message == WM_CHAR || message == WM_SYSCHAR) && entry.key == wparam
        };
        matches.then_some(entry.command_id)
    })
}

/// Whether the thread's SHIFT/CTRL/ALT state equals the entry's requested
/// modifier set exactly (a pressed key with no matching `F*` bit kills the
/// match, mirroring real `TranslateAccelerator`).
fn modifiers_match(entry_flags: u16, keyboard: &KeyboardState) -> bool {
    let shift_down = keyboard.get(VK_SHIFT) & KEY_STATE_DOWN != 0;
    let ctrl_down = keyboard.get(VK_CONTROL) & KEY_STATE_DOWN != 0;
    let alt_down = keyboard.get(VK_MENU) & KEY_STATE_DOWN != 0;
    shift_down == (entry_flags & FSHIFT != 0)
        && ctrl_down == (entry_flags & FCONTROL != 0)
        && alt_down == (entry_flags & FALT != 0)
}
