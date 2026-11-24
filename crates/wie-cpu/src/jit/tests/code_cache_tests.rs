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

use super::*;
use crate::jit::cache_persist::LedgerProbe;
use crate::jit::engine::{CodeCache, CodeCacheCounts};
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
    cpu.virtual_alloc(
        CODE_BASE,
        0x1000,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_EXECUTE_READWRITE,
    )
    .expect("alloc");
    cpu.mem_write(CODE_BASE, CODE).expect("write code");
    cpu
}

/// Step to [`STOP_RIP`] and return the resulting register file.
///
/// Tolerates a step ending on the harness `ud2`, exactly as the rest of the
/// suite does: the terminator is the intended stop, not a failure.
fn run_to_stop(cpu: &mut JitCpu) -> RegFile {
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
/// host-helper imports. This asserts it on a real captured block: every
/// relocation resolves to a named import of the live module. If a future
/// lowering change introduced a relocation this test cannot classify (a
/// Cranelift libcall, a data object, a chain-table call to another block), it
/// fails here rather than in production on the second launch of a real guest.
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
    cpu.virtual_alloc(
        CHAIN_BASE,
        0x1000,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_EXECUTE_READWRITE,
    )
    .expect("alloc chain region");
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
    cpu.virtual_alloc(
        other,
        0x1000,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_EXECUTE_READWRITE,
    )
    .expect("alloc unrelated page");
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
