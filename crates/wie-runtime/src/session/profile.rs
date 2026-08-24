//! `RuntimeProfile` collection, getters, and report formatting.

use ahash::HashMap;

/// Host-side timing breakdown for one session (enabled via `WIE_RUNTIME_PROFILE=1`).
///
/// Fields are private; read them through the getters and mutate them through
/// the `pub(crate)` accumulator methods (the run pump is the only writer).
#[derive(Debug, Clone, Default)]
pub struct RuntimeProfile {
    init_ns: u128,
    emu_ns: u128,
    handler_ns: u128,
    resolve_ns: u128,
    host_stops: u64,
    noisy_calls: u64,
    charged_calls: u64,
    /// Per-export counts and handler time (`library!name` → (count, ns));
    /// exposed as the plural getter [`Self::by_exports`] because the value is
    /// a whole map, not one export.
    by_export: HashMap<String, (u64, u128)>,
    wall_ns: u128,
    cpu_user_us: u64,
    cpu_sys_us: u64,
    jit: Option<wie_cpu::JitStats>,
    mem_backend: String,
    idle_policy: String,
    idle_parks: u64,
    idle_park_ns: u128,
    /// Wall nanoseconds spent parked / waiting-for-message per session
    /// (Painpoint 1): empty-`GetMessage` parks, host object/pthread/CS wait
    /// parks. The idle-attribution counter from the perf plan.
    idle_residency_ns: u128,
    frames_published: u64,
    /// Zero-copy hand-backs of the published buffer (`Arc::try_unwrap` hit).
    hand_back_unwrap: u64,
    /// Clone-fallback hand-backs (host still held the previous frame's Arc).
    hand_back_clone: u64,
    publish_ns: u128,
    publish_ns_last: u128,
    blit_copy_ns: u128,
    blit_copy_ns_last: u128,
    present_ns: u128,
    present_ns_last: u128,
    last_frame_host_stops: u64,
    last_frame_iced_insns: u64,
    last_frame_jit_insns: u64,
    /// Guest-side accumulated `shared_winapi` wait (ns; Task 7).
    guest_lock_wait_ns: u128,
    /// Largest single guest-side `shared_winapi` wait (ns).
    guest_lock_wait_max_ns: u128,
    /// Presenter-side accumulated `shared_winapi` wait (ns).
    presenter_lock_wait_ns: u128,
    /// Largest single presenter-side `shared_winapi` wait (ns).
    presenter_lock_wait_max_ns: u128,
}

impl RuntimeProfile {
    /// Wall time spent in session init (PE loading, patch, pre-compilation).
    #[must_use]
    pub fn init_ns(&self) -> u128 {
        self.init_ns
    }
    /// Wall time spent inside `run_until_stop` (guest instruction execution).
    #[must_use]
    pub fn emu_ns(&self) -> u128 {
        self.emu_ns
    }
    /// Wall time spent in WinAPI handlers / dispatch / return_from_win64_api.
    #[must_use]
    pub fn handler_ns(&self) -> u128 {
        self.handler_ns
    }
    /// Wall time spent resolving hook VA → fake API entry.
    #[must_use]
    pub fn resolve_ns(&self) -> u128 {
        self.resolve_ns
    }
    /// Number of times the CPU stopped on a fake-API hook (host entry points).
    #[must_use]
    pub fn host_stops(&self) -> u64 {
        self.host_stops
    }
    /// Noisy API calls (not charged to max_api).
    #[must_use]
    pub fn noisy_calls(&self) -> u64 {
        self.noisy_calls
    }
    /// Charged interesting API calls.
    #[must_use]
    pub fn charged_calls(&self) -> u64 {
        self.charged_calls
    }
    /// Per-export counts and handler time (library!name → (count, handler_ns)).
    #[must_use]
    pub fn by_exports(&self) -> &HashMap<String, (u64, u128)> {
        &self.by_export
    }
    /// End-to-end wall time for the run (set by CLI / micro runner when available).
    #[must_use]
    pub fn wall_ns(&self) -> u128 {
        self.wall_ns
    }
    /// Process user CPU microseconds (delta over the run, when available).
    #[must_use]
    pub fn cpu_user_us(&self) -> u64 {
        self.cpu_user_us
    }
    /// Process system CPU microseconds (delta over the run, when available).
    #[must_use]
    pub fn cpu_sys_us(&self) -> u64 {
        self.cpu_sys_us
    }
    /// JIT / interpreter diagnostics snapshot at end of run.
    #[must_use]
    pub fn jit(&self) -> Option<wie_cpu::JitStats> {
        self.jit
    }
    /// Active memory backend name (`hash`, …).
    #[must_use]
    pub fn mem_backend(&self) -> &str {
        &self.mem_backend
    }
    /// Host idle policy name (`busy` / `yield` / `park`).
    #[must_use]
    pub fn idle_policy(&self) -> &str {
        &self.idle_policy
    }
    /// Empty-message / host park quanta applied by the outer run loop.
    #[must_use]
    pub fn idle_parks(&self) -> u64 {
        self.idle_parks
    }
    /// Wall nanoseconds spent in host idle parks (message quanta).
    #[must_use]
    pub fn idle_park_ns(&self) -> u128 {
        self.idle_park_ns
    }
    /// Wall nanoseconds the session spent parked / waiting-for-message:
    /// empty-`GetMessage` parks plus host object/pthread/CS wait parks.
    #[must_use]
    pub fn idle_residency_ns(&self) -> u128 {
        self.idle_residency_ns
    }
    /// Number of published frames (frame timing enabled only).
    #[must_use]
    pub fn frames_published(&self) -> u64 {
        self.frames_published
    }
    /// Number of zero-copy published-buffer hand-backs (`Arc::try_unwrap` hit).
    #[must_use]
    pub fn hand_back_unwrap(&self) -> u64 {
        self.hand_back_unwrap
    }
    /// Number of clone-fallback hand-backs (host still held the previous Arc).
    #[must_use]
    pub fn hand_back_clone(&self) -> u64 {
        self.hand_back_clone
    }
    /// Accumulated publish wall time (ns).
    #[must_use]
    pub fn publish_ns(&self) -> u128 {
        self.publish_ns
    }
    /// Duration of the most recent publish (ns).
    #[must_use]
    pub fn publish_ns_last(&self) -> u128 {
        self.publish_ns_last
    }
    /// Accumulated BitBlt mask-copy (`mask_bgra_to_0rgb`) wall time (ns).
    #[must_use]
    pub fn blit_copy_ns(&self) -> u128 {
        self.blit_copy_ns
    }
    /// Duration of the most recent mask copy (ns).
    #[must_use]
    pub fn blit_copy_ns_last(&self) -> u128 {
        self.blit_copy_ns_last
    }
    /// Accumulated host present (softbuffer copy + upload) wall time (ns).
    #[must_use]
    pub fn present_ns(&self) -> u128 {
        self.present_ns
    }
    /// Duration of the most recent host present (ns).
    #[must_use]
    pub fn present_ns_last(&self) -> u128 {
        self.present_ns_last
    }
    /// Host stops between the last two published frames.
    #[must_use]
    pub fn last_frame_host_stops(&self) -> u64 {
        self.last_frame_host_stops
    }
    /// Iced-retired instructions between the last two published frames.
    #[must_use]
    pub fn last_frame_iced_insns(&self) -> u64 {
        self.last_frame_iced_insns
    }
    /// JIT-retired instructions between the last two published frames.
    #[must_use]
    pub fn last_frame_jit_insns(&self) -> u64 {
        self.last_frame_jit_insns
    }
    /// Guest-side accumulated `shared_winapi` wait (ns) — the primary pump
    /// and worker threads blocked on the big mutex while a peer held it.
    #[must_use]
    pub fn guest_lock_wait_ns(&self) -> u128 {
        self.guest_lock_wait_ns
    }
    /// Largest single guest-side `shared_winapi` wait (ns).
    #[must_use]
    pub fn guest_lock_wait_max_ns(&self) -> u128 {
        self.guest_lock_wait_max_ns
    }
    /// Presenter-side accumulated `shared_winapi` wait (ns) — the host
    /// frame loop blocked on the big mutex while the guest held it.
    #[must_use]
    pub fn presenter_lock_wait_ns(&self) -> u128 {
        self.presenter_lock_wait_ns
    }
    /// Largest single presenter-side `shared_winapi` wait (ns).
    #[must_use]
    pub fn presenter_lock_wait_max_ns(&self) -> u128 {
        self.presenter_lock_wait_max_ns
    }

    /// Accumulate one `run_until_stop` quantum into `emu_ns`.
    pub(crate) fn add_emu_ns(&mut self, ns: u128) {
        self.emu_ns = self.emu_ns.saturating_add(ns);
    }

    /// Accumulate hook-address-resolution time into `resolve_ns`.
    pub(crate) fn add_resolve_ns(&mut self, ns: u128) {
        self.resolve_ns = self.resolve_ns.saturating_add(ns);
    }

    /// Count one host stop on a fake-API hook.
    pub(crate) fn inc_host_stops(&mut self) {
        self.host_stops = self.host_stops.saturating_add(1);
    }

    /// Record one completed API handler: handler time, noisy/charged counter
    /// and the per-export accumulator (when `export_key` is supplied).
    pub(crate) fn record_handler(&mut self, ns: u128, noisy: bool, export_key: Option<&str>) {
        self.handler_ns = self.handler_ns.saturating_add(ns);
        if noisy {
            self.noisy_calls = self.noisy_calls.saturating_add(1);
        } else {
            self.charged_calls = self.charged_calls.saturating_add(1);
        }
        if let Some(key) = export_key {
            let entry = self.by_export.entry(key.to_owned()).or_insert((0, 0));
            entry.0 = entry.0.saturating_add(1);
            entry.1 = entry.1.saturating_add(ns);
        }
    }

    /// Record one host idle park (quanta + wall time).
    pub(crate) fn record_idle_park(&mut self, park_ns: u128) {
        self.idle_parks = self.idle_parks.saturating_add(1);
        self.idle_park_ns = self.idle_park_ns.saturating_add(park_ns);
    }

    /// Accumulate wall time spent parked / waiting-for-message into the
    /// idle-residency counter (Painpoint 1 attribution).
    pub(crate) fn add_idle_residency_ns(&mut self, ns: u128) {
        self.idle_residency_ns = self.idle_residency_ns.saturating_add(ns);
    }

    /// Set session-init wall time.
    pub(crate) fn set_init_ns(&mut self, ns: u128) {
        self.init_ns = ns;
    }

    /// Set the active memory backend name.
    pub(crate) fn set_mem_backend(&mut self, name: String) {
        self.mem_backend = name;
    }

    /// Set the host idle policy name.
    pub(crate) fn set_idle_policy(&mut self, policy: String) {
        self.idle_policy = policy;
    }

    /// Set the JIT/interpreter stats snapshot.
    pub(crate) fn set_jit(&mut self, stats: Option<wie_cpu::JitStats>) {
        self.jit = stats;
    }

    /// Human-readable multi-line report for stderr / logs.
    #[must_use]
    pub fn report(&self) -> String {
        let total = self
            .emu_ns()
            .saturating_add(self.handler_ns())
            .saturating_add(self.resolve_ns());
        let pct = |part: u128| -> f64 {
            if total == 0 {
                0.0
            } else {
                (part as f64) * 100.0 / (total as f64)
            }
        };
        let mut lines = Vec::new();
        lines.push("=== WIE_RUNTIME_PROFILE ===".to_owned());
        if !self.mem_backend().is_empty() {
            lines.push(format!("mem_backend={}", self.mem_backend()));
        }
        if !self.idle_policy().is_empty() {
            lines.push(format!("idle_policy={}", self.idle_policy()));
        }
        if self.idle_parks() > 0 || self.idle_park_ns() > 0 || self.idle_residency_ns() > 0 {
            lines.push(format!(
                "idle_parks={} idle_park_ms={:.2} idle_residency_ms={:.2}",
                self.idle_parks(),
                self.idle_park_ns() as f64 / 1e6,
                self.idle_residency_ns() as f64 / 1e6
            ));
        }
        lines.push(format!(
            "host_stops={} noisy={} charged={}",
            self.host_stops(),
            self.noisy_calls(),
            self.charged_calls()
        ));
        if self.guest_lock_wait_ns() > 0 || self.presenter_lock_wait_ns() > 0 {
            lines.push(format!(
                "winapi_lock_wait: guest_ms={:.3} guest_max_ms={:.3} \
                 presenter_ms={:.3} presenter_max_ms={:.3}",
                self.guest_lock_wait_ns() as f64 / 1e6,
                self.guest_lock_wait_max_ns() as f64 / 1e6,
                self.presenter_lock_wait_ns() as f64 / 1e6,
                self.presenter_lock_wait_max_ns() as f64 / 1e6,
            ));
        }
        if self.wall_ns() > 0 || self.cpu_user_us() > 0 || self.cpu_sys_us() > 0 {
            let wall_ms = self.wall_ns() as f64 / 1e6;
            let cpu_ms = (self.cpu_user_us().saturating_add(self.cpu_sys_us())) as f64 / 1e3;
            let cpu_pct = if wall_ms > 0.0 {
                (cpu_ms / wall_ms) * 100.0
            } else {
                0.0
            };
            lines.push(format!(
                "wall_ms={:.2}  cpu_user_ms={:.2}  cpu_sys_ms={:.2}  cpu%≈{:.1}",
                wall_ms,
                self.cpu_user_us() as f64 / 1e3,
                self.cpu_sys_us() as f64 / 1e3,
                cpu_pct
            ));
        }
        lines.push(format!(
            "emu_ms={:.2} ({:.1}%)  handler_ms={:.2} ({:.1}%)  resolve_ms={:.2} ({:.1}%)  total_accounted_ms={:.2}  init_ms={:.2}",
            self.emu_ns() as f64 / 1e6,
            pct(self.emu_ns()),
            self.handler_ns() as f64 / 1e6,
            pct(self.handler_ns()),
            self.resolve_ns() as f64 / 1e6,
            pct(self.resolve_ns()),
            total as f64 / 1e6,
            self.init_ns() as f64 / 1e6,
        ));
        if let Some(j) = self.jit() {
            lines.push(format!(
                "jit: insns={} iced={} compiles={} skip={} bg_compiles={} bg_stalls={} bg_stall_us={} bg_fallback={} cache_hits={} load={} store={}",
                j.exec.jit_insns,
                j.exec.iced_insns,
                j.compile.compiles,
                j.compile.compile_skip,
                j.bg.compiles,
                j.bg.waits,
                j.bg.wait_us,
                j.bg.inline_fallbacks,
                j.exec.cache_hits,
                j.mem.load_calls,
                j.mem.store_calls
            ));
            // General JIT decision + compile-timing diagnostics.
            let p = &j.profile;
            if p.compile_us > 0 {
                lines.push(format!(
                    "jit_profile: compile_us={} eager={} hot={} bg_enq={} bg_hit={} bg_to={} inline={} iced_fb={} never={}",
                    p.compile_us,
                    p.eager_compiles,
                    p.hot_compiles,
                    p.bg_enqueues,
                    p.bg_wait_hits,
                    p.bg_wait_timeouts,
                    p.inline_compiles,
                    p.iced_fallbacks,
                    p.never_marks
                ));
            }
        }
        if self.frames_published() > 0 || self.publish_ns_last() > 0 {
            lines.push(format!(
                "frames_published={} publish_ms={:.3} publish_ms_last={:.3} \
                 blit_copy_ms={:.3} blit_copy_ms_last={:.3} \
                 present_ms={:.3} present_ms_last={:.3}",
                self.frames_published(),
                self.publish_ns() as f64 / 1e6,
                self.publish_ns_last() as f64 / 1e6,
                self.blit_copy_ns() as f64 / 1e6,
                self.blit_copy_ns_last() as f64 / 1e6,
                self.present_ns() as f64 / 1e6,
                self.present_ns_last() as f64 / 1e6,
            ));
            lines.push(format!("hand_back_unwrap={}", self.hand_back_unwrap()));
            lines.push(format!("hand_back_clone={}", self.hand_back_clone()));
            let frame_total = self
                .last_frame_iced_insns()
                .saturating_add(self.last_frame_jit_insns());
            lines.push(format!(
                "last_frame: host_stops={} iced_insns={} jit_insns={} iced_ratio={:.3}",
                self.last_frame_host_stops(),
                self.last_frame_iced_insns(),
                self.last_frame_jit_insns(),
                if frame_total == 0 {
                    0.0
                } else {
                    self.last_frame_iced_insns() as f64 / frame_total as f64
                }
            ));
        }
        let mut ranked: Vec<_> = self.by_exports().iter().collect();
        ranked.sort_by(|a, b| b.1.1.cmp(&a.1.1).then(b.1.0.cmp(&a.1.0)));
        lines.push("top exports by handler time:".to_owned());
        for (name, (count, ns)) in ranked.into_iter().take(20) {
            lines.push(format!(
                "  {count:>7}  {:>8.2} ms  {name}",
                *ns as f64 / 1e6
            ));
        }
        lines.push("top exports by count:".to_owned());
        let mut by_count = self.by_exports().iter().collect::<Vec<_>>();
        by_count.sort_by_key(|(_, (count, _))| std::cmp::Reverse(*count));
        for (name, (count, ns)) in by_count.into_iter().take(15) {
            lines.push(format!(
                "  {count:>7}  {:>8.2} ms  {name}",
                *ns as f64 / 1e6
            ));
        }
        // JIT ledger + iced-residue opcode histogram (`WIE_JIT_OPCODE_HISTO=1`).
        // Part of the report itself so SIGINT prints them like every other
        // section, without needing a tracing subscriber.
        if let Some(j) = self.jit() {
            lines.extend(wie_cpu::jit_profile_report_lines(&j));
        }
        lines.join("\n")
    }
}

impl super::RuntimeSession {
    /// Accumulated host-side profile (empty unless `WIE_RUNTIME_PROFILE` is set).
    #[must_use]
    pub fn profile(&self) -> &RuntimeProfile {
        &self.profile
    }

    /// Mutable profile for outer run-loop counters (e.g. idle parks).
    pub fn profile_mut(&mut self) -> &mut RuntimeProfile {
        &mut self.profile
    }

    /// Whether profiling is enabled for this session.
    #[must_use]
    pub fn profile_enabled(&self) -> bool {
        self.profile_enabled
    }

    /// Snapshot JIT/CPU stats + optional wall/CPU deltas into the profile.
    ///
    /// Called by micro runners after `run_until_stop` when profiling is enabled.
    pub fn finalize_profile(&mut self, wall_ns: u128, cpu_user_us: u64, cpu_sys_us: u64) {
        if self.profile_enabled {
            self.sample_frame_timing();
            self.profile.wall_ns = wall_ns;
            self.profile.cpu_user_us = cpu_user_us;
            self.profile.cpu_sys_us = cpu_sys_us;
            self.profile.jit = self.process.with_mut(|e, _| e.cpu_stats());
            self.profile.mem_backend = self
                .process
                .with_mut(|e, _| e.mem_backend_name().to_owned());
        }
        // Residual iced histogram (opt-in via `WIE_EXEC_TRACE=1`).
        wie_cpu::dump_iced_counters();
        // Mem helper path breakdown (`WIE_JIT_MEM_TRACE=1` or `WIE_EXEC_TRACE=1`).
        if let Some(ref j) = self.profile.jit {
            wie_cpu::dump_mem_path_stats(j);
        } else if let Some(j) = self.process.with_mut(|e, _| e.cpu_stats()) {
            wie_cpu::dump_mem_path_stats(&j);
        }
    }

    /// Enable frame timing (publish / blit-copy / present instrumentation)
    /// without the `WIE_RUNTIME_PROFILE` env var. Used by the micro-suite
    /// frame-time budget gate.
    pub fn enable_frame_timing(&mut self) {
        self.profile_enabled = true;
        self.process.lock_wait_stats.set_enabled(true);
        wie_winapi::present::set_frame_timing_enabled(true);
    }

    /// Publish duration of the most recent frame (ns; 0 when timing disabled).
    #[must_use]
    pub fn present_publish_ns_last(&self) -> u128 {
        self.process
            .with_winapi_ref(|st| st.try_present().map_or(0, |p| p.publish_ns_last))
    }

    /// Copy the shared `shared_winapi` wait accumulators (Task 7) into the
    /// profile snapshot. The atomics are read without taking the WinAPI
    /// lock, so this is safe while guest workers are still running.
    pub(super) fn sync_lock_wait_stats(&mut self) {
        let snap = self.process.lock_wait_stats.snapshot();
        self.profile.guest_lock_wait_ns = u128::from(snap.guest_total_ns);
        self.profile.guest_lock_wait_max_ns = u128::from(snap.guest_max_ns);
        self.profile.presenter_lock_wait_ns = u128::from(snap.presenter_total_ns);
        self.profile.presenter_lock_wait_max_ns = u128::from(snap.presenter_max_ns);
    }

    /// Per-frame timing — copy the present-side accumulators (publish,
    /// blit-copy, host present) into the profile and log per-frame host-stop /
    /// iced-vs-jit deltas whenever a new frame was published since the last
    /// sample. Runs once per host-stop quantum when profiling is enabled.
    pub(super) fn sample_frame_timing(&mut self) {
        // Fold the lock-free `shared_winapi` wait accumulators (written by
        // guest workers and the presenter without the session profile) into
        // the profile snapshot. Runs before the present-state read so the
        // sync also applies to non-GUI sessions (no present state).
        self.sync_lock_wait_stats();
        let present = self.process.with_winapi_ref(|st| {
            st.try_present().map(|p| {
                (
                    p.frames_published,
                    p.publish_ns,
                    p.publish_ns_last,
                    p.blit_copy_ns,
                    p.blit_copy_ns_last,
                    p.present_ns,
                    p.present_ns_last,
                    p.hand_back_unwrap,
                    p.hand_back_clone,
                    p.generation,
                )
            })
        });
        let Some((
            frames_published,
            publish_ns,
            publish_ns_last,
            blit_copy_ns,
            blit_copy_ns_last,
            present_ns,
            present_ns_last,
            hand_back_unwrap,
            hand_back_clone,
            generation,
        )) = present
        else {
            return;
        };
        self.profile.frames_published = frames_published;
        self.profile.publish_ns = publish_ns;
        self.profile.publish_ns_last = publish_ns_last;
        self.profile.blit_copy_ns = blit_copy_ns;
        self.profile.blit_copy_ns_last = blit_copy_ns_last;
        self.profile.present_ns = present_ns;
        self.profile.present_ns_last = present_ns_last;
        self.profile.hand_back_unwrap = hand_back_unwrap;
        self.profile.hand_back_clone = hand_back_clone;

        if generation == self.frame_last_gen {
            return;
        }
        let (iced, jit) = self
            .process
            .with_mut(|e, _| e.cpu_stats())
            .map_or((0, 0), |s| (s.exec.iced_insns, s.exec.jit_insns));
        let iced_delta = iced.saturating_sub(self.frame_last_iced);
        let jit_delta = jit.saturating_sub(self.frame_last_jit);
        self.profile.last_frame_host_stops = self
            .profile
            .host_stops
            .saturating_sub(self.frame_last_stops);
        self.profile.last_frame_iced_insns = iced_delta;
        self.profile.last_frame_jit_insns = jit_delta;
        let frame_total = iced_delta.saturating_add(jit_delta);
        tracing::debug!(
            target: "wiegui",
            generation,
            host_stops = self.profile.last_frame_host_stops,
            iced_insns = iced_delta,
            jit_insns = jit_delta,
            iced_ratio = if frame_total == 0 {
                0.0
            } else {
                iced_delta as f64 / frame_total as f64
            },
            "frame timing"
        );
        self.frame_last_gen = generation;
        self.frame_last_stops = self.profile.host_stops;
        self.frame_last_iced = iced;
        self.frame_last_jit = jit;
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::RuntimeProfile;

    /// The report folds guest + presenter wait totals and maxima into one
    /// line with ms precision, and only when profiling actually recorded
    /// waits — the disabled default prints nothing.
    #[test]
    fn report_includes_lock_wait_line_only_when_nonzero() {
        let profile = RuntimeProfile::default();
        assert!(
            !profile.report().contains("winapi_lock_wait"),
            "disabled profiling must not emit the lock-wait line"
        );

        let profile = RuntimeProfile {
            guest_lock_wait_ns: 1_500_000,
            guest_lock_wait_max_ns: 1_200_000,
            presenter_lock_wait_ns: 250_000,
            presenter_lock_wait_max_ns: 250_000,
            ..RuntimeProfile::default()
        };
        let report = profile.report();
        assert!(report.contains("winapi_lock_wait"), "{report}");
        assert!(report.contains("guest_ms=1.500"), "{report}");
        assert!(report.contains("guest_max_ms=1.200"), "{report}");
        assert!(report.contains("presenter_ms=0.250"), "{report}");
        assert!(report.contains("presenter_max_ms=0.250"), "{report}");
        // The getters surface the same values.
        assert_eq!(profile.guest_lock_wait_ns(), 1_500_000);
        assert_eq!(profile.guest_lock_wait_max_ns(), 1_200_000);
        assert_eq!(profile.presenter_lock_wait_ns(), 250_000);
        assert_eq!(profile.presenter_lock_wait_max_ns(), 250_000);
    }

    /// The idle-residency counter (Painpoint 1) surfaces in the report only
    /// once it recorded park time, and its getter mirrors the stored value.
    #[test]
    fn report_emits_idle_residency_when_recorded() {
        let mut profile = RuntimeProfile::default();
        assert!(
            !profile.report().contains("idle_residency_ms"),
            "no park time → no residency line"
        );
        profile.add_idle_residency_ns(1_500_000);
        profile.record_idle_park(1_500_000);
        let report = profile.report();
        assert!(report.contains("idle_residency_ms=1.50"), "{report}");
        assert_eq!(profile.idle_residency_ns(), 1_500_000);
    }

    /// A snapshot fold (the `sync_lock_wait_stats` shape) copies the
    /// lock-free atomics into the profile totals and maxima.
    #[test]
    fn lock_wait_snapshot_folds_into_profile() {
        let stats = crate::mt_runtime::LockWaitStats::new();
        stats.set_enabled(true);
        stats.record_guest(1_000);
        stats.record_guest(3_000);
        stats.record_presenter(2_000);
        let snap = stats.snapshot();
        let profile = RuntimeProfile {
            guest_lock_wait_ns: u128::from(snap.guest_total_ns),
            guest_lock_wait_max_ns: u128::from(snap.guest_max_ns),
            presenter_lock_wait_ns: u128::from(snap.presenter_total_ns),
            presenter_lock_wait_max_ns: u128::from(snap.presenter_max_ns),
            ..RuntimeProfile::default()
        };
        assert_eq!(profile.guest_lock_wait_ns(), 4_000);
        assert_eq!(profile.guest_lock_wait_max_ns(), 3_000);
        assert_eq!(profile.presenter_lock_wait_ns(), 2_000);
        assert_eq!(profile.presenter_lock_wait_max_ns(), 2_000);
    }
}
