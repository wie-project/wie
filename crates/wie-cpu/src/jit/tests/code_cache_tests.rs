//! Persisted-machine-code round-trip tests (`WIE_JIT_CODE_CACHE`).
//!
//! The feature's whole value is "compile once, replay forever", and its whole
//! risk is "replay the wrong instructions". These tests pin both halves:
//!
//! - the **round trip**: a block compiled in one engine, persisted, dropped,
//!   re-defined in a *different* engine from the persisted bytes, and executed
//!   there must leave the guest in exactly the state the compiled block left it
//!   in. Assertions are on guest-visible state (`RegFile`), never on host
//!   addresses, which legitimately differ per process.
//! - the **refusals**: every guard must refuse and fall back to compiling rather
//!   than mapping or running anything unproven. Each test asserts the specific
//!   refusal counter moved.
//!
//! `WIE_JIT_CODE_CACHE` is inert under `cfg(test)` (see
//! `JitConfig::code_cache_enabled`) so the ambient cache cannot perturb the
//! suite; these tests inject a directory-scoped [`CodeCache`] through
//! `JitShared::set_code_cache` instead.

use super::harness::map_code_page;
use super::*;
use crate::jit::cache_persist::LedgerProbe;
use crate::jit::engine::{
    CodeCache, CodeCacheCounts, PersistedCode, SilentReason, classify_capture_relocs,
};
use cranelift_module::ModuleRelocTarget;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Arbitrary nonzero PE hash: the code cache is keyed by it and 0 means "not
/// attached".
const PE_HASH: u64 = 0xC0DE_0000_0000_0001;
const CODE_BASE: u64 = 0x3000_0000;

/// `mov eax, 0x2a; add eax, ebx; nop; ud2` — three real instructions, so the
/// block exercises arithmetic, falls through, and stops on the trailing `ud2`
/// (a pure fallthrough exit, so it has NO chain-table successor and therefore no
/// `FunctionOffset` relocation — see `code_cache`'s module docs on why that
/// class is not replayable).
const CODE: &[u8] = &[
    0xb8, 0x2a, 0x00, 0x00, 0x00, // mov eax, 42
    0x01, 0xd8, // add eax, ebx
    0x90, // nop
    0x0f, 0x0b, // ud2 — linear-decode terminator / harness stop
];
const STOP_RIP: u64 = CODE_BASE + 9;

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "wie-code-cache-test-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _cleaned: std::io::Result<()> = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// One engine with the code written, a code cache injected + attached, and the
/// background lane forced on (so an installed `Ready` entry is picked up by the
/// dispatcher rather than recompiled — the production default).
fn engine_with_cache(dir: &Path) -> (JitCpu, Arc<CodeCache>) {
    let cpu = new_cpu_with_code();
    let cache = Arc::new(CodeCache::enabled_in(
        dir.to_path_buf(),
        JitConfig::get().opt_level(),
    ));
    assert!(cache.enabled(), "injected cache must be enabled");
    cpu.shared.set_code_cache(Some(Arc::clone(&cache)));
    cpu.shared.attach_pe_cache(PE_HASH);
    (cpu, cache)
}

fn new_cpu_with_code() -> JitCpu {
    let mut cpu = JitCpu::open_x86_64();
    cpu.shared.bg_force.store(true, Ordering::Relaxed);
    map_code_page(&mut cpu, CODE_BASE);
    cpu.mem_write(CODE_BASE, CODE).expect("write code");
    cpu
}

/// Step to [`STOP_RIP`] and return the resulting register file.
///
/// Tolerates a step ending on the harness `ud2`, exactly as the rest of the
/// suite does: the terminator is the intended stop, not a failure.
fn run_to_stop(cpu: &mut JitCpu) -> RegFile {
    // Compile + persist the entry block on the guest thread **before** the
    // guest reaches it.
    //
    // The background lane is forced on in this file (production default), and
    // its installs land asynchronously. With the no-stall dispatch policy
    // (`JitCpu::step_one`) an interpreted step never waits for an install, so a
    // walk over these 10 bytes queues one block per instruction and several
    // installs are in flight at once — and the ledger row that survives the
    // flush is then whichever overlapping block landed first, not necessarily
    // `CODE_BASE`. That is install order, which none of these tests are about:
    // they pin the *round trip* (a compiled block persisted, replayed in a
    // different engine, producing the same guest state). Compiling the entry
    // block here pins the order without weakening a single assertion.
    cpu.precompile_at(CODE_BASE);
    cpu.write_rip(CODE_BASE).expect("rip");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let rip = cpu.read_rip().expect("read rip");
        if rip >= STOP_RIP {
            break;
        }
        let Ok((result, _retired)) = cpu.step_one() else {
            break;
        };
        if !matches!(result, StepResult::Continue) {
            break;
        }
    }
    cpu.thread.regs.clone()
}

/// Block the compiler worker so the install is settled before assertions.
fn await_ready(cpu: &JitCpu) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cpu.has_ready_at(CODE_BASE) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn ready_block(cpu: &JitCpu) -> CompiledBlock {
    let pin = cpu.shared.cache.pin();
    match pin.get(&CODE_BASE) {
        Some(CacheEntry::Ready(c)) => *c,
        _ => panic!("expected a Ready entry at CODE_BASE"),
    }
}

/// Every architectural GPR as a comparable array.
///
/// `RegFile` has no `PartialEq`, and asserting on a hand-picked subset would let
/// a replay that scribbled on an unrelated register through.
fn gprs(r: &RegFile) -> [u64; 16] {
    std::array::from_fn(|i| r.gpr(i))
}

fn counts(cache: &CodeCache) -> CodeCacheCounts {
    cache.counters().snapshot()
}

/// The same numbers as the test helper, read through [`JitShared`] rather than
/// through the cache handle — the path the profile dump will use.
fn counts_via_shared(cpu: &JitCpu) -> CodeCacheCounts {
    cpu.shared.code_cache_counters()
}

/// Drive the exact seam the dispatcher's warm-boot path uses. Deterministic,
/// unlike the visit-threshold / bg-worker race a full dispatch would race.
fn force_probe(cpu: &JitCpu) -> Option<LedgerProbe> {
    let mem = cpu.shared.mem.read().expect("mem lock");
    cpu.shared.restore_or_probe(&mem, CODE_BASE)
}

// ---------------------------------------------------------------------------
// Round trip
// ---------------------------------------------------------------------------

#[test]
fn persisted_code_is_replayed_in_a_fresh_engine_and_matches_the_compile() {
    let dir = scratch_dir("round-trip");

    // --- cold run: compile, capture, persist -----------------------------
    let (regs_compiled, cold, addr_cold, insn_count) = {
        let (mut cpu, cache) = engine_with_cache(&dir);
        let regs = run_to_stop(&mut cpu);
        await_ready(&cpu);
        let compiled = ready_block(&cpu);
        (
            regs,
            counts(&cache),
            compiled.func as usize,
            compiled.insn_count,
        )
    };
    assert_eq!(regs_compiled.rax(), 42, "sanity: the block did its work");
    assert!(cold.compiled >= 1, "a blob was persisted, got {cold:?}");
    assert_eq!(cold.restored, 0, "a cold run cannot restore");
    assert_eq!(
        cold.skipped_unpersistable, 0,
        "a chain-free block is persistable: {cold:?}"
    );
    // `cpu`/`cache` dropped at the end of the block, which flushes the file.

    // --- warm run: a *different* engine, the same bytes on disk ----------
    let (mut cpu, cache) = engine_with_cache(&dir);
    assert_eq!(
        counts(&cache).restored,
        0,
        "attach alone must not restore: restoration is lazy, per block"
    );

    let probe = force_probe(&cpu).expect("warm boot: the blob was found and replayed");
    let warm = counts(&cache);
    assert!(warm.restored >= 1, "a block was replayed, got {warm:?}");
    assert_eq!(warm.compiled, 0, "nothing was compiled: {warm:?}");
    assert_eq!(warm.refused(), 0, "no guard fired: {warm:?}");
    assert_eq!(warm.blocks_replayed, warm.restored, "got {warm:?}");
    assert_eq!(
        counts_via_shared(&cpu),
        warm,
        "the JitShared-facing snapshot must agree with the cache's own"
    );
    assert!(
        cpu.shared
            .code_cache_summary()
            .expect("enabled cache has a summary")
            .contains(&format!("restored={}", warm.restored)),
        "the summary must name the hit rate"
    );
    assert!(cache.test_len() >= 1, "a blob is in the table");
    assert_eq!(
        probe.insn_count, insn_count,
        "the replayed block is the same block, not a neighbour"
    );

    // The restored block must be a NEW function in THIS module, not the old
    // pointer: that is the whole difference between "replayed" and "leaked".
    let restored = ready_block(&cpu);
    assert!(
        restored.func_id.is_some(),
        "a replayed block must join the FuncId-keyed chain world"
    );
    assert_ne!(
        restored.func as usize, addr_cold,
        "the cold block's memory is gone; this is a re-definition"
    );

    let regs_replayed = run_to_stop(&mut cpu);
    assert_eq!(
        gprs(&regs_replayed),
        gprs(&regs_compiled),
        "the replayed block must leave every guest register as the compile did"
    );
}

/// The reference-class census, measured rather than assumed.
///
/// The whole safety argument for replaying machine code is that the emitted
/// code's address-bearing references are a *closed, enumerable* set — our own
/// host-helper imports, plus direct chain edges recorded as guest VAs. This
/// asserts the import half on a real captured block: every relocation resolves to
/// a named import of the live module. If a future lowering change introduced a
/// relocation this test cannot classify (a Cranelift libcall, a data object), it
/// fails here rather than in production on the second launch of a real guest. The
/// chain-edge half of the enumeration is pinned by
/// `captured_relocations_are_classified_total`.
#[test]
fn a_real_block_capture_references_only_named_host_imports() {
    let dir = scratch_dir("reloc-census");
    let (mut cpu, cache) = engine_with_cache(&dir);
    let _ = run_to_stop(&mut cpu);
    await_ready(&cpu);

    let blob = cache.test_blob(CODE_BASE).unwrap_or_else(|| {
        panic!(
            "no blob at CODE_BASE: vas={:#x?} attached_key={:#x?}",
            cache.test_vas(),
            cache.test_attached_key()
        )
    });
    assert!(
        !blob.relocs.is_empty(),
        "sanity: a block that loads guest memory must reference a host helper"
    );
    let mut census: Vec<(String, u32)> = Vec::new();
    for r in &blob.relocs {
        let ModuleRelocTarget::User {
            namespace: 0,
            index,
        } = r.name
        else {
            panic!("unexpected relocation target class: {:?}", r.name);
        };
        let name = cpu
            .shared
            .code_import_name(index)
            .unwrap_or_else(|| panic!("relocation names import #{index}, which is not declared"));
        census.push((format!("{:?}", r.kind), index));
        assert!(
            !name.is_empty(),
            "import #{index} must have a name; an anonymous target is not replayable"
        );
    }
    tracing::debug!("relocation census: {census:?}");
}

// ---------------------------------------------------------------------------
// Deterministic flush
// ---------------------------------------------------------------------------

/// The regression this whole section exists for.
///
/// The flush used to live only in `CodeCache::drop`, so it ran only when the
/// last `Arc<CodeCache>` went away. Measured over five runs each, that never
/// happened for `crt_hello` and `write_file` (and only 1-in-5 for
/// `shell_folders`): `Arc<JitShared>` was still at strong-count 1 inside
/// `RuntimeSession::drop`, so the warm boot stayed permanently cold for real
/// guests — including 7-Zip, the workload the lever exists for. Session teardown
/// now calls `JitShared::finish_jit_caches()`.
///
/// This test reproduces that exact shape: the cache is still referenced when
/// `finish()` is called, so nothing would flush it if the flush were still
/// drop-only.
#[test]
fn finish_flushes_while_the_cache_is_still_referenced() {
    let dir = scratch_dir("finish-while-alive");
    let (mut cpu, cache) = engine_with_cache(&dir);
    let _ = run_to_stop(&mut cpu);
    await_ready(&cpu);
    assert!(
        cache.counters().snapshot().compiled >= 1,
        "a blob was recorded"
    );

    // Every reference is still alive here — this is the shape that used to lose
    // the cache. `sole_code_file` asserts the file appears anyway.
    assert_eq!(
        std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "code"))
            .count(),
        0,
        "nothing may be on disk before the explicit flush"
    );

    // The exact call guest-session teardown makes.
    cpu.shared.finish_jit_caches();

    let path = sole_code_file(&dir);
    let bytes = std::fs::read(&path).expect("flush wrote the file");
    assert!(
        bytes.len() > 64,
        "the file has a payload, got {} bytes",
        bytes.len()
    );
    assert_eq!(cache.counters().snapshot().summaries_reported, 1);
}

#[test]
fn finish_is_idempotent_under_a_double_call() {
    let dir = scratch_dir("finish-twice");
    let (mut cpu, cache) = engine_with_cache(&dir);
    cpu.precompile_at(CODE_BASE);
    let _ = run_to_stop(&mut cpu);
    await_ready(&cpu);

    // Session teardown flushes, then the last Arc drops and the backstop fires.
    // Both run. The file must be written (not truncated), and the counters
    // reported exactly once.
    cpu.shared.finish_jit_caches();
    let first = std::fs::read(sole_code_file(&dir)).expect("first flush wrote");
    cpu.shared.finish_jit_caches();
    drop(cpu.shared.code_cache_for_test());
    let second = std::fs::read(sole_code_file(&dir)).expect("second flush kept the file");

    assert_eq!(
        first, second,
        "a redundant flush must not truncate or corrupt the file"
    );
    assert_eq!(
        cache.counters().snapshot().summaries_reported,
        1,
        "the counters must be reported exactly once"
    );

    // And the file is still loadable: a corrupt rewrite would show up as a
    // refusal on the next process, which is what `record_and_reload` covers.
    let (cpu2, cache2) = engine_with_cache(&dir);
    let probe = force_probe(&cpu2).expect("the twice-flushed file still restores");
    assert_eq!(probe.insn_count, 1 + 2, "sanity: same block as recorded");
    assert_eq!(cache2.counters().snapshot().restored, 1);
    assert_eq!(cache2.counters().snapshot().refused(), 0);
}

/// An invalidated blob must not come back through the flush the invalidation
/// itself triggers.
///
/// `record` inserts a blob into the table and *then* queues it in
/// `FlushState::pending`, so a flush racing those two steps can see it in the
/// table, in the queue, or in neither — which is why the queue exists at all.
/// `invalidate_range` drops it from the table and immediately flushes, and that
/// flush used to append every queued record whose VA was missing from its table
/// snapshot, writing the dead blob straight back to disk. The byte-hash guard
/// made that harmless (a later probe refuses it), but the file then carried a
/// block the engine had already invalidated — and the guard here is the file's
/// contents, not a probe.
///
/// `A` and `B` share one page, which does not matter: the code cache's
/// invalidation takes the byte range it is given, and only `A`'s is invalidated.
#[test]
fn an_invalidated_blob_is_not_rewritten_by_the_flush_it_triggers() {
    let dir = scratch_dir("no-resurrect");
    let (mut cpu, cache) = engine_with_chain(&dir);
    // `A` first, so neither block's chain edge is the direct form: this test is
    // about the flush, not about chaining.
    cpu.precompile_at(CHAIN_BASE);
    cpu.precompile_at(CHAIN_BASE + 0x08);
    assert_eq!(counts(&cache).compiled, 2, "both blocks queued");

    // Nothing is on disk yet — the two records are still pending, which is the
    // state the invalidation below has to cope with.
    assert_eq!(
        std::fs::read_dir(&dir)
            .expect("read cache dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "code"))
            .count(),
        0,
        "records queue until a flush"
    );

    // SMC over `A`'s bytes: drops its blob from the table, then flushes.
    cpu.invalidate_code_range(CHAIN_BASE, 5);

    // Read the file back through the real load path, so "the file still parses"
    // and "the blob is not on disk" are the same assertion: a resurrected record
    // shows up as a loaded entry, a corrupt file as an empty table.
    let (_cpu2, cache2) = engine_with_chain(&dir);
    let vas = cache2.test_vas();
    assert!(
        !vas.contains(&CHAIN_BASE),
        "an invalidated blob must not be rewritten by the flush it triggers, \
         loaded VAs: {vas:#x?}"
    );
    assert!(
        vas.contains(&(CHAIN_BASE + 0x08)),
        "the untouched blob must still be on disk, loaded VAs: {vas:#x?}"
    );
}

// ---------------------------------------------------------------------------
// Refusals — every one of these must fall back to compiling, never to garbage
// ---------------------------------------------------------------------------

#[test]
fn tampered_blob_is_refused_and_never_replayed() {
    let dir = scratch_dir("tamper");
    seed_one_blob(&dir);

    // Damage the persisted file the way a truncated write or a bit flip would:
    // flip a byte in the middle of the binary payload.
    let path = sole_code_file(&dir);
    let good = std::fs::read(&path).expect("read blob file");
    assert!(good.len() > 64, "sanity: the file has a payload");
    let mut tampered = good.clone();
    let at = tampered.len() / 2;
    tampered[at] ^= 0xff;
    std::fs::write(&path, tampered).expect("write tampered blob");

    let (mut cpu, cache) = engine_with_cache(&dir);
    let probe = force_probe(&cpu);
    let c = counts(&cache);
    assert_eq!(
        c.restored, 0,
        "a tampered blob must never be replayed, got {c:?}"
    );
    assert_eq!(c.blocks_replayed, 0, "got {c:?}");
    assert!(
        c.refused() > 0 || probe.is_none(),
        "expected a refusal, got {c:?} (probe={probe:?})"
    );
    // A refused blob leaves the block cold: the normal compile path still runs.
    let _ = run_to_stop(&mut cpu);
    await_ready(&cpu);
    assert!(
        cpu.has_ready_at(CODE_BASE),
        "a refused blob must fall back to compiling"
    );
}

#[test]
fn blob_is_refused_when_the_guest_bytes_change() {
    let dir = scratch_dir("smc");
    seed_one_blob(&dir);

    let (mut cpu, cache) = engine_with_cache(&dir);
    // SMC-style divergence: the recorded byte hash no longer describes guest
    // memory, so the code would be replaying semantics for different bytes.
    let mut changed = CODE.to_vec();
    changed[1] = 0x2b;
    cpu.mem_write(CODE_BASE, &changed)
        .expect("rewrite guest bytes");

    assert!(force_probe(&cpu).is_none(), "stale bytes must not replay");
    let c = counts(&cache);
    assert_eq!(c.refused_bytes_changed, 1, "got {c:?}");
    assert_eq!(c.restored, 0);
}

#[test]
fn blob_from_a_different_pe_is_not_reused() {
    let dir = scratch_dir("other-pe");
    seed_one_blob(&dir);

    let cpu = new_cpu_with_code();
    let cache = Arc::new(CodeCache::enabled_in(
        dir.to_path_buf(),
        JitConfig::get().opt_level(),
    ));
    cpu.shared.set_code_cache(Some(Arc::clone(&cache)));
    // Same guest VA, same bytes, different PE: the file key differs, so this
    // process opens an empty one and must not inherit the blob.
    cpu.shared.attach_pe_cache(PE_HASH ^ 0xFFFF_FFFF);

    assert!(
        force_probe(&cpu).is_none(),
        "a different PE must not inherit the blob"
    );
    assert_eq!(cache.counters().snapshot().restored, 0);
}

// ---------------------------------------------------------------------------
// No generation literal in the emitted code
// ---------------------------------------------------------------------------

/// Guest region for the chain fixture (unique per test-file convention).
const CHAIN_BASE: u64 = 0x10c0_0000;
/// Iterations of the two-block ring.
const CHAIN_ITERS: u64 = 64;

/// Two distinct blocks joined by an unmerged `jmp`, so each iteration crosses
/// two chain edges:
///
/// ```text
/// A (+0x00): add rax,rbx ; jmp B            <- rbx is a read-only live-in
/// B (+0x08): dec rcx ; jnz A ; nop
/// stop(+0x0e): ud2
/// ```
///
/// `jmp`'s target is deliberately not A's fallthrough (the three padding nops):
/// the decoder folds a jump-to-next into a fallthrough and would merge A and B
/// into one block, making `hops` vacuous.
const CHAIN_CODE: [u8; 16] = [
    0x48, 0x01, 0xd8, // +0x00 add rax,rbx
    0xeb, 0x03, // +0x03 jmp +0x08
    0x90, 0x90, 0x90, // +0x05 padding (must NOT be the jmp target)
    0x48, 0xff, 0xc9, // +0x08 dec rcx
    0x75, 0xf3, // +0x0b jnz +0x00
    0x90, // +0x0d nop
    0x0f, 0x0b, // +0x0e ud2 (loop-exit stop)
];
const CHAIN_STOP: u64 = CHAIN_BASE + 0x0e;

/// An engine with the chain fixture mapped, a code cache injected + attached,
/// and `rcx`/`rbx` seeded so the ring terminates with a known `rax`.
fn engine_with_chain(dir: &Path) -> (JitCpu, Arc<CodeCache>) {
    let (mut cpu, cache) = engine_with_cache(dir);
    map_code_page(&mut cpu, CHAIN_BASE);
    cpu.mem_write(CHAIN_BASE, &CHAIN_CODE).expect("write chain");
    cpu.thread.regs.set_gpr(1, CHAIN_ITERS); // rcx
    cpu.thread.regs.set_gpr(3, 1); // rbx
    (cpu, cache)
}

/// Persist one chain block's blob into `dir`, recorded under `inv_gen`.
///
/// **One block per engine, deliberately.** A block compiled while its successor
/// is already in `chain_ids` emits a direct `FunctionOffset` host call, which
/// this format refuses to persist (module docs on the relocation census). Two
/// engines means neither block is `Ready` when the other compiles, so both edges
/// go through the named-import lookup helper — which is replayable, and which is
/// what makes the restored blocks able to chain at all.
///
/// Each engine reopens the file the previous one wrote, and a flush rewrites the
/// whole table, so successive calls accumulate into one corpus file.
fn seed_chain_blob(dir: &Path, inv_gen: u64, va: u64) {
    let (mut cpu, cache) = engine_with_chain(dir);
    cpu.shared.invalidate_gen.store(inv_gen, Ordering::Release);
    cpu.precompile_at(va);
    assert!(cpu.has_ready_at(va), "seed block {va:#x} must compile");
    let c = counts(&cache);
    assert_eq!(c.compiled, 1, "one blob recorded: {c:?}");
    assert_eq!(
        c.skipped_unpersistable, 0,
        "the chain edge must stay a lookup call, not a direct one: {c:?}"
    );
    let blob = cache
        .test_blob(va)
        .unwrap_or_else(|| panic!("no blob at {va:#x}"));
    assert_eq!(
        blob.inv_gen, inv_gen,
        "the recorded generation is a diagnostic, not a gate"
    );
    assert!(
        blob.relocs
            .iter()
            .all(|r| matches!(r.name, ModuleRelocTarget::User { namespace: 0, .. })),
        "every relocation must target a host import ({} relocs)",
        blob.relocs.len()
    );
    cpu.shared.finish_jit_caches();
    assert!(sole_code_file(dir).exists(), "blob file written");
    drop((cpu, cache));
}

/// Persist both blocks of the chain under the same generation.
fn seed_chain(dir: &Path, inv_gen: u64) {
    seed_chain_blob(dir, inv_gen, CHAIN_BASE);
    seed_chain_blob(dir, inv_gen, CHAIN_BASE + 0x08);
}

/// Restore both chain blocks into a fresh engine and link them into this
/// thread's chain table, mirroring what one dispatch prologue does.
fn restore_chain(cpu: &mut JitCpu) -> CodeCacheCounts {
    for va in [CHAIN_BASE, CHAIN_BASE + 0x08] {
        let mem = cpu.shared.mem.read().expect("mem lock");
        assert!(
            cpu.shared.restore_or_probe(&mem, va).is_some(),
            "blob at {va:#x} must restore; counts={:?}",
            counts_via_shared(cpu)
        );
    }
    cpu.resync_chain_table(cpu.shared.cache_epoch.load(Ordering::Relaxed));
    counts_via_shared(cpu)
}

/// Run the restored ring to its `ud2` and assert the guest-visible result.
///
/// `A` is the only `add`, and `rbx == 1`, so a correct run leaves
/// `rax == CHAIN_ITERS` and `rcx == 0`.
fn run_chain(cpu: &mut JitCpu) {
    cpu.run_until_stop(CHAIN_BASE, CHAIN_STOP, 0, 100_000, 0, 0)
        .expect("run restored chain");
    assert_eq!(
        cpu.thread.regs.rip, CHAIN_STOP,
        "execution must reach the harness stop"
    );
    assert_eq!(cpu.thread.regs.gpr(1), 0, "rcx must count down to zero");
    assert_eq!(
        cpu.thread.regs.gpr(0),
        CHAIN_ITERS,
        "chained edges must carry register state (A adds rbx=1 per iteration)"
    );
}

/// The test that replaces the deleted generation gate, and the one that fails
/// if anyone re-introduces a baked `iconst` for the invalidation guard.
///
/// A blob recorded under generation 7 is restored by a process sitting at
/// generation 0 — the exact mismatch the old gate refused — and then **chains**.
/// Restoring is not the point; the `hops > 0` is. A blob whose emitted guard
/// still carried generation 7 as an immediate would restore happily and then
/// bail to the dispatcher at every single edge, so `hops` would be 0 and the
/// guard would have silently become the chainability filter it was replaced by.
#[test]
fn a_blob_recorded_under_another_generation_restores_and_chains() {
    let dir = scratch_dir("crossgen");
    seed_chain(&dir, 7);

    let (mut cpu, _cache) = engine_with_chain(&dir);
    assert_eq!(
        cpu.shared.invalidate_gen.load(Ordering::Acquire),
        0,
        "a fresh process starts at generation 0, so the blob's 7 is stale"
    );

    let c = restore_chain(&mut cpu);
    assert_eq!(c.restored, 2, "both blobs must restore: {c:?}");
    assert_eq!(
        c.refused(),
        0,
        "a generation mismatch is no longer a refusal: {c:?}"
    );
    assert_eq!(c.compiled, 0, "nothing may fall back to compiling: {c:?}");

    run_chain(&mut cpu);
    assert!(
        cpu.stats().chain.hops > 0,
        "a restored blob recorded under another generation must still chain; \
         hops == 0 means a per-block generation literal is baked into the code"
    );
}

/// One corpus file holding blobs written under two different generations, both
/// restored by a third process at yet another generation.
///
/// This is the shape real warm boots produce, and the shape the removed gate
/// turned into a coin flip: before the change this is where `restored` and
/// `refused(gen=…)` diverged run to run, because which blobs were admitted
/// depended on whether the capturing process's counter happened to match. Here
/// both must land, both must chain, and no refusal may be reported for
/// generation at all.
#[test]
fn a_corpus_of_mixed_generation_blobs_restores_and_chains_completely() {
    let dir = scratch_dir("mixedgen");

    // Blob A at generation 0, in its own engine: a fresh `JitShared` cannot see
    // B, so A's edge stays a lookup call rather than a direct one.
    seed_chain(&dir, 0);
    {
        let (mut cpu, cache) = engine_with_chain(&dir);
        // Re-open the file written above and add B under a bumped generation.
        cpu.shared.invalidate_gen.fetch_add(7, Ordering::Release);
        // `precompile_at` now offers the block to the code cache before
        // compiling it, so the blob `seed_chain` wrote for B at generation 0
        // would be *replayed* here and this arm would record nothing new. The
        // arm's whole point is a file holding two generations, so drop the older
        // blob first: this is the "nothing to replay, therefore compile" case.
        cache.invalidate_range(CHAIN_BASE + 0x08, CHAIN_BASE + 0x0a);
        cpu.precompile_at(CHAIN_BASE + 0x08);
        let c = counts(&cache);
        assert_eq!(c.skipped_unpersistable, 0, "got {c:?}");
        assert_eq!(
            cache.test_blob(CHAIN_BASE + 0x08).map(|b| b.inv_gen),
            Some(7),
            "B is the blob from the later generation"
        );
        assert_eq!(
            cache.test_blob(CHAIN_BASE).map(|b| b.inv_gen),
            Some(0),
            "A survives the rewrite from the earlier generation"
        );
        cpu.shared.finish_jit_caches();
        drop((cpu, cache));
    }

    // Third process, generation 0 again, opens the single mixed file.
    let (mut cpu, _cache) = engine_with_chain(&dir);
    let c = restore_chain(&mut cpu);
    assert_eq!(c.restored, 2, "both blobs must restore: {c:?}");
    assert_eq!(c.refused(), 0, "no generation refusal may remain: {c:?}");

    run_chain(&mut cpu);
    assert!(
        cpu.stats().chain.hops > 0,
        "both mixed-generation blobs must chain, got {} hops",
        cpu.stats().chain.hops
    );
}

/// A restored block whose *frame* bake goes stale must fall back to the
/// dispatcher, not chain on.
///
/// Invalidating an unrelated page leaves the chain fixture's own bytes and
/// therefore its blob hash untouched, but bumps `invalidate_gen` — so the frame
/// the restored entry runs under carries a stale bake even though every `Ready`
/// block still matches its bytes. The emitted guard's job in that state is to
/// exit to the dispatcher, which purges and re-syncs: correctness first,
/// chaining second. This is the one direction the coarser frame-generation
/// guard is allowed to move in, and it is why it cannot produce a wrong answer.
///
/// (If a future change re-stamps surviving `Ready` entries on invalidation, this
/// test's `hops == 0` assertion is the thing to relax — that change is strictly
/// better and would restore chaining for this case.)
#[test]
fn a_stale_frame_bake_after_an_unrelated_invalidation_exits_instead_of_chaining() {
    let dir = scratch_dir("stalebake");
    seed_chain(&dir, 0);

    let (mut cpu, _cache) = engine_with_chain(&dir);
    let c = restore_chain(&mut cpu);
    assert_eq!(c.restored, 2, "both blobs must restore: {c:?}");

    // A second, untouched page — the invalidation cannot overlap the fixture,
    // so no `Ready` entry is dropped and the persisted hashes still match.
    let other = CHAIN_BASE + 0x10_0000;
    map_code_page(&mut cpu, other);
    cpu.mem_write(other, &[0x90, 0x90, 0x90, 0x90, 0x90])
        .expect("write unrelated");
    // Compiled so its address is a known code page: `invalidate_code_range`
    // ignores a range no compiled block covers, and would not bump at all.
    cpu.precompile_at(other);
    assert!(cpu.has_ready_at(other), "unrelated block must compile");
    cpu.invalidate_code_range(other, 4);
    assert_ne!(
        cpu.shared.invalidate_gen.load(Ordering::Acquire),
        0,
        "an unrelated invalidation still bumps the shared generation"
    );
    assert!(
        cpu.has_ready_at(CHAIN_BASE),
        "the fixture's own bytes are untouched, so its entry survives"
    );

    let resyncs_before = cpu.stats().chain.resyncs;
    run_chain(&mut cpu);
    assert!(
        cpu.stats().chain.resyncs > resyncs_before,
        "the dispatcher must observe the generation change and rebuild its \
         chain table (resyncs {} -> {})",
        resyncs_before,
        cpu.stats().chain.resyncs
    );
    assert_eq!(
        cpu.stats().chain.hops,
        0,
        "a stale frame bake must exit at every chain edge, never chain on"
    );
}

// ---------------------------------------------------------------------------
// Direct chain edges (format v3): a guest VA crosses, never an address
// ---------------------------------------------------------------------------

/// Persist the whole ring from **one** engine, so `A`'s `jmp` compiles while `B`
/// is already chainable and therefore becomes a direct chain `bl` (`Arm64Call`
/// to `B`'s `FuncId`) rather than a late-bound lookup call.
///
/// That is the case the format used to refuse — a block whose relocation names
/// another block instead of a host import. The successor's guest VA is the only
/// part of that reference that survives a process boundary.
fn seed_chained_ring(dir: &Path) -> CodeCacheCounts {
    let (mut cpu, cache) = engine_with_chain(dir);
    cpu.precompile_at(CHAIN_BASE + 0x08);
    cpu.precompile_at(CHAIN_BASE);
    let c = counts(&cache);
    assert_eq!(c.compiled, 2, "both blocks recorded: {c:?}");
    assert_eq!(
        c.skipped_unpersistable, 0,
        "a direct chain edge is persistable: {c:?}"
    );
    let a = cache.test_blob(CHAIN_BASE).expect("blob at A");
    assert_eq!(
        a.chain_relocs.len(),
        1,
        "A's `jmp B` must be exactly one chain edge, got {:?}",
        a.chain_relocs
    );
    assert_eq!(
        a.chain_relocs[0].target_va,
        CHAIN_BASE + 0x08,
        "the edge records the successor's guest VA, never a `FuncId`"
    );
    for r in &a.relocs {
        let ModuleRelocTarget::User {
            namespace: 0,
            index,
        } = r.name
        else {
            panic!("unexpected relocation target class: {:?}", r.name);
        };
        assert!(
            cpu.shared.code_import_name(index).is_some(),
            "the import class still names host imports only (#{index})"
        );
    }
    cpu.shared.finish_jit_caches();
    drop((cpu, cache));
    c
}

/// Restore the ring **successors first**: a chain edge is re-resolved against a
/// successor this run already holds, so restoring one before its target is
/// (correctly) a refusal.
fn restore_chained_ring(cpu: &mut JitCpu) -> CodeCacheCounts {
    for va in [CHAIN_BASE + 0x08, CHAIN_BASE] {
        let mem = cpu.shared.mem.read().expect("mem lock");
        assert!(
            cpu.shared.restore_or_probe(&mem, va).is_some(),
            "blob at {va:#x} must restore; counts={:?}",
            counts_via_shared(cpu)
        );
    }
    cpu.resync_chain_table(cpu.shared.cache_epoch.load(Ordering::Relaxed));
    counts_via_shared(cpu)
}

/// The `FuncId` this run currently chains `va` to, if any.
fn chain_id_of(cpu: &JitCpu, va: u64) -> Option<u32> {
    cpu.shared
        .chain_ids
        .pin()
        .get(&va)
        .map(|t| t.func_id.as_u32())
}

/// The headline case: a block carrying a direct chain-table edge persists, is
/// replayed in a fresh process, and **chains** there.
///
/// `hops > 0` is the assertion that matters. Restoring is not the goal — a block
/// that restores and then bails to the dispatcher at its only chain edge would
/// satisfy `restored` while giving the warm boot back nothing, which is exactly
/// the trade the generation gate's removal had to avoid.
#[test]
fn a_chained_block_persists_restores_and_chains() {
    let dir = scratch_dir("chain-persist");
    seed_chained_ring(&dir);

    let (mut cpu, _cache) = engine_with_chain(&dir);
    assert!(
        !cpu.has_ready_at(CHAIN_BASE + 0x08),
        "sanity: nothing is compiled in the warm process before the restore"
    );
    let c = restore_chained_ring(&mut cpu);
    assert_eq!(c.restored, 2, "both blocks replay: {c:?}");
    assert_eq!(c.refused(), 0, "no guard fired: {c:?}");
    assert_eq!(c.compiled, 0, "nothing fell back to compiling: {c:?}");

    run_chain(&mut cpu);
    assert!(
        cpu.stats().chain.hops > 0,
        "a replayed block's direct chain edge must reach the live successor; \
         hops == 0 means the edge was dropped or aimed at a foreign address"
    );
}

/// A chain edge whose successor this run cannot offer must refuse the blob and
/// let the block be compiled — never honour a target it cannot name.
#[test]
fn a_chain_edge_with_no_live_successor_is_refused() {
    let dir = scratch_dir("chain-absent");
    seed_chained_ring(&dir);

    let (mut cpu, cache) = engine_with_chain(&dir);
    {
        let mem = cpu.shared.mem.read().expect("mem lock");
        assert!(
            cpu.shared.restore_or_probe(&mem, CHAIN_BASE).is_none(),
            "A must not restore while its only successor is absent"
        );
    }
    let c = counts(&cache);
    assert_eq!(c.restored, 0, "nothing may be replayed: {c:?}");
    assert_eq!(
        c.refused_unrelocatable, 1,
        "the unresolvable edge is the refusal: {c:?}"
    );
    assert!(
        !cpu.has_ready_at(CHAIN_BASE),
        "a refused blob must not install anything: {c:?}"
    );

    // The refusal is a compile, not a dead end.
    cpu.precompile_at(CHAIN_BASE);
    assert!(
        cpu.has_ready_at(CHAIN_BASE),
        "the ordinary compile path still runs"
    );
}

/// Guest region for the two-page fixture: `A` and `B` must sit on **different**
/// pages, because SMC invalidation is page-granular — writing `B`'s code would
/// otherwise drop `A`'s blob along with it, and the point here is to restore `A`
/// *after* the live chain table moved.
const RESOLVE_A: u64 = 0x10d0_0000;
const RESOLVE_B: u64 = 0x10d0_1000;
/// `A`: `add rax,rbx ; jmp RESOLVE_B`, then padding the jump must not target.
const RESOLVE_A_CODE: [u8; 11] = [
    0x48, 0x01, 0xd8, // +0x00 add rax,rbx
    0xe9, 0xf8, 0x0f, 0x00, 0x00, // +0x03 jmp rel32 -> RESOLVE_B
    0x90, 0x90, 0x90, // +0x08 padding
];
/// `B`: `dec rcx ; jnz RESOLVE_A ; ud2` — the `ud2` is the harness stop.
///
/// The backward branch is the `0f 85` form: a page is `rel8`-unreachable.
const RESOLVE_B_CODE: [u8; 11] = [
    0x48, 0xff, 0xc9, // +0x00 dec rcx
    0x0f, 0x85, 0xf7, 0xef, 0xff, 0xff, // +0x03 jnz rel32 -> RESOLVE_A
    0x0f, 0x0b, // +0x09 ud2
];
const RESOLVE_STOP: u64 = RESOLVE_B + 0x09;

/// An engine with the two-page ring mapped and the code cache attached.
fn engine_with_two_page_ring(dir: &Path) -> (JitCpu, Arc<CodeCache>) {
    let (mut cpu, cache) = engine_with_cache(dir);
    cpu.virtual_alloc(
        RESOLVE_A,
        0x2000,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_EXECUTE_READWRITE,
    )
    .expect("alloc two-page ring");
    cpu.mem_write(RESOLVE_A, &RESOLVE_A_CODE).expect("write A");
    cpu.mem_write(RESOLVE_B, &RESOLVE_B_CODE).expect("write B");
    cpu.thread.regs.set_gpr(1, CHAIN_ITERS); // rcx
    cpu.thread.regs.set_gpr(3, 1); // rbx
    (cpu, cache)
}

/// The recorded edge is re-resolved, so it lands on the successor this run has —
/// including after the live chain table has *moved*.
///
/// `B` is compiled, its bytes are changed, and it is compiled again: the table
/// then offers a different `FuncId` for the same guest VA, over code that
/// computes something the recording process never emitted. Restoring `A`
/// afterwards must chain into that *new* block — one iteration of the ring, not
/// [`CHAIN_ITERS`]. A baked target could only reach the old one, which belongs to
/// another process entirely.
#[test]
fn a_recorded_chain_target_is_re_resolved_against_the_live_run() {
    let dir = scratch_dir("chain-resolve");

    // --- cold run: one engine, `B` first so `A` chains directly to it --------
    {
        let (mut cpu, cache) = engine_with_two_page_ring(&dir);
        cpu.precompile_at(RESOLVE_B);
        cpu.precompile_at(RESOLVE_A);
        let a = cache.test_blob(RESOLVE_A).expect("blob at A");
        assert_eq!(a.chain_relocs.len(), 1, "A has one direct chain edge");
        assert_eq!(a.chain_relocs[0].target_va, RESOLVE_B);
        assert_eq!(counts(&cache).skipped_unpersistable, 0);
        cpu.shared.finish_jit_caches();
    }

    // --- warm run: the live table moves before `A` is restored --------------
    let (mut cpu, _cache) = engine_with_two_page_ring(&dir);
    cpu.precompile_at(RESOLVE_B);
    let before = chain_id_of(&cpu, RESOLVE_B);
    assert!(before.is_some(), "B is chainable before the change");

    // SMC on `B`'s page only: `dec rcx` -> `sub rcx, rcx`, so the ring now exits
    // after a single iteration.
    cpu.mem_write(RESOLVE_B, &[0x48, 0x29, 0xc9])
        .expect("rewrite B");
    cpu.invalidate_code_range(RESOLVE_B, 3);
    cpu.precompile_at(RESOLVE_B);
    let after = chain_id_of(&cpu, RESOLVE_B);
    assert_ne!(
        before, after,
        "the live chain table must offer a different FuncId for B now"
    );

    // `A`'s own bytes are untouched, so its blob still restores — and its edge
    // must be re-resolved to the block that exists now.
    let restored_before_a = counts_via_shared(&cpu).restored;
    {
        let mem = cpu.shared.mem.read().expect("mem lock");
        assert!(
            cpu.shared.restore_or_probe(&mem, RESOLVE_A).is_some(),
            "A must restore: counts={:?}",
            counts_via_shared(&cpu)
        );
    }
    cpu.resync_chain_table(cpu.shared.cache_epoch.load(Ordering::Relaxed));
    let c = counts_via_shared(&cpu);
    // A delta, not an absolute: `precompile_at(RESOLVE_B)` above now offers B to
    // the code cache and B's blob is replayed there, so the absolute count also
    // carries that restore. What this test is about is A's.
    assert_eq!(c.restored, restored_before_a + 1, "A replays: {c:?}");
    assert_eq!(c.refused(), 0, "the live successor is enough: {c:?}");

    cpu.run_until_stop(RESOLVE_A, RESOLVE_STOP, 0, 100_000, 0, 0)
        .expect("run the re-resolved ring");
    assert_eq!(
        cpu.thread.regs.rip, RESOLVE_STOP,
        "execution must reach the harness stop"
    );
    assert_eq!(
        cpu.thread.regs.gpr(0),
        1,
        "the edge must run the live successor's semantics (one iteration), not \
         the bytes that were in memory when the blob was recorded"
    );
    assert_eq!(
        cpu.thread.regs.gpr(1),
        0,
        "rcx was zeroed by `sub rcx, rcx`"
    );
    assert!(
        cpu.stats().chain.hops > 0,
        "the re-resolved edge must chain, got {} hops",
        cpu.stats().chain.hops
    );
}

/// The classification policy is a total function of a captured relocation list.
///
/// Pure — no JIT, no guest — because this is the enumeration the module docs'
/// safety argument rests on: every relocation lands in exactly one replayable
/// class or takes the whole block out of the cache.
#[test]
fn captured_relocations_are_classified_total() {
    use cranelift_codegen::binemit::Reloc;
    use cranelift_module::ModuleReloc;

    let reloc = |kind, index: u32, addend: i64| ModuleReloc {
        offset: 4,
        kind,
        name: ModuleRelocTarget::User {
            namespace: 0,
            index,
        },
        addend,
    };
    // `import_count` is the live module's; 14 is arbitrary here.
    const IMPORTS: u32 = 14;

    // An import: replayable by index, and never re-resolved as a chain edge.
    let (imports, chain) =
        classify_capture_relocs(vec![reloc(Reloc::Arm64Call, 3, 0)], IMPORTS, |_| {
            panic!("an import must not be treated as a chain edge")
        })
        .expect("a host import is persistable");
    assert_eq!(imports.len(), 1, "the import class keeps it");
    assert!(chain.is_empty(), "and the chain class does not");

    // A direct chain call to a block FuncId: recorded as the successor's VA.
    let (imports, chain) =
        classify_capture_relocs(vec![reloc(Reloc::Arm64Call, 22, 0)], IMPORTS, |id| {
            assert_eq!(id.as_u32(), 22, "the block id is what gets re-resolved");
            Some(0x1234)
        })
        .expect("a direct chain edge is persistable");
    assert!(imports.is_empty());
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0].target_va, 0x1234);

    // A block id this run cannot name to a VA: refuse the block.
    assert!(
        classify_capture_relocs(vec![reloc(Reloc::Arm64Call, 22, 0)], IMPORTS, |_| None).is_none(),
        "an unnameable successor is a refusal"
    );
    // Any non-call reference to a block has no guest-VA-shaped meaning.
    assert!(
        classify_capture_relocs(vec![reloc(Reloc::Abs8, 22, 0)], IMPORTS, |_| Some(1)).is_none(),
        "only a direct call can be re-resolved"
    );
    // A `bl` never carries an addend; a non-zero one is not something to guess at.
    assert!(
        classify_capture_relocs(vec![reloc(Reloc::Arm64Call, 22, 4)], IMPORTS, |_| Some(1))
            .is_none(),
        "a non-zero addend is refused"
    );
    // A data object (namespace 1) is not replayable at all.
    assert!(
        classify_capture_relocs(
            vec![ModuleReloc {
                offset: 4,
                kind: Reloc::Abs8,
                name: ModuleRelocTarget::User {
                    namespace: 1,
                    index: 0,
                },
                addend: 0,
            }],
            IMPORTS,
            |_| Some(1),
        )
        .is_none(),
        "a data object is refused"
    );
}

/// Chain-edge structural checks happen before any offset reaches
/// cranelift-jit's relocation applier, whose own check is a `debug_assert`.
#[test]
fn structurally_unsound_chain_edges_are_refused() {
    use cranelift_codegen::binemit::Reloc;
    use cranelift_module::ModuleReloc;

    let build = |code_len: usize, offset: u32, target_va: u64| {
        let (relocs, chain_relocs) = classify_capture_relocs(
            vec![ModuleReloc {
                offset: 0,
                kind: Reloc::Arm64Call,
                name: ModuleRelocTarget::User {
                    namespace: 0,
                    index: 99,
                },
                addend: 0,
            }],
            1,
            |_| Some(target_va),
        )
        .expect("one direct chain edge");
        // `classify_capture_relocs` only ever produces a sound offset; moving it
        // afterwards is what the structural check exists to catch.
        let mut chain_relocs = chain_relocs;
        chain_relocs[0].offset = offset;
        PersistedCode {
            va: CODE_BASE,
            guest_end: CODE_BASE + 0x10,
            bytes_hash: 0,
            insn_count: 1,
            inv_gen: 0,
            compiled_at_opt: OptTier::Base,
            align: 16,
            code: vec![0u8; code_len],
            relocs,
            chain_relocs,
        }
    };

    assert!(
        build(16, 0, CHAIN_BASE).structurally_sound(),
        "a chain edge inside the blob is sound"
    );
    assert!(
        !build(4, 2, CHAIN_BASE).structurally_sound(),
        "an edge whose 4 bytes run past the end is not"
    );
    assert!(
        !build(16, 0, 0).structurally_sound(),
        "guest VA 0 is not code"
    );
    let mut dup = build(16, 0, CHAIN_BASE);
    let edge = dup.chain_relocs[0];
    dup.chain_relocs.push(edge);
    assert!(
        !dup.structurally_sound(),
        "two chain edges patching the same bytes are ambiguous"
    );

    let mut mixed = build(16, 0, CHAIN_BASE);
    mixed.relocs.push(ModuleReloc {
        offset: 0,
        kind: Reloc::Arm64Call,
        name: ModuleRelocTarget::User {
            namespace: 0,
            index: 0,
        },
        addend: 0,
    });
    assert!(
        !mixed.structurally_sound(),
        "an import reloc and a chain edge on one offset are ambiguous"
    );
}

/// Write one valid blob to `dir` by compiling the block once in a throwaway
/// engine, then dropping it (which flushes).
fn seed_one_blob(dir: &Path) {
    let (mut cpu, cache) = engine_with_cache(dir);
    let _ = run_to_stop(&mut cpu);
    await_ready(&cpu);
    let c = counts(&cache);
    assert!(c.compiled >= 1, "seeding must persist a blob: {c:?}");
    assert_eq!(c.skipped_unpersistable, 0, "got {c:?}");
    drop((cpu, cache));
    assert!(sole_code_file(dir).exists(), "blob file written on drop");
}

/// The single `.code` file the cache wrote under `dir`.
fn sole_code_file(dir: &Path) -> PathBuf {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("read cache dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "code"))
        .collect();
    assert_eq!(found.len(), 1, "expected one .code file, got {found:?}");
    found.pop().expect("one file")
}

// --- ledger write order, driven by the real install path --------------------

/// Walk the 10 code bytes one `step_one` at a time, exactly as [`run_to_stop`]
/// does **without** its `precompile_at(CODE_BASE)` ordering pin, so the entry
/// block's background install races the later blocks' installs.
fn walk_without_ordering_pin(cpu: &mut JitCpu) {
    cpu.write_rip(CODE_BASE).expect("rip");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let Ok(rip) = cpu.read_rip() else { break };
        if rip >= STOP_RIP {
            break;
        }
        let Ok((result, _retired)) = cpu.step_one() else {
            break;
        };
        if !matches!(result, StepResult::Continue) {
            break;
        }
    }
    // Installs land asynchronously; wait for the ledger to stop growing so the
    // assertions see the settled set rather than a snapshot mid-walk.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last = usize::MAX;
    let mut quiet = 0;
    while Instant::now() < deadline && quiet < 4 {
        std::thread::sleep(Duration::from_millis(25));
        let now = cpu.shared.persist.test_entries_len();
        quiet = if now == last { quiet + 1 } else { 0 };
        last = now;
    }
}

/// The ledger row for the entry block must survive an **unordered** interpreted
/// walk: the walk queues one block per instruction, so the entry block's install
/// races every later block's, and a refused (`Never`) verdict for the same VA
/// can land in the same window (this is the shape that used to erase the row —
/// see `cache_persist`'s `write_order` tests for the mechanism and the fix).
///
/// Two directions, because they fail for different reasons: the in-memory table
/// must hold the entry block's `Ready` row, and the *next process* must be able
/// to consume it.
///
/// This is a **guard**, not the repro: it also passed before the fix, because in
/// this harness the entry block's install usually wins the race on its own. The
/// erasure needs both writers on the *same* first contact, which
/// `cache_persist::write_order::ready_beats_never_under_a_concurrent_first_contact`
/// drives directly (and which failed in ~20–44 % of rounds before the fix).
#[test]
fn entry_block_ledger_row_survives_an_unordered_walk() {
    use crate::jit::cache_persist;
    let dir = scratch_dir("ledger-order");
    cache_persist::test_set_dir(dir.clone());
    let (mut cpu, _cache) = engine_with_cache(&dir);
    walk_without_ordering_pin(&mut cpu);

    let table = cpu.shared.persist.test_vas();
    assert!(
        table.contains(&CODE_BASE),
        "the entry block never reached the ledger: table={table:x?}",
    );
    // It must be a *positive* row, not a `Never` verdict for the same VA: a
    // `Never` probes as absent everywhere and would seed `CacheEntry::Never`
    // next run, permanently retiring a block that compiles fine.
    let (never, end, insns) = cpu
        .shared
        .persist
        .test_row(CODE_BASE)
        .expect("entry block row");
    assert!(!never, "entry block row holds a Never verdict");
    assert!(
        end > CODE_BASE && end <= CODE_BASE + CODE.len() as u64,
        "entry block extent {end:#x} inside the walked bytes"
    );
    assert!(insns > 0, "entry block row carries an instruction count");

    cpu.shared.persist.test_flush();
    let disk = cache_persist::PersistentJitCache::test_disk_vas(dir.clone(), PE_HASH);
    assert!(
        disk.contains(&CODE_BASE),
        "flush wrote {} rows without the entry block: {disk:x?}",
        disk.len()
    );
    // And the row a later process would load is still the positive one.
    assert!(
        cache_persist::PersistentJitCache::test_disk_row_never(dir, PE_HASH, CODE_BASE)
            == Some(false),
        "the reloaded ledger serves a Never verdict for the entry block"
    );
}

// ---------------------------------------------------------------------------
// Silent give-ups inside `record_code`
// ---------------------------------------------------------------------------

/// The five `return`s in `JitShared::record_code` used to leave no trace: a
/// block could be dropped before it ever became a `PersistedCode` and the run
/// would still report `unpersistable=0`, which means "this format refuses that
/// code", not "there was no code to offer". These are the reasons that are
/// actually reachable from a test, each driven through the real install path
/// (`persist_record_ready`) rather than by poking the counters.
#[test]
fn record_code_counts_its_silent_give_ups_by_reason() {
    let dir = scratch_dir("silent-reasons");
    let (mut cpu, cache) = engine_with_cache(&dir);
    let nofunc = SilentReason::NoFunc.index();
    let lock = SilentReason::Lock.index();
    let capture = SilentReason::Capture.index();
    let hash = SilentReason::Hash.index();

    // --- compile once with the cache off, so the capture stays in the module
    // A real `record_code` would consume it, and the phases below need one
    // capture each in the order they are written.
    cpu.shared.set_code_cache(None);
    cpu.precompile_at(CODE_BASE);
    cpu.shared.set_code_cache(Some(Arc::clone(&cache)));
    let compiled = ready_block(&cpu);

    // --- lock: another thread holds the engine ---------------------------
    // The block is dropped on purpose rather than blocking a disk concern on
    // the install path, so this is expected and benign — and now visible.
    {
        let _held = cpu.shared.engine.lock().expect("engine lock");
        cpu.shared.persist_record_ready(&ready_block(&cpu));
    }

    // The engine was free this time, so the lock give-up above is the only one
    // counted so far.
    let c = counts(&cache);
    assert_eq!(c.silent[lock], 1, "held engine is a counted give-up: {c:?}");
    assert_eq!(
        c.skipped_unpersistable, 0,
        "a lock give-up is not the format refusing the code: {c:?}"
    );

    // --- nofunc: a block with no Cranelift id ----------------------------
    let mut no_id = compiled;
    no_id.func_id = None;
    cpu.shared.persist_record_ready(&no_id);
    assert_eq!(
        counts(&cache).silent[nofunc],
        1,
        "a FuncId-less install is a counted give-up"
    );

    // --- hash: the block's guest bytes cannot be read back ---------------
    // Real capture, unmapped guest range: this is the case where the blob
    // could not be keyed to what will run next time, so persisting it would be
    // worse than recompiling.
    let mut unmapped = compiled;
    unmapped.guest_start = 0x5000_0000;
    unmapped.guest_end = 0x5000_0004;
    cpu.shared.persist_record_ready(&unmapped);
    assert_eq!(
        counts(&cache).silent[hash],
        1,
        "an unhashable guest range is a counted give-up"
    );

    // --- capture: the capture was already consumed -----------------------
    // The failed `hash` attempt above took it, which is exactly the shape a
    // second install of the same block sees.
    cpu.shared.persist_record_ready(&compiled);
    let c = counts(&cache);
    assert_eq!(c.silent[capture], 1, "a consumed capture is counted: {c:?}");

    // Every reason kept its own slot: no cross-counting, and `compiled` (the
    // blob counter) never moved because no blob was ever offered.
    assert_eq!(c.compiled, 0, "no give-up may count as a persist: {c:?}");
    assert_eq!(
        c.skipped_unpersistable, 0,
        "and none of them is the format's refusal: {c:?}"
    );
    assert!(
        c.silent.iter().sum::<u64>() == 4,
        "one give-up per phase and no more: {c:?}"
    );
}

/// The breakdown in the exit summary must name every reason, zeros included:
/// a field that only appears when non-zero is how `restored=0 refused=0` stayed
/// unreadable for the whole of the previous slice.
#[test]
fn the_summary_names_every_silent_reason() {
    let dir = scratch_dir("silent-summary");
    let (_cpu, cache) = engine_with_cache(&dir);
    let line = cache.summary().expect("enabled cache has a summary");
    for label in ["nofunc", "lock", "nomod", "capture", "hash"] {
        assert!(
            line.contains(&format!("{label}=0")),
            "summary omits {label}: {line}"
        );
    }
    assert!(
        line.contains("unpersistable_silent("),
        "the breakdown needs a name that is not `unpersistable`: {line}"
    );
}
