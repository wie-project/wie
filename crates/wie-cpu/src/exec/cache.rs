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
    let gen_now = mem.generation();
    let idx = decode_slot_index(rip);
    let cached = DECODE_CACHE.with(|c| c.borrow().get(idx).copied());
    if let Some(slot) = cached
        && slot.rip == rip
        && slot.mem_gen == gen_now
        && slot.len != 0
    {
        return Some((slot.instr, slot.len));
    }
    // Miss: fetch + decode + fill.
    let mut fetch_buf = [0_u8; 15];
    let n = mem.fetch_into(rip, &mut fetch_buf).ok()?;
    let mut decoder =
        iced_x86::Decoder::with_ip(64, fetch_buf.get(..n)?, rip, iced_x86::DecoderOptions::NONE);
    let instr = decoder.decode();
    if instr.is_invalid() || instr.len() == 0 {
        return None;
    }
    let len = u32::try_from(instr.len()).ok()?;
    DECODE_CACHE.with(|c| {
        if let Some(dst) = c.borrow_mut().get_mut(idx) {
            *dst = DecodeSlot {
                rip,
                mem_gen: gen_now,
                instr,
                len,
            };
        }
    });
    Some((instr, len))
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
