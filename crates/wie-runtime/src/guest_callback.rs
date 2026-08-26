//! Shared guest WndProc enter/leave helpers (session + dual-trace).

use anyhow::{Context, Result};
use wie_cpu::CpuEngine;
use wie_winapi::{GuestCallbackRequest, OuterReturn};

/// Set up Win64 frame + args and transfer control to a guest WndProc.
///
/// Returns the outer API `RSP` (to restore on trampoline return).
///
/// Stack below the original host-API frame:
/// ```text
/// [dispatch_rsp]      return address of outer API caller
/// [dispatch_rsp-8]    alignment padding
/// [dispatch_rsp-0x28] 32-byte shadow space
/// [dispatch_rsp-0x30] trampoline return address  ← new RSP / WndProc entry
/// ```
pub(crate) fn install_guest_callback_frame(
    engine: &mut dyn CpuEngine,
    request: &GuestCallbackRequest,
    trampoline: u64,
) -> Result<u64> {
    let dispatch_rsp = engine
        .read_rsp()
        .context("failed to read RSP before guest callback")?;
    write_callback_frame(
        engine,
        trampoline,
        dispatch_rsp,
        [
            request.window_handle,
            u64::from(request.message),
            request.word_parameter,
            request.long_parameter,
        ],
        request.callback_address,
    )?;
    Ok(dispatch_rsp)
}

/// Set up a Win64 frame + args for a guest `FONTENUMPROC` callback.
///
/// The enumeration callback ABI is `(lpelfe, lpntme, FontType, lParam)` in
/// RCX/RDX/R8/R9 — the `lpntme` pointer must cross RDX in full 64 bits, which
/// the WndProc bridge (RDX = 32-bit message) cannot. The request packs the
/// args as `window_handle = lpelfe`, `word_parameter = lpntme`, `message =
/// FontType`, `long_parameter = lParam`.
///
/// `dispatch_rsp` is the RSP the frame is installed relative to. For the first
/// callback it is the current RSP; for a continuation it is the ORIGINAL outer
/// API RSP (so the final completion restores the true outer frame even though
/// each re-entry grows the stack down by 0x30).
pub(crate) fn install_guest_enum_callback_frame(
    engine: &mut dyn CpuEngine,
    request: &GuestCallbackRequest,
    trampoline: u64,
    dispatch_rsp: u64,
) -> Result<()> {
    write_callback_frame(
        engine,
        trampoline,
        dispatch_rsp,
        [
            request.window_handle,
            request.word_parameter,
            u64::from(request.message),
            request.long_parameter,
        ],
        request.callback_address,
    )
}

/// Shared Win64 callback-frame writer: write the trampoline return address,
/// clear the shadow space, set RSP/args/RIP.
///
/// The extra 8 bytes at `[rsp+0x28]` (the 5th Win64 stack arg) are also zeroed
/// so a `LPTIMECALLBACK` (`uTimerID, uMsg, dwUser, dw1, dw2`) sees `dw2 == 0`
/// without a dedicated timer frame. The slot is padding for 4-arg WndProcs and
/// harmless to zero there.
fn write_callback_frame(
    engine: &mut dyn CpuEngine,
    trampoline: u64,
    dispatch_rsp: u64,
    args: [u64; 4],
    callback_address: u64,
) -> Result<()> {
    // 0x30 keeps WndProc entry RSP ≡ 8 (mod 16) when the outer API entry
    // was itself 8-aligned, matching the Win64 ABI.
    let frame_rsp = dispatch_rsp
        .checked_sub(0x30)
        .context("guest callback stack frame underflow")?;

    engine
        .mem_write(frame_rsp, &trampoline.to_le_bytes())
        .context("failed to write guest callback trampoline return address")?;

    let shadow_address = frame_rsp
        .checked_add(8)
        .context("guest callback shadow space address overflow")?;

    engine
        .mem_write(shadow_address, &[0_u8; 0x20])
        .context("failed to clear guest callback shadow space")?;

    // 5th stack arg slot for timer callbacks (`dw2`): zeroed for all callbacks
    // (padding for WndProc, required zero for `timeSetEvent`).
    let stack_arg_addr = frame_rsp
        .checked_add(0x28)
        .context("guest callback stack arg address overflow")?;
    engine
        .mem_write(stack_arg_addr, &0_u64.to_le_bytes())
        .context("failed to clear guest callback stack arg")?;

    engine
        .write_rsp(frame_rsp)
        .context("failed to set RSP for guest callback")?;
    engine
        .write_rcx(args[0])
        .context("failed to set RCX for guest callback")?;
    engine
        .write_rdx(args[1])
        .context("failed to set RDX for guest callback")?;
    engine
        .write_r8(args[2])
        .context("failed to set R8 for guest callback")?;
    engine
        .write_r9(args[3])
        .context("failed to set R9 for guest callback")?;
    engine
        .write_rip(callback_address)
        .context("failed to set RIP for guest callback")?;

    Ok(())
}

/// CreateWindowEx returns the HWND unless WM_CREATE returned -1.
#[must_use]
pub(crate) fn create_window_return_value(lresult: u64, outer_return: OuterReturn) -> u64 {
    match outer_return {
        OuterReturn::CreateWindow(hwnd) => {
            let low = u32::try_from(lresult & 0xffff_ffff).unwrap_or(0);
            let create_status = i32::from_ne_bytes(low.to_ne_bytes());
            if create_status == -1 { 0 } else { hwnd }
        }
        OuterReturn::Fixed(value) => value,
        OuterReturn::Passthrough => lresult,
    }
}

/// Restore outer API frame and return from the host API after WndProc finishes.
///
/// Returns `(return_value, return_address)`.
pub(crate) fn finish_guest_callback(
    engine: &mut dyn CpuEngine,
    dispatch_rsp: u64,
    outer_return: OuterReturn,
) -> Result<(u64, u64)> {
    let lresult = engine
        .read_rax()
        .context("failed to read LRESULT from guest callback")?;
    let return_value = create_window_return_value(lresult, outer_return);

    engine
        .write_rsp(dispatch_rsp)
        .context("failed to restore RSP for outer API completion")?;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from outer API after guest callback")?;

    Ok((return_value, return_address))
}
