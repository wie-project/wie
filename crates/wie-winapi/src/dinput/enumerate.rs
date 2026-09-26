//! `IDirectInput8::EnumDevices` — the two-device enumeration, driven by the
//! runtime's full-iteration guest-callback bridge
//! ([`WinApiControlSignal::EnumerationCallbackRequested`]).
//!
//! # Why the enumeration bridge and not the WndProc bridge
//!
//! `DIDEVICEENUMCALLBACK` is `BOOL(LPCDIDEVICEINSTANCEA, LPVOID)` — a
//! **64-bit** `pvRef` in RDX. `GuestCallbackRequest`'s WndProc encoding puts a
//! 32-bit message in RDX, which would truncate the guest's context pointer.
//! The enumeration frame writer keeps RDX full-width, so this lane uses the
//! enumeration signal with the same argument packing the GDI font lane uses:
//! `window_handle` = `lpddi`, `word_parameter` = the stop/enumstop flags,
//! `long_parameter` = `pvRef`.
//!
//! # Why `gdi32::enumerate` hosts the router
//!
//! The runtime's continuation path names exactly one advance function
//! (`session::pump` → `gdi32::enumerate::advance_enumeration`). Rather than
//! teach the run loop about a second lane, `gdi32::enumerate` gained a
//! three-line router that offers the id to this module first. The layering
//! compromise is documented at the router.

use std::sync::Mutex;

use anyhow::{Context, Result};

use crate::gdi32::{ArgReg, read_arg};
use crate::guest_memory::{read_u32, write_u32 as write_guest_u32};
use crate::{
    GuestCallbackRequest, HandlerContext, OuterReturn, WinApiControlSignal, WinApiHandlerResult,
    WinApiState,
};

use super::device::write_device_instance;
use super::{DEVICES, DI_OK, DInputDeviceClass};

/// `DIENUM_STOP` (dinput.h:215) — the callback returned FALSE, stop
/// enumerating.
///
/// The runtime's generic continuation test is "the callback's return value is
/// non-zero", which coincides exactly with the documented contract: a guest
/// returns `DIENUM_CONTINUE` (1) to keep going and `DIENUM_STOP` (0) to stop.
/// A guest returning neither is outside the contract; the runtime's
/// non-zero test then continues, which is why the two constants are pinned by
/// a test rather than checked here (this function has no access to the
/// callback's return value — the runtime tests it before calling in).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const DIENUM_STOP: u32 = 0;
/// `DIENUM_CONTINUE` (dinput.h:216) — the callback returned TRUE, keep
/// enumerating. See [`DIENUM_STOP`].
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const DIENUM_CONTINUE: u32 = 1;

/// `sizeof(DIDEVICEINSTANCEA)` on Win64 (dinput.h:443-455).
const DEVICE_INSTANCE_SIZE: u32 = 580;

/// One in-flight `EnumDevices`.
///
/// Owns the single `DIDEVICEINSTANCEA` the guest callback sees: the bridge is
/// one record per enumeration, rewritten in place between items, so the pointer
/// the guest received for item *n* is the same pointer it gets for item *n+1* —
/// which is what real DirectInput does, the record only being valid for the
/// duration of the callback.
struct EnumerationState {
    /// The devices to visit, in `EnumDevices` order (keyboard, then mouse),
    /// already filtered by the caller's `dwDevType`.
    items: Vec<DInputDeviceClass>,
    /// Index of the item the *next* callback reports.
    index: usize,
    /// Guest address of the `DIDEVICEINSTANCEA` the callback points at.
    buffer_va: u64,
    /// The guest `DIDEVICEENUMCALLBACK`.
    callback_va: u64,
    /// The guest's `pvRef`, forwarded verbatim.
    context: u64,
    /// `dwFlags` the caller passed (`DIEDFL_*`).
    flags: u32,
}

/// Active enumerations keyed by the `enumeration_id` the signal carries. The
/// runtime advances the index on each non-zero callback return.
static ENUMERATIONS: Mutex<Vec<(u64, EnumerationState)>> = Mutex::new(Vec::new());
static NEXT_ENUM_ID: Mutex<u64> = Mutex::new(1);

/// Register a new enumeration and return its id.
fn register(state: EnumerationState) -> u64 {
    let id = {
        let mut next = NEXT_ENUM_ID
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = *next;
        *next = next.wrapping_add(1);
        id
    };
    ENUMERATIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((id, state));
    id
}

/// `HRESULT EnumDevices(DWORD dwDevType, LPDIENUMDEVICESCALLBACK, LPVOID pvRef,
/// DWORD dwFlags)` — dinput.h:2410.
///
/// Win64: `rcx = this`, `rdx = dwDevType`, `r8 = lpCallback`, `r9 = pvRef`,
/// `[rsp+0x28] = dwFlags`.
///
/// A `dwDevType` filter that matches neither of WIE's two devices (a joystick
/// or gamepad class, say) completes **without ever calling back** and still
/// returns `DI_OK` — the same "no matching device" outcome real DirectInput
/// gives, and the honest one: a guest probing for a joystick is told there is
/// none, not handed silence.
pub(crate) fn dispatch_enumerate_device(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let dev_type = read_arg(engine, ArgReg::Rdx, "EnumDevices")?;
    let callback = read_arg(engine, ArgReg::R8, "EnumDevices")?;
    let context = read_arg(engine, ArgReg::R9, "EnumDevices")?;
    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for EnumDevices")?;
    let flags_address = rsp
        .checked_add(0x28)
        .context("EnumDevices dwFlags address overflow")?;
    let flags = read_u32(engine, flags_address).context("failed to read EnumDevices dwFlags")?;

    // A NULL callback is invalid per the Win32 contract; fail soft with
    // success rather than bridging into a null pointer.
    if callback == 0 {
        return ctx.finish(DI_OK);
    }

    let items: Vec<DInputDeviceClass> = DEVICES
        .into_iter()
        .filter(|class| class.matches_dev_type(dev_type))
        .collect();
    let Some(first) = items.first().copied() else {
        return ctx.finish(DI_OK);
    };

    let buffer_va = {
        let state = &mut *ctx.state;
        state
            .heap_state
            .heap
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .alloc_coherent(engine, u64::from(DEVICE_INSTANCE_SIZE))
    };
    if buffer_va == 0 {
        return ctx.finish(DI_OK);
    }
    // The record's `dwSize` is written up front because every consumer
    // (including WIE's own `write_device_instance`) reads it back to echo it.
    write_guest_u32(engine, buffer_va, DEVICE_INSTANCE_SIZE)
        .context("failed to size the DIDEVICEINSTANCEA")?;

    let id = register(EnumerationState {
        items,
        index: 0,
        buffer_va,
        callback_va: callback,
        context,
        flags,
    });
    // Pre-fill the record for item 0 so the first callback sees a valid one.
    write_device_instance(engine, buffer_va, first)?;

    Err(WinApiControlSignal::EnumerationCallbackRequested {
        request: make_request(buffer_va, callback, context, flags),
        enumeration_id: id,
    }
    .into())
}

/// The `GuestCallbackRequest` for one `DIDEVICEENUMCALLBACK` invocation.
///
/// Argument packing for the enumeration frame writer (RCX/RDX/R8/R9 become
/// `window_handle` / `word_parameter` / `message` / `long_parameter`): RCX and
/// RDX are what the callback actually reads — `lpddi` and `pvRef`. R8 carries
/// the `DIEDFL_*` flags so a trace can explain why the walk stopped, and R9
/// carries the caller's `dwDevType` filter.
fn make_request(buffer_va: u64, callback: u64, context: u64, flags: u32) -> GuestCallbackRequest {
    GuestCallbackRequest {
        callback_address: callback,
        window_handle: buffer_va,
        message: flags,
        word_parameter: u64::from(flags),
        long_parameter: context,
        unicode: false,
        // `EnumDevices` returns DI_OK once the walk completes — never the
        // callback's BOOL, which only means "keep going / stop".
        outer_return: OuterReturn::Fixed(DI_OK),
    }
}

/// Advance a DirectInput device enumeration to the next device.
///
/// Called by the runtime (via the `gdi32::enumerate` router) on each non-zero
/// callback return. Returns `Some(next_request)` to re-enter the callback, or
/// `None` when the enumeration is complete — at which point the state is
/// dropped, so a finished enumeration leaves nothing behind.
pub(crate) fn advance_enumeration(
    engine: &mut dyn wie_cpu::CpuEngine,
    _state: &mut WinApiState,
    enumeration_id: u64,
) -> Result<Option<GuestCallbackRequest>> {
    let mut enums = ENUMERATIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some((_, entry)) = enums.iter_mut().find(|(id, _)| *id == enumeration_id) else {
        return Ok(None);
    };

    let next_index = entry.index.saturating_add(1);
    let buffer_va = entry.buffer_va;
    let callback_va = entry.callback_va;
    let context = entry.context;
    let flags = entry.flags;
    let Some(class) = entry.items.get(next_index).copied() else {
        // Enumeration complete: drop the state.
        let _ = entry;
        enums.retain(|(id, _)| *id != enumeration_id);
        return Ok(None);
    };
    entry.index = next_index;
    drop(enums);

    // Republish the record in place for the next device before re-entering, so
    // the callback the guest is about to run sees the *next* device.
    write_device_instance(engine, buffer_va, class)?;
    Ok(Some(make_request(buffer_va, callback_va, context, flags)))
}
