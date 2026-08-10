use anyhow::{Context, Result};

use crate::guest_memory::write_u64 as write_guest_u64;
use crate::{HandlerContext, WinApiHandlerResult};

/// `timeSetEvent` handles start here and count up — never 0, the documented
/// failure value.
const TIMER_HANDLE_BASE: u64 = 0x5500_0001;
/// The single fake wave-out handle `waveOutOpen` writes to `*phwo`.
const WAVE_OUT_HANDLE: u64 = 0x5500_0101;

/// `TIMERR_NOERROR` — `timeKillEvent` success.
const TIMERR_NOERROR: u64 = 0;
/// `TIMERR_NOCANDO` — `timeKillEvent` could not find the timer.
const TIMERR_NOCANDO: u64 = 97;
/// `MMSYSERR_NOERROR` — wave-out success.
const MMSYSERR_NOERROR: u64 = 0;
/// `MMSYSERR_INVALPARAM` — a required NULL pointer was passed.
const MMSYSERR_INVALPARAM: u64 = 11;

/// WINMM timer/audio handle tables (`timeSetEvent`, `waveOut*`), owned by
/// this module and heap-allocated on first load via `DllId::Winmm`.
#[derive(Debug, Default)]
pub struct WinmmState {
    /// Next `timeSetEvent` handle (0 = unseeded; the first alloc starts at
    /// [`TIMER_HANDLE_BASE`]).
    next_timer_handle: u64,
    /// Live multimedia timers, keyed by their handle.
    timers: Vec<WinmmTimerRecord>,
}

/// One live `timeSetEvent` registration.
///
/// WIE runs no host timer thread: the callback is stored, not fired. A guest
/// that drives its message loop / Sleep will hit the host frequently, but
/// firing the guest callback from an arbitrary host stop is out of scope for
/// this milestone (documented no-op).
#[allow(dead_code)] // stored for the future callback path; only `handle` is read today
#[derive(Debug)]
struct WinmmTimerRecord {
    handle: u64,
    delay_ms: u32,
    callback_va: u64,
    user_data: u64,
}

#[cfg(test)]
impl WinmmState {
    /// Test-only projection of the live timer table: `(handle, delay_ms,
    /// callback_va, user_data)` per registration.
    pub(crate) fn timer_records(&self) -> Vec<(u64, u32, u64, u64)> {
        self.timers
            .iter()
            .map(|t| (t.handle, t.delay_ms, t.callback_va, t.user_data))
            .collect()
    }
}

/// Dispatch a `WINMM.dll` export by name (case-insensitive) — the string
/// path for APIs beyond the dense `timeGetTime` row.
pub fn dispatch_winmm_extra(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "timesetevent" => Ok(Some(handle_time_set_event(ctx)?)),
        "timekillevent" => Ok(Some(handle_time_kill_event(ctx)?)),
        "waveoutopen" => Ok(Some(handle_wave_out_open(ctx)?)),
        "waveoutclose" => Ok(Some(handle_wave_out_close(ctx)?)),
        "waveoutprepareheader" => Ok(Some(handle_wave_out_prepare_header(ctx)?)),
        "waveoutunprepareheader" => Ok(Some(handle_wave_out_unprepare_header(ctx)?)),
        "waveoutwrite" => Ok(Some(handle_wave_out_write(ctx)?)),
        "waveoutgetnumdevs" => Ok(Some(handle_wave_out_get_num_devs(ctx)?)),
        _ => Ok(None),
    }
}

/// Handles `WINMM.dll!timeSetEvent`.
///
/// Signature: `MMRESULT timeSetEvent(UINT uDelay, UINT uResolution,
/// LPTIMECALLBACK fptc, DWORD_PTR dwUser, UINT fuEvent)`. Allocates a handle
/// from [`TIMER_HANDLE_BASE`] upward in [`WinmmState`], records the
/// registration, and returns the handle (nonzero). No host timer thread is
/// spawned — the callback is documented to fire only when the guest happens
/// to hit a host stop (YAGNI for the milestone).
pub fn handle_time_set_event(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let delay_raw = engine
        .read_rcx()
        .context("failed to read RCX for timeSetEvent")?;
    let _resolution_raw = engine
        .read_rdx()
        .context("failed to read RDX for timeSetEvent")?;
    let callback_va = engine
        .read_r8()
        .context("failed to read R8 for timeSetEvent")?;
    let user_data = engine
        .read_r9()
        .context("failed to read R9 for timeSetEvent")?;

    let delay_ms = u32::try_from(delay_raw).context("timeSetEvent delay does not fit u32")?;

    let winmm = state.winmm();
    if winmm.next_timer_handle == 0 {
        winmm.next_timer_handle = TIMER_HANDLE_BASE;
    }
    let handle = winmm.next_timer_handle;
    winmm.next_timer_handle = winmm.next_timer_handle.saturating_add(1);
    winmm.timers.push(WinmmTimerRecord {
        handle,
        delay_ms,
        callback_va,
        user_data,
    });

    ctx.finish(handle)
}

/// Handles `WINMM.dll!timeKillEvent`.
///
/// Signature: `MMRESULT timeKillEvent(UINT uTimerID)`. Removes the timer from
/// the table; returns `TIMERR_NOERROR` when found, else `TIMERR_NOCANDO`.
pub fn handle_time_kill_event(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let timer_id = engine
        .read_rcx()
        .context("failed to read RCX for timeKillEvent")?;

    let winmm = state.winmm();
    let removed = winmm.timers.iter().any(|timer| timer.handle == timer_id);
    if removed {
        winmm.timers.retain(|timer| timer.handle != timer_id);
    }

    let return_value = if removed {
        TIMERR_NOERROR
    } else {
        TIMERR_NOCANDO
    };

    ctx.finish(return_value)
}

/// Handles `WINMM.dll!waveOutOpen`.
///
/// Signature: `MMRESULT waveOutOpen(LPHWAVEOUT phwo, UINT uDeviceID,
/// LPCWAVEFORMATEX pwfx, DWORD_PTR dwCallback, DWORD_PTR dwInstance,
/// DWORD fdwOpen)`. Writes the fake [`WAVE_OUT_HANDLE`] to `*phwo`; returns
/// `MMSYSERR_NOERROR`, or `MMSYSERR_INVALPARAM` for a NULL `phwo`.
pub fn handle_wave_out_open(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let phwo = engine
        .read_rcx()
        .context("failed to read RCX for waveOutOpen")?;
    let _device_id = engine
        .read_rdx()
        .context("failed to read RDX for waveOutOpen")?;
    let _format_va = engine
        .read_r8()
        .context("failed to read R8 for waveOutOpen")?;
    let _callback = engine
        .read_r9()
        .context("failed to read R9 for waveOutOpen")?;

    if phwo == 0 {
        return ctx.finish(MMSYSERR_INVALPARAM);
    }

    write_guest_u64(engine, phwo, WAVE_OUT_HANDLE).context("failed to write wave-out handle")?;

    ctx.finish(MMSYSERR_NOERROR)
}

/// Handles `WINMM.dll!waveOutClose`.
///
/// The fake device needs no teardown; always `MMSYSERR_NOERROR`.
pub fn handle_wave_out_close(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hwo = engine
        .read_rcx()
        .context("failed to read RCX for waveOutClose")?;

    ctx.finish(MMSYSERR_NOERROR)
}

/// Handles `WINMM.dll!waveOutPrepareHeader`.
///
/// Validates the header pointer (NULL → `MMSYSERR_INVALPARAM`) and otherwise
/// acknowledges the header without inspecting it.
pub fn handle_wave_out_prepare_header(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hwo = engine
        .read_rcx()
        .context("failed to read RCX for waveOutPrepareHeader")?;
    let header_va = engine
        .read_rdx()
        .context("failed to read RDX for waveOutPrepareHeader")?;
    let _header_len = engine
        .read_r8()
        .context("failed to read R8 for waveOutPrepareHeader")?;

    let return_value = if header_va == 0 {
        MMSYSERR_INVALPARAM
    } else {
        MMSYSERR_NOERROR
    };

    ctx.finish(return_value)
}

/// Handles `WINMM.dll!waveOutUnprepareHeader`.
///
/// Nothing was prepared; always `MMSYSERR_NOERROR`.
pub fn handle_wave_out_unprepare_header(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hwo = engine
        .read_rcx()
        .context("failed to read RCX for waveOutUnprepareHeader")?;
    let _header_va = engine
        .read_rdx()
        .context("failed to read RDX for waveOutUnprepareHeader")?;
    let _header_len = engine
        .read_r8()
        .context("failed to read R8 for waveOutUnprepareHeader")?;

    ctx.finish(MMSYSERR_NOERROR)
}

/// Handles `WINMM.dll!waveOutWrite`.
///
/// Documented no-op: the fake device plays nothing, but the buffer is
/// acknowledged so the guest's playback pipeline proceeds.
pub fn handle_wave_out_write(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hwo = engine
        .read_rcx()
        .context("failed to read RCX for waveOutWrite")?;
    let _header_va = engine
        .read_rdx()
        .context("failed to read RDX for waveOutWrite")?;
    let _header_len = engine
        .read_r8()
        .context("failed to read R8 for waveOutWrite")?;

    ctx.finish(MMSYSERR_NOERROR)
}

/// Handles `WINMM.dll!waveOutGetNumDevs`.
///
/// One fake device exists so `waveOutOpen(WAVE_MAPPER, …)` succeeds.
pub fn handle_wave_out_get_num_devs(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ctx.finish(1)
}

/// Handles `WINMM.dll!timeGetTime`.
///
/// B5: returns the same ms-since-session-epoch value the in-guest stub reads
/// from the host-written guest clock table (slot 2), so the host fallback and
/// the stub can never disagree about how much time has passed.
pub fn handle_time_get_time(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let return_value = crate::kernel32::clock::tick_count_32();

    ctx.finish(return_value)
}
