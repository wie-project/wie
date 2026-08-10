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
