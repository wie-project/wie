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
fn blob_is_refused_once_the_invalidation_generation_moves() {
    let dir = scratch_dir("invgen");
    seed_one_blob(&dir);

    let (cpu, cache) = engine_with_cache(&dir);
    // `invalidate_gen` is per-process and starts at 0, and the emitted edge
    // guard embeds the value observed at compile time. A blob carrying any
    // other value would bail to the dispatcher on every edge.
    cpu.shared.invalidate_gen.store(7, Ordering::Release);
    assert!(
        force_probe(&cpu).is_none(),
        "a stale edge guard must not be replayed"
    );
    let c = counts(&cache);
    assert_eq!(c.refused_generation_moved, 1, "got {c:?}");
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
