//! Phase 2: hybrid Cranelift block JIT + iced interpreter fallback.
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
    private_interfaces, // JitShared/PerThreadJitState expose crate-private types
    clippy::indexing_slicing, // fixed gpr[0..16]
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::arithmetic_side_effects,
    clippy::unwrap_used // Mutex/RwLock poison recovery is hard-coded (never occurs in practice)
)]

mod block;
mod fast_api;
mod lower;
mod trampolines;

pub use fast_api::{FastApiKind, JitFastPathConfig, JitHeapLayout};

use crate::exec::{self, HookWindow, StepResult};
use crate::mem::{self, GuestMemory, PAGE_SIZE, PAGE_SIZE_USIZE};
use crate::regs::RegFile;
use crate::{CodeHookOutcome, InvalidMemoryAccess, RwxPerms};
use crate::{CpuEngine, CpuError, RunUntilHook};
use block::{BlockKind, decode_pure_gpr_block, pure_is_self_loop};
use fast_api::{
    install_heap_layout, wie_ucrt_fflush, wie_ucrt_free, wie_ucrt_fwrite, wie_ucrt_iob,
    wie_ucrt_malloc, wie_ucrt_memcpy, wie_ucrt_strlen,
};
use lower::{
    CHAIN_SLOTS, CompiledBlock, JitCtx, MemPathSlice, MemPin, PIN_SLOTS, STICKY_WAYS, TLB_EMPTY,
    TLB_SETS, TlbBucket, TlbBucketAux, XmmSlot, chain_table_clear, chain_table_insert,
    compile_block, empty_tlb_aux, empty_tlb_bucket, wie_f32_binop, wie_f64_binop,
    wie_jit_chain_lookup, wie_jit_host_span, wie_jit_load, wie_jit_store, wie_jit_string,
    wie_sse_cvt, wie_sse_fp_binop, wie_sse_fp_unop, wie_sse_int_binop, wie_sse_pshufb_hi,
    wie_sse_pshufb_lo, wie_sse_shift,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};
use trampolines::match_micro_stub;

/// Compile after this many visits to the same guest entry (skip cold code).
///
/// Default **100**: lower values (e.g. 12) cut residual iced but thrash short
/// non-loop blocks on 7za and **increase** wall (sweep 2026-07-21: thr=100 best).
/// Residual iced under thr=100 is almost all already-lowerable warmup (Mov/Call/…).
/// Override: `WIE_JIT_HOTNESS=N` (`0` = eager first visit). Tests use 0.
fn hotness_threshold() -> u32 {
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
fn pure_loop_hotness() -> u32 {
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
pub(super) fn jit_mem_inline_enabled() -> bool {
    !matches!(jit_mem_mode(), JitMemMode::Slow)
}

/// Whether Cranelift may emit **data** pin IR (heap + VirtualAlloc) after sticky.
///
/// Default sticky still fills all pin slots for helper `pin_resolve`; only
/// `WIE_JIT_MEM=pin` adds IR probes (can help some heaps, tax on thrashy paths).
pub(super) fn jit_mem_pin_enabled() -> bool {
    matches!(jit_mem_mode(), JitMemMode::Pin)
}

/// Opt-in mem helper resolution histogram (`WIE_JIT_MEM_TRACE=1`).
fn mem_path_trace_enabled() -> bool {
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
pub(super) fn jit_super_enabled(self_loop: bool) -> bool {
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
fn jit_chain_enabled() -> bool {
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
fn bg_jit_enabled() -> bool {
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
fn bg_wait_timeout() -> Duration {
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
const BG_QUEUE_CAP: usize = 1024;

/// Per-entry wait cell for background compiles.
///
/// A mutex + condvar pair lets a guest thread block **only** on the entry it
/// is about to execute (the worker calls [`Self::notify_all`] when it resolves
/// that entry), instead of waiting on the whole queue. `notify_all` needs no
/// lock; the mutex exists solely so `wait_timeout` is well-defined.
struct BgWaitCell {
    lock: Mutex<()>,
    cv: Condvar,
}

impl BgWaitCell {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            lock: Mutex::new(()),
            cv: Condvar::new(),
        })
    }

    /// Wake all waiters for this entry (worker install/fail/drop paths).
    fn notify_all(&self) {
        self.cv.notify_all();
    }

    /// Block until woken or `timeout` elapses. Spurious wakeups return early —
    /// callers must re-check the cache state.
    fn wait_timeout(&self, timeout: Duration) {
        let guard = self.lock.lock().unwrap();
        let (guard, _timed_out) = self.cv.wait_timeout(guard, timeout).unwrap();
        drop(guard);
    }
}

/// Result of handing a block to the background worker.
enum BgEnqueueOutcome {
    /// Entry transitioned to `Queued`; the caller may wait on this cell
    /// for the worker's Ready entry.
    Queued(Arc<BgWaitCell>),
    /// Already `Ready` in the cache (worker beat us); caller re-reads.
    Ready,
    /// Worker unavailable / queue full / block not compilable: caller falls
    /// back to inline compilation (or iced for NotPure).
    Unavailable,
}

/// Intermediate states observed while waiting on a background compile.
enum BgWaitState {
    Ready(CompiledBlock),
    Never,
}

/// Shared JIT state: Cranelift module + compilation cache + guest memory.
/// One instance per process, shared via Arc across all per-thread engines.
#[doc(hidden)]
pub struct JitShared {
    /// Cranelift JIT module (behind Mutex: compiled blocks are serialized anyway).
    #[doc(hidden)]
    pub engine: Mutex<Option<JitEngine>>,
    /// Guest memory: page tables, mmap backend. RwLock protects metadata;
    /// reads are the common path (generation, pins, decode), writes are rare (map/free).
    #[doc(hidden)]
    pub mem: RwLock<GuestMemory>,
    /// Guest entry VA → CacheEntry.
    #[doc(hidden)]
    pub cache: RwLock<HashMap<u64, CacheEntry>>,
    /// Ready-block FuncIds for chaining.
    #[doc(hidden)]
    pub chain_ids: RwLock<HashMap<u64, cranelift_module::FuncId>>,
    /// Guest page keys covered by Ready blocks (SMC tracking).
    #[doc(hidden)]
    pub code_pages: Mutex<HashMap<u64, u32>>,
    /// Guest page keys written by JIT stores (SMC invalidation). Separate lock
    /// so drain_pending_code_writes avoids GuestMemory write lock.
    pub pending_code_writes: Mutex<Vec<u64>>,
    /// Set when too many distinct pages were written; drain does a full code flush.
    pub pending_code_overflow: AtomicBool,
    /// Cached GuestMemory generation, updated on map/protect/free (avoids mem lock).
    pub mem_gen: AtomicU64,
    /// Whether the Cranelift engine was successfully initialized.
    pub engine_ready: AtomicBool,
    /// Background compiler queue sender. The worker thread owns the receiver
    /// and dies when this sender drops (i.e. when the last `Arc<JitShared>` goes
    /// away). `None` when the worker was never spawned or failed to spawn.
    pub bg_tx: Mutex<Option<SyncSender<(u64, BlockKind)>>>,
    /// Set once the worker thread has been spawned (spawn-once latch).
    pub bg_spawned: AtomicBool,
    /// Whether the background compiler is currently alive (set true on spawn,
    /// cleared when the worker exits). Guest threads fall back to inline
    /// compilation when this is false.
    pub bg_alive: Arc<AtomicBool>,
    /// Bumped on every background cache install. Per-thread engines re-sync
    /// their late-bound chain tables when they observe a new epoch.
    pub cache_epoch: AtomicU64,
    /// Total background compiles installed (shared across threads; surfaced in
    /// per-thread [`JitStats`] snapshots).
    pub bg_compiles: AtomicU64,
    /// UCRT fast-API pairs mirrored from each engine's `configure_fast_path` so
    /// the worker lowers calls exactly like the inline path would.
    pub bg_fast_api: Mutex<Vec<(u64, FastApiKind)>>,
    /// Test-only latch forcing the background path on for this instance
    /// (env-independent, and per-`JitShared` so parallel unit tests cannot
    /// interfere with each other).
    #[cfg(test)]
    bg_force: AtomicBool,
}

impl JitShared {
    fn new() -> Self {
        let has_engine = match JitEngine::new() {
            Ok(e) => {
                tracing::info!("cranelift JIT module ready");
                Some(e)
            }
            Err(e) => {
                tracing::warn!(error = %e, "cranelift JIT unavailable; iced-only");
                None
            }
        };
        let engine_ready = has_engine.is_some();
        Self {
            engine: Mutex::new(has_engine),
            mem: RwLock::new(GuestMemory::new()),
            cache: RwLock::new(HashMap::new()),
            chain_ids: RwLock::new(HashMap::new()),
            code_pages: Mutex::new(HashMap::new()),
            pending_code_writes: Mutex::new(Vec::new()),
            pending_code_overflow: AtomicBool::new(false),
            mem_gen: AtomicU64::new(0),
            engine_ready: AtomicBool::new(engine_ready),
            bg_tx: Mutex::new(None),
            bg_spawned: AtomicBool::new(false),
            bg_alive: Arc::new(AtomicBool::new(false)),
            cache_epoch: AtomicU64::new(0),
            bg_compiles: AtomicU64::new(0),
            bg_fast_api: Mutex::new(Vec::new()),
            #[cfg(test)]
            bg_force: AtomicBool::new(false),
        }
    }

    /// Whether the background path is enabled for this instance: env default
    /// (on for real runs, off under `cfg(test)`) or the test latch.
    fn bg_enabled_here(&self) -> bool {
        bg_jit_enabled() || self.bg_force_test()
    }

    #[cfg(test)]
    fn bg_force_test(&self) -> bool {
        self.bg_force.load(Ordering::Relaxed)
    }

    #[cfg(not(test))]
    fn bg_force_test(&self) -> bool {
        let _ = self;
        false
    }

    /// Install a compiled block into the shared cache (inline-path entry point).
    ///
    /// Shared-only bookkeeping: drops any replaced Ready entry from `chain_ids`
    /// and `code_pages`, registers the new block for direct chaining + SMC
    /// tracking, and inserts the Ready entry. No per-thread side effects.
    pub(crate) fn insert_ready(&self, rip: u64, compiled: CompiledBlock) {
        let removed = {
            let mut cache = self.cache.write().unwrap();
            let old = cache.remove(&rip);
            if let Some(CacheEntry::Ready(ref old)) = old {
                self.chain_ids.write().unwrap().remove(&rip);
                Some((old.guest_start, old.guest_end))
            } else {
                None
            }
        };
        if let Some((gs, ge)) = removed {
            self.code_pages_remove_range(gs, ge);
        }
        if jit_chain_enabled()
            && let Some(fid) = compiled.func_id
        {
            self.chain_ids.write().unwrap().insert(rip, fid);
        }
        self.code_pages_add_range(compiled.guest_start, compiled.guest_end);
        self.cache
            .write()
            .unwrap()
            .insert(rip, CacheEntry::Ready(compiled));
    }

    /// Spawn the background compiler thread exactly once.
    ///
    /// The thread is detached (no stored join handle): it holds a `Weak` to
    /// this shared state and dies when the last `Arc` drops (the sender in
    /// `bg_tx` disconnects, so `recv` errors). This avoids a self-join if the
    /// worker happens to hold the final strong reference while a job runs.
    pub(crate) fn ensure_bg_worker(self: &Arc<Self>) {
        if self.bg_spawned.swap(true, Ordering::AcqRel) {
            return;
        }
        let (tx, rx) = mpsc::sync_channel::<(u64, BlockKind)>(BG_QUEUE_CAP);
        *self.bg_tx.lock().unwrap() = Some(tx);
        let weak = Arc::downgrade(self);
        let alive = Arc::clone(&self.bg_alive);
        let spawned = std::thread::Builder::new()
            .name("wie-jit-bg-compiler".into())
            .spawn(move || {
                Self::bg_worker_main(&weak, &rx);
                alive.store(false, Ordering::Release);
            });
        if spawned.is_ok() {
            // The worker is up (it can only exit after this sender drops,
            // which requires the shared state to be gone — impossible while
            // we hold `self`). Guests may enqueue immediately.
            self.bg_alive.store(true, Ordering::Release);
        } else {
            // Spawn failed (resource limits): revert so callers fall back
            // to inline compilation and a later call can retry.
            self.bg_spawned.store(false, Ordering::Release);
            self.bg_tx.lock().unwrap().take();
        }
    }

    /// Worker main loop: compile queued blocks, install Ready entries, wake
    /// waiters. Exits when the channel disconnects (shared state dropped).
    fn bg_worker_main(shared: &Weak<Self>, rx: &Receiver<(u64, BlockKind)>) {
        while let Ok((rip, kind)) = rx.recv() {
            let Some(shared) = shared.upgrade() else {
                break;
            };
            let mem_gen_before = shared.mem_gen.load(Ordering::Acquire);
            let fast_api = shared.bg_fast_api.lock().unwrap().clone();
            let compiled = shared.compile_from_kind_shared(&fast_api, rip, kind);
            // If guest memory was remapped (map/protect/free) or a code page
            // write is pending while we compiled, the cached bytes may be
            // stale — drop the result and let the guest re-request.
            let stale = shared.mem_gen.load(Ordering::Acquire) != mem_gen_before
                || compiled.as_ref().is_some_and(|c| {
                    let len =
                        usize::try_from(c.guest_end.saturating_sub(c.guest_start)).unwrap_or(0);
                    shared.pending_code_write_overlaps(c.guest_start, len)
                });
            if stale {
                shared.bg_install_drop(rip);
            } else {
                match compiled {
                    Some(c) => shared.bg_install_ready(rip, c),
                    None => shared.bg_install_never(rip),
                }
            }
        }
    }

    /// Compile a decoded block without any per-thread state.
    ///
    /// Identical to the inline path's lowering (same `compile_block`, same
    /// `chain_ids` snapshot for direct chaining, same `call_fast` resolution)
    /// so background output is byte-for-byte the same code — only the *when*
    /// differs.
    fn compile_from_kind_shared(
        &self,
        fast_api: &[(u64, FastApiKind)],
        rip: u64,
        result: BlockKind,
    ) -> Option<CompiledBlock> {
        match result {
            BlockKind::Pure {
                insns,
                end_rip,
                bytes_len,
                term,
            } => {
                // 1–3 insn guest stubs: hand-written host trampoline (no Cranelift).
                if let Some(micro) = match_micro_stub(&insns, term) {
                    let guest_end = rip.saturating_add(u64::from(bytes_len));
                    return Some(CompiledBlock {
                        func: micro.func(),
                        func_id: None,
                        insn_count: micro.insn_count(),
                        uses_sse: false,
                        xmm_live_mask: 0,
                        xmm_may_def_mask: 0,
                        guest_start: rip,
                        guest_end,
                    });
                }

                // Resolve import thunks before mutably borrowing the JIT engine.
                let call_fast = match term {
                    Some(block::BlockTerm::Call { target, .. }) => {
                        let mem = self.mem.read().unwrap();
                        let final_va = resolve_thunk_va(&mem, target);
                        drop(mem);
                        fast_api
                            .iter()
                            .find_map(|&(k, kind)| (k == final_va).then_some(kind))
                    }
                    _ => None,
                };
                let chain_on = jit_chain_enabled();
                let empty_chain = HashMap::new();
                let mut eng_guard = self.engine.lock().unwrap();
                let eng = eng_guard.as_mut()?;
                let chain_ids = &*self.chain_ids.read().unwrap();
                let chain_map = if chain_on { chain_ids } else { &empty_chain };
                compile_block(
                    eng, rip, &insns, end_rip, term, call_fast, chain_map, bytes_len,
                )
                .ok()
            }
            BlockKind::NotPure => None,
        }
    }

    /// Worker-side install of a successfully compiled block: cache + chaining +
    /// SMC bookkeeping + epoch bump + waiter notification.
    fn bg_install_ready(&self, rip: u64, compiled: CompiledBlock) {
        let notify = {
            let mut cache = self.cache.write().unwrap();
            let old = cache.remove(&rip);
            let notify = match &old {
                Some(CacheEntry::Queued(n)) => Some(Arc::clone(n)),
                _ => None, // resolved elsewhere (guest inline fallback): skip
            };
            if let Some(CacheEntry::Ready(ref old)) = old {
                self.chain_ids.write().unwrap().remove(&rip);
                self.code_pages_remove_range(old.guest_start, old.guest_end);
            }
            if jit_chain_enabled()
                && let Some(fid) = compiled.func_id
            {
                self.chain_ids.write().unwrap().insert(rip, fid);
            }
            self.code_pages_add_range(compiled.guest_start, compiled.guest_end);
            cache.insert(rip, CacheEntry::Ready(compiled));
            notify
        };
        self.cache_epoch.fetch_add(1, Ordering::Relaxed);
        self.bg_compiles.fetch_add(1, Ordering::Relaxed);
        if let Some(n) = notify {
            n.notify_all();
        }
    }

    /// Worker-side resolution when the block failed to compile: mark Never and
    /// wake waiters (they fall through to iced, no inline re-attempt needed).
    fn bg_install_never(&self, rip: u64) {
        let notify = {
            let mut cache = self.cache.write().unwrap();
            match cache.remove(&rip) {
                Some(CacheEntry::Queued(n)) => {
                    cache.insert(rip, CacheEntry::Never);
                    Some(n)
                }
                _ => None,
            }
        };
        if let Some(n) = notify {
            n.notify_all();
        }
    }

    /// Worker-side resolution when the result was stale (guest memory changed
    /// while compiling): drop the Queued entry entirely so the guest re-decodes
    /// fresh, and wake waiters so they fall back immediately instead of burning
    /// their full wait budget.
    fn bg_install_drop(&self, rip: u64) {
        let notify = {
            let mut cache = self.cache.write().unwrap();
            match cache.remove(&rip) {
                Some(CacheEntry::Queued(n)) => Some(n),
                _ => None,
            }
        };
        if let Some(n) = notify {
            n.notify_all();
        }
    }

    /// Whether any guest code page write is currently pending over `[addr, addr+len)`.
    fn pending_code_write_overlaps(&self, addr: u64, len: usize) -> bool {
        if len == 0 {
            return false;
        }
        let end = addr.saturating_add(u64::try_from(len).unwrap_or(u64::MAX));
        let pending = self.pending_code_writes.lock().unwrap();
        let mut page = addr >> 12;
        let last = end.saturating_sub(1) >> 12;
        while page <= last {
            if pending.contains(&page) {
                return true;
            }
            page = page.saturating_add(1);
        }
        false
    }

    fn code_pages_overlap(&self, addr: u64, len: usize) -> bool {
        let code_pages = self.code_pages.lock().unwrap();
        if len == 0 || code_pages.is_empty() {
            return false;
        }
        let end = addr.saturating_add(u64::try_from(len).unwrap_or(u64::MAX));
        if end <= addr {
            return !code_pages.is_empty();
        }
        let mut page = addr >> 12;
        let last = end.saturating_sub(1) >> 12;
        while page <= last {
            if code_pages.contains_key(&page) {
                return true;
            }
            page = page.saturating_add(1);
        }
        false
    }

    fn code_pages_add_range(&self, guest_start: u64, guest_end: u64) {
        if guest_end <= guest_start {
            return;
        }
        let mut code_pages = self.code_pages.lock().unwrap();
        let mut page = guest_start >> 12;
        let last = guest_end.saturating_sub(1) >> 12;
        while page <= last {
            code_pages
                .entry(page)
                .and_modify(|c| *c = c.saturating_add(1))
                .or_insert(1);
            page = page.saturating_add(1);
        }
    }

    fn code_pages_remove_range(&self, guest_start: u64, guest_end: u64) {
        if guest_end <= guest_start {
            return;
        }
        let mut code_pages = self.code_pages.lock().unwrap();
        let mut page = guest_start >> 12;
        let last = guest_end.saturating_sub(1) >> 12;
        while page <= last {
            match code_pages.get_mut(&page) {
                Some(c) if *c > 1 => *c = c.saturating_sub(1),
                Some(_) => {
                    code_pages.remove(&page);
                }
                None => {}
            }
            page = page.saturating_add(1);
        }
    }
}

// SAFETY: GuestMemory is behind `RwLock`; raw pointers inside GuestMemory are
// non-owning views of mmap arenas that live for the process lifetime.
// The lock provides synchronization for metadata updates; the JIT hot path uses
// per-thread TLB with host pointers directly.
#[expect(unsafe_code)]
unsafe impl Send for JitShared {}
#[expect(unsafe_code)]
unsafe impl Sync for JitShared {}

/// Per-thread JIT execution state: registers, TLB, chain table, shadow stack.
/// One instance per guest thread. Not shared.
#[doc(hidden)]
pub struct PerThreadJitState {
    /// x86-64 register file.
    #[doc(hidden)]
    pub regs: RegFile,
    /// Runtime hook window (stop-bitmap for fake-API range).
    #[doc(hidden)]
    pub hooks: Option<HookWindow>,
    /// Recent RIP history for diagnostics (ring buffer).
    pub rip_trace: [u64; 32],
    pub rip_trace_i: usize,
    pub rip_trace_n: usize,
    /// Instructions retired via interpreter (Phase 0 baselines).
    pub iced_steps: u64,
    /// Persistent set-associative page TLB across chained blocks.
    pub tlb_sets: [TlbBucket; TLB_SETS],
    pub tlb_aux: [TlbBucketAux; TLB_SETS],
    /// Sticky last-hit page for inline IR mem path.
    pub tlb_hot_page: u64,
    pub tlb_hot_ptr: *mut u8,
    pub tlb_hot_prot: u64,
    pub tlb_hot_gen: u64,
    /// Multi sticky ways.
    pub sticky_page: [u64; STICKY_WAYS],
    pub sticky_ptr: [*mut u8; STICKY_WAYS],
    pub sticky_prot: [u64; STICKY_WAYS],
    pub sticky_gen: [u64; STICKY_WAYS],
    pub sticky_rr: u64,
    /// Region-direct pins.
    pub pins: [MemPin; PIN_SLOTS],
    /// Generation at which pins were last rebuilt.
    pub pins_gen: u64,
    /// Open-addressing guest VA → host block fn (late-bound block chaining).
    pub chain_slots: Box<[lower::ChainSlot; CHAIN_SLOTS]>,
    /// Phase 4.2 monomorphic edge IC.
    pub edge_ic_va: [u64; lower::EDGE_IC_SLOTS],
    pub edge_ic_fn: [u64; lower::EDGE_IC_SLOTS],
    pub edge_ic_rr: u64,
    /// Shadow return-stack depth.
    pub shadow_sp: u64,
    pub shadow_ret: [u64; lower::SHADOW_DEPTH],
}

// SAFETY: TLB/pin raw pointers are non-owning views of guest mmap arenas.
// Each PerThreadJitState is owned by one host thread; never moved between threads.
#[expect(unsafe_code)]
unsafe impl Send for PerThreadJitState {}

impl PerThreadJitState {
    fn new() -> Self {
        Self {
            regs: RegFile::new(),
            hooks: None,
            rip_trace: [0; 32],
            rip_trace_i: 0,
            rip_trace_n: 0,
            iced_steps: 0,
            tlb_sets: [empty_tlb_bucket(); TLB_SETS],
            tlb_aux: [empty_tlb_aux(); TLB_SETS],
            tlb_hot_page: TLB_EMPTY,
            tlb_hot_ptr: std::ptr::null_mut(),
            tlb_hot_prot: 0,
            tlb_hot_gen: 0,
            sticky_page: [TLB_EMPTY; STICKY_WAYS],
            sticky_ptr: [std::ptr::null_mut(); STICKY_WAYS],
            sticky_prot: [0; STICKY_WAYS],
            sticky_gen: [0; STICKY_WAYS],
            sticky_rr: 0,
            pins: [MemPin::EMPTY; PIN_SLOTS],
            pins_gen: u64::MAX,
            chain_slots: Box::new([lower::ChainSlot::empty(); CHAIN_SLOTS]),
            edge_ic_va: [0; lower::EDGE_IC_SLOTS],
            edge_ic_fn: [0; lower::EDGE_IC_SLOTS],
            edge_ic_rr: 0,
            shadow_sp: 0,
            shadow_ret: [0; lower::SHADOW_DEPTH],
        }
    }
}

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

pub(crate) struct JitEngine {
    module: cranelift_jit::JITModule,
    ctx: cranelift_codegen::Context,
    func_ctx: cranelift::prelude::FunctionBuilderContext,
    next_name: u32,
    /// Shared signature: `(i64 ctx_ptr)` — host C ABI (callable from Rust).
    block_sig: cranelift::codegen::ir::Signature,
    /// Host `wie_jit_load` import.
    load_id: cranelift_module::FuncId,
    /// Host `wie_jit_store` import.
    store_id: cranelift_module::FuncId,
    /// Host bulk string helper.
    string_id: cranelift_module::FuncId,
    /// Soft-translated host span for inline string copies.
    host_span_id: cranelift_module::FuncId,
    /// Scalar f32 binop helper.
    f32_id: cranelift_module::FuncId,
    /// Scalar f64 binop helper.
    f64_id: cranelift_module::FuncId,
    /// Packed integer SSE2 lane op helper (SIMD-off path + pack/pmul*).
    sse_int_id: cranelift_module::FuncId,
    /// Packed SSE2 shift helper (imm + variable count).
    sse_shift_id: cranelift_module::FuncId,
    /// `pshufb` result low half (full 16-byte table + mask).
    sse_pshufb_lo_id: cranelift_module::FuncId,
    /// `pshufb` result high half.
    sse_pshufb_hi_id: cranelift_module::FuncId,
    /// FP unary (sqrt) helper.
    sse_fp_unop_id: cranelift_module::FuncId,
    /// FP min/max helper.
    sse_fp_binop_id: cranelift_module::FuncId,
    /// Integer↔FP convert helper.
    sse_cvt_id: cranelift_module::FuncId,
    /// Host chain-table lookup (`wie_jit_chain_lookup`).
    lookup_id: cranelift_module::FuncId,
    /// UCRT fast-path imports (malloc, free, memcpy, …).
    ucrt: UcrtImportIds,
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
    if !mem_path_trace_enabled() {
        return;
    }
    let helpers = s.load_calls.saturating_add(s.store_calls);
    eprintln!(
        "[wie] mem_path helpers={helpers} load={} store={}",
        s.load_calls, s.store_calls
    );
    eprintln!(
        "[wie]   resolve: sticky={} multi={} pin={} walk={} cross={} slow={}",
        s.mem_sticky_hit,
        s.mem_multi_hit,
        s.mem_pin_hit,
        s.mem_walk_hit,
        s.mem_cross_page,
        s.mem_slow
    );
    eprintln!(
        "[wie]   sticky_miss: key={} gen={} prot={} swaps={}",
        s.mem_sticky_miss_key, s.mem_sticky_miss_gen, s.mem_sticky_miss_prot, s.mem_sticky_swaps
    );
    eprintln!(
        "[wie]   addr_vs_pin: stack={} heap={} outside={}",
        s.mem_addr_stack_pin, s.mem_addr_heap_pin, s.mem_addr_outside
    );
    eprintln!(
        "[wie]   gen: bumps={} peak={}  pins: stack_bytes={:#x} heap_bytes={:#x} allow={:#x}",
        s.mem_gen_bumps, s.mem_gen_peak, s.pin_stack_bytes, s.pin_heap_bytes, s.pin_allow_bits
    );
    if helpers > 0 {
        let pct10 = |n: u64| -> u64 { n.saturating_mul(1000).checked_div(helpers).unwrap_or(0) };
        let fmt = |n: u64| {
            let t = pct10(n);
            format!("{}.{}", t.checked_div(10).unwrap_or(0), t % 10)
        };
        eprintln!(
            "[wie]   resolve%: multi={}% pin={}% walk={}% key_miss={}% outside={}%",
            fmt(s.mem_multi_hit),
            fmt(s.mem_pin_hit),
            fmt(s.mem_walk_hit),
            fmt(s.mem_sticky_miss_key),
            fmt(s.mem_addr_outside),
        );
    }
}

/// Lightweight counters for `WIE_CPU=jit` diagnostics / Phase 0 baselines.
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
    /// compilation (worker backlog / dead worker visibility, roadmap B9).
    pub compile_stall_fallback: u64,
    /// Cache hits (native run).
    pub cache_hits: u64,
    /// Calls into host `wie_jit_load` (TLB hit or miss).
    pub load_calls: u64,
    /// Calls into host `wie_jit_store` (TLB hit or miss).
    pub store_calls: u64,
    /// Phase 4.x: selective code-cache invalidations (SMC / X-loss / unmap).
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
}

impl JitCpu {
    /// Open hybrid JIT on the host ISA (ARM64 on Apple Silicon).
    #[must_use]
    pub fn open_x86_64() -> Self {
        let shared = JitShared::new();
        Self {
            shared: Arc::new(shared),
            thread: PerThreadJitState::new(),
            fast_api: Vec::new(),
            stats: JitStats::default(),
            last_mem_gen: 0,
            chain_sync_epoch: 0,
        }
    }

    /// Borrow the shared JIT state (compilation cache + guest memory).
    #[must_use]
    pub fn shared_jit(&self) -> &Arc<JitShared> {
        &self.shared
    }

    /// Create a per-thread engine sharing the compilation cache + guest memory.
    #[must_use]
    pub fn new_shared(shared: Arc<JitShared>) -> Self {
        Self {
            shared,
            thread: PerThreadJitState::new(),
            fast_api: Vec::new(),
            stats: JitStats::default(),
            last_mem_gen: 0,
            chain_sync_epoch: 0,
        }
    }

    /// Snapshot of JIT diagnostics counters (Phase 0 baselines).
    ///
    /// Merges the shared background-compile counter into the per-thread
    /// snapshot so `WIE_RUNTIME_PROFILE` sees background work.
    #[must_use]
    pub fn stats(&self) -> JitStats {
        let mut s = self.stats;
        s.bg_compiles = s
            .bg_compiles
            .saturating_add(self.shared.bg_compiles.load(Ordering::Relaxed));
        s
    }

    /// Install UCRT/heap fast-path config (called once after fake-API table build).
    pub fn configure_fast_path(&mut self, cfg: JitFastPathConfig) {
        install_heap_layout(cfg.heap);
        let pairs = cfg.pairs.clone();
        self.fast_api = cfg.pairs;
        // Mirror the pairs so the background worker lowers UCRT calls exactly
        // like the inline path (same `call_fast` → same emitted code).
        *self.shared.bg_fast_api.lock().unwrap() = pairs;
        self.clear_compiled();
        self.invalidate_chain_and_shadow();
    }

    // `fast_api_kind` was the on-demand helper used by the old peek loop;
    // `block_kind_ends_in_fast_ucrt` inlines the same lookup on the miss path.
    // Kept accessible for future callers (e.g. non-Pure fast-API detection).
    #[allow(dead_code)]
    #[inline]
    fn fast_api_kind(&self, va: u64) -> Option<FastApiKind> {
        self.fast_api
            .iter()
            .find_map(|&(k, kind)| (k == va).then_some(kind))
    }

    fn insert_ready(&mut self, rip: u64, compiled: CompiledBlock) {
        self.shared.insert_ready(rip, compiled);
    }

    fn clear_compiled(&mut self) {
        self.shared.cache.write().unwrap().clear();
        self.shared.chain_ids.write().unwrap().clear();
        self.shared.code_pages.lock().unwrap().clear();
    }

    fn invalidate_code_range(&mut self, addr: u64, len: usize) {
        {
            let cache = self.shared.cache.read().unwrap();
            if cache.is_empty() || len == 0 {
                return;
            }
        }
        if !self.code_pages_overlap(addr, len) {
            return;
        }
        let write_end = addr.saturating_add(u64::try_from(len).unwrap_or(u64::MAX));
        let to_drop: Vec<u64> = {
            let cache = self.shared.cache.read().unwrap();
            cache
                .iter()
                .filter_map(|(va, entry)| match entry {
                    CacheEntry::Ready(c)
                        if ranges_overlap(c.guest_start, c.guest_end, addr, write_end) =>
                    {
                        Some(*va)
                    }
                    _ => None,
                })
                .collect()
        };
        if to_drop.is_empty() {
            return;
        }
        for va in &to_drop {
            let mut cache = self.shared.cache.write().unwrap();
            if let Some(CacheEntry::Ready(c)) = cache.remove(va) {
                drop(cache);
                self.shared
                    .code_pages_remove_range(c.guest_start, c.guest_end);
            }
            self.shared.chain_ids.write().unwrap().remove(va);
        }
        self.stats.code_invs = self.stats.code_invs.saturating_add(1);
        self.invalidate_chain_and_shadow();
        if jit_chain_enabled() {
            let cache = self.shared.cache.read().unwrap();
            for (va, entry) in &*cache {
                if let CacheEntry::Ready(c) = entry {
                    let fn_ptr = c.func as usize as u64;
                    chain_table_insert(self.thread.chain_slots.as_mut(), *va, fn_ptr);
                }
            }
        }
    }

    #[inline]
    fn code_pages_overlap(&self, addr: u64, len: usize) -> bool {
        self.shared.code_pages_overlap(addr, len)
    }

    fn drain_pending_code_writes(&mut self) {
        let overflow = self
            .shared
            .pending_code_overflow
            .swap(false, Ordering::Relaxed);
        let pages = std::mem::take(&mut *self.shared.pending_code_writes.lock().unwrap());
        if overflow {
            if !self.shared.cache.read().unwrap().is_empty() {
                self.clear_compiled();
                self.invalidate_chain_and_shadow();
                self.stats.code_invs = self.stats.code_invs.saturating_add(1);
            }
            return;
        }
        for page in pages {
            self.invalidate_code_range(page << 12, PAGE_SIZE_USIZE);
        }
    }

    const PENDING_WRITE_PAGE_CAP: usize = 256;

    fn note_code_write(&self, address: u64, len: usize) {
        if len == 0 || self.shared.pending_code_overflow.load(Ordering::Relaxed) {
            return;
        }
        let len_u = u64::try_from(len).unwrap_or(u64::MAX);
        let end = address.saturating_add(len_u);
        if end <= address && len > 0 {
            self.shared
                .pending_code_overflow
                .store(true, Ordering::Relaxed);
            self.shared.pending_code_writes.lock().unwrap().clear();
            return;
        }
        let mut guard = self.shared.pending_code_writes.lock().unwrap();
        let mut page = address >> 12;
        let last = end.saturating_sub(1) >> 12;
        while page <= last {
            if guard.len() >= Self::PENDING_WRITE_PAGE_CAP {
                self.shared
                    .pending_code_overflow
                    .store(true, Ordering::Relaxed);
                guard.clear();
                return;
            }
            if guard.last().copied() != Some(page) {
                guard.push(page);
            }
            page = page.saturating_add(1);
        }
    }

    fn code_inv_span_for_free(
        &self,
        addr: u64,
        size: usize,
        free_type: u32,
    ) -> Option<(u64, usize)> {
        if (free_type & mem::MEM_RELEASE) != 0 {
            return self
                .shared
                .mem
                .read()
                .unwrap()
                .allocation_span_at_base(addr);
        }
        if (free_type & mem::MEM_DECOMMIT) != 0 {
            if size == 0 {
                return None;
            }
            let page_base = addr & !(PAGE_SIZE - 1);
            let end = addr.saturating_add(u64::try_from(size).unwrap_or(u64::MAX));
            let page_end = end
                .saturating_add(PAGE_SIZE - 1)
                .wrapping_div(PAGE_SIZE)
                .saturating_mul(PAGE_SIZE);
            let n = usize::try_from(page_end.saturating_sub(page_base)).unwrap_or(0);
            if n == 0 {
                return None;
            }
            return Some((page_base, n));
        }
        None
    }

    #[cfg(test)]
    #[must_use]
    fn has_ready_at(&self, rip: u64) -> bool {
        matches!(
            self.shared.cache.read().unwrap().get(&rip),
            Some(CacheEntry::Ready(_))
        )
    }

    /// Returns `(result, guest_insns_retired)` for budget accounting.
    fn step_one(&mut self) -> Result<(StepResult, usize), CpuError> {
        let rip = self.thread.regs.rip;
        if let Some(hook) = self.thread.hooks.as_ref()
            && hook.should_host_stop(rip)
        {
            return Ok((
                StepResult::HostStop {
                    address: rip,
                    size: 1,
                },
                0,
            ));
        }

        if self.shared.engine_ready.load(Ordering::Relaxed) {
            // Read-first pattern: acquire read lock, clone entry, drop lock, then act.
            let entry = {
                let cache = self.shared.cache.read().unwrap();
                cache.get(&rip).cloned()
            };
            if let Some(entry) = entry {
                match entry {
                    CacheEntry::Ready(compiled) => {
                        self.stats.cache_hits = self.stats.cache_hits.saturating_add(1);
                        let meta = CompiledRunMeta::from(&compiled);
                        return Ok(self.finish_compiled(rip, meta));
                    }
                    CacheEntry::Never => { /* fall through to iced */ }
                    CacheEntry::Queued(notify) => {
                        // About to execute the entry the worker is compiling:
                        // block only for this entry, and only briefly. On timeout
                        // (queue backlog / dead worker) fall back to inline.
                        if let Some(compiled) = self.wait_bg_ready(rip, &notify) {
                            let meta = CompiledRunMeta::from(&compiled);
                            return Ok(self.finish_compiled(rip, meta));
                        }
                        if let Some(compiled) = self.try_compile(rip) {
                            let meta = CompiledRunMeta::from(&compiled);
                            self.insert_ready(rip, compiled);
                            return Ok(self.finish_compiled(rip, meta));
                        }
                        self.shared
                            .cache
                            .write()
                            .unwrap()
                            .insert(rip, CacheEntry::Never);
                    }
                    CacheEntry::Hot { visits, thr } => {
                        let next = visits.saturating_add(1);
                        if thr > 0 && next < thr {
                            self.shared
                                .cache
                                .write()
                                .unwrap()
                                .insert(rip, CacheEntry::Hot { visits: next, thr });
                        } else {
                            // Threshold crossed. Prefer the background worker:
                            // enqueue and keep executing on iced this visit; the
                            // compiled block lands in the cache for the next one.
                            // Inline compilation is only the fallback.
                            let kind = {
                                let mem = self.shared.mem.read().unwrap();
                                block::decode_pure_gpr_block(&mem, self.thread.hooks.as_ref(), rip)
                            };
                            match self.enqueue_bg(rip, &kind) {
                                BgEnqueueOutcome::Queued(_) | BgEnqueueOutcome::Ready => {
                                    // Continue on iced this visit.
                                }
                                BgEnqueueOutcome::Unavailable => {
                                    if let Some(compiled) = self.try_compile_from_kind(rip, kind) {
                                        let meta = CompiledRunMeta::from(&compiled);
                                        self.insert_ready(rip, compiled);
                                        return Ok(self.finish_compiled(rip, meta));
                                    }
                                    self.shared
                                        .cache
                                        .write()
                                        .unwrap()
                                        .insert(rip, CacheEntry::Never);
                                }
                            }
                        }
                    }
                }
            } else {
                // Miss: decode the block once and route the same BlockKind through
                // fast-UCRT / self-loop / try_compile. Was three iced-decode passes
                // over the same up-to-96-insn body before.
                let kind = {
                    let mem = self.shared.mem.read().unwrap();
                    block::decode_pure_gpr_block(&mem, self.thread.hooks.as_ref(), rip)
                };
                let is_ucrt = block_kind_ends_in_fast_ucrt(&self.shared, &self.fast_api, &kind);
                let is_loop = pure_is_self_loop(&kind, rip);
                let thr = if is_ucrt {
                    2
                } else if is_loop {
                    pure_loop_hotness()
                } else {
                    hotness_threshold()
                };
                if thr == 0 || is_ucrt {
                    // Eager compile: the entry is required NOW (there may be no
                    // revisit). Prefer the background worker and block briefly
                    // on this entry only; inline compile is the fallback.
                    match self.enqueue_bg(rip, &kind) {
                        BgEnqueueOutcome::Queued(notify) => {
                            if let Some(compiled) = self.wait_bg_ready(rip, &notify) {
                                let meta = CompiledRunMeta::from(&compiled);
                                return Ok(self.finish_compiled(rip, meta));
                            }
                            // Deadline missed — compile inline (replaces Queued).
                            if let Some(compiled) = self.try_compile_from_kind(rip, kind) {
                                let meta = CompiledRunMeta::from(&compiled);
                                self.insert_ready(rip, compiled);
                                return Ok(self.finish_compiled(rip, meta));
                            }
                            self.shared
                                .cache
                                .write()
                                .unwrap()
                                .insert(rip, CacheEntry::Never);
                        }
                        BgEnqueueOutcome::Ready => {
                            // Worker beat us: the cache already holds Ready.
                            let compiled = {
                                let cache = self.shared.cache.read().unwrap();
                                cache.get(&rip).and_then(|e| match e {
                                    CacheEntry::Ready(c) => Some(*c),
                                    _ => None,
                                })
                            };
                            if let Some(compiled) = compiled {
                                let meta = CompiledRunMeta::from(&compiled);
                                return Ok(self.finish_compiled(rip, meta));
                            }
                            // Vanished (invalidated mid-flight) — fall through to iced.
                        }
                        BgEnqueueOutcome::Unavailable => {
                            if let Some(compiled) = self.try_compile_from_kind(rip, kind) {
                                let meta = CompiledRunMeta::from(&compiled);
                                self.insert_ready(rip, compiled);
                                return Ok(self.finish_compiled(rip, meta));
                            }
                            self.shared
                                .cache
                                .write()
                                .unwrap()
                                .insert(rip, CacheEntry::Never);
                        }
                    }
                } else {
                    self.shared
                        .cache
                        .write()
                        .unwrap()
                        .insert(rip, CacheEntry::Hot { visits: 1, thr });
                }
            }
        }

        // Iced does not maintain the shadow return stack — drop prediction.
        self.thread.shadow_sp = 0;
        self.stats.iced_insns = self.stats.iced_insns.saturating_add(1);
        // Inline step_once_result: push RIP trace, call exec::step, update counters.
        {
            let rip = self.thread.regs.rip;
            let i = self.thread.rip_trace_i & 31;
            if let Some(slot) = self.thread.rip_trace.get_mut(i) {
                *slot = rip;
            }
            self.thread.rip_trace_i = self.thread.rip_trace_i.wrapping_add(1);
            if self.thread.rip_trace_n < 32 {
                self.thread.rip_trace_n = self.thread.rip_trace_n.saturating_add(1);
            }
        }
        let hook = self.thread.hooks.as_ref();
        let result = exec::step(
            &self.shared.mem.read().unwrap(),
            &mut self.thread.regs,
            hook,
        )?;
        if matches!(result, StepResult::Continue) {
            self.thread.iced_steps = self.thread.iced_steps.saturating_add(1);
        }
        self.drain_pending_code_writes();
        Ok((result, 1))
    }

    /// True when a Pure block at `rip` ends in a near-call to a registered UCRT fast API.
    ///
    /// Kept for callers that don't already have a decoded [`BlockKind`] in hand
    /// (currently none — the miss path pre-decodes and calls the helper below).
    #[allow(dead_code)]
    fn peek_fast_ucrt_call(&self, rip: u64) -> bool {
        if self.fast_api.is_empty() {
            return false;
        }
        let mem = self.shared.mem.read().unwrap();
        let kind = decode_pure_gpr_block(&mem, self.thread.hooks.as_ref(), rip);
        drop(mem);
        block_kind_ends_in_fast_ucrt(&self.shared, &self.fast_api, &kind)
    }

    #[allow(dead_code)]
    fn peek_self_loop(&self, rip: u64) -> bool {
        let mem = self.shared.mem.read().unwrap();
        let kind = decode_pure_gpr_block(&mem, self.thread.hooks.as_ref(), rip);
        pure_is_self_loop(&kind, rip)
    }

    /// Hand a block to the background compiler.
    ///
    /// Queues the exact decoded block so the worker compiles the same bytes the
    /// guest classified. The cache entry transitions to `Queued` only after the
    /// queue slot is reserved (a full queue must never strand a Queued entry).
    fn enqueue_bg(&mut self, rip: u64, kind: &BlockKind) -> BgEnqueueOutcome {
        if !self.shared.bg_enabled_here() || !self.shared.engine_ready.load(Ordering::Relaxed) {
            return BgEnqueueOutcome::Unavailable;
        }
        self.shared.ensure_bg_worker();
        if !self.shared.bg_alive.load(Ordering::Relaxed) {
            return BgEnqueueOutcome::Unavailable;
        }
        if matches!(kind, BlockKind::NotPure) {
            self.shared
                .cache
                .write()
                .unwrap()
                .insert(rip, CacheEntry::Never);
            return BgEnqueueOutcome::Unavailable;
        }
        let tx_guard = self.shared.bg_tx.lock().unwrap();
        let Some(tx) = tx_guard.as_ref() else {
            return BgEnqueueOutcome::Unavailable;
        };
        if tx.try_send((rip, kind.clone())).is_err() {
            return BgEnqueueOutcome::Unavailable;
        }
        // Transition the entry (only from Hot/absent; never clobber Ready/Never).
        let mut cache = self.shared.cache.write().unwrap();
        match cache.get(&rip) {
            None | Some(CacheEntry::Hot { .. }) => {
                let cell = BgWaitCell::new();
                cache.insert(rip, CacheEntry::Queued(Arc::clone(&cell)));
                BgEnqueueOutcome::Queued(cell)
            }
            Some(CacheEntry::Queued(_) | CacheEntry::Never) => BgEnqueueOutcome::Unavailable,
            Some(CacheEntry::Ready(_)) => BgEnqueueOutcome::Ready,
        }
    }

    /// Wait (bounded) for the background worker to resolve `rip`.
    ///
    /// The guest only reaches this when it is about to execute the entry. The
    /// wait is per-entry (the cell from the Queued entry, not the whole queue)
    /// and time-boxed by [`bg_wait_timeout`]; on timeout the caller falls back
    /// to inline compilation so a worker stall can never deadlock the guest.
    /// Returns the Ready block once installed.
    fn wait_bg_ready(&mut self, rip: u64, cell: &BgWaitCell) -> Option<CompiledBlock> {
        if !self.shared.bg_alive.load(Ordering::Relaxed) {
            return None; // worker gone: inline fallback
        }
        let budget = bg_wait_timeout();
        let start = Instant::now();
        loop {
            let state = {
                let cache = self.shared.cache.read().unwrap();
                match cache.get(&rip) {
                    Some(CacheEntry::Ready(c)) => Some(BgWaitState::Ready(*c)),
                    Some(CacheEntry::Never) => Some(BgWaitState::Never),
                    Some(CacheEntry::Queued(_)) => None,
                    // Re-decided or invalidated while we waited: inline fallback.
                    Some(CacheEntry::Hot { .. }) | None => return None,
                }
            };
            match state {
                Some(BgWaitState::Ready(c)) => {
                    self.stats.compile_stalls = self.stats.compile_stalls.saturating_add(1);
                    self.stats.compile_stall_us = self.stats.compile_stall_us.saturating_add(
                        u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX),
                    );
                    return Some(c);
                }
                Some(BgWaitState::Never) => return None, // worker failed → iced
                None => {}
            }
            let elapsed = start.elapsed();
            if elapsed >= budget {
                self.stats.compile_stalls = self.stats.compile_stalls.saturating_add(1);
                self.stats.compile_stall_us = self
                    .stats
                    .compile_stall_us
                    .saturating_add(u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX));
                self.stats.compile_stall_fallback =
                    self.stats.compile_stall_fallback.saturating_add(1);
                return None;
            }
            // Chunked wait: a notification that races with our re-check (or a
            // spurious wakeup) costs at most one 1 ms chunk, never the whole
            // budget. The loop re-checks the cache after each wake.
            cell.wait_timeout(budget.saturating_sub(elapsed).min(Duration::from_millis(1)));
        }
    }

    /// Re-insert every Ready block into this thread's late-bound chain table.
    ///
    /// Runs only when the shared `cache_epoch` advanced (background installs),
    /// so worker-compiled blocks chain from compiled code exactly like inline
    /// compiles would have.
    fn resync_chain_table(&mut self) {
        if !jit_chain_enabled() {
            return;
        }
        let cache = self.shared.cache.read().unwrap();
        for (va, entry) in &*cache {
            if let CacheEntry::Ready(c) = entry {
                let fn_ptr = c.func as usize as u64;
                chain_table_insert(self.thread.chain_slots.as_mut(), *va, fn_ptr);
            }
        }
    }

    fn try_compile(&mut self, rip: u64) -> Option<CompiledBlock> {
        let kind = {
            let mem_guard = self.shared.mem.read().unwrap();
            decode_pure_gpr_block(&mem_guard, self.thread.hooks.as_ref(), rip)
        };
        self.try_compile_from_kind(rip, kind)
    }

    /// Compile a block from an already-decoded [`BlockKind`], skipping the
    /// full iced-decode pass that would otherwise repeat previous work.
    ///
    /// Shares the lowering with the background worker ([`JitShared::compile_from_kind_shared`]);
    /// this wrapper adds the per-thread side effects: compile stats and the
    /// thread-local chain-table entry.
    fn try_compile_from_kind(&mut self, rip: u64, result: BlockKind) -> Option<CompiledBlock> {
        let compiled = self
            .shared
            .compile_from_kind_shared(&self.fast_api, rip, result);
        let Some(compiled) = compiled else {
            self.stats.compile_skip = self.stats.compile_skip.saturating_add(1);
            return None;
        };
        self.stats.compiles = self.stats.compiles.saturating_add(1);
        if jit_chain_enabled() {
            let fn_ptr = compiled.func as usize as u64;
            chain_table_insert(self.thread.chain_slots.as_mut(), rip, fn_ptr);
        }
        tracing::debug!(
            start = format_args!("{rip:#x}"),
            insns = compiled.insn_count,
            "jit compiled block"
        );
        Some(compiled)
    }

    fn finish_compiled(&mut self, entry_rip: u64, meta: CompiledRunMeta) -> (StepResult, usize) {
        if let Some(inv) = self.run_compiled(entry_rip, meta) {
            (StepResult::InvalidMemory(inv), 0)
        } else {
            self.stats.jit_insns = self
                .stats
                .jit_insns
                .saturating_add(u64::from(meta.insn_count));
            (
                StepResult::Continue,
                usize::try_from(meta.insn_count).unwrap_or(1),
            )
        }
    }

    /// Returns `Some(InvalidMem)` when a host mem helper faulted.
    fn run_compiled(&mut self, entry_rip: u64, meta: CompiledRunMeta) -> Option<exec::InvalidMem> {
        // Refresh pins only when GuestMemory generation changes (map/protect/free).
        // Rebuilding VAD-ranked pins every block was measurable on 7za.
        let mem_gen = self.shared.mem_gen.load(Ordering::Acquire);
        if self.thread.pins_gen != mem_gen {
            let mem = self.shared.mem.read().unwrap();
            let infos = mem.jit_region_pins();
            self.thread.pins = std::array::from_fn(|i| MemPin::from_info(infos[i]));
            self.thread.pins_gen = mem_gen;
        }
        // Gen-bump + pin-shape diagnostics (cheap; always on for stats).
        if self.last_mem_gen != 0 && mem_gen > self.last_mem_gen {
            self.stats.mem_gen_bumps = self
                .stats
                .mem_gen_bumps
                .saturating_add(mem_gen.saturating_sub(self.last_mem_gen));
        }
        self.last_mem_gen = mem_gen;
        if mem_gen > self.stats.mem_gen_peak {
            self.stats.mem_gen_peak = mem_gen;
        }
        {
            let stack = self.thread.pins[0];
            self.stats.pin_stack_bytes = if stack.is_empty() {
                0
            } else {
                stack.span_bytes()
            };
            let mut data_bytes = 0_u64;
            let mut bits = 0_u64;
            if !stack.is_empty() {
                bits |= stack.allow & 0b11;
            }
            for (i, pin) in self.thread.pins.iter().enumerate().skip(1) {
                if pin.is_empty() {
                    continue;
                }
                data_bytes = data_bytes.saturating_add(pin.span_bytes());
                // Pack first two data pins' allow into bits 2..5 for compact dump.
                if i <= 2 {
                    let shift = (i.saturating_sub(1).saturating_add(1)) * 2;
                    bits |= (pin.allow & 0b11) << shift;
                }
            }
            self.stats.pin_heap_bytes = data_bytes;
            self.stats.pin_allow_bits = bits;
        }
        let mem_guard = self.shared.mem.read().unwrap();
        let mem_ptr = (&raw const *mem_guard).cast_mut();
        let regs = &mut self.thread.regs;
        // Full GPR snapshot on entry: late-bound chaining reloads live regs from
        // JitCtx, so every architectural GPR must be valid for successors.
        let mut gpr = [0_u64; 16];
        for (i, slot) in gpr.iter_mut().enumerate() {
            *slot = regs.gpr(i);
        }
        // Pure GPR blocks skip the XMM bank copy on both sides of the call.
        // SSE blocks load only live XMMs (Phase 5.5 Track A live mask).
        let mut xmm = [XmmSlot::ZERO; 16];
        if meta.uses_sse {
            let mut m = meta.xmm_live_mask;
            // If mask is empty but uses_sse (fp-only edge), load all.
            if m == 0 {
                m = 0xffff;
            }
            let mut i = 0_usize;
            while m != 0 {
                if m & 1 != 0 {
                    let v = regs.xmm_at(i);
                    if let Some(slot) = xmm.get_mut(i) {
                        *slot = XmmSlot::from_u128(v);
                    }
                }
                m >>= 1;
                i = i.saturating_add(1);
            }
        }
        let mut ctx = JitCtx {
            gpr,
            rflags: regs.rflags,
            rip: entry_rip,
            mem: mem_ptr,
            fault: 0,
            fault_addr: 0,
            fault_size: 0,
            fault_access: 0,
            tlb_sets: self.thread.tlb_sets,
            tlb_aux: self.thread.tlb_aux,
            xmm,
            shadow_sp: self.thread.shadow_sp,
            shadow_ret: self.thread.shadow_ret,
            chain_slots: self.thread.chain_slots.as_mut_ptr(),
            tlb_hot_page: self.thread.tlb_hot_page,
            tlb_hot_ptr: self.thread.tlb_hot_ptr,
            // 0 = Cranelift path (host falls back to full writeback);
            // trampolines OR their dirty bits; chain sets 0xffff.
            gpr_dirty_bits: 0,
            load_calls: 0,
            store_calls: 0,
            tlb_hot_prot: self.thread.tlb_hot_prot,
            mem_gen,
            tlb_hot_gen: self.thread.tlb_hot_gen,
            pins: self.thread.pins,
            edge_ic_va: self.thread.edge_ic_va,
            edge_ic_fn: self.thread.edge_ic_fn,
            edge_ic_rr: self.thread.edge_ic_rr,
            xmm_dirty_bits: 0,
            mem_path: MemPathSlice::default(),
            sticky_page: self.thread.sticky_page,
            sticky_ptr: self.thread.sticky_ptr,
            sticky_prot: self.thread.sticky_prot,
            sticky_gen: self.thread.sticky_gen,
            sticky_rr: self.thread.sticky_rr,
            // Fresh dispatcher entry always starts a new host-chain budget.
            chain_depth: 0,
        };
        drop(mem_guard); // GuestMemory read lock already released; compiled block runs on TLB/pins.
        // SAFETY: func is a finalized Cranelift block; TLB/pins resolve to stable mmap pointers.
        unsafe {
            (meta.func)(std::ptr::from_mut(&mut ctx));
        }
        self.stats.load_calls = self.stats.load_calls.saturating_add(ctx.load_calls);
        self.stats.store_calls = self.stats.store_calls.saturating_add(ctx.store_calls);
        {
            let m = &ctx.mem_path;
            let s = &mut self.stats;
            s.mem_sticky_hit = s.mem_sticky_hit.saturating_add(m.sticky_hit);
            s.mem_multi_hit = s.mem_multi_hit.saturating_add(m.multi_hit);
            s.mem_pin_hit = s.mem_pin_hit.saturating_add(m.pin_hit);
            s.mem_walk_hit = s.mem_walk_hit.saturating_add(m.walk_hit);
            s.mem_cross_page = s.mem_cross_page.saturating_add(m.cross_page);
            s.mem_slow = s.mem_slow.saturating_add(m.slow);
            s.mem_sticky_miss_key = s.mem_sticky_miss_key.saturating_add(m.sticky_miss_key);
            s.mem_sticky_miss_gen = s.mem_sticky_miss_gen.saturating_add(m.sticky_miss_gen);
            s.mem_sticky_miss_prot = s.mem_sticky_miss_prot.saturating_add(m.sticky_miss_prot);
            s.mem_sticky_swaps = s.mem_sticky_swaps.saturating_add(m.sticky_swaps);
            s.mem_addr_stack_pin = s.mem_addr_stack_pin.saturating_add(m.addr_in_stack_pin);
            s.mem_addr_heap_pin = s.mem_addr_heap_pin.saturating_add(m.addr_in_heap_pin);
            s.mem_addr_outside = s.mem_addr_outside.saturating_add(m.addr_outside_pins);
        }
        // Phase 4.x: guest stores via `GuestMemory::write` leave a pending range;
        // apply selective code invalidation only after the native frame returns.
        // Persist per-thread execution state from JitCtx.
        self.thread.tlb_sets = ctx.tlb_sets;
        self.thread.tlb_aux = ctx.tlb_aux;
        self.thread.tlb_hot_page = ctx.tlb_hot_page;
        self.thread.tlb_hot_ptr = ctx.tlb_hot_ptr;
        self.thread.tlb_hot_prot = ctx.tlb_hot_prot;
        self.thread.tlb_hot_gen = ctx.tlb_hot_gen;
        self.thread.sticky_page = ctx.sticky_page;
        self.thread.sticky_ptr = ctx.sticky_ptr;
        self.thread.sticky_prot = ctx.sticky_prot;
        self.thread.sticky_gen = ctx.sticky_gen;
        self.thread.sticky_rr = ctx.sticky_rr;
        self.thread.edge_ic_va = ctx.edge_ic_va;
        self.thread.edge_ic_fn = ctx.edge_ic_fn;
        self.thread.edge_ic_rr = ctx.edge_ic_rr;
        self.thread.shadow_sp = ctx.shadow_sp;
        self.thread.shadow_ret = ctx.shadow_ret;
        // Prefer cumulative trampoline dirty bits (partial writeback when a
        // micro-stub does not chain). Cranelift leaves bits at 0 → full sync
        // (internal block chaining can dirty arbitrary GPRs).
        let dirty = if ctx.fault != 0 || ctx.gpr_dirty_bits == 0 {
            0xffff_u16
        } else {
            ctx.gpr_dirty_bits as u16
        };
        if dirty == 0xffff {
            for i in 0..16 {
                if let Some(&v) = ctx.gpr.get(i) {
                    regs.set_gpr(i, v);
                }
            }
        } else {
            let mut m = dirty;
            let mut i = 0_usize;
            while m != 0 {
                if m & 1 != 0
                    && let Some(&v) = ctx.gpr.get(i)
                {
                    regs.set_gpr(i, v);
                }
                m >>= 1;
                i = i.saturating_add(1);
            }
        }
        if meta.uses_sse {
            // Cranelift blocks skip the per-def `xmm_dirty_bits` RMW — the static
            // `xmm_may_def_mask` covers them. Trampolines still set dirty from Rust,
            // so we always OR both so trampoline-only writes and Cranelift writes
            // are both covered.
            let dirty = if ctx.fault != 0 {
                u16::try_from(ctx.xmm_dirty_bits).unwrap_or(0xffff)
            } else {
                u16::try_from(ctx.xmm_dirty_bits).unwrap_or(0)
            };
            let mut mask = dirty | meta.xmm_may_def_mask;
            if mask == 0 {
                mask = meta.xmm_live_mask;
            }
            let mut i = 0_usize;
            let mut m = mask;
            while m != 0 {
                if m & 1 != 0
                    && let Some(slot) = ctx.xmm.get(i)
                {
                    regs.set_xmm_at(i, slot.to_u128());
                }
                m >>= 1;
                i = i.saturating_add(1);
            }
        }
        regs.set_rflags_checked(ctx.rflags);
        regs.rip = ctx.rip;
        let fault = if ctx.fault != 0 {
            Some(exec::InvalidMem {
                access_type: i32::try_from(ctx.fault_access).unwrap_or(0),
                address: ctx.fault_addr,
                size: i32::try_from(ctx.fault_size).unwrap_or(0),
                value: 0,
            })
        } else {
            None
        };
        // Safe point: drop Ready blocks overlapping any stores from this block.
        self.drain_pending_code_writes();
        fault
    }

    fn invalidate_tlb(&mut self) {
        self.thread.tlb_sets = [empty_tlb_bucket(); TLB_SETS];
        self.thread.tlb_aux = [empty_tlb_aux(); TLB_SETS];
        self.thread.tlb_hot_page = TLB_EMPTY;
        self.thread.tlb_hot_ptr = std::ptr::null_mut();
        self.thread.tlb_hot_prot = 0;
        self.thread.tlb_hot_gen = 0;
        self.thread.sticky_page = [TLB_EMPTY; STICKY_WAYS];
        self.thread.sticky_ptr = [std::ptr::null_mut(); STICKY_WAYS];
        self.thread.sticky_prot = [0; STICKY_WAYS];
        self.thread.sticky_gen = [0; STICKY_WAYS];
        self.thread.sticky_rr = 0;
        self.thread.pins = [MemPin::EMPTY; PIN_SLOTS];
        self.thread.pins_gen = u64::MAX;
    }

    fn invalidate_chain_and_shadow(&mut self) {
        chain_table_clear(self.thread.chain_slots.as_mut());
        self.thread.edge_ic_va = [0; lower::EDGE_IC_SLOTS];
        self.thread.edge_ic_fn = [0; lower::EDGE_IC_SLOTS];
        self.thread.edge_ic_rr = 0;
        self.thread.shadow_sp = 0;
        self.thread.shadow_ret = [0; lower::SHADOW_DEPTH];
        // The table is empty now; force a full re-sync from the cache on the
        // next dispatch (picks up worker-installed Ready blocks too).
        self.chain_sync_epoch = u64::MAX;
    }
}

/// Snapshot of a Ready block needed to run it without holding a cache borrow.
#[derive(Clone, Copy)]
struct CompiledRunMeta {
    func: unsafe extern "C" fn(*mut JitCtx),
    insn_count: u32,
    uses_sse: bool,
    /// XMMi referenced in the block (selective entry load).
    xmm_live_mask: u16,
    /// XMMi that may be defined (conservative exit writeback on fault).
    xmm_may_def_mask: u16,
}

impl From<&CompiledBlock> for CompiledRunMeta {
    fn from(c: &CompiledBlock) -> Self {
        Self {
            func: c.func,
            insn_count: c.insn_count,
            uses_sse: c.uses_sse,
            xmm_live_mask: c.xmm_live_mask,
            xmm_may_def_mask: c.xmm_may_def_mask,
        }
    }
}

/// Half-open range overlap: `[a0, a1)` vs `[b0, b1)`.
#[inline]
fn ranges_overlap(a0: u64, a1: u64, b0: u64, b1: u64) -> bool {
    a0 < b1 && b0 < a1
}

/// Follow PE import thunks / short jumps to the final callee VA.
/// Predicate over an already-decoded [`BlockKind`]: does its terminator call
/// a registered UCRT fast-path API? Shared between `peek_fast_ucrt_call` and
/// the miss-path pre-decode so the block is decoded only once.
fn block_kind_ends_in_fast_ucrt(
    shared: &JitShared,
    fast_api: &[(u64, FastApiKind)],
    kind: &BlockKind,
) -> bool {
    if fast_api.is_empty() {
        return false;
    }
    let target = match kind {
        BlockKind::Pure {
            term: Some(block::BlockTerm::Call { target, .. }),
            ..
        } => *target,
        _ => return false,
    };
    let mem = shared.mem.read().unwrap();
    let final_va = resolve_thunk_va(&mem, target);
    fast_api.iter().any(|&(k, _)| k == final_va)
}

fn resolve_thunk_va(mem: &GuestMemory, mut va: u64) -> u64 {
    let mut buf = [0_u8; 16];
    for _ in 0..4 {
        if mem.read(va, &mut buf).is_err() {
            return va;
        }
        if buf[0] == 0xff && buf[1] == 0x25 {
            let rel = i32::from_le_bytes([buf[2], buf[3], buf[4], buf[5]]);
            let iat = va
                .wrapping_add(6)
                .wrapping_add(i64::from(rel).cast_unsigned());
            let mut slot = [0_u8; 8];
            if mem.read(iat, &mut slot).is_ok() {
                va = u64::from_le_bytes(slot);
                continue;
            }
            return va;
        }
        if buf[0] == 0xe9 {
            let rel = i32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]);
            va = va
                .wrapping_add(5)
                .wrapping_add(i64::from(rel).cast_unsigned());
            continue;
        }
        if buf[0] == 0x48 && buf[1] == 0xb8 && buf[10] == 0xff && buf[11] == 0xe0 {
            va = u64::from_le_bytes([
                buf[2], buf[3], buf[4], buf[5], buf[6], buf[7], buf[8], buf[9],
            ]);
            continue;
        }
        if buf[0] == 0xeb {
            let rel = buf[1].cast_signed();
            va = va
                .wrapping_add(2)
                .wrapping_add(i64::from(rel).cast_unsigned());
            continue;
        }
        return va;
    }
    va
}

/// Cranelift `opt_level` from `WIE_JIT_OPT` (`speed` | `speed_and_size` | `none`).
/// Default: `speed` (Phase 5.5 — hot guest blocks over code size).
fn jit_opt_level() -> &'static str {
    use std::sync::OnceLock;
    static LVL: OnceLock<&'static str> = OnceLock::new();
    LVL.get_or_init(|| match std::env::var("WIE_JIT_OPT") {
        Ok(v) if v.eq_ignore_ascii_case("none") || v == "0" => "none",
        Ok(v)
            if v.eq_ignore_ascii_case("speed_and_size")
                || v.eq_ignore_ascii_case("size")
                || v.eq_ignore_ascii_case("speed-and-size") =>
        {
            "speed_and_size"
        }
        Ok(v) if v.eq_ignore_ascii_case("speed") || v.eq_ignore_ascii_case("fast") => "speed",
        _ => "speed",
    })
}

/// Run Cranelift IR verifier only when `WIE_JIT_VERIFY=1`.
///
/// Previously enabled under `cfg(test)` unconditionally — every test-driven
/// perf run (release-mode `cargo test`) paid the verifier tax on every
/// compile. Tests that need verifier coverage should set `WIE_JIT_VERIFY=1`
/// explicitly. The oracle tests already exercise the lowering paths without
/// requiring an always-on verifier.
fn jit_verifier_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        matches!(
            std::env::var("WIE_JIT_VERIFY"),
            Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on")
        )
    })
}

impl JitEngine {
    fn new() -> Result<Self, String> {
        use cranelift::prelude::*;
        use cranelift_codegen::settings::Configurable;
        use cranelift_jit::{JITBuilder, JITModule};
        use cranelift_module::{Linkage, Module, default_libcall_names};

        let mut flag_builder = settings::builder();
        // Phase 5.5 Track D: prefer speed of host code for hot translated blocks.
        flag_builder
            .set("opt_level", jit_opt_level())
            .map_err(|e| e.to_string())?;
        let verify = if jit_verifier_enabled() {
            "true"
        } else {
            "false"
        };
        flag_builder
            .set("enable_verifier", verify)
            .map_err(|e| e.to_string())?;
        flag_builder
            .set("is_pic", "false")
            .map_err(|e| e.to_string())?;
        flag_builder
            .set("use_colocated_libcalls", "false")
            .map_err(|e| e.to_string())?;
        flag_builder
            .set("enable_probestack", "false")
            .map_err(|e| e.to_string())?;
        // Guest frames are not host-unwound; skip metadata tax.
        flag_builder
            .set("unwind_info", "false")
            .map_err(|e| e.to_string())?;
        // Not a Wasm sandbox heap — soft-translate already bounds guest accesses.
        flag_builder
            .set("enable_heap_access_spectre_mitigation", "false")
            .map_err(|e| e.to_string())?;

        let mut isa_builder =
            cranelift_native::builder().map_err(|msg| format!("host ISA unsupported: {msg}"))?;
        // Apple Silicon: cranelift_native already enables LSE/PAC/FP16 + macOS PAC B-key.
        // Re-assert PAC signing so JIT call/return stays ABI-consistent if detect fails.
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            isa_builder
                .enable("sign_return_address")
                .map_err(|e| e.to_string())?;
            isa_builder
                .enable("sign_return_address_with_bkey")
                .map_err(|e| e.to_string())?;
            isa_builder.enable("has_pauth").map_err(|e| e.to_string())?;
        }
        let isa = isa_builder
            .finish(settings::Flags::new(flag_builder))
            .map_err(|e| e.to_string())?;

        let mut builder = JITBuilder::with_isa(isa, default_libcall_names());
        // SAFETY: function pointers are valid for the process lifetime.
        builder.symbol("wie_jit_load", wie_jit_load as *const u8);
        builder.symbol("wie_jit_store", wie_jit_store as *const u8);
        builder.symbol("wie_jit_string", wie_jit_string as *const u8);
        builder.symbol("wie_jit_host_span", wie_jit_host_span as *const u8);
        builder.symbol("wie_f32_binop", wie_f32_binop as *const u8);
        builder.symbol("wie_f64_binop", wie_f64_binop as *const u8);
        builder.symbol("wie_sse_int_binop", wie_sse_int_binop as *const u8);
        builder.symbol("wie_sse_shift", wie_sse_shift as *const u8);
        builder.symbol("wie_sse_pshufb_lo", wie_sse_pshufb_lo as *const u8);
        builder.symbol("wie_sse_pshufb_hi", wie_sse_pshufb_hi as *const u8);
        builder.symbol("wie_sse_fp_unop", wie_sse_fp_unop as *const u8);
        builder.symbol("wie_sse_fp_binop", wie_sse_fp_binop as *const u8);
        builder.symbol("wie_sse_cvt", wie_sse_cvt as *const u8);
        builder.symbol("wie_jit_chain_lookup", wie_jit_chain_lookup as *const u8);
        builder.symbol("wie_ucrt_malloc", wie_ucrt_malloc as *const u8);
        builder.symbol("wie_ucrt_free", wie_ucrt_free as *const u8);
        builder.symbol("wie_ucrt_memcpy", wie_ucrt_memcpy as *const u8);
        builder.symbol("wie_ucrt_strlen", wie_ucrt_strlen as *const u8);
        builder.symbol("wie_ucrt_iob", wie_ucrt_iob as *const u8);
        builder.symbol("wie_ucrt_fwrite", wie_ucrt_fwrite as *const u8);
        builder.symbol("wie_ucrt_fflush", wie_ucrt_fflush as *const u8);
        let mut module = JITModule::new(builder);

        // Host default call-conv (AppleAarch64 / SystemV) — must match Rust `extern "C"`.
        let mut block_sig = module.make_signature();
        block_sig.params.push(AbiParam::new(types::I64));

        // load: (ctx, addr, size, insn_ip) -> i64
        let mut load_sig = module.make_signature();
        load_sig.params.push(AbiParam::new(types::I64));
        load_sig.params.push(AbiParam::new(types::I64));
        load_sig.params.push(AbiParam::new(types::I64));
        load_sig.params.push(AbiParam::new(types::I64));
        load_sig.returns.push(AbiParam::new(types::I64));
        let load_id = module
            .declare_function("wie_jit_load", Linkage::Import, &load_sig)
            .map_err(|e| e.to_string())?;

        // store: (ctx, addr, size, value, insn_ip)
        let mut store_sig = module.make_signature();
        store_sig.params.push(AbiParam::new(types::I64));
        store_sig.params.push(AbiParam::new(types::I64));
        store_sig.params.push(AbiParam::new(types::I64));
        store_sig.params.push(AbiParam::new(types::I64));
        store_sig.params.push(AbiParam::new(types::I64));
        let store_id = module
            .declare_function("wie_jit_store", Linkage::Import, &store_sig)
            .map_err(|e| e.to_string())?;

        // string: (ctx, op, size, flags, insn_ip) -> stay
        let mut string_sig = module.make_signature();
        string_sig.params.push(AbiParam::new(types::I64));
        string_sig.params.push(AbiParam::new(types::I64));
        string_sig.params.push(AbiParam::new(types::I64));
        string_sig.params.push(AbiParam::new(types::I64));
        string_sig.params.push(AbiParam::new(types::I64));
        string_sig.returns.push(AbiParam::new(types::I64));
        let string_id = module
            .declare_function("wie_jit_string", Linkage::Import, &string_sig)
            .map_err(|e| e.to_string())?;

        // host_span: (ctx, guest_va, len, write) -> host_ptr_or_0
        let mut span_sig = module.make_signature();
        span_sig.params.push(AbiParam::new(types::I64));
        span_sig.params.push(AbiParam::new(types::I64));
        span_sig.params.push(AbiParam::new(types::I64));
        span_sig.params.push(AbiParam::new(types::I64));
        span_sig.returns.push(AbiParam::new(types::I64));
        let host_span_id = module
            .declare_function("wie_jit_host_span", Linkage::Import, &span_sig)
            .map_err(|e| e.to_string())?;

        // f32/f64 binop: (op, a, b) -> r
        let mut f_sig = module.make_signature();
        f_sig.params.push(AbiParam::new(types::I64));
        f_sig.params.push(AbiParam::new(types::I64));
        f_sig.params.push(AbiParam::new(types::I64));
        f_sig.returns.push(AbiParam::new(types::I64));
        let f32_id = module
            .declare_function("wie_f32_binop", Linkage::Import, &f_sig)
            .map_err(|e| e.to_string())?;
        let f64_id = module
            .declare_function("wie_f64_binop", Linkage::Import, &f_sig)
            .map_err(|e| e.to_string())?;

        // sse int/shift/fp-minmax binop: (op, a, b) -> r — same shape as f_sig.
        let sse_int_id = module
            .declare_function("wie_sse_int_binop", Linkage::Import, &f_sig)
            .map_err(|e| e.to_string())?;
        let sse_shift_id = module
            .declare_function("wie_sse_shift", Linkage::Import, &f_sig)
            .map_err(|e| e.to_string())?;
        let sse_fp_binop_id = module
            .declare_function("wie_sse_fp_binop", Linkage::Import, &f_sig)
            .map_err(|e| e.to_string())?;

        // fp unop / cvt: (op, a) -> r.
        let mut sse2_sig = module.make_signature();
        sse2_sig.params.push(AbiParam::new(types::I64));
        sse2_sig.params.push(AbiParam::new(types::I64));
        sse2_sig.returns.push(AbiParam::new(types::I64));
        let sse_fp_unop_id = module
            .declare_function("wie_sse_fp_unop", Linkage::Import, &sse2_sig)
            .map_err(|e| e.to_string())?;
        let sse_cvt_id = module
            .declare_function("wie_sse_cvt", Linkage::Import, &sse2_sig)
            .map_err(|e| e.to_string())?;

        // pshufb halves: (a_lo, a_hi, b_lo, b_hi) -> r.
        let mut sse4_sig = module.make_signature();
        for _ in 0..4 {
            sse4_sig.params.push(AbiParam::new(types::I64));
        }
        sse4_sig.returns.push(AbiParam::new(types::I64));
        let sse_pshufb_lo_id = module
            .declare_function("wie_sse_pshufb_lo", Linkage::Import, &sse4_sig)
            .map_err(|e| e.to_string())?;
        let sse_pshufb_hi_id = module
            .declare_function("wie_sse_pshufb_hi", Linkage::Import, &sse4_sig)
            .map_err(|e| e.to_string())?;

        // chain lookup: (ctx, va) -> fn_ptr
        let mut lookup_sig = module.make_signature();
        lookup_sig.params.push(AbiParam::new(types::I64));
        lookup_sig.params.push(AbiParam::new(types::I64));
        lookup_sig.returns.push(AbiParam::new(types::I64));
        let lookup_id = module
            .declare_function("wie_jit_chain_lookup", Linkage::Import, &lookup_sig)
            .map_err(|e| e.to_string())?;

        // UCRT: (ctx, …args) -> rax  /  free is void
        let mut sig_ctx1 = module.make_signature();
        sig_ctx1.params.push(AbiParam::new(types::I64)); // ctx
        sig_ctx1.params.push(AbiParam::new(types::I64)); // a0
        sig_ctx1.returns.push(AbiParam::new(types::I64));
        let mut sig_ctx1_void = module.make_signature();
        sig_ctx1_void.params.push(AbiParam::new(types::I64));
        sig_ctx1_void.params.push(AbiParam::new(types::I64));
        let mut sig_ctx3 = module.make_signature();
        sig_ctx3.params.push(AbiParam::new(types::I64));
        sig_ctx3.params.push(AbiParam::new(types::I64));
        sig_ctx3.params.push(AbiParam::new(types::I64));
        sig_ctx3.params.push(AbiParam::new(types::I64));
        sig_ctx3.returns.push(AbiParam::new(types::I64));
        let mut sig_ctx4 = module.make_signature();
        sig_ctx4.params.push(AbiParam::new(types::I64));
        for _ in 0..4 {
            sig_ctx4.params.push(AbiParam::new(types::I64));
        }
        sig_ctx4.returns.push(AbiParam::new(types::I64));
        let mut sig_1 = module.make_signature();
        sig_1.params.push(AbiParam::new(types::I64));
        sig_1.returns.push(AbiParam::new(types::I64));

        let malloc = module
            .declare_function("wie_ucrt_malloc", Linkage::Import, &sig_ctx1)
            .map_err(|e| e.to_string())?;
        let free = module
            .declare_function("wie_ucrt_free", Linkage::Import, &sig_ctx1_void)
            .map_err(|e| e.to_string())?;
        let memcpy = module
            .declare_function("wie_ucrt_memcpy", Linkage::Import, &sig_ctx3)
            .map_err(|e| e.to_string())?;
        let strlen = module
            .declare_function("wie_ucrt_strlen", Linkage::Import, &sig_ctx1)
            .map_err(|e| e.to_string())?;
        let iob = module
            .declare_function("wie_ucrt_iob", Linkage::Import, &sig_1)
            .map_err(|e| e.to_string())?;
        let fwrite = module
            .declare_function("wie_ucrt_fwrite", Linkage::Import, &sig_ctx4)
            .map_err(|e| e.to_string())?;
        let fflush = module
            .declare_function("wie_ucrt_fflush", Linkage::Import, &sig_1)
            .map_err(|e| e.to_string())?;

        Ok(Self {
            module,
            ctx: cranelift_codegen::Context::new(),
            func_ctx: FunctionBuilderContext::new(),
            next_name: 0,
            block_sig,
            load_id,
            store_id,
            string_id,
            host_span_id,
            f32_id,
            f64_id,
            sse_int_id,
            sse_shift_id,
            sse_pshufb_lo_id,
            sse_pshufb_hi_id,
            sse_fp_unop_id,
            sse_fp_binop_id,
            sse_cvt_id,
            lookup_id,
            ucrt: UcrtImportIds {
                malloc,
                free,
                memcpy,
                strlen,
                iob,
                fwrite,
                fflush,
            },
        })
    }
}

impl CpuEngine for JitCpu {
    fn mem_map(&mut self, address: u64, size: usize, perms: RwxPerms) -> Result<(), CpuError> {
        let mut mem = self.shared.mem.write().unwrap();
        let r = mem.map(address, size, perms);
        self.shared
            .mem_gen
            .store(mem.generation(), Ordering::Release);
        r
    }

    fn mem_write(&mut self, address: u64, bytes: &[u8]) -> Result<(), CpuError> {
        self.shared.mem.write().unwrap().write(address, bytes)?;
        self.note_code_write(address, bytes.len());
        self.drain_pending_code_writes();
        Ok(())
    }

    fn mem_read(&mut self, address: u64, bytes: &mut [u8]) -> Result<(), CpuError> {
        self.shared.mem.read().unwrap().read(address, bytes)
    }

    fn host_span(&mut self, address: u64, len: usize, write: bool) -> Option<*mut u8> {
        self.shared
            .mem
            .read()
            .unwrap()
            .host_span(address, len, write)
    }

    fn host_slice(&self, address: u64, len: usize) -> Option<&[u8]> {
        if len == 0 {
            return Some(&[]);
        }
        let ptr = self
            .shared
            .mem
            .read()
            .unwrap()
            .host_span(address, len, false)?;
        // SAFETY: as `IcedCpu::host_slice` — the mmap arena outlives the read
        // guard, and the `&self` borrow excludes concurrent unmapping.
        #[expect(unsafe_code)]
        Some(unsafe { std::slice::from_raw_parts(ptr, len) })
    }

    fn mem_copy(&mut self, dst: u64, src: u64, len: usize) -> bool {
        self.shared.mem.read().unwrap().mem_copy(dst, src, len)
    }

    fn mem_fill(&mut self, address: u64, byte: u8, len: usize) -> bool {
        self.shared.mem.read().unwrap().mem_fill(address, byte, len)
    }

    fn mem_generation(&self) -> u64 {
        self.shared.mem.read().unwrap().generation()
    }

    fn virtual_alloc(
        &mut self,
        addr: u64,
        size: usize,
        alloc_type: u32,
        protect: u32,
    ) -> Result<u64, CpuError> {
        let r = self
            .shared
            .mem
            .write()
            .unwrap()
            .virtual_alloc(addr, size, alloc_type, protect);
        self.shared.mem_gen.store(
            self.shared.mem.read().unwrap().generation(),
            Ordering::Release,
        );
        self.invalidate_tlb();
        r
    }

    fn virtual_free(&mut self, addr: u64, size: usize, free_type: u32) -> Result<(), CpuError> {
        let inv_span = self.code_inv_span_for_free(addr, size, free_type);
        self.invalidate_tlb();
        let r = self
            .shared
            .mem
            .write()
            .unwrap()
            .virtual_free(addr, size, free_type);
        self.shared.mem_gen.store(
            self.shared.mem.read().unwrap().generation(),
            Ordering::Release,
        );
        if r.is_ok()
            && let Some((a, n)) = inv_span
        {
            self.invalidate_code_range(a, n);
        }
        r
    }

    fn virtual_protect(
        &mut self,
        addr: u64,
        size: usize,
        new_protect: u32,
    ) -> Result<u32, CpuError> {
        let r = self
            .shared
            .mem
            .write()
            .unwrap()
            .virtual_protect(addr, size, new_protect);
        self.shared.mem_gen.store(
            self.shared.mem.read().unwrap().generation(),
            Ordering::Release,
        );
        // X-loss: dropping execute permission invalidates any compiled blocks
        // over the range. An unparseable protect is treated as non-executable,
        // matching the previous `allows_execute(u32)`, which returned false for
        // values outside the supported set.
        let loses_exec = crate::mem::protect::PageProtect::from_win32(new_protect)
            .is_none_or(|p| !p.allows_execute());
        if r.is_ok() && loses_exec {
            self.invalidate_code_range(addr, size);
        }
        self.invalidate_tlb();
        r
    }

    fn virtual_query(&self, addr: u64) -> crate::MemoryBasicInformation {
        self.shared.mem.read().unwrap().virtual_query(addr)
    }

    fn flush_instruction_cache(&mut self, addr: u64, size: usize) -> Result<(), CpuError> {
        if size == 0 {
            if !self.shared.cache.read().unwrap().is_empty() {
                self.clear_compiled();
                self.invalidate_chain_and_shadow();
                self.stats.code_invs = self.stats.code_invs.saturating_add(1);
            }
        } else {
            self.invalidate_code_range(addr, size);
        }
        Ok(())
    }

    fn mem_map_image(
        &mut self,
        address: u64,
        size: usize,
        perms: RwxPerms,
    ) -> Result<(), CpuError> {
        let r = self
            .shared
            .mem
            .write()
            .unwrap()
            .map_image(address, size, perms);
        self.shared.mem_gen.store(
            self.shared.mem.read().unwrap().generation(),
            Ordering::Release,
        );
        self.invalidate_tlb();
        r
    }

    fn cpu_stats(&self) -> Option<crate::JitStats> {
        Some(self.stats())
    }

    fn mem_backend_name(&self) -> &'static str {
        self.shared.mem.read().unwrap().backend_name()
    }

    fn register_region(&mut self, region: crate::mem::GuestRegion) {
        self.shared.mem.write().unwrap().register_region(region);
    }

    fn find_region(&self, va: u64) -> Option<crate::mem::GuestRegion> {
        self.shared.mem.read().unwrap().find_region(va).cloned()
    }

    fn install_runtime_hooks(
        &mut self,
        hook_begin: u64,
        hook_end: u64,
        stop_bitmap: std::sync::Arc<[u8]>,
    ) -> Result<(), CpuError> {
        self.clear_compiled();
        self.invalidate_tlb();
        self.invalidate_chain_and_shadow();
        let range_len = hook_end.saturating_sub(hook_begin).saturating_add(1);
        let expected_bytes = usize::try_from(range_len).unwrap_or(usize::MAX).div_ceil(8);
        if expected_bytes != usize::MAX && stop_bitmap.len() < expected_bytes {
            return Err(CpuError::Message(format!(
                "stop_bitmap too small: {} < {expected_bytes}",
                stop_bitmap.len()
            )));
        }
        self.thread.hooks = Some(HookWindow {
            begin: hook_begin,
            end: hook_end,
            stop_bitmap,
        });
        Ok(())
    }

    fn configure_jit_fast_path(&mut self, cfg: JitFastPathConfig) {
        self.configure_fast_path(cfg);
        self.invalidate_chain_and_shadow();
    }

    fn precompile_at(&mut self, address: u64) {
        if !self.shared.engine_ready.load(Ordering::Relaxed) {
            return;
        }
        if let Some(hook) = self.thread.hooks.as_ref()
            && hook.should_host_stop(address)
        {
            return;
        }
        if let Some(compiled) = self.try_compile(address) {
            self.insert_ready(address, compiled);
        } else {
            self.shared
                .cache
                .write()
                .unwrap()
                .entry(address)
                .or_insert(CacheEntry::Never);
        }
    }

    fn run_until_stop(
        &mut self,
        begin: u64,
        until: u64,
        _timeout: u64,
        count: usize,
        _hook_begin: u64,
        _hook_end: u64,
    ) -> Result<RunUntilHook, CpuError> {
        self.thread.regs.rip = begin;
        let budget = if count == 0 { 100_000_000_usize } else { count };
        let mut executed = 0_usize;
        while executed < budget {
            let rip = self.thread.regs.rip;
            if until != 0 && rip == until {
                break;
            }
            if let Some(hook) = self.thread.hooks.as_ref()
                && hook.should_host_stop(rip)
            {
                return Ok(RunUntilHook {
                    code: CodeHookOutcome {
                        hit: true,
                        address: rip,
                        size: 1,
                    },
                    invalid_memory: InvalidMemoryAccess {
                        hit: false,
                        exception_code: 0,
                        access_type: 0,
                        address: 0,
                        size: 0,
                        value: 0,
                    },
                });
            }
            // Pick up background-installed Ready blocks into this thread's
            // chain table (cheap relaxed load; resync only after an install).
            let epoch = self.shared.cache_epoch.load(Ordering::Relaxed);
            if epoch != self.chain_sync_epoch {
                self.chain_sync_epoch = epoch;
                self.resync_chain_table();
            }
            // Hot chain: run consecutive Ready blocks without re-entering step_one.
            let mut chain_result = None;
            if self.shared.engine_ready.load(Ordering::Relaxed) {
                let meta = {
                    let cache = self.shared.cache.read().unwrap();
                    cache.get(&rip).and_then(|e| match e {
                        CacheEntry::Ready(c) => Some(CompiledRunMeta::from(c)),
                        _ => None,
                    })
                };
                if let Some(meta) = meta {
                    self.stats.cache_hits = self.stats.cache_hits.saturating_add(1);
                    let (result, retired) = self.finish_compiled(rip, meta);
                    match result {
                        StepResult::Continue => {
                            executed = executed.saturating_add(retired.max(1));
                            continue;
                        }
                        other => {
                            chain_result = Some(other);
                        }
                    }
                }
            }
            if let Some(result) = chain_result {
                return match result {
                    StepResult::HostStop { address, size } => Ok(RunUntilHook {
                        code: CodeHookOutcome {
                            hit: true,
                            address,
                            size,
                        },
                        invalid_memory: InvalidMemoryAccess {
                            hit: false,
                            exception_code: 0,
                            access_type: 0,
                            address: 0,
                            size: 0,
                            value: 0,
                        },
                    }),
                    StepResult::InvalidMemory(inv) => Ok(RunUntilHook {
                        code: CodeHookOutcome {
                            hit: false,
                            address: 0,
                            size: 0,
                        },
                        invalid_memory: InvalidMemoryAccess {
                            hit: true,
                            exception_code: crate::exception_code::ACCESS_VIOLATION,
                            access_type: inv.access_type,
                            address: inv.address,
                            size: inv.size,
                            value: inv.value,
                        },
                    }),
                    StepResult::Continue => Err(CpuError::Message(
                        "unexpected Continue from chained block RET".into(),
                    )),
                };
            }
            let (result, retired) = self.step_one()?;
            match result {
                StepResult::Continue => {
                    executed = executed.saturating_add(retired.max(1));
                }
                StepResult::HostStop { address, size } => {
                    return Ok(RunUntilHook {
                        code: CodeHookOutcome {
                            hit: true,
                            address,
                            size,
                        },
                        invalid_memory: InvalidMemoryAccess {
                            hit: false,
                            exception_code: 0,
                            access_type: 0,
                            address: 0,
                            size: 0,
                            value: 0,
                        },
                    });
                }
                StepResult::InvalidMemory(inv) => {
                    return Ok(RunUntilHook {
                        code: CodeHookOutcome {
                            hit: false,
                            address: 0,
                            size: 0,
                        },
                        invalid_memory: InvalidMemoryAccess {
                            hit: true,
                            exception_code: crate::exception_code::ACCESS_VIOLATION,
                            access_type: inv.access_type,
                            address: inv.address,
                            size: inv.size,
                            value: inv.value,
                        },
                    });
                }
            }
        }
        Ok(RunUntilHook {
            code: CodeHookOutcome {
                hit: false,
                address: 0,
                size: 0,
            },
            invalid_memory: InvalidMemoryAccess {
                hit: false,
                exception_code: 0,
                access_type: 0,
                address: 0,
                size: 0,
                value: 0,
            },
        })
    }

    fn return_from_win64_api(&mut self, rax: u64) -> Result<u64, CpuError> {
        self.thread.shadow_sp = 0;
        let rsp = self.thread.regs.rsp();
        let mut ret_bytes = [0_u8; 8];
        self.shared
            .mem
            .read()
            .unwrap()
            .read(rsp, &mut ret_bytes)
            .map_err(|e| CpuError::Message(format!("return_from_win64_api stack read: {e}")))?;
        let return_address = u64::from_le_bytes(ret_bytes);
        self.thread.regs.set_rsp(rsp.wrapping_add(8));
        self.thread.regs.set_rax(rax);
        self.thread.regs.rip = return_address;
        Ok(return_address)
    }

    fn read_rip(&mut self) -> Result<u64, CpuError> {
        Ok(self.thread.regs.rip)
    }
    fn write_rip(&mut self, value: u64) -> Result<(), CpuError> {
        self.thread.regs.rip = value;
        Ok(())
    }
    fn read_rsp(&mut self) -> Result<u64, CpuError> {
        Ok(self.thread.regs.rsp())
    }
    fn write_rsp(&mut self, value: u64) -> Result<(), CpuError> {
        self.thread.regs.set_rsp(value);
        Ok(())
    }
    fn read_rax(&mut self) -> Result<u64, CpuError> {
        Ok(self.thread.regs.rax())
    }
    fn write_rax(&mut self, value: u64) -> Result<(), CpuError> {
        self.thread.regs.set_rax(value);
        Ok(())
    }
    fn read_rcx(&mut self) -> Result<u64, CpuError> {
        Ok(self.thread.regs.rcx())
    }
    fn write_rcx(&mut self, value: u64) -> Result<(), CpuError> {
        self.thread.regs.set_rcx(value);
        Ok(())
    }
    fn read_rdx(&mut self) -> Result<u64, CpuError> {
        Ok(self.thread.regs.rdx())
    }
    fn write_rdx(&mut self, value: u64) -> Result<(), CpuError> {
        self.thread.regs.set_rdx(value);
        Ok(())
    }
    fn read_r8(&mut self) -> Result<u64, CpuError> {
        Ok(self.thread.regs.r8())
    }
    fn write_r8(&mut self, value: u64) -> Result<(), CpuError> {
        self.thread.regs.set_r8(value);
        Ok(())
    }
    fn read_r9(&mut self) -> Result<u64, CpuError> {
        Ok(self.thread.regs.r9())
    }
    fn write_r9(&mut self, value: u64) -> Result<(), CpuError> {
        self.thread.regs.set_r9(value);
        Ok(())
    }
    fn read_rbx(&mut self) -> Result<u64, CpuError> {
        Ok(self.thread.regs.gpr(3))
    }
    fn read_r12(&mut self) -> Result<u64, CpuError> {
        Ok(self.thread.regs.gpr(12))
    }

    fn snapshot_thread_context(&mut self) -> crate::ThreadContext {
        self.thread.regs.snapshot()
    }

    fn restore_thread_context(&mut self, ctx: &crate::ThreadContext) {
        self.thread.regs.restore(ctx);
    }

    fn on_thread_switch(&mut self) {
        self.invalidate_tlb();
        self.invalidate_chain_and_shadow();
    }
}

#[cfg(test)]
#[expect(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::mem::protect;
    use crate::mem::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE};

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
            if jit_chain_enabled() {
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

    // --- Phase 7 stress residual (invalidation multi-region / FIC) ---

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
        assert!(cpu.shared.chain_ids.read().unwrap().contains_key(&base));
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
            cpu.shared.cache.read().unwrap().get(&base),
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

    // --- B4: integer-SIMD JIT family — iced vs JIT dual-path gates ---
    //
    // Every newly-lowered SSE2 mnemonic runs once on the iced interpreter
    // (reference) and once through a compiled JIT block with identical guest
    // state; the two register files must match exactly (GPRs, XMMs, RFLAGS,
    // RIP). The `has_ready_at` assertion guarantees the block actually went
    // through Cranelift rather than silently falling back to iced.

    use crate::IcedCpu;
    use crate::exec::StepResult;
    use crate::regs::rflags;
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
        ] {
            let (iced, jit) = simd_dual(bytes, &[], |r| set_pair(r, x0, x1));
            assert_same_regs(&iced, &jit, name);
        }
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
                r.rflags = rflags::ALWAYS1 | rflags::CF;
            });
            assert_same_regs(&iced, &jit, name);
            // a < b → CF=1, ZF=0, PF=0, OF/AF/SF=0.
            assert!(iced.flag(rflags::CF), "{name} iced CF");
            assert!(!iced.flag(rflags::ZF), "{name} iced ZF");
            assert!(!iced.flag(rflags::OF), "{name} iced OF");
            assert!(jit.flag(rflags::CF), "{name} jit CF");
            assert!(!jit.flag(rflags::ZF), "{name} jit ZF");
            assert!(!jit.flag(rflags::OF), "{name} jit OF");
        }
        // a == b → ZF=1, CF=0, PF=0.
        let (iced, jit) = simd_dual(&[0x0f, 0x2f, 0xc1], &[], |r| set_pair(r, b32, b32));
        assert!(iced.flag(rflags::ZF), "iced eq ZF");
        assert!(!iced.flag(rflags::CF), "iced eq CF");
        assert!(jit.flag(rflags::ZF), "jit eq ZF");
        assert!(!jit.flag(rflags::CF), "jit eq CF");
        // NaN → unordered: ZF=PF=CF=1.
        let (iced, jit) = simd_dual(&[0x0f, 0x2f, 0xc1], &[], |r| {
            set_pair(r, u128::from(f32::NAN.to_bits()), b32);
        });
        assert!(iced.flag(rflags::PF), "iced nan PF");
        assert!(iced.flag(rflags::CF), "iced nan CF");
        assert!(jit.flag(rflags::PF), "jit nan PF");
        assert!(jit.flag(rflags::CF), "jit nan CF");
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
}
