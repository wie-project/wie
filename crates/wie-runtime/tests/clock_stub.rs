//! B5 clock-table integration: in-guest clock stubs fire with zero host stops.
//!
//! Requires a mingw-built micro-exe that calls a clock API (`guess_price.exe`
//! seeds its RNG with `srand(GetTickCount())`); skips when not built
//! (`make -C micro-exes`).

use std::path::PathBuf;

fn micro_exe(name: &str) -> Option<PathBuf> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("micro-exes/out");
    path.push(name);
    path.is_file().then_some(path)
}

/// `guess_price.exe` calls `GetTickCount()` (via `srand`) at startup. With the
/// B5 host-written clock table planted, GetTickCount must NOT appear among the
/// profiled host stops — the in-guest stub reads the table instead.
#[test]
fn get_tick_count_is_a_guest_stub_not_a_host_stop() {
    let Some(path) = micro_exe("guess_price.exe") else {
        eprintln!("skip: micro-exes/out/guess_price.exe not built (run make -C micro-exes)");
        return;
    };

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::ExitOnIdle)
            .expect("guess_price session starts");
    // Frame timing turns on the runtime profile (host_stops / by_export)
    // without the `WIE_RUNTIME_PROFILE` env var.
    session.enable_frame_timing();
    // Injected stdin: the console game reads one line, then EOFs and exits.
    session.set_stdin_bytes(b"50\n".to_vec());

    let summary = session.run_until_stop(500_000).expect("guess_price runs");

    match &summary.termination {
        wie_runtime::EntryTraceTermination::ExitProcess { code } => {
            assert_eq!(*code, 0, "guess_price must exit 0 with injected stdin");
        }
        other => {
            panic!("guess_price stopped unexpectedly: {other:?}");
        }
    }

    let profile = session.profile();
    assert!(
        !profile.by_export.contains_key("KERNEL32.dll!GetTickCount"),
        "GetTickCount caused a host stop — the in-guest table stub should absorb it"
    );
}
