use super::*;

// --- Cross-thread stale-chain repair (shared invalidate_gen) ---

/// A hard invalidation must survive the dispatch loop: the `u64::MAX`
/// sentinel is now consumed by `resync_chain_table` itself, so the first
/// dispatch after `invalidate_chain_and_shadow` takes the FULL-rebuild branch
/// and re-links surviving Ready blocks. Regression for the caller that
/// pre-stored the epoch over the sentinel, making that branch unreachable
/// from dispatch.
#[test]
fn dispatch_after_hard_invalidation_full_rebuilds_chain_table() {
    let mut cpu = JitCpu::open_x86_64();
    let base = alloc_code_page(&mut cpu);
    cpu.test_plant_ready(base, base + 8);

    // Hard invalidation: table cleared, `chain_sync_epoch = u64::MAX`.
    cpu.invalidate_chain_and_shadow();
    assert!(
        !cpu.thread
            .chain_slots
            .iter()
            .any(|s| s.va == base && s.fn_ptr != 0),
        "hard invalidation clears the chain table"
    );

    // One dispatch: the prologue must see the sentinel and full-rebuild.
    cpu.run_until_stop(base, 0, 0, 2, 0, 0).expect("dispatch");
    assert_eq!(
        cpu.chain_sync_epoch,
        cpu.shared.cache_epoch.load(Ordering::Relaxed),
        "sentinel consumed; epoch recorded by resync itself"
    );
    assert!(
        cpu.thread
            .chain_slots
            .iter()
            .any(|s| s.va == base && s.fn_ptr != 0),
        "full rebuild re-linked the surviving Ready block"
    );
    assert!(
        cpu.stats().chain.resyncs >= 1 && cpu.stats().chain.resync_entries >= 1,
        "the rebuild pass must be counted"
    );
}

/// Foreign invalidation: thread B's chain table holds thread A's block; A
/// drops it from the SHARED cache via a range invalidation. B must observe
/// the shared `invalidate_gen` bump, arm the full-rebuild sentinel, and
/// unlink the dropped VA instead of chaining into the stale fn_ptr forever.
#[test]
fn foreign_invalidation_forces_full_rebuild_and_unlinks_dropped_va() {
    let shared = Arc::new(JitShared::new());
    let mut a = JitCpu::new_shared(Arc::clone(&shared));
    let mut b = JitCpu::new_shared(Arc::clone(&shared));

    // "Thread A" maps code and installs a Ready block into the shared cache.
    let va_a = alloc_code_page(&mut a);
    a.test_plant_ready(va_a, va_a + 8);

    // "Thread B" maps its own region and syncs once from watermark 0: the
    // delta pass links BOTH blocks — including A's — which is exactly the
    // state that goes stale when A invalidates without B knowing.
    let va_b = alloc_code_page(&mut b);
    b.test_plant_ready(va_b, va_b + 8);
    b.resync_chain_table(b.shared.cache_epoch.load(Ordering::Relaxed));
    assert!(
        b.thread
            .chain_slots
            .iter()
            .any(|s| s.va == va_a && s.fn_ptr != 0),
        "B chained A's block (stale-prone setup)"
    );

    // A invalidates its range: the shared Ready entry is dropped and the
    // shared generation bumped; only A's own local state is repaired here.
    a.invalidate_code_range(va_a, 8);
    assert!(!a.has_ready_at(va_a));
    assert_ne!(
        shared.invalidate_gen.load(Ordering::Relaxed),
        0,
        "foreign invalidation must bump the shared generation"
    );

    // B's next dispatch detects the foreign generation: FULL rebuild (not
    // delta — recent_installs did not grow), so va_a leaves the table while
    // B's own block stays linked. Only a full rebuild can remove entries;
    // a delta pass would leave va_a chained to the dead fn_ptr.
    let entries_before = b.stats().chain.resync_entries;
    b.run_until_stop(va_b, 0, 0, 1, 0, 0)
        .expect("B second dispatch");
    assert_eq!(
        b.seen_invalidate_gen,
        shared.invalidate_gen.load(Ordering::Relaxed),
        "B observed the foreign invalidation generation"
    );
    assert!(
        !b.thread
            .chain_slots
            .iter()
            .any(|s| s.va == va_a && s.fn_ptr != 0),
        "dropped VA must be unlinked from B's chain table"
    );
    assert!(
        b.thread
            .chain_slots
            .iter()
            .any(|s| s.va == va_b && s.fn_ptr != 0),
        "B's own block stays linked"
    );
    assert_eq!(
        b.stats()
            .chain
            .resync_entries
            .saturating_sub(entries_before),
        1,
        "rebuild width is the post-drop cache (one block), not a delta of zero"
    );
}
