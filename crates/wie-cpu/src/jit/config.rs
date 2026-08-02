//! JIT configuration knobs (env-var driven, OnceLock-cached).
//!
//! Extracted verbatim from `jit/mod.rs` (Phase 4 split, lane 2). Visibility
//! bumps: everything read outside this module is `pub(super)` (visible across
//! `crate::jit`, matching the old private-in-`mod.rs` scope).

#![allow(
    unsafe_code, // Cranelift finalized fn pointers + host mem helpers
    private_interfaces, // JitShared/PerThreadJitState expose crate-private types
    clippy::indexing_slicing, // fixed gpr[0..16]
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::arithmetic_side_effects,
    clippy::unwrap_used // Mutex/RwLock poison recovery is hard-coded (never occurs in practice)
)]

use std::time::Duration;

/// Compile after this many visits to the same guest entry (skip cold code).
///
/// Default **100**: lower values (e.g. 12) cut residual iced but thrash short
/// non-loop blocks on 7za and **increase** wall (sweep 2026-07-21: thr=100 best).
/// Residual iced under thr=100 is almost all already-lowerable warmup (Mov/Call/…).
/// Override: `WIE_JIT_HOTNESS=N` (`0` = eager first visit). Tests use 0.
pub(super) fn hotness_threshold() -> u32 {
    use std::sync::OnceLock;
    static THR: OnceLock<u32> = OnceLock::new();
    *THR.get_or_init(|| {
        if cfg!(test) {
            return 0;
        }
        match std::env::var("WIE_JIT_HOTNESS") {
            Ok(v) => v.parse::<u32>().unwrap_or(100),
            Err(_) => 100,
        }
    })
}

/// Known pure self-loops: compile sooner (trade one Cranelift pass vs iced warmup).
/// Override: `WIE_JIT_LOOP_HOTNESS=N` (default 8; tests 0).
pub(super) fn pure_loop_hotness() -> u32 {
    use std::sync::OnceLock;
    static THR: OnceLock<u32> = OnceLock::new();
    *THR.get_or_init(|| {
        if cfg!(test) {
            return 0;
        }
        match std::env::var("WIE_JIT_LOOP_HOTNESS") {
            Ok(v) => v.parse::<u32>().unwrap_or(8),
            Err(_) => 8,
        }
    })
}

/// JIT memory lower mode (`WIE_JIT_MEM`).
///
/// - unset / `sticky` — sticky-TLB IR + **stack pin** (4.1b); helpers use all
///   pin slots (stack / heaps / VirtualAlloc) via `pin_resolve`
/// - `slow` — helper-only loads/stores (oracle / bisect; no host ptr in IR)
/// - `pin` — sticky + stack + **top-2 data pin IR** (heaps/VA); helpers same
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JitMemMode {
    Slow,
    Sticky,
    Pin,
}

fn jit_mem_mode() -> JitMemMode {
    use std::sync::OnceLock;
    static MODE: OnceLock<JitMemMode> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("WIE_JIT_MEM") {
        Ok(v) if v.eq_ignore_ascii_case("slow") || v == "0" || v.eq_ignore_ascii_case("off") => {
            JitMemMode::Slow
        }
        Ok(v) if v.eq_ignore_ascii_case("pin") => JitMemMode::Pin,
        Ok(v) if v.eq_ignore_ascii_case("sticky") || v.eq_ignore_ascii_case("fast") => {
            JitMemMode::Sticky
        }
        _ => JitMemMode::Sticky,
    })
}

/// Whether Cranelift may emit inline sticky-TLB load/store (not helper-only).
pub(crate) fn jit_mem_inline_enabled() -> bool {
    !matches!(jit_mem_mode(), JitMemMode::Slow)
}

/// Whether Cranelift may emit **data** pin IR (heap + VirtualAlloc) after sticky.
///
/// Default sticky still fills all pin slots for helper `pin_resolve`; only
/// `WIE_JIT_MEM=pin` adds IR probes (can help some heaps, tax on thrashy paths).
pub(crate) fn jit_mem_pin_enabled() -> bool {
    matches!(jit_mem_mode(), JitMemMode::Pin)
}

/// Opt-in mem helper resolution histogram (`WIE_JIT_MEM_TRACE=1`).
pub(super) fn mem_path_trace_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| match std::env::var("WIE_JIT_MEM_TRACE") {
        Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes") => {
            true
        }
        // Also dump when residual iced trace is on (profiling runs).
        _ => matches!(
            std::env::var("WIE_EXEC_TRACE"),
            Ok(v) if v == "1" || v.eq_ignore_ascii_case("true")
        ),
    })
}

/// Block-wide stack super path (`WIE_JIT_SUPER`).
///
/// - unset / `loop` — **default**: only self-loop blocks (safe; `long_loop`-style)
/// - `0` / `off` / `false` — disabled (sticky/pin probes only)
/// - `all` / `1` / `true` — all stack-pin-shaped blocks (experimental; can host-fault
///   on non-loop super, e.g. `7za a` under default All previously)
pub(crate) fn jit_super_enabled(self_loop: bool) -> bool {
    use std::sync::OnceLock;
    #[derive(Clone, Copy)]
    enum SuperMode {
        Off,
        LoopOnly,
        All,
    }
    static MODE: OnceLock<SuperMode> = OnceLock::new();
    let mode = *MODE.get_or_init(|| match std::env::var("WIE_JIT_SUPER") {
        Ok(v) if v == "0" || v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("false") => {
            SuperMode::Off
        }
        Ok(v)
            if v == "1"
                || v.eq_ignore_ascii_case("true")
                || v.eq_ignore_ascii_case("on")
                || v.eq_ignore_ascii_case("all") =>
        {
            SuperMode::All
        }
        // unset, "loop", "selfloop", or any other value → self-loops only
        _ => SuperMode::LoopOnly,
    });
    match mode {
        SuperMode::Off => false,
        SuperMode::LoopOnly => self_loop,
        SuperMode::All => true,
    }
}

/// Late-bound + direct block chaining (`WIE_JIT_CHAIN=0` disables).
pub(super) fn jit_chain_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            std::env::var("WIE_JIT_CHAIN"),
            Ok(v) if v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off")
        )
    })
}

/// Background compiler worker (`WIE_JIT_BG=0` disables).
///
/// Default: **on** for real runs, **off** under `cfg(test)` so the unit-test
/// suite keeps the deterministic inline-compile path (hotness is 0 there, so
/// every block is eager). Dedicated worker tests force it on per-`JitShared`
/// via [`JitShared::bg_force`].
pub(super) fn bg_jit_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| match std::env::var("WIE_JIT_BG") {
        Ok(v) if v == "0" || v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("false") => {
            false
        }
        Ok(v) if v == "1" || v.eq_ignore_ascii_case("on") || v.eq_ignore_ascii_case("true") => true,
        _ => !cfg!(test),
    })
}

/// Max guest-wait for a background compile before falling back to inline
/// compilation. A single compile is ~50–500 µs, so 10 ms is ~20× headroom;
/// the fallback only triggers on queue backlog or a dead worker.
/// Override: `WIE_JIT_BG_TIMEOUT_US=N`.
pub(super) fn bg_wait_timeout() -> Duration {
    use std::sync::OnceLock;
    static D: OnceLock<Duration> = OnceLock::new();
    *D.get_or_init(|| {
        let us = match std::env::var("WIE_JIT_BG_TIMEOUT_US") {
            Ok(v) => v.parse::<u64>().unwrap_or(10_000),
            Err(_) => 10_000,
        };
        Duration::from_micros(us)
    })
}

/// Max queued background compile requests. Bounds worker memory (each entry
/// holds a decoded block) and keeps guest wait budgets meaningful: beyond this,
/// the guest falls back to inline compilation instead of queueing behind an
/// unbounded backlog.
pub(super) const BG_QUEUE_CAP: usize = 1024;
