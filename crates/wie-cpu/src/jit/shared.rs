//! Process-wide compiled-code cache, guest memory, and the background compiler worker.
//!
//! One [`JitShared`] per process, shared via `Arc` across all per-thread
//! engines. Wait cells / enqueue outcomes / worker helpers used from
//! `pipeline.rs` or `mod.rs` tests are `pub(super)`.

#![allow(
    unsafe_code, // Cranelift finalized fn pointers + host mem helpers
    private_interfaces // JitShared/PerThreadJitState expose crate-private types
)]

use super::CacheEntry;
use super::block::{self, BlockKind};
use super::config::{BG_QUEUE_CAP, JitConfig};
use super::engine::JitEngine;
use super::fast_api::FastApiKind;
use super::gen_tlb::GenTlb;
use super::lower::{
    self, CHAIN_SLOTS, CompiledBlock, MemPin, PIN_SLOTS, STICKY_WAYS, TLB_EMPTY, TLB_SETS,
    TLB_WAYS_PER_SET, TlbValue, compile_block,
};
use super::pipeline::resolve_thunk_va;
use super::trampolines::match_micro_stub;
use crate::ConcurrentHashMap;
use crate::exec::HookWindow;
use crate::mem::GuestMemory;
use crate::regs::RegFile;
use ahash::HashMap;
use ahash::HashMapExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex, RwLock, Weak};
use std::time::Duration;

/// Per-entry wait cell for background compiles.
///
/// A mutex + condvar pair lets a guest thread block **only** on the entry it
/// is about to execute (the worker calls [`Self::notify_all`] when it resolves
/// that entry), instead of waiting on the whole queue. `notify_all` needs no
/// lock; the mutex exists solely so `wait_timeout` is well-defined.
pub(super) struct BgWaitCell {
    lock: Mutex<()>,
    cv: Condvar,
}

impl BgWaitCell {
    pub(super) fn new() -> Arc<Self> {
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
    pub(super) fn wait_timeout(&self, timeout: Duration) {
        // The lock guards only the condvar wait (no panic-capable work is done
        // while holding it), so a poison would mean a hard invariant violation
        // we cannot recover from — treat it as an unrecoverable fault.
        let guard = self.lock.lock().expect("bg wait lock poisoned");
        let (guard, _timed_out) = self
            .cv
            .wait_timeout(guard, timeout)
            .expect("condvar wait_timeout");
        drop(guard);
    }
}

/// Result of handing a block to the background worker.
pub(super) enum BgEnqueueOutcome {
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
pub(super) enum BgWaitState {
    Ready(CompiledBlock),
    Never,
}

/// Process-wide JIT compilation cache and guest memory, shared across threads.
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
    pub cache: ConcurrentHashMap<u64, CacheEntry>,
    /// Ready-block FuncIds for chaining.
    #[doc(hidden)]
    pub chain_ids: ConcurrentHashMap<u64, cranelift_module::FuncId>,
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
    /// the worker lowers calls exactly like the inline path would. Shared as an
    /// `Arc<[_]>` so each background job pays a refcount bump, not a Vec clone.
    pub bg_fast_api: Mutex<Arc<[(u64, FastApiKind)]>>,
    /// Test-only latch forcing the background path on for this instance
    /// (env-independent, and per-`JitShared` so parallel unit tests cannot
    /// interfere with each other).
    #[cfg(test)]
    pub(super) bg_force: AtomicBool,
}

impl JitShared {
    pub(super) fn new() -> Self {
        let has_engine = match JitEngine::new() {
            Ok(e) => {
                tracing::debug!("cranelift JIT module ready");
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
            cache: ConcurrentHashMap::new(),
            chain_ids: ConcurrentHashMap::new(),
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
            bg_fast_api: Mutex::new(Arc::from(Vec::new())),
            #[cfg(test)]
            bg_force: AtomicBool::new(false),
        }
    }

    /// Whether the background path is enabled for this instance: env default
    /// (on for real runs, off under `cfg(test)`) or the test latch.
    pub(super) fn bg_enabled_here(&self) -> bool {
        JitConfig::get().bg_enabled() || self.bg_force_test()
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
            let cache = self.cache.pin();
            let old = cache.remove(&rip).cloned();
            if let Some(CacheEntry::Ready(old)) = old {
                self.chain_ids.pin().remove(&rip);
                Some((old.guest_start, old.guest_end))
            } else {
                None
            }
        };
        if let Some((gs, ge)) = removed {
            self.code_pages_remove_range(gs, ge);
        }
        if JitConfig::get().chain_enabled()
            && let Some(fid) = compiled.func_id
        {
            self.chain_ids.pin().insert(rip, fid);
        }
        self.code_pages_add_range(compiled.guest_start, compiled.guest_end);
        self.cache.pin().insert(rip, CacheEntry::Ready(compiled));
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
            // Arc clone: refcount bump only (table is built once per engine).
            let fast_api = shared.bg_fast_api.lock().unwrap().clone();
            let compiled = shared.compile_from_kind_shared(fast_api.as_ref(), rip, kind);
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
    pub(super) fn compile_from_kind_shared(
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
                let chain_on = JitConfig::get().chain_enabled();
                // Snapshot the chain-id table for this compile (pin guard must
                // not outlive the snapshot — the engine lock and `compile_block`
                // run after it drops).
                let chain_map: HashMap<u64, cranelift_module::FuncId> = if chain_on {
                    let guard = self.chain_ids.pin();
                    guard.iter().map(|(&va, &fid)| (va, fid)).collect()
                } else {
                    HashMap::new()
                };
                let mut eng_guard = self.engine.lock().unwrap();
                let eng = eng_guard.as_mut()?;
                compile_block(
                    eng, rip, &insns, end_rip, term, call_fast, &chain_map, bytes_len,
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
            let cache = self.cache.pin();
            let old = cache.remove(&rip).cloned();
            let notify = match &old {
                Some(CacheEntry::Queued(n)) => Some(Arc::clone(n)),
                _ => None, // resolved elsewhere (guest inline fallback): skip
            };
            if let Some(CacheEntry::Ready(old)) = old {
                self.chain_ids.pin().remove(&rip);
                self.code_pages_remove_range(old.guest_start, old.guest_end);
            }
            if JitConfig::get().chain_enabled()
                && let Some(fid) = compiled.func_id
            {
                self.chain_ids.pin().insert(rip, fid);
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
            let cache = self.cache.pin();
            match cache.remove(&rip).cloned() {
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
            let cache = self.cache.pin();
            match cache.remove(&rip).cloned() {
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

    pub(super) fn code_pages_overlap(&self, addr: u64, len: usize) -> bool {
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

    pub(super) fn code_pages_remove_range(&self, guest_start: u64, guest_end: u64) {
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
    /// Instructions retired via the iced interpreter (diagnostic counter).
    pub iced_steps: u64,
    /// Persistent set-associative page TLB across chained blocks.
    pub tlb: GenTlb<u64, TlbValue, TLB_SETS, TLB_WAYS_PER_SET>,
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
    /// Monomorphic edge IC.
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
    pub(super) fn new() -> Self {
        Self {
            regs: RegFile::new(),
            hooks: None,
            rip_trace: [0; 32],
            rip_trace_i: 0,
            rip_trace_n: 0,
            iced_steps: 0,
            tlb: GenTlb::new(),
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
