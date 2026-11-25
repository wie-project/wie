//! Per-stage accounting for `CreateFile*`.
//!
//! Startup measured `kernel32!CreateFileW` at **1 447 µs per call over 34 calls
//! (49 ms)** in DOOM Retro (`docs/architecture/startup-import-costs.md`), which
//! is implausible for opening a file on a local disk and is not a plausible call
//! count for a guest that loads nine files. These three stages split it: the
//! directory-watch probe that runs before every open, the open itself, and the
//! guest-I/O mirror registration that follows it.
//!
//! Read the totals in the report with one rule: aggregate handler time
//! (`handler_ms`) varies ~2x run to run on a loaded host and is not evidence of
//! anything. An isolated stage cost that moves by more than that band is.

use crate::stage_timer::{Counter, Stage, field as stage_field, timed};

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

/// Sub-stages of guest-handle registration (`guest_io_host::register_open_file`).
///
/// `register` measured **2 331 µs per call over 9 calls (21 ms)** in DOOM Retro
/// startup, which is why it is split. The `mirror` stage that split found is
/// **gone**: it timed the O(size) copy of the file's bytes into the guest
/// arena, at 5 199.62 µs per call over 3 calls, and the copy served nothing
/// (see `register_open_file`). What is left is the slot scan — up to 128 guest
/// slots with a host `mem_read` each — plus the slot store and the lookup.
static REG_LOOKUP: Stage = Stage::new();
static REG_SLOT_SCAN: Stage = Stage::new();
static REG_SLOT_STORE: Stage = Stage::new();

/// Sub-stages of `CloseHandle`'s file path, which measured 890 µs per call.
static CLOSE_SYNC_VIRTUAL: Stage = Stage::new();
static CLOSE_PERSIST: Stage = Stage::new();
static CLOSE_UNREGISTER: Stage = Stage::new();

pub(crate) fn reg_lookup_start() -> std::time::Instant {
    std::time::Instant::now()
}

pub(crate) fn reg_lookup_done(start: std::time::Instant) {
    REG_LOOKUP.record_since(start);
}

pub(crate) fn timed_reg_slot_scan<T>(f: impl FnOnce() -> T) -> T {
    timed(&REG_SLOT_SCAN, f)
}

pub(crate) fn timed_reg_slot_store<T>(f: impl FnOnce() -> T) -> T {
    timed(&REG_SLOT_STORE, f)
}

pub(crate) fn timed_close_sync_virtual<T>(f: impl FnOnce() -> T) -> T {
    timed(&CLOSE_SYNC_VIRTUAL, f)
}

pub(crate) fn timed_close_persist<T>(f: impl FnOnce() -> T) -> T {
    timed(&CLOSE_PERSIST, f)
}

/// `ReadFile` calls split by whether the handle owns a guest-I/O arena mirror.
///
/// A handle with a slot publishes length and cursor so the guest can answer
/// seeks and sizes without a host stop. `slotted` counts the `ReadFile` calls
/// that reached this handler anyway; since the byte mirror was removed there is
/// no read fast path left, so that split now measures how often the slot bought
/// the call nothing — which is every read.
static READFILE_CALLS: Counter = Counter::new();
static READFILE_SLOTTED: Counter = Counter::new();
static READFILE_SLOTTED_BYTES: Counter = Counter::new();
static READFILE_UNSLOTTED_BYTES: Counter = Counter::new();

/// Record one `ReadFile` handler call.
///
/// `slotted` means the handle holds a guest I/O handle-table slot. It used to
/// mean "owns a mirror of its bytes in the arena"; there is no mirror any more,
/// so what the split now answers is whether the published length/cursor bought
/// the call anything — and the answer for every read is no, because the read
/// itself always comes here.
pub(crate) fn count_read(slotted: bool, bytes: u64) {
    READFILE_CALLS.add(1);
    if slotted {
        READFILE_SLOTTED.add(1);
        READFILE_SLOTTED_BYTES.add(bytes);
    } else {
        READFILE_UNSLOTTED_BYTES.add(bytes);
    }
}

pub(crate) fn timed_close_unregister<T>(f: impl FnOnce() -> T) -> T {
    timed(&CLOSE_UNREGISTER, f)
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
    let mut lines = vec![format!(
        "createfile_stages: {fields} (calls/cost/per-call; dir_probe is the watch stat on every open)"
    )];
    if REG_SLOT_SCAN.read().0 > 0 {
        let fields = [
            stage_field("lookup", &REG_LOOKUP),
            stage_field("slot_scan", &REG_SLOT_SCAN),
            stage_field("slot_store", &REG_SLOT_STORE),
        ]
        .join(" ");
        lines.push(format!(
            "register_stages: {fields} (calls/cost/per-call; no byte mirror any more, slot_scan walks up to 128 slots)"
        ));
    }
    if CLOSE_PERSIST.read().0 > 0 || CLOSE_UNREGISTER.read().0 > 0 {
        let fields = [
            stage_field("sync_virtual", &CLOSE_SYNC_VIRTUAL),
            stage_field("persist", &CLOSE_PERSIST),
            stage_field("unregister", &CLOSE_UNREGISTER),
        ]
        .join(" ");
        lines.push(format!(
            "closehandle_stages: {fields} (calls/cost/per-call; persist writes the whole buffered file back to its host path)"
        ));
    }
    if READFILE_CALLS.get() > 0 {
        let calls = READFILE_CALLS.get();
        let slotted = READFILE_SLOTTED.get();
        let pct = if calls == 0 {
            0.0
        } else {
            100.0 * f64::from(u32::try_from(slotted).unwrap_or(u32::MAX))
                / f64::from(u32::try_from(calls).unwrap_or(u32::MAX))
        };
        lines.push(format!(
            "readfile_slots: calls={calls} slotted={slotted} ({pct:.1}%) slotted_bytes={} unslotted_bytes={} (a read of a handle that holds a guest handle-table slot still crossed the host boundary; the slot carries length and cursor only — the byte mirror was removed, so every read is served by the host)",
            READFILE_SLOTTED_BYTES.get(),
            READFILE_UNSLOTTED_BYTES.get()
        ));
    }
    lines
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
