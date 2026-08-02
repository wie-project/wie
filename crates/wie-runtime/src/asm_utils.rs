//! Shared x86-64 machine-code patching helpers for in-guest stub generation.
//!
//! Every guest accelerator (guest I/O, MBWC, heap) emits a code buffer with
//! placeholder branch immediates and patches them once the layout is known.
//! The helpers were triplicated across those modules and merged here
//! (Idiom Phase I). `clear_bit` is shared with the stop-bitmap planters.

/// Write a 32-bit relative displacement (`E8`/`E9`/`0F 8x` imm32) at `imm_at`.
///
/// `next_ip` is the offset of the instruction following the immediate;
/// the displacement stored is `target − next_ip`. All values are small
/// offsets within the emitted code buffer.
pub(crate) fn patch_rel32(code: &mut [u8], imm_at: usize, next_ip: usize, target: usize) {
    let rel = target as i32 - next_ip as i32;
    code[imm_at..imm_at + 4].copy_from_slice(&rel.to_le_bytes());
}

/// Write an 8-bit relative displacement (short-jump imm8) at `imm_at`.
///
/// Panics when the displacement does not fit in `i8` — the stub layout is
/// wrong, and silently truncating would jump to the wrong address.
pub(crate) fn patch_rel8(code: &mut [u8], imm_at: usize, next_ip: usize, target: usize) {
    let rel = target as isize - next_ip as isize;
    assert!((-128..128).contains(&rel), "rel8 out of range {rel}");
    if let Some(slot) = code.get_mut(imm_at) {
        *slot = rel as i8 as u8;
    }
}

/// Clear one bit in a guest stop bitmap (fake-API VA range).
pub(crate) fn clear_bit(bitmap: &mut [u8], bit_index: usize) {
    let byte = bit_index >> 3;
    let bit = bit_index & 7;
    if let Some(slot) = bitmap.get_mut(byte) {
        *slot &= !(1_u8 << bit);
    }
}
