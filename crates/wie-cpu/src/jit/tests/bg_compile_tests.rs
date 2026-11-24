use super::harness::alloc_code;
use super::*;

/// `mov eax, 0x2a; nop` then `ud2` — a 2-insn pure fallthrough block. The
/// trailing `ud2` stops the linear decode (a zero-filled page would otherwise
/// decode as `add [rax], al` for the full 96-insn budget).
const MOV_EAX_2A: [u8; 8] = [0xb8, 0x2a, 0x00, 0x00, 0x00, 0x90, 0x0f, 0x0b];

// --- Background compiler (B10) ---

#[test]
fn bg_worker_end_to_end_step() {
    // Force the background path on for this engine only (parallel tests
    // keep their own inline default).
    let mut cpu = JitCpu::open_x86_64();
    cpu.shared.bg_force.store(true, Ordering::Relaxed);
    let base = alloc_code(&mut cpu, &MOV_EAX_2A);
    cpu.write_rip(base).expect("rip");

    // Eager first visit (hotness 0 in tests): enqueue + bounded wait.
    // The block may run via the worker's Ready install within the wait
    // budget, or — on a timeout (slow first compile in debug) — via the
    // cooldown path: one interpreted visit now, Ready installed by the
    // worker shortly after. Both produce identical guest state, so only
    // the outcomes are asserted; Ready is polled for below.
    let (result, _retired) = cpu.step_one().expect("step");
    assert!(matches!(result, StepResult::Continue));
    assert_eq!(cpu.thread.regs.rax(), 0x2a);
    // The worker processes the queued job regardless of who won the race;
    // poll (bounded) for its install so the shared counters are settled.
    let deadline = Instant::now() + Duration::from_secs(5);
    while cpu.stats().bg.compiles < 1 || !cpu.has_ready_at(base) {
        assert!(Instant::now() < deadline, "worker install timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(cpu.has_ready_at(base), "a Ready block must be installed");
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
    cpu.resync_chain_table(cpu.shared.cache_epoch.load(Ordering::Relaxed));
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
    let outcome = cpu.enqueue_bg(
        base,
        &BlockKind::NotPure,
        cpu.shared.invalidate_gen.load(Ordering::Relaxed),
        true,
    );
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
    let cell = BgWaitCell::new(100);
    let r = cpu.wait_bg_ready(0x1022_0000_u64, &cell);
    assert!(r.is_none());
    assert_eq!(cpu.stats().bg.inline_fallbacks, 0);
}

#[test]
fn bg_worker_dedups_queued_entry() {
    let mut cpu = JitCpu::open_x86_64();
    cpu.shared.bg_force.store(true, Ordering::Relaxed);
    let base = alloc_code(&mut cpu, &MOV_EAX_2A);
    let kind = {
        let mem = cpu.shared.mem.read().unwrap();
        block::decode_pure_gpr_block(&mem, cpu.thread.hooks.as_ref(), base)
    };
    assert!(matches!(kind, BlockKind::Pure { .. }));
    let baked = cpu.shared.invalidate_gen.load(Ordering::Relaxed);
    let first = cpu.enqueue_bg(base, &kind, baked, true);
    assert!(matches!(first, BgEnqueueOutcome::Queued(_)));
    // A second enqueue of the same rip must not re-queue: either the entry
    // is still Queued (dedup → Unavailable) or the worker already won
    // (Ready). Never a fresh Queued cell.
    let second = cpu.enqueue_bg(base, &kind, baked, true);
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
    let base = alloc_code(&mut cpu, &MOV_EAX_2A);
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

    // Visit 2: next=3, not < 3 → crossed. Background is disabled in the
    // unit suite (no bg_force), so enqueue reports Unavailable and the
    // inline fallback compiles — the ONLY path allowed to do so.
    cpu.write_rip(base).expect("rip");
    let inline_before = cpu.stats().profile.inline_compiles;
    let (result, _) = cpu.step_one().expect("step 2");
    assert!(matches!(result, StepResult::Continue));
    assert!(cpu.has_ready_at(base), "threshold crossed → compiled");
    assert!(
        cpu.stats().profile.inline_compiles > inline_before,
        "Unavailable outcome must still inline-compile"
    );
    let s = cpu.stats();
    assert_eq!(s.promo.timed_out, 0);
    assert_eq!(s.promo.cooled_down, 0);
    assert_eq!(s.bg.inline_fallbacks, 0, "no wait happened on this path");

    // Invalidation clears the compiled block (SMC / unmap path).
    cpu.invalidate_code_range(base, 8);
    assert!(!cpu.has_ready_at(base), "invalidation must drop the block");

    // --- Cooldown × invalidation interleaving ---
    // Arm a cooldown near its threshold, then invalidate the range:
    // only Ready entries are dropped, so the cooldown entry survives
    // and keeps its hysteresis threshold.
    cpu.shared.cache.pin().insert(
        base,
        CacheEntry::Hot {
            visits: 198,
            thr: 200,
        },
    );
    cpu.invalidate_code_range(base, 8);
    assert!(
        matches!(
            cpu.shared.cache.pin().get(&base),
            Some(CacheEntry::Hot {
                visits: 198,
                thr: 200
            })
        ),
        "invalidation must not clobber a cooldown entry"
    );
    // Crossing after invalidation recompiles from the CURRENT bytes.
    cpu.write_rip(base).expect("rip");
    let (result, _) = cpu.step_one().expect("re-cross visit 1");
    assert!(matches!(result, StepResult::Continue));
    assert!(!cpu.has_ready_at(base), "visit 1 below threshold");
    cpu.write_rip(base).expect("rip");
    let (result, _) = cpu.step_one().expect("re-cross visit 2");
    assert!(matches!(result, StepResult::Continue));
    assert!(
        cpu.has_ready_at(base),
        "post-invalidation re-cross compiles"
    );
    cpu.invalidate_code_range(base, 8);
    assert!(
        !cpu.has_ready_at(base),
        "second invalidation drops it again"
    );

    // Reverse order — Ready invalidated while a fresh Hot counter is
    // growing: the counter survives, next crossing recompiles.
    cpu.shared.cache.pin().insert(
        base,
        CacheEntry::Hot {
            visits: 201,
            thr: 202,
        },
    );
    cpu.invalidate_code_range(base, 8);
    cpu.write_rip(base).expect("rip");
    let (result, _) = cpu.step_one().expect("counter-survives step");
    assert!(matches!(result, StepResult::Continue));
    assert!(
        cpu.has_ready_at(base),
        "surviving counter re-crosses immediately"
    );
}

// --- Timeout → Cooldown (never inline while the worker lives) ---

/// Simulated stalled worker: an alive `bg_alive` flag with nobody draining
/// the queue. The wait must time out and then make progress via an inline
/// compile on the guest thread instead of cooling down.
#[test]
fn bg_timeout_arms_cooldown_never_inline_compiles() {
    let mut cpu = JitCpu::open_x86_64();
    let base = alloc_code(&mut cpu, &MOV_EAX_2A);
    // Alive-but-stalled worker: no thread will ever resolve this entry.
    cpu.shared.bg_alive.store(true, Ordering::Relaxed);
    let cell = BgWaitCell::new(100);
    cpu.shared
        .cache
        .pin()
        .insert(base, CacheEntry::Queued(Arc::clone(&cell)));

    cpu.write_rip(base).expect("rip");
    let (result, retired) = cpu.step_one().expect("step");
    assert!(matches!(result, StepResult::Continue));
    // Timeout inline-compiles the block (2 insns) instead of running one iced step.
    assert!(
        retired >= 2,
        "timeout must inline-compile, not run one iced insn"
    );
    assert!(
        cpu.has_ready_at(base),
        "timeout must install Ready via inline compile"
    );
    let s = cpu.stats();
    assert_eq!(
        s.bg.inline_fallbacks, 1,
        "inline fallback must fire on timeout"
    );
    assert_eq!(s.promo.timed_out, 1);
    assert_eq!(s.promo.cooled_down, 0, "timeout must not arm cooldown");
    assert_eq!(s.promo.stalled_ok, 0);

    // Invalidation drops the Ready block installed by the inline compile.
    cpu.invalidate_code_range(base, 8);
    assert!(
        !cpu.has_ready_at(base),
        "invalidation must drop the inline-installed Ready"
    );
}

/// Timeout now makes progress via inline compile instead of cooldown
/// hysteresis; successive timeouts each install a Ready block.
#[test]
fn bg_cooldown_hysteresis_doubles_then_quadruples() {
    let mut cpu = JitCpu::open_x86_64();
    let base = alloc_code(&mut cpu, &MOV_EAX_2A);
    cpu.shared.bg_alive.store(true, Ordering::Relaxed);
    for (i, &thr) in [100_u32, 200, 400].iter().enumerate() {
        let cell = BgWaitCell::new(thr);
        cpu.shared
            .cache
            .pin()
            .insert(base, CacheEntry::Queued(Arc::clone(&cell)));
        let r = cpu.wait_bg_ready(base, &cell);
        assert!(r.is_some(), "timeout must inline-compile Ready (iter {i})");
        assert!(
            cpu.has_ready_at(base),
            "timeout {i}: Ready must be installed"
        );
        // Clear for next iteration (invalidate drops Ready).
        cpu.invalidate_code_range(base, 8);
        assert!(!cpu.has_ready_at(base));
    }
    let s = cpu.stats();
    assert_eq!(s.promo.timed_out, 3);
    assert_eq!(s.promo.cooled_down, 0, "timeout must not arm cooldown");
    assert_eq!(s.bg.inline_fallbacks, 3);
    // NotPure block still marks Never instead of looping as Hot.
    let cell = BgWaitCell::new(30_000);
    cpu.shared
        .cache
        .pin()
        .insert(base, CacheEntry::Queued(Arc::clone(&cell)));
    // Corrupt the code so the inline compile fails (NotPure due to ud2-only).
    cpu.mem_write(base, &[0x0f, 0x0b, 0x0f, 0x0b])
        .expect("bad code");
    let r = cpu.wait_bg_ready(base, &cell);
    assert!(r.is_none(), "NotPure inline must return None");
    assert!(matches!(
        cpu.shared.cache.pin().get(&base),
        Some(CacheEntry::Never)
    ));
}

/// A cooled-down block re-promotes once its raised threshold is crossed
/// and the real worker installs the Ready block.
#[test]
fn bg_cooldown_repromotes_and_installs_after_crossing() {
    let mut cpu = JitCpu::open_x86_64();
    cpu.shared.bg_force.store(true, Ordering::Relaxed);
    let base = alloc_code(&mut cpu, &MOV_EAX_2A);
    // Cooldown armed near its threshold: two more visits cross it.
    cpu.shared.cache.pin().insert(
        base,
        CacheEntry::Hot {
            visits: 198,
            thr: 200,
        },
    );
    for _ in 0..2 {
        cpu.write_rip(base).expect("rip");
        let (result, _) = cpu.step_one().expect("step");
        assert!(matches!(result, StepResult::Continue));
    }
    // Second crossing enqueued to the real worker; poll for install.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cpu.has_ready_at(base) {
        assert!(
            Instant::now() < deadline,
            "worker never installed after re-promotion"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(cpu.stats().bg.compiles >= 1);
    let s = cpu.stats();
    assert_eq!(s.promo.timed_out, 0);
    assert_eq!(s.promo.deferred, 0);
}

/// Backpressure: a deep compile queue defers promotion (doubled threshold)
/// instead of enqueueing into a backlog guests would time out on.
#[test]
fn bg_backpressure_deep_queue_defers_promotion() {
    let mut cpu = JitCpu::open_x86_64();
    cpu.shared.bg_force.store(true, Ordering::Relaxed);
    let base = alloc_code(&mut cpu, &MOV_EAX_2A);
    // Simulate a saturated queue.
    cpu.shared.bg_queue_depth.store(600, Ordering::Relaxed);
    cpu.shared.cache.pin().insert(
        base,
        CacheEntry::Hot {
            visits: 99,
            thr: 100,
        },
    );
    cpu.write_rip(base).expect("rip");
    let enq_before = cpu.stats().profile.bg_enqueues;
    let (result, _) = cpu.step_one().expect("step");
    assert!(matches!(result, StepResult::Continue));
    assert_eq!(
        cpu.stats().profile.bg_enqueues,
        enq_before,
        "deep queue must skip enqueue"
    );
    assert!(
        matches!(
            cpu.shared.cache.pin().get(&base),
            Some(CacheEntry::Hot {
                visits: 0,
                thr: 200
            })
        ),
        "backpressure must re-arm a doubled threshold"
    );
    assert_eq!(cpu.stats().promo.deferred, 1);
    assert!(!cpu.has_ready_at(base));

    // Queue drained → the same block promotes normally again.
    cpu.shared.bg_queue_depth.store(0, Ordering::Relaxed);
    cpu.shared.cache.pin().insert(
        base,
        CacheEntry::Hot {
            visits: 199,
            thr: 200,
        },
    );
    cpu.write_rip(base).expect("rip");
    let _ = cpu.step_one().expect("step after drain");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cpu.has_ready_at(base) {
        assert!(
            Instant::now() < deadline,
            "worker never installed after drain"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}
