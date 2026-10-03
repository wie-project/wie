//! Every `WIE_*` environment variable this crate reads, one named getter each.
//!
//! # Why this exists
//!
//! The knobs are **kill-switches for bisecting**, not configuration. See the
//! "Environment knobs (full table)" section of `docs/RUNBOOK.md`: the workflow
//! is "suspect the JIT → `WIE_CPU=iced`", "string helper looks wrong →
//! `WIE_STRING_BULK=0`". That workflow needs the variables to be settable *from
//! the shell, at any moment, with no recompile*, so they must stay ambient
//! process state read through `std::env` at the point of use. A `Config` struct
//! threaded through `SessionOptions` would be a different design and is
//! deliberately not what this module does: it would change `SessionOptions`'s
//! shape and remove the ability to flip a switch between two runs of the same
//! binary without relaunching the emulator's front end.
//!
//! What this module *does* buy is greppability. Before it existed the complete
//! read set was scattered across `memory.rs`, `mt_runtime.rs`, `guest_*.rs`,
//! `session/init.rs`, `session/mod.rs` and `trace.rs`, each doing a bare
//! `std::env::var` inline. `docs/RUNBOOK.md`'s knob table drifted as a result.
//! Now the set is one list, and `tests/runbook_knobs.rs` asserts the table
//! against it.
//!
//! # Two hard rules for anyone editing this file
//!
//! 1. **Do not change *when* a knob is observed.** Some are deliberately read
//!    late and some early, and the asymmetry matters. `runtime_profile()` is
//!    read once during session construction because arming the profile gate has
//!    to happen before the first frame is timed; `guest_heap_rewire()` is read
//!    when the accelerator is installed, which is well after layout is fixed.
//!    Moving a read earlier or later is a behaviour change even when the code
//!    still compiles and every test still passes.
//! 2. **Do not add a cache to a getter that does not have one.** Caching is
//!    only safe where the runtime already promises not to observe changes. Two
//!    getters below are `OnceLock`-cached and were cached *before* this module
//!    existed; every other getter is a plain read-through, because the GUI path
//!    mutates the process environment (`std::env::set_var`) and a cache added
//!    now could observe a different value than the code did before.
//!
//! `wie-cli` reads its own knobs (`WIE_API_TRACE`, `WIE_INPUT_SCRIPT`,
//! `WIE_CAPTURE_STREAM`) and owns the `WIE_IDLE` / `WIE_ROOT` `set_var` calls;
//! `wie-cpu` and `wie-winapi` read theirs in their own crates. This module is
//! the single list for `wie-runtime` only.

use std::sync::OnceLock;

// ── Guest accelerators ────────────────────────────────────────────────────────

/// `WIE_GUEST_HEAP` — rewire process-heap `HeapAlloc`/`HeapFree` to guest code.
///
/// Default **off**: the host freelist is cheaper than the dual guest path in
/// wall/CPU terms. The in-guest control block is planted either way, so the
/// host path stays coherent.
///
/// Read by [`crate::guest_heap_accel::install_guest_heap_accel`], after the
/// memory layout is fixed.
pub(crate) fn guest_heap_rewire() -> bool {
    matches!(
        std::env::var("WIE_GUEST_HEAP").as_deref(),
        Ok("1" | "true" | "on" | "yes")
    )
}

/// `WIE_GUEST_IO` — I/O accelerator selection, as the raw string.
///
/// Parsed by [`crate::guest_io::guest_io_rewire`], which owns the
/// `0` / `all` / `read,seek,size` grammar. Kept as the raw value here so this
/// module stays a list of reads rather than a second copy of the grammar.
pub(crate) fn guest_io_mode() -> Option<String> {
    std::env::var("WIE_GUEST_IO").ok()
}

/// `WIE_GUEST_MBWC` — rewire guest `MultiByteToWideChar` to guest code.
///
/// Default **off**: per-character expansion in the emulator is slower in
/// wall-clock than one host stop plus a bulk `mem_write` for WIE's `CP_UTF8`
/// traffic.
///
/// Read by [`crate::guest_mbwc::install_guest_mbwc`].
pub(crate) fn guest_mbwc_rewire() -> bool {
    matches!(
        std::env::var("WIE_GUEST_MBWC").as_deref(),
        Ok("1" | "true" | "on" | "yes")
    )
}

// ── Memory layout ─────────────────────────────────────────────────────────────

/// `WIE_PROCESS_HEAP_MB` — guest process-heap size in MiB (default **512**).
///
/// Read by [`crate::memory::RuntimeMemoryLayout::with_env_overrides`], i.e.
/// while the arena map is being built. Parsing, the `> 0` check and the 16 GiB
/// clamp stay at the call site; this only hands over the raw string.
pub(crate) fn process_heap_mb() -> Option<String> {
    std::env::var("WIE_PROCESS_HEAP_MB").ok()
}

/// `WIE_NO_HOOK_SLICES` — cap on the no-hook slice lookahead, as a raw string.
///
/// Read by [`crate::memory::RuntimeMemoryLayout::with_env_overrides`].
pub(crate) fn no_hook_slice_limit() -> Option<String> {
    std::env::var("WIE_NO_HOOK_SLICES").ok()
}

/// `WIE_GUEST_ENV` — `"NAME=VALUE;NAME2=VALUE2"` pairs injected into the guest
/// environment of every new session, as the raw `OsString`.
///
/// Read by [`crate::memory::apply_host_guest_env_overrides`].
///
/// `OsString` rather than `String` because the call site already handles a
/// non-UTF-8 value by bailing out, and normalising to `String` here would
/// change *which* values are silently ignored.
pub(crate) fn guest_env_pairs() -> Option<std::ffi::OsString> {
    std::env::var_os("WIE_GUEST_ENV")
}

// ── Volume roots ──────────────────────────────────────────────────────────────

/// `WIE_ROOT` — optional bottle override for guest `C:\`, else `None`.
///
/// Delegates to [`wie_winapi::bottle_root_from_env`] rather than reading the
/// variable here: `wie-winapi` owns the single literal and three call sites in
/// this crate share it (`default_winapi_state`, session init,
/// [`crate::trace::run_micro_exe`]). Re-reading it locally would let the bottle
/// root and the volume config disagree.
pub(crate) fn bottle_root_from_env() -> Option<std::path::PathBuf> {
    wie_winapi::bottle_root_from_env()
}

/// `WIE_DRIVE_D` — host root for the guest `D:\` bridge (`auto` = host cwd),
/// else `None`. See [`bottle_root_from_env`] for why this delegates.
pub(crate) fn drive_d_root_from_env() -> Option<std::path::PathBuf> {
    wie_winapi::drive_d_from_env()
}

// ── Threading ─────────────────────────────────────────────────────────────────

/// `WIE_MT_DEBUG` — verbose guest worker-thread tracing.
///
/// **Cached**, and was before this module existed: the getter is hit on every
/// spawn / park / worker-exit path, so a per-call `getenv()` was measurable.
/// `OnceLock` is safe here because `wie-runtime` does not mutate its own
/// environment; the `set_var` calls live in `wie-cli`.
pub(crate) fn mt_debug() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("WIE_MT_DEBUG").is_some())
}

/// `WIE_ALLOW_MISSING_GUESTS` — downgrade the "missing guest fixture" panic to
/// a skip, for a machine without the mingw-w64 cross toolchain.
///
/// **Test-only.** `#[cfg(test)]` because it exists purely to relax the
/// fixture-presence rule the integration suite enforces; no production path may
/// read it.
#[cfg(test)]
pub(crate) fn allow_missing_guests() -> bool {
    std::env::var_os("WIE_ALLOW_MISSING_GUESTS").is_some()
}

// ── Diagnostics ───────────────────────────────────────────────────────────────

/// `WIE_API_JOURNAL` — per-API journal file for backend A/B diffs.
///
/// **Cached**, and was before this module existed: [`crate::journal_api_return`]
/// runs on the host-stop hot path, where a `getenv()` per API call was real
/// work. The journal target cannot change mid-run in any useful way — it is
/// opened per line, but a session that changed journals halfway through would
/// produce two interleaved files and an unusable diff.
///
/// Returns `&'static str`, not `String`, so the hot path does not clone per
/// API stop; the `OnceLock` owns the `String`.
pub(crate) fn api_journal_path() -> Option<&'static str> {
    static JOURNAL_PATH: OnceLock<Option<String>> = OnceLock::new();
    JOURNAL_PATH
        .get_or_init(|| std::env::var("WIE_API_JOURNAL").ok())
        .as_deref()
}

/// `WIE_RUNTIME_PROFILE` — arm the runtime profile report (wall/CPU%, host
/// stops, JIT counters, `mem_backend`).
///
/// Read **once**, during session construction in
/// [`crate::session::RuntimeSession::from_init`], because arming the gate also
/// calls `wie_winapi::present::set_frame_timing_enabled(true)`. Arming it later
/// than that would leave the first frames untimed; arming it earlier is not
/// possible without reading the variable before the session exists.
pub(crate) fn runtime_profile() -> bool {
    std::env::var_os("WIE_RUNTIME_PROFILE").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the module is that the knob *names* live in one greppable
    /// place. This test does not exercise the env lookup (that is inherently
    /// process-global and racy under `nextest`'s per-test processes); it
    /// asserts the roster instead, so deleting or renaming a getter is a test
    /// failure rather than a silent RUNBOOK drift.
    #[test]
    fn knob_getters_are_present() {
        // Referencing each getter keeps them from being deleted as dead code
        // and documents the roster. `let _ = f;` is a binding, so
        // `let_underscore_drop` does not fire on the function items.
        let roster: [fn() -> bool; 4] = [
            guest_heap_rewire,
            guest_mbwc_rewire,
            runtime_profile,
            api_journal_path_is_some,
        ];
        for f in roster {
            let _ = f;
        }
        let _ = guest_io_mode;
        let _ = process_heap_mb;
        let _ = no_hook_slice_limit;
        let _ = guest_env_pairs;
        let _ = bottle_root_from_env;
        let _ = drive_d_root_from_env;
        let _ = mt_debug;
    }

    fn api_journal_path_is_some() -> bool {
        api_journal_path().is_some()
    }

    #[test]
    fn allow_missing_guests_is_read_only_in_test_builds() {
        // Present in test builds only; see the `#[cfg(test)]` on the getter.
        let _ = allow_missing_guests;
    }
}
