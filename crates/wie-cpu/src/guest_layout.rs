//! Shared guest-layout constants (TEB, CRT data page, FILE* cookies, process heap).
//!
//! `wie-cpu` is the common dependency of `wie-winapi` and `wie-runtime`, so
//! constants that cross the crate boundary live here instead of being
//! restated in each crate. Every consumer — the JIT fast paths, the UCRT
//! handlers, the runtime session, the guest-heap accelerators — reads or
//! writes the same guest objects and must agree on their addresses; one
//! definition keeps them in lockstep.
//!
//! Protection encodings are deliberately NOT here: `RwxPerms` (rwx bits) and
//! `PageProtect` (`PAGE_*`) collide numerically while meaning different
//! things, so they stay separate types with explicit conversions
//! (`wie_cpu::RwxPerms`, `wie_cpu::mem::protect::PageProtect`).

use crate::GS_BASE;

// ── TEB ──────────────────────────────────────────────────────────────

/// Offset of `TEB.LastErrorValue` from the GS base.
pub const TEB_LAST_ERROR_OFFSET: u64 = 0x68;

/// Guest VA of `TEB.LastErrorValue` (GS base + offset).
pub const TEB_LAST_ERROR_VA: u64 = GS_BASE + TEB_LAST_ERROR_OFFSET;

/// Offset of `NT_TIB.StackBase` (exclusive top of the thread's stack).
pub const TEB_STACK_BASE_OFFSET: u64 = 0x08;

/// Offset of `NT_TIB.StackLimit` (low end of the committed stack).
pub const TEB_STACK_LIMIT_OFFSET: u64 = 0x10;

/// Offset of `NT_TIB.Self` (the TEB self pointer).
pub const TEB_SELF_OFFSET: u64 = 0x30;

/// Offset of `TEB.ProcessEnvironmentBlock` (every TEB points at the one PEB).
pub const TEB_PEB_OFFSET: u64 = 0x60;

/// Size of one per-thread TEB page (Windows x64: a full page per TEB).
pub const TEB_PAGE_SIZE: usize = 0x1000;

// ── CRT data page ────────────────────────────────────────────────────

/// Guest base of the synthetic UCRT data page (FILE* cookies, CRT pointer slots).
pub const CRT_GUEST_BASE: u64 = 0x0000_0000_6800_0000;

/// FILE* cookie for stdin (`__acrt_iob_func(0)`).
pub const CRT_FILE_STDIN: u64 = CRT_GUEST_BASE;
/// FILE* cookie for stdout (`__acrt_iob_func(1)`).
pub const CRT_FILE_STDOUT: u64 = CRT_GUEST_BASE + 0x100;
/// FILE* cookie for stderr (`__acrt_iob_func(2)`).
pub const CRT_FILE_STDERR: u64 = CRT_GUEST_BASE + 0x200;
/// Slot holding `char***` for `__p__environ`.
pub const CRT_ENVIRON_PTR_SLOT: u64 = CRT_GUEST_BASE + 0x300;
/// Slot holding `char***` for `__p___argv`.
pub const CRT_ARGV_PTR_SLOT: u64 = CRT_GUEST_BASE + 0x308;
/// Slot holding `int*` for `__p___argc`.
pub const CRT_ARGC_SLOT: u64 = CRT_GUEST_BASE + 0x310;
/// Slot holding `int*` for `__p__commode`.
pub const CRT_COMMODE_SLOT: u64 = CRT_GUEST_BASE + 0x318;
/// Slot holding `int*` for `__p__fmode`.
pub const CRT_FMODE_SLOT: u64 = CRT_GUEST_BASE + 0x320;
/// Slot holding `char**` for `__p__acmdln`.
pub const CRT_ACMDLN_PTR_SLOT: u64 = CRT_GUEST_BASE + 0x328;
/// Slot holding `wchar_t***` for `__p___wargv`.
pub const CRT_WARGV_PTR_SLOT: u64 = CRT_GUEST_BASE + 0x330;
/// Slot holding `wchar_t***` for `__p__wenviron`.
pub const CRT_WENVIRON_PTR_SLOT: u64 = CRT_GUEST_BASE + 0x338;
/// Vararg staging scratch for the UCRT format handlers.
pub const CRT_FORMAT_SCRATCH: u64 = CRT_GUEST_BASE + 0x340;
/// `char* argv[]` pointer table (null-terminated).
pub const CRT_ARGV_TABLE: u64 = CRT_GUEST_BASE + 0x400;
/// Storage for argv string bodies.
pub const CRT_ARGV_STRINGS: u64 = CRT_GUEST_BASE + 0x500;
/// One past the last byte of the CRT data page.
pub const CRT_PAGE_END: u64 = CRT_GUEST_BASE + 0x1000;

// ── process heap ─────────────────────────────────────────────────────

/// Number of fixed size classes (powers-of-two-ish ladder).
pub const HEAP_SIZE_CLASS_COUNT: usize = 24;

/// Size classes in bytes (strictly increasing, all ≥ 16, 16-byte aligned).
pub const SIZE_CLASSES: [u64; HEAP_SIZE_CLASS_COUNT] = [
    16, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536, 2048, 3072, 4096, 6144, 8192,
    12288, 16384, 24576, 32768, 49152, 65536,
];

/// Threshold above which blocks use the large free-list instead of size classes.
///
/// Historically 16 MiB, which made `round_up_size(65537..=16MiB)` return a
/// full 16 MiB slab on every medium `malloc` — progressive process-heap burn
/// during 7za/LZMA.
pub const LARGE_THRESHOLD: u64 = 65_536;

/// Heap control block: bump cursor lives at `ctrl + 0`.
pub const HEAP_CTRL_BUMP_OFFSET: u64 = 0;
/// Heap control block: first freelist head sits at `ctrl + 8`.
pub const HEAP_CTRL_HEAD_BASE: u64 = 8;
/// Heap control block: stride between freelist heads (one u64 per class).
pub const HEAP_CTRL_HEAD_STRIDE: u64 = 8;
/// Heap control block size in bytes (bump cursor + all freelist heads).
pub const HEAP_CTRL_SIZE: usize = 8 + HEAP_SIZE_CLASS_COUNT * 8;
/// Per-block size header written at `payload - 8` (bump and size-class paths).
pub const HEAP_BLOCK_HEADER_SIZE: u64 = 8;
/// Payload alignment (16 bytes) applied by bump allocation.
pub const HEAP_PAYLOAD_ALIGN: u64 = 16;

// Compile-time drift gates: every constant is pinned to the guest ABI value
// it encodes, so a mistyped edit fails the build instead of corrupting a
// guest layout (same pattern as `RuntimeMemoryLayout`'s const gate).
const _: () = assert!(TEB_LAST_ERROR_OFFSET == 0x68);
const _: () = assert!(TEB_LAST_ERROR_VA == GS_BASE + 0x68);
const _: () = assert!(TEB_STACK_BASE_OFFSET == 0x08);
const _: () = assert!(TEB_STACK_LIMIT_OFFSET == 0x10);
const _: () = assert!(TEB_SELF_OFFSET == 0x30);
const _: () = assert!(TEB_PEB_OFFSET == 0x60);
const _: () = assert!(TEB_PAGE_SIZE == 0x1000);
// Field offsets are strictly ordered and each is a distinct slot in the page.
const _: () = assert!(TEB_STACK_BASE_OFFSET < TEB_STACK_LIMIT_OFFSET);
const _: () = assert!(TEB_STACK_LIMIT_OFFSET < TEB_SELF_OFFSET);
const _: () = assert!(TEB_SELF_OFFSET < TEB_PEB_OFFSET);
const _: () = assert!(TEB_PEB_OFFSET < TEB_LAST_ERROR_OFFSET);
const _: () = assert!(CRT_GUEST_BASE == 0x0000_0000_6800_0000);
const _: () = assert!(CRT_FILE_STDIN == CRT_GUEST_BASE);
const _: () = assert!(CRT_FILE_STDOUT == CRT_GUEST_BASE + 0x100);
const _: () = assert!(CRT_FILE_STDERR == CRT_GUEST_BASE + 0x200);
const _: () = assert!(CRT_ENVIRON_PTR_SLOT == CRT_GUEST_BASE + 0x300);
const _: () = assert!(CRT_ARGV_PTR_SLOT == CRT_GUEST_BASE + 0x308);
const _: () = assert!(CRT_ARGC_SLOT == CRT_GUEST_BASE + 0x310);
const _: () = assert!(CRT_COMMODE_SLOT == CRT_GUEST_BASE + 0x318);
const _: () = assert!(CRT_FMODE_SLOT == CRT_GUEST_BASE + 0x320);
const _: () = assert!(CRT_ACMDLN_PTR_SLOT == CRT_GUEST_BASE + 0x328);
const _: () = assert!(CRT_WARGV_PTR_SLOT == CRT_GUEST_BASE + 0x330);
const _: () = assert!(CRT_WENVIRON_PTR_SLOT == CRT_GUEST_BASE + 0x338);
const _: () = assert!(CRT_FORMAT_SCRATCH == CRT_GUEST_BASE + 0x340);
const _: () = assert!(CRT_ARGV_TABLE == CRT_GUEST_BASE + 0x400);
const _: () = assert!(CRT_ARGV_STRINGS == CRT_GUEST_BASE + 0x500);
const _: () = assert!(CRT_PAGE_END == CRT_GUEST_BASE + 0x1000);
const _: () = assert!(LARGE_THRESHOLD == 65_536);
const _: () = assert!(HEAP_CTRL_BUMP_OFFSET == 0);
const _: () = assert!(HEAP_CTRL_HEAD_BASE == 8);
const _: () = assert!(HEAP_CTRL_HEAD_STRIDE == 8);
const _: () = assert!(HEAP_CTRL_SIZE == 8 + HEAP_SIZE_CLASS_COUNT * 8);
const _: () = assert!(HEAP_BLOCK_HEADER_SIZE == 8);
const _: () = assert!(HEAP_PAYLOAD_ALIGN == 16);
const _: () = assert!(matches!(
    SIZE_CLASSES,
    [
        16, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536, 2048, 3072, 4096, 6144, 8192,
        12288, 16384, 24576, 32768, 49152, 65536
    ]
));

#[cfg(test)]
mod tests {
    use super::*;

    /// TEB layout: last-error slot sits at GS base + 0x68, exactly as the
    /// micro-stub matcher and the guest-stub table expect.
    #[test]
    fn teb_last_error_va_matches_guest_abi() {
        assert_eq!(TEB_LAST_ERROR_OFFSET, 0x68);
        assert_eq!(TEB_LAST_ERROR_VA, GS_BASE + 0x68);
    }

    /// TEB field offsets: the standard x64 NT_TIB/TEB layout that the session
    /// initializer and the per-thread TEB init (`crate::teb`) both write.
    /// Ordering and in-page placement are pinned by the module-level const
    /// gates; this test documents the values.
    #[test]
    fn teb_field_offsets_match_guest_abi() {
        assert_eq!(TEB_STACK_BASE_OFFSET, 0x08);
        assert_eq!(TEB_STACK_LIMIT_OFFSET, 0x10);
        assert_eq!(TEB_SELF_OFFSET, 0x30);
        assert_eq!(TEB_PEB_OFFSET, 0x60);
        assert_eq!(TEB_PAGE_SIZE, 0x1000);
    }

    /// CRT data page: every named slot lands at the offset the runtime session,
    /// the UCRT handlers, and the FILE* cookie stubs all assume.
    #[test]
    fn crt_page_slots_match_guest_abi() {
        assert_eq!(CRT_FILE_STDIN, CRT_GUEST_BASE);
        assert_eq!(CRT_FILE_STDOUT, CRT_GUEST_BASE + 0x100);
        assert_eq!(CRT_FILE_STDERR, CRT_GUEST_BASE + 0x200);
        assert_eq!(CRT_ENVIRON_PTR_SLOT, CRT_GUEST_BASE + 0x300);
        assert_eq!(CRT_ARGV_PTR_SLOT, CRT_GUEST_BASE + 0x308);
        assert_eq!(CRT_ARGC_SLOT, CRT_GUEST_BASE + 0x310);
        assert_eq!(CRT_COMMODE_SLOT, CRT_GUEST_BASE + 0x318);
        assert_eq!(CRT_FMODE_SLOT, CRT_GUEST_BASE + 0x320);
        assert_eq!(CRT_ACMDLN_PTR_SLOT, CRT_GUEST_BASE + 0x328);
        assert_eq!(CRT_WARGV_PTR_SLOT, CRT_GUEST_BASE + 0x330);
        assert_eq!(CRT_WENVIRON_PTR_SLOT, CRT_GUEST_BASE + 0x338);
        assert_eq!(CRT_FORMAT_SCRATCH, CRT_GUEST_BASE + 0x340);
        assert_eq!(CRT_ARGV_TABLE, CRT_GUEST_BASE + 0x400);
        assert_eq!(CRT_ARGV_STRINGS, CRT_GUEST_BASE + 0x500);
        assert_eq!(CRT_PAGE_END, CRT_GUEST_BASE + 0x1000);
    }

    /// Heap size classes: the exact ladder both `GuestHeap` (winapi) and the
    /// JIT `malloc`/`free` helpers round against; the last class equals the
    /// large threshold.
    #[test]
    fn heap_size_classes_are_canonical() {
        let expected: [u64; 24] = [
            16, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536, 2048, 3072, 4096, 6144,
            8192, 12288, 16384, 24576, 32768, 49152, 65536,
        ];
        assert_eq!(SIZE_CLASSES, expected);
        assert_eq!(HEAP_SIZE_CLASS_COUNT, 24);
        assert!(SIZE_CLASSES.iter().all(|&c| c >= 16 && c % 16 == 0));
        assert!(SIZE_CLASSES.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(SIZE_CLASSES.last().copied(), Some(LARGE_THRESHOLD));
        assert_eq!(LARGE_THRESHOLD, 65_536);
    }

    /// Heap control block geometry shared by the host `GuestHeap`, the JIT
    /// helpers, and the in-guest heap accelerator.
    #[test]
    fn heap_control_block_layout_matches() {
        assert_eq!(HEAP_CTRL_BUMP_OFFSET, 0);
        assert_eq!(HEAP_CTRL_HEAD_BASE, 8);
        assert_eq!(HEAP_CTRL_HEAD_STRIDE, 8);
        assert_eq!(HEAP_CTRL_SIZE, 8 + 24 * 8);
        assert_eq!(HEAP_BLOCK_HEADER_SIZE, 8);
        assert_eq!(HEAP_PAYLOAD_ALIGN, 16);
    }
}
