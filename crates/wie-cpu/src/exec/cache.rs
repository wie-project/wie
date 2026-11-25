//! Decode cache for the iced x86-64 interpreter.
//!
//! Direct-mapped RIP → decoded `Instruction` cache plus the optional mnemonic
//! tracer counters (`WIE_EXEC_TRACE=1`). Low-level CPU arithmetic
//! intentionally uses wrapping ops, truncating casts, and direct indexing of
//! fixed-size buffers — clippy pedantic is not useful here.

use crate::mem::GuestMemory;
use iced_x86::Instruction;
use std::sync::LazyLock;
use std::sync::atomic::AtomicU64;

/// Iced-interpreter mnemonic counters (activated by `WIE_EXEC_TRACE=1`).
/// Works in release too — 7za residual discovery should not require a debug build.
pub(super) static ICED_COUNTERS: LazyLock<Box<[AtomicU64]>> = LazyLock::new(|| {
    std::iter::repeat_with(|| AtomicU64::new(0))
        .take(2048)
        .collect()
});
pub(super) static ICED_TRACE_ENABLED: LazyLock<bool> =
    LazyLock::new(|| std::env::var("WIE_EXEC_TRACE").is_ok_and(|v| v == "1"));

/// Direct-mapped RIP → decoded `Instruction` cache (per thread).
///
/// Every iced step used to do a full `Decoder::with_ip` + `decode()` pair — the
/// dominant cost on interpreter-bound loops. This cache stores the last
/// `DECODE_CACHE_SLOTS` distinct instruction decodes indexed by `rip`, tagged
/// with the `GuestMemory::generation` snapshot so any protect / free / SMC
/// invalidation naturally shoots the whole cache without explicit clearing.
///
/// `Instruction` is `Copy` (~32 bytes); at 512 slots this is ~24 KiB per thread.
const DECODE_CACHE_SLOTS: usize = 512;

/// Maximum x86-64 instruction length. A fetch this wide always contains one
/// whole instruction, so a decode from it can never be a truncated read.
pub(super) const MAX_INSN_LEN: usize = 15;

/// Fetch attempts for a decode-time read of guest memory. `mem_gen` is bumped by
/// every concurrent `map`/`protect`/`free`, and `GuestMemory::fetch_into` reports
/// that as an error by design ("instruction fetch generation changed …"), so a
/// single attempt turns another thread's `VirtualProtect` into a decode failure.
/// `WIE_DECODE_RETRY=0` restores the pre-fix single-attempt behaviour, for
/// reproducing the race.
static DECODE_FETCH_ATTEMPTS: LazyLock<u32> = LazyLock::new(|| {
    std::env::var("WIE_DECODE_RETRY")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(3)
});

/// `WIE_MT_DEBUG=1` logs every decode-time fetch that needed more than one
/// attempt, plus the exact error string of a failed one. This is the standing
/// evidence for the `mt_contention` transient-fault diagnosis: the string names
/// which of the fetch's failure modes happened.
static MT_DEBUG: LazyLock<bool> =
    LazyLock::new(|| std::env::var("WIE_MT_DEBUG").is_ok_and(|v| v == "1"));

/// What a decode-time fetch produced.
pub(super) enum Fetched {
    /// `n` bytes at the rip, read under mapping generation `mgen`.
    Bytes { n: usize, mgen: u64, attempts: u32 },
    /// Every attempt failed; carries the last error verbatim.
    Failed(crate::CpuError),
}

/// Read up to [`MAX_INSN_LEN`] bytes at `rip` for the decoder, retrying a
/// transient mapping-generation change.
///
/// A retry is only ever taken on a *failed* fetch, and each attempt re-samples
/// the generation, so this cannot turn a genuine fault (unmapped, no-execute)
/// into a success: those fail identically on every attempt.
pub(super) fn fetch_for_decode(
    mem: &GuestMemory,
    rip: u64,
    buf: &mut [u8; MAX_INSN_LEN],
) -> Fetched {
    let attempts = (*DECODE_FETCH_ATTEMPTS).max(1);
    let mut last: Option<crate::CpuError> = None;
    for attempt in 0..attempts {
        let mgen = mem.generation();
        match mem.fetch_into(rip, buf) {
            Ok(n) => {
                if attempt > 0 && *MT_DEBUG {
                    eprintln!(
                        "[decode] rip={rip:#x} n={n} mgen={mgen} attempts={} reason=retried-after: {}",
                        attempt + 1,
                        last.as_ref().map(|e| e.to_string()).unwrap_or_default()
                    );
                }
                return Fetched::Bytes {
                    n,
                    mgen,
                    attempts: attempt + 1,
                };
            }
            Err(e) => {
                if *MT_DEBUG {
                    eprintln!(
                        "[decode] rip={rip:#x} mgen={mgen} attempts={} fetch-failed: {e}",
                        attempt + 1
                    );
                }
                last = Some(e);
            }
        }
    }
    Fetched::Failed(last.unwrap_or_else(|| {
        crate::CpuError::Message("instruction fetch failed with no error recorded".to_string())
    }))
}

/// Is the short fetch at `rip` explained by the end of the mapping, or was it
/// truncated while more bytes were readable?
///
/// `fetch_into` shrinks to the last legal prefix, so `n < MAX_INSN_LEN` means a
/// permission or mapping boundary sits at `rip + n`. That is a legitimate
/// shortening — but only if the byte at `rip + n` really is unreadable. If it
/// *is* readable, the shrink was spurious (the race in `fetch_into`'s generation
/// check) and a decode from that buffer may be a truncated instruction: exactly
/// the "executes fabricated instructions" shape. Returns `true` when the
/// shortfall is **not** explained, i.e. when the decode must not be trusted.
fn shortfall_unexplained(mem: &GuestMemory, rip: u64, n: usize) -> bool {
    if n >= MAX_INSN_LEN {
        return false;
    }
    let Some(next) = rip.checked_add(u64::try_from(n).unwrap_or(0)) else {
        return false;
    };
    let mut probe = [0_u8; 1];
    // Readable ⇒ the fetch could have had this byte ⇒ the truncation was the
    // race, not a boundary.
    mem.read(next, &mut probe).is_ok()
}

/// The `WIE_MT_DEBUG` switch, for callers that report a fetch failure.
pub(super) fn mt_debug_enabled() -> bool {
    *MT_DEBUG
}

/// Why a decode did not yield an instruction. The two cases are different bugs
/// and the caller must not conflate them.
pub(super) enum DecodeOutcome {
    /// A whole instruction, decoded from a fetch that provably covered it.
    Decoded(Instruction, u32),
    /// Bytes were fetched and iced rejected them: the encoding really is invalid.
    InvalidEncoding,
    /// The bytes could not be read at all (unmapped, no-execute, or a mapping
    /// race that outlived the retry budget). This is a **fetch** condition, not a
    /// decode failure, and reporting it as one is what made a lost race look like
    /// a bad instruction.
    FetchFailed(crate::CpuError),
}

#[derive(Clone, Copy)]
struct DecodeSlot {
    /// RIP tag; `u64::MAX` when the slot is cold.
    rip: u64,
    /// `GuestMemory::generation` when the decode was captured.
    mem_gen: u64,
    /// Cached iced-x86 `Instruction`.
    instr: Instruction,
    /// Instruction length in bytes (0 when cold).
    len: u32,
}

impl DecodeSlot {
    fn empty() -> Self {
        // `Instruction::new` is not const in this iced-x86 version, so this
        // helper stays a plain fn (called at thread-local init time only).
        Self {
            rip: u64::MAX,
            mem_gen: 0,
            instr: Instruction::new(),
            len: 0,
        }
    }
}

thread_local! {
    static DECODE_CACHE: std::cell::RefCell<Box<[DecodeSlot; DECODE_CACHE_SLOTS]>> =
        std::cell::RefCell::new({
            // Build a Vec then reify to a fixed-size Box<[T; N]>. Vec<T>::into_boxed_slice
            // returns Box<[T]>; TryFrom<Box<[T]>> for Box<[T; N]> handles the resize.
            let vec: Vec<DecodeSlot> = (0..DECODE_CACHE_SLOTS).map(|_| DecodeSlot::empty()).collect();
            vec.into_boxed_slice()
                .try_into()
                .unwrap_or_else(|_| Box::new([DecodeSlot::empty(); DECODE_CACHE_SLOTS]))
        });
}

#[inline]
fn decode_slot_index(rip: u64) -> usize {
    // Fold in high and mid bits so nearby RIPs (single-instruction advance)
    // spread across the cache instead of colliding on the same slot.
    let x = rip ^ (rip >> 12) ^ (rip >> 28);
    (x as usize) & (DECODE_CACHE_SLOTS - 1)
}

/// Look up (or fill) a cached iced decode at `rip`. Returns `(instruction, length)`.
///
/// A cache miss re-runs the iced-x86 decoder. Hits under the same guest-memory
/// generation return the cached instruction without touching the decoder.
pub(super) fn decode_at(mem: &GuestMemory, rip: u64) -> Option<(Instruction, u32)> {
    match decode_at_detail(mem, rip) {
        DecodeOutcome::Decoded(instr, len) => Some((instr, len)),
        DecodeOutcome::InvalidEncoding | DecodeOutcome::FetchFailed(_) => None,
    }
}

/// Look up (or fill) a cached decode at `rip`, keeping a rejected encoding and a
/// failed fetch **distinct**.
///
/// The distinction is load-bearing: `GuestMemory::fetch_into` refuses to hand out
/// bytes whose mapping generation moved while it was validating them, and that is
/// a *lost race* against another thread's `map`/`protect`/`free`, not a bad
/// instruction. Collapsing it into "no decode" is what made `mt_contention.exe`
/// report `invalid instruction at 0x140001481` on the perfectly valid `73 23`
/// (`jae`) at the back edge of `phase_interlock`: the interpreter's caller reached
/// `decode_at` mid-invalidation, got an error, and printed a decode failure.
pub(super) fn decode_at_detail(mem: &GuestMemory, rip: u64) -> DecodeOutcome {
    let gen_now = mem.generation();
    let idx = decode_slot_index(rip);
    let cached = DECODE_CACHE.with(|c| c.borrow().get(idx).copied());
    if let Some(slot) = cached
        && slot.rip == rip
        && slot.mem_gen == gen_now
        && slot.len != 0
    {
        return DecodeOutcome::Decoded(slot.instr, slot.len);
    }
    // Miss: fetch + decode + fill.
    let mut fetch_buf = [0_u8; MAX_INSN_LEN];
    let (n, mgen) = match fetch_for_decode(mem, rip, &mut fetch_buf) {
        Fetched::Bytes { n, mgen, .. } => (n, mgen),
        Fetched::Failed(e) => return DecodeOutcome::FetchFailed(e),
    };
    if n == 0 {
        return DecodeOutcome::InvalidEncoding;
    }
    // A short fetch that a boundary does not explain would put a *prefix* of an
    // instruction in front of the decoder, and a decoder handed a prefix happily
    // returns some other, shorter instruction. Re-fetch once; if the shortfall
    // survives, refuse rather than execute a fabricated instruction.
    let (n, mgen) = if shortfall_unexplained(mem, rip, n) {
        match fetch_for_decode(mem, rip, &mut fetch_buf) {
            Fetched::Bytes { n, mgen, attempts } if !shortfall_unexplained(mem, rip, n) => {
                if *MT_DEBUG {
                    eprintln!(
                        "[decode] rip={rip:#x} n={n} attempts={attempts} \
                         reason=truncated-fetch-resolved"
                    );
                }
                (n, mgen)
            }
            Fetched::Bytes { n, .. } => {
                return DecodeOutcome::FetchFailed(crate::CpuError::Message(format!(
                    "decode fetch truncated at {rip:#x}: {n} bytes while {next:#x} is readable \
                     (concurrent map/protect)",
                    next = rip.saturating_add(u64::try_from(n).unwrap_or(0))
                )));
            }
            Fetched::Failed(e) => return DecodeOutcome::FetchFailed(e),
        }
    } else {
        (n, mgen)
    };
    let Some(bytes) = fetch_buf.get(..n) else {
        return DecodeOutcome::InvalidEncoding;
    };
    let mut decoder = iced_x86::Decoder::with_ip(64, bytes, rip, iced_x86::DecoderOptions::NONE);
    let instr = decoder.decode();
    if instr.is_invalid() || instr.len() == 0 {
        return DecodeOutcome::InvalidEncoding;
    }
    let Ok(len) = u32::try_from(instr.len()) else {
        return DecodeOutcome::InvalidEncoding;
    };
    // The decoded instruction must lie inside what the fetch actually covered.
    // `len > n` is unreachable through iced (it cannot read past its buffer); the
    // check exists so a future decoder change cannot silently violate the
    // invariant this cache now depends on.
    if usize::try_from(len).unwrap_or(usize::MAX) > n {
        return DecodeOutcome::InvalidEncoding;
    }
    DECODE_CACHE.with(|c| {
        if let Some(dst) = c.borrow_mut().get_mut(idx) {
            *dst = DecodeSlot {
                rip,
                mem_gen: mgen,
                instr,
                len,
            };
        }
    });
    DecodeOutcome::Decoded(instr, len)
}

/// Invalidate the entire per-thread iced decode cache.
///
/// Callers use this when they know something perturbed guest code without
/// bumping [`GuestMemory::generation`] (e.g. an explicit `FlushInstructionCache`
/// or a SMC path that wants to be safe). Exposed but unused today; kept for
/// future SMC/`FlushInstructionCache` hookup.
#[allow(dead_code)]
pub(crate) fn iced_decode_cache_flush() {
    DECODE_CACHE.with(|c| {
        for slot in c.borrow_mut().iter_mut() {
            *slot = DecodeSlot::empty();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{
        DecodeOutcome, Fetched, MAX_INSN_LEN, decode_at, decode_at_detail, fetch_for_decode,
    };
    use crate::RwxPerms;
    use crate::mem::GuestMemory;

    const PAGE: usize = 0x1000;
    const PAGE_U: u64 = 0x1000;
    /// `jae rel8` — the two bytes at `0x140001481` in `mt_contention.exe`.
    const JAE: [u8; 2] = [0x73, 0x23];

    /// A fetch that shortens because the *mapping* ends must still decode: a
    /// two-byte instruction in the last two bytes of a page is whole.
    #[test]
    fn a_legal_shortening_at_the_end_of_a_mapping_decodes() {
        let mut mem = GuestMemory::new();
        mem.map(0x10_0000, PAGE, RwxPerms::ALL).expect("map page");
        let rip = 0x10_0000 + PAGE_U - 2;
        mem.write(rip, &JAE).expect("write jae");

        let mut buf = [0_u8; MAX_INSN_LEN];
        match fetch_for_decode(&mem, rip, &mut buf) {
            Fetched::Bytes { n, .. } => assert_eq!(n, 2, "the mapping ends after two bytes"),
            Fetched::Failed(e) => panic!("fetch failed: {e}"),
        }
        match decode_at_detail(&mem, rip) {
            DecodeOutcome::Decoded(instr, len) => {
                assert_eq!(len, 2, "jae is two bytes");
                assert_eq!(instr.next_ip(), rip + 2);
            }
            DecodeOutcome::InvalidEncoding => panic!("mapping-end truncation must decode"),
            DecodeOutcome::FetchFailed(e) => panic!("mapping-end truncation must decode: {e}"),
        }
    }

    /// The mutation guard for the truncated-fetch sibling.
    ///
    /// The page after the code is **readable but not executable**, so the fetch
    /// legitimately stops at the page boundary while the byte beyond it can be
    /// read: a decode from that buffer would be a *prefix*, and iced would happily
    /// return a shorter, different instruction. `shortfall_unexplained` is what
    /// refuses it. Make that function return `false` unconditionally and this test
    /// decodes the truncated buffer instead of reporting a fetch failure.
    #[test]
    fn a_truncated_fetch_with_more_bytes_readable_is_not_decoded() {
        let mut mem = GuestMemory::new();
        mem.map(0x10_0000, PAGE, RwxPerms::ALL)
            .expect("map code page");
        mem.map(0x10_1000, PAGE, RwxPerms::READ)
            .expect("map readable page");
        let rip = 0x10_0000 + PAGE_U - 2;
        mem.write(rip, &JAE).expect("write jae");

        let mut buf = [0_u8; MAX_INSN_LEN];
        match fetch_for_decode(&mem, rip, &mut buf) {
            Fetched::Bytes { n, .. } => assert_eq!(n, 2, "no-execute page shortens the fetch"),
            Fetched::Failed(e) => panic!("fetch failed: {e}"),
        }
        assert!(
            super::shortfall_unexplained(&mem, rip, 2),
            "the byte past the shortfall is readable, so the truncation is not a boundary"
        );
        match decode_at_detail(&mem, rip) {
            DecodeOutcome::FetchFailed(_) => {}
            DecodeOutcome::Decoded(_, len) => {
                panic!("decoded a truncated instruction ({len} bytes) instead of refusing")
            }
            DecodeOutcome::InvalidEncoding => {
                panic!("a truncated buffer must not be reported as a bad encoding")
            }
        }
    }

    /// An unmapped rip is a *fetch* condition. It must never be reported as an
    /// invalid encoding, which is what turned a lost mapping race into a fatal
    /// worker death.
    #[test]
    fn an_unmapped_rip_is_a_fetch_failure_not_a_bad_encoding() {
        let mem = GuestMemory::new();
        match decode_at_detail(&mem, 0x50_0000) {
            DecodeOutcome::FetchFailed(_) => {}
            DecodeOutcome::InvalidEncoding => panic!("unmapped must not be a bad encoding"),
            DecodeOutcome::Decoded(_, _) => panic!("nothing is mapped there"),
        }
        assert!(
            decode_at(&mem, 0x50_0000).is_none(),
            "the compat wrapper agrees"
        );
    }

    /// The other half of the split: genuinely invalid bytes on a good mapping
    /// still report an invalid encoding.
    #[test]
    fn a_real_invalid_encoding_is_still_reported_as_one() {
        let mut mem = GuestMemory::new();
        mem.map(0x10_0000, PAGE, RwxPerms::ALL).expect("map page");
        mem.write(0x10_0000, &[0x06]).expect("write invalid opcode"); // push es: invalid in 64-bit
        match decode_at_detail(&mem, 0x10_0000) {
            DecodeOutcome::InvalidEncoding => {}
            DecodeOutcome::Decoded(_, len) => panic!("decoded {len} bytes of an invalid opcode"),
            DecodeOutcome::FetchFailed(e) => panic!("fetch was fine, decode was not: {e}"),
        }
    }
}
