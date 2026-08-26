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

/// `TIME_ONESHOT` — `timeSetEvent` fires once.
const TIME_ONESHOT: u32 = 0x0000;
/// `TIME_PERIODIC` — `timeSetEvent` fires repeatedly.
const TIME_PERIODIC: u32 = 0x0001;

/// A due multimedia timer ready to fire.
///
/// Returned by [`WinmmState::pop_due_timers`]; the pump turns each into a
/// [`crate::GuestCallbackRequest`] for the guest `LPTIMECALLBACK`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueTimer {
    /// Timer handle (`uTimerID`).
    pub handle: u64,
    /// Guest callback VA (`fptc`).
    pub callback_va: u64,
    /// User data (`dwUser`).
    pub user_data: u64,
}

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
/// Each timer stores its absolute `due_tick_ms` (wrapping `u32` ms since
/// session epoch) and whether it is periodic. The pump polls
/// [`WinmmState::pop_due_timers`] at safe boundaries (host stops and the
/// `WaitingForMessage` idle point) and dispatches due callbacks as guest
/// `LPTIMECALLBACK` invocations. While a timer callback runs, remaining due
/// timers wait for the next boundary (reentrancy guard in the pump checks
/// `pending_callbacks`); a callback that itself calls `timeSetEvent` naturally
/// enqueues a future record.
///
/// `due_tick_ms` uses wrapping `u32` addition so the 49.7-day `GetTickCount`
/// wrap is transparent (`wrapping_add` / `wrapping_sub` with the 0x8000_0000
/// half-range test).
#[derive(Debug)]
struct WinmmTimerRecord {
    handle: u64,
    delay_ms: u32,
    callback_va: u64,
    user_data: u64,
    due_tick_ms: u32,
    periodic: bool,
}

impl WinmmState {
    /// Return and remove due timers for `now_tick`.
    ///
    /// One-shot timers are removed; periodic timers are re-armed
    /// (`due_tick_ms += delay_ms`) and remain live. The caller should dispatch
    /// at most one callback per boundary while `pending_callbacks` is non-empty
    /// (see pump reentrancy rule) — remaining due timers will be returned on
    /// the next poll.
    pub fn pop_due_timers(&mut self, now_tick: u32) -> Vec<DueTimer> {
        let mut due = Vec::new();
        let mut index = 0;
        while index < self.timers.len() {
            let is_due = {
                let rec = match self.timers.get(index) {
                    Some(r) => r,
                    None => break,
                };
                is_due_tick(now_tick, rec.due_tick_ms)
            };
            if is_due {
                let periodic = self.timers.get(index).is_some_and(|r| r.periodic);
                let delay = self.timers.get(index).map_or(0, |r| r.delay_ms);
                let handle = self.timers.get(index).map_or(0, |r| r.handle);
                let callback_va = self.timers.get(index).map_or(0, |r| r.callback_va);
                let user_data = self.timers.get(index).map_or(0, |r| r.user_data);
                due.push(DueTimer {
                    handle,
                    callback_va,
                    user_data,
                });
                if periodic {
                    if let Some(rec) = self.timers.get_mut(index) {
                        rec.due_tick_ms = rec.due_tick_ms.wrapping_add(delay);
                    }
                    index = index.saturating_add(1);
                } else if index < self.timers.len() {
                    self.timers.remove(index);
                } else {
                    break;
                }
            } else {
                index = index.saturating_add(1);
            }
        }
        due
    }

    /// Pop the next due timer, if any, for `now_tick`.
    ///
    /// Like [`Self::pop_due_timers`] but drains at most one entry — the
    /// pump's per-quantum dispatch uses this to honour the reentrancy rule
    /// (one callback per boundary).
    pub fn pop_next_due_timer(&mut self, now_tick: u32) -> Option<DueTimer> {
        let mut due_index: Option<usize> = None;
        for (idx, rec) in self.timers.iter().enumerate() {
            if is_due_tick(now_tick, rec.due_tick_ms) {
                due_index = Some(idx);
                break;
            }
        }
        let idx = due_index?;
        let rec = self.timers.get(idx)?.clone();
        // Clone fields before mutation.
        let due = DueTimer {
            handle: rec.handle,
            callback_va: rec.callback_va,
            user_data: rec.user_data,
        };
        if rec.periodic {
            if let Some(slot) = self.timers.get_mut(idx) {
                slot.due_tick_ms = slot.due_tick_ms.wrapping_add(rec.delay_ms);
            }
        } else if idx < self.timers.len() {
            self.timers.remove(idx);
        }
        Some(due)
    }
}

impl Clone for WinmmTimerRecord {
    fn clone(&self) -> Self {
        Self {
            handle: self.handle,
            delay_ms: self.delay_ms,
            callback_va: self.callback_va,
            user_data: self.user_data,
            due_tick_ms: self.due_tick_ms,
            periodic: self.periodic,
        }
    }
}

/// Whether `due` is in the past relative to `now` in wrapping `u32` time.
///
/// The half-range test matches Windows `GetTickCount` wrap handling: a due
/// time is considered reached when `now - due < 0x8000_0000`.
fn is_due_tick(now: u32, due: u32) -> bool {
    now.wrapping_sub(due) < 0x8000_0000
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

    /// Test-only projection including `due_tick_ms` and `periodic`.
    #[cfg(test)]
    pub(crate) fn timer_records_full(&self) -> Vec<(u64, u32, u64, u64, u32, bool)> {
        self.timers
            .iter()
            .map(|t| {
                (
                    t.handle,
                    t.delay_ms,
                    t.callback_va,
                    t.user_data,
                    t.due_tick_ms,
                    t.periodic,
                )
            })
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
        "waveoutgetdevcapsw" => Ok(Some(handle_wave_out_get_dev_caps_w(ctx)?)),
        "waveoutgeterrortextw" => Ok(Some(handle_wave_out_get_error_text_w(ctx)?)),
        "waveoutreset" => Ok(Some(handle_wave_out_reset(ctx)?)),
        "waveingetnumdevs" => Ok(Some(handle_wave_in_get_num_devs(ctx)?)),
        "waveinopen"
        | "waveinclose"
        | "waveinaddbuffer"
        | "waveinprepareheader"
        | "waveinunprepareheader"
        | "waveinstart"
        | "waveinreset" => Ok(Some(handle_wave_in_no_driver(ctx)?)),
        "waveingetdevcapsw" => Ok(Some(handle_wave_in_no_driver(ctx)?)),
        "midioutgetdevcapsa"
        | "midioutprepareheader"
        | "midioutreset"
        | "midioutsetvolume"
        | "midioutshortmsg"
        | "midioutunprepareheader" => Ok(Some(handle_midi_no_driver(ctx)?)),
        "midioutgeterrortexta" => Ok(Some(handle_midi_out_get_error_text_a(ctx)?)),
        "midistreamopen" | "midistreamclose" | "midistreamout" | "midistreampause"
        | "midistreamproperty" | "midistreamrestart" | "midistreamstop" => {
            Ok(Some(handle_midi_no_driver(ctx)?))
        }
        "timebeginperiod" | "timeendperiod" => Ok(Some(handle_time_begin_end_period(ctx)?)),
        _ => Ok(None),
    }
}

/// Handles `WINMM.dll!timeSetEvent`.
///
/// Signature: `MMRESULT timeSetEvent(UINT uDelay, UINT uResolution,
/// LPTIMECALLBACK fptc, DWORD_PTR dwUser, UINT fuEvent)`. Allocates a handle
/// from [`TIMER_HANDLE_BASE`] upward in [`WinmmState`], records the
/// registration with `due_tick_ms = now.wrapping_add(delay)` and `periodic`
/// from `TIME_PERIODIC` in `fuEvent` (the 5th Win64 stack arg at
/// `[rsp+0x28]`), and returns the handle (nonzero). The host has no timer
/// thread — the pump polls `pop_due_timers(now)` at safe boundaries and
/// dispatches the guest `LPTIMECALLBACK` (`uTimerID, uMsg=0, dwUser, 0, 0`).
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

    // 5th Win64 arg `fuEvent` at [rsp+0x28] (like PeekMessage's wRemoveMsg).
    let flags = engine
        .read_rsp()
        .ok()
        .and_then(|rsp| crate::guest_memory::read_u32(engine, rsp.wrapping_add(0x28)).ok())
        .unwrap_or(TIME_ONESHOT);
    let periodic = (flags & TIME_PERIODIC) != 0;

    let now_raw = crate::kernel32::clock::tick_count_32();
    let now = u32::try_from(now_raw & u64::from(u32::MAX)).unwrap_or(0);
    let due_tick_ms = now.wrapping_add(delay_ms);

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
        due_tick_ms,
        periodic,
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

/// Acknowledge a wave-out call whose `n_args` register arguments are all
/// ignored, returning `MMSYSERR_NOERROR`.
fn ack_no_error(ctx: &mut HandlerContext<'_>, n_args: u32) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    match n_args {
        1 => {
            let _arg0 = engine.read_rcx()?;
        }
        3 => {
            let _arg0 = engine.read_rcx()?;
            let _arg1 = engine.read_rdx()?;
            let _arg2 = engine.read_r8()?;
        }
        _ => {}
    }
    ctx.finish(MMSYSERR_NOERROR)
}

/// Handles `WINMM.dll!waveOutClose`.
///
/// The fake device needs no teardown; always `MMSYSERR_NOERROR`.
pub fn handle_wave_out_close(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ack_no_error(ctx, 1)
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
    ack_no_error(ctx, 3)
}

/// Handles `WINMM.dll!waveOutWrite`.
///
/// Documented no-op: the fake device plays nothing, but the buffer is
/// acknowledged so the guest's playback pipeline proceeds.
pub fn handle_wave_out_write(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ack_no_error(ctx, 3)
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

/// `MMSYSERR_NODRIVER` — no MIDI / capture device driver is present.
const MMSYSERR_NODRIVER: u64 = 97;

/// Handles `WINMM.dll!waveOutGetDevCapsW` — the single fake wave-out device's
/// capabilities (kept consistent with `waveOutGetNumDevs` = 1).
pub fn handle_wave_out_get_dev_caps_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _device_id = engine
        .read_rcx()
        .context("failed to read RCX for waveOutGetDevCapsW")?;
    let caps_va = engine
        .read_rdx()
        .context("failed to read RDX for waveOutGetDevCapsW")?;
    let _caps_size = engine
        .read_r8()
        .context("failed to read R8 for waveOutGetDevCapsW")?;
    if caps_va == 0 {
        return ctx.finish(MMSYSERR_INVALPARAM);
    }
    // WAVEOUTCAPSW (84 bytes): wMid@0, wPid@2, vDriverVersion@4, dwFormats@8,
    // wChannels@12, wReserved1@14, dwSupport@16, wszPname[32]@20.
    let mut buf = [0_u8; 84];
    buf[8..12].copy_from_slice(&0xFFFF_u32.to_le_bytes()); // all WAVE_FORMAT_* bits
    buf[12..14].copy_from_slice(&2_u16.to_le_bytes()); // stereo
    let name = "WIE Virtual Audio";
    for (i, b) in name.encode_utf16().enumerate() {
        let off = 20 + i * 2;
        if off + 2 <= buf.len() {
            buf[off..off + 2].copy_from_slice(&b.to_le_bytes());
        }
    }
    engine
        .mem_write(caps_va, &buf)
        .context("failed to write WAVEOUTCAPSW")?;
    ctx.finish(MMSYSERR_NOERROR)
}

/// Shared no-driver tail: the register arguments are consumed and ignored,
/// then `MMSYSERR_NODRIVER` reports the missing device.
fn no_driver(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _arg0 = engine.read_rcx()?;
    let _arg1 = engine.read_rdx()?;
    let _arg2 = engine.read_r8()?;
    let _arg3 = engine.read_r9()?;
    ctx.finish(MMSYSERR_NODRIVER)
}

/// Handles the `WINMM.dll!waveIn*` family — no capture device driver, so
/// every operation fails with `MMSYSERR_NODRIVER`.
pub fn handle_wave_in_no_driver(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    no_driver(ctx)
}

/// Handles the `WINMM.dll!midiOut*` / `midiStream*` families — no MIDI device
/// driver, so every operation fails with `MMSYSERR_NODRIVER`.
pub fn handle_midi_no_driver(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    no_driver(ctx)
}

/// The error text for the common `MMSYSERR_*` codes.
fn mmsys_error_text(error_code: u64) -> &'static str {
    match error_code {
        MMSYSERR_NOERROR => "No error.",
        MMSYSERR_INVALPARAM => "Invalid parameter.",
        MMSYSERR_NODRIVER => "No device driver is present.",
        _ => "Unknown error.",
    }
}

/// Shared `waveOutGetErrorTextW` / `midiOutGetErrorTextA` body: look up the
/// error text and write it with `write` (UTF-16 vs ANSI).
fn get_error_text(
    ctx: &mut HandlerContext<'_>,
    api: &str,
    write: fn(&mut dyn wie_cpu::CpuEngine, u64, usize, &str) -> anyhow::Result<usize>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let error_code = engine.read_rcx()?;
    let text_va = engine.read_rdx()?;
    let text_len = engine.read_r8()?;
    if text_va != 0 {
        write(
            engine,
            text_va,
            usize::try_from(text_len).unwrap_or(0),
            mmsys_error_text(error_code),
        )
        .with_context(|| format!("failed to write {api} buffer"))?;
    }
    ctx.finish(MMSYSERR_NOERROR)
}

/// Handles `WINMM.dll!waveOutGetErrorTextW` — writes the error text for the
/// common codes into the guest buffer.
pub fn handle_wave_out_get_error_text_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    get_error_text(
        ctx,
        "waveOutGetErrorTextW",
        crate::guest_string::write_utf16_c_string,
    )
}

/// Handles `WINMM.dll!midiOutGetErrorTextA` — writes the error text for the
/// common codes into the guest ANSI buffer.
pub fn handle_midi_out_get_error_text_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    get_error_text(
        ctx,
        "midiOutGetErrorTextA",
        crate::guest_string::write_ansi_c_string,
    )
}

/// Handles `WINMM.dll!waveOutReset` — the fake device plays nothing, so the
/// reset is a no-op like `waveOutWrite`.
pub fn handle_wave_out_reset(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _hwo = ctx.engine.read_rcx()?;
    ctx.finish(MMSYSERR_NOERROR)
}

/// Handles `WINMM.dll!waveInGetNumDevs` — no capture devices.
pub fn handle_wave_in_get_num_devs(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ctx.finish(0)
}

/// Handles `WINMM.dll!timeBeginPeriod` / `timeEndPeriod` — accepted no-ops
/// returning `TIMERR_NOERROR` (host timers are not quantum-constrained).
pub fn handle_time_begin_end_period(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _period = ctx.engine.read_rcx()?;
    ctx.finish(TIMERR_NOERROR)
}
