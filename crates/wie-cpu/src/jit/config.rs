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

use super::tier::TIER_BUDGET_DEFAULT;
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
/// - unset / `sticky` (**default**) — direct-mapped refill-table IR + stack pin;
///   helpers use all pin slots (stack / heaps / VirtualAlloc) via `pin_resolve`
/// - `slow` — helper-only loads/stores (oracle / bisect; no host ptr in IR)
/// - `pin` — refill table + stack + **top-2 data pin IR** (heaps/VA); helpers same
///
/// "Sticky" is the historical name of the mode and of its host-side victim ways
/// (`JitCtx::sticky_*`); the inline structure the emitted probe consults is now
/// the 256-slot direct-mapped `JitCtx::refill` table, which replaced the emitted
/// two-way sticky cascade (`docs/architecture/tlb-probe-redesign.md`). The mode
/// name is kept because it is a user-facing env value.
///
/// **Why `sticky` is the default and `pin` is the opt-in.** The data-pin stage
/// adds a third probe ahead of the helper on *every* memory op, and that IR
/// volume is not free: measured on the two load-heavy classes, `pin` costs
/// **25 % on `7za h`** (ratio 0.745, range 0.721–0.750, 6/6 interleaved pairs)
/// and **22.5 % on `7za x`** (0.775, 0.766–0.779, 6/6) with byte-identical
/// output and the same retired guest instruction count. It does cut host
/// load-helper calls 43 %, which is why it looked like a win on the DOOM Retro
/// *startup* helper count (21 M → 2.4 M) that the old default was justified by
/// — a startup counter, never a throughput pairing. Per-compile cost is higher
/// at every block-size bucket (``7za x``: 119/253/519/720/1741/5205/17600 µs for
/// sticky vs 89/194/375/510/1322/3936/14701 for pin at 1/2/4/8/16/32/64 insns),
/// so the loss is IR volume, not scheduling luck.
///
/// `pin` stays reachable for the case that motivated it, and the experiment that
/// would bring it back is a paired run on a real GPU-bound guest: if data-pin IR
/// buys throughput there, it should win that class *on throughput*.
/// See `docs/architecture/performance-ceiling.md` and
/// `docs/architecture/compile-selection-policy.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum JitMemMode {
    Slow,
    Sticky,
    Pin,
}

/// Consolidated JIT configuration, initialized once from the environment.
///
/// The flat bool surface mirrors the one-knob-per-env-var contract (`WIE_JIT_*`);
/// folding the toggles into enums would obscure that mapping for no benefit.
pub(super) struct JitConfig {
    pure_loop_hotness: u32,
    target_work: u64,
    opcode_hist_enabled: bool,
    jit_mem_mode: JitMemMode,
    mem_path_trace: bool,
    /// `WIE_JIT_SUPER=0` — ablation switch for the stack super path.
    ///
    /// The super path is on for self-loop blocks; that is the only shape it
    /// helps, because it emits *two* paths inside one block (bare host memory
    /// vs the sticky/pin probes) and so cannot reduce entry counts, which is
    /// the only thing a memory path could win on. The former `=all` extended
    /// it to every stack-pin-shaped block and measured 1.008 / 0.971 / 0.992 /
    /// 1.061 across the four harness classes (mx9 / mx1 / `x` / `h`) — noise —
    /// and it used to segfault on `7za h` (see `jit/block.rs`); the value was
    /// removed rather than kept as a trap.
    super_disabled: bool,
    chain_enabled: bool,
    ssa_flags_enabled: bool,
    bg_enabled: bool,
    bg_wait_timeout: Duration,
    opt_level: &'static str,
    tier_enabled: bool,
    tier_budget: usize,
    simd_enabled: bool,
    tlb_neon_enabled: bool,
    string_inline_enabled: bool,
    jit_workers: usize,
    verifier_enabled: bool,
    code_cache_enabled: bool,
    /// `WIE_JIT_DUMP_IR=<path>` — append the CLIF of each compiled block to that
    /// file (capped, see `IR_DUMP_BLOCKS`). Off by default.
    ///
    /// This exists because `opt_level = none` means **the emitter is the
    /// optimiser**: every redundant `band`/`ireduce`/`uextend`/`insertlane` the
    /// lowering writes becomes real machine code, and nothing downstream removes
    /// it. Whether a given arithmetic shape emits 5 host ops or 2 is therefore a
    /// question about CLIF, and this is how it gets answered — by counting ops
    /// in the dump rather than reasoning about them.
    ir_dump_path: Option<std::path::PathBuf>,
    /// `WIE_JIT_REGION=1` — compile a hot RIP that is a **loop header** as one
    /// Cranelift function for the whole loop region, with the back edge as a
    /// native `jump`, instead of one function per basic block chained by hops
    /// (`jit/lower/region.rs`, `jit/lower/region_emit.rs`). Off by default; see
    /// [`Self::region_enabled`] for why.
    region_enabled: bool,
    /// `WIE_JIT_MAP=<path>` — append `host_addr guest_start insns` for every
    /// compiled block, so an OS-level sample of this process can be attributed
    /// to guest code. Off by default.
    ///
    /// The JIT's code is anonymous to the host toolchain: `sample(1)` reports
    /// those frames as `<unknown binary>`, and cranelift's symbol registry is
    /// invisible to it. That leaves "where do the retired instructions go?"
    /// answerable only from inside the emulator (a counter in emitted code,
    /// which perturbs the thing being measured) or from this map, which is
    /// written once per compile and costs nothing at run time. Sort by address
    /// and each block's range is `[addr, next_addr)`.
    jit_map_path: Option<std::path::PathBuf>,
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
            // Known pure self-loops: compile sooner (one Cranelift pass vs iced
            // warmup). Default 8; tests 0.
            pure_loop_hotness: env_u32("WIE_JIT_LOOP_HOTNESS", 8, true),
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
                    // Sticky, not pin: pin's data-pin stage measures 22–25 %
                    // *slower* on both load-heavy classes (see the doc comment
                    // on `JitMemMode`). `WIE_JIT_MEM=pin` opts back in.
                    JitMemMode::Sticky
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
            // Off-switch only: the super path is on for self-loop blocks.
            // `=all` was removed — see the `super_disabled` field.
            super_disabled: match std::env::var("WIE_JIT_SUPER") {
                Ok(v) => {
                    let off = v == "0"
                        || v.eq_ignore_ascii_case("off")
                        || v.eq_ignore_ascii_case("false");
                    if !off {
                        // Any other value — including the deleted `all`/`1`/`loop`
                        // — silently means the default. Say so, because an old
                        // measurement arm or CI job setting `=all` would
                        // otherwise look like it ran the super path when it ran
                        // the ordinary one.
                        tracing::warn!(
                            value = %v,
                            "WIE_JIT_SUPER is an off-switch only (0/off/false); \
                             this value selects the default self-loop-only super path"
                        );
                    }
                    off
                }
                Err(_) => false,
            },
            // Late-bound + direct block chaining (`WIE_JIT_CHAIN=0` disables).
            chain_enabled: !matches!(
                std::env::var("WIE_JIT_CHAIN"),
                Ok(v) if v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off")
            ),
            ssa_flags_enabled: !matches!(
                std::env::var("WIE_JIT_SSA_FLAGS"),
                Ok(v) if v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off")
            ),
            // Background compiler worker. Default: on for real runs, off under
            // `cfg(test)` (every block is forced eager there — see the
            // `forced_eager` argument in `pipeline::select_hot_threshold` — so
            // the deterministic inline-compile path keeps the unit suite
            // stable).
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
            // Default `none`: compile cost, not emitted-code quality, is the
            // binding constraint for real guests. 7-Zip Extra `7za.exe i`
            // improves 1.46x cold-ledger / 1.74x warm-ledger, and the
            // foreground `eager` compile count is UNCHANGED at 987 — the win is
            // per-compile cost, not fewer compiles. Background compiles are NOT
            // starved at `speed`: `bg_hit` is 0 on a cold ledger in both arms,
            // and 739 -> 849 (median) warm, i.e. ~15%, not 0 -> 751. The price
            // is compute-bound steady state: `long_loop` regresses ~38%
            // (0.300s -> 0.415s median). `WIE_JIT_OPT=speed` is the documented
            // opt-out for compute-bound guests. Do not "fix" this back to
            // `speed`; hot-block tier-up is the intended recovery and is not
            // implemented yet.
            opt_level: opt_level_from_env(std::env::var("WIE_JIT_OPT").ok()),
            // Per-block opt-level tier-up (`WIE_JIT_TIER`, default on; `0`
            // restores the pre-tiering behaviour exactly — one module, one
            // opt level, every block at the base level).
            tier_enabled: tier_enabled_from_env(std::env::var("WIE_JIT_TIER").ok()),
            // Cap on tier-up decisions per run (`WIE_JIT_TIER_BUDGET`).
            tier_budget: tier_budget_from_env(std::env::var("WIE_JIT_TIER_BUDGET").ok()),
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
            // Cranelift IR verifier (`WIE_JIT_VERIFIER`). Profile-dependent
            // default: ON in debug builds (the per-function CLIF check is the
            // only thing that makes an ill-typed lowering fail loudly instead
            // of miscompiling silently, and nextest runs the dev profile, so CI
            // keeps that guard), OFF in release (measured -37% 7-Zip boot
            // wall-clock, compile work bit-identical). An explicit value wins
            // in both profiles in both directions. Caveat: nothing in the repo
            // records the verifier ever firing, so "the suite is green with it
            // off" means *no known bug is masked*, not *it is redundant* — and it
            // never checked x86 semantics, which is the JIT-vs-iced
            // differential's job, not its.
            verifier_enabled: verifier_enabled_from_env(std::env::var("WIE_JIT_VERIFIER").ok())
                .unwrap_or(cfg!(debug_assertions)),
            // Persist emitted machine code alongside the metadata ledger
            // (`WIE_JIT_CODE_CACHE`). DEFAULT OFF, and it is the only knob in
            // this file that is off by default rather than on: a restored block
            // is *replayed host code*, so any hole in the capture/restore
            // round-trip is a wrong-answer or branch-into-garbage bug rather
            // than a slowdown. Everything it does has a cold fallback, so the
            // opt-out is a pure correctness switch, not a perf switch.
            code_cache_enabled: code_cache_enabled_from_env(
                std::env::var("WIE_JIT_CODE_CACHE").ok(),
            ),
            // Loop-region compilation (`WIE_JIT_REGION=1`). Parsed as a plain
            // opt-in, NOT forced off under `cfg(test)`: the unit tests that
            // exercise the region emitter need it on, and they flip the
            // per-`JitShared` latch (`JitShared::region_force`) rather than this
            // process-wide value, because `JitConfig` is a `OnceLock` read once
            // per process (see `region_enabled`).
            region_enabled: matches!(
                std::env::var("WIE_JIT_REGION"),
                Ok(v)
                    if v == "1"
                        || v.eq_ignore_ascii_case("true")
                        || v.eq_ignore_ascii_case("on")
                        || v.eq_ignore_ascii_case("yes")
            ),
            ir_dump_path: std::env::var_os("WIE_JIT_DUMP_IR")
                .filter(|p| !p.is_empty())
                .map(std::path::PathBuf::from),
            jit_map_path: std::env::var_os("WIE_JIT_MAP")
                .filter(|p| !p.is_empty())
                .map(std::path::PathBuf::from),
        }
    }

    /// Whether persisted machine code may be restored (`WIE_JIT_CODE_CACHE`).
    ///
    /// Default **off**. `WIE_JIT_CACHE` (the metadata ledger) is independent and
    /// stays on by default; this knob only governs the code half.
    #[must_use]
    pub(super) fn code_cache_enabled(&self) -> bool {
        self.code_cache_enabled
    }

    /// Whether a hot loop header compiles as a **loop region** — one Cranelift
    /// function for the whole loop, with the back edge as a native `jump`
    /// (`WIE_JIT_REGION=1`).
    ///
    /// # Why the default is off
    ///
    /// Not for want of a reason to turn it on, but because of what a default
    /// *means* here: every other default in this file was chosen by a paired
    /// measurement on the calibrated corpus (`scripts/real-guest-perf.sh`), and a
    /// region changes the shape of emitted code for whole loops rather than
    /// adding a probe to one path. The measured evidence for it is one loop in
    /// one guest (DOOM Retro's 2-block, 36-instruction inner loop — the
    /// `frames_published` arm in `docs/RUNBOOK.md`), on the one host that could
    /// run it; and the emitter's exit-store set is deliberately conservative
    /// (see `lower::region_emit`) in a way that a wider corpus might want
    /// tightened before it is everybody's default. Anything that can silently
    /// miscompile instead of slowing down stays behind a knob until a corpus says
    /// otherwise — the same rule `WIE_JIT_CODE_CACHE` follows, and the opposite
    /// of how `WIE_JIT_MEM`'s default was justified (that one had 6/6 paired
    /// runs behind it).
    ///
    /// The other half: a region is *one* compilation unit for up to
    /// `RegionLimits::max_blocks` blocks, so its compile cost is paid up front
    /// against a visit threshold calibrated for a single block. That trade is
    /// what a default would be asserting, and one guest is not a corpus.
    #[must_use]
    pub(super) fn region_enabled(&self) -> bool {
        self.region_enabled
    }

    /// Fingerprint of everything that changes the *emitted code* for the same
    /// guest bytes — the identity a persisted code blob must match.
    ///
    /// The metadata ledger only ever records "these bytes compiled OK", which is
    /// a claim about a compiler configuration but not about a byte layout. A
    /// persisted code blob is a claim about the exact instructions emitted, so
    /// it additionally needs every knob that changes lowering, plus the build
    /// and target shape. `WIE_JIT_VERSION` guards the WIE version already;
    /// this guards everything the version cannot see (a knob flip, a debug vs
    /// release build, a different host).
    ///
    /// Deliberately a *separate* key component from
    /// [`Self::opt_level`]: opt level is already folded into the ledger key, but
    /// a knob flip must not invalidate a user's metadata ledger, and folding it
    /// into the code-cache key instead keeps the two files independent.
    /// Bump when `JitCtx`'s field ORDER changes without changing its size:
    /// emitted code bakes every field offset into the block, so a persisted
    /// blob is only valid against the layout it was compiled with.
    ///
    /// 2 — the shadow return stack (`shadow_sp`, `shadow_ret`) was removed.
    /// 3 — the direct-mapped refill table (`refill: [RefillSlot; 256]`) and
    ///     `mem_path_on` were appended; the emitted mem probe now indexes
    ///     `OFF_REFILL` with a 32-byte slot stride instead of walking
    ///     `sticky_page` / `sticky_ptr` / `sticky_prot` / `sticky_gen`.
    pub(super) const FRAME_LAYOUT_VERSION: u64 = 3;

    /// Where to append CLIF dumps, if `WIE_JIT_DUMP_IR` names a file.
    #[must_use]
    pub(super) fn ir_dump_path(&self) -> Option<&std::path::Path> {
        self.ir_dump_path.as_deref()
    }

    /// Where to append the host-address → guest-block map, if `WIE_JIT_MAP`
    /// names a file.
    #[must_use]
    pub(super) fn jit_map_path(&self) -> Option<&std::path::Path> {
        self.jit_map_path.as_deref()
    }

    #[must_use]
    pub(super) fn emit_fingerprint(&self) -> u64 {
        fn mix(h: u64, v: u64) -> u64 {
            let h = h ^ v
                .wrapping_add(0x9e37_79b9_7f4a_7c15)
                .wrapping_add(h << 6)
                .wrapping_add(h >> 2);
            h.wrapping_mul(0x0000_0100_0000_01b3)
        }
        let mut h = 0xcbf2_9ce4_8422_2325_u64;
        h = mix(h, self.opt_level.len() as u64);
        for b in self.opt_level.as_bytes() {
            h = mix(h, u64::from(*b));
        }
        // Lowering-shape knobs. Order is fixed; each contributes a distinct bit
        // so no two knob combinations collide by construction.
        h = mix(h, self.jit_mem_mode as u64);
        h = mix(h, u64::from(self.super_disabled));
        h = mix(h, u64::from(self.chain_enabled));
        h = mix(h, u64::from(self.ssa_flags_enabled));
        h = mix(h, u64::from(self.simd_enabled));
        h = mix(h, u64::from(self.string_inline_enabled));
        h = mix(h, u64::from(self.verifier_enabled));
        // Loop-region compilation. It belongs in the *emitter* fingerprint and
        // not in `FRAME_LAYOUT_VERSION`, because it changes neither `JitCtx` nor
        // the frame ABI: it changes which guest range one function covers. A
        // region compiled at VA V is a different function (different
        // `insn_count`, different `guest_end`, a native back edge) than the
        // per-block translation of V, so a blob recorded under one mode must not
        // be restored under the other. Keying the `.code` file on this bit — the
        // same treatment `chain_enabled` gets — makes the two modes separate
        // files instead of one file with two meanings per key.
        h = mix(h, u64::from(self.region_enabled));
        // Build + host shape. `cfg!` keeps this a compile-time constant, so a
        // debug-built test process can never accept a release blob.
        h = mix(h, u64::from(cfg!(debug_assertions)));
        h = mix(h, std::mem::size_of::<usize>() as u64);
        // Frame layout. Emitted code carries every `JitCtx` field offset as an
        // immediate (`OFF_*`), so a blob compiled against a different struct
        // layout does not merely miss — it reads and writes the wrong fields,
        // at full speed, with no diagnostic. Size catches the common case
        // (add/remove a field); `FRAME_LAYOUT_VERSION` must be bumped by hand
        // for a change that reorders fields without moving the size.
        h = mix(h, std::mem::size_of::<super::lower::JitCtx>() as u64);
        h = mix(h, Self::FRAME_LAYOUT_VERSION);
        h = mix(h, u64::from(u8::MAX)); // fingerprint-format v1 marker
        h
    }

    /// Known pure self-loops: compile sooner (one Cranelift pass vs iced
    /// warmup). Default 8; tests 0.
    #[must_use]
    pub(super) fn pure_loop_hotness(&self) -> u32 {
        self.pure_loop_hotness
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

    /// Whether stack-super IR is allowed for this block: self-loops only, and
    /// not under `WIE_JIT_SUPER=0`.
    #[must_use]
    pub(crate) fn super_enabled(&self, self_loop: bool) -> bool {
        self_loop && !self.super_disabled
    }

    /// Late-bound + direct block chaining (`WIE_JIT_CHAIN=0` disables).
    #[must_use]
    pub(super) fn chain_enabled(&self) -> bool {
        self.chain_enabled
    }

    #[must_use]
    pub(super) fn ssa_flags_enabled(&self) -> bool {
        self.ssa_flags_enabled
    }

    /// Background compiler worker. Default: on for real runs, off under
    /// `cfg(test)` (every block is forced eager there, so the deterministic
    /// inline-compile path keeps the unit suite stable).
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
    ///
    /// Default `none`: see the measured trade-off on the field initializer in
    /// [`JitConfig::from_env`]. `WIE_JIT_OPT=speed` opts back in.
    #[must_use]
    pub(super) fn opt_level(&self) -> &'static str {
        self.opt_level
    }

    /// Per-block opt-level tier-up (`WIE_JIT_TIER`, default on).
    ///
    /// This is the bisect switch for the whole tiering mechanism: `0` builds
    /// one Cranelift module at the base opt level and makes every block a
    /// `OptTier::Base` decision, which is byte-for-byte the pre-tiering
    /// behaviour.
    #[must_use]
    pub(super) fn tier_enabled(&self) -> bool {
        self.tier_enabled
    }

    /// Cap on tier-up decisions per run (`WIE_JIT_TIER_BUDGET`, default
    /// [`TIER_BUDGET_DEFAULT`]). Consumed at decision time, so a pathological
    /// guest cannot spend the run's compile budget re-establishing `speed`.
    #[must_use]
    pub(super) fn tier_budget(&self) -> usize {
        self.tier_budget
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

    /// Whether Cranelift runs its per-function IR verifier
    /// (`WIE_JIT_VERIFIER`; default `cfg!(debug_assertions)` — on in debug,
    /// off in release).
    ///
    /// The verifier is the per-function CLIF check (SSA/dominance, type
    /// correctness, use-before-def, operand constraints) and is the only thing
    /// that turns a lowering bug into a diagnosable error instead of wrong host
    /// code. It never checked x86 *semantics*: the JIT-vs-iced differential is
    /// the semantic gate, so its cost buys type/SSA safety, not correctness of
    /// the lowering's meaning. Defaulting it to `debug_assertions` keeps that
    /// net over the dev-profile test suite (so `cargo nextest` — the profile CI
    /// uses — compiles with the verifier on) while not charging release
    /// guests for it. An explicit `WIE_JIT_VERIFIER=1` in a release build
    /// re-arms it for diagnosis.
    ///
    /// Honest caveat, unchanged by that default: nothing in this repo records
    /// the verifier ever firing, so a green suite with it off means *no known
    /// bug is masked*, **not** that the verifier is redundant.
    #[must_use]
    pub(crate) fn verifier_enabled(&self) -> bool {
        self.verifier_enabled
    }
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

/// Default Cranelift `opt_level`: compile cheap, pay for `speed` only where
/// hotness earns it.
const OPT_LEVEL_DEFAULT: &str = "none";

/// Parse the Cranelift `opt_level` from a `WIE_JIT_OPT` value.
///
/// Split out of [`JitConfig::from_env`] so the whole decision table is
/// unit-testable without mutating the process environment: [`JitConfig`] lives
/// behind a process-wide `OnceLock`, so an in-test `set_var` could neither be
/// undone nor observed, and `set_var` is `unsafe` under this crate's lint set
/// anyway. Same rationale as `cache_persist::resolve_config`.
///
/// Unset, empty, and unrecognised values all take [`OPT_LEVEL_DEFAULT`].
fn opt_level_from_env(raw: Option<String>) -> &'static str {
    match raw {
        Some(v) if v.eq_ignore_ascii_case("none") || v == "0" => "none",
        Some(v)
            if v.eq_ignore_ascii_case("speed_and_size")
                || v.eq_ignore_ascii_case("size")
                || v.eq_ignore_ascii_case("speed-and-size") =>
        {
            "speed_and_size"
        }
        Some(v) if v.eq_ignore_ascii_case("speed") || v.eq_ignore_ascii_case("fast") => "speed",
        _ => OPT_LEVEL_DEFAULT,
    }
}

/// Parse the per-block tier-up switch from a `WIE_JIT_TIER` value.
///
/// Default on: tiering is the mechanism that recovers the compute-bound case
/// the cheap-compile default gives up. Only the explicit off-spellings
/// (`0` / `false` / `off` / `no`) disable it — the bisect switch, so an
/// unrecognised value must NOT silently disable a feature whose absence looks
/// like a 38% perf regression.
fn tier_enabled_from_env(raw: Option<String>) -> bool {
    !matches!(raw, Some(v) if v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("no"))
}

/// Ceiling on [`TIER_BUDGET_DEFAULT`]; an override above it is clamped.
const TIER_BUDGET_MAX: usize = 65_536;

/// Parse the per-run tier-up budget from a `WIE_JIT_TIER_BUDGET` value.
///
/// Absent or unparsable → [`TIER_BUDGET_DEFAULT`]. `0` is a legal value and
/// disables tiering (no block may consume budget, so none tiers up) — that is
/// the difference between "off, and here is the evidence" and the `WIE_JIT_TIER`
/// switch itself, which is the off-switch for the *mechanism*.
fn tier_budget_from_env(raw: Option<String>) -> usize {
    raw.and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(TIER_BUDGET_DEFAULT)
        .min(TIER_BUDGET_MAX)
}

/// Parse the Cranelift IR-verifier switch from a `WIE_JIT_VERIFIER` value.
///
/// Returns the **explicit** choice only; unset, empty and unrecognised values
/// are `None` so the caller can apply the profile default
/// (`cfg!(debug_assertions)`) instead of baking one build's answer into the
/// parser. Both directions are spellable in both profiles on purpose: `0` in a
/// debug build is how you stop paying the verifier's cost while bisecting a JIT
/// bug, and `1` in a release build is the confirmation path for a suspected
/// miscompile. Pure, for the same reason as [`tier_enabled_from_env`] and
/// [`opt_level_from_env`] — no `set_var`, so the whole table is unit-testable.
fn verifier_enabled_from_env(raw: Option<String>) -> Option<bool> {
    match raw {
        Some(v)
            if v == "0"
                || v.eq_ignore_ascii_case("false")
                || v.eq_ignore_ascii_case("off")
                || v.eq_ignore_ascii_case("no") =>
        {
            Some(false)
        }
        Some(v)
            if v == "1"
                || v.eq_ignore_ascii_case("true")
                || v.eq_ignore_ascii_case("on")
                || v.eq_ignore_ascii_case("yes") =>
        {
            Some(true)
        }
        _ => None,
    }
}

/// Parse the persisted-machine-code switch from a `WIE_JIT_CODE_CACHE` value.
///
/// **Default off.** Opt-in rather than opt-out, unlike every other knob here:
/// restoring persisted code replays host instructions captured by a previous
/// process, so the failure mode of a bug in it is "guest executes the wrong
/// thing" rather than "guest runs slower". Pure (no `set_var`) so the table is
/// unit-testable, and it also refuses under `cfg(test)`/nextest for the same
/// reason the ledger does — see [`super::cache_persist`]'s
/// `under_test_process` note about processes sharing one on-disk file.
fn code_cache_enabled_from_env(raw: Option<String>) -> bool {
    // A test process never restores persisted code: the round-trip is exercised
    // by an explicit, directory-scoped test instead of by the ambient cache.
    if cfg!(test) || std::env::var_os("NEXTEST").is_some() {
        return false;
    }
    matches!(
        raw,
        Some(v)
            if v == "1"
                || v.eq_ignore_ascii_case("true")
                || v.eq_ignore_ascii_case("on")
                || v.eq_ignore_ascii_case("yes")
    )
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{
        OPT_LEVEL_DEFAULT, opt_level_from_env, tier_budget_from_env, tier_enabled_from_env,
        verifier_enabled_from_env,
    };

    #[test]
    fn opt_level_defaults_to_none() {
        // The cheap-compile default is the load-bearing policy decision of
        // this change; see the `from_env` comment for the measurements.
        assert_eq!(OPT_LEVEL_DEFAULT, "none");
        assert_eq!(opt_level_from_env(None), "none");
    }

    #[test]
    fn opt_level_honours_explicit_values() {
        for (raw, want) in [
            ("none", "none"),
            ("NONE", "none"),
            ("None", "none"),
            ("0", "none"),
            ("speed", "speed"),
            ("SPEED", "speed"),
            ("fast", "speed"),
            ("speed_and_size", "speed_and_size"),
            ("SPEED_AND_SIZE", "speed_and_size"),
            ("size", "speed_and_size"),
            ("speed-and-size", "speed_and_size"),
        ] {
            assert_eq!(
                opt_level_from_env(Some(raw.to_string())),
                want,
                "WIE_JIT_OPT={raw}"
            );
        }
    }

    #[test]
    fn opt_level_falls_back_to_none_on_junk() {
        // Unknown / empty spellings must not silently re-enable `speed`.
        for raw in ["", "  ", "turbo", "None ", "1", "nonee"] {
            assert_eq!(
                opt_level_from_env(Some(raw.to_string())),
                "none",
                "WIE_JIT_OPT={raw:?} must take the cheap-compile default"
            );
        }
    }

    #[test]
    fn every_parsed_level_is_a_cranelift_opt_level() {
        // Cranelift rejects an unknown string at `JitEngine::new` time, which
        // would turn a typo into a hard startup failure rather than a default.
        for want in ["none", "speed", "speed_and_size"] {
            assert_eq!(opt_level_from_env(Some(want.to_string())), want);
        }
    }

    /// `WIE_JIT_TIER=0` must be the bisect switch: it restores the
    /// pre-tiering behaviour exactly, so it has to be reachable by every
    /// off-spelling a bisect might use.
    #[test]
    fn tier_switch_defaults_on_and_only_explicit_off_disables() {
        assert!(tier_enabled_from_env(None), "default is on");
        assert!(tier_enabled_from_env(Some(String::new())));
        assert!(tier_enabled_from_env(Some("1".into())));
        assert!(tier_enabled_from_env(Some("true".into())));
        assert!(
            tier_enabled_from_env(Some("turbo".into())),
            "junk must not disable"
        );
        for off in ["0", "false", "FALSE", "off", "OFF", "no", "No"] {
            assert!(
                !tier_enabled_from_env(Some(off.into())),
                "WIE_JIT_TIER={off}"
            );
        }
    }

    /// A zero budget is a legal "tier nothing" setting; junk takes the default
    /// rather than silently meaning zero.
    #[test]
    fn tier_budget_parses_zero_default_and_junk() {
        assert_eq!(tier_budget_from_env(None), super::TIER_BUDGET_DEFAULT);
        assert_eq!(tier_budget_from_env(Some("0".into())), 0);
        assert_eq!(tier_budget_from_env(Some("8".into())), 8);
        assert_eq!(
            tier_budget_from_env(Some("junk".into())),
            super::TIER_BUDGET_DEFAULT
        );
        // Clamped, never wrapped.
        assert_eq!(
            tier_budget_from_env(Some("999999999".into())),
            super::TIER_BUDGET_MAX
        );
    }

    /// The verifier default is **profile-dependent**, so the test is too: it
    /// asserts the mapping and the profile default separately rather than
    /// baking in one build's answer. `nextest` runs the dev profile, so CI
    /// compiles with the verifier on; release builds run it off.
    #[test]
    fn verifier_switch_defaults_to_profile_and_explicit_values_win() {
        let profile = cfg!(debug_assertions);

        // Unset, empty, whitespace and unrecognised junk mean "no explicit
        // choice" (`None`, for the caller to default) — never a silent
        // disable, which is the one failure mode that matters here.
        for raw in [
            None,
            Some(String::new()),
            Some("  ".into()),
            Some("turbo".into()),
            Some("offe".into()),
            Some("none".into()),
        ] {
            assert_eq!(
                verifier_enabled_from_env(raw.clone()),
                None,
                "WIE_JIT_VERIFIER={raw:?} must not be read as an explicit choice"
            );
            // …and the call site's collapse of that `None` is the profile
            // default, in whichever profile this test was compiled under.
            assert_eq!(
                verifier_enabled_from_env(raw.clone()).unwrap_or(cfg!(debug_assertions)),
                profile,
                "WIE_JIT_VERIFIER={raw:?} must fall back to the profile default \
                 (debug_assertions={profile})"
            );
        }

        // Every off-spelling disables it in *any* profile — including a debug
        // build, which is where you turn it off while bisecting a JIT bug.
        for off in ["0", "false", "FALSE", "off", "OFF", "no", "No"] {
            assert_eq!(
                verifier_enabled_from_env(Some(off.into())),
                Some(false),
                "WIE_JIT_VERIFIER={off} must disable the verifier regardless of profile"
            );
            assert!(
                !verifier_enabled_from_env(Some(off.into())).unwrap_or(cfg!(debug_assertions)),
                "WIE_JIT_VERIFIER={off} must override the profile default"
            );
        }

        // …and every on-spelling arms it in *any* profile, including release:
        // this is the confirmation path for a suspected miscompile.
        for on in ["1", "true", "TRUE", "on", "ON", "yes", "Yes"] {
            assert_eq!(
                verifier_enabled_from_env(Some(on.into())),
                Some(true),
                "WIE_JIT_VERIFIER={on} must arm the verifier regardless of profile"
            );
            assert!(
                verifier_enabled_from_env(Some(on.into())).unwrap_or(cfg!(debug_assertions)),
                "WIE_JIT_VERIFIER={on} must override the profile default"
            );
        }
    }
}
