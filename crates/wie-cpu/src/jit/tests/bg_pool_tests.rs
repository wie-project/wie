//! Background worker-pool contracts: multi-producer no-loss stress, lane
//! ordering, and depth-cap backpressure.
//!
//! The pool replaced the single background worker (bounded channel + one
//! thread) with K condvar-driven workers over a shared two-lane queue
//! ([`BgPool`]): urgent jobs — guest threads that block (or are about to
//! block) on the compiled block — pop before normal jobs (speculative
//! prefetches). These tests pin the three contracts that make the pool safe:
//!
//! 1. every successfully enqueued job eventually resolves (stress),
//! 2. the urgent-before-normal pop discipline holds,
//! 3. the [`BG_QUEUE_CAP`] depth bound rejects instead of stranding.

use super::*;
use config::BG_QUEUE_CAP;

const POOL_CAP_BASE: u64 = 0x1280_0000;

/// Urgent-before-normal ordering: a job enqueued URGENT is popped before any
/// NORMAL job already in the queue at enqueue time (and before later arrivals
/// in either lane); each lane keeps FIFO order. This exercises the exact
/// `pop_job` discipline the workers use.
#[test]
fn bg_pool_pops_urgent_lane_before_normal_fifo() {
    let pool = BgPool::new();
    // Three normals arrive first, then an urgent, another normal, a second
    // urgent. Pop order interleaves lanes by priority, not by arrival.
    for rip in [0xa1_u64, 0xa2, 0xa3] {
        assert!(pool.push(rip, BlockKind::NotPure, 0, false), "normal push");
    }
    assert!(pool.push(0xb1, BlockKind::NotPure, 0, true), "urgent push");
    assert!(pool.push(0xa4, BlockKind::NotPure, 0, false), "late normal");
    assert!(
        pool.push(0xb2, BlockKind::NotPure, 0, true),
        "second urgent"
    );

    let mut q = pool.q.lock().unwrap();
    let mut popped: Vec<u64> = Vec::new();
    while let Some(job) = q.pop_job() {
        popped.push(job.rip);
    }
    drop(q);
    // Both urgents (FIFO) jump ahead of every normal that was queued when
    // they arrived; normals keep their arrival order behind them.
    assert_eq!(popped, vec![0xb1, 0xb2, 0xa1, 0xa2, 0xa3, 0xa4]);
}

/// Depth-cap backpressure: [`BG_QUEUE_CAP`] combined jobs across BOTH lanes
/// are accepted; further pushes from either lane are rejected (producers fall
/// back to inline compilation) until a worker drains below the cap.
#[test]
fn bg_pool_rejects_pushes_at_capacity_from_either_lane() {
    let pool = BgPool::new();
    let cap = u64::try_from(BG_QUEUE_CAP).unwrap_or(u64::MAX);
    for i in 0..cap {
        // Alternate lanes so the cap is proven against the combined depth,
        // not one lane alone.
        let urgent = i % 2 == 0;
        assert!(
            pool.push(POOL_CAP_BASE + i, BlockKind::NotPure, 0, urgent),
            "push {i} below cap must be accepted"
        );
    }
    // Full: both lanes reject.
    assert!(!pool.push(1, BlockKind::NotPure, 0, true));
    assert!(!pool.push(2, BlockKind::NotPure, 0, false));
    // Drain exactly one job; capacity frees for exactly one push.
    {
        let mut q = pool.q.lock().unwrap();
        assert!(q.pop_job().is_some(), "drain must free one slot");
    }
    assert!(pool.push(POOL_CAP_BASE, BlockKind::NotPure, 0, true));
    assert!(!pool.push(POOL_CAP_BASE + 1, BlockKind::NotPure, 0, false));
}

/// Stress slot layout: unique VA per synthetic block, 16-byte stride.
fn stress_slot_va(idx: usize) -> u64 {
    const STRESS_BASE: u64 = 0x1300_0000;
    const STRIDE: u64 = 16;
    STRESS_BASE + STRIDE * u64::try_from(idx).unwrap_or(u64::MAX)
}

/// M producer threads enqueue N distinct synthetic blocks concurrently while
/// the worker pool runs; every job must resolve to a Ready install (no loss,
/// no strand), every job must be processed by a worker, and the queue depth
/// must return to zero. Teardown (dropping the engines and the shared state
/// with live workers) happens implicitly at scope exit and must complete
/// cleanly — this test hanging means teardown is broken.
#[test]
fn bg_pool_multi_producer_stress_all_blocks_resolve() {
    let shared = Arc::new(JitShared::new());
    shared.bg_force.store(true, Ordering::Relaxed);
    // Warm the pool BEFORE producers start so no enqueue races the spawn
    // window (a caller observing `bg_alive == false` mid-warmup legitimately
    // falls back to inline compilation — that path is not under test here).
    shared.ensure_bg_worker();
    assert!(shared.bg_alive.load(Ordering::Relaxed), "pool must be up");

    // Map one region and write distinct blocks: `mov eax, imm32` carrying a
    // unique mark per slot, `nop`, then `ud2` to stop linear decode (zeroed
    // pages would decode as a full 96-insn budget of `add [rax],al`).
    const PRODUCERS: usize = 4;
    const PER_PRODUCER: usize = 24;
    let total = PRODUCERS * PER_PRODUCER;
    {
        let mut setup = JitCpu::new_shared(Arc::clone(&shared));
        setup
            .virtual_alloc(
                stress_slot_va(0),
                0x4000,
                MEM_RESERVE | MEM_COMMIT,
                protect::PAGE_EXECUTE_READWRITE,
            )
            .expect("alloc");
        for idx in 0..total {
            let mark = u32::try_from(idx).unwrap_or(1) | 0x0100_0000;
            let mut code: Vec<u8> = vec![0xb8];
            code.extend_from_slice(&mark.to_le_bytes());
            code.extend_from_slice(&[0x90, 0x0f, 0x0b]);
            setup
                .mem_write(stress_slot_va(idx), &code)
                .expect("write slot");
        }
    }

    // Producers each own an engine (`PerThreadJitState` is 1:1 with host
    // threads) and fire speculative prefetches — the NORMAL lane, exactly
    // like session-init prewarming.
    let producers: Vec<_> = (0..PRODUCERS)
        .map(|p| {
            let sh = Arc::clone(&shared);
            std::thread::spawn(move || {
                let mut cpu = JitCpu::new_shared(sh);
                for j in 0..PER_PRODUCER {
                    let idx = p * PER_PRODUCER + j;
                    cpu.precompile_deferred_at(stress_slot_va(idx));
                }
            })
        })
        .collect();
    for p in producers {
        p.join().expect("producer thread");
    }

    // Every enqueued rip must leave the Queued state (Ready or Never); these
    // blocks compile cleanly, so Ready is the only acceptable end state. The
    // queue never fills here (96 << BG_QUEUE_CAP) and no producer can inline-
    // fallback, so all `total` jobs deterministically pass through workers.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let unresolved = (0..total)
            .filter(|&idx| {
                matches!(
                    shared.cache.pin().get(&stress_slot_va(idx)),
                    None | Some(CacheEntry::Queued(_))
                )
            })
            .count();
        if unresolved == 0 || Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    for idx in 0..total {
        assert!(
            matches!(
                shared.cache.pin().get(&stress_slot_va(idx)),
                Some(CacheEntry::Ready(_))
            ),
            "slot {idx} must resolve Ready (job lost or still queued)"
        );
    }
    // Workers processed everything handed to them, and the queue drained.
    let total_jobs = u64::try_from(total).unwrap_or(u64::MAX);
    assert_eq!(
        shared.bg_compile.compile_count.load(Ordering::Relaxed),
        total_jobs,
        "worker pool must have processed every enqueued job"
    );
    assert_eq!(
        shared.bg_queue_depth.load(Ordering::Relaxed),
        0,
        "queue depth must return to zero after all jobs resolve"
    );
    assert!(
        shared.bg_workers_spawned.load(Ordering::Relaxed) >= 1,
        "the forced background path must spawn the worker pool"
    );
}
