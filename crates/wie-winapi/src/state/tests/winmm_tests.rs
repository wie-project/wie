//! WINMM (Windows Multimedia) handler tests: the `timeGetTime` / `timeSetEvent`
//! / `timeKillEvent` timer surface and the fake wave-out device. All handlers
//! run through `crate::winmm::dispatch_winmm_extra` except the dense
//! `timeGetTime` row, which is called directly.
use super::*;

/// Dispatch one WINMM API through the extra-dispatch table and return the
/// handler's return value (the guest RAX after the Win64 return).
///
/// `Ok(None)` from the dispatcher would mean the API name is unknown — a test
/// bug — so the helper fails loudly on it.
fn dispatch_winmm(engine: &mut IcedCpu, state: &mut WinApiState, name: &str) -> u64 {
    let mut ctx = HandlerContext::new(engine, default_env(), state);
    crate::winmm::dispatch_winmm_extra(&mut ctx, name)
        .expect("dispatch must succeed")
        .expect("handled")
        .return_value
}

/// Direct call to the dense `WinApiId::WinmmTimegettime` handler.
fn time_get_time(engine: &mut IcedCpu, state: &mut WinApiState) -> u64 {
    crate::winmm::handle_time_get_time(&mut HandlerContext::new(engine, default_env(), state))
        .expect("timeGetTime")
        .return_value
}

#[test]
fn test_time_get_time_monotonic_sanity() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let first = time_get_time(&mut engine, &mut state);
    // The session epoch is captured on first access, so the first read can be
    // 0 ms; a short sleep guarantees the second read is past zero.
    std::thread::sleep(std::time::Duration::from_millis(5));
    let second = time_get_time(&mut engine, &mut state);
    assert!(
        second >= first,
        "timeGetTime must never go backwards: {second} < {first}"
    );
    assert!(second > 0, "after a sleep the counter must be past zero");
    assert!(
        first <= u64::from(u32::MAX) && second <= u64::from(u32::MAX),
        "timeGetTime stays inside the 32-bit ms range"
    );
}

#[test]
fn test_wave_out_get_num_devs_one() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    assert_eq!(
        dispatch_winmm(&mut engine, &mut state, "waveOutGetNumDevs"),
        1
    );
}

#[test]
fn test_time_set_event_allocates_handles() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // Register order: RCX=delay, R8=callback, R9=user_data (RDX=resolution).
    write_regs(&mut engine, 10, 1, 0x1234, 0x5678, 0);
    let first = dispatch_winmm(&mut engine, &mut state, "timeSetEvent");
    let second = dispatch_winmm(&mut engine, &mut state, "timeSetEvent");
    assert!(
        first >= 0x5500_0001,
        "handles start at TIMER_HANDLE_BASE, got {first:#x}"
    );
    assert!(
        second > first,
        "each call allocates a distinct, increasing handle"
    );
}

#[test]
fn test_time_set_event_stores_state() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 10, 1, 0x1234, 0x5678, 0);
    let handle = dispatch_winmm(&mut engine, &mut state, "timeSetEvent");
    let records = state.winmm().timer_records();
    assert_eq!(records.len(), 1, "one timer recorded");
    assert_eq!(
        records.first().copied(),
        Some((handle, 10, 0x1234, 0x5678)),
        "the record captures delay, callback VA, and user data"
    );
}

#[test]
fn test_time_kill_event_success() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 10, 1, 0x1234, 0x5678, 0);
    let handle = dispatch_winmm(&mut engine, &mut state, "timeSetEvent");
    write_regs(&mut engine, handle, 0, 0, 0, 0);
    let result = dispatch_winmm(&mut engine, &mut state, "timeKillEvent");
    assert_eq!(result, 0, "TIMERR_NOERROR");
    assert_eq!(state.winmm().timer_records().len(), 0, "timer removed");
}

#[test]
fn test_time_kill_event_unknown() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x1234, 0, 0, 0, 0);
    let result = dispatch_winmm(&mut engine, &mut state, "timeKillEvent");
    assert_eq!(result, 97, "TIMERR_NOCANDO");
    assert_eq!(state.winmm().timer_records().len(), 0);
}

#[test]
fn test_time_set_event_kill_round_trip() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 25, 1, 0x9999, 0xaaaa, 0);
    let handle = dispatch_winmm(&mut engine, &mut state, "timeSetEvent");
    assert!(handle >= 0x5500_0001);
    write_regs(&mut engine, handle, 0, 0, 0, 0);
    assert_eq!(dispatch_winmm(&mut engine, &mut state, "timeKillEvent"), 0);
    // Killing the same id again reports it is gone.
    write_regs(&mut engine, handle, 0, 0, 0, 0);
    assert_eq!(dispatch_winmm(&mut engine, &mut state, "timeKillEvent"), 97);
    assert_eq!(state.winmm().timer_records().len(), 0);
}

#[test]
fn test_wave_out_open_null_phwo_invalparam() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    let result = dispatch_winmm(&mut engine, &mut state, "waveOutOpen");
    assert_eq!(result, 11, "MMSYSERR_INVALPARAM");
}

#[test]
fn test_wave_out_open_valid_writes_handle() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let phwo = 0x5000_u64;
    write_regs(&mut engine, phwo, 0, 0, 0, 0);
    let result = dispatch_winmm(&mut engine, &mut state, "waveOutOpen");
    assert_eq!(result, 0, "MMSYSERR_NOERROR");
    let mut bytes = [0_u8; 8];
    engine.mem_read(phwo, &mut bytes).expect("read phwo");
    assert_eq!(
        u64::from_le_bytes(bytes),
        0x5500_0101,
        "WAVE_OUT_HANDLE written to *phwo"
    );
}

#[test]
fn test_wave_out_open_close_round_trip() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let phwo = 0x5000_u64;
    write_regs(&mut engine, phwo, 0, 0, 0, 0);
    assert_eq!(dispatch_winmm(&mut engine, &mut state, "waveOutOpen"), 0);
    // waveOutClose ignores its handle argument and always succeeds.
    write_regs(&mut engine, 0x5500_0101, 0, 0, 0, 0);
    assert_eq!(dispatch_winmm(&mut engine, &mut state, "waveOutClose"), 0);
}

#[test]
fn test_wave_out_prepare_header_null_invalparam() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // hwo in RCX, NULL header pointer in RDX.
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    let result = dispatch_winmm(&mut engine, &mut state, "waveOutPrepareHeader");
    assert_eq!(result, 11, "MMSYSERR_INVALPARAM");
}

#[test]
fn test_wave_out_prepare_header_valid_noerror() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // hwo in RCX, a valid header VA in RDX.
    write_regs(&mut engine, 1, 0x5000, 0, 0, 0);
    let result = dispatch_winmm(&mut engine, &mut state, "waveOutPrepareHeader");
    assert_eq!(result, 0, "MMSYSERR_NOERROR");
}

/// Write the 5th Win64 stack arg (`fuEvent`) for `timeSetEvent` at
/// `[rsp+0x28]`, like `PeekMessage`'s `wRemoveMsg`.
fn set_time_set_event_flags(engine: &mut IcedCpu, flags: u32) {
    let rsp = engine.read_rsp().expect("rsp for flags");
    engine
        .mem_write(rsp.wrapping_add(0x28), &flags.to_le_bytes())
        .expect("write fuEvent flags");
}

#[test]
fn test_pop_due_one_shot_zero_delay() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    set_time_set_event_flags(&mut engine, 0); // TIME_ONESHOT
    write_regs(&mut engine, 0, 1, 0x1234, 0x5678, 0);
    let handle = dispatch_winmm(&mut engine, &mut state, "timeSetEvent");
    let due = state
        .winmm()
        .timer_records_full()
        .first()
        .map_or(0, |r| r.4);
    let popped = state.pop_due_timers(due);
    assert_eq!(
        popped.len(),
        1,
        "delay 0 one-shot must be due at its due tick"
    );
    assert_eq!(popped.first().map(|d| d.handle), Some(handle));
    assert_eq!(popped.first().map(|d| d.callback_va), Some(0x1234));
    assert_eq!(popped.first().map(|d| d.user_data), Some(0x5678));
    assert_eq!(
        state.winmm().timer_records().len(),
        0,
        "one-shot must be removed after pop"
    );
    // Second pop at same tick finds nothing.
    assert_eq!(state.pop_due_timers(due).len(), 0);
}

#[test]
fn test_pop_due_periodic_rearms_and_kill() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let delay = 10_u32;
    set_time_set_event_flags(&mut engine, 0x0001); // TIME_PERIODIC
    write_regs(&mut engine, u64::from(delay), 1, 0x1234, 0x5678, 0);
    let handle = dispatch_winmm(&mut engine, &mut state, "timeSetEvent");
    let due = state
        .winmm()
        .timer_records_full()
        .first()
        .map_or(0, |r| r.4);
    // First fire at due.
    let first = state.pop_due_timers(due);
    assert_eq!(first.len(), 1);
    assert_eq!(first.first().map(|d| d.handle), Some(handle));
    // Re-armed: due += delay, still present.
    let rearmed_due = state
        .winmm()
        .timer_records_full()
        .first()
        .map_or(0, |r| r.4);
    assert_eq!(rearmed_due, due.wrapping_add(delay));
    assert_eq!(
        state.winmm().timer_records_full().first().map(|r| r.5),
        Some(true),
        "periodic flag must persist"
    );
    // Not yet due at old due.
    assert_eq!(state.pop_due_timers(due).len(), 0);
    // Due again at new due.
    let second = state.pop_due_timers(rearmed_due);
    assert_eq!(second.len(), 1);
    assert_eq!(second.first().map(|d| d.handle), Some(handle));
    // Kill stops periodic.
    write_regs(&mut engine, handle, 0, 0, 0, 0);
    assert_eq!(dispatch_winmm(&mut engine, &mut state, "timeKillEvent"), 0);
    assert_eq!(state.winmm().timer_records().len(), 0);
    assert_eq!(
        state.pop_due_timers(rearmed_due.wrapping_add(delay)).len(),
        0,
        "killed periodic must not fire again"
    );
}

#[test]
fn test_pop_due_not_due_stays() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    set_time_set_event_flags(&mut engine, 0); // one-shot
    write_regs(&mut engine, 1000, 1, 0x1234, 0x5678, 0);
    dispatch_winmm(&mut engine, &mut state, "timeSetEvent");
    let due = state
        .winmm()
        .timer_records_full()
        .first()
        .map_or(0, |r| r.4);
    let not_due = due.wrapping_sub(1);
    let popped = state.pop_due_timers(not_due);
    assert_eq!(popped.len(), 0, "timer must not fire before due");
    assert_eq!(state.winmm().timer_records().len(), 1, "timer stays put");
}

#[test]
fn test_pop_next_due_timer_singular() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    set_time_set_event_flags(&mut engine, 0);
    write_regs(&mut engine, 5, 1, 0x1111, 0x2222, 0);
    let handle = dispatch_winmm(&mut engine, &mut state, "timeSetEvent");
    let due = state
        .winmm()
        .timer_records_full()
        .first()
        .map_or(0, |r| r.4);
    let one = state.pop_next_due_timer(due);
    assert_eq!(one.as_ref().map(|d| d.handle), Some(handle));
    assert_eq!(state.pop_next_due_timer(due), None);
}

/// Wave 5 slice 1: `waveOutWrite` with a CALLBACK_FUNCTION device sinks the
/// buffer's PCM, marks the header `WHDR_DONE` (polling contract), and queues
/// a `WOM_DONE` completion due at `now + buffer_duration_ms` through the
/// shared timer table.
#[test]
fn test_wave_out_write_sinks_pcm_and_queues_wom_done() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();

    // waveOutOpen(phwo=0x5000, format @ 0x2000, callback @ 0xBEEF_0000,
    // fdwOpen = CALLBACK_FUNCTION (0x30000), dwInstance = 0x7A).
    let phwo = 0x5000_u64;
    let format_va = 0x2000_u64;
    let callback_va = 0xBEEF_0000_u64;
    // WAVEFORMATEX: PCM, stereo, 22050 Hz, 4 bytes/frame, 16-bit.
    let mut fmt = [0_u8; 18];
    fmt[0..2].copy_from_slice(&1_u16.to_le_bytes()); // wFormatTag = PCM
    fmt[2..4].copy_from_slice(&2_u16.to_le_bytes()); // nChannels
    fmt[4..8].copy_from_slice(&22_050_u32.to_le_bytes()); // nSamplesPerSec
    fmt[8..12].copy_from_slice(&88_200_u32.to_le_bytes()); // nAvgBytesPerSec
    fmt[12..14].copy_from_slice(&4_u16.to_le_bytes()); // nBlockAlign
    fmt[14..16].copy_from_slice(&16_u16.to_le_bytes()); // wBitsPerSample
    engine.mem_write(format_va, &fmt).expect("write format");
    let flags = 0x0003_0000_u64; // CALLBACK_FUNCTION
    engine
        .mem_write(STACK_TOP - 0x28, &flags.to_le_bytes())
        .expect("write fdwOpen slot");
    engine
        .mem_write(STACK_TOP - 0x30, &0x7A_u64.to_le_bytes())
        .expect("write dwInstance slot");
    write_regs(&mut engine, phwo, 0, format_va, callback_va, 0);
    assert_eq!(dispatch_winmm(&mut engine, &mut state, "waveOutOpen"), 0);

    // WAVEHDR @ 0x2100: lpData=0x3000, dwBufferLength=8820 (200ms at
    // 22050 Hz stereo 16-bit), dwFlags=WHDR_PREPARED|WHDR_INQUEUE.
    let header_va = 0x2100_u64;
    let data_va = 0x3000_u64;
    let pcm: Vec<u8> = (0..8820).map(|i| (i & 0xFF) as u8).collect();
    engine.mem_write(data_va, &pcm).expect("write pcm");
    let mut hdr = [0_u8; 48];
    hdr[0..8].copy_from_slice(&data_va.to_le_bytes());
    hdr[8..12].copy_from_slice(&8820_u32.to_le_bytes());
    hdr[24..28].copy_from_slice(&0x12_u32.to_le_bytes()); // PREPARED|INQUEUE
    engine.mem_write(header_va, &hdr).expect("write header");

    write_regs(&mut engine, 0x5500_0101, header_va, 0, 0, 0);
    assert_eq!(dispatch_winmm(&mut engine, &mut state, "waveOutWrite"), 0);

    // 1. The PCM landed in the playback sink.
    assert_eq!(
        state.winmm().playback_sink(),
        pcm.as_slice(),
        "the submitted buffer must reach the playback sink"
    );
    // 2. The header is DONE and no longer INQUEUE.
    let mut flags_out = [0_u8; 4];
    engine
        .mem_read(header_va + 24, &mut flags_out)
        .expect("read dwFlags");
    let out_flags = u32::from_le_bytes(flags_out);
    assert_ne!(out_flags & 0x1, 0, "WHDR_DONE must be set");
    assert_eq!(
        out_flags & 0x10,
        0,
        "WHDR_INQUEUE must be cleared after write"
    );
    // 3. A WOM_DONE completion is queued (~200 ms out) for the function
    // callback, carrying the wave-out handle.
    let now =
        u32::try_from(crate::kernel32::clock::tick_count_32() & u64::from(u32::MAX)).unwrap_or(0);
    // The completion is NOT due yet: the buffer is ~200 ms of audio
    // (allow slack for clock sampling between the write and this read).
    assert!(
        state.pop_next_due_timer(now).is_none(),
        "a ~200 ms buffer must not complete immediately"
    );
    // Advancing the guest clock past the buffer's duration delivers it.
    let later = now.wrapping_add(300);
    let due = state
        .pop_next_due_timer(later)
        .expect("the WOM_DONE completion must fire ~200 ms out");
    assert_eq!(due.handle, 0x5500_0101, "the completion names the device");
    assert_eq!(
        due.kind,
        crate::winmm::DueTimerKind::WaveOutDone,
        "the pump dispatches it with the waveOutProc ABI"
    );
    assert_eq!(due.callback_va, callback_va, "the guest function VA");
    assert_eq!(due.user_data, 0x7A, "dwInstance round-trips");
}

/// `waveOutReset` / `waveOutClose` drop pending `WOM_DONE` completions.
#[test]
fn test_wave_out_reset_drops_pending_completions() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let phwo = 0x5000_u64;
    engine
        .mem_write(STACK_TOP - 0x28, &0x0003_0000_u64.to_le_bytes())
        .expect("write fdwOpen");
    write_regs(&mut engine, phwo, 0, 0, 0xBEEF_0000, 0);
    assert_eq!(dispatch_winmm(&mut engine, &mut state, "waveOutOpen"), 0);

    // Queue a completion directly (no PCM needed for the reset semantics),
    // scheduled 60 s out so it is not due during the test.
    let now =
        u32::try_from(crate::kernel32::clock::tick_count_32() & u64::from(u32::MAX)).unwrap_or(0);
    state.winmm().queue_wave_out_done(0x5500_0101, 60_000, now);
    assert!(
        state.pop_next_due_timer(now).is_none(),
        "the completion is due 60 s out"
    );
    // reset_wave_out through waveOutReset drops it even when due.
    write_regs(&mut engine, 0x5500_0101, 0, 0, 0, 0);
    assert_eq!(dispatch_winmm(&mut engine, &mut state, "waveOutReset"), 0);
    assert!(
        state.pop_next_due_timer(now.wrapping_add(60_001)).is_none(),
        "waveOutReset must drop the pending WOM_DONE"
    );
}
