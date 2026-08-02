//! Windows x64 exception handling data structures.
//!
//! Layouts match the PE/COFF specification §5 (x64 exception handling):
//! `RUNTIME_FUNCTION` (12 bytes), `UNWIND_INFO` (variable), `UNWIND_CODE` (2 bytes each).
//!
//! These structs describe:
//! - How to find a function's unwind metadata from its RIP (`.pdata` → `RUNTIME_FUNCTION`)
//! - How to reverse the function's prologue during stack unwinding (`UNWIND_INFO` + `UNWIND_CODE`)
//! - Where the language-specific exception handler lives (flags in `UNWIND_INFO`)

// PE / UWOP interpreters use fixed field strides and guest-buffer indexes; saturating
// every intermediate offset would obscure the PE layout. Bounds are checked via
// `get` / `read_mem` failure, not panic-free arithmetic at every step.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::as_conversions,
    clippy::integer_division,
    clippy::match_same_arms,
    clippy::result_unit_err,
    clippy::needless_range_loop,
    clippy::too_many_arguments
)]

/// One entry in the `.pdata` section.  12 bytes.  4-byte aligned.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeFunction {
    /// RVA of the function start (relative to image base).
    pub begin_address: u32,
    /// RVA of the function end (exclusive).
    pub end_address: u32,
    /// RVA of the `UNWIND_INFO` structure.  0 if no unwind data (leaf function).
    pub unwind_data: u32,
}

impl RuntimeFunction {
    pub const SIZE: usize = 12;

    /// Read one entry from a byte slice at `offset`.
    pub fn from_bytes(bytes: &[u8], offset: usize) -> Option<Self> {
        let b = bytes.get(offset..offset + 12)?;
        Some(Self {
            begin_address: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            end_address: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            unwind_data: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
        })
    }

    /// The guest VA of the function start, given the image base.
    #[inline]
    pub fn begin_va(&self, image_base: u64) -> u64 {
        image_base.saturating_add(u64::from(self.begin_address))
    }

    /// The guest VA of the function end (exclusive), given the image base.
    #[inline]
    pub fn end_va(&self, image_base: u64) -> u64 {
        image_base.saturating_add(u64::from(self.end_address))
    }

    /// Whether this entry covers the given guest VA.
    #[inline]
    pub fn covers(&self, va: u64, image_base: u64) -> bool {
        va >= self.begin_va(image_base) && va < self.end_va(image_base)
    }
}

// ── Unwind info ────────────────────────────────────────────────────────

/// Header of the `UNWIND_INFO` structure.  Variable-length: followed by
/// `CountOfCodes` × `UNWIND_CODE` (2 bytes each), optionally padded to
/// 4-byte alignment, then the language-specific handler data if
/// `Flags & (UNW_FLAG_EHANDLER | UNW_FLAG_UHANDLER)` is set
/// (4-byte RVA of handler + 4-byte handler data).
#[derive(Debug, Clone, Copy)]
pub struct UnwindInfo {
    /// Version (should be 1 for x64).
    pub version: u8,
    /// Flags: `UNW_FLAG_NHANDLER` (0), `UNW_FLAG_EHANDLER` (1),
    /// `UNW_FLAG_UHANDLER` (2), `UNW_FLAG_CHAININFO` (4).
    pub flags: u8,
    /// Length of the function prologue in bytes.
    pub size_of_prolog: u8,
    /// Number of `UNWIND_CODE` entries that follow.
    pub count_of_codes: u8,
    /// Nonvolatile register used as frame pointer (0 = none).
    pub frame_register: u8,
    /// Scaled offset from frame register to RSP at function entry.
    pub frame_offset: u8,
}

impl UnwindInfo {
    /// `EXCEPTION_EXECUTE_HANDLER`: this frame has a language-specific handler.
    pub const FLAG_EHANDLER: u8 = 1;
    /// `UNW_FLAG_NHANDLER`: no handler — unwind only.
    pub const FLAG_NHANDLER: u8 = 0;
    /// `UNW_FLAG_UHANDLER`: unwind handler (termination).
    pub const FLAG_UHANDLER: u8 = 2;
    /// `UNW_FLAG_CHAININFO`: this unwind info is followed by another.
    pub const FLAG_CHAININFO: u8 = 4;

    /// Read from bytes at `offset`.
    pub fn from_bytes(bytes: &[u8], offset: usize) -> Option<Self> {
        let b = bytes.get(offset..offset + 4)?;
        Some(Self {
            version: b[0] & 0x07,
            flags: b[0] >> 3,
            size_of_prolog: b[1],
            count_of_codes: b[2],
            frame_register: b[3] & 0x0F,
            frame_offset: (b[3] >> 4) & 0x0F,
        })
    }

    /// Total size of the UNWIND_INFO header + unwind codes (padded to 4 bytes).
    #[inline]
    pub fn header_size(&self) -> usize {
        let codes = usize::from(self.count_of_codes) * 2;
        let unpadded = 4 + codes;
        (unpadded + 3) & !3 // round up to 4
    }

    /// Total size including handler RVA + data if any handler flag is set
    /// (`FLAG_EHANDLER`, `FLAG_UHANDLER`, or both).
    #[inline]
    pub fn total_size(&self) -> usize {
        let base = self.header_size();
        if self.flags & (Self::FLAG_EHANDLER | Self::FLAG_UHANDLER) != 0 {
            base + 8 // handler RVA (4) + handler data (4) per PE/COFF §5.2
        } else {
            base
        }
    }
}

/// One unwind code entry — 2 bytes.
#[derive(Debug, Clone, Copy)]
pub struct UnwindCode {
    /// Offset in the prologue where this operation begins.
    pub code_offset: u8,
    /// `UWOP_*` opcode.
    pub unwind_op: u8,
    /// Operation-specific info (register index for push/save, allocation size bits).
    pub op_info: u8,
}

impl UnwindCode {
    pub const SIZE: usize = 2;

    pub fn from_bytes(bytes: &[u8], offset: usize) -> Option<Self> {
        let b = bytes.get(offset..offset + 2)?;
        Some(Self {
            code_offset: b[0],
            unwind_op: b[1] & 0x0F,
            op_info: (b[1] >> 4) & 0x0F,
        })
    }
}

// ── UWOP opcodes ───────────────────────────────────────────────────────

#[allow(dead_code)]
pub mod uwop {
    pub const PUSH_NONVOL: u8 = 0;
    pub const ALLOC_LARGE: u8 = 1;
    pub const ALLOC_SMALL: u8 = 2;
    pub const SET_FPREG: u8 = 3;
    pub const SAVE_NONVOL: u8 = 4;
    pub const SAVE_NONVOL_FAR: u8 = 5;
    pub const SAVE_XMM128: u8 = 6;
    pub const SAVE_XMM128_FAR: u8 = 7;
    pub const PUSH_MACHFRAME: u8 = 8;
}

// ── Function table lookup ──────────────────────────────────────────────

/// Result of `RtlLookupFunctionEntry`: the found entry + its module base.
#[derive(Debug, Clone, Copy)]
pub struct FunctionEntry<'a> {
    pub entry: &'a RuntimeFunction,
    pub image_base: u64,
}

/// Look up the `RUNTIME_FUNCTION` covering `control_pc` from the registered
/// function tables.  Binary search per-module.
pub fn lookup_function_entry(
    tables: &crate::sync_obj::SyncState,
    control_pc: u64,
) -> Option<FunctionEntry<'_>> {
    for (&image_base, entries) in &tables.function_tables {
        if entries.is_empty() {
            continue;
        }
        let first_va = entries[0].begin_va(image_base);
        let last_va = entries.last()?.end_va(image_base);
        if control_pc < first_va || control_pc >= last_va {
            tracing::debug!(
                control_pc = format_args!("{:#x}", control_pc),
                first_va = format_args!("{:#x}", first_va),
                last_va = format_args!("{:#x}", last_va),
                "lookup: out of range"
            );
            continue;
        }
        let key = (control_pc - image_base) as u32;
        match entries.binary_search_by_key(&key, |e| e.begin_address) {
            Ok(i) => {
                return Some(FunctionEntry {
                    entry: &entries[i],
                    image_base,
                });
            }
            Err(0) => {
                tracing::debug!(key, "lookup: before first entry");
            }
            Err(i) => {
                let candidate = &entries[i - 1];
                let match_rva = control_pc - image_base;
                tracing::debug!(
                    key,
                    candidate_begin = candidate.begin_address,
                    candidate_end = candidate.end_address,
                    "lookup: binary search miss, checking candidate"
                );
                if match_rva < u64::from(candidate.end_address) {
                    return Some(FunctionEntry {
                        entry: candidate,
                        image_base,
                    });
                }
            }
        }
    }
    None
}

/// Parse `.pdata` section bytes into a sorted `Vec<RuntimeFunction>`.
/// Returns an empty vec if the section is empty or malformed.
pub fn parse_pdata(bytes: &[u8]) -> Vec<RuntimeFunction> {
    let count = bytes.len() / RuntimeFunction::SIZE;
    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        if let Some(e) = RuntimeFunction::from_bytes(bytes, i * RuntimeFunction::SIZE) {
            // Skip null sentinel entries (padding at end of .pdata section).
            if e.begin_address == 0 && e.end_address == 0 && e.unwind_data == 0 {
                continue;
            }
            entries.push(e);
        }
    }
    // .pdata is sorted by the linker — already in begin_address order.
    entries
}

// ── Unwind context ─────────────────────────────────────────────────────

/// Simplified register context for stack unwinding.
/// Uses the same GPR indices as the UWOP register encoding (0=RAX..15=R15).
#[derive(Debug, Clone, Copy)]
pub struct UnwindContext {
    pub rip: u64,
    pub rsp: u64,
    pub gpr: [u64; 16],
    /// Nonvolatile XMM registers (XMM6–XMM15).  Indices 0–15; only 6–15
    /// are restored during unwinding.
    pub xmm: [u128; 16],
}

impl UnwindContext {
    /// Register index constants matching UWOP encoding.
    pub const RBP: usize = 5;
    pub const RSI: usize = 6;
    pub const RDI: usize = 7;
    pub const R12: usize = 12;
    pub const R13: usize = 13;
    pub const R14: usize = 14;
    pub const R15: usize = 15;
}

/// Result of one unwind step.
#[derive(Debug, Clone, Copy)]
pub struct UnwindResult {
    /// Context of the caller frame.
    pub ctx: UnwindContext,
    /// Handler RVA (relative to image base) of the language-specific handler, if any.
    pub handler_rva: Option<u32>,
    /// Raw DWORD immediately following the handler RVA in `.xdata`.
    ///
    /// ABI interpretation differs:
    /// - **MSVC**: RVA of language data (`FuncInfo` / scope table) relative to image base.
    /// - **Mingw-w64 SEH**: first 4 bytes of the **embedded** LSDA (not an RVA). See
    ///   [`language_data_candidates`].
    pub handler_data: Option<u32>,
    /// Guest VA of `ExceptionData[]` (first byte after the personality RVA in
    /// `UNWIND_INFO`). Mingw embeds the Itanium LSDA here; MSVC stores FuncInfo RVA.
    pub exception_data_va: Option<u64>,
}

/// Candidate guest VAs for language-specific data (LSDA or FuncInfo).
///
/// Order (clean-room PE/COFF + Mingw SEH practice):
/// 1. `exception_data_va` — embedded LSDA (GCC/Mingw `__gxx_personality_seh0`)
/// 2. `image_base + language_data` — MSVC FuncInfo / scope-table RVA
/// 3. `unwind_va + language_data` and low-16 variant — legacy offset packing
///
/// Duplicates are collapsed while preserving order.
pub fn language_data_candidates(
    image_base: u64,
    unwind_va: u64,
    language_data: u32,
    exception_data_va: Option<u64>,
) -> Vec<u64> {
    let full = u64::from(language_data);
    let mut out = Vec::with_capacity(4);
    let push = |v: &mut Vec<u64>, x: u64| {
        if x != 0 && !v.contains(&x) {
            v.push(x);
        }
    };
    if let Some(ed) = exception_data_va {
        push(&mut out, ed);
    }
    push(&mut out, image_base.saturating_add(full));
    push(&mut out, unwind_va.saturating_add(full));
    push(&mut out, unwind_va.saturating_add(full & 0xffff));
    out
}

mod dwarf;
mod unwind;

pub use dwarf::LandingPadMatch;
pub use unwind::{
    MemRead, ReadError, find_cleanup_landing_pad, find_landing_pad, find_landing_pad_ex,
    virtual_unwind,
};
