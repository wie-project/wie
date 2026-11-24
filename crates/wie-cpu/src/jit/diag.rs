//! JIT diagnostics reporting: mem-path histogram, background-promotion
//! ledger, and the sampled iced-residue opcode histogram.
//!
//! Split out of `jit/mod.rs` (file-size policy, ADR-002); the public surface
//! is re-exported unchanged at `crate::jit`.

use super::JitStats;
use super::config::JitConfig;
use super::pipeline;

/// Dump mem-path histogram when `WIE_JIT_MEM_TRACE=1` or `WIE_EXEC_TRACE=1`.
///
/// Also surfaces two Phase-0 counters through the same call site (the runtime
/// profile's `finalize_profile` invokes this unconditionally, and each section
/// carries its own gate):
/// - the background-promotion outcome ledger (printed whenever any outcome
///   was recorded),
/// - the sampled iced-residue opcode histogram (`WIE_JIT_OPCODE_HISTO=1`).
pub fn dump_mem_path_stats(s: &JitStats) {
    if JitConfig::get().mem_path_trace_enabled() {
        dump_mem_path_histogram(s);
    }
    // Promotion outcome ledger: per-enqueue resolution of the background
    // compile pipeline. Diagnostic-only here (explicit trace knobs) — the
    // runtime profile report carries it on every profiling path, so a bare
    // run stays silent. An ERROR-styled line for routine bookkeeping reads
    // as a failure that did not happen.
    if JitConfig::get().mem_path_trace_enabled()
        && let Some(line) = bg_ledger_line(s)
    {
        tracing::error!("{line}");
    }
    // Sampled opcode histogram over the interpreted residue (opt-in).
    if JitConfig::get().opcode_hist_enabled() {
        dump_opcode_histogram();
    }
}

/// Background-promotion outcome ledger rendered as one report line.
///
/// `None` while nothing was recorded (quiet by default).
fn bg_ledger_line(s: &JitStats) -> Option<String> {
    let ledger_total = s
        .promo
        .hit_ready
        .saturating_add(s.promo.stalled_ok)
        .saturating_add(s.promo.timed_out)
        .saturating_add(s.promo.cooled_down)
        .saturating_add(s.promo.deferred);
    if ledger_total == 0 {
        return None;
    }
    Some(format!(
        "[wie] jit_bg_ledger: workers={} hit_ready={} stalled_ok={} timed_out={} cooled_down={} deferred={} (total={ledger_total})",
        s.bg.workers,
        s.promo.hit_ready,
        s.promo.stalled_ok,
        s.promo.timed_out,
        s.promo.cooled_down,
        s.promo.deferred
    ))
}

/// Report lines for the `=== WIE_RUNTIME_PROFILE ===` block: the promotion
/// ledger and the sampled iced-residue opcode histogram
/// (`WIE_JIT_OPCODE_HISTO=1`). Rendered into the report itself so they print
/// on every profile path — SIGINT included — without depending on a tracing
/// subscriber being installed (tracing is silent under a default RUST_LOG).
#[must_use]
pub fn jit_profile_report_lines(s: &JitStats) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(line) = bg_ledger_line(s) {
        out.push(line);
    }
    if let Some(line) = chain_stats_line(s) {
        out.push(line);
    }
    if let Some(line) = tier_line(s) {
        out.push(line);
    }
    if JitConfig::get().opcode_hist_enabled() {
        out.extend(opcode_histogram_lines());
    }
    out
}

/// Direct-chaining health as one report line (G5): epoch-advance rate,
/// resync width, and the chain-edge volume (`hops` / `store_ops`).
///
/// The first three describe the chain TABLE (how often it is refreshed, how
/// wide it is); `hops` is the hit rate — edges actually taken in native code —
/// and `store_ops` the GPR writes those edges performed. Keep them on one line
/// so `hops` is never read next to `avg_width` alone and mistaken for it.
///
/// `None` while nothing was recorded.
fn chain_stats_line(s: &JitStats) -> Option<String> {
    if s.chain.epoch_bumps == 0 && s.chain.resyncs == 0 && s.chain.hops == 0 {
        return None;
    }
    let width = s
        .chain
        .resync_entries
        .checked_div(s.chain.resyncs)
        .unwrap_or(0);
    // Same derived-ratio shape as `avg_width`: two decimal-free integer means,
    // 0 when the denominator is 0.
    let stores_per_hop = s.chain.store_ops.checked_div(s.chain.hops).unwrap_or(0);
    Some(format!(
        "[wie] jit_chain: epoch_bumps={} resyncs={} avg_width={width} inline_inserts={} code_invs={} hops={} hop_store_ops={} stores_per_hop={stores_per_hop}",
        s.chain.epoch_bumps,
        s.chain.resyncs,
        s.chain.inline_inserts,
        s.exec.code_invs,
        s.chain.hops,
        s.chain.store_ops
    ))
}

/// Opt-level tier-up ledger as one report line: how many blocks earned `speed`
/// (self-loops), how many tier compiles the verifier rejected, and how much of
/// the run's budget is left. Reported separately from `compile_us` so the win
/// stays attributable to per-compile cost rather than to the tier-up count.
///
/// `None` when no block tiered up and nothing was rejected.
fn tier_line(s: &JitStats) -> Option<String> {
    if s.profile.tier_compiles == 0 && s.profile.tier_rejects == 0 {
        return None;
    }
    Some(format!(
        "[wie] jit_tier: tier_compiles={} tier_rejects={} budget_left={}",
        s.profile.tier_compiles, s.profile.tier_rejects, s.profile.tier_budget_left
    ))
}

fn dump_mem_path_histogram(s: &JitStats) {
    let helpers = s.mem.load_calls.saturating_add(s.mem.store_calls);
    tracing::error!(
        "[wie] mem_path helpers={helpers} load={} store={}",
        s.mem.load_calls,
        s.mem.store_calls
    );
    tracing::error!(
        "[wie]   resolve: sticky={} multi={} pin={} walk={} cross={} slow={}",
        s.mem.sticky_hit,
        s.mem.multi_hit,
        s.mem.pin_hit,
        s.mem.walk_hit,
        s.mem.cross_page,
        s.mem.slow
    );
    tracing::error!(
        "[wie]   sticky_miss: key={} gen={} prot={} swaps={}",
        s.mem.sticky_miss_key,
        s.mem.sticky_miss_gen,
        s.mem.sticky_miss_prot,
        s.mem.sticky_swaps
    );
    tracing::error!(
        "[wie]   addr_vs_pin: stack={} heap={} outside={}",
        s.mem.addr_stack_pin,
        s.mem.addr_heap_pin,
        s.mem.addr_outside
    );
    tracing::error!(
        "[wie]   gen: bumps={} peak={}  pins: stack_bytes={:#x} heap_bytes={:#x} allow={:#x}",
        s.mem.gen_bumps,
        s.mem.gen_peak,
        s.mem.pin_stack_bytes,
        s.mem.pin_heap_bytes,
        s.mem.pin_allow_bits
    );
    if helpers > 0 {
        let pct10 = |n: u64| -> u64 { n.saturating_mul(1000).checked_div(helpers).unwrap_or(0) };
        let fmt = |n: u64| {
            let t = pct10(n);
            format!("{}.{}", t.checked_div(10).unwrap_or(0), t % 10)
        };
        tracing::error!(
            "[wie]   resolve%: multi={}% pin={}% walk={}% key_miss={}% outside={}%",
            fmt(s.mem.multi_hit),
            fmt(s.mem.pin_hit),
            fmt(s.mem.walk_hit),
            fmt(s.mem.sticky_miss_key),
            fmt(s.mem.addr_outside),
        );
    }
}

/// Dump the sampled iced-residue opcode histogram (`WIE_JIT_OPCODE_HISTO=1`).
///
/// Buckets are keyed by the iced-x86 mnemonic discriminant; names are
/// resolved through `Mnemonic::try_from`. Top 60 shown, like
/// [`crate::exec::dump_iced_counters`], but sampled (every 64th step) so it
/// can stay on for whole-session profiling.
fn dump_opcode_histogram() {
    let lines = opcode_histogram_lines();
    if lines.is_empty() {
        tracing::error!("[wie] jit_opcode_hist: empty (no sampled steps)");
        return;
    }
    for line in &lines {
        tracing::error!("{line}");
    }
}

/// Render the opcode-residue histogram as report lines.
///
/// Buckets are keyed by the iced-x86 mnemonic discriminant; names are
/// resolved through `Mnemonic::try_from`. Top 60 shown, like
/// [`crate::exec::dump_iced_counters`]. Two producers feed the same buckets
/// with different sampling, so the header states both: iced residue is
/// sampled (every [`pipeline::OPCODE_SAMPLE_EVERY`]th step) to stay on for
/// whole-session profiling, while JIT rejections are counted once per block —
/// a rejected block is compiled at most once per VA, so sampling there would
/// hide the signal. Empty when the gate is off or no samples were taken.
fn opcode_histogram_lines() -> Vec<String> {
    let samples = pipeline::OPCODE_SAMPLES.load(std::sync::atomic::Ordering::Relaxed);
    crate::exec::render_mnemonic_histogram(&pipeline::OPCODE_HISTO, true, |total| {
        format!(
            "--- jit opcode-residue histogram (iced residue sampled 1/{}, \
             jit rejections recorded per rejected block, {samples} iced samples, total={total}) ---",
            pipeline::OPCODE_SAMPLE_EVERY
        )
    })
}
