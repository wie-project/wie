//! JIT configuration knobs (env-var driven, once per process).
//!
//! All knobs live in one [`JitConfig`] behind a single `OnceLock`, so a knob
//! lookup is one lock-free load plus a field read — the same cost profile as
//! the former per-knob `OnceLock`s. `WIE_JIT_*` env parsing and defaults are
//! preserved verbatim; only the storage is consolidated.

#![allow(
    unsafe_code, // Cranelift finalized fn pointers + host mem helpers
    private_interfaces // JitShared/PerThreadJitState expose crate-private types
)]

use std::sync::OnceLock;
use std::time::Duration;

/// Max queued background compile requests. Bounds worker memory (each entry
/// holds a decoded block) and keeps guest wait budgets meaningful: beyond this,
/// the guest falls back to inline compilation instead of queueing behind an
/// unbounded backlog.
pub(super) const BG_QUEUE_CAP: usize = 1024;

/// JIT memory lower mode (`WIE_JIT_MEM`).
///
/// - unset / `sticky` — sticky-TLB IR + **stack pin** (4.1b); helpers use all
///   pin slots (stack / heaps / VirtualAlloc) via `pin_resolve`
/// - `slow` — helper-only loads/stores (oracle / bisect; no host ptr in IR)
/// - `pin` — sticky + stack + **top-2 data pin IR** (heaps/VA); helpers same
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum JitMemMode {
    Slow,
    Sticky,
    Pin,
}

/// Block-wide stack super path (`WIE_JIT_SUPER`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SuperMode {
    Off,
    LoopOnly,
    All,
}

/// Consolidated JIT configuration, initialized once from the environment.
///
/// The flat bool surface mirrors the one-knob-per-env-var contract (`WIE_JIT_*`);
/// folding the toggles into enums would obscure that mapping for no benefit.
pub(super) struct JitConfig {
    hotness_threshold: u32,
    pure_loop_hotness: u32,
    eager_block_insns: usize,
    jit_mem_mode: JitMemMode,
    mem_path_trace: bool,
    super_mode: SuperMode,
    chain_enabled: bool,
    bg_enabled: bool,
    bg_wait_timeout: Duration,
    opt_level: &'static str,
    verifier_enabled: bool,
    simd_enabled: bool,
    tlb_neon_enabled: bool,
    string_inline_enabled: bool,
}

static CONFIG: OnceLock<JitConfig> = OnceLock::new();

impl JitConfig {
    /// The process-wide JIT configuration.
    ///
    /// Lock-free after the first call; safe on hot paths (per-block compile
    /// decisions).
    #[must_use]
    pub(super) fn get() -> &'static Self {
        CONFIG.get_or_init(Self::from_env)
    }

    fn from_env() -> Self {
        Self {
            // Compile after this many visits to the same guest entry (skip
            // cold code). Default 100: lower values cut residual iced but thrash
            // short non-loop blocks on 7za and increase wall. Tests use 0.
            hotness_threshold: hotness_threshold_from_env(),
            // Known pure self-loops: compile sooner (one Cranelift pass vs iced
            // warmup). Default 8; tests 0.
            pure_loop_hotness: env_u32("WIE_JIT_LOOP_HOTNESS", 8, true),
            // Large one-shot Pure blocks skip the fixed hotness wait (see
            // `eager_block_insns_from_env`). Default 48; `=0` disables.
            eager_block_insns: eager_block_insns_from_env(),
            jit_mem_mode: match std::env::var("WIE_JIT_MEM") {
                Ok(v)
                    if v.eq_ignore_ascii_case("slow")
                        || v == "0"
                        || v.eq_ignore_ascii_case("off") =>
                {
                    JitMemMode::Slow
                }
                Ok(v) if v.eq_ignore_ascii_case("pin") => JitMemMode::Pin,
                Ok(v) if v.eq_ignore_ascii_case("sticky") || v.eq_ignore_ascii_case("fast") => {
                    JitMemMode::Sticky
                }
                _ => {
                    // Pin mode reduces helper-path memory ops by ~89% on
                    // Doom Retro startup (21M→2.4M load helpers).
                    JitMemMode::Pin
                }
            },
            // Opt-in mem helper resolution histogram (also dumps when residual
            // iced trace is on, for profiling runs).
            mem_path_trace: matches!(
                std::env::var("WIE_JIT_MEM_TRACE"),
                Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes")
            ) || matches!(
                std::env::var("WIE_EXEC_TRACE"),
                Ok(v) if v == "1" || v.eq_ignore_ascii_case("true")
            ),
            // - unset / `loop` — default: only self-loop blocks (`long_loop`-style)
            // - `0` / `off` / `false` — disabled (sticky/pin probes only)
            // - `all` / `1` / `true` — all stack-pin-shaped blocks (experimental)
            super_mode: match std::env::var("WIE_JIT_SUPER") {
                Ok(v)
                    if v == "0"
                        || v.eq_ignore_ascii_case("off")
                        || v.eq_ignore_ascii_case("false") =>
                {
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
                _ => SuperMode::LoopOnly,
            },
            // Late-bound + direct block chaining (`WIE_JIT_CHAIN=0` disables).
            chain_enabled: !matches!(
                std::env::var("WIE_JIT_CHAIN"),
                Ok(v) if v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off")
            ),
            // Background compiler worker. Default: on for real runs, off under
            // `cfg(test)` (hotness is 0 there, so every block is eager; the
            // deterministic inline-compile path keeps the unit suite stable).
            // Dedicated worker tests force it on per-`JitShared` via
            // [`JitShared::bg_force`].
            bg_enabled: match std::env::var("WIE_JIT_BG") {
                Ok(v)
                    if v == "0"
                        || v.eq_ignore_ascii_case("off")
                        || v.eq_ignore_ascii_case("false") =>
                {
                    false
                }
                Ok(v)
                    if v == "1"
                        || v.eq_ignore_ascii_case("on")
                        || v.eq_ignore_ascii_case("true") =>
                {
                    true
                }
                _ => !cfg!(test),
            },
            // Max guest-wait for a background compile before falling back to
            // inline compilation. A single compile is ~50–500 µs, so 10 ms is
            // ~20× headroom; the fallback only triggers on queue backlog or a
            // dead worker.
            bg_wait_timeout: Duration::from_micros(env_u64("WIE_JIT_BG_TIMEOUT_US", 1_000)),
            // Cranelift `opt_level`: `speed` | `speed_and_size` | `none`.
            // Default `speed` (hot guest blocks over code size).
            opt_level: match std::env::var("WIE_JIT_OPT") {
                Ok(v) if v.eq_ignore_ascii_case("none") || v == "0" => "none",
                Ok(v)
                    if v.eq_ignore_ascii_case("speed_and_size")
                        || v.eq_ignore_ascii_case("size")
                        || v.eq_ignore_ascii_case("speed-and-size") =>
                {
                    "speed_and_size"
                }
                Ok(v) if v.eq_ignore_ascii_case("speed") || v.eq_ignore_ascii_case("fast") => {
                    "speed"
                }
                _ => "speed",
            },
            // Run the Cranelift IR verifier only when `WIE_JIT_VERIFY=1`.
            // (Never on under `cfg(test)` automatically — every release-mode
            // test run would pay the verifier tax on every compile.)
            verifier_enabled: matches!(
                std::env::var("WIE_JIT_VERIFY"),
                Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on")
            ),
            // Emit Cranelift SIMD types for SSE (`WIE_JIT_SIMD=0` disables).
            simd_enabled: !matches!(
                std::env::var("WIE_JIT_SIMD"),
                Ok(v) if v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off")
            ),
            // Neon software-TLB tag compare (`WIE_TLB_NEON=0` → scalar 4-way scan).
            tlb_neon_enabled: !matches!(
                std::env::var("WIE_TLB_NEON"),
                Ok(v) if v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off")
            ),
            // Inline REP MOVS/STOS for 16–64 byte const counts
            // (`WIE_STRING_INLINE=0` disables).
            string_inline_enabled: !matches!(
                std::env::var("WIE_STRING_INLINE"),
                Ok(v) if v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off")
            ),
        }
    }

    /// Compile after this many visits to the same guest entry (skip cold code).
    /// Default 100: lower values cut residual iced but thrash short non-loop
    /// blocks on 7za and increase wall. Tests use 0.
    #[must_use]
    pub(super) fn hotness_threshold(&self) -> u32 {
        self.hotness_threshold
    }

    /// Known pure self-loops: compile sooner (one Cranelift pass vs iced
    /// warmup). Default 8; tests 0.
    #[must_use]
    pub(super) fn pure_loop_hotness(&self) -> u32 {
        self.pure_loop_hotness
    }

    /// One-shot eager-compile cutoff. Pure, non-loop, non-UCRT blocks with at
    /// least this many guest instructions (lowerable body length) compile on
    /// first sight instead of waiting out the fixed hotness threshold. This
    /// tunes interpreter-bound cold init off iced; short fragments stay
    /// visit-gated so short-block compile thrash does not regress. `0` disables.
    #[must_use]
    pub(super) fn eager_block_insns(&self) -> usize {
        self.eager_block_insns
    }

    /// Whether Cranelift may emit inline sticky-TLB load/store (not helper-only).
    #[must_use]
    pub(crate) fn mem_inline_enabled(&self) -> bool {
        !matches!(self.jit_mem_mode, JitMemMode::Slow)
    }

    /// Whether Cranelift may emit **data** pin IR (heap + VirtualAlloc) after sticky.
    #[must_use]
    pub(crate) fn mem_pin_enabled(&self) -> bool {
        matches!(self.jit_mem_mode, JitMemMode::Pin)
    }

    /// Whether the mem-helper resolution histogram is active.
    #[must_use]
    pub(super) fn mem_path_trace_enabled(&self) -> bool {
        self.mem_path_trace
    }

    /// Whether stack-super IR is allowed for this block (`self_loop` blocks
    /// always are; `all` extends it to every stack-pin-shaped block).
    #[must_use]
    pub(crate) fn super_enabled(&self, self_loop: bool) -> bool {
        match self.super_mode {
            SuperMode::Off => false,
            SuperMode::LoopOnly => self_loop,
            SuperMode::All => true,
        }
    }

    /// Late-bound + direct block chaining (`WIE_JIT_CHAIN=0` disables).
    #[must_use]
    pub(super) fn chain_enabled(&self) -> bool {
        self.chain_enabled
    }

    /// Background compiler worker. Default: on for real runs, off under
    /// `cfg(test)` (hotness is 0 there, so every block is eager; the
    /// deterministic inline-compile path keeps the unit suite stable).
    #[must_use]
    pub(super) fn bg_enabled(&self) -> bool {
        self.bg_enabled
    }

    /// Max guest-wait for a background compile before falling back to inline
    /// compilation. A single compile is ~50–500 µs, so 10 ms is ~20× headroom;
    /// the fallback only triggers on queue backlog or a dead worker.
    #[must_use]
    pub(super) fn bg_wait_timeout(&self) -> Duration {
        self.bg_wait_timeout
    }

    /// Cranelift `opt_level`: `speed` | `speed_and_size` | `none`.
    /// Default `speed` (hot guest blocks over code size).
    #[must_use]
    pub(super) fn opt_level(&self) -> &'static str {
        self.opt_level
    }

    /// Run the Cranelift IR verifier only when `WIE_JIT_VERIFY=1`.
    #[must_use]
    pub(super) fn verifier_enabled(&self) -> bool {
        self.verifier_enabled
    }

    /// Emit Cranelift SIMD types for SSE (`WIE_JIT_SIMD=0` disables).
    #[must_use]
    pub(super) fn simd_enabled(&self) -> bool {
        self.simd_enabled
    }

    /// Neon software-TLB tag compare (`WIE_TLB_NEON=0` → scalar 4-way scan).
    #[must_use]
    pub(super) fn tlb_neon_enabled(&self) -> bool {
        self.tlb_neon_enabled
    }

    /// Inline REP MOVS/STOS for 16–64 byte const counts
    /// (`WIE_STRING_INLINE=0` disables).
    #[must_use]
    pub(super) fn string_inline_enabled(&self) -> bool {
        self.string_inline_enabled
    }
}

/// Upper bound on the fixed hotness threshold (`WIE_JIT_HOTNESS_THRESHOLD`).
///
/// Clamping keeps an experimental override from deferring a genuinely hot block
/// forever; the default (100) is far below this cap.
const HOTNESS_THRESHOLD_MAX: u32 = 1_000_000;

/// Parse the fixed hotness threshold from `WIE_JIT_HOTNESS_THRESHOLD`.
///
/// Default 100 (the historical fixed threshold). Parsed safely and clamped to
/// `[1, HOTNESS_THRESHOLD_MAX]` so an invalid or absurd value cannot break the
/// block-decision path. Under `cfg(test)` the threshold is 0 so every block
/// compiles eagerly (deterministic unit suite).
fn hotness_threshold_from_env() -> u32 {
    if cfg!(test) {
        return 0;
    }
    let raw = std::env::var("WIE_JIT_HOTNESS_THRESHOLD")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(100);
    raw.clamp(1, HOTNESS_THRESHOLD_MAX)
}

/// Eager-compile cutoff for large one-shot Pure blocks (`WIE_JIT_EAGER_BLOCK_INSNS`).
///
/// Default 48. `0` disables eager-by-size, so every non-loop block waits out
/// the fixed hotness threshold (useful for diagnosing compile-thrash regressions).
/// The decision cost is a usize compare during decode; when the fixed hotness
/// is already `0` (unit suite) the block compiles eagerly regardless.
fn eager_block_insns_from_env() -> usize {
    std::env::var("WIE_JIT_EAGER_BLOCK_INSNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(48)
}

/// Parse `name` as `u32`, falling back to `default` on absence/invalid input.
///
/// `force_zero_under_test` mirrors the historical `cfg!(test)` short-circuit
/// (hotness knobs are 0 in the unit suite so every block compiles eagerly).
fn env_u32(name: &str, default: u32, force_zero_under_test: bool) -> u32 {
    if force_zero_under_test && cfg!(test) {
        return 0;
    }
    match std::env::var(name) {
        Ok(v) => v.parse::<u32>().unwrap_or(default),
        Err(_) => default,
    }
}

/// Parse `name` as `u64`, falling back to `default` on absence/invalid input.
fn env_u64(name: &str, default: u64) -> u64 {
    match std::env::var(name) {
        Ok(v) => v.parse::<u64>().unwrap_or(default),
        Err(_) => default,
    }
}
