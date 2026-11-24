// Implicit-lock RMW semantics: `XCHG` / `CMPXCHG` against memory must be a
// single atomic read-modify-write on **both** backends.
//
// Split out of `tests/mod.rs` (file-size policy, ADR-002). Every test here runs
// against the interpreter and the JIT, because both were broken: the
// interpreter did a plain load plus a separate store, and the JIT *compiled*
// that broken pair to native code.
//
// ## What these tests do and do not guarantee
//
// **They are not a proof.** The load and the store of one `xchg` are two
// instructions wide; nothing in this runtime can split them at a hook, so any
// white-box single-threaded test of atomicity is vacuous and any test that does
// split them is a race. The `*_rmw_is_mutually_exclusive_across_engines` tests
// are therefore deliberately *probabilistic* high-signal races: 32 real OS
// threads, one RMW each, no release, 300 independent trials. Measured against a
// load/store `atomic_rmw` / `atomic_cas` on a load-12 host, they detect the
// defect at trials 0, 43 and 53 and pass outright against an atomic one — but a
// pass is evidence, not proof.
//
// **The permission tests are deterministic** and are the load-bearing
// correctness argument: they prove the atomic paths still route every access
// through the software page-permission oracle, so making the RMW atomic did
// not turn it into an unchecked host write.
use super::*;
use crate::ThreadContext;
use crate::exec::{AccessType, StepResult as StepResultAlias};
use crate::regs::Rflags;
use iced_x86::{Decoder, DecoderOptions, Mnemonic};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

/// `xchg dword ptr [rbx], eax` — 87 /r. Implicitly locked on x86-64 (no LOCK
/// prefix in the encoding), which is why the memory form must be atomic.
const XCHG_MEM_R32: [u8; 2] = [0x87, 0x03];
/// `lock cmpxchg dword ptr [rbx], ecx` — F0 0F B1 /r.
const LOCK_CMPXCHG_MEM_R32: [u8; 4] = [0xF0, 0x0F, 0xB1, 0x0B];
/// `xchg word ptr [rbx], ax` — 66 87 /r. A sub-word operand has no host atomic
/// to use, so it always takes the locked fallback.
const XCHG_MEM_R16: [u8; 3] = [0x66, 0x87, 0x03];

/// 64 KiB-aligned: MEM_RESERVE rounds both base and size to the Windows
/// allocation granularity, and the code reservations below are rounded the same
/// way, so the data page needs its own non-overlapping 64 KiB slot.
pub(super) const DATA_BASE: u64 = 0x4008_0000;

// The interpreter's decode cache is process-wide and thread-local, keyed by
// (rip, mem_generation) — and every test CPU here starts at generation 0, so
// the *same* rip always hits the *first* decode of that rip in the process.
// Distinct code VAs per distinct byte sequence are therefore mandatory: reusing
// one rip for two encodings makes the second case silently execute the first
// case's instruction.
const CODE_XCHG32: u64 = 0x4000_0000;
const CODE_XCHG16: u64 = 0x4001_0000;
const CODE_CMPXCHG: u64 = 0x4002_0000;
/// Decode-only (never executed), so it only has to differ from the above.
const CODE_DECODE_ONLY: u64 = 0x4003_0000;
/// 32-bit lock word. Aligned, so `host_span` can hand out an `AtomicI32`.
pub(super) const LOCK_WORD: u64 = DATA_BASE;
/// One byte in, so no naturally-aligned host atomic exists — the locked
/// fallback has to supply the mutual exclusion.
pub(super) const UNALIGNED_WORD: u64 = DATA_BASE + 1;

// ── Engine plumbing ─────────────────────────────────────────────────────

pub(super) enum Engine {
    Iced(Box<IcedCpu>),
    Jit(Box<JitCpu>),
}

impl Engine {
    pub(super) fn cpu(&mut self) -> &mut dyn CpuEngine {
        match self {
            Self::Iced(c) => c.as_mut(),
            Self::Jit(c) => c.as_mut(),
        }
    }

    /// Execute enough to retire the single RMW under test.
    pub(super) fn run_rmw(&mut self) -> StepResultAlias {
        match self {
            Self::Iced(c) => c.step_once_result().expect("iced step"),
            Self::Jit(c) => {
                let (result, _retired) = c.step_one().expect("jit step");
                result
            }
        }
    }

    pub(super) fn set_state(&mut self, ctx: &ThreadContext) {
        self.cpu().restore_thread_context(ctx);
        // A real guest thread switch does the same TLB/chain drop; without it a
        // JIT engine would reuse the previous trial's translated pointers.
        self.cpu().on_thread_switch();
    }

    pub(super) fn rax(&mut self) -> u64 {
        self.cpu().read_rax().expect("read rax")
    }

    /// ZF after the RMW (CMPXCHG is the only instruction here that sets it).
    fn zf(&mut self) -> bool {
        let rflags = self.cpu().snapshot_thread_context().rflags;
        (rflags & Rflags::ZF) == Rflags::ZF
    }
}

pub(super) fn open(backend: Backend) -> Engine {
    match backend {
        Backend::Iced => Engine::Iced(Box::new(IcedCpu::open_x86_64())),
        Backend::Jit => Engine::Jit(Box::new(JitCpu::open_x86_64())),
    }
}

/// A fresh per-thread engine over the *same* guest memory — the 1:1 model the
/// runtime uses for guest threads (`IcedCpu::new_standalone_with_mem` /
/// `JitCpu::new_shared`), and the reason two guest threads can race at all.
pub(super) fn open_worker(primary: &Engine) -> Engine {
    match primary {
        Engine::Iced(c) => Engine::Iced(Box::new(IcedCpu::new_standalone_with_mem(Arc::clone(
            c.guest_mem_arc(),
        )))),
        Engine::Jit(c) => Engine::Jit(Box::new(JitCpu::new_shared(Arc::clone(c.shared_jit())))),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Backend {
    Iced,
    Jit,
}

pub(super) const BACKENDS: [Backend; 2] = [Backend::Iced, Backend::Jit];

/// Register file for one RMW: RIP at the code page, RBX pointing at the word.
pub(super) fn ctx(code_base: u64, word: u64, rax: u64, rcx: u64) -> ThreadContext {
    let mut gpr = [0_u64; 16];
    gpr[0] = rax; // RAX — the XCHG source / the CMPXCHG accumulator
    gpr[1] = rcx; // RCX — the CMPXCHG source
    gpr[3] = word; // RBX — the memory operand's base register
    ThreadContext {
        rip: code_base,
        gpr,
        ..ThreadContext::default()
    }
}

/// Map code + data, then plant `op` at the code base with a `nop` filler and a
/// `ud2` terminator (the filler reaches the 2-insn compile minimum; the `ud2`
/// stops linear decode so the zero-filled tail cannot extend the block).
pub(super) fn plant(primary: &mut Engine, code_base: u64, op: &[u8], word: u64, initial: u32) {
    let cpu = primary.cpu();
    // One reservation (MEM_RESERVE wants a 64 KiB-aligned base) covering the
    // code page, then map the shared data page to plain RW:
    // `host_span(.., write=true)` refuses executable spans, so an RWX data page
    // would push every RMW in these tests onto the fallback and the host-atomic
    // arm would go untested.
    cpu.virtual_alloc(
        code_base,
        0x1000,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_EXECUTE_READWRITE,
    )
    .expect("code alloc");
    cpu.virtual_alloc(
        DATA_BASE,
        0x1000,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_READWRITE,
    )
    .expect("data alloc");

    let mut code = Vec::with_capacity(op.len() + 3);
    code.extend_from_slice(op);
    code.extend_from_slice(&[0x90, 0x0F, 0x0B]);
    cpu.mem_write(code_base, &code).expect("write code");
    cpu.mem_write(word, &initial.to_le_bytes())
        .expect("write lock word");
}

pub(super) fn read_word(primary: &mut Engine, word: u64) -> u32 {
    let mut buf = [0_u8; 4];
    primary.cpu().mem_read(word, &mut buf).expect("read word");
    u32::from_le_bytes(buf)
}

pub(super) fn write_word(primary: &mut Engine, word: u64, value: u32) {
    primary
        .cpu()
        .mem_write(word, &value.to_le_bytes())
        .expect("write word");
}

// ── Encoding sanity (deterministic) ─────────────────────────────────────

/// The bytes above must still be the instructions these tests claim: an iced
/// upgrade that re-encodes `XCHG` memory forms (or drops the `LOCK` prefix
/// from `CMPXCHG`) would otherwise silently weaken the tests.
#[test]
fn rmw_test_encodings_are_what_we_think() {
    let mut dec = Decoder::with_ip(64, &XCHG_MEM_R32, CODE_DECODE_ONLY, DecoderOptions::NONE);
    let xchg = dec.decode();
    assert_eq!(xchg.mnemonic(), Mnemonic::Xchg, "87 03 must be XCHG");
    // iced reports the LOCK *prefix byte* only: the XCHG memory form is locked
    // by the architecture with no prefix in the encoding, and `has_lock_prefix`
    // does not reflect that. Nothing may decide XCHG atomicity from this flag —
    // key off the memory operand instead.
    assert!(
        !xchg.has_lock_prefix(),
        "87 03 carries no LOCK prefix; implicit locking is not visible here"
    );
    assert_eq!(xchg.memory_base(), iced_x86::Register::RBX);

    let mut dec = Decoder::with_ip(
        64,
        &LOCK_CMPXCHG_MEM_R32,
        CODE_DECODE_ONLY,
        DecoderOptions::NONE,
    );
    let cas = dec.decode();
    assert_eq!(
        cas.mnemonic(),
        Mnemonic::Cmpxchg,
        "F0 0F B1 0B must be CMPXCHG"
    );
    assert!(cas.has_lock_prefix(), "the test encoding carries LOCK");
    assert_eq!(cas.memory_base(), iced_x86::Register::RBX);

    let mut dec = Decoder::with_ip(64, &XCHG_MEM_R16, CODE_DECODE_ONLY, DecoderOptions::NONE);
    let x16 = dec.decode();
    assert_eq!(x16.mnemonic(), Mnemonic::Xchg, "66 87 03 must be XCHG");
    assert_eq!(
        x16.memory_size(),
        iced_x86::MemorySize::UInt16,
        "the sub-word case must really be 16-bit, or it stops covering the fallback"
    );
}

// ── Single-threaded semantics (deterministic) ───────────────────────────

/// `XCHG [rbx], eax` swaps and clears ZF. Checks the aligned host-atomic path,
/// the locked fallback (unaligned), and the sub-word fallback in one sweep.
#[test]
fn xchg_mem_swaps_word_and_register_on_both_backends() {
    for backend in BACKENDS {
        for (label, op, word) in [
            ("aligned", &XCHG_MEM_R32[..], LOCK_WORD),
            ("unaligned", &XCHG_MEM_R32[..], UNALIGNED_WORD),
            ("16-bit", &XCHG_MEM_R16[..], UNALIGNED_WORD),
        ] {
            let mut cpu = open(backend);
            let initial = if op.len() == 3 {
                0x3333_u32
            } else {
                0x1111_1111
            };
            let incoming = if op.len() == 3 {
                0x2222_u32
            } else {
                0x2222_2222
            };
            let code_base = if op.len() == 3 {
                CODE_XCHG16
            } else {
                CODE_XCHG32
            };
            plant(&mut cpu, code_base, op, word, initial);
            cpu.set_state(&ctx(code_base, word, u64::from(incoming), 0));
            assert!(
                matches!(cpu.run_rmw(), StepResultAlias::Continue),
                "{backend:?} {label}: xchg must retire"
            );
            assert_eq!(
                read_word(&mut cpu, word),
                incoming,
                "{backend:?} {label}: memory must receive the register value"
            );
            assert_eq!(
                cpu.rax(),
                u64::from(initial),
                "{backend:?} {label}: register must receive the old memory value"
            );
        }
    }
}

/// `CMPXCHG [rbx], ecx`: equal → memory gets RCX, RAX and ZF unchanged;
/// unequal → RAX gets the memory word, ZF cleared, memory untouched.
#[test]
fn cmpxchg_mem_compares_and_exchanges_on_both_backends() {
    for backend in BACKENDS {
        for (label, word) in [("aligned", LOCK_WORD), ("unaligned", UNALIGNED_WORD)] {
            let mut cpu = open(backend);
            plant(&mut cpu, CODE_CMPXCHG, &LOCK_CMPXCHG_MEM_R32, word, 5);
            // RAX == word → exchange happens, ZF set, RAX untouched.
            cpu.set_state(&ctx(CODE_CMPXCHG, word, 5, 9));
            assert!(matches!(cpu.run_rmw(), StepResultAlias::Continue));
            assert_eq!(read_word(&mut cpu, word), 9, "{backend:?} {label}: stored");
            assert_eq!(cpu.rax(), 5, "{backend:?} {label}: acc unchanged");
            assert!(cpu.zf(), "{backend:?} {label}: ZF set on exchange");

            // RAX != word → no store, RAX gets the word, ZF cleared.
            cpu.set_state(&ctx(CODE_CMPXCHG, word, 7, 3));
            assert!(matches!(cpu.run_rmw(), StepResultAlias::Continue));
            assert_eq!(
                read_word(&mut cpu, word),
                9,
                "{backend:?} {label}: memory must not change on failure"
            );
            assert_eq!(cpu.rax(), 9, "{backend:?} {label}: acc gets the word");
            assert!(!cpu.zf(), "{backend:?} {label}: ZF clear on failure");
        }
    }
}

// ── Permission oracle (deterministic — the load-bearing argument) ───────

/// Assert an RMW against `word` faults as a guest **write** and leaves the
/// word byte-for-byte unchanged.
fn assert_write_faults(backend: Backend, label: &str, word: u32) {
    let mut cpu = open(backend);
    plant(&mut cpu, CODE_XCHG32, &XCHG_MEM_R32, DATA_BASE, word);
    // The RMW is a store as well as a load, so a page that cannot be written
    // must fault with the write access type — whatever the atomic path does
    // internally. This is the invariant an unchecked host write would break.
    cpu.cpu()
        .virtual_protect(DATA_BASE, 0x1000, protect::PAGE_READONLY)
        .expect("protect ro");
    cpu.set_state(&ctx(CODE_XCHG32, DATA_BASE, 0xdead_beef, 0));
    match cpu.run_rmw() {
        StepResultAlias::InvalidMemory(inv) => {
            assert_eq!(
                inv.access_type,
                AccessType::Write,
                "{backend:?} {label}: must fault as a write"
            );
            assert_eq!(inv.address, DATA_BASE, "{backend:?} {label}: fault address");
        }
        other => panic!("{backend:?} {label}: expected a write fault, got {other:?}"),
    }
    assert_eq!(
        read_word(&mut cpu, DATA_BASE),
        word,
        "{backend:?} {label}: a faulting atomic RMW must not write"
    );
}

/// A read-only guest page must still fault through the atomic RMW path.
#[test]
fn xchg_mem_faults_on_readonly_page() {
    for backend in BACKENDS {
        assert_write_faults(backend, "readonly", 0x1234_5678);
    }
}

/// The locked fallback (which `host_span` cannot serve) must not become an
/// escape hatch: a read-only page is exactly the case where the host atomic is
/// unavailable, so this test exercises the *new* path rather than the old one.
#[test]
fn xchg_mem_falls_back_without_bypassing_the_write_check() {
    for backend in BACKENDS {
        let mut cpu = open(backend);
        plant(&mut cpu, CODE_XCHG32, &XCHG_MEM_R32, DATA_BASE, 0x0bad_f00d);
        cpu.cpu()
            .virtual_protect(DATA_BASE, 0x1000, protect::PAGE_READONLY)
            .expect("protect ro");
        assert!(
            cpu.cpu().host_span(DATA_BASE, 4, true).is_none(),
            "{backend:?}: no host-atomic path exists for a read-only page, \
             so this case must go through the locked fallback"
        );
        cpu.set_state(&ctx(CODE_XCHG32, DATA_BASE, 0xdead_beef, 0));
        match cpu.run_rmw() {
            StepResultAlias::InvalidMemory(inv) => {
                assert_eq!(
                    inv.access_type,
                    AccessType::Write,
                    "{backend:?}: the fallback must fault as a write"
                );
            }
            other => panic!("{backend:?}: expected a write fault, got {other:?}"),
        }
        assert_eq!(read_word(&mut cpu, DATA_BASE), 0x0bad_f00d);
    }
}

/// An unmapped word faults as a write and, again, writes nothing.
#[test]
fn xchg_mem_faults_on_unmapped_word() {
    // Far outside the reservation (MEM_RESERVE rounds the span up, so a nearby
    // address can still land inside the arena).
    const UNMAPPED: u64 = DATA_BASE + 0x0040_0000;
    for backend in BACKENDS {
        let mut cpu = open(backend);
        plant(&mut cpu, CODE_XCHG32, &XCHG_MEM_R32, LOCK_WORD, 0x0bad_f00d);
        cpu.set_state(&ctx(CODE_XCHG32, UNMAPPED, 0xdead_beef, 0));
        match cpu.run_rmw() {
            // Read or Write: on an unmapped word the atomic RMW's *load* is the
            // first access to touch the page, so either tag is defensible. What
            // matters is that it faults at all and writes nothing — the
            // write-specific proof is the read-only page case above.
            StepResultAlias::InvalidMemory(inv) => {
                assert!(
                    matches!(inv.access_type, AccessType::Read | AccessType::Write),
                    "{backend:?}: unexpected access type {:?}",
                    inv.access_type
                );
                assert_eq!(inv.address, UNMAPPED, "{backend:?}");
            }
            other => panic!("{backend:?}: expected a fault, got {other:?}"),
        }
        assert_eq!(read_word(&mut cpu, LOCK_WORD), 0x0bad_f00d, "{backend:?}");
    }
}

/// An executable-but-not-writable page is the one case where `host_span`
/// refuses for a reason *other* than the permission bits: SMC has to go
/// through `write` so the JIT invalidates code. The RMW must take the locked
/// fallback there and still respect the missing write permission.
#[test]
fn xchg_mem_on_executable_page_still_needs_write_permission() {
    for backend in BACKENDS {
        let mut cpu = open(backend);
        plant(&mut cpu, CODE_XCHG32, &XCHG_MEM_R32, DATA_BASE, 0x0bad_f00d);
        cpu.cpu()
            .virtual_protect(DATA_BASE, 0x1000, protect::PAGE_EXECUTE_READ)
            .expect("protect rx");
        assert!(
            cpu.cpu().host_span(DATA_BASE, 4, true).is_none(),
            "{backend:?}: host_span must refuse a write onto an executable span"
        );
        cpu.set_state(&ctx(CODE_XCHG32, DATA_BASE, 0xdead_beef, 0));
        match cpu.run_rmw() {
            StepResultAlias::InvalidMemory(inv) => {
                assert_eq!(inv.access_type, AccessType::Write, "{backend:?}");
            }
            other => panic!("{backend:?}: expected a write fault, got {other:?}"),
        }
        assert_eq!(read_word(&mut cpu, DATA_BASE), 0x0bad_f00d, "{backend:?}");
    }
}

// ── Mutual exclusion (high-signal race, not a proof) ────────────────────

/// Threads hammering one RMW against one word. Each thread does a *single*
/// exchange with no release, exactly like a guest spinlock's acquisition
/// attempt against a word nobody holds.
///
/// Invariants, both of which a load-then-store RMW violates loudly:
///
/// * the initial value can be observed by **at most one** thread — every other
///   thread must read some other thread's `tid`, which is only true if the RMW
///   orders its read against its write;
/// * `0` ("the lock was free") can be observed by **at most one** thread.
///
/// With 32 threads released from a barrier, hundreds of trials and a
/// two-instruction window, a non-atomic implementation produces a dozen
/// simultaneous observers in the first trial. A pass is evidence of atomicity,
/// not a proof of it.
fn run_xchg_rmw_race(word: u64, unaligned: bool) {
    const THREADS: usize = 32;
    const TRIALS: usize = 300;

    for backend in BACKENDS {
        let mut primary = open(backend);
        plant(&mut primary, CODE_XCHG32, &XCHG_MEM_R32, word, u32::MAX);
        if unaligned {
            // `host_span` maps the 4 bytes fine — it is *alignment* that
            // keeps `host_atomic_rmw` from offering an `AtomicI32`, so this
            // address can only be served by the striped host lock.
            assert_eq!(
                word % 4,
                1,
                "{backend:?}: the unaligned variant must not be 4-byte aligned"
            );
        }

        // Atomic slots: each thread owns one, the main thread reads them all
        // after the `fire` barrier, so no borrow of the array outlives a spawn.
        let observed: Vec<AtomicU64> = (0..THREADS).map(|_| AtomicU64::new(0)).collect();
        let arm = Barrier::new(THREADS + 1);
        let fire = Barrier::new(THREADS + 1);
        // Engines are built here and moved in: the shared primary is not `Sync`
        // (per-thread JIT state holds raw host pointers), and the runtime builds
        // worker engines on the spawning thread for the same reason.
        let workers: Vec<Engine> = (0..THREADS).map(|_| open_worker(&primary)).collect();
        std::thread::scope(|scope| {
            for (tid, mut cpu) in (1..=THREADS).zip(workers) {
                let arm = &arm;
                let fire = &fire;
                let slot = &observed[tid - 1];
                scope.spawn(move || {
                    let state = ctx(CODE_XCHG32, word, u64::try_from(tid).unwrap_or(u64::MAX), 0);
                    for _ in 0..TRIALS {
                        arm.wait();
                        cpu.set_state(&state);
                        assert!(
                            matches!(cpu.run_rmw(), StepResultAlias::Continue),
                            "xchg must retire"
                        );
                        // XCHG leaves the old memory value in the register.
                        slot.store(cpu.rax(), Ordering::Relaxed);
                        fire.wait();
                    }
                });
            }

            // Recorded, not asserted, inside the scope: a panic here would
            // strand every worker on the next `fire.wait()` and the failure
            // would read as a hang instead of as the race it is.
            let mut stale_reads: Option<(usize, usize)> = None;
            let mut double_acquire: Option<(usize, usize)> = None;
            let mut lost_write: Option<(usize, u32)> = None;
            for trial in 0..TRIALS {
                write_word(&mut primary, word, u32::MAX);
                arm.wait();
                fire.wait();

                let seen: Vec<u64> = observed
                    .iter()
                    .map(|slot| slot.load(Ordering::Relaxed))
                    .collect();
                let first = seen.iter().filter(|&&v| v == u64::from(u32::MAX)).count();
                let free = seen.iter().filter(|&&v| v == 0).count();
                let final_word = read_word(&mut primary, word);
                if first != 1 && stale_reads.is_none() {
                    stale_reads = Some((trial, first));
                }
                if free > 1 && double_acquire.is_none() {
                    double_acquire = Some((trial, free));
                }
                if !(1..=THREADS).contains(&(final_word as usize)) && lost_write.is_none() {
                    lost_write = Some((trial, final_word));
                }
            }
            assert!(
                stale_reads.is_none() && double_acquire.is_none() && lost_write.is_none(),
                "{backend:?} (unaligned={unaligned}): {THREADS} threads x \
                 {TRIALS} trials of a single `xchg` against one word found \
                 {stale_reads:?} (trial, threads that read the initial value \
                 — only one may), {double_acquire:?} (trial, threads that all \
                 saw the word free and 'acquired' it), {lost_write:?} (trial, \
                 final word that is not any thread's tid — a lost write). \
                 The RMW is not atomic."
            );
        });
    }
}

#[test]
fn xchg_rmw_is_mutually_exclusive_across_engines() {
    run_xchg_rmw_race(LOCK_WORD, false);
}

#[test]
fn xchg_rmw_unaligned_fallback_is_mutually_exclusive() {
    run_xchg_rmw_race(UNALIGNED_WORD, true);
}

/// `CMPXCHG` with `RAX == 0` (expected) against a word holding `0`: under a
/// correct atomic CAS exactly one thread can observe the expected value and
/// exchange. A load-then-store version lets several threads read `0` and all
/// believe they won.
#[test]
fn cmpxchg_rmw_is_mutually_exclusive_across_engines() {
    const THREADS: usize = 32;
    const TRIALS: usize = 300;

    for backend in BACKENDS {
        let mut primary = open(backend);
        plant(
            &mut primary,
            CODE_CMPXCHG,
            &LOCK_CMPXCHG_MEM_R32,
            LOCK_WORD,
            0,
        );
        let winners: Vec<AtomicU64> = (0..THREADS).map(|_| AtomicU64::new(0)).collect();
        let arm = Barrier::new(THREADS + 1);
        let fire = Barrier::new(THREADS + 1);
        let workers: Vec<Engine> = (0..THREADS).map(|_| open_worker(&primary)).collect();
        std::thread::scope(|scope| {
            for (tid, mut cpu) in (1..=THREADS).zip(workers) {
                let arm = &arm;
                let fire = &fire;
                let slot = &winners[tid - 1];
                scope.spawn(move || {
                    // RAX = expected (0), RCX = this thread's tid.
                    let state = ctx(
                        CODE_CMPXCHG,
                        LOCK_WORD,
                        0,
                        u64::try_from(tid).unwrap_or(u64::MAX),
                    );
                    for _ in 0..TRIALS {
                        arm.wait();
                        cpu.set_state(&state);
                        assert!(
                            matches!(cpu.run_rmw(), StepResultAlias::Continue),
                            "cmpxchg must retire"
                        );
                        // RAX keeps `expected` only if this thread won.
                        slot.store(cpu.rax(), Ordering::Relaxed);
                        fire.wait();
                    }
                });
            }

            // Recorded, not asserted, inside the scope: a panic here would
            // strand every worker on the next `fire.wait()` and the failure
            // would read as a hang instead of as the race it is.
            let mut double_winner: Option<(usize, usize)> = None;
            let mut lost_write: Option<(usize, u32)> = None;
            for trial in 0..TRIALS {
                write_word(&mut primary, LOCK_WORD, 0);
                arm.wait();
                fire.wait();
                let won = winners
                    .iter()
                    .filter(|slot| slot.load(Ordering::Relaxed) == 0)
                    .count();
                let final_word = read_word(&mut primary, LOCK_WORD);
                if won != 1 && double_winner.is_none() {
                    double_winner = Some((trial, won));
                }
                if !(1..=THREADS).contains(&(final_word as usize)) && lost_write.is_none() {
                    lost_write = Some((trial, final_word));
                }
            }
            assert!(
                double_winner.is_none() && lost_write.is_none(),
                "{backend:?}: {THREADS} threads x {TRIALS} trials of a single \
                 `lock cmpxchg` against one word found {double_winner:?} \
                 (trial, threads that all exchanged — exactly one may) and \
                 {lost_write:?} (trial, final word that is not any thread's tid \
                 — a lost write). The CAS is not atomic."
            );
        });
    }
}
