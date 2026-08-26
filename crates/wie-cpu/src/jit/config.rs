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

/// Work-weighted promotion target (`WIE_JIT_TARGET_WORK`, default 900).
///
/// A block of N instructions promotes after `clamp(TARGET_WORK / N, FLOOR,
/// CEILING)` visits, i.e. when accumulated interpreted work exceeds the
/// predicted compile cost × margin. 900 keeps a 9-insn block at ≈100 visits —
/// the historical flat-threshold behavior — while a 90-insn block now needs
/// only ~10 revisits to justify its ~10× larger compile cost.
pub(super) const WORK_TARGET_DEFAULT: u64 = 900;

/// Lower clamp on the size-aware visit threshold: even a huge block promotes
/// after this many revisits (compile cost never justifies waiting longer).
pub(super) const WORK_THRESHOLD_FLOOR: u32 = 8;

/// Upper clamp on the size-aware visit threshold: a tiny block never waits
/// longer than this, however cheap its compile would be.
pub(super) const WORK_THRESHOLD_CEILING: u32 = 10_000;

/// Hysteresis cap for cooldown thresholds: each wait timeout doubles the
/// block's threshold, capped at CEILING×4 so a pathological block cannot
/// defer its promotion forever.
pub(super) const COOLDOWN_THRESHOLD_CAP: u32 = WORK_THRESHOLD_CEILING * 4;

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
    target_work: u64,
    opcode_hist_enabled: bool,
    jit_mem_mode: JitMemMode,
    mem_path_trace: bool,
    super_mode: SuperMode,
    chain_enabled: bool,
    bg_enabled: bool,
    bg_wait_timeout: Duration,
    opt_level: &'static str,
    simd_enabled: bool,
    tlb_neon_enabled: bool,
    string_inline_enabled: bool,
    jit_workers: usize,
}

/// Bounds of the background worker-pool size (`WIE_JIT_WORKERS`).
///
/// One worker per pool is the floor (a zero-worker pool would strand every
/// queued job); four is the ceiling because Cranelift compiles are
/// CPU-bound and guest threads need the remaining cores for execution.
const JIT_WORKERS_MIN: usize = 1;
const JIT_WORKERS_MAX: usize = 4;

/// Default background worker-pool size: half the reported parallelism.
///
/// Workers exist to overlap Cranelift compiles with guest execution, not to
/// saturate the machine; halving leaves the other half of the cores to the
/// 1:1 guest threads. Clamped so a single-core host still gets one worker.
fn jit_workers_default() -> usize {
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    (cores / 2).clamp(JIT_WORKERS_MIN, JIT_WORKERS_MAX)
}

/// Parse the background worker-pool size from `WIE_JIT_WORKERS`.
///
/// Absent or invalid input falls back to [`jit_workers_default`]; explicit
/// values clamp into `[JIT_WORKERS_MIN, JIT_WORKERS_MAX]` like the siblings.
fn jit_workers_from_env() -> usize {
    match std::env::var("WIE_JIT_WORKERS") {
        Ok(v) => v.parse::<usize>().map_or_else(
            |_| jit_workers_default(),
            |n| n.clamp(JIT_WORKERS_MIN, JIT_WORKERS_MAX),
        ),
        Err(_) => jit_workers_default(),
    }
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
            // `eager_block_insns_from_env`). Default 0 (disabled — the size
            // rule lost its arithmetic justification, see perf-plan §4);
            // `>0` re-enables for diagnosis.
            eager_block_insns: eager_block_insns_from_env(),
            // Work-weighted promotion target (`WIE_JIT_TARGET_WORK`).
            target_work: target_work_from_env(),
            // Sampled opcode histogram over the iced residue
            // (`WIE_JIT_OPCODE_HISTO=1`); surfaced by the profile dump.
            opcode_hist_enabled: matches!(
                std::env::var("WIE_JIT_OPCODE_HISTO"),
                Ok(v) if v == "1" || v.eq_ignore_ascii_case("true")
            ),
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
            // Background worker-pool size (`WIE_JIT_WORKERS`).
            jit_workers: jit_workers_from_env(),
        }
    }

    /// Hotness regime switch. Nonzero selects work-weighted promotion (the
    /// numeric value is no longer the visit count itself); `0` — forced under
    /// `cfg(test)` — means "compile everything eagerly on first sight" so the
    /// unit suite stays byte-for-byte deterministic.
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
    /// first sight instead of waiting out the visit threshold. Default 0
    /// (disabled): a one-shot large block costs far more compiled than
    /// interpreted whenever it does not revisit, so eager-by-size is a loss —
    /// set `WIE_JIT_EAGER_BLOCK_INSNS` to re-enable for diagnosis.
    #[must_use]
    pub(super) fn eager_block_insns(&self) -> usize {
        self.eager_block_insns
    }

    /// Work-weighted promotion target: a block of N instructions promotes
    /// after `clamp(target_work / N, WORK_THRESHOLD_FLOOR,
    /// WORK_THRESHOLD_CEILING)` visits (`WIE_JIT_TARGET_WORK`).
    #[must_use]
    pub(super) fn target_work(&self) -> u64 {
        self.target_work
    }

    /// Sampled opcode histogram over the iced residue (`WIE_JIT_OPCODE_HISTO=1`).
    #[must_use]
    pub(super) fn opcode_hist_enabled(&self) -> bool {
        self.opcode_hist_enabled
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

    /// Background worker-pool size (`WIE_JIT_WORKERS`, default ≈ cores/2,
    /// clamped to [1, 4]).
    #[must_use]
    pub(super) fn jit_workers(&self) -> usize {
        self.jit_workers
    }
}

/// Upper bound on the fixed hotness threshold (`WIE_JIT_HOTNESS_THRESHOLD`).
///
/// Clamping keeps an experimental override from deferring a genuinely hot block
/// forever; the default (100) is far below this cap.
const HOTNESS_THRESHOLD_MAX: u32 = 1_000_000;

/// Parse the hotness regime switch from `WIE_JIT_HOTNESS_THRESHOLD`.
///
/// Nonzero (default 100) selects work-weighted promotion; the value itself no
/// longer sets a visit count. `0` forces eager-everything. Under `cfg(test)`
/// the switch is 0 so every block compiles eagerly on first sight
/// (deterministic unit suite).
fn hotness_threshold_from_env() -> u32 {
    if cfg!(test) {
        return 0;
    }
    let raw = std::env::var("WIE_JIT_HOTNESS_THRESHOLD")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(100);
    raw.clamp(0, HOTNESS_THRESHOLD_MAX)
}

/// Eager-compile cutoff for large one-shot Pure blocks (`WIE_JIT_EAGER_BLOCK_INSNS`).
///
/// Default 0: disabled. The original rationale failed arithmetic — a one-shot
/// 96-insn block costs ~9 µs interpreted vs ~1 ms compiled, so forcing a
/// first-sight compile is a ~100× loss whenever the block does not revisit.
/// A positive value re-enables the rule for diagnosis. The decision cost is a
/// usize compare during decode; when the regime switch is already `0` (unit
/// suite) the block compiles eagerly regardless.
fn eager_block_insns_from_env() -> usize {
    std::env::var("WIE_JIT_EAGER_BLOCK_INSNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0)
}

/// Parse the work-weighted promotion target from `WIE_JIT_TARGET_WORK`.
///
/// Default 900 (keeps a 9-insn block at ≈100 visits). Clamped to a sane range
/// so an absurd override cannot pin every threshold at a clamp boundary.
fn target_work_from_env() -> u64 {
    let raw = std::env::var("WIE_JIT_TARGET_WORK")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(WORK_TARGET_DEFAULT);
    raw.clamp(16, 10_000_000)
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
