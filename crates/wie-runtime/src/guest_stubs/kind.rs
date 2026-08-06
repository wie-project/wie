//! Stub kinds and their classification metadata.

use super::config::GuestStubConfig;
use super::encode::{StubCtx, encode_copy_u64_to_ptr, encode_load_u32_table};

/// Kind of in-guest stub to plant at a fake API VA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum GuestStubKind {
    /// `ret` — void stdcall/win64 return.
    VoidRet,
    /// `mov rax, rcx; ret` — identity pointer (Encode/DecodePointer).
    IdentityRcxToRax,
    /// `xor eax, eax; ret` — return 0 / FALSE / NULL.
    ReturnZero,
    /// `mov eax, imm32; ret` — fixed 32-bit return in RAX zero-extended.
    ReturnImm32(u32),
    /// `mov rax, imm64; ret` — full 64-bit RAX (fits in 16-byte IAT stride).
    ReturnImm64(u64),
    /// `mov rax, imm64; mov eax, [rax]; ret` — load DWORD from fixed guest VA.
    LoadZx32FromVa(u64),
    /// `mov rax, imm64; mov rax, [rax]; ret` — load QWORD from fixed guest VA.
    LoadZx64FromVa(u64),
    /// `mov [rax], ecx; ret` — store DWORD to fixed guest VA.
    StoreEcxToVa(u64),
    /// `*rcx = qword[slot_va]` — copy one u64 table slot through a guest
    /// pointer (`GetSystemTimeAsFileTime`; NULL pointer skipped, void return).
    CopyU64FromVaToRcxPtr { slot_va: u64 },
    /// `*rcx = qword[slot_va]; return 1` (`QueryPerformanceCounter` /
    /// `QueryPerformanceFrequency` — Microsoft returns BOOL TRUE).
    CopyU64FromVaToRcxPtrRetOne { slot_va: u64 },
    /// `FlsGetValue`: index in RCX, table of u64 values at fixed VA.
    FlsGetValue { table_va: u64, max_slots: u32 },
    /// `FlsSetValue`: RCX=index, RDX=value; returns TRUE. OOR → FALSE.
    FlsSetValue { table_va: u64, max_slots: u32 },
    /// `__acrt_iob_func(ix)` → FILE* cookie (stdin/stdout/stderr).
    AcrtIobFunc,
    /// `GetSystemMetrics` / `GetSysColor`: load `u32` from table\[rcx\] if rcx < max.
    LoadU32FromTable { table_va: u64, max_index: u32 },
    /// `GetSysColorBrush`: return `base + color_index` (Microsoft: HBRUSH handle).
    SysColorBrush { base: u64 },
    /// `GetCurrentDirectoryW` — Microsoft Learn buffer / return-value rules.
    GetCurrentDirectoryW { cwd_blob_va: u64 },
    /// `_initterm(first, last)` — call each non-null `void (*)()` in `[first, last)`.
    Initterm,
    /// `_initterm_e(first, last)` — call each non-null `int (*)()`; stop on non-zero.
    InittermE,
    /// `DialogBoxParamA/W` — the modal dialog loop, run entirely in-guest.
    ///
    /// Calls `CreateDialogParamA/W` (host builds the dialog + controls and
    /// sends `WM_INITDIALOG`), then loops `GetMessageA` → `IsDialogMessageA`
    /// → `DispatchMessageA` until `WM_QUIT`, then returns the value stored in
    /// the fixed dialog-result slot. All callees are fake-VA host stops.
    DialogBoxParam {
        /// Fake-VA of `CreateDialogParamA` or `CreateDialogParamW`.
        create_dialog_param_va: u64,
        /// Fake-VA of `GetMessageA`.
        get_message_va: u64,
        /// Fake-VA of `IsDialogMessageA`.
        is_dialog_message_va: u64,
        /// Fake-VA of `DispatchMessageA`.
        dispatch_message_va: u64,
        /// Guest VA of the `u32` dialog-result slot.
        dialog_result_va: u64,
    },
}

impl GuestStubKind {
    /// Encodes the stub body. Most stubs fit the 16-byte IAT stride; longer ones
    /// are planted outside the hooked range with a 12-byte `jmp` at the IAT.
    #[must_use]
    pub(crate) fn encode(self, cfg: &GuestStubConfig) -> Vec<u8> {
        match self {
            Self::VoidRet => vec![0xc3],
            Self::IdentityRcxToRax => vec![0x48, 0x89, 0xc8, 0xc3],
            Self::ReturnZero => vec![0x31, 0xc0, 0xc3],
            Self::ReturnImm32(imm) => {
                let mut buf = vec![0xb8, 0, 0, 0, 0, 0xc3];
                buf[1..5].copy_from_slice(&imm.to_le_bytes());
                buf
            }
            Self::ReturnImm64(imm) => {
                let mut buf = vec![0x48, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0xc3];
                buf[2..10].copy_from_slice(&imm.to_le_bytes());
                buf
            }
            Self::LoadZx32FromVa(va) => {
                let mut buf = vec![0x48, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0x8b, 0x00, 0xc3];
                buf[2..10].copy_from_slice(&va.to_le_bytes());
                buf
            }
            Self::LoadZx64FromVa(va) => {
                // mov rax, imm64 ; mov rax, [rax] ; ret (14 bytes — fits IAT stride)
                let mut buf = vec![0x48, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0x48, 0x8b, 0x00, 0xc3];
                buf[2..10].copy_from_slice(&va.to_le_bytes());
                buf
            }
            Self::StoreEcxToVa(va) => {
                let mut buf = vec![0x48, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0x89, 0x08, 0xc3];
                buf[2..10].copy_from_slice(&va.to_le_bytes());
                buf
            }
            Self::CopyU64FromVaToRcxPtr { slot_va } => encode_copy_u64_to_ptr(slot_va, false),
            Self::CopyU64FromVaToRcxPtrRetOne { slot_va } => encode_copy_u64_to_ptr(slot_va, true),
            Self::FlsGetValue { max_slots, .. } => {
                encode_with(cfg, |ctx| ctx.encode_fls_get(max_slots))
            }
            Self::FlsSetValue { max_slots, .. } => {
                encode_with(cfg, |ctx| ctx.encode_fls_set(max_slots))
            }
            Self::AcrtIobFunc => {
                // mov eax, 0x68000000 ; shl ecx, 8 ; add eax, ecx ; ret
                let mut buf = vec![0xb8, 0x00, 0x00, 0x00, 0x68];
                buf.extend_from_slice(&[0xc1, 0xe1, 0x08]);
                buf.extend_from_slice(&[0x01, 0xc8]);
                buf.push(0xc3);
                buf
            }
            Self::LoadU32FromTable {
                table_va,
                max_index,
            } => encode_load_u32_table(table_va, max_index),
            Self::SysColorBrush { base } => {
                // mov rax, base ; add rax, rcx ; ret  (14 bytes — fits IAT stride)
                let mut buf = vec![0x48, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0];
                buf[2..10].copy_from_slice(&base.to_le_bytes());
                buf.extend_from_slice(&[0x48, 0x01, 0xc8, 0xc3]);
                buf
            }
            Self::GetCurrentDirectoryW { .. } => {
                encode_with(cfg, |ctx| ctx.encode_get_current_directory_w())
            }
            Self::Initterm => encode_with(cfg, |ctx| ctx.encode_initterm(false)),
            Self::InittermE => encode_with(cfg, |ctx| ctx.encode_initterm(true)),
            Self::DialogBoxParam {
                create_dialog_param_va,
                ..
            } => encode_with(cfg, |ctx| {
                ctx.encode_dialog_box_param(create_dialog_param_va)
            }),
        }
    }

    /// Whether the body must be planted outside the IAT slot (jmp trampoline at entry).
    ///
    /// Uses an **exhaustive `match`** — every variant must be listed.  When a new
    /// variant is added to [`GuestStubKind`], the compiler forces the developer
    /// to consider whether it needs an out-of-line helper.
    #[must_use]
    pub(crate) fn needs_out_of_line_helper(self) -> bool {
        // Exhaustive match: every variant must be listed.  No catch-all `_` arm.
        match self {
            Self::VoidRet => false,
            Self::IdentityRcxToRax => false,
            Self::ReturnZero => false,
            Self::ReturnImm32(_) => false,
            Self::ReturnImm64(_) => false,
            Self::LoadZx32FromVa(_) => false,
            Self::LoadZx64FromVa(_) => false,
            Self::StoreEcxToVa(_) => false,
            Self::CopyU64FromVaToRcxPtr { .. } => true,
            Self::CopyU64FromVaToRcxPtrRetOne { .. } => true,
            Self::FlsGetValue { .. } => true,
            Self::FlsSetValue { .. } => true,
            Self::AcrtIobFunc => false,
            Self::LoadU32FromTable { .. } => true,
            Self::SysColorBrush { .. } => false,
            Self::GetCurrentDirectoryW { .. } => true,
            Self::Initterm => true,
            Self::InittermE => true,
            Self::DialogBoxParam { .. } => true,
        }
    }

    /// Whether this kind embeds a guest address from the runtime config.
    ///
    /// When `true`, the cached `stub_kind` from `make_entry` (computed with
    /// `CLASSIFY_ONLY` — all VAs zero) must be re-derived with the real config
    /// before its body can be encoded.  Simple stubs (`VoidRet`, `ReturnZero`,
    /// …) never need re-classification.
    ///
    /// Uses an **exhaustive `match`** — every variant must be listed.  When a new
    /// variant is added to [`GuestStubKind`], the compiler forces the developer
    /// to choose whether it needs re-classification.  There is no catch-all
    /// `_` arm, so forgetting is a compile error, not a runtime bug.
    #[must_use]
    pub(crate) fn needs_real_guest_addresses(&self) -> bool {
        // Exhaustive match: every variant must be listed.  No catch-all `_` arm.
        match self {
            Self::VoidRet => false,
            Self::IdentityRcxToRax => false,
            Self::ReturnZero => false,
            Self::ReturnImm32(_) => false,
            Self::ReturnImm64(_) => true, // GetCommandLineA/W, GetProcessHeap, GetDesktopWindow
            Self::LoadZx32FromVa(_) => true, // TEB_LAST_ERROR_VA (constant, but safe to re-classify)
            Self::LoadZx64FromVa(_) => true, // clock-table slot VA (config-derived)
            Self::StoreEcxToVa(_) => true,   // TEB_LAST_ERROR_VA (same)
            Self::CopyU64FromVaToRcxPtr { .. } => true, // clock-table slot VA
            Self::CopyU64FromVaToRcxPtrRetOne { .. } => true, // clock-table slot VA
            Self::FlsGetValue { .. } => true,
            Self::FlsSetValue { .. } => true,
            Self::AcrtIobFunc => false,
            Self::LoadU32FromTable { .. } => true,
            Self::SysColorBrush { .. } => true, // FAKE_SYSCOLOR_BRUSH_BASE constant — harmless re-classify
            Self::GetCurrentDirectoryW { .. } => true,
            Self::Initterm => false,
            Self::InittermE => false,
            Self::DialogBoxParam { .. } => true,
        }
    }
}

/// Encodes a [`StubCtx`]-based stub body into a fresh buffer.
///
/// The ctx-based encoders append into the buffer they hold, so a caller that
/// wants a self-contained `Vec<u8>` body (plant-time, tests) hands the config
/// in and takes the buffer back.
fn encode_with(cfg: &GuestStubConfig, encode: impl FnOnce(&mut StubCtx<'_>)) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut ctx = StubCtx::new(&mut buf, cfg);
    encode(&mut ctx);
    buf
}
