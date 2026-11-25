//! The "third category" harness: a live page reported `unmapped` while a peer
//! thread frees a **different** region. See `docs/architecture/cpu-and-memory.md`.
//!
//! **Time-bounded, not iteration-bounded.** An iteration count scales with the
//! churn that accumulates in the allocator, not with the thing being measured:
//! at 2_000 iterations this test reported in 12 ms, and at 10_000 and 200_000 it
//! produced nothing in 234-252 s. A fixed wall budget always terminates, always
//! prints, and yields a rate comparable across machines and revisions.
//!
//! Two phases, each with its own budget:
//!
//! * **assertion phase** — walks of `REGION_B`, mapped once and never freed;
//! * **control phase** — walks of the region the peer thread is freeing, which
//!   must refuse; a zero there means the sensor is dead and the assertion phase's
//!   zero proves nothing.
//!
//! Nothing in the writer is fatal and nothing here is asserted except that the
//! churn itself could allocate.

use crate::RwxPerms;
use crate::mem::GuestMemory;
use crate::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Where the churn region is *requested*.
const REGION_A_HINT: u64 = 0x2000_0000;
/// Region that is mapped once and must never be disturbed.
const REGION_B: u64 = 0x3000_0000;
const PAGE: usize = 0x1000;
/// `PAGE_READWRITE`, as `virtual_alloc` takes the raw Win32 protection word.
const PAGE_READWRITE: u32 = 0x04;
/// Wall budget per phase.
const BUDGET: Duration = Duration::from_secs(2);

/// Resolve one page through the lock-free reader path, exactly as a stub would.
fn resolves(mem: &GuestMemory, va: u64) -> bool {
    mem.page_data_ptr_walk(va >> 12).is_some() || mem.page_data_ptr(va >> 12).is_some()
}

fn note_first(slot: &Mutex<Option<String>>, msg: String) {
    if let Ok(mut g) = slot.lock()
        && g.is_none()
    {
        *g = Some(msg);
    }
}

#[test]
fn a_peer_free_of_another_region_never_makes_a_stable_page_unmapped() {
    let mem = Arc::new(Mutex::new(GuestMemory::new()));
    let base;
    {
        let mut m = mem.lock().expect("lock");
        m.map(REGION_B, PAGE, RwxPerms::ALL)
            .expect("map stable region");
        m.write(REGION_B, &[0xAA_u8]).expect("seed stable region");
        base = m
            .virtual_alloc(
                REGION_A_HINT,
                PAGE,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
            .expect("prime the churn region");
    }

    // The reader side's raw pointer, taken once and used without a lock: this is
    // what `run_compiled` hands the stubs. A read guard instead would let the
    // writer serialise the two and the test would pass vacuously.
    let raw: *const GuestMemory = {
        let guard = mem.lock().expect("lock");
        std::ptr::from_ref(&*guard)
    };

    let stop = Arc::new(AtomicBool::new(false));
    let alloc_failures = Arc::new(AtomicU64::new(0));
    let free_failures = Arc::new(AtomicU64::new(0));
    let first_failure = Arc::new(Mutex::new(None::<String>));
    let first_loss = Arc::new(Mutex::new(None::<String>));
    // Control-phase refusals are *not* losses: keeping them in the same slot
    // printed `first_loss=A …` next to `losses=0`, which reads as a contradiction.
    let first_refusal = Arc::new(Mutex::new(None::<String>));
    let live_base = Arc::new(AtomicU64::new(base));
    let churn_cycles = Arc::new(AtomicU64::new(0));

    // --- assertion phase: walks of never-freed REGION_B --------------------
    let mut walks_b = 0_u64;
    let mut losses = 0_u64;
    let began = Instant::now();
    {
        let (w_stop, w_mem) = (Arc::clone(&stop), Arc::clone(&mem));
        let (w_af, w_ff, w_first) = (
            Arc::clone(&alloc_failures),
            Arc::clone(&free_failures),
            Arc::clone(&first_failure),
        );
        let (w_base, w_cycles) = (Arc::clone(&live_base), Arc::clone(&churn_cycles));
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let mut cur = w_base.load(Ordering::Relaxed);
                while !w_stop.load(Ordering::Relaxed) {
                    let Ok(mut m) = w_mem.lock() else { break };
                    if let Err(e) = m.virtual_free(cur, 0, MEM_RELEASE) {
                        w_ff.fetch_add(1, Ordering::Relaxed);
                        note_first(&w_first, format!("free {cur:#x}: {e}"));
                    }
                    match m.virtual_alloc(
                        REGION_A_HINT,
                        PAGE,
                        MEM_COMMIT | MEM_RESERVE,
                        PAGE_READWRITE,
                    ) {
                        Ok(p) => {
                            cur = p;
                            w_base.store(p, Ordering::Relaxed);
                        }
                        Err(e) => {
                            w_af.fetch_add(1, Ordering::Relaxed);
                            note_first(&w_first, format!("alloc: {e}"));
                        }
                    }
                    w_cycles.fetch_add(1, Ordering::Relaxed);
                }
            });
            // SAFETY: the reader only calls `&self` methods; the writer's `&mut`
            // mutation is the very race this test measures, and it is what the
            // production stubs do with `ctx.mem`.
            #[expect(unsafe_code)]
            let reader = unsafe { &*raw };
            let deadline = Instant::now() + BUDGET;
            while Instant::now() < deadline {
                let i = walks_b;
                walks_b += 1;
                if !resolves(reader, REGION_B) {
                    losses += 1;
                    note_first(&first_loss, format!("B walk_idx={i} addr={REGION_B:#x}"));
                }
            }
            stop.store(true, Ordering::Relaxed);
        });
    }
    let wall_b = began.elapsed();

    // --- control phase: walks of the region being freed --------------------
    let mut walks_a = 0_u64;
    let mut refusals = 0_u64;
    let began_a = Instant::now();
    {
        stop.store(false, Ordering::Relaxed);
        let (w_stop, w_mem) = (Arc::clone(&stop), Arc::clone(&mem));
        let (w_af, w_ff, w_first) = (
            Arc::clone(&alloc_failures),
            Arc::clone(&free_failures),
            Arc::clone(&first_failure),
        );
        let (w_base, w_cycles) = (Arc::clone(&live_base), Arc::clone(&churn_cycles));
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let mut cur = w_base.load(Ordering::Relaxed);
                while !w_stop.load(Ordering::Relaxed) {
                    let Ok(mut m) = w_mem.lock() else { break };
                    if let Err(e) = m.virtual_free(cur, 0, MEM_RELEASE) {
                        w_ff.fetch_add(1, Ordering::Relaxed);
                        note_first(&w_first, format!("free {cur:#x}: {e}"));
                    }
                    match m.virtual_alloc(
                        REGION_A_HINT,
                        PAGE,
                        MEM_COMMIT | MEM_RESERVE,
                        PAGE_READWRITE,
                    ) {
                        Ok(p) => {
                            cur = p;
                            w_base.store(p, Ordering::Relaxed);
                        }
                        Err(e) => {
                            w_af.fetch_add(1, Ordering::Relaxed);
                            note_first(&w_first, format!("alloc: {e}"));
                        }
                    }
                    w_cycles.fetch_add(1, Ordering::Relaxed);
                }
            });
            // SAFETY: as above.
            #[expect(unsafe_code)]
            let reader = unsafe { &*raw };
            let deadline = Instant::now() + BUDGET;
            while Instant::now() < deadline {
                let i = walks_a;
                walks_a += 1;
                let target = live_base.load(Ordering::Relaxed);
                if !resolves(reader, target) {
                    refusals += 1;
                    note_first(&first_refusal, format!("A walk_idx={i} addr={target:#x}"));
                }
            }
            stop.store(true, Ordering::Relaxed);
        });
    }
    let wall_a = began_a.elapsed();

    let af = alloc_failures.load(Ordering::Relaxed);
    let ff = free_failures.load(Ordering::Relaxed);
    let cycles = churn_cycles.load(Ordering::Relaxed);
    let first = first_failure
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_else(|| "none".to_string());
    let fl = first_loss
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_else(|| "none".to_string());
    let fr = first_refusal
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_else(|| "none".to_string());
    let rate = |n: u64, w: Duration| {
        let s = w.as_secs_f64();
        if s > 0.0 { (n as f64 / s) as u64 } else { 0 }
    };
    eprintln!(
        "[harness] budget_ms={} wall_b_ms={} walks_b={walks_b} walks_per_s_b={} losses={losses} \
         losses_per_s={} wall_a_ms={} walks_a={walks_a} walks_per_s_a={} refusals={refusals} \
         refusals_per_s={} churn_cycles={cycles} alloc_failures={af} free_failures={ff} \
         first_loss={fl} first_refusal={fr} first_failure={first}",
        BUDGET.as_millis(),
        wall_b.as_millis(),
        rate(walks_b, wall_b),
        rate(losses, wall_b),
        wall_a.as_millis(),
        rate(walks_a, wall_a),
        rate(refusals, wall_a),
    );

    // The churn must be sound, or every number above is meaningless.
    assert_eq!(af, 0, "the churn itself could not allocate: {first}");
}
