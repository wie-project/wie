//! Per-stage accounting for `LoadLibrary*` resolution.
//!
//! `kernel32!LoadLibraryW` showed up in a DOOM Retro profile at 10 044 calls and
//! 846 ms — 84 µs per call, which is not a plausible cost for a cached lookup
//! and is not a plausible call rate for a game. One number like that has four
//! candidate explanations (directory rescan, per-call `stat`, a lock taken round
//! an I/O, or a legitimate probe for a module that is not there), and they are
//! indistinguishable from the total. These counters split it: every stage of
//! [`super::module::resolve_or_load_dll`] records how often it ran and how long
//! it took, so the report names the stage that owns the time.
//!
//! Attribution result, for the record: every `LoadLibraryW` from DOOM Retro was a
//! miss, and 93 % of the call was rebuilding the search-directory list. That is
//! what [`crate::dll_loader::resolve_dll_path`] now memoises.
//!
//! Counts are process-wide and never reset: this is diagnostic output for one
//! run, the same convention as [`crate::sdl2`]'s export stats.

use crate::stage_timer::{Stage, field as stage_field, timed};
use std::sync::atomic::{AtomicU64, Ordering};

/// Resolutions answered by the main-module name check.
static MAIN: Stage = Stage::new();
/// Resolutions answered by the fake/well-known module table.
static FAKE: Stage = Stage::new();
/// Resolutions answered by the loaded-module map (the cached-lookup path).
static LOADED: Stage = Stage::new();
/// Resolutions that had to go to disk: the DLL search order.
static SEARCH: Stage = Stage::new();
/// Resolutions that had to parse and map a PE image.
static MAP: Stage = Stage::new();
/// The part of the search order that builds the candidate directory list.
static SEARCH_DIRS: Stage = Stage::new();
/// The part of the search order that stats candidates on disk.
static SEARCH_STAT: Stage = Stage::new();
/// Number of `is_file()` probes the search order made.
static PROBES: AtomicU64 = AtomicU64::new(0);
/// Names that ended in `ERROR_MOD_NOT_FOUND`.
static MISSING: Stage = Stage::new();

/// Wrap the main-module check.
pub(crate) fn timed_main<T>(f: impl FnOnce() -> T) -> T {
    timed(&MAIN, f)
}

/// Wrap the fake/well-known module lookup.
pub(crate) fn timed_fake<T>(f: impl FnOnce() -> T) -> T {
    timed(&FAKE, f)
}

/// Wrap the loaded-module map lookup. This is the path a cached call takes, so
/// its average is the floor any fix should reach.
pub(crate) fn timed_loaded<T>(f: impl FnOnce() -> T) -> T {
    timed(&LOADED, f)
}

/// Wrap the DLL search order (`resolve_dll_path`).
pub(crate) fn timed_search<T>(f: impl FnOnce() -> T) -> T {
    timed(&SEARCH, f)
}

/// Wrap building the search-directory list.
pub(crate) fn timed_search_dirs<T>(f: impl FnOnce() -> T) -> T {
    timed(&SEARCH_DIRS, f)
}

/// Wrap one candidate `is_file()` probe.
pub(crate) fn timed_probe<T>(f: impl FnOnce() -> T) -> T {
    let _count = PROBES.fetch_add(1, Ordering::Relaxed);
    timed(&SEARCH_STAT, f)
}

/// Wrap PE parse + mapping (`load_dll`).
pub(crate) fn timed_map<T>(f: impl FnOnce() -> T) -> T {
    timed(&MAP, f)
}

/// Record one `ERROR_MOD_NOT_FOUND` outcome.
pub(crate) fn note_missing() {
    timed(&MISSING, || ());
}

/// Report lines for the `WIE_RUNTIME_PROFILE` block; empty until something ran.
#[must_use]
pub fn report_lines() -> Vec<String> {
    let (loaded_calls, _) = LOADED.read();
    let (missing, _) = MISSING.read();
    let (searched, _) = SEARCH.read();
    let (mapped, _) = MAP.read();
    if loaded_calls + missing + searched + mapped == 0 {
        return Vec::new();
    }
    let fields = [
        stage_field("main", &MAIN),
        stage_field("fake", &FAKE),
        stage_field("loaded", &LOADED),
        stage_field("search", &SEARCH),
        stage_field("map", &MAP),
        stage_field("search_dirs", &SEARCH_DIRS),
        stage_field("search_stat", &SEARCH_STAT),
        format!("probes={}", PROBES.load(Ordering::Relaxed)),
    ]
    .join(" ");
    vec![format!(
        "loadlibrary_stages: {fields} missing={missing} \
         (calls/cost/per-call; search is the DLL search order, map is PE parse)"
    )]
}

/// Zero every counter — tests only, so one test's calls do not skew the next.
#[cfg(all(test, debug_assertions))]
pub(crate) fn reset_for_test() {
    for stage in [
        &MAIN,
        &FAKE,
        &LOADED,
        &SEARCH,
        &MAP,
        &SEARCH_DIRS,
        &SEARCH_STAT,
        &MISSING,
    ] {
        stage.reset();
    }
    PROBES.store(0, Ordering::Relaxed);
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::{report_lines, reset_for_test, timed_loaded};

    /// The counters are what turn one export-level total into an attributable
    /// breakdown, so their shape is pinned: an empty report before any call, and
    /// a line naming each stage afterwards.
    #[test]
    fn stages_are_counted_and_reported() {
        reset_for_test();
        assert!(
            report_lines().is_empty(),
            "nothing recorded yet means no line"
        );
        timed_loaded(|| 7_u8);
        let lines = report_lines();
        assert_eq!(lines.len(), 1, "one aggregate line");
        assert!(lines[0].starts_with("loadlibrary_stages:"), "{}", lines[0]);
        assert!(lines[0].contains("loaded=1/"), "{}", lines[0]);
    }
}
