//! Cross-thread invalidation guards on chained edges (baked `invalidate_gen`).
//!
//! The doomretro wedge: a guest thread deep inside chained native execution
//! (FuncRef hops + self-loop backedges) never returns to the dispatcher, so
//! a foreign thread's `invalidate_code_range` went unnoticed indefinitely.
//! Every compiled block now bakes the shared generation at compile time;
//! chain hops, self-loop backedges, and the Rust-side `chain_tail` compare
//! the live generation against that bake and return to the dispatcher on a
//! mismatch, where the existing purge + full-rebuild logic takes over.
//!
//! These tests exercise guard EMISSION, which is gated on
//! `WIE_JIT_CHAIN` (guards ride chained edges and must not exist when
//! chaining is off) — like the chain-table tests, they are only meaningful
//! with chaining enabled.

use super::*;

/// Guest code region base for these tests (unique per test-file convention).
const INV_BASE: u64 = 0x1060_0000;

/// Chain A→B with a DIRECT href hop; invalidate B's range (SMC write) after
/// both blocks are compiled but before execution; run A. The hop guard must
/// fire (generation moved past A's bake), return to the dispatcher, and the
/// fresh bytes at B must execute — the stale-B fn pointer baked into A's
/// direct call must never run.
#[test]
fn inv_guard_hop_exits_to_dispatcher_after_target_invalidation() {
    let mut cpu = JitCpu::open_x86_64();
    cpu.virtual_alloc(
        INV_BASE,
        0x3000,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_EXECUTE_READWRITE,
    )
    .expect("alloc");
    // Put A and B on DIFFERENT pages: range invalidation works page-granular,
    // and a same-page drop would discard A too (making the test vacuous).
    let va_a = INV_BASE;
    let va_b = INV_BASE + 0x1000;
    let stop = INV_BASE + 0x2000;

    // A: jmp rel32 → va_b
    let rel_ab = va_b - (va_a + 5);
    let mut code_a: Vec<u8> = vec![0xe9];
    code_a.extend_from_slice(&rel_ab.to_le_bytes());
    cpu.mem_write(va_a, &code_a).expect("write A");

    // B(old): mov eax, OLD_MARK ; jmp rel32 → stop
    const OLD_MARK: u32 = 0x1111_1111;
    const NEW_MARK: u32 = 0x2222_2222;
    let rel_bs = stop - (va_b + 10);
    let code_b = |mark: u32| {
        let mut v: Vec<u8> = vec![0xb8];
        v.extend_from_slice(&mark.to_le_bytes());
        v.push(0xe9);
        v.extend_from_slice(&rel_bs.to_le_bytes());
        v
    };
    cpu.mem_write(va_b, &code_b(OLD_MARK)).expect("write B old");

    // Harness stop marker.
    cpu.mem_write(stop, &[0x0f, 0x0b]).expect("write ud2");

    // Compile B first, then A — A's chain-map snapshot then contains B's
    // FuncId, so A emits a DIRECT host call to B (the exact edge that used
    // to bypass every runtime staleness check).
    cpu.precompile_at(va_b);
    cpu.precompile_at(va_a);
    assert!(cpu.has_ready_at(va_a) && cpu.has_ready_at(va_b));

    // Overwrite B's bytes (fresh program) — the SMC drain invalidates B's
    // page and bumps `invalidate_gen`, while A survives on its own page.
    cpu.mem_write(va_b, &code_b(NEW_MARK)).expect("write B new");
    assert!(
        !cpu.has_ready_at(va_b),
        "invalidation must drop B from the shared cache"
    );
    assert!(
        cpu.has_ready_at(va_a),
        "A sits on another page and must stay Ready (stale-prone setup)"
    );

    // Run from A. Without the guard, A's direct call executes stale B
    // (eax == OLD_MARK). With it, the hop exits to the dispatcher, which
    // purges, re-decodes the fresh bytes, and runs them (eax == NEW_MARK).
    cpu.write_rip(va_a).expect("rip");
    let mut final_rip = va_a;
    for _ in 0..64 {
        final_rip = cpu.read_rip().expect("rip read");
        if final_rip >= stop {
            break;
        }
        match cpu.step_one() {
            Ok((StepResult::Continue, _)) => {}
            _ => break,
        }
    }

    assert_eq!(final_rip, stop, "execution must reach the harness stop");
    assert_eq!(
        cpu.thread.regs.gpr(0),
        u64::from(NEW_MARK),
        "fresh B bytes must run; OLD_MARK means stale-B executed"
    );
    assert_eq!(
        cpu.stats().exec.iced_insns,
        0,
        "every block here must run through the JIT, not the interpreter"
    );
    assert!(cpu.stats().exec.code_invs >= 1);
}

/// Self-loop invalidated MID-execution by a second engine sharing the same
/// `JitShared`. The runner spins indefinitely on the old backedge unless the
/// emitted backedge guard observes the generation bump and returns to its
/// dispatcher, which re-decodes the replaced bytes and terminates. Bounded by
/// the channel receive timeout: an unfired guard strands the runner inside
/// one native frame and fails the test.
#[test]
fn inv_guard_self_loop_exits_on_foreign_invalidation() {
    let shared = Arc::new(JitShared::new());
    // Invalidator runs on THIS thread (created here, stays here).
    let mut a = JitCpu::new_shared(Arc::clone(&shared));

    let base = 0x1070_0000_u64;
    a.virtual_alloc(
        base,
        0x2000,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_EXECUTE_READWRITE,
    )
    .expect("alloc");
    let loop_va = base;

    // Old program: inc eax ; cmp eax, 0 ; jne loop_va — a genuine infinite
    // self-loop (eax only reaches 0 again after a full 2^32 wrap).
    const LOOP_LEN: u64 = 7;
    let old_code: [u8; 7] = [0xff, 0xc0, 0x83, 0xf8, 0x00, 0x75, 0xf9];
    a.mem_write(loop_va, &old_code).expect("write loop");

    // New program written by the invalidator: finite, distinct marker.
    // mov eax, 0x00c0ffee ; nop filler ; ud2 stops linear decode.
    // `decode_pure_gpr_block` EXCLUDES the non-lowerable ud2, so the fresh
    // block covers [loop_va, loop_va + 6) and exits with RIP at the ud2;
    // the ud2 itself then executes as a degrade-not-die partial no-op
    // (Wave 4: RIP advances, no state change), so the runner lands PAST it.
    const DONE_MARK: u64 = 0x00c0_ffee;
    const UD2_OFFSET: u64 = 6;
    let new_code: [u8; 7] = [0xb8, 0xee, 0xff, 0xc0, 0x00, 0x90, 0x0f];

    // Runner engine must be CREATED on its own host thread (PerThreadJitState
    // is 1:1 with threads and must not migrate).
    let (tx, rx) = std::sync::mpsc::channel::<(u64, u64)>();
    let runner_shared = Arc::clone(&shared);
    let runner = std::thread::spawn(move || {
        let mut r = JitCpu::new_shared(runner_shared);
        r.write_rip(loop_va).expect("runner rip");
        let mut last_rip = loop_va;
        for _ in 0..50_000_000 {
            match r.step_one() {
                Ok((StepResult::Continue, _)) => {
                    last_rip = r.read_rip().expect("runner rip read");
                    if last_rip >= loop_va.saturating_add(LOOP_LEN) {
                        break;
                    }
                }
                _ => break,
            }
        }
        let rax = r.thread.regs.gpr(0);
        tx.send((last_rip, rax)).expect("runner tx");
    });

    // Wait until the shared cache holds the compiled self-loop — the runner
    // has then either entered the native backedge or is one scheduler tick
    // away from it — then give it a moment to actually be spinning inside
    // the frame before dropping the code out from under it.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let ready = matches!(shared.cache.pin().get(&loop_va), Some(CacheEntry::Ready(_)));
        if ready || Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        matches!(shared.cache.pin().get(&loop_va), Some(CacheEntry::Ready(_))),
        "runner must have compiled the self-loop"
    );
    std::thread::sleep(Duration::from_millis(100));

    // Foreign invalidation: replaces the guest bytes AND bumps the shared
    // generation via the SMC drain.
    let mut full_new: Vec<u8> = new_code.to_vec();
    full_new.push(0x0b); // completes ud2 (decode-stopper)
    a.mem_write(loop_va, &full_new).expect("foreign smc write");
    assert!(
        !matches!(shared.cache.pin().get(&loop_va), Some(CacheEntry::Ready(_))),
        "invalidation must drop the self-loop entry"
    );
    assert_ne!(
        shared.invalidate_gen.load(Ordering::Relaxed),
        0,
        "foreign invalidation must bump the shared generation"
    );

    // Bounded join: an unfired backedge guard leaves the runner trapped in
    // one native frame forever and this receive times out.
    let outcome = rx.recv_timeout(Duration::from_secs(30));
    assert!(
        outcome.is_ok(),
        "self-loop must exit within bounded time after foreign invalidation"
    );
    let (rip, rax) = outcome.expect("checked above");
    runner.join().expect("runner thread");
    assert_eq!(
        rax, DONE_MARK,
        "post-invalidation bytes must produce their own marker, not loop state"
    );
    assert!(
        rip >= loop_va.saturating_add(UD2_OFFSET),
        "runner must have exited the loop head into the fresh block \
         (degrade-not-die steps past the ud2; got rip {rip:#x})"
    );
}
