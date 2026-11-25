//! Per-stage accounting for `CreateFile*`.
//!
//! Startup measured `kernel32!CreateFileW` at **1 447 µs per call over 34 calls
//! (49 ms)** in DOOM Retro (`docs/architecture/startup-import-costs.md`), which
//! is implausible for opening a file on a local disk and is not a plausible call
//! count for a guest that loads nine files. These three stages split it: the
//! directory-watch probe that runs before every open, the open itself, and the
//! guest-I/O mirror registration that follows it.

use crate::stage_timer::{Stage, field as stage_field, timed};

/// `open_directory_for_watch`: the `stat` probe on every `CreateFile`.
static DIR_PROBE: Stage = Stage::new();
/// `open_or_create_guest_path`: guest→host resolution plus the host open.
static OPEN: Stage = Stage::new();
/// `guest_io_host::register_open_file`: the guest I/O mirror.
static REGISTER: Stage = Stage::new();

pub(crate) fn timed_dir_probe<T>(f: impl FnOnce() -> T) -> T {
    timed(&DIR_PROBE, f)
}

pub(crate) fn timed_open<T>(f: impl FnOnce() -> T) -> T {
    timed(&OPEN, f)
}

pub(crate) fn timed_register<T>(f: impl FnOnce() -> T) -> T {
    timed(&REGISTER, f)
}

/// Report lines for the `WIE_RUNTIME_PROFILE` block; empty until something ran.
#[must_use]
pub(crate) fn report_lines() -> Vec<String> {
    let total = DIR_PROBE.read().0 + OPEN.read().0 + REGISTER.read().0;
    if total == 0 {
        return Vec::new();
    }
    let fields = [
        stage_field("dir_probe", &DIR_PROBE),
        stage_field("open", &OPEN),
        stage_field("register", &REGISTER),
    ]
    .join(" ");
    vec![format!(
        "createfile_stages: {fields} (calls/cost/per-call; dir_probe is the watch stat on every open)"
    )]
}

/// Zero every counter — tests only.
#[cfg(all(test, debug_assertions))]
pub(crate) fn reset_for_test() {
    for stage in [&DIR_PROBE, &OPEN, &REGISTER] {
        stage.reset();
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::{report_lines, reset_for_test, timed_open};

    #[test]
    fn stages_are_counted_and_reported() {
        reset_for_test();
        assert!(
            report_lines().is_empty(),
            "nothing recorded yet means no line"
        );
        timed_open(|| 1_u8);
        let lines = report_lines();
        assert_eq!(lines.len(), 1, "one aggregate line");
        assert!(lines[0].starts_with("createfile_stages:"), "{}", lines[0]);
        assert!(lines[0].contains("open=1/"), "{}", lines[0]);
    }
}
