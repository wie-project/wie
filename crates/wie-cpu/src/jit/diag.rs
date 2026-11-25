//! JIT diagnostics reporting: mem-path histogram, background-promotion
//! ledger, and the sampled iced-residue opcode histogram.
//!
//! Split out of `jit/mod.rs` (file-size policy, ADR-002); the public surface
//! is re-exported unchanged at `crate::jit`.

use super::JitStats;
use super::config::JitConfig;
use super::pipeline;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

// ── Per-rip block-entry counts ────────────────────────────────────────────
//
// The startup-latency question is *how many compiled blocks are entered exactly
// once*, i.e. how much compilation bought nothing. Nothing in the engine answers
// it: `CacheEntry::Hot { visits, .. }` stops counting the moment the block is
// installed as `Ready`, so it is a promotion counter, not an entry total.
//
// The count is one increment on a path that already rebuilds pins, restamps the
// generation and does several compares, and it is gated on the profile, so a
// normal run pays one relaxed load and a branch:
//
// ```text
// if !entry_counts_enabled() { return; }
// ```
//
// Entries are serialised behind one `Mutex` **on purpose**. A lock-free sharded
// table would be faster, but two guest rIPs sharing a slot would merge their
// counts, and the two questions this exists to answer — "how many blocks were
// entered exactly once" and "what share of entries do the top 20 take" — are
// both computed from exact per-rIP totals. A diagnostic that quietly collides
// two blocks cannot answer either. Enabled only with `WIE_RUNTIME_PROFILE`, so
// the serialisation never applies to a run whose numbers are being timed.

static ENTRY_COUNTS: OnceLock<Mutex<HashMap<u64, u64>>> = OnceLock::new();

/// Whether block-entry counting is armed. `WIE_RUNTIME_PROFILE` is the same gate
/// the runtime profile uses, so the counts appear exactly when a profile report
/// is going to be printed.
fn entry_counts_enabled() -> bool {
    profile_gate_on()
}

/// Whether the `WIE_RUNTIME_PROFILE` gate is on. The exact switch the runtime
/// profile uses, so every counter here appears precisely when a report is going
/// to be printed, and costs one relaxed `OnceLock` load when it is not.
pub(crate) fn profile_gate_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("WIE_RUNTIME_PROFILE").is_some())
}

/// Count one entry to the compiled block at `rip`.
///
/// Called once per dispatch of a compiled block. Cheap when off (one relaxed
/// load of a `OnceLock` plus a branch); exact and serialised when on.
///
/// Called from the first statement of `run_compiled` (`jit/pipeline.rs`), which
/// already carries `entry_rip` for its pin refresh.
pub(crate) fn count_block_entry(rip: u64) {
    if !entry_counts_enabled() {
        return;
    }
    let counts = ENTRY_COUNTS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut g) = counts.lock() {
        let e = g.entry(rip).or_insert(0_u64);
        *e = e.saturating_add(1);
    }
}

/// `part` as a percentage of `total`, 0.0 when `total` is zero.
fn share_pct(part: u64, total: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    #[expect(clippy::cast_precision_loss, reason = "report percentages only")]
    {
        100.0 * (part as f64) / (total as f64)
    }
}

/// The block-entry histogram, as report lines.
///
/// Answers, in one report: how many blocks were entered exactly once (compiled
/// for nothing), how many entries those account for, and what share of all
/// entries the twenty hottest blocks take.
pub(crate) fn block_entry_report_lines(stats: &JitStats) -> Vec<String> {
    if !entry_counts_enabled() {
        return Vec::new();
    }
    let Some(counts) = ENTRY_COUNTS.get() else {
        return Vec::new();
    };
    let Ok(g) = counts.lock() else {
        return Vec::new();
    };
    if g.is_empty() {
        return Vec::new();
    }
    let v: Vec<(u64, u64)> = g.iter().map(|(rip, n)| (*rip, *n)).collect();
    // The counter and `exec.cache_hits` are not the same measure, and the
    // difference has to be visible on every run rather than explained once:
    // `cache_hits` charges a block found `Ready` in the cache, while this
    // counter also charges the block that an *inline* compile produced and the
    // dispatcher executed on the same visit. Measured on `crt_hello`: 6 entries
    // against `cache_hits=5` with `jit_profile: inline=1`, and 17 against 16 on
    // an earlier run — the delta is the inline-executed block, every time.
    let mut out = entry_report_lines(&v);
    if let Some(first) = out.first_mut() {
        let cache_hits = stats.exec.cache_hits;
        let entries: u64 = v.iter().map(|(_, n)| *n).sum();
        let delta = entries.saturating_sub(cache_hits);
        first.push_str(&format!(
            "; cache_hits={cache_hits}, delta={delta} (inline-compiled and run on the same visit)"
        ));
    }
    out
}

/// The histogram arithmetic, over an explicit `(rip, entries)` list.
///
/// Separated from the global so it can be asserted: the two numbers this exists
/// to produce — the count of blocks entered exactly once, and the share of all
/// entries taken by the top twenty — are the deliverable, and they should not
/// only ever be read off a live run.
fn entry_report_lines(v: &[(u64, u64)]) -> Vec<String> {
    if v.is_empty() {
        return Vec::new();
    }
    let mut v: Vec<(u64, u64)> = v.to_vec();
    v.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let total_entries: u64 = v.iter().map(|(_, n)| *n).sum();
    let distinct = u64::try_from(v.len()).unwrap_or(u64::MAX);
    let once = v.iter().filter(|(_, n)| *n == 1).count();
    let at_most_two = v.iter().filter(|(_, n)| *n <= 2).count();
    let top20: u64 = v.iter().take(20).map(|(_, n)| *n).sum();
    let pct = |part: u64| share_pct(part, total_entries);
    let mut out = vec![format!(
        "[wie] jit_block_entries: {total_entries} entries over {distinct} blocks; \
         {once} blocks entered exactly once ({:.1}% of blocks), <=2 entries: {at_most_two}",
        share_pct(u64::try_from(once).unwrap_or(u64::MAX), distinct)
    )];
    out.push(format!(
        "[wie] jit_block_entries: top 20 blocks take {top20} entries = {:.1}% of all entries; \
         blocks entered once account for {once} entries = {:.1}%",
        pct(top20),
        pct(u64::try_from(once).unwrap_or(0))
    ));
    for (rip, n) in v.iter().take(10) {
        out.push(format!(
            "[wie] jit_block_entries:   {rip:#x} entries={n} share={:.2}%",
            pct(*n)
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Eager-promotion site attribution (`WIE_RUNTIME_PROFILE`).
//
// `jit_profile: eager=N` is a single number in front of three different code
// paths: the region fold, the boot-mode self-loop baseline branch, and the
// `eager || persist_eager` branch. Work-gating the fast-UCRT arm moved that
// number from 368 to 365 on `crt_hello`, i.e. the arm the comment at the
// no-stall site credited with the startup cost was worth 3 — so the split has to
// come from a measurement, not from reading one arm and inferring. These
// counters are that measurement, and they render `sum_sites` next to the
// profile's own `eager=N` so an unexplained residual is visible on every run
// instead of turning into a story about which arm "must" be firing.
//
// The per-RIP map exists to separate the two readings of a large count: many
// blocks decided once apiece, or one block re-decided on every visit. The second
// is a defect (a baseline compile whose install does not stick), the first is a
// policy shape, and they need different fixes.
// ---------------------------------------------------------------------------

/// Which non-loop `eager_compiles` site fired.
#[derive(Clone, Copy)]
pub(super) enum EagerSite {
    /// A latch's loop header was folded into a region (`promote_loop_headers`).
    RegionFold,
    /// The `eager || persist_eager` branch.
    EagerBranch,
}

#[derive(Default)]
struct EagerTally {
    region_fold: u64,
    eager_branch: u64,
    loop_attempts: u64,
    loop_compiles: u64,
    loop_cache_present: u64,
    loop_rips: HashMap<u64, u64>,
}

static EAGER_TALLY: OnceLock<Mutex<EagerTally>> = OnceLock::new();

fn eager_tally() -> &'static Mutex<EagerTally> {
    EAGER_TALLY.get_or_init(|| Mutex::new(EagerTally::default()))
}

/// Count one decision at a non-loop `eager_compiles` site.
pub(super) fn count_eager_site(site: EagerSite) {
    if !profile_gate_on() {
        return;
    }
    if let Ok(mut g) = eager_tally().lock() {
        match site {
            EagerSite::RegionFold => g.region_fold = g.region_fold.saturating_add(1),
            EagerSite::EagerBranch => g.eager_branch = g.eager_branch.saturating_add(1),
        }
    }
}

/// Note one boot-mode self-loop baseline **attempt**, and whether the cache
/// already held an entry for `rip` when the attempt was made.
///
/// A nonzero `cache_present_at_attempt` is a contradiction — the decision site
/// is only reached from the cache-miss path — so it is counted rather than
/// assumed away.
pub(super) fn note_loop_baseline_attempt(rip: u64, cache_present: bool) {
    if !profile_gate_on() {
        return;
    }
    if let Ok(mut g) = eager_tally().lock() {
        g.loop_attempts = g.loop_attempts.saturating_add(1);
        if cache_present {
            g.loop_cache_present = g.loop_cache_present.saturating_add(1);
        }
        let e = g.loop_rips.entry(rip).or_insert(0_u64);
        *e = e.saturating_add(1);
    }
}

/// Note that a boot-mode self-loop baseline compile succeeded — the one site
/// that charges both `eager_compiles` and `bg_enqueues`.
pub(super) fn note_loop_baseline_compile() {
    if !profile_gate_on() {
        return;
    }
    if let Ok(mut g) = eager_tally().lock() {
        g.loop_compiles = g.loop_compiles.saturating_add(1);
    }
}

/// A point-in-time copy of the tally, so the arithmetic can be asserted.
#[derive(Default)]
struct EagerSnapshot {
    region_fold: u64,
    eager_branch: u64,
    loop_attempts: u64,
    loop_compiles: u64,
    loop_cache_present: u64,
    loop_rips: Vec<(u64, u64)>,
    /// The profile's own `eager=` total, for the reconciliation.
    reported_eager: u64,
}

/// Render the site split, with the reconciliation against `eager=` in the line.
fn eager_site_lines(snap: &EagerSnapshot) -> Vec<String> {
    let sum_sites = snap
        .region_fold
        .saturating_add(snap.eager_branch)
        .saturating_add(snap.loop_compiles);
    if sum_sites == 0 && snap.loop_attempts == 0 && snap.reported_eager == 0 {
        return Vec::new();
    }
    let mut rips = snap.loop_rips.clone();
    rips.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let distinct = u64::try_from(rips.len()).unwrap_or(u64::MAX);
    let max_per_rip = rips.first().map_or(0, |(_, n)| *n);
    let unexplained = snap.reported_eager.saturating_sub(sum_sites);
    let mut out = vec![format!(
        "[wie] jit_eager_sites: region_fold={} eager_branch={} loop_baseline_compiles={} \
         (attempts={} distinct_rips={distinct} max_per_rip={max_per_rip} \
         cache_present_at_attempt={}) | sum_sites={sum_sites} reported_eager={} unexplained={unexplained}",
        snap.region_fold,
        snap.eager_branch,
        snap.loop_compiles,
        snap.loop_attempts,
        snap.loop_cache_present,
        snap.reported_eager
    )];
    for (rip, n) in rips.iter().take(3) {
        out.push(format!(
            "[wie] jit_eager_sites:   loop_baseline {rip:#x} decisions={n}"
        ));
    }
    out
}

/// The site split as report lines; empty when nothing was recorded.
fn eager_site_report_lines(stats: &JitStats) -> Vec<String> {
    if !profile_gate_on() {
        return Vec::new();
    }
    let Some(tally) = EAGER_TALLY.get() else {
        return Vec::new();
    };
    let Ok(g) = tally.lock() else {
        return Vec::new();
    };
    let snap = EagerSnapshot {
        region_fold: g.region_fold,
        eager_branch: g.eager_branch,
        loop_attempts: g.loop_attempts,
        loop_compiles: g.loop_compiles,
        loop_cache_present: g.loop_cache_present,
        loop_rips: g.loop_rips.iter().map(|(r, n)| (*r, *n)).collect(),
        reported_eager: stats.profile.eager_compiles,
    };
    eager_site_lines(&snap)
}

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
/// ledger, direct-chaining health, the opt-level tier ledger, the persistent
/// machine-code cache (`WIE_JIT_CODE_CACHE`) and the sampled iced-residue
/// opcode histogram (`WIE_JIT_OPCODE_HISTO=1`). Rendered into the report itself
/// so they print on every profile path — SIGINT included — without depending on
/// a tracing subscriber being installed (tracing is silent under a default
/// RUST_LOG).
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
    // Unconditional, unlike its neighbours: the code cache is the one feature
    // whose *absence of data* is the interesting result. The exit-time summary
    // in `CodeCache::summary` is silent when `WIE_JIT_CODE_CACHE` is off, and
    // it does not run at all for every guest, so a gated line here would leave
    // a reader unable to tell "the cache did nothing" from "this build does not
    // measure it".
    out.push(code_cache_line(s));
    if JitConfig::get().opcode_hist_enabled() {
        out.extend(opcode_histogram_lines());
    }
    out.extend(block_entry_report_lines(s));
    out.extend(eager_site_report_lines(s));
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

/// Persistent machine-code cache (`WIE_JIT_CODE_CACHE`) as one report line.
///
/// `restored` blocks replayed from disk instead of compiled is the whole point
/// of the knob; `refused` (summed over every reason — bytes, generation,
/// file, target, malformed) and `unpersistable` (emitted code carrying a
/// relocation the format refuses to persist, expected and *not* an error) are
/// what explain a low or a zero `restored`.
///
/// Printed unconditionally, zeros included: see [`jit_profile_report_lines`].
/// With the knob off every counter is genuinely 0, so the line reads as
/// "measured, nothing happened" rather than being absent.
fn code_cache_line(s: &JitStats) -> String {
    let restored = s.profile.code_cache_restored;
    let refused = s.profile.code_cache_refused;
    // Same derived-ratio shape as `stores_per_hop`: an integer percent, 0 when
    // the denominator is 0. Denominator is restored + every refusal — what the
    // cache actually had to offer — not `compile.compiles`, so this measures
    // restore correctness rather than guest coverage. Identical to
    // `CodeCacheCounts::hit_rate_pct`, recomputed here because only the folded
    // totals survive the cross-thread merge.
    let hit_rate = restored
        .saturating_mul(100)
        .checked_div(restored.saturating_add(refused))
        .unwrap_or(0);
    format!(
        "[wie] jit_code_cache: restored={restored} refused={refused} unpersistable={} hit_rate={hit_rate}%",
        s.profile.code_cache_unpersistable
    )
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
    let mut lines = crate::exec::render_mnemonic_histogram(
        &pipeline::OPCODE_HISTO,
        true,
        |total| {
            format!(
                "--- jit opcode-residue histogram (iced residue sampled 1/{}, \
             jit rejections recorded per rejected block, {samples} iced samples, total={total}) ---",
                pipeline::OPCODE_SAMPLE_EVERY
            )
        },
    );
    // Rendered beside the histogram because the histogram is mnemonic-keyed and
    // therefore cannot say *why* a block was refused: `Unsupported` is routine
    // (an x87 guest runs at interpreter speed by design), whereas `Form` means
    // the JIT implements that instruction and not that operand shape — the class
    // that used to be invisible. Pushed even when no samples were taken, so a
    // session that only refused blocks still reports a reason.
    let (unsupported, form, verifier) = crate::jit::lower::err::refusal_counts();
    if unsupported + form + verifier > 0 {
        lines.push(format!(
            "[wie] jit_lower_refusals: unsupported={unsupported} form={form} verifier={verifier}"
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cache-off case is the one that must not go missing: a run with
    /// `WIE_JIT_CODE_CACHE` unset has to report measured zeros, not silence —
    /// silence is indistinguishable from a build that lost the counters.
    #[test]
    fn code_cache_line_reports_zeros_without_the_knob() {
        assert_eq!(
            code_cache_line(&JitStats::default()),
            "[wie] jit_code_cache: restored=0 refused=0 unpersistable=0 hit_rate=0%"
        );
    }

    /// `hit_rate` is `restored / (restored + refused)`, matching
    /// `CodeCacheCounts::hit_rate_pct`: a block with no blob at all is an absent
    /// cache, not a failed restore, so it must stay out of the denominator.
    #[test]
    fn code_cache_line_hit_rate_uses_refusals_as_denominator() {
        let mut s = JitStats::default();
        s.profile.code_cache_restored = 23;
        s.profile.code_cache_refused = 2;
        s.profile.code_cache_unpersistable = 40;
        assert_eq!(
            code_cache_line(&s),
            "[wie] jit_code_cache: restored=23 refused=2 unpersistable=40 hit_rate=92%"
        );

        // Every refusal, no restores: 0%.
        s.profile.code_cache_restored = 0;
        assert!(code_cache_line(&s).ends_with("hit_rate=0%"));

        // Restores but no refusals: 100%, and not a divide-by-zero.
        s.profile.code_cache_restored = 5;
        s.profile.code_cache_refused = 0;
        assert!(code_cache_line(&s).ends_with("hit_rate=100%"));
    }

    /// The line is unconditional, unlike the quiet-by-default ledger/tier lines.
    #[test]
    fn profile_report_always_carries_the_code_cache_line() {
        let quiet = JitStats::default();
        assert!(
            !jit_profile_report_lines(&quiet)
                .iter()
                .any(|l| l.starts_with("[wie] jit_bg_ledger:")),
            "ledger stays quiet when nothing was recorded"
        );
        let lines = jit_profile_report_lines(&quiet);
        assert!(
            lines.iter().any(|l| l.starts_with("[wie] jit_code_cache:")),
            "code-cache line missing from {lines:?}"
        );
    }

    /// The site split must reconcile against the profile's own `eager=N`, and
    /// any residual must be named rather than absorbed. This is the shape that
    /// started the question: `crt_hello` reports `eager=368` while the two
    /// non-loop sites account for 3 of it, so `unexplained=3` on the first line
    /// is the number that ends the guessing about which arm "must" be firing.
    #[test]
    fn eager_site_line_names_the_unexplained_residual() {
        let snap = EagerSnapshot {
            region_fold: 0,
            eager_branch: 0,
            loop_attempts: 365,
            loop_compiles: 365,
            loop_cache_present: 0,
            loop_rips: vec![(0x1400_0f0f0, 300), (0x1400_0a1c0, 60), (0x1400_0b220, 5)],
            reported_eager: 368,
        };
        let lines = eager_site_lines(&snap);
        assert_eq!(lines.len(), 4, "one summary line plus the top three rips");
        let head = &lines[0];
        for want in [
            "loop_baseline_compiles=365",
            "attempts=365",
            "distinct_rips=3",
            "max_per_rip=300",
            "sum_sites=365",
            "reported_eager=368",
            "unexplained=3",
        ] {
            assert!(
                head.contains(want),
                "summary line is missing {want}: {head}"
            );
        }
        // Sorted by decisions, so the re-decided block is the first one named:
        // one rip with 300 of 365 attempts is the "install does not stick"
        // signature, and it must be distinguishable from 300 blocks decided once.
        assert!(lines[1].contains("0x14000f0f0"), "{}", lines[1]);
        assert!(lines[1].contains("decisions=300"), "{}", lines[1]);
        // The decision site is reached from the cache-miss path, so an entry
        // visible at attempt time is a contradiction the report must expose.
        assert_eq!(snap.loop_cache_present, 0);
        // Nothing recorded and nothing reported prints nothing, not a zero line.
        assert!(eager_site_lines(&EagerSnapshot::default()).is_empty());
    }

    /// The two numbers this counter exists to produce, asserted on an explicit
    /// histogram: blocks entered exactly once, and the share of all entries the
    /// top twenty take. Thirty blocks here — twenty entered once, five twice,
    /// five hot — so the expected values are exact, not eyeballed.
    #[test]
    fn entry_report_answers_the_two_questions() {
        let mut v: Vec<(u64, u64)> = (0..20).map(|i| (0x1000 + i * 4, 1)).collect();
        v.extend((0..5).map(|i| (0x2000 + i * 4, 2)));
        v.extend([
            (0x3000, 1000_u64),
            (0x3004, 500),
            (0x3008, 250),
            (0x300c, 100),
            (0x3010, 50),
        ]);
        // total = 20*1 + 5*2 + 1900 = 1930; top 20 = 1900 + 10 (the doubled) +
        // 10 (once-only blocks, which fill the remaining ten slots) = 1920.
        let lines = entry_report_lines(&v);
        assert!(
            lines[0].contains("1930 entries over 30 blocks"),
            "summary line was {:?}",
            lines[0]
        );
        assert!(
            lines[0].contains("20 blocks entered exactly once"),
            "once-only count was {:?}",
            lines[0]
        );
        assert!(
            lines[1].contains("top 20 blocks take 1920 entries = 99.5%"),
            "top-20 share was {:?}",
            lines[1]
        );
        assert!(
            lines[1].contains("blocks entered once account for 20 entries = 1.0%"),
            "once-only share was {:?}",
            lines[1]
        );
        // The hottest block leads the listing, and its share is over half.
        assert!(
            lines[2].contains("0x3000 entries=1000 share=51.81%"),
            "hot block line was {:?}",
            lines[2]
        );
    }
}
