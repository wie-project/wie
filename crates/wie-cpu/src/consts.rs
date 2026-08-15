//! Architecture-wide numeric constants shared by the iced interpreter and the
//! Cranelift JIT lowering.
//!
//! x86-64 operand widths, the shift/rotate count masks that derive from them,
//! and the lane-splat multipliers appear as bare literals across both
//! backends. Naming them here keeps the interpreter and the JIT in lockstep
//! and lets each site say what the number means instead of guessing.

/// Bits in one byte — the factor for every `bytes → bits` conversion.
pub(crate) const BITS_PER_BYTE: u32 = 8;

/// Bits in a u16 operand (word).
pub(crate) const WORD_BITS: u32 = 16;
/// Bits in a u32 operand (dword); the 5-bit shift-count space.
pub(crate) const DWORD_BITS: u32 = 32;
/// Bits in a u64 operand (qword); the 6-bit shift-count space.
pub(crate) const QWORD_BITS: u32 = 64;

/// Bytes in a u32 operand (dword).
pub(crate) const DWORD_BYTES: usize = 4;
/// Bytes in a u64 operand (qword; also a 64-bit stack slot).
pub(crate) const QWORD_BYTES: usize = 8;
/// Bytes in an XMM register (128 bits).
pub(crate) const XMM_BYTES: usize = 16;

/// All-ones mask of the low 8 bits (`0xff`).
pub(crate) const BYTE_MASK: u64 = 0xff;
/// All-ones mask of the low 32 bits (`0xffff_ffff`).
pub(crate) const DWORD_MASK: u64 = 0xffff_ffff;

/// Shift/rotate count mask for 32-bit operands: the count is taken mod 32
/// (`0x1F`).
pub(crate) const SHIFT_MASK_32: u32 = DWORD_BITS - 1;
/// Shift/rotate count mask for 64-bit operands: the count is taken mod 64
/// (`0x3F`).
pub(crate) const SHIFT_MASK_64: u32 = QWORD_BITS - 1;

/// Splat a 16-bit lane value into every word of a u64 (4 copies).
pub(crate) const SPLAT_WORD: u64 = 0x0001_0001_0001_0001;
/// Splat a 32-bit lane value into every dword of a u64 (2 copies).
pub(crate) const SPLAT_DWORD: u64 = 0x0000_0001_0000_0001;
