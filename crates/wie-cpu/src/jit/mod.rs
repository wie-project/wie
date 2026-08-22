//! Hybrid Cranelift block JIT + iced interpreter fallback.
//!
//! **Strategy:** decode a lowerable block at RIP (GPR, mem, ALU, shift, call/ret,
//! jcc, SSE, bulk string); if hot enough, compile once and cache by guest entry VA.
//! Complex forms / cold sites → iced `step`.
//!
//! **Fast UCRT path:** hot CRT imports (`malloc`/`memcpy`/…) are Cranelift imports;
//! `call` to those fake-API VAs is lowered in-place (no host-stop).
//! **Block chaining:** self-loops, direct `call` to known successors, and late-bound
//! open-addressing chain-table lookups keep control in native code (no dispatcher).
//! **Shadow return stack:** `call` pushes guest return VA; `ret` validates and
//! chain-lookups the target for better call/ret prediction.

#![allow(
    unsafe_code, // Cranelift finalized fn pointers + host mem helpers
    private_interfaces // JitShared/PerThreadJitState expose crate-private types
)]

mod block;
mod config;
mod cpu_engine;
mod engine;
mod fast_api;
mod gen_tlb;
mod lower;
mod pipeline;
mod profile;
mod shared;
mod trampolines;

pub(crate) use engine::JitEngine;
pub use fast_api::{FastApiKind, JitFastPathConfig, JitHeapLayout};
pub use profile::{BgCompileProfile, JitProfile, PROFILE_BUCKETS, TimeBuckets};
pub use shared::{JitShared, PerThreadJitState};

use config::JitConfig;
use lower::CompiledBlock;
use shared::BgWaitCell;
use std::sync::Arc;

/// All 16 dirty bits set (GPR or XMM bank): the "everything is dirty" sentinel
/// for trampolines / fault paths that cannot track individual registers.
pub(super) const ALL_DIRTY_BITS: u16 = u16::MAX;

/// Hybrid CPU: Cranelift for hot pure-GPR blocks, iced for everything else.
///
/// Holds a shared compilation cache + guest memory (Arc) and per-thread
/// execution state (registers, TLB, chain table, shadow stack).
pub struct JitCpu {
    /// Shared compilation cache + guest memory (one per process).
    pub(crate) shared: Arc<JitShared>,
    /// Per-thread execution state (registers, TLB, chain table, etc.).
    pub(crate) thread: PerThreadJitState,
    /// Fake-API VA → fast UCRT kind (per-thread; configured once during init).
    pub(crate) fast_api: Vec<(u64, FastApiKind)>,
    /// Diagnostic counters (per-thread).
    pub(crate) stats: JitStats,
    /// Previous `GuestMemory::generation` at last `run_compiled` (diag).
    pub(crate) last_mem_gen: u64,
    /// Last `JitShared::cache_epoch` this thread re-synced its chain table at.
    /// `u64::MAX` means "dirty — resync on next dispatch" (set after the table
    /// is cleared).
    pub(crate) chain_sync_epoch: u64,
}

// SAFETY: Arc<JitShared> is Send + Sync (via unsafe impl above).
// PerThreadJitState is Send (raw pointers owned by one thread).
#[expect(unsafe_code)]
unsafe impl Send for JitCpu {}

#[doc(hidden)]
#[derive(Clone)]
/// Shared cache entry for one guest entry VA (`Ready` / `Never` / `Hot` / `Queued`).
pub enum CacheEntry {
    /// Native block ready to run.
    Ready(CompiledBlock),
    /// Do not retry decode/compile at this VA (cold fail or non-pure).
    Never,
    /// Visit counter + compile threshold (threshold fixed on first sight so we
    /// do not re-decode for UCRT peek on every warmup visit).
    Hot { visits: u32, thr: u32 },
    /// Enqueued for background compilation. The [`BgWaitCell`] wakes guest
    /// threads waiting specifically for this entry (the worker calls
    /// `notify_all` when it installs the Ready block or gives up).
    Queued(Arc<BgWaitCell>),
}
/// Cranelift `FuncId`s for direct UCRT host calls.
#[derive(Clone, Copy)]
pub(super) struct UcrtImportIds {
    pub malloc: cranelift_module::FuncId,
    pub free: cranelift_module::FuncId,
    pub memcpy: cranelift_module::FuncId,
    pub strlen: cranelift_module::FuncId,
    pub iob: cranelift_module::FuncId,
    pub fwrite: cranelift_module::FuncId,
    pub fflush: cranelift_module::FuncId,
}

impl UcrtImportIds {
    pub(super) fn for_kind(self, kind: FastApiKind) -> cranelift_module::FuncId {
        match kind {
            FastApiKind::Malloc => self.malloc,
            FastApiKind::Free => self.free,
            FastApiKind::Memcpy => self.memcpy,
            FastApiKind::Strlen => self.strlen,
            FastApiKind::AcrtIobFunc => self.iob,
            FastApiKind::Fwrite => self.fwrite,
            FastApiKind::Fflush => self.fflush,
        }
    }
}

/// Dump helper mem-path histogram when `WIE_JIT_MEM_TRACE=1` or `WIE_EXEC_TRACE=1`.
pub fn dump_mem_path_stats(s: &JitStats) {
    if !JitConfig::get().mem_path_trace_enabled() {
        return;
    }
    let helpers = s.load_calls.saturating_add(s.store_calls);
    tracing::error!(
        "[wie] mem_path helpers={helpers} load={} store={}",
        s.load_calls,
        s.store_calls
    );
    tracing::error!(
        "[wie]   resolve: sticky={} multi={} pin={} walk={} cross={} slow={}",
        s.mem_sticky_hit,
        s.mem_multi_hit,
        s.mem_pin_hit,
        s.mem_walk_hit,
        s.mem_cross_page,
        s.mem_slow
    );
    tracing::error!(
        "[wie]   sticky_miss: key={} gen={} prot={} swaps={}",
        s.mem_sticky_miss_key,
        s.mem_sticky_miss_gen,
        s.mem_sticky_miss_prot,
        s.mem_sticky_swaps
    );
    tracing::error!(
        "[wie]   addr_vs_pin: stack={} heap={} outside={}",
        s.mem_addr_stack_pin,
        s.mem_addr_heap_pin,
        s.mem_addr_outside
    );
    tracing::error!(
        "[wie]   gen: bumps={} peak={}  pins: stack_bytes={:#x} heap_bytes={:#x} allow={:#x}",
        s.mem_gen_bumps,
        s.mem_gen_peak,
        s.pin_stack_bytes,
        s.pin_heap_bytes,
        s.pin_allow_bits
    );
    if helpers > 0 {
        let pct10 = |n: u64| -> u64 { n.saturating_mul(1000).checked_div(helpers).unwrap_or(0) };
        let fmt = |n: u64| {
            let t = pct10(n);
            format!("{}.{}", t.checked_div(10).unwrap_or(0), t % 10)
        };
        tracing::error!(
            "[wie]   resolve%: multi={}% pin={}% walk={}% key_miss={}% outside={}%",
            fmt(s.mem_multi_hit),
            fmt(s.mem_pin_hit),
            fmt(s.mem_walk_hit),
            fmt(s.mem_sticky_miss_key),
            fmt(s.mem_addr_outside),
        );
    }
}

/// Lightweight counters for `WIE_CPU=jit` diagnostics.
#[derive(Debug, Default, Clone, Copy)]
pub struct JitStats {
    /// Instructions retired via native blocks.
    pub jit_insns: u64,
    /// Instructions retired via iced fallback.
    pub iced_insns: u64,
    /// Successful block compiles.
    pub compiles: u64,
    /// Block decode declined or cold skip.
    pub compile_skip: u64,
    /// Blocks compiled on the background worker (shared counter; merged into
    /// per-thread snapshots by [`JitCpu::stats`]).
    pub bg_compiles: u64,
    /// Times this thread waited for a background compile (any resolution).
    pub compile_stalls: u64,
    /// Total wall µs this thread spent waiting on background compiles.
    pub compile_stall_us: u64,
    /// Background waits that missed the deadline and fell back to inline
    /// compilation (worker backlog / dead worker visibility).
    pub compile_stall_fallback: u64,
    /// Cache hits (native run).
    pub cache_hits: u64,
    /// Calls into host `wie_jit_load` (TLB hit or miss).
    pub load_calls: u64,
    /// Calls into host `wie_jit_store` (TLB hit or miss).
    pub store_calls: u64,
    /// Selective code-cache invalidations (SMC / X-loss / unmap).
    pub code_invs: u64,
    /// Helper sticky hit after IR miss.
    pub mem_sticky_hit: u64,
    /// Helper multi-way TLB hit.
    pub mem_multi_hit: u64,
    /// Helper region-pin hit.
    pub mem_pin_hit: u64,
    /// Helper page-walk install hit.
    pub mem_walk_hit: u64,
    /// Helper cross-page (slow).
    pub mem_cross_page: u64,
    /// Helper full slow path (`GuestMemory::{read,write}`).
    pub mem_slow: u64,
    /// Sticky miss reason: wrong/empty page key.
    pub mem_sticky_miss_key: u64,
    /// Sticky miss reason: generation mismatch.
    pub mem_sticky_miss_gen: u64,
    /// Sticky miss reason: R/W denied.
    pub mem_sticky_miss_prot: u64,
    /// Sticky hot-page replacements.
    pub mem_sticky_swaps: u64,
    /// Helper VA inside stack pin.
    pub mem_addr_stack_pin: u64,
    /// Helper VA inside heap pin.
    pub mem_addr_heap_pin: u64,
    /// Helper VA outside both pins.
    pub mem_addr_outside: u64,
    /// Times `GuestMemory::generation` increased between `run_compiled` entries.
    pub mem_gen_bumps: u64,
    /// Peak `mem_gen` observed.
    pub mem_gen_peak: u64,
    /// Last stack pin guest span size (0 if empty).
    pub pin_stack_bytes: u64,
    /// Last heap pin guest span size (0 if empty).
    pub pin_heap_bytes: u64,
    /// Last pin allow bits: bit0 stack R, bit1 stack W, bit2 heap R, bit3 heap W.
    pub pin_allow_bits: u64,
    /// Adaptive-JIT cost-model instrumentation (timing + decision counters).
    pub profile: JitProfile,
}
#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    mod chain_tests;
    mod strlen_repro;
    use super::*;
    use crate::exec::StepResult;
    use crate::mem::protect;
    use crate::mem::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE};
    use crate::regs::RegFile;
    use crate::{CpuEngine, RwxPerms};
    use block::BlockKind;
    use config::JitConfig;
    use lower::{JitCtx, chain_table_insert};
    use pipeline::ranges_overlap;
    use shared::{BgEnqueueOutcome, BgWaitCell};
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    unsafe extern "C" fn dummy_block(_ctx: *mut JitCtx) {}

    impl JitCpu {
        /// Test helper: plant a Ready entry without Cranelift.
        fn test_plant_ready(&mut self, rip: u64, guest_end: u64) {
            self.insert_ready(
                rip,
                CompiledBlock {
                    func: dummy_block,
                    func_id: None,
                    insn_count: 1,
                    uses_sse: false,
                    xmm_live_mask: 0,
                    xmm_may_def_mask: 0,
                    guest_start: rip,
                    guest_end,
                },
            );
            if JitConfig::get().chain_enabled() {
                let fn_ptr = dummy_block as *const () as usize as u64;
                chain_table_insert(self.thread.chain_slots.as_mut(), rip, fn_ptr);
            }
            // Simulate edge IC hit for S6.
            self.thread.edge_ic_va[0] = rip;
            self.thread.edge_ic_fn[0] = dummy_block as *const () as usize as u64;
        }
    }

    #[test]
    fn code_inv_x_loss_drops_ready() {
        let mut cpu = JitCpu::open_x86_64();
        let base = 0x1000_0000_u64;
        cpu.virtual_alloc(
            base,
            0x1000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_EXECUTE_READWRITE,
        )
        .expect("alloc");
        cpu.test_plant_ready(base, base + 16);
        assert!(cpu.has_ready_at(base));
        assert!(cpu.code_pages_overlap(base, 16));

        cpu.virtual_protect(base, 0x1000, protect::PAGE_READONLY)
            .expect("x-loss");
        assert!(!cpu.has_ready_at(base));
        assert!(!cpu.code_pages_overlap(base, 16));
        assert_eq!(cpu.thread.edge_ic_va[0], 0);
        assert!(cpu.stats().code_invs >= 1);
    }

    #[test]
    fn code_inv_smc_write_drops_ready() {
        let mut cpu = JitCpu::open_x86_64();
        let base = 0x1001_0000_u64;
        cpu.virtual_alloc(
            base,
            0x1000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_EXECUTE_READWRITE,
        )
        .expect("alloc");
        cpu.test_plant_ready(base + 0x10, base + 0x20);
        assert!(cpu.has_ready_at(base + 0x10));

        // Guest/host store into compiled range.
        cpu.mem_write(base + 0x12, &[0x90, 0x90]).expect("smc");
        assert!(!cpu.has_ready_at(base + 0x10));
        assert_eq!(cpu.thread.edge_ic_fn[0], 0);
    }

    #[test]
    fn code_inv_data_write_leaves_code() {
        let mut cpu = JitCpu::open_x86_64();
        let code = 0x1002_0000_u64;
        let data = 0x1003_0000_u64;
        cpu.virtual_alloc(
            code,
            0x1000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_EXECUTE_READWRITE,
        )
        .expect("code");
        cpu.virtual_alloc(
            data,
            0x1000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_READWRITE,
        )
        .expect("data");
        cpu.test_plant_ready(code, code + 8);
        let invs = cpu.stats().code_invs;
        cpu.mem_write(data, &[1, 2, 3, 4]).expect("data write");
        assert!(cpu.has_ready_at(code));
        assert_eq!(cpu.stats().code_invs, invs);
        assert_eq!(cpu.thread.edge_ic_va[0], code);
    }

    #[test]
    fn code_inv_free_drops_ready() {
        let mut cpu = JitCpu::open_x86_64();
        let base = 0x1004_0000_u64;
        cpu.virtual_alloc(
            base,
            0x1000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_EXECUTE_READWRITE,
        )
        .expect("alloc");
        cpu.test_plant_ready(base, base + 4);
        cpu.virtual_free(base, 0, MEM_RELEASE).expect("free");
        assert!(!cpu.has_ready_at(base));
        assert!(cpu.shared.code_pages.lock().unwrap().is_empty());
    }

    #[test]
    fn code_inv_rx_stays_on_x_preserve() {
        let mut cpu = JitCpu::open_x86_64();
        let base = 0x1005_0000_u64;
        cpu.virtual_alloc(
            base,
            0x1000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_EXECUTE_READWRITE,
        )
        .expect("alloc");
        cpu.test_plant_ready(base, base + 8);
        // Keep execute; only drop write — code content unchanged.
        cpu.virtual_protect(base, 0x1000, protect::PAGE_EXECUTE_READ)
            .expect("rx");
        assert!(cpu.has_ready_at(base));
    }

    #[test]
    fn ranges_overlap_half_open() {
        assert!(ranges_overlap(0x10, 0x20, 0x1f, 0x30));
        assert!(!ranges_overlap(0x10, 0x20, 0x20, 0x30));
        assert!(ranges_overlap(0x10, 0x20, 0x00, 0x11));
    }

    #[test]
    fn no_w_tlb_on_executable_page() {
        let mut cpu = JitCpu::open_x86_64();
        let base = 0x1006_0000_u64;
        cpu.virtual_alloc(
            base,
            0x1000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_EXECUTE_READWRITE,
        )
        .expect("alloc");
        let e = cpu
            .shared
            .mem
            .write()
            .unwrap()
            .page_tlb_entry(base >> 12)
            .expect("tlb");
        assert!(e.allow_r);
        assert!(!e.allow_w);
        let data = 0x1007_0000_u64;
        cpu.virtual_alloc(
            data,
            0x1000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_READWRITE,
        )
        .expect("data");
        let e2 = cpu
            .shared
            .mem
            .write()
            .unwrap()
            .page_tlb_entry(data >> 12)
            .expect("data tlb");
        assert!(e2.allow_r && e2.allow_w);
        let _ = RwxPerms::ALL; // silence if unused in some cfgs
    }

    // --- Stress residual (invalidation multi-region / FIC) ---

    #[test]
    fn code_inv_smc_across_page_boundary() {
        let mut cpu = JitCpu::open_x86_64();
        let base = 0x1010_0000_u64;
        cpu.virtual_alloc(
            base,
            0x2000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_EXECUTE_READWRITE,
        )
        .expect("alloc 2 pages");
        // Ready block straddles page boundary (last 8 B of page0 + first of page1).
        let entry = base + 0x0ff8;
        cpu.test_plant_ready(entry, entry + 16);
        assert!(cpu.has_ready_at(entry));
        // Store on page1 half of the range.
        cpu.mem_write(base + 0x1000, &[0x90, 0x90]).expect("smc p1");
        assert!(!cpu.has_ready_at(entry));
        assert_eq!(cpu.thread.edge_ic_va[0], 0);
    }

    #[test]
    fn code_inv_multi_region_protect_and_free() {
        let mut cpu = JitCpu::open_x86_64();
        let a = 0x1011_0000_u64;
        let b = 0x1012_0000_u64;
        for base in [a, b] {
            cpu.virtual_alloc(
                base,
                0x1000,
                MEM_RESERVE | MEM_COMMIT,
                protect::PAGE_EXECUTE_READWRITE,
            )
            .expect("alloc");
            cpu.test_plant_ready(base, base + 8);
        }
        assert!(cpu.has_ready_at(a) && cpu.has_ready_at(b));
        // X-loss on A only.
        cpu.virtual_protect(a, 0x1000, protect::PAGE_READONLY)
            .expect("protect a");
        assert!(!cpu.has_ready_at(a));
        assert!(cpu.has_ready_at(b));
        assert_eq!(cpu.thread.edge_ic_va[0], 0); // edge IC cleared on any selective drop
        // Free B.
        cpu.virtual_free(b, 0, MEM_RELEASE).expect("free b");
        assert!(!cpu.has_ready_at(b));
    }

    #[test]
    fn flush_instruction_cache_drops_ready_range() {
        let mut cpu = JitCpu::open_x86_64();
        let base = 0x1013_0000_u64;
        cpu.virtual_alloc(
            base,
            0x1000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_EXECUTE_READWRITE,
        )
        .expect("alloc");
        cpu.test_plant_ready(base, base + 16);
        assert!(cpu.has_ready_at(base));
        cpu.flush_instruction_cache(base, 16).expect("fic");
        assert!(!cpu.has_ready_at(base));
        assert!(cpu.stats().code_invs >= 1);
    }

    #[test]
    fn flush_instruction_cache_size_zero_clears_all() {
        let mut cpu = JitCpu::open_x86_64();
        let a = 0x1014_0000_u64;
        let b = 0x1015_0000_u64;
        for base in [a, b] {
            cpu.virtual_alloc(
                base,
                0x1000,
                MEM_RESERVE | MEM_COMMIT,
                protect::PAGE_EXECUTE_READWRITE,
            )
            .expect("alloc");
            cpu.test_plant_ready(base, base + 4);
        }
        cpu.flush_instruction_cache(0, 0).expect("fic all");
        assert!(!cpu.has_ready_at(a));
        assert!(!cpu.has_ready_at(b));
        assert!(cpu.shared.code_pages.lock().unwrap().is_empty());
    }

    // --- Background compiler (B10) ---

    #[test]
    fn bg_worker_end_to_end_step() {
        // Force the background path on for this engine only (parallel tests
        // keep their own inline default).
        let mut cpu = JitCpu::open_x86_64();
        cpu.shared.bg_force.store(true, Ordering::Relaxed);
        let base = 0x1020_0000_u64;
        cpu.virtual_alloc(
            base,
            0x1000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_EXECUTE_READWRITE,
        )
        .expect("alloc");
        // `mov eax, 0x2a; nop` then `ud2` — a 2-insn pure fallthrough block.
        // The trailing `ud2` stops the linear decode (zero-filled pages would
        // otherwise decode as `add [rax],al` for the full 96-insn budget).
        cpu.mem_write(base, &[0xb8, 0x2a, 0x00, 0x00, 0x00, 0x90, 0x0f, 0x0b])
            .expect("write");
        cpu.write_rip(base).expect("rip");

        // Eager first visit (hotness 0 in tests): enqueue + wait + run compiled.
        // The block may run via the worker's Ready install OR the inline
        // fallback if the worker misses the wait budget — both produce the same
        // executed code, so only the outcomes are asserted.
        let (result, _retired) = cpu.step_one().expect("step");
        assert!(matches!(result, StepResult::Continue));
        assert_eq!(cpu.thread.regs.rax(), 0x2a);
        assert!(cpu.has_ready_at(base), "a Ready block must be installed");
        // The worker processes the queued job regardless of who won the race;
        // poll (bounded) for its install so the shared counters are settled.
        let deadline = Instant::now() + Duration::from_secs(5);
        while cpu.stats().bg_compiles < 1 {
            assert!(Instant::now() < deadline, "worker install timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(cpu.shared.chain_ids.pin().contains_key(&base));
        assert!(
            cpu.shared.cache_epoch.load(Ordering::Relaxed) >= 1,
            "worker install must bump the chain-sync epoch"
        );
        assert!(
            cpu.shared
                .pending_code_writes
                .lock()
                .unwrap()
                .iter()
                .all(|page| *page != base >> 12),
            "a fresh compile must not leave a pending SMC page"
        );
        // Chain-table re-sync picks the worker-installed block up.
        cpu.chain_sync_epoch = 0;
        cpu.resync_chain_table();
        assert!(
            cpu.thread
                .chain_slots
                .iter()
                .any(|s| s.va == base && s.fn_ptr != 0),
            "resync must chain worker-installed blocks"
        );
    }

    #[test]
    fn bg_worker_notpure_becomes_never() {
        let mut cpu = JitCpu::open_x86_64();
        cpu.shared.bg_force.store(true, Ordering::Relaxed);
        let base = 0x1021_0000_u64;
        let outcome = cpu.enqueue_bg(base, &BlockKind::NotPure);
        assert!(matches!(outcome, BgEnqueueOutcome::Unavailable));
        assert!(matches!(
            cpu.shared.cache.pin().get(&base),
            Some(CacheEntry::Never)
        ));
    }

    #[test]
    fn bg_wait_returns_none_without_worker() {
        // No worker spawned (fresh engine, nothing enqueued): waiting must
        // return immediately so the caller falls back to inline compilation.
        let mut cpu = JitCpu::open_x86_64();
        let cell = BgWaitCell::new();
        let r = cpu.wait_bg_ready(0x1022_0000_u64, &cell);
        assert!(r.is_none());
        assert_eq!(cpu.stats().compile_stall_fallback, 0);
    }

    #[test]
    fn bg_worker_dedups_queued_entry() {
        let mut cpu = JitCpu::open_x86_64();
        cpu.shared.bg_force.store(true, Ordering::Relaxed);
        let base = 0x1023_0000_u64;
        cpu.virtual_alloc(
            base,
            0x1000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_EXECUTE_READWRITE,
        )
        .expect("alloc");
        cpu.mem_write(base, &[0xb8, 0x2a, 0x00, 0x00, 0x00, 0x90, 0x0f, 0x0b])
            .expect("write");
        let kind = {
            let mem = cpu.shared.mem.read().unwrap();
            block::decode_pure_gpr_block(&mem, cpu.thread.hooks.as_ref(), base)
        };
        assert!(matches!(kind, BlockKind::Pure { .. }));
        let first = cpu.enqueue_bg(base, &kind);
        assert!(matches!(first, BgEnqueueOutcome::Queued(_)));
        // A second enqueue of the same rip must not re-queue: either the entry
        // is still Queued (dedup → Unavailable) or the worker already won
        // (Ready). Never a fresh Queued cell.
        let second = cpu.enqueue_bg(base, &kind);
        assert!(
            matches!(
                second,
                BgEnqueueOutcome::Unavailable | BgEnqueueOutcome::Ready
            ),
            "re-enqueue of a queued rip must not re-queue"
        );
    }

    // --- Adaptive-JIT hotness threshold ---

    #[test]
    fn hot_threshold_crossing_compiles_and_invalidates() {
        let mut cpu = JitCpu::open_x86_64();
        let base = 0x1031_0000_u64;
        cpu.virtual_alloc(
            base,
            0x1000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_EXECUTE_READWRITE,
        )
        .expect("alloc");
        cpu.mem_write(base, &[0xb8, 0x2a, 0x00, 0x00, 0x00, 0x90, 0x0f, 0x0b])
            .expect("write");
        cpu.write_rip(base).expect("rip");

        // Plant a Hot entry with a cost-model-style threshold of 3: the first
        // visit (visits=1 → next=2 < 3) must not cross; the second must.
        cpu.shared
            .cache
            .pin()
            .insert(base, CacheEntry::Hot { visits: 1, thr: 3 });
        // Visit 1: not crossed → stays Hot, runs one iced insn (RIP advances).
        cpu.write_rip(base).expect("rip");
        let (result, _) = cpu.step_one().expect("step 1");
        assert!(matches!(result, StepResult::Continue));
        assert!(!cpu.has_ready_at(base), "threshold not crossed on visit 1");
        assert!(matches!(
            cpu.shared.cache.pin().get(&base),
            Some(CacheEntry::Hot { visits: 2, .. })
        ));

        // Visit 2: next=3, not < 3 → crossed → compile.
        cpu.write_rip(base).expect("rip");
        let (result, _) = cpu.step_one().expect("step 2");
        assert!(matches!(result, StepResult::Continue));
        assert!(cpu.has_ready_at(base), "threshold crossed → compiled");

        // Invalidation clears the compiled block (SMC / unmap path).
        cpu.invalidate_code_range(base, 8);
        assert!(!cpu.has_ready_at(base), "invalidation must drop the block");
    }

    // --- B4: integer-SIMD JIT family — iced vs JIT dual-path gates ---
    //
    // Every newly-lowered SSE2 mnemonic runs once on the iced interpreter
    // (reference) and once through a compiled JIT block with identical guest
    // state; the two register files must match exactly (GPRs, XMMs, RFLAGS,
    // RIP). The `has_ready_at` assertion guarantees the block actually went
    // through Cranelift rather than silently falling back to iced.

    use crate::IcedCpu;
    use crate::regs::Rflags;
    use iced_x86::{Decoder, DecoderOptions, Register};

    const SIMD_BASE: u64 = 0x2000_0000;
    const SIMD_DATA: u64 = SIMD_BASE + 0x1000;

    /// Number of instructions `bytes` decodes to, stopping at the trailing
    /// `ud2` terminator (test-encoding sanity).
    fn decode_count(bytes: &[u8]) -> usize {
        let mut dec = Decoder::with_ip(64, bytes, 0, DecoderOptions::NONE);
        let mut n = 0;
        while dec.position() < bytes.len() {
            let insn = dec.decode();
            assert!(
                !insn.is_invalid(),
                "invalid test encoding at {n}: {bytes:02x?}"
            );
            if insn.mnemonic() == iced_x86::Mnemonic::Ud2 {
                break;
            }
            n += 1;
        }
        n
    }

    /// Run `code` (a single op; a trailing `nop; ud2` is appended to reach the
    /// 2-insn compile minimum and to stop linear decode past our bytes — the
    /// zero-filled tail would otherwise decode as `add [rax], al` and extend
    /// the block, faulting on the unmapped address) on iced and the JIT with
    /// identical guest state.
    fn simd_dual(code: &[u8], data: &[u8], setup: impl Fn(&mut RegFile)) -> (RegFile, RegFile) {
        let mut full = Vec::with_capacity(code.len() + 3);
        full.extend_from_slice(code);
        full.extend_from_slice(&[0x90, 0x0f, 0x0b]); // nop filler + ud2 terminator
        let n_insns = decode_count(&full);

        // --- iced reference ---
        // The iced decode cache is thread-local and keyed by (rip, mem_gen);
        // fresh test CPUs all share mem_gen 0, so flush or a later case at the
        // same base would decode the previous case's instruction.
        crate::exec::iced_decode_cache_flush();
        let mut iced = IcedCpu::open_x86_64();
        iced.virtual_alloc(
            SIMD_BASE,
            0x2000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_EXECUTE_READWRITE,
        )
        .expect("iced alloc");
        iced.mem_write(SIMD_BASE, &full).expect("iced code");
        iced.mem_write(SIMD_DATA, data).expect("iced data");
        setup(iced.regs_mut());
        iced.write_rip(SIMD_BASE).expect("iced rip");
        for _ in 0..n_insns {
            iced.step_once().expect("iced step");
        }

        // --- JIT (eager compile: hotness is 0 under cfg(test)) ---
        let mut cpu = JitCpu::open_x86_64();
        cpu.virtual_alloc(
            SIMD_BASE,
            0x2000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_EXECUTE_READWRITE,
        )
        .expect("jit alloc");
        cpu.mem_write(SIMD_BASE, &full).expect("jit code");
        cpu.mem_write(SIMD_DATA, data).expect("jit data");
        setup(&mut cpu.thread.regs);
        cpu.write_rip(SIMD_BASE).expect("jit rip");
        let (result, _retired) = cpu.step_one().expect("jit step");
        assert!(
            matches!(result, StepResult::Continue),
            "jit result {result:?}"
        );
        assert!(
            cpu.has_ready_at(SIMD_BASE),
            "block must compile, not run iced"
        );
        assert_eq!(
            cpu.stats().iced_insns,
            0,
            "block ran on iced instead of JIT"
        );

        (iced.regs().clone(), cpu.thread.regs.clone())
    }

    fn assert_same_regs(iced: &RegFile, jit: &RegFile, what: &str) {
        for i in 0..16 {
            assert_eq!(iced.gpr(i), jit.gpr(i), "{what}: gpr[{i}]");
            assert_eq!(iced.xmm_at(i), jit.xmm_at(i), "{what}: xmm[{i}]");
        }
        assert_eq!(iced.rflags, jit.rflags, "{what}: rflags");
        assert_eq!(iced.rip, jit.rip, "{what}: rip");
    }

    fn set_pair(regs: &mut RegFile, x0: u128, x1: u128) {
        regs.write_xmm(Register::XMM0, x0).expect("xmm0");
        regs.write_xmm(Register::XMM1, x1).expect("xmm1");
    }

    #[test]
    fn simd_packed_arith_matches_iced() {
        // Carry / borrow / sign patterns across every lane width.
        let x0 = 0xF1E2_D3C4_B5A6_9788_7766_5544_3322_1100_u128;
        let x1 = 0x0102_0304_0506_0708_090A_0B0C_0D0E_0F10_u128;
        for (name, bytes) in [
            ("paddb", &[0x66, 0x0f, 0xfc, 0xc1][..]),
            ("paddw", &[0x66, 0x0f, 0xfd, 0xc1][..]),
            ("paddd", &[0x66, 0x0f, 0xfe, 0xc1][..]),
            ("paddq", &[0x66, 0x0f, 0xd4, 0xc1][..]),
            ("psubb", &[0x66, 0x0f, 0xf8, 0xc1][..]),
            ("psubw", &[0x66, 0x0f, 0xf9, 0xc1][..]),
            ("psubd", &[0x66, 0x0f, 0xfa, 0xc1][..]),
            ("psubq", &[0x66, 0x0f, 0xfb, 0xc1][..]),
        ] {
            let (iced, jit) = simd_dual(bytes, &[], |r| set_pair(r, x0, x1));
            assert_same_regs(&iced, &jit, name);
        }
        // Hand-check PADDD (lane-wise 32-bit adds).
        let a = 0x0000_0002_0000_0001_0000_0002_0000_0001_u128;
        let b = 0x0000_0004_0000_0003_0000_0004_0000_0003_u128;
        let (iced, jit) = simd_dual(&[0x66, 0x0f, 0xfe, 0xc1], &[], |r| set_pair(r, a, b));
        let want = 0x0000_0006_0000_0004_0000_0006_0000_0004_u128;
        assert_eq!(iced.xmm_at(0), want, "iced paddd");
        assert_eq!(jit.xmm_at(0), want, "jit paddd");
    }

    /// Reference `PMOVMSKB` mask for a 128-bit xmm value (byte i sign → bit i).
    fn pmovmskb_expected(x: u128) -> u64 {
        let mut mask = 0_u64;
        for i in 0_u64..16 {
            if (x >> (i.saturating_mul(8))) & 0x80 != 0 {
                mask |= 1_u64 << i;
            }
        }
        mask
    }

    #[test]
    fn simd_pmovmskb_matches_iced() {
        // PMOVMSKB eax, xmm1 (66 0F D7 C1): pack the 16 byte-sign bits of
        // xmm1 into the low 16 bits of eax; the upper GPR bits are zeroed.
        // Low 8 bytes 0x80 → bits 0-7 set; high 8 bytes 0x00 → bits 8-15 clear.
        let x1 = 0x0000_0000_0000_0000_8080_8080_8080_8080_u128;
        let (iced, jit) = simd_dual(&[0x66, 0x0f, 0xd7, 0xc1], &[], |r| set_pair(r, 0, x1));
        assert_same_regs(&iced, &jit, "pmovmskb");
        assert_eq!(iced.gpr(0), 0xFF, "iced pmovmskb mask");
        assert_eq!(jit.gpr(0), 0xFF, "jit pmovmskb mask");

        // Every byte 0xFF → all 16 sign bits set; RAX stays 32-bit zero-extended.
        let (iced, jit) = simd_dual(&[0x66, 0x0f, 0xd7, 0xc1], &[], |r| {
            set_pair(r, 0, u128::MAX)
        });
        assert_same_regs(&iced, &jit, "pmovmskb all-set");
        assert_eq!(iced.gpr(0), 0xFFFF, "iced pmovmskb all-set");
        assert_eq!(jit.gpr(0), 0xFFFF, "jit pmovmskb all-set");
    }

    #[test]
    fn simd_vpmovmskb_matches_iced() {
        // VEX VPMOVMSKB eax, xmm1 (C5 F9 D7 C1) — same semantics, own mnemonic.
        let x1 = 0x807F_0080_FF00_0001_8000_7F7F_8080_0100_u128;
        let (iced, jit) = simd_dual(&[0xc5, 0xf9, 0xd7, 0xc1], &[], |r| set_pair(r, 0, x1));
        assert_same_regs(&iced, &jit, "vpmovmskb");
        let want = pmovmskb_expected(x1);
        assert_eq!(iced.gpr(0), want, "iced vpmovmskb");
        assert_eq!(jit.gpr(0), want, "jit vpmovmskb");
    }

    #[test]
    fn gpr_bsr_bsf_matches_iced() {
        // BSR/BSF: the index of the most/least significant set bit;
        // src == 0 → ZF=1, dst written 0.
        // 32-bit form (0F BD C1 / 0F BC C1 — eax, ecx) and the REX.W 64-bit
        // form (48 0F BD C1 / 48 0F BC C1 — rax, rcx).
        for (name, bytes32, bytes64) in [
            (
                "bsr",
                &[0x0f, 0xbd, 0xc1][..],
                &[0x48, 0x0f, 0xbd, 0xc1][..],
            ),
            (
                "bsf",
                &[0x0f, 0xbc, 0xc1][..],
                &[0x48, 0x0f, 0xbc, 0xc1][..],
            ),
        ] {
            for (label, rcx) in [
                ("low", 0x1_u64),
                ("mid", 0x0400_0000_u64),
                ("high", 0x8000_0000_u64),
                ("zero", 0_u64),
            ] {
                let (iced, jit) = simd_dual(bytes32, &[], |r| {
                    r.set_gpr(1, rcx);
                });
                assert_same_regs(&iced, &jit, name);
                // 63 − leading_zeros works for both widths: the 32-bit operand's
                // zero-extended top half is counted by clz and cancels.
                let want = if rcx == 0 {
                    0
                } else if name == "bsr" {
                    u64::from(63_u32).saturating_sub(u64::from(rcx.leading_zeros()))
                } else {
                    u64::from(rcx.trailing_zeros())
                };
                assert_eq!(iced.gpr(0), want, "iced {name} 32-bit {label}");
                assert_eq!(jit.gpr(0), want, "jit {name} 32-bit {label}");
            }
            for (label, rcx) in [
                ("low", 0x1_u64),
                ("high", 0x8000_0000_0000_0000_u64),
                ("zero", 0_u64),
            ] {
                let (iced, jit) = simd_dual(bytes64, &[], |r| {
                    r.set_gpr(1, rcx);
                });
                assert_same_regs(&iced, &jit, name);
                let want = if rcx == 0 {
                    0
                } else if name == "bsr" {
                    u64::from(63_u32).saturating_sub(u64::from(rcx.leading_zeros()))
                } else {
                    u64::from(rcx.trailing_zeros())
                };
                assert_eq!(iced.gpr(0), want, "iced {name} 64-bit {label}");
                assert_eq!(jit.gpr(0), want, "jit {name} 64-bit {label}");
            }
        }
    }

    #[test]
    fn simd_saturating_arith_matches_iced() {
        // Signed/unsigned saturation edges: 0x7F+1, 0x80+0x80, 0x00-1, 0xFFFF+1.
        let x0 = 0x0000_0000_0000_0000_7F7F_7F80_0100_807F_u128;
        let x1 = 0x0000_0000_0000_0000_0101_0101_FFFF_8080_u128;
        for (name, bytes) in [
            ("paddsb", &[0x66, 0x0f, 0xec, 0xc1][..]),
            ("paddsw", &[0x66, 0x0f, 0xed, 0xc1][..]),
            ("paddusb", &[0x66, 0x0f, 0xdc, 0xc1][..]),
            ("paddusw", &[0x66, 0x0f, 0xdd, 0xc1][..]),
            ("psubsb", &[0x66, 0x0f, 0xe8, 0xc1][..]),
            ("psubsw", &[0x66, 0x0f, 0xe9, 0xc1][..]),
            ("psubusb", &[0x66, 0x0f, 0xd8, 0xc1][..]),
            ("psubusw", &[0x66, 0x0f, 0xd9, 0xc1][..]),
        ] {
            let (iced, jit) = simd_dual(bytes, &[], |r| set_pair(r, x0, x1));
            assert_same_regs(&iced, &jit, name);
        }
        // Hand-check PADDSB low byte: 0x7F + 0x01 → 0x7F (signed sat).
        let a = 0x0000_0000_0000_0000_0000_0000_0000_007F_u128;
        let b = 0x0000_0000_0000_0000_0000_0000_0000_0001_u128;
        let (iced, jit) = simd_dual(&[0x66, 0x0f, 0xec, 0xc1], &[], |r| set_pair(r, a, b));
        assert_eq!(iced.xmm_at(0) & 0xff, 0x7f, "iced paddsb sat");
        assert_eq!(jit.xmm_at(0) & 0xff, 0x7f, "jit paddsb sat");
        // PADDUSB: 0xFF + 0x01 → 0xFF (unsigned sat).
        let a = 0x0000_0000_0000_0000_0000_0000_0000_00FF_u128;
        let (iced, jit) = simd_dual(&[0x66, 0x0f, 0xdc, 0xc1], &[], |r| set_pair(r, a, b));
        assert_eq!(iced.xmm_at(0) & 0xff, 0xff, "iced paddusb sat");
        assert_eq!(jit.xmm_at(0) & 0xff, 0xff, "jit paddusb sat");
    }

    #[test]
    fn simd_multiply_matches_iced() {
        let x0 = 0x0001_0002_0003_0004_0005_0006_0007_0008_u128;
        let x1 = 0x8000_7FFF_0100_0200_1000_2000_4000_8000_u128;
        for (name, bytes) in [
            ("pmullw", &[0x66, 0x0f, 0xd5, 0xc1][..]),
            ("pmulhw", &[0x66, 0x0f, 0xe5, 0xc1][..]),
            ("pmulhuw", &[0x66, 0x0f, 0xe4, 0xc1][..]),
            ("pmuludq", &[0x66, 0x0f, 0xf4, 0xc1][..]),
            ("pmaddwd", &[0x66, 0x0f, 0xf5, 0xc1][..]),
        ] {
            let (iced, jit) = simd_dual(bytes, &[], |r| set_pair(r, x0, x1));
            assert_same_regs(&iced, &jit, name);
        }
        // Hand-check PMULUDQ: qword0 = dword0(a)*dword0(b), qword1 = dword2.
        let a = 0x0000_0000_0000_0002_0000_0000_0000_0003_u128;
        let b = 0x0000_0000_0000_0004_0000_0000_0000_0005_u128;
        let (iced, jit) = simd_dual(&[0x66, 0x0f, 0xf4, 0xc1], &[], |r| set_pair(r, a, b));
        let want = 0x0000_0000_0000_0008_0000_0000_0000_000F_u128;
        assert_eq!(iced.xmm_at(0), want, "iced pmuludq");
        assert_eq!(jit.xmm_at(0), want, "jit pmuludq");
    }

    #[test]
    fn simd_compare_matches_iced() {
        let x0 = 0x8180_7F00_0100_FFFF_807F_0001_8000_7FFF_u128;
        let x1 = 0x7F7F_7F7F_0101_FFFF_8080_0000_7FFF_8000_u128;
        for (name, bytes) in [
            ("pcmpeqb", &[0x66, 0x0f, 0x74, 0xc1][..]),
            ("pcmpeqw", &[0x66, 0x0f, 0x75, 0xc1][..]),
            ("pcmpeqd", &[0x66, 0x0f, 0x76, 0xc1][..]),
            ("pcmpgtb", &[0x66, 0x0f, 0x64, 0xc1][..]),
            ("pcmpgtw", &[0x66, 0x0f, 0x65, 0xc1][..]),
            ("pcmpgtd", &[0x66, 0x0f, 0x66, 0xc1][..]),
        ] {
            let (iced, jit) = simd_dual(bytes, &[], |r| set_pair(r, x0, x1));
            assert_same_regs(&iced, &jit, name);
        }
        // Hand-check PCMPGTD: 0x8000_0000 > 0x7FFF_FFFF is false (signed).
        let a = 0x0000_0000_0000_0000_0000_0000_8000_0000_u128;
        let b = 0x0000_0000_0000_0000_0000_0000_7FFF_FFFF_u128;
        let (iced, jit) = simd_dual(&[0x66, 0x0f, 0x66, 0xc1], &[], |r| set_pair(r, a, b));
        assert_eq!(iced.xmm_at(0) & 0xffff_ffff, 0, "iced pcmpgtd signed");
        assert_eq!(jit.xmm_at(0) & 0xffff_ffff, 0, "jit pcmpgtd signed");
    }

    #[test]
    fn simd_shifts_matches_iced() {
        let x0 = 0xF0E0_D0C0_B0A0_9080_7060_5040_3020_1000_u128;
        // imm forms: dst xmm0, imm8 = 5.
        for (name, bytes) in [
            ("psllw imm", &[0x66, 0x0f, 0x71, 0xf0, 0x05][..]),
            ("pslld imm", &[0x66, 0x0f, 0x72, 0xf0, 0x05][..]),
            ("psllq imm", &[0x66, 0x0f, 0x73, 0xf0, 0x05][..]),
            ("psrlw imm", &[0x66, 0x0f, 0x71, 0xd0, 0x05][..]),
            ("psrld imm", &[0x66, 0x0f, 0x72, 0xd0, 0x05][..]),
            ("psrlq imm", &[0x66, 0x0f, 0x73, 0xd0, 0x05][..]),
            ("psraw imm", &[0x66, 0x0f, 0x71, 0xe0, 0x05][..]),
            ("psrad imm", &[0x66, 0x0f, 0x72, 0xe0, 0x05][..]),
        ] {
            let (iced, jit) = simd_dual(bytes, &[], |r| set_pair(r, x0, 0));
            assert_same_regs(&iced, &jit, name);
        }
        // variable forms: count lanes in xmm1.
        let cnt = 0x0000_0000_0000_0000_0004_0000_0010_0010_u128;
        for (name, bytes) in [
            ("psllw var", &[0x66, 0x0f, 0xf1, 0xc1][..]),
            ("pslld var", &[0x66, 0x0f, 0xf2, 0xc1][..]),
            ("psllq var", &[0x66, 0x0f, 0xf3, 0xc1][..]),
            ("psrlw var", &[0x66, 0x0f, 0xd1, 0xc1][..]),
            ("psrld var", &[0x66, 0x0f, 0xd2, 0xc1][..]),
            ("psrlq var", &[0x66, 0x0f, 0xd3, 0xc1][..]),
            ("psraw var", &[0x66, 0x0f, 0xe1, 0xc1][..]),
            ("psrad var", &[0x66, 0x0f, 0xe2, 0xc1][..]),
        ] {
            let (iced, jit) = simd_dual(bytes, &[], |r| set_pair(r, x0, cnt));
            assert_same_regs(&iced, &jit, name);
        }
        // Hand-check PSRLD imm 4: 0x0000_0000_1000_0000 → 0x0000_0000_0100_0000.
        let a = 0x0000_0000_0000_0000_0000_0000_1000_0000_u128;
        let (iced, jit) = simd_dual(&[0x66, 0x0f, 0x72, 0xd0, 0x04], &[], |r| set_pair(r, a, 0));
        assert_eq!(iced.xmm_at(0) & 0xffff_ffff, 0x0100_0000, "iced psrld");
        assert_eq!(jit.xmm_at(0) & 0xffff_ffff, 0x0100_0000, "jit psrld");
        // Oversized count (32 ≥ 32) → 0 per x86.
        let big = 0x0020_0020_0020_0020_0020_0020_0020_0020_u128;
        let (iced, jit) = simd_dual(&[0x66, 0x0f, 0xf2, 0xc1], &[], |r| set_pair(r, x0, big));
        assert_eq!(iced.xmm_at(0), 0, "iced pslld big-count zeroes");
        assert_eq!(jit.xmm_at(0), 0, "jit pslld big-count zeroes");
    }

    #[test]
    fn simd_byte_shifts_match_iced() {
        // Psrldq/Pslldq: whole-128-bit byte shifts (66 0F 73 /3 ib and /7 ib).
        let x0 = 0xF0E0_D0C0_B0A0_9080_7060_5040_3020_1000_u128;
        for (name, bytes) in [
            ("psrldq imm1", &[0x66, 0x0f, 0x73, 0xd8, 0x01][..]),
            ("psrldq imm8", &[0x66, 0x0f, 0x73, 0xd8, 0x08][..]),
            ("psrldq imm15", &[0x66, 0x0f, 0x73, 0xd8, 0x0f][..]),
            ("psrldq imm16", &[0x66, 0x0f, 0x73, 0xd8, 0x10][..]),
            ("pslldq imm1", &[0x66, 0x0f, 0x73, 0xf8, 0x01][..]),
            ("pslldq imm8", &[0x66, 0x0f, 0x73, 0xf8, 0x08][..]),
            ("pslldq imm15", &[0x66, 0x0f, 0x73, 0xf8, 0x0f][..]),
            ("pslldq imm16", &[0x66, 0x0f, 0x73, 0xf8, 0x10][..]),
        ] {
            let (iced, jit) = simd_dual(bytes, &[], |r| set_pair(r, x0, 0));
            assert_same_regs(&iced, &jit, name);
        }
        // Hand-check PSRLDQ imm1: byte 0 dropped, zero-filled on the left.
        let (iced, jit) = simd_dual(&[0x66, 0x0f, 0x73, 0xd8, 0x01], &[], |r| set_pair(r, x0, 0));
        assert_eq!(
            iced.xmm_at(0),
            0x00F0_E0D0_C0B0_A090_8070_6050_4030_2010,
            "iced psrldq 1"
        );
        assert_eq!(
            jit.xmm_at(0),
            0x00F0_E0D0_C0B0_A090_8070_6050_4030_2010,
            "jit psrldq 1"
        );
        // Hand-check PSRLDQ imm15: only the top byte survives (shifted to bit 0).
        let (iced, jit) = simd_dual(&[0x66, 0x0f, 0x73, 0xd8, 0x0f], &[], |r| set_pair(r, x0, 0));
        assert_eq!(iced.xmm_at(0), 0xF0, "iced psrldq 15");
        assert_eq!(jit.xmm_at(0), 0xF0, "jit psrldq 15");
        // Hand-check PSRLDQ imm16 → zero.
        let (iced, jit) = simd_dual(&[0x66, 0x0f, 0x73, 0xd8, 0x10], &[], |r| set_pair(r, x0, 0));
        assert_eq!(iced.xmm_at(0), 0, "iced psrldq 16 zeroes");
        assert_eq!(jit.xmm_at(0), 0, "jit psrldq 16 zeroes");
    }

    #[test]
    fn simd_cvtdq2pd_matches_iced() {
        // CVTDQ2PD xmm, xmm/m64: two packed signed dwords → two doubles.
        for (name, bytes, is_mem) in [
            ("cvtdq2pd reg", &[0xf3, 0x0f, 0xe6, 0xc1][..], false),
            ("cvtdq2pd mem", &[0xf3, 0x0f, 0xe6, 0x01][..], true),
        ] {
            let (iced, jit) = simd_dual(
                bytes,
                &[0x02, 0x00, 0x00, 0x00, 0xfe, 0xff, 0xff, 0xff],
                |r| {
                    set_pair(r, 0, 0);
                    if is_mem {
                        r.set_gpr_public(1, SIMD_DATA); // RCX = m64 base
                    }
                },
            );
            assert_same_regs(&iced, &jit, name);
        }
        // Hand-check: dwords 1 and -2 → doubles 1.0 (low) and -2.0 (high).
        let src = 0xffff_fffe_0000_0001_u128;
        let expect = u128::from(1.0_f64.to_bits()) | (u128::from((-2.0_f64).to_bits()) << 64);
        let (iced, jit) = simd_dual(&[0xf3, 0x0f, 0xe6, 0xc1], &[], |r| set_pair(r, 0, src));
        assert_eq!(iced.xmm_at(0), expect, "iced cvtdq2pd 1,-2");
        assert_eq!(jit.xmm_at(0), expect, "jit cvtdq2pd 1,-2");
    }

    #[test]
    fn simd_cvtps2pd_matches_iced() {
        // CVTPS2PD xmm, xmm/m64: two packed singles → two doubles (0F 5A).
        for (name, bytes, is_mem) in [
            ("cvtps2pd reg", &[0x0f, 0x5a, 0xc1][..], false),
            ("cvtps2pd mem", &[0x0f, 0x5a, 0x01][..], true),
        ] {
            let (iced, jit) = simd_dual(
                bytes,
                &[0x00, 0x00, 0x80, 0x3f, 0x00, 0x00, 0x00, 0xc0],
                |r| {
                    set_pair(r, 0, 0);
                    if is_mem {
                        r.set_gpr_public(1, SIMD_DATA); // RCX = m64 base
                    }
                },
            );
            assert_same_regs(&iced, &jit, name);
        }
        // Hand-check: singles 1.0f and -2.0f → doubles 1.0 (low) and -2.0 (high).
        let src = u128::from(0xc000_0000_3f80_0000_u64);
        let expect = u128::from(1.0_f64.to_bits()) | (u128::from((-2.0_f64).to_bits()) << 64);
        let (iced, jit) = simd_dual(&[0x0f, 0x5a, 0xc1], &[], |r| set_pair(r, 0, src));
        assert_eq!(iced.xmm_at(0), expect, "iced cvtps2pd 1,-2");
        assert_eq!(jit.xmm_at(0), expect, "jit cvtps2pd 1,-2");
    }

    #[test]
    fn simd_cvtpd2dq_matches_iced() {
        // CVTPD2DQ xmm, xmm/m128 (F2 0F E6): two doubles → two dwords, upper zeroed.
        for (name, bytes, is_mem) in [
            ("cvtpd2dq reg", &[0xf2, 0x0f, 0xe6, 0xc1][..], false),
            ("cvtpd2dq mem", &[0xf2, 0x0f, 0xe6, 0x01][..], true),
        ] {
            let (iced, jit) = simd_dual(
                bytes,
                &[
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf0, 0x3f, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0xc0,
                ],
                |r| {
                    set_pair(r, 0, 0x1234_5678_9abc_def0);
                    if is_mem {
                        r.set_gpr_public(1, SIMD_DATA); // RCX = m128 base
                    }
                },
            );
            assert_same_regs(&iced, &jit, name);
        }
        // Hand-check: doubles 1.0 and -2.0 → dwords 1 and -2 in the low 64 bits.
        let src = u128::from((-2.0_f64).to_bits()) << 64 | u128::from(1.0_f64.to_bits());
        let expect = u128::from(1_u64) | (u128::from(0xffff_fffe_u64) << 32);
        let (iced, jit) = simd_dual(&[0xf2, 0x0f, 0xe6, 0xc1], &[], |r| set_pair(r, 0, src));
        assert_eq!(iced.xmm_at(0), expect, "iced cvtpd2dq 1,-2");
        assert_eq!(jit.xmm_at(0), expect, "jit cvtpd2dq 1,-2");
    }

    #[test]
    fn simd_cvtpd2ps_matches_iced() {
        // CVTPD2PS xmm, xmm/m128 (66 0F 5A): two doubles → two singles, upper zeroed.
        for (name, bytes, is_mem) in [
            ("cvtpd2ps reg", &[0x66, 0x0f, 0x5a, 0xc1][..], false),
            ("cvtpd2ps mem", &[0x66, 0x0f, 0x5a, 0x01][..], true),
        ] {
            let (iced, jit) = simd_dual(
                bytes,
                &[
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf0, 0x3f, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0xc0,
                ],
                |r| {
                    set_pair(r, 0, 0x1122_3344_5566_7788);
                    if is_mem {
                        r.set_gpr_public(1, SIMD_DATA); // RCX = m128 base
                    }
                },
            );
            assert_same_regs(&iced, &jit, name);
        }
        // Hand-check: doubles 1.0 and -2.0 → singles 1.0f and -2.0f.
        let src = u128::from((-2.0_f64).to_bits()) << 64 | u128::from(1.0_f64.to_bits());
        let expect = u128::from(1.0_f32.to_bits()) | (u128::from((-2.0_f32).to_bits()) << 32);
        let (iced, jit) = simd_dual(&[0x66, 0x0f, 0x5a, 0xc1], &[], |r| set_pair(r, 0, src));
        assert_eq!(iced.xmm_at(0), expect, "iced cvtpd2ps 1,-2");
        assert_eq!(jit.xmm_at(0), expect, "jit cvtpd2ps 1,-2");
    }

    #[test]
    fn simd_cvtsd2ss_matches_iced() {
        // CVTSD2SS xmm, xmm/m64 (F2 0F 5A): low double → single, upper preserved.
        for (name, bytes, is_mem) in [
            ("cvtsd2ss reg", &[0xf2, 0x0f, 0x5a, 0xc1][..], false),
            ("cvtsd2ss mem", &[0xf2, 0x0f, 0x5a, 0x01][..], true),
        ] {
            let (iced, jit) = simd_dual(
                bytes,
                &[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf0, 0x3f],
                |r| {
                    set_pair(r, 0, 0x1234_5678_9abc_def0_1122_3344_5566_7788);
                    if is_mem {
                        r.set_gpr_public(1, SIMD_DATA); // RCX = m64 base
                    }
                },
            );
            assert_same_regs(&iced, &jit, name);
        }
        // Hand-check: low double 1.0 → single 1.0f, bits 32-127 preserved.
        let old =
            u128::from(0xdead_beef_cafe_babe_u64) << 64 | u128::from(0x1122_3344_5566_7788_u64);
        let src = u128::from(1.0_f64.to_bits());
        let expect =
            (old & 0xffff_ffff_ffff_ffff_ffff_ffff_0000_0000_u128) | u128::from(1.0_f32.to_bits());
        let (iced, jit) = simd_dual(&[0xf2, 0x0f, 0x5a, 0xc1], &[], |r| set_pair(r, old, src));
        assert_eq!(iced.xmm_at(0), expect, "iced cvtsd2ss preserve");
        assert_eq!(jit.xmm_at(0), expect, "jit cvtsd2ss preserve");
    }

    #[test]
    fn simd_cvtss2sd_matches_iced() {
        // CVTSS2SD xmm, xmm/m32 (F3 0F 5A): low single → double, upper preserved.
        for (name, bytes, is_mem) in [
            ("cvtss2sd reg", &[0xf3, 0x0f, 0x5a, 0xc1][..], false),
            ("cvtss2sd mem", &[0xf3, 0x0f, 0x5a, 0x01][..], true),
        ] {
            let (iced, jit) = simd_dual(
                bytes,
                &[0x00, 0x00, 0x80, 0x3f, 0x00, 0x00, 0x00, 0x00],
                |r| {
                    set_pair(r, 0, 0x1234_5678_9abc_def0_1122_3344_5566_7788);
                    if is_mem {
                        r.set_gpr_public(1, SIMD_DATA); // RCX = m32 base
                    }
                },
            );
            assert_same_regs(&iced, &jit, name);
        }
        // Hand-check: low single 1.0f → double 1.0, bits 64-127 preserved.
        let old =
            u128::from(0xdead_beef_cafe_babe_u64) << 64 | u128::from(0x1122_3344_5566_7788_u64);
        let src = u128::from(1.0_f32.to_bits());
        let expect =
            (old & 0xffff_ffff_ffff_ffff_0000_0000_0000_0000_u128) | u128::from(1.0_f64.to_bits());
        let (iced, jit) = simd_dual(&[0xf3, 0x0f, 0x5a, 0xc1], &[], |r| set_pair(r, old, src));
        assert_eq!(iced.xmm_at(0), expect, "iced cvtss2sd preserve");
        assert_eq!(jit.xmm_at(0), expect, "jit cvtss2sd preserve");
    }

    #[test]
    fn simd_pack_unpack_matches_iced() {
        let x0 = 0x807F_FF00_1234_5678_0001_FFFF_8000_7FFF_u128;
        let x1 = 0x0102_0304_0506_0708_090A_0B0C_0D0E_0F10_u128;
        for (name, bytes) in [
            ("packsswb", &[0x66, 0x0f, 0x63, 0xc1][..]),
            ("packssdw", &[0x66, 0x0f, 0x6b, 0xc1][..]),
            ("packuswb", &[0x66, 0x0f, 0x67, 0xc1][..]),
            ("punpcklbw", &[0x66, 0x0f, 0x60, 0xc1][..]),
            ("punpcklwd", &[0x66, 0x0f, 0x61, 0xc1][..]),
            ("punpckldq", &[0x66, 0x0f, 0x62, 0xc1][..]),
            ("punpcklqdq", &[0x66, 0x0f, 0x6c, 0xc1][..]),
            ("punpckhbw", &[0x66, 0x0f, 0x68, 0xc1][..]),
            ("punpckhwd", &[0x66, 0x0f, 0x69, 0xc1][..]),
            ("punpckhdq", &[0x66, 0x0f, 0x6a, 0xc1][..]),
            ("punpckhqdq", &[0x66, 0x0f, 0x6d, 0xc1][..]),
        ] {
            let (iced, jit) = simd_dual(bytes, &[], |r| set_pair(r, x0, x1));
            assert_same_regs(&iced, &jit, name);
        }
        // Hand-check PUNPCKLBW low bytes: [a0,b0,a1,b1,a2,b2,a3,b3].
        let a = 0x0000_0000_0000_0000_0000_0000_0000_0000_u128;
        let b = 0x0000_0000_0000_0000_0000_0000_0000_0000_u128;
        let a = a | 0x0000_0000_0000_0000_0000_0000_0403_0201_u128;
        let b = b | 0x0000_0000_0000_0000_0000_0000_0807_0605_u128;
        let (iced, jit) = simd_dual(&[0x66, 0x0f, 0x60, 0xc1], &[], |r| set_pair(r, a, b));
        let want = 0x0000_0000_0000_0000_0804_0703_0602_0501_u128;
        assert_eq!(iced.xmm_at(0), want, "iced punpcklbw");
        assert_eq!(jit.xmm_at(0), want, "jit punpcklbw");
        // Hand-check PACKSSWB: word 0x8000 saturates to byte 0x80.
        let a = 0x0000_0000_0000_0000_0000_0000_0000_8000_u128;
        let (iced, jit) = simd_dual(&[0x66, 0x0f, 0x63, 0xc1], &[], |r| set_pair(r, a, 0));
        assert_eq!(iced.xmm_at(0) & 0xff, 0x80, "iced packsswb");
        assert_eq!(jit.xmm_at(0) & 0xff, 0x80, "jit packsswb");
    }

    #[test]
    fn simd_shuffles_match_iced() {
        let x0 = 0x1122_3344_5566_7788_99AA_BBCC_DDEE_FF00_u128;
        let x1 = 0x0011_2233_4455_6677_8899_AABB_CCDD_EEFF_u128;
        for (name, bytes) in [
            ("pshufd", &[0x66, 0x0f, 0x70, 0xc1, 0x1b][..]),
            ("pshuflw", &[0xf2, 0x0f, 0x70, 0xc1, 0x1b][..]),
            ("pshufhw", &[0xf3, 0x0f, 0x70, 0xc1, 0x1b][..]),
            ("shufpd", &[0x66, 0x0f, 0xc6, 0xc1, 0x01][..]),
        ] {
            let (iced, jit) = simd_dual(bytes, &[], |r| set_pair(r, x0, x1));
            assert_same_regs(&iced, &jit, name);
        }
        // Hand-check SHUFPD imm 0x01: low lane from src2, high lane from src1
        // (the UCRT wcscpy fast path uses this — the guest File menus died on
        // it before the iced implementation landed).
        let a = 0x1111_2222_3333_4444_5555_6666_7777_8888_u128;
        let b = 0xAAAA_BBBB_CCCC_DDDD_EEEE_FFFF_0000_1111_u128;
        let (iced, jit) = simd_dual(&[0x66, 0x0f, 0xc6, 0xc1, 0x01], &[], |r| set_pair(r, a, b));
        let want = 0x1111_2222_3333_4444_EEEE_FFFF_0000_1111_u128;
        assert_eq!(iced.xmm_at(0), want, "iced shufpd");
        assert_eq!(jit.xmm_at(0), want, "jit shufpd");
        // Hand-check PSHUFD imm 0x1B = [3,2,1,0] (reverse dwords).
        let a = 0x0000_0001_0000_0002_0000_0003_0000_0004_u128;
        let (iced, jit) = simd_dual(&[0x66, 0x0f, 0x70, 0xc1, 0x1b], &[], |r| set_pair(r, 0, a));
        let want = 0x0000_0004_0000_0003_0000_0002_0000_0001_u128;
        assert_eq!(iced.xmm_at(0), want, "iced pshufd");
        assert_eq!(jit.xmm_at(0), want, "jit pshufd");
        // Hand-check PSHUFB with mask 0x10 repeated → table[0] per byte.
        let table = 0x0100_0000_0000_0000_0000_0000_0000_0042_u128;
        let mask = 0x1010_1010_1010_1010_1010_1010_1010_1010_u128;
        let (iced, jit) = simd_dual(&[0x66, 0x0f, 0x38, 0x00, 0xc1], &[], |r| {
            set_pair(r, table, mask);
        });
        assert_eq!(
            iced.xmm_at(0),
            0x4242_4242_4242_4242_4242_4242_4242_4242,
            "iced pshufb"
        );
        assert_eq!(
            jit.xmm_at(0),
            0x4242_4242_4242_4242_4242_4242_4242_4242,
            "jit pshufb"
        );
        // Bit-7 in the mask → 0 for that byte.
        let mask = 0x8080_8080_8080_8080_8080_8080_8080_8080_u128;
        let (iced, jit) = simd_dual(&[0x66, 0x0f, 0x38, 0x00, 0xc1], &[], |r| {
            set_pair(r, table, mask);
        });
        assert_eq!(iced.xmm_at(0), 0, "iced pshufb bit7 zeroes");
        assert_eq!(jit.xmm_at(0), 0, "jit pshufb bit7 zeroes");
    }

    #[test]
    fn simd_converts_match_iced() {
        // cvtsi2ss/cvtsi2sd from r32/r64.
        for (name, bytes) in [
            ("cvtsi2ss r32", &[0xf3, 0x0f, 0x2a, 0xc1][..]),
            ("cvtsi2ss r64", &[0xf3, 0x48, 0x0f, 0x2a, 0xc1][..]),
            ("cvtsi2sd r32", &[0xf2, 0x0f, 0x2a, 0xc1][..]),
            ("cvtsi2sd r64", &[0xf2, 0x48, 0x0f, 0x2a, 0xc1][..]),
        ] {
            let (iced, jit) = simd_dual(bytes, &[], |r| {
                set_pair(r, 0, 0);
                r.set_gpr_public(1, 0x0000_0000_0000_00FF); // RCX = 255
            });
            assert_same_regs(&iced, &jit, name);
        }
        // FP → int (both widths, truncating and rounding).
        for (name, bytes) in [
            ("cvttss2si r32", &[0xf3, 0x0f, 0x2c, 0xc0][..]),
            ("cvttss2si r64", &[0xf3, 0x48, 0x0f, 0x2c, 0xc0][..]),
            ("cvtss2si r32", &[0xf3, 0x0f, 0x2d, 0xc0][..]),
            ("cvttsd2si r64", &[0xf2, 0x48, 0x0f, 0x2c, 0xc0][..]),
            ("cvtsd2si r32", &[0xf2, 0x0f, 0x2d, 0xc0][..]),
        ] {
            let (iced, jit) = simd_dual(bytes, &[], |r| {
                let bits = 42.75_f32.to_bits();
                set_pair(r, u128::from(bits), 0);
                r.set_gpr_public(0, 0);
            });
            assert_same_regs(&iced, &jit, name);
        }
        // Hand-check CVTTSS2SI: 42.75 → 42 (truncate).
        let (iced, jit) = simd_dual(&[0xf3, 0x0f, 0x2c, 0xc0], &[], |r| {
            set_pair(r, u128::from(42.75_f32.to_bits()), 0);
            r.set_gpr_public(0, 0xdead_beef);
        });
        assert_eq!(iced.gpr(0), 42, "iced cvttss2si");
        assert_eq!(jit.gpr(0), 42, "jit cvttss2si");
        // Hand-check CVTTSS2SI on NaN → INT_MIN (0x80000000), 32-bit write zero-extends.
        let (iced, jit) = simd_dual(&[0xf3, 0x0f, 0x2c, 0xc0], &[], |r| {
            set_pair(r, u128::from(f32::NAN.to_bits()), 0);
            r.set_gpr_public(0, 0);
        });
        assert_eq!(iced.gpr(0), 0x8000_0000, "iced cvttss2si nan");
        assert_eq!(jit.gpr(0), 0x8000_0000, "jit cvttss2si nan");
        // Packed converts.
        for (name, bytes) in [
            ("cvtps2dq", &[0x66, 0x0f, 0x5b, 0xc1][..]),
            ("cvtdq2ps", &[0x0f, 0x5b, 0xc1][..]),
            ("cvttps2dq", &[0xf3, 0x0f, 0x5b, 0xc1][..]),
        ] {
            let (iced, jit) = simd_dual(bytes, &[], |r| {
                // four f32 lanes: 1.5, -2.5, 3.0, 4.25
                let f0 = 1.5_f32.to_bits();
                let f1 = (-2.5_f32).to_bits();
                let f2 = 3.0_f32.to_bits();
                let f3 = 4.25_f32.to_bits();
                let v = u128::from(f0)
                    | (u128::from(f1) << 32)
                    | (u128::from(f2) << 64)
                    | (u128::from(f3) << 96);
                set_pair(r, v, 0);
            });
            assert_same_regs(&iced, &jit, name);
        }
    }

    #[test]
    fn simd_fp_minmax_sqrt_match_iced() {
        let x0 = 0x3FF0_0000_0000_0000_3FE0_0000_3FC0_0000_u128;
        let x1 = 0x4000_0000_0000_0000_4000_0000_3F80_0000_u128;
        for (name, bytes) in [
            ("sqrtss", &[0xf3, 0x0f, 0x51, 0xc1][..]),
            ("sqrtsd", &[0xf2, 0x0f, 0x51, 0xc1][..]),
            ("sqrtps", &[0x0f, 0x51, 0xc1][..]),
            ("sqrtpd", &[0x66, 0x0f, 0x51, 0xc1][..]),
            ("minss", &[0xf3, 0x0f, 0x5d, 0xc1][..]),
            ("minsd", &[0xf2, 0x0f, 0x5d, 0xc1][..]),
            ("minps", &[0x0f, 0x5d, 0xc1][..]),
            ("minpd", &[0x66, 0x0f, 0x5d, 0xc1][..]),
            ("maxss", &[0xf3, 0x0f, 0x5f, 0xc1][..]),
            ("maxsd", &[0xf2, 0x0f, 0x5f, 0xc1][..]),
            ("maxps", &[0x0f, 0x5f, 0xc1][..]),
            ("maxpd", &[0x66, 0x0f, 0x5f, 0xc1][..]),
        ] {
            let (iced, jit) = simd_dual(bytes, &[], |r| set_pair(r, x0, x1));
            assert_same_regs(&iced, &jit, name);
        }
        // Hand-check SQRTSS of 4.0 → 2.0.
        let a = 0x0000_0000_0000_0000_0000_0000_0000_0000_u128;
        let b = 0x0000_0000_0000_0000_0000_0000_0000_0000_u128;
        let a = a | u128::from(123.456_f32.to_bits());
        let b = b | u128::from(4.0_f32.to_bits());
        let (iced, jit) = simd_dual(&[0xf3, 0x0f, 0x51, 0xc1], &[], |r| set_pair(r, a, b));
        let want = u64::from(2.0_f32.to_bits());
        assert_eq!(
            iced.xmm_at(0) & 0xffff_ffff,
            u128::from(want),
            "iced sqrtss"
        );
        assert_eq!(jit.xmm_at(0) & 0xffff_ffff, u128::from(want), "jit sqrtss");
        // MINSS with a NaN operand returns the source (x86 semantics).
        let nan = u128::from(f32::NAN.to_bits());
        let (iced, jit) = simd_dual(&[0xf3, 0x0f, 0x5d, 0xc1], &[], |r| set_pair(r, a | nan, b));
        let lo = iced.xmm_at(0) & 0xffff_ffff;
        assert_eq!(lo, b & 0xffff_ffff, "iced minss nan→src");
        let lo = jit.xmm_at(0) & 0xffff_ffff;
        assert_eq!(lo, b & 0xffff_ffff, "jit minss nan→src");
    }

    #[test]
    fn simd_comis_sets_flags_like_iced() {
        let a32 = u128::from(1.0_f32.to_bits());
        let b32 = u128::from(2.0_f32.to_bits());
        for (name, bytes) in [
            ("comiss", &[0x0f, 0x2f, 0xc1][..]),
            ("ucomiss", &[0x0f, 0x2e, 0xc1][..]),
            ("comisd", &[0x66, 0x0f, 0x2f, 0xc1][..]),
            ("ucomisd", &[0x66, 0x0f, 0x2e, 0xc1][..]),
        ] {
            // Pre-set CF so we can see it being cleared.
            let (iced, jit) = simd_dual(bytes, &[], |r| {
                set_pair(r, a32, b32);
                r.rflags = Rflags::ALWAYS1 | Rflags::CF;
            });
            assert_same_regs(&iced, &jit, name);
            // a < b → CF=1, ZF=0, PF=0, OF/AF/SF=0.
            assert!(iced.flag(Rflags::CF), "{name} iced CF");
            assert!(!iced.flag(Rflags::ZF), "{name} iced ZF");
            assert!(!iced.flag(Rflags::OF), "{name} iced OF");
            assert!(jit.flag(Rflags::CF), "{name} jit CF");
            assert!(!jit.flag(Rflags::ZF), "{name} jit ZF");
            assert!(!jit.flag(Rflags::OF), "{name} jit OF");
        }
        // a == b → ZF=1, CF=0, PF=0.
        let (iced, jit) = simd_dual(&[0x0f, 0x2f, 0xc1], &[], |r| set_pair(r, b32, b32));
        assert!(iced.flag(Rflags::ZF), "iced eq ZF");
        assert!(!iced.flag(Rflags::CF), "iced eq CF");
        assert!(jit.flag(Rflags::ZF), "jit eq ZF");
        assert!(!jit.flag(Rflags::CF), "jit eq CF");
        // NaN → unordered: ZF=PF=CF=1.
        let (iced, jit) = simd_dual(&[0x0f, 0x2f, 0xc1], &[], |r| {
            set_pair(r, u128::from(f32::NAN.to_bits()), b32);
        });
        assert!(iced.flag(Rflags::PF), "iced nan PF");
        assert!(iced.flag(Rflags::CF), "iced nan CF");
        assert!(jit.flag(Rflags::PF), "jit nan PF");
        assert!(jit.flag(Rflags::CF), "jit nan CF");
    }

    #[test]
    fn simd_movq_movd_gpr_bridge_matches_iced() {
        let x = 0x8899_AABB_CCDD_EEFF_0011_2233_4455_6677_u128;
        for (name, bytes) in [
            ("movq xmm,r64", &[0x66, 0x48, 0x0f, 0x6e, 0xc1][..]),
            ("movq r64,xmm", &[0x66, 0x48, 0x0f, 0x7e, 0xc1][..]),
            ("movd xmm,r32", &[0x66, 0x0f, 0x6e, 0xc1][..]),
            ("movd r32,xmm", &[0x66, 0x0f, 0x7e, 0xc1][..]),
        ] {
            let (iced, jit) = simd_dual(bytes, &[], |r| {
                set_pair(r, x, 0);
                r.set_gpr_public(1, 0x1234_5678_9ABC_DEF0); // RCX
            });
            assert_same_regs(&iced, &jit, name);
        }
        // Hand-check MOVQ xmm,r64: low qword moved, high 64 zeroed.
        let (iced, jit) = simd_dual(&[0x66, 0x48, 0x0f, 0x6e, 0xc1], &[], |r| {
            set_pair(r, u128::MAX, 0);
            r.set_gpr_public(1, 0x0011_2233_4455_6677);
        });
        let want = 0x0000_0000_0000_0000_0011_2233_4455_6677_u128;
        assert_eq!(iced.xmm_at(0), want, "iced movq zeroes hi");
        assert_eq!(jit.xmm_at(0), want, "jit movq zeroes hi");
        // Hand-check MOVQ r64,xmm: low qword extracted.
        let (iced, jit) = simd_dual(&[0x66, 0x48, 0x0f, 0x7e, 0xc1], &[], |r| {
            set_pair(r, 0x8899_AABB_CCDD_EEFF_0011_2233_4455_6677, 0);
            r.set_gpr_public(1, 0);
        });
        assert_eq!(iced.gpr(1), 0x0011_2233_4455_6677, "iced movq extract");
        assert_eq!(jit.gpr(1), 0x0011_2233_4455_6677, "jit movq extract");
        // MOVD zero-extends 32 bits.
        let (iced, jit) = simd_dual(&[0x66, 0x0f, 0x6e, 0xc1], &[], |r| {
            set_pair(r, u128::MAX, 0);
            r.set_gpr_public(1, 0x1234_5678_9ABC_DEF0);
        });
        let want = 0x0000_0000_0000_0000_0000_0000_9ABC_DEF0_u128;
        assert_eq!(iced.xmm_at(0), want, "iced movd zero-extends");
        assert_eq!(jit.xmm_at(0), want, "jit movd zero-extends");
    }

    #[test]
    fn simd_memory_operands_match_iced() {
        let data = 0x0001_0002_0003_0004_0005_0006_0007_0008_u128.to_le_bytes();
        for (name, bytes) in [
            ("paddd [rax]", &[0x66, 0x0f, 0xfe, 0x00][..]),
            ("pshufb [rax]", &[0x66, 0x0f, 0x38, 0x00, 0x00][..]),
            ("comiss [rax]", &[0x0f, 0x2f, 0x00][..]),
            ("cvtsi2ss [rax]", &[0xf3, 0x0f, 0x2a, 0x00][..]),
        ] {
            let (iced, jit) = simd_dual(bytes, &data, |r| {
                set_pair(r, 0x0000_0004_0000_0003_0000_0002_0000_0001, 0);
                r.set_gpr_public(0, SIMD_DATA); // RAX = data base
            });
            assert_same_regs(&iced, &jit, name);
        }
        // Hand-check PADDD xmm0, [rax]: add dword0 of memory (0x00000001).
        let a = 0x0000_0000_0000_0000_0000_0000_0000_0005_u128;
        let data = 0x0000_0000_0000_0000_0000_0000_0000_0001_u128.to_le_bytes();
        let (iced, jit) = simd_dual(&[0x66, 0x0f, 0xfe, 0x00], &data, |r| {
            set_pair(r, a, 0);
            r.set_gpr_public(0, SIMD_DATA);
        });
        assert_eq!(
            iced.xmm_at(0),
            0x0000_0000_0000_0000_0000_0000_0000_0006,
            "iced paddd mem"
        );
        assert_eq!(
            jit.xmm_at(0),
            0x0000_0000_0000_0000_0000_0000_0000_0006,
            "jit paddd mem"
        );
    }

    // --- Per-thread TEB: GS-relative accesses resolve per engine ---

    const GS_TEB_CODE: u64 = 0x2001_0000;
    /// Worker TEB page (page-aligned, disjoint from the primary [`crate::GS_BASE`]).
    const GS_TEB_WORKER: u64 = 0x0000_7000_0040_C000;
    /// Seed value in the primary TEB last-error slot.
    const GS_PRIMARY_ERR: u32 = 0x1111_1111;
    /// Seed value in the worker TEB last-error slot.
    const GS_WORKER_ERR: u32 = 0x2222_2222;
    /// Value a store test writes through `mov [gs:0x68], ecx`.
    const GS_NEW_ERR: u32 = 0x3333_3333;

    /// Map the code page + primary/worker TEB pages and seed distinct
    /// last-error values into each TEB's `TEB_LAST_ERROR_OFFSET` slot.
    fn gs_teb_setup(engine: &mut dyn CpuEngine) {
        engine
            .virtual_alloc(
                GS_TEB_CODE,
                0x1000,
                MEM_RESERVE | MEM_COMMIT,
                protect::PAGE_EXECUTE_READWRITE,
            )
            .expect("code page");
        engine
            .virtual_alloc(
                crate::GS_BASE,
                0x1000,
                MEM_RESERVE | MEM_COMMIT,
                protect::PAGE_READWRITE,
            )
            .expect("primary TEB page");
        engine
            .virtual_alloc(
                GS_TEB_WORKER,
                0x1000,
                MEM_RESERVE | MEM_COMMIT,
                protect::PAGE_READWRITE,
            )
            .expect("worker TEB page");
        let off = crate::guest_layout::TEB_LAST_ERROR_OFFSET;
        engine
            .mem_write(crate::GS_BASE + off, &GS_PRIMARY_ERR.to_le_bytes())
            .expect("primary last-error");
        engine
            .mem_write(GS_TEB_WORKER + off, &GS_WORKER_ERR.to_le_bytes())
            .expect("worker last-error");
    }

    /// Run `code` (trailing `nop; ud2` appended) on iced and the JIT, both
    /// bound to `GS_TEB_WORKER` via `set_gs_base`; returns both engines so
    /// callers can inspect registers and guest memory.
    fn gs_teb_dual(code: &[u8], setup: impl Fn(&mut RegFile)) -> (IcedCpu, JitCpu) {
        let mut full = Vec::with_capacity(code.len() + 3);
        full.extend_from_slice(code);
        full.extend_from_slice(&[0x90, 0x0f, 0x0b]); // nop filler + ud2 terminator
        let n_insns = decode_count(&full);

        // iced reference, bound to the worker TEB page.
        crate::exec::iced_decode_cache_flush();
        let mut iced = IcedCpu::open_x86_64();
        gs_teb_setup(&mut iced);
        iced.mem_write(GS_TEB_CODE, &full).expect("iced code");
        setup(iced.regs_mut());
        iced.set_gs_base(GS_TEB_WORKER);
        iced.write_rip(GS_TEB_CODE).expect("iced rip");
        for _ in 0..n_insns {
            iced.step_once().expect("iced step");
        }

        // JIT, bound to the worker TEB page.
        let mut cpu = JitCpu::open_x86_64();
        gs_teb_setup(&mut cpu);
        cpu.mem_write(GS_TEB_CODE, &full).expect("jit code");
        setup(&mut cpu.thread.regs);
        cpu.set_gs_base(GS_TEB_WORKER);
        cpu.write_rip(GS_TEB_CODE).expect("jit rip");
        let (result, _retired) = cpu.step_one().expect("jit step");
        assert!(
            matches!(result, StepResult::Continue),
            "jit result {result:?}"
        );
        assert!(
            cpu.has_ready_at(GS_TEB_CODE),
            "block must compile, not run iced"
        );
        assert_eq!(
            cpu.stats().iced_insns,
            0,
            "block ran on iced instead of JIT"
        );

        (iced, cpu)
    }

    /// `mov eax, [gs:0x68]` reads the BOUND TEB's last-error on both engines.
    #[test]
    fn gs_relative_load_reads_bound_teb_on_both_engines() {
        // 65 8b 05 <disp32> — mov eax, [gs:0x68].
        let code = [0x65, 0x8b, 0x04, 0x25, 0x68, 0x00, 0x00, 0x00];
        let (iced, jit) = gs_teb_dual(&code, |_| {});
        assert_eq!(
            iced.regs().gpr(0),
            u64::from(GS_WORKER_ERR),
            "iced reads the worker TEB slot"
        );
        assert_eq!(
            jit.thread.regs.gpr(0),
            u64::from(GS_WORKER_ERR),
            "jit reads the worker TEB slot"
        );
    }

    /// With the default binding (`GS_BASE`) the same access reads the PRIMARY
    /// TEB — the primary thread's behavior is preserved.
    #[test]
    fn gs_relative_load_defaults_to_primary_teb_on_both_engines() {
        let full = [
            0x65, 0x8b, 0x04, 0x25, 0x68, 0x00, 0x00, 0x00, 0x90, 0x0f, 0x0b,
        ];
        let n_insns = decode_count(&full);

        crate::exec::iced_decode_cache_flush();
        let mut iced = IcedCpu::open_x86_64();
        gs_teb_setup(&mut iced);
        iced.mem_write(GS_TEB_CODE, &full).expect("iced code");
        iced.write_rip(GS_TEB_CODE).expect("iced rip");
        for _ in 0..n_insns {
            iced.step_once().expect("iced step");
        }
        assert_eq!(
            iced.regs().gpr(0),
            u64::from(GS_PRIMARY_ERR),
            "iced default binding reads the primary TEB"
        );

        let mut cpu = JitCpu::open_x86_64();
        gs_teb_setup(&mut cpu);
        cpu.mem_write(GS_TEB_CODE, &full).expect("jit code");
        cpu.write_rip(GS_TEB_CODE).expect("jit rip");
        let (result, _retired) = cpu.step_one().expect("jit step");
        assert!(
            matches!(result, StepResult::Continue),
            "jit result {result:?}"
        );
        assert!(cpu.has_ready_at(GS_TEB_CODE), "block must compile");
        assert_eq!(
            cpu.thread.regs.gpr(0),
            u64::from(GS_PRIMARY_ERR),
            "jit default binding reads the primary TEB"
        );
    }

    /// `mov [gs:0x68], ecx` writes the BOUND TEB's last-error, leaving the
    /// primary slot untouched, on both engines.
    #[test]
    fn gs_relative_store_writes_bound_teb_on_both_engines() {
        // 65 89 0d <disp32> — mov [gs:0x68], ecx.
        let code = [0x65, 0x89, 0x0c, 0x25, 0x68, 0x00, 0x00, 0x00];
        let (iced, jit) = gs_teb_dual(&code, |r| r.set_gpr_public(1, u64::from(GS_NEW_ERR)));
        let off = crate::guest_layout::TEB_LAST_ERROR_OFFSET;

        let mut worker = [0_u8; 4];
        let mut primary = [0_u8; 4];
        iced.guest_mem_arc()
            .read()
            .unwrap()
            .read(GS_TEB_WORKER + off, &mut worker)
            .expect("read iced worker slot");
        iced.guest_mem_arc()
            .read()
            .unwrap()
            .read(crate::GS_BASE + off, &mut primary)
            .expect("read iced primary slot");
        assert_eq!(
            u32::from_le_bytes(worker),
            GS_NEW_ERR,
            "iced store reached the worker TEB slot"
        );
        assert_eq!(
            u32::from_le_bytes(primary),
            GS_PRIMARY_ERR,
            "iced store left the primary TEB slot untouched"
        );

        {
            let mem = jit.shared_jit().mem.read().unwrap();
            mem.read(GS_TEB_WORKER + off, &mut worker)
                .expect("read jit worker slot");
            mem.read(crate::GS_BASE + off, &mut primary)
                .expect("read jit primary slot");
        }
        assert_eq!(
            u32::from_le_bytes(worker),
            GS_NEW_ERR,
            "jit store reached the worker TEB slot"
        );
        assert_eq!(
            u32::from_le_bytes(primary),
            GS_PRIMARY_ERR,
            "jit store left the primary TEB slot untouched"
        );
    }

    /// The GetLastError micro-stub trampoline resolves the TEB last-error slot
    /// against the engine's GS base: one compiled stub serves BOTH the primary
    /// binding and a worker binding.
    #[test]
    fn last_error_stub_trampoline_uses_engine_gs_base() {
        // mov eax, [gs:0x68]; ret — the planted GetLastError stub body
        // (ModRM 04 + SIB 25 forces the base-less disp32 form).
        let get = [0x65, 0x8b, 0x04, 0x25, 0x68, 0x00, 0x00, 0x00, 0xc3];
        let mut cpu = JitCpu::open_x86_64();
        gs_teb_setup(&mut cpu);
        // The stub ends in `ret`, so the guest stack must hold a return address.
        let stack = 0x2002_0000_u64;
        cpu.virtual_alloc(
            stack,
            0x1000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_READWRITE,
        )
        .expect("stack");
        let ret_slot = stack + 0xff0;
        cpu.mem_write(ret_slot, &0x2002_1000_u64.to_le_bytes())
            .expect("return address");
        cpu.mem_write(GS_TEB_CODE, &get).expect("stub code");

        // Worker binding: the stub reads the worker TEB slot.
        cpu.set_gs_base(GS_TEB_WORKER);
        cpu.write_rsp(ret_slot).expect("rsp");
        cpu.write_rip(GS_TEB_CODE).expect("rip");
        let (result, _retired) = cpu.step_one().expect("step");
        assert!(matches!(result, StepResult::Continue), "result {result:?}");
        assert!(cpu.has_ready_at(GS_TEB_CODE), "stub compiled as a block");
        assert_eq!(
            cpu.stats().iced_insns,
            0,
            "stub must run the hand-written trampoline, not iced"
        );
        assert_eq!(
            cpu.thread.regs.gpr(0),
            u64::from(GS_WORKER_ERR),
            "worker binding reads the worker TEB"
        );

        // Primary binding on the SAME compiled stub: reads the primary slot.
        cpu.set_gs_base(crate::GS_BASE);
        cpu.write_rsp(ret_slot).expect("rsp");
        cpu.write_rip(GS_TEB_CODE).expect("rip");
        let (result, _retired) = cpu.step_one().expect("step");
        assert!(matches!(result, StepResult::Continue), "result {result:?}");
        assert_eq!(
            cpu.thread.regs.gpr(0),
            u64::from(GS_PRIMARY_ERR),
            "primary binding reads the primary TEB"
        );
        assert_eq!(
            cpu.stats().compiles,
            1,
            "one compiled stub served both thread bindings"
        );
    }

    /// The SetLastError micro-stub trampoline stores into the engine-bound
    /// TEB page, never the fixed primary page.
    #[test]
    fn last_error_store_trampoline_writes_engine_teb() {
        // mov [gs:0x68], ecx; ret — the planted SetLastError stub body.
        let set = [0x65, 0x89, 0x0c, 0x25, 0x68, 0x00, 0x00, 0x00, 0xc3];
        let mut cpu = JitCpu::open_x86_64();
        gs_teb_setup(&mut cpu);
        let stack = 0x2002_0000_u64;
        cpu.virtual_alloc(
            stack,
            0x1000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_READWRITE,
        )
        .expect("stack");
        let ret_slot = stack + 0xff0;
        cpu.mem_write(ret_slot, &0x2002_1000_u64.to_le_bytes())
            .expect("return address");
        cpu.mem_write(GS_TEB_CODE, &set).expect("stub code");
        cpu.thread.regs.set_gpr_public(1, u64::from(GS_NEW_ERR));
        cpu.set_gs_base(GS_TEB_WORKER);
        cpu.write_rsp(ret_slot).expect("rsp");
        cpu.write_rip(GS_TEB_CODE).expect("rip");
        let (result, _retired) = cpu.step_one().expect("step");
        assert!(matches!(result, StepResult::Continue), "result {result:?}");
        assert_eq!(
            cpu.stats().iced_insns,
            0,
            "stub must run the hand-written trampoline, not iced"
        );

        let off = crate::guest_layout::TEB_LAST_ERROR_OFFSET;
        let mut worker = [0_u8; 4];
        let mut primary = [0_u8; 4];
        {
            let mem = cpu.shared_jit().mem.read().unwrap();
            mem.read(GS_TEB_WORKER + off, &mut worker)
                .expect("read worker slot");
            mem.read(crate::GS_BASE + off, &mut primary)
                .expect("read primary slot");
        }
        assert_eq!(
            u32::from_le_bytes(worker),
            GS_NEW_ERR,
            "store reached the worker TEB slot"
        );
        assert_eq!(
            u32::from_le_bytes(primary),
            GS_PRIMARY_ERR,
            "store left the primary TEB slot untouched"
        );
    }
}
