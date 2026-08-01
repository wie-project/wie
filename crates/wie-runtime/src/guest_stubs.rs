//! In-guest machine-code stubs for trivial WinAPI entries.
//!
//! These run entirely inside the guest without a host stop when the code hook
//! treats their instruction bytes as passthrough (see `install_runtime_hooks`).
//!
//! # Correctness policy (Microsoft Learn)
//!
//! Only plant stubs when the in-guest body can honour the documented API contract
//! for the subset of behaviour WIE models (fixed guest environment, published
//! guest memory). **Do not** accelerate APIs with simplified “always success”
//! answers that diverge from Learn (e.g. `VirtualProtect` with NULL
//! `lpflOldProtect` must fail; `VirtualQuery` must describe real regions —
//! those stay on the host until RegionTable-backed handlers exist).
//!
//! `LocalAlloc` / `GlobalAlloc` with `LMEM_MOVEABLE` / lock semantics also stay
//! on the host — a thin `HeapAlloc` wrapper would break real apps.

use anyhow::{Context, Result};

/// Guest-visible layout for Phase 5 data-backed stubs.
///
/// Offsets within `data_base` (must match session init):
/// - `0x000`: metrics `u32[METRICS_COUNT]`
/// - `0x400`: colors `u32[COLOR_COUNT]`
/// - `0x500`: cwd blob — `u32 char_count` + UTF-16 path with NUL
#[derive(Debug, Clone, Copy)]
pub(crate) struct GuestStubConfig {
    pub fls_table_va: u64,
    pub metrics_table_va: u64,
    pub colors_table_va: u64,
    pub cwd_blob_va: u64,
    /// `GetCommandLineA` buffer (ANSI, NUL-terminated) in env data page.
    pub command_line_a_va: u64,
    /// `GetCommandLineW` buffer (UTF-16, NUL-terminated) in env data page.
    pub command_line_w_va: u64,
    /// Fake-VA of `CreateDialogParamA` (modal-dialog stub callee).
    pub create_dialog_param_a_va: u64,
    /// Fake-VA of `CreateDialogParamW` (modal-dialog stub callee).
    pub create_dialog_param_w_va: u64,
    /// Fake-VA of `GetMessageA` (modal-dialog stub callee).
    pub get_message_a_va: u64,
    /// Fake-VA of `IsDialogMessageA` (modal-dialog stub callee).
    pub is_dialog_message_a_va: u64,
    /// Fake-VA of `DispatchMessageA` (modal-dialog stub callee).
    pub dispatch_message_a_va: u64,
    /// Guest VA of the `u32` dialog-result slot (`EndDialog` writes, the
    /// `DialogBoxParam` stub reads after `WM_QUIT`).
    pub dialog_result_va: u64,
    /// Guest VA of the host-written clock table (B5): 6 × u64 slots refreshed
    /// on every host stop; in-guest clock stubs read it with no host stop.
    pub clock_table_va: u64,
}

/// Offset of the dialog-result slot inside the guest stub data page.
pub(crate) const DIALOG_RESULT_OFFSET: u64 = 0x1000;

/// Slot offsets into the 6×u64 guest clock table (B5).
///
/// Must match `wie_winapi::kernel32::clock::clock_table_values` slot order:
/// - `[0]` tick_count u32 — `GetTickCount`
/// - `[1]` tick_count_64 — `GetTickCount64`
/// - `[2]` time_get_time u32 — `timeGetTime`
/// - `[3]` system_time_as_filetime — `GetSystemTimeAsFileTime`
/// - `[4]` qpc_counter — `QueryPerformanceCounter`
/// - `[5]` qpc_frequency — `QueryPerformanceFrequency`
pub(crate) const CLOCK_TABLE_SLOT_TICK: u64 = 0;
pub(crate) const CLOCK_TABLE_SLOT_TICK64: u64 = 8;
pub(crate) const CLOCK_TABLE_SLOT_TIME: u64 = 16;
pub(crate) const CLOCK_TABLE_SLOT_FILETIME: u64 = 24;
pub(crate) const CLOCK_TABLE_SLOT_QPC: u64 = 32;
pub(crate) const CLOCK_TABLE_SLOT_QPC_FREQ: u64 = 40;
/// Total clock-table size in bytes (6 × u64).
pub(crate) const CLOCK_TABLE_SIZE: usize = 48;

impl GuestStubConfig {
    /// Placeholder VAs for trait classification only (`is_some()`).
    pub(crate) const CLASSIFY_ONLY: Self = Self {
        fls_table_va: 0,
        metrics_table_va: 0,
        colors_table_va: 0,
        cwd_blob_va: 0,
        command_line_a_va: 0,
        command_line_w_va: 0,
        create_dialog_param_a_va: 0,
        create_dialog_param_w_va: 0,
        get_message_a_va: 0,
        is_dialog_message_a_va: 0,
        dispatch_message_a_va: 0,
        dialog_result_va: 0,
        clock_table_va: 0,
    };

    #[must_use]
    pub(crate) fn from_layout(layout: &crate::memory::RuntimeMemoryLayout) -> Self {
        let base = layout.guest_stub_data_base;
        Self {
            fls_table_va: layout.guest_fls_table_base,
            metrics_table_va: base,
            colors_table_va: base + OFFSET_COLORS,
            cwd_blob_va: base + OFFSET_CWD,
            command_line_a_va: layout.env_data_base + 0x100,
            command_line_w_va: layout.env_data_base + 0x200,
            // The modal-loop callees are WinApiIds with deterministic dense
            // fake VAs; they resolve even when the guest never imported them
            // (the stop bitmap defaults to host-stop everywhere).
            create_dialog_param_a_va: wie_winapi::encode_export(
                wie_winapi::WinApiId::User32Createdialogparama,
            ),
            create_dialog_param_w_va: wie_winapi::encode_export(
                wie_winapi::WinApiId::User32Createdialogparamw,
            ),
            get_message_a_va: wie_winapi::encode_export(wie_winapi::WinApiId::User32Getmessagea),
            is_dialog_message_a_va: wie_winapi::encode_export(
                wie_winapi::WinApiId::User32Isdialogmessagea,
            ),
            dispatch_message_a_va: wie_winapi::encode_export(
                wie_winapi::WinApiId::User32Dispatchmessagea,
            ),
            dialog_result_va: base + DIALOG_RESULT_OFFSET,
            clock_table_va: layout.clock_table_va,
        }
    }
}

/// Metrics table length (SM_* indices fit in a byte for common queries).
pub const METRICS_COUNT: usize = 256;
/// SysColor table length (COLOR_* indices used by host handler).
pub const COLOR_COUNT: usize = 32;
/// Max UTF-16 code units stored for cwd (excluding NUL).
pub const CWD_MAX_CHARS: usize = 260;

const OFFSET_COLORS: u64 = 0x400;
const OFFSET_CWD: u64 = 0x500;
/// Bytes: u32 count + (CWD_MAX_CHARS+1) * u16
pub const CWD_BLOB_SIZE: usize = 4 + (CWD_MAX_CHARS + 1) * 2;

/// LANGID for en-US (Microsoft Learn primary language + sublanguage).
const LANG_EN_US: u32 = 0x0409;
/// Fake desktop HWND (matches `wie_winapi::user32` FAKE_DESKTOP_WINDOW_HANDLE).
const FAKE_DESKTOP_WINDOW: u64 = 0x0000_0000_6600_0110;
/// Fake system-color brush base (matches user32).
const FAKE_SYSCOLOR_BRUSH_BASE: u64 = 0x0000_0000_6601_0000;

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
    pub(crate) fn encode(self) -> Vec<u8> {
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
            Self::FlsGetValue {
                table_va,
                max_slots,
            } => encode_fls_get(table_va, max_slots),
            Self::FlsSetValue {
                table_va,
                max_slots,
            } => encode_fls_set(table_va, max_slots),
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
            Self::GetCurrentDirectoryW { cwd_blob_va } => {
                encode_get_current_directory_w(cwd_blob_va)
            }
            Self::Initterm => encode_initterm(false),
            Self::InittermE => encode_initterm(true),
            Self::DialogBoxParam {
                create_dialog_param_va,
                get_message_va,
                is_dialog_message_va,
                dispatch_message_va,
                dialog_result_va,
            } => encode_dialog_box_param(
                create_dialog_param_va,
                get_message_va,
                is_dialog_message_va,
                dispatch_message_va,
                dialog_result_va,
            ),
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

/// `_initterm` / `_initterm_e` body — iterate function-pointer range and call.
///
/// Win64: `RCX=first`, `RDX=last` (half-open). Each entry is `void (*)()` or
/// `int (*)()`; NULL entries are skipped. For `_initterm_e`, a non-zero return
/// aborts the loop and is returned to the caller.
fn encode_initterm(check_status: bool) -> Vec<u8> {
    // push rbx; push rsi
    // mov rbx, rcx          ; cur
    // mov rsi, rdx          ; end
    // .loop:
    //   cmp rbx, rsi
    //   jae .done
    //   mov rax, qword ptr [rbx]
    //   add rbx, 8
    //   test rax, rax
    //   jz .loop
    //   sub rsp, 0x28
    //   call rax
    //   add rsp, 0x28
    //   ; if check_status: test eax,eax; jnz .fail_ret
    //   jmp .loop
    // .done:
    //   xor eax, eax
    //   pop rsi; pop rbx; ret
    // .fail_ret: (initterm_e only)
    //   pop rsi; pop rbx; ret   ; eax already holds status
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(&[0x53, 0x56]); // push rbx; push rsi
    buf.extend_from_slice(&[0x48, 0x89, 0xcb]); // mov rbx, rcx
    buf.extend_from_slice(&[0x48, 0x89, 0xd6]); // mov rsi, rdx
    let loop_at = buf.len();
    buf.extend_from_slice(&[0x48, 0x39, 0xf3]); // cmp rbx, rsi
    let jae_imm = buf.len() + 1;
    buf.extend_from_slice(&[0x73, 0x00]); // jae .done (patch)
    buf.extend_from_slice(&[0x48, 0x8b, 0x03]); // mov rax, [rbx]
    buf.extend_from_slice(&[0x48, 0x83, 0xc3, 0x08]); // add rbx, 8
    buf.extend_from_slice(&[0x48, 0x85, 0xc0]); // test rax, rax
    let jz_imm = buf.len() + 1;
    buf.extend_from_slice(&[0x74, 0x00]); // jz .loop (patch)
    buf.extend_from_slice(&[0x48, 0x83, 0xec, 0x28]); // sub rsp, 0x28
    buf.extend_from_slice(&[0xff, 0xd0]); // call rax
    buf.extend_from_slice(&[0x48, 0x83, 0xc4, 0x28]); // add rsp, 0x28
    if check_status {
        buf.extend_from_slice(&[0x85, 0xc0]); // test eax, eax
        let jnz_imm = buf.len() + 1;
        buf.extend_from_slice(&[0x75, 0x00]); // jnz .fail_ret (patch)
        // jmp .loop
        let jmp_imm = buf.len() + 1;
        buf.extend_from_slice(&[0xeb, 0x00]);
        let done_at = buf.len();
        buf.extend_from_slice(&[0x31, 0xc0]); // xor eax, eax
        buf.extend_from_slice(&[0x5e, 0x5b, 0xc3]); // pop rsi; pop rbx; ret
        let fail_at = buf.len();
        buf.extend_from_slice(&[0x5e, 0x5b, 0xc3]); // pop rsi; pop rbx; ret (keep eax)
        patch_rel8(&mut buf, jae_imm, done_at);
        patch_rel8(&mut buf, jz_imm, loop_at);
        patch_rel8(&mut buf, jnz_imm, fail_at);
        patch_rel8(&mut buf, jmp_imm, loop_at);
    } else {
        // jmp .loop
        let jmp_imm = buf.len() + 1;
        buf.extend_from_slice(&[0xeb, 0x00]);
        let done_at = buf.len();
        buf.extend_from_slice(&[0x31, 0xc0]); // xor eax, eax
        buf.extend_from_slice(&[0x5e, 0x5b, 0xc3]); // pop rsi; pop rbx; ret
        patch_rel8(&mut buf, jae_imm, done_at);
        patch_rel8(&mut buf, jz_imm, loop_at);
        patch_rel8(&mut buf, jmp_imm, loop_at);
    }
    buf
}

/// `DialogBoxParamA/W` modal-loop body (out-of-line helper).
///
/// Win64 entry: `RCX=hInstance, RDX=lpTemplateName, R8=hWndParent,
/// R9=lpDialogFunc, [rsp+0x28]=dwInitParam`.
///
/// ```text
/// push rbx; sub rsp, 0x60     ; rbx = alignment pad (preserved)
/// mov rax, [rsp+0x90]        ; dwInitParam (caller's 5th arg)
/// mov [rsp+0x28], rax        ; 5th arg slot for CreateDialogParam
/// call CreateDialogParamA/W  ; host: build dialog, WM_INITDIALOG bridge
/// test rax, rax; jnz .created
///   mov rax, -1; jmp .done   ; creation failed → DialogBoxParam returns -1
/// .created:
/// mov [rsp+0x20], rax        ; hwnd lives in OUR frame — the WM_INITDIALOG
///                            ; guest callback clobbers every register, and
///                            ; its frame sits BELOW ours, so the stack slot
///                            ; survives
/// .loop:
///   lea rcx, [rsp+0x28]      ; lpMsg
///   xor edx, edx             ; hWnd = NULL — real modal loops pull every
///                            ; thread message (the owner's WM_TIMER must
///                            ; still be dispatched inside the dialog)
///   xor r8d, r8d; xor r9d, r9d
///   call GetMessageA
///   test eax, eax; jz .quit
///   mov rcx, [rsp+0x20]; lea rdx, [rsp+0x28]
///   call IsDialogMessageA
///   test eax, eax; jnz .loop ; consumed (Tab/Enter/Esc) → keep going
///   lea rcx, [rsp+0x28]
///   call DispatchMessageA
///   jmp .loop
/// .quit:
/// mov rax, dialog_result_va  ; EndDialog wrote the result here
/// mov eax, [rax]
/// .done:
/// add rsp, 0x60; pop rbx; ret
/// ```
///
/// `[rsp+0x90]` = the caller's 5th arg: entry `rsp` = R0, after `push rbx`
/// (8) + `sub rsp, 0x60` the frame is at R0-0x68, and the caller's stack arg
/// sits at `[R0+0x28]` = `[rsp+0x68+0x28]` = `[rsp+0x90]`.
fn encode_dialog_box_param(
    create_dialog_param_va: u64,
    get_message_va: u64,
    is_dialog_message_va: u64,
    dispatch_message_va: u64,
    dialog_result_va: u64,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(180);
    // push rbx ; sub rsp, 0x60
    buf.extend_from_slice(&[0x53]);
    buf.extend_from_slice(&[0x48, 0x83, 0xec, 0x60]);
    // mov rax, [rsp+0x90] ; mov [rsp+0x28], rax
    buf.extend_from_slice(&[0x48, 0x8b, 0x84, 0x24, 0x90, 0x00, 0x00, 0x00]);
    buf.extend_from_slice(&[0x48, 0x89, 0x44, 0x24, 0x28]);
    // call CreateDialogParamA/W
    buf.extend_from_slice(&[0x48, 0xb8]);
    buf.extend_from_slice(&create_dialog_param_va.to_le_bytes());
    buf.extend_from_slice(&[0xff, 0xd0]);
    // test rax, rax ; jnz .created
    buf.extend_from_slice(&[0x48, 0x85, 0xc0]);
    let jnz_created = buf.len() + 1;
    buf.extend_from_slice(&[0x75, 0x00]);
    // mov rax, -1 ; jmp .done
    buf.extend_from_slice(&[0x48, 0xc7, 0xc0, 0xff, 0xff, 0xff, 0xff]);
    let jmp_done = buf.len() + 1;
    buf.extend_from_slice(&[0xeb, 0x00]);
    // .created: mov [rsp+0x20], rax  (hwnd slot)
    let created_at = buf.len();
    buf.extend_from_slice(&[0x48, 0x89, 0x44, 0x24, 0x20]);
    // .loop:
    let loop_at = buf.len();
    // lea rcx, [rsp+0x28] ; xor edx, edx ; xor r8d, r8d ; xor r9d, r9d
    buf.extend_from_slice(&[0x48, 0x8d, 0x4c, 0x24, 0x28]);
    buf.extend_from_slice(&[0x31, 0xd2]);
    buf.extend_from_slice(&[0x45, 0x31, 0xc0]);
    buf.extend_from_slice(&[0x45, 0x31, 0xc9]);
    // call GetMessageA
    buf.extend_from_slice(&[0x48, 0xb8]);
    buf.extend_from_slice(&get_message_va.to_le_bytes());
    buf.extend_from_slice(&[0xff, 0xd0]);
    // test eax, eax ; jz .quit
    buf.extend_from_slice(&[0x85, 0xc0]);
    let jz_quit = buf.len() + 1;
    buf.extend_from_slice(&[0x74, 0x00]);
    // mov rcx, [rsp+0x20] ; lea rdx, [rsp+0x28] ; call IsDialogMessageA
    buf.extend_from_slice(&[0x48, 0x8b, 0x4c, 0x24, 0x20]);
    buf.extend_from_slice(&[0x48, 0x8d, 0x54, 0x24, 0x28]);
    buf.extend_from_slice(&[0x48, 0xb8]);
    buf.extend_from_slice(&is_dialog_message_va.to_le_bytes());
    buf.extend_from_slice(&[0xff, 0xd0]);
    // test eax, eax ; jnz .loop (consumed)
    buf.extend_from_slice(&[0x85, 0xc0]);
    let jnz_loop = buf.len() + 1;
    buf.extend_from_slice(&[0x75, 0x00]);
    // lea rcx, [rsp+0x28] ; call DispatchMessageA ; jmp .loop
    buf.extend_from_slice(&[0x48, 0x8d, 0x4c, 0x24, 0x28]);
    buf.extend_from_slice(&[0x48, 0xb8]);
    buf.extend_from_slice(&dispatch_message_va.to_le_bytes());
    buf.extend_from_slice(&[0xff, 0xd0]);
    let jmp_loop = buf.len() + 1;
    buf.extend_from_slice(&[0xeb, 0x00]);
    // .quit: mov rax, dialog_result_va ; mov eax, [rax]
    let quit_at = buf.len();
    buf.extend_from_slice(&[0x48, 0xb8]);
    buf.extend_from_slice(&dialog_result_va.to_le_bytes());
    buf.extend_from_slice(&[0x8b, 0x00]);
    // .done: add rsp, 0x60 ; pop rbx ; ret
    let done_at = buf.len();
    buf.extend_from_slice(&[0x48, 0x83, 0xc4, 0x60]);
    buf.push(0x5b);
    buf.push(0xc3);

    patch_rel8(&mut buf, jnz_created, created_at);
    patch_rel8(&mut buf, jmp_done, done_at);
    patch_rel8(&mut buf, jz_quit, quit_at);
    patch_rel8(&mut buf, jnz_loop, loop_at);
    patch_rel8(&mut buf, jmp_loop, loop_at);
    buf
}

fn encode_fls_get(table_va: u64, max_slots: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32);
    buf.extend_from_slice(&[0x48, 0x81, 0xf9]);
    buf.extend_from_slice(&max_slots.to_le_bytes());
    let jae_imm = buf.len() + 1;
    buf.extend_from_slice(&[0x73, 0x00]);
    buf.extend_from_slice(&[0x48, 0xb8]);
    buf.extend_from_slice(&table_va.to_le_bytes());
    buf.extend_from_slice(&[0x48, 0x8b, 0x04, 0xc8]);
    buf.push(0xc3);
    let zero_at = buf.len();
    buf.extend_from_slice(&[0x31, 0xc0, 0xc3]);
    patch_rel8(&mut buf, jae_imm, zero_at);
    buf
}

fn encode_fls_set(table_va: u64, max_slots: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(40);
    buf.extend_from_slice(&[0x48, 0x81, 0xf9]);
    buf.extend_from_slice(&max_slots.to_le_bytes());
    let jae_imm = buf.len() + 1;
    buf.extend_from_slice(&[0x73, 0x00]);
    buf.extend_from_slice(&[0x48, 0xb8]);
    buf.extend_from_slice(&table_va.to_le_bytes());
    buf.extend_from_slice(&[0x48, 0x89, 0x14, 0xc8]);
    buf.extend_from_slice(&[0xb8, 0x01, 0x00, 0x00, 0x00]);
    buf.push(0xc3);
    let fail_at = buf.len();
    buf.extend_from_slice(&[0x31, 0xc0, 0xc3]);
    patch_rel8(&mut buf, jae_imm, fail_at);
    buf
}

fn encode_load_u32_table(table_va: u64, max_index: u32) -> Vec<u8> {
    // cmp rcx, max ; jae .zero
    // mov rax, table ; mov eax, [rax+rcx*4] ; ret
    // .zero: xor eax,eax ; ret
    let mut buf = Vec::with_capacity(32);
    buf.extend_from_slice(&[0x48, 0x81, 0xf9]);
    buf.extend_from_slice(&max_index.to_le_bytes());
    let jae_imm = buf.len() + 1;
    buf.extend_from_slice(&[0x73, 0x00]);
    buf.extend_from_slice(&[0x48, 0xb8]);
    buf.extend_from_slice(&table_va.to_le_bytes());
    buf.extend_from_slice(&[0x8b, 0x04, 0x88]); // mov eax, [rax+rcx*4]
    buf.push(0xc3);
    let zero_at = buf.len();
    buf.extend_from_slice(&[0x31, 0xc0, 0xc3]);
    patch_rel8(&mut buf, jae_imm, zero_at);
    buf
}

/// `GetSystemTimeAsFileTime` / `QueryPerformanceCounter` body: copy one u64
/// clock-table slot through the guest pointer in RCX (out-of-line helper).
///
/// ```text
/// mov rax, slot_va       ; mov rdx, [rax]  ; load the table slot
/// test rcx, rcx          ; NULL pointer → skip the store (host semantics)
/// jz  .skip
/// mov [rcx], rdx
/// .skip: (mov eax, 1 ;) ret     ; ret_one → QPC/QPF returns TRUE
/// ```
fn encode_copy_u64_to_ptr(slot_va: u64, ret_one: bool) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32);
    buf.extend_from_slice(&[0x48, 0xb8]); // mov rax, imm64
    buf.extend_from_slice(&slot_va.to_le_bytes());
    buf.extend_from_slice(&[0x48, 0x8b, 0x10]); // mov rdx, [rax]
    buf.extend_from_slice(&[0x48, 0x85, 0xc9]); // test rcx, rcx
    let jz_imm = buf.len() + 1;
    buf.extend_from_slice(&[0x74, 0x00]); // jz .skip (patched)
    buf.extend_from_slice(&[0x48, 0x89, 0x11]); // mov [rcx], rdx
    let skip_at = buf.len();
    if ret_one {
        buf.extend_from_slice(&[0xb8, 0x01, 0x00, 0x00, 0x00]); // mov eax, 1
    }
    buf.push(0xc3);
    patch_rel8(&mut buf, jz_imm, skip_at);
    buf
}

/// `GetCurrentDirectoryW` per Microsoft Learn:
/// - success: return chars written **excluding** NUL
/// - buffer too small / size query: return required size **including** NUL
/// - size query: `nBufferLength == 0` and `lpBuffer == NULL` (we also treat
///   null buffer as size query, matching common app patterns and host)
fn encode_get_current_directory_w(cwd_blob_va: u64) -> Vec<u8> {
    // Layout: [u32 char_count][u16 path...][u16 0]
    // RCX = nBufferLength, RDX = lpBuffer
    //
    // mov r8, cwd_blob
    // mov eax, [r8]              ; char_count
    // lea r9d, [eax+1]           ; required_with_nul
    // test rdx, rdx
    // jz .need_size
    // test rcx, rcx
    // jz .need_size
    // cmp rcx, rax               ; need length > char_count (room for NUL)
    // jbe .need_size
    // ; copy (char_count+1) UTF-16 units to [rdx]
    // push rsi / rdi
    // lea rsi, [r8+4]
    // mov rdi, rdx
    // lea ecx, [eax+1]
    // rep movsw
    // pop rdi / rsi
    // ; eax still char_count (rep uses ecx only)
    // ret
    // .need_size: mov eax, r9d ; ret
    let mut buf = Vec::with_capacity(80);
    buf.extend_from_slice(&[0x49, 0xb8]); // mov r8, imm64
    buf.extend_from_slice(&cwd_blob_va.to_le_bytes());
    buf.extend_from_slice(&[0x41, 0x8b, 0x00]); // mov eax, [r8]
    buf.extend_from_slice(&[0x44, 0x8d, 0x48, 0x01]); // lea r9d, [rax+1]
    buf.extend_from_slice(&[0x48, 0x85, 0xd2]); // test rdx, rdx
    let jz1 = buf.len() + 1;
    buf.extend_from_slice(&[0x74, 0x00]); // jz .need_size
    buf.extend_from_slice(&[0x48, 0x85, 0xc9]); // test rcx, rcx
    let jz2 = buf.len() + 1;
    buf.extend_from_slice(&[0x74, 0x00]);
    buf.extend_from_slice(&[0x48, 0x39, 0xc1]); // cmp rcx, rax
    let jbe = buf.len() + 1;
    buf.extend_from_slice(&[0x76, 0x00]); // jbe .need_size
    // preserve RSI/RDI (Win64 non-volatiles)
    buf.extend_from_slice(&[0x56, 0x57]); // push rsi, rdi
    buf.extend_from_slice(&[0x49, 0x8d, 0x70, 0x04]); // lea rsi, [r8+4]
    buf.extend_from_slice(&[0x48, 0x89, 0xd7]); // mov rdi, rdx
    buf.extend_from_slice(&[0x8d, 0x48, 0x01]); // lea ecx, [rax+1]
    buf.extend_from_slice(&[0xf3, 0xa5]); // rep movsw
    buf.extend_from_slice(&[0x5f, 0x5e]); // pop rdi, rsi
    // restore eax = char_count (destroyed by lea ecx if we only used eax — eax intact)
    buf.push(0xc3);
    let need_size = buf.len();
    buf.extend_from_slice(&[0x44, 0x89, 0xc8]); // mov eax, r9d
    buf.push(0xc3);
    patch_rel8(&mut buf, jz1, need_size);
    patch_rel8(&mut buf, jz2, need_size);
    patch_rel8(&mut buf, jbe, need_size);
    buf
}

fn patch_rel8(buf: &mut [u8], imm_at: usize, target: usize) {
    let next_ip = imm_at + 1;
    let rel = target as isize - next_ip as isize;
    debug_assert!((-128..128).contains(&rel), "rel8 out of range: {rel}");
    if let Some(slot) = buf.get_mut(imm_at) {
        *slot = rel as i8 as u8;
    }
}

/// x64 TEB.LastErrorValue offset (also used as our guest mirror VA when TEB base is 0).
pub const TEB_LAST_ERROR_VA: u64 = wie_cpu::GS_BASE + 0x68;

/// Guest FLS table slot count (index 0..N-1 accelerated).
pub const GUEST_FLS_SLOT_COUNT: u32 = 256;

/// Classify a library/export as an in-guest stub when safe under Microsoft Learn.
#[must_use]
pub(crate) fn classify_guest_stub(
    library: &str,
    name: &str,
    cfg: &GuestStubConfig,
) -> Option<GuestStubKind> {
    // UCRT pure helpers: cover indirect `call reg` paths that miss the JIT near-call
    // fast path (still no host-stop). Matches FILE* cookies / CRT slots in `wie_winapi::ucrt`.
    if wie_winapi::ucrt::is_ucrt_library(library) {
        const CRT: u64 = 0x0000_0000_6800_0000;
        let n = name;
        if n.eq_ignore_ascii_case("__acrt_iob_func") {
            return Some(GuestStubKind::AcrtIobFunc);
        }
        // `_initterm` / `_initterm_e` must invoke guest constructor tables.
        if n.eq_ignore_ascii_case("_initterm") {
            return Some(GuestStubKind::Initterm);
        }
        if n.eq_ignore_ascii_case("_initterm_e") {
            return Some(GuestStubKind::InittermE);
        }
        // No-op / fixed-success CRT init (host handlers only returned 0).
        if n.eq_ignore_ascii_case("fflush")
            || n.eq_ignore_ascii_case("setvbuf")
            || n.eq_ignore_ascii_case("_crt_atexit")
            || n.eq_ignore_ascii_case("_set_invalid_parameter_handler")
            || n.eq_ignore_ascii_case("_set_app_type")
            || n.eq_ignore_ascii_case("_set_new_mode")
            || n.eq_ignore_ascii_case("_configure_narrow_argv")
            || n.eq_ignore_ascii_case("_initialize_narrow_environment")
            || n.eq_ignore_ascii_case("__setusermatherr")
            || n.eq_ignore_ascii_case("_configthreadlocale")
            || n.eq_ignore_ascii_case("_cexit")
            || n.eq_ignore_ascii_case("signal")
        {
            return Some(GuestStubKind::ReturnZero);
        }
        if n.eq_ignore_ascii_case("__p__environ") {
            return Some(GuestStubKind::ReturnImm64(CRT + 0x300));
        }
        if n.eq_ignore_ascii_case("__p___argv") {
            return Some(GuestStubKind::ReturnImm64(CRT + 0x308));
        }
        if n.eq_ignore_ascii_case("__p___argc") {
            return Some(GuestStubKind::ReturnImm64(CRT + 0x310));
        }
        if n.eq_ignore_ascii_case("__p__commode") {
            return Some(GuestStubKind::ReturnImm64(CRT + 0x318));
        }
        if n.eq_ignore_ascii_case("__p__fmode") {
            return Some(GuestStubKind::ReturnImm64(CRT + 0x320));
        }
        if n.eq_ignore_ascii_case("__p__acmdln") {
            return Some(GuestStubKind::ReturnImm64(CRT + 0x328));
        }
        return None;
    }

    let n = name;

    // --- USER32 pure queries (fixed guest desktop environment) ---
    if library.eq_ignore_ascii_case("USER32.dll") {
        if n.eq_ignore_ascii_case("GetSystemMetrics") {
            return Some(GuestStubKind::LoadU32FromTable {
                table_va: cfg.metrics_table_va,
                max_index: METRICS_COUNT as u32,
            });
        }
        if n.eq_ignore_ascii_case("GetSysColor") {
            return Some(GuestStubKind::LoadU32FromTable {
                table_va: cfg.colors_table_va,
                max_index: COLOR_COUNT as u32,
            });
        }
        if n.eq_ignore_ascii_case("GetSysColorBrush") {
            // Microsoft: returns a handle to the logical brush; we use a stable
            // fake HBRUSH space base+index (same as host user32 handler).
            return Some(GuestStubKind::SysColorBrush {
                base: FAKE_SYSCOLOR_BRUSH_BASE,
            });
        }
        if n.eq_ignore_ascii_case("GetDesktopWindow") {
            // Microsoft: handle to the desktop window — single fake desktop HWND.
            return Some(GuestStubKind::ReturnImm64(FAKE_DESKTOP_WINDOW));
        }
        if n.eq_ignore_ascii_case("DialogBoxParamA") {
            return Some(GuestStubKind::DialogBoxParam {
                create_dialog_param_va: cfg.create_dialog_param_a_va,
                get_message_va: cfg.get_message_a_va,
                is_dialog_message_va: cfg.is_dialog_message_a_va,
                dispatch_message_va: cfg.dispatch_message_a_va,
                dialog_result_va: cfg.dialog_result_va,
            });
        }
        if n.eq_ignore_ascii_case("DialogBoxParamW") {
            return Some(GuestStubKind::DialogBoxParam {
                create_dialog_param_va: cfg.create_dialog_param_w_va,
                get_message_va: cfg.get_message_a_va,
                is_dialog_message_va: cfg.is_dialog_message_a_va,
                dispatch_message_va: cfg.dispatch_message_a_va,
                dialog_result_va: cfg.dialog_result_va,
            });
        }
        return None;
    }

    if library.eq_ignore_ascii_case("WINMM.dll") {
        // B5: timeGetTime reads the host-written guest clock table (slot 2).
        if n.eq_ignore_ascii_case("timeGetTime") {
            return Some(GuestStubKind::LoadZx32FromVa(
                cfg.clock_table_va.saturating_add(CLOCK_TABLE_SLOT_TIME),
            ));
        }
        return None;
    }

    if !library.eq_ignore_ascii_case("KERNEL32.dll") && !library.eq_ignore_ascii_case("ntdll.dll") {
        return None;
    }

    if n.eq_ignore_ascii_case("EncodePointer") || n.eq_ignore_ascii_case("DecodePointer") {
        return Some(GuestStubKind::IdentityRcxToRax);
    }
    // Enter/Leave/DeleteCriticalSection: host only (MT.1 real owner/recursion).
    // InitializeCriticalSection* stays on host (writes RTL_CRITICAL_SECTION).
    if n.eq_ignore_ascii_case("GetLastError") {
        return Some(GuestStubKind::LoadZx32FromVa(TEB_LAST_ERROR_VA));
    }
    if n.eq_ignore_ascii_case("SetLastError") {
        return Some(GuestStubKind::StoreEcxToVa(TEB_LAST_ERROR_VA));
    }
    if n.eq_ignore_ascii_case("FlsGetValue") {
        return Some(GuestStubKind::FlsGetValue {
            table_va: cfg.fls_table_va,
            max_slots: GUEST_FLS_SLOT_COUNT,
        });
    }
    if n.eq_ignore_ascii_case("FlsSetValue") {
        return Some(GuestStubKind::FlsSetValue {
            table_va: cfg.fls_table_va,
            max_slots: GUEST_FLS_SLOT_COUNT,
        });
    }

    if n.eq_ignore_ascii_case("SetHandleCount")
        || n.eq_ignore_ascii_case("OutputDebugStringA")
        || n.eq_ignore_ascii_case("OutputDebugStringW")
    {
        return Some(GuestStubKind::VoidRet);
    }
    // B5: the host refreshes a guest clock table on every stop; these in-guest
    // stubs read it with no host stop. A constant stub is deliberately NOT
    // planted — a guest frame loop computing `dt = now - last` would busy-wait
    // on `dt == 0` forever (the frozen-clock trap documented below). The table
    // advances monotonically because the refresh derives from one monotonic
    // session epoch; `WIE_FIXED_CLOCK=1` freezes it (write-once at session
    // init), preserving the deterministic-trace kill switch.
    if n.eq_ignore_ascii_case("GetTickCount") {
        return Some(GuestStubKind::LoadZx32FromVa(
            cfg.clock_table_va.saturating_add(CLOCK_TABLE_SLOT_TICK),
        ));
    }
    if n.eq_ignore_ascii_case("GetTickCount64") {
        return Some(GuestStubKind::LoadZx64FromVa(
            cfg.clock_table_va.saturating_add(CLOCK_TABLE_SLOT_TICK64),
        ));
    }
    if n.eq_ignore_ascii_case("GetSystemTimeAsFileTime") {
        return Some(GuestStubKind::CopyU64FromVaToRcxPtr {
            slot_va: cfg.clock_table_va.saturating_add(CLOCK_TABLE_SLOT_FILETIME),
        });
    }
    if n.eq_ignore_ascii_case("QueryPerformanceCounter") {
        return Some(GuestStubKind::CopyU64FromVaToRcxPtrRetOne {
            slot_va: cfg.clock_table_va.saturating_add(CLOCK_TABLE_SLOT_QPC),
        });
    }
    if n.eq_ignore_ascii_case("QueryPerformanceFrequency") {
        return Some(GuestStubKind::CopyU64FromVaToRcxPtrRetOne {
            slot_va: cfg.clock_table_va.saturating_add(CLOCK_TABLE_SLOT_QPC_FREQ),
        });
    }
    if n.eq_ignore_ascii_case("GetCurrentProcessId") {
        return Some(GuestStubKind::ReturnImm32(0x1234));
    }
    // Primary TID is fixed (`PRIMARY_THREAD_ID` / 0x5678). Host path reads
    // ThreadState when the stub is not planted (workers in MT.2).
    if n.eq_ignore_ascii_case("GetCurrentThreadId") {
        return Some(GuestStubKind::ReturnImm32(wie_winapi::PRIMARY_THREAD_ID));
    }
    if n.eq_ignore_ascii_case("IsDebuggerPresent") {
        return Some(GuestStubKind::ReturnZero);
    }
    // Sleep is never planted: host idle policy (Phase 6) must see every call.
    if n.eq_ignore_ascii_case("GetACP") {
        return Some(GuestStubKind::ReturnImm32(1252));
    }
    if n.eq_ignore_ascii_case("GetOEMCP") {
        return Some(GuestStubKind::ReturnImm32(437));
    }
    // Microsoft Learn: LANGID en-US = 0x0409 for both when guest is fixed en-US.
    if n.eq_ignore_ascii_case("GetSystemDefaultLangID")
        || n.eq_ignore_ascii_case("GetUserDefaultLangID")
    {
        return Some(GuestStubKind::ReturnImm32(LANG_EN_US));
    }
    if n.eq_ignore_ascii_case("GetCurrentProcess") {
        // Microsoft: (HANDLE)(LONG_PTR)-1 process pseudohandle.
        return Some(GuestStubKind::ReturnImm64(u64::MAX));
    }
    if n.eq_ignore_ascii_case("GetProcessHeap") {
        return Some(GuestStubKind::ReturnImm64(0x0000_0000_5000_0000));
    }
    // Microsoft: returns pointer to the command-line string for the process.
    if n.eq_ignore_ascii_case("GetCommandLineA") {
        return Some(GuestStubKind::ReturnImm64(cfg.command_line_a_va));
    }
    if n.eq_ignore_ascii_case("GetCommandLineW") {
        return Some(GuestStubKind::ReturnImm64(cfg.command_line_w_va));
    }
    if n.eq_ignore_ascii_case("GetCurrentDirectoryW") {
        return Some(GuestStubKind::GetCurrentDirectoryW {
            cwd_blob_va: cfg.cwd_blob_va,
        });
    }

    // Intentionally NOT stubbed (would damage apps if simplified):
    // - VirtualProtect: NULL lpflOldProtect must fail (Learn); real protect later Phase 3
    // - VirtualQuery: must describe real VA regions (RegionTable)
    // - LocalAlloc/GlobalAlloc: LMEM_MOVEABLE / lock / size-0 discard semantics
    // - SetUnhandledExceptionFilter: must return previous filter for chaining

    None
}

/// Builds metrics/colors tables matching host `fake_system_metric` / `GetSysColor`.
pub(crate) fn build_stub_data_page() -> Vec<u8> {
    let mut page = vec![0_u8; 0x500 + CWD_BLOB_SIZE];
    // Metrics
    for i in 0..METRICS_COUNT {
        let v = fake_system_metric(i as u64) as u32;
        let off = i * 4;
        page[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    // Colors at OFFSET_COLORS
    let color_base = OFFSET_COLORS as usize;
    for i in 0..COLOR_COUNT {
        let v = fake_sys_color(i as u64) as u32;
        let off = color_base + i * 4;
        page[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    // cwd blob starts empty (char_count = 0); session fills after process identity.
    page
}

/// Publish UTF-16 current directory into the guest cwd blob (Microsoft path string).
pub(crate) fn publish_cwd_wide(
    engine: &mut dyn wie_cpu::CpuEngine,
    cwd_blob_va: u64,
    directory: &str,
) -> Result<()> {
    let mut units: Vec<u16> = directory.encode_utf16().collect();
    if units.len() > CWD_MAX_CHARS {
        units.truncate(CWD_MAX_CHARS);
    }
    let char_count = u32::try_from(units.len()).context("cwd char_count overflow")?;
    units.push(0); // NUL
    let mut blob = vec![0_u8; CWD_BLOB_SIZE];
    blob[0..4].copy_from_slice(&char_count.to_le_bytes());
    for (i, u) in units.iter().enumerate() {
        let off = 4 + i * 2;
        if off + 2 <= blob.len() {
            blob[off..off + 2].copy_from_slice(&u.to_le_bytes());
        }
    }
    engine
        .mem_write(cwd_blob_va, &blob)
        .context("failed to publish guest cwd blob")?;
    Ok(())
}

/// Publish the current clock snapshot into the host-written guest clock table (B5).
///
/// Slot layout must match the classify offsets above and
/// `wie_winapi::kernel32::clock::clock_table_values`. The host refreshes this
/// table on every API stop (and once at session init) so the in-guest clock
/// stubs observe advancing values with no host stop.
pub(crate) fn refresh_clock_table(
    engine: &mut dyn wie_cpu::CpuEngine,
    clock_table_va: u64,
) -> Result<()> {
    let mut table = [0_u8; CLOCK_TABLE_SIZE];
    let values = wie_winapi::kernel32::clock::clock_table_values();
    for (slot, value) in values.iter().enumerate() {
        let off = slot.saturating_mul(8);
        if let Some(dst) = table.get_mut(off..off.saturating_add(8)) {
            dst.copy_from_slice(&value.to_le_bytes());
        }
    }
    engine
        .mem_write(clock_table_va, &table)
        .context("failed to refresh guest clock table")
}

/// Must match `wie_winapi::user32::fake_system_metric`.
fn fake_system_metric(metric_index: u64) -> u64 {
    match metric_index {
        0 | 16 => 1024,
        1 => 768,
        2 | 3 => 17,
        4 => 23,
        5 | 6 | 19 | 80 => 1,
        7 | 8 | 32 | 33 | 36 | 37 => 4,
        11..=14 => 32,
        15 => 20,
        17 => 728,
        28 | 34 => 112,
        29 | 35 => 27,
        30 | 31 => 18,
        38 | 39 => 75,
        _ => 0,
    }
}

/// Must match `wie_winapi::user32::handle_get_sys_color` COLORREF values.
fn fake_sys_color(color_index: u64) -> u64 {
    match color_index {
        1 | 6 | 7..=9 | 18 => 0x0000_0000,
        2 | 13 => 0x00d7_7830,
        3 => 0x00bf_bfbf,
        5 | 14 => 0x00ff_ffff,
        10 | 11 => 0x00b4_b4b4,
        12 => 0x00ab_abab,
        16 => 0x00a0_a0a0,
        17 => 0x006d_6d6d,
        0 => 0x00c8_c8c8,
        _ => 0x00f0_f0f0,
    }
}

/// Writes guest stubs into the fake-API mapping and builds a stop-bit mask.
///
/// Bitmap: bit=1 means "host must stop here", bit=0 means passthrough (guest stub).
///
/// Uses the pre-computed `stub_kind` cached on each entry during `make_entry`
/// — the kind variant is correct, but the embedded addresses (table_va, etc.)
/// were set to zero (CLASSIFY_ONLY).  We re-classify only the entries that
/// need real addresses: those whose kind embeds a VA (FlsGetValue, etc.).
pub(crate) fn plant_guest_stubs(
    engine: &mut dyn wie_cpu::CpuEngine,
    entries: &[crate::hooks::RuntimeFakeApiEntry],
    fake_api_base: u64,
    fake_api_size: usize,
    cfg: &GuestStubConfig,
    helper_code_base: u64,
    helper_code_size: usize,
) -> Result<Vec<u8>> {
    let mut stop_bitmap = vec![0xff_u8; fake_api_size.div_ceil(8)];

    let mut planted = 0_usize;
    let mut helper_cursor = helper_code_base;
    let helper_end = helper_code_base.saturating_add(helper_code_size as u64);
    let mut seen_kinds: std::collections::HashSet<GuestStubKind> = std::collections::HashSet::new();

    for entry in entries {
        let kind = match &entry.stub_kind {
            // Most stub kinds embed the VA in the kind itself (set in make_entry
            // with CLASSIFY_ONLY — VA is 0).  Re-classify only those that need
            // a real guest address from the config.
            Some(kind) if kind.needs_real_guest_addresses() => {
                match classify_guest_stub(&entry.library, &entry.name, cfg) {
                    Some(real_kind) => real_kind,
                    None => continue,
                }
            }
            Some(kind) => *kind,
            None => continue,
        };
        if seen_kinds.insert(kind) {
            tracing::debug!(name = %entry.name, kind = ?kind, "planted new guest stub kind");
        }
        let body = kind.encode();
        let va = entry.fake_target_va;
        if va < fake_api_base {
            continue;
        }
        let offset = usize::try_from(va - fake_api_base)
            .context("guest stub VA offset does not fit usize")?;

        if kind.needs_out_of_line_helper() {
            let body_len = body.len() as u64;
            if helper_cursor
                .checked_add(body_len)
                .is_none_or(|end| end > helper_end)
            {
                tracing::warn!(
                    name = %entry.name,
                    "guest stub helper region full; leaving host path"
                );
                continue;
            }
            engine
                .mem_write(helper_cursor, &body)
                .context("failed to write out-of-line guest stub body")?;
            let mut jmp = [0_u8; 12];
            jmp[0] = 0x48;
            jmp[1] = 0xb8;
            jmp[2..10].copy_from_slice(&helper_cursor.to_le_bytes());
            jmp[10] = 0xff;
            jmp[11] = 0xe0;
            if offset.checked_add(12).is_none_or(|end| end > fake_api_size) {
                continue;
            }
            engine
                .mem_write(va, &jmp)
                .context("failed to write guest stub entry jmp")?;
            for byte_off in offset..offset.saturating_add(12) {
                clear_bit(&mut stop_bitmap, byte_off);
            }
            helper_cursor = helper_cursor.saturating_add(body_len.saturating_add(15) & !15);
        } else {
            let len = body.len();
            if offset
                .checked_add(len)
                .is_none_or(|end| end > fake_api_size)
            {
                continue;
            }
            engine
                .mem_write(va, &body)
                .context("failed to write guest API stub bytes")?;
            for byte_off in offset..offset.saturating_add(len) {
                clear_bit(&mut stop_bitmap, byte_off);
            }
        }
        planted = planted.saturating_add(1);
    }

    tracing::debug!(planted, "planted in-guest WinAPI stubs");
    Ok(stop_bitmap)
}

fn clear_bit(bitmap: &mut [u8], bit_index: usize) {
    let byte = bit_index / 8;
    let bit = bit_index % 8;
    if let Some(slot) = bitmap.get_mut(byte) {
        *slot &= !(1_u8 << bit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_cwd_stub_encodes_and_patches_rel8() {
        let body = GuestStubKind::GetCurrentDirectoryW {
            cwd_blob_va: 0x7000_0004_3500,
        }
        .encode();
        assert!(body.len() > 20);
        assert_eq!(*body.last().unwrap(), 0xc3);
    }

    #[test]
    fn dialog_box_stub_encodes_modal_loop() {
        let body = GuestStubKind::DialogBoxParam {
            create_dialog_param_va: 0x0000_7000_0000_2c00,
            get_message_va: 0x0000_7000_0000_1f60,
            is_dialog_message_va: 0x0000_7000_0000_2c10,
            dispatch_message_va: 0x0000_7000_0000_22c0,
            dialog_result_va: 0x0000_7000_0040_a000,
        }
        .encode();
        // ~120 bytes of modal loop; must end in `ret`.
        assert!(body.len() > 80, "dialog stub too short: {}", body.len());
        assert!(body.len() < 200, "dialog stub too long: {}", body.len());
        assert_eq!(*body.last().unwrap(), 0xc3);
        // The failure path (CreateDialogParam → 0) must return -1 in RAX
        // (0x48 0xc7 0xc0 0xff.. = `mov rax, -1`) and skip the result load.
        let neg1 = [0x48, 0xc7, 0xc0, 0xff, 0xff, 0xff, 0xff];
        assert!(
            body.windows(neg1.len()).any(|w| w == neg1),
            "dialog stub must materialize -1 on CreateDialogParam failure"
        );
        // The result must be loaded via `mov eax, [rax]` after a `mov rax, imm`.
        let load = [0x8b, 0x00, 0xc3];
        assert!(
            body.windows(load.len()).any(|w| w == load)
                || body.windows(2).any(|w| w == [0x8b, 0x00]),
            "dialog stub must end with the dialog-result load"
        );
    }

    #[test]
    fn dialog_callee_vas_resolve_without_imports() {
        // A guest that imports ONLY DialogBoxParamA never imports the modal
        // loop's callees; their fake VAs must still decode to the right
        // WinApiIds (deterministic encode_export, stop-bit default host-stop).
        let cfg = GuestStubConfig::from_layout(&crate::memory::DEFAULT_LAYOUT);
        let expect_export = |va: u64, id: wie_winapi::WinApiId| {
            assert!(
                va >= wie_winapi::FAKE_API_BASE,
                "callee VA {va:#x} outside fake-API window"
            );
            assert_eq!(
                wie_winapi::decode_fake_va(va),
                Some(wie_winapi::FakeVa::Export(id)),
                "callee VA {va:#x} does not decode to {id:?}"
            );
        };
        expect_export(
            cfg.create_dialog_param_a_va,
            wie_winapi::WinApiId::User32Createdialogparama,
        );
        expect_export(
            cfg.create_dialog_param_w_va,
            wie_winapi::WinApiId::User32Createdialogparamw,
        );
        expect_export(
            cfg.get_message_a_va,
            wie_winapi::WinApiId::User32Getmessagea,
        );
        expect_export(
            cfg.is_dialog_message_a_va,
            wie_winapi::WinApiId::User32Isdialogmessagea,
        );
        expect_export(
            cfg.dispatch_message_a_va,
            wie_winapi::WinApiId::User32Dispatchmessagea,
        );
        // The dialog-result slot lives inside the mapped stub data page.
        assert!(
            cfg.dialog_result_va >= crate::memory::DEFAULT_LAYOUT.guest_stub_data_base
                && cfg.dialog_result_va
                    < crate::memory::DEFAULT_LAYOUT.guest_stub_data_base
                        + crate::memory::DEFAULT_LAYOUT.guest_stub_data_size as u64
        );
        // CLASSIFY_ONLY must differ from the real config (forces the
        // needs_real_guest_addresses re-classification path).
        let classify_only = GuestStubConfig::CLASSIFY_ONLY;
        assert_ne!(classify_only.dialog_result_va, cfg.dialog_result_va);
    }

    #[test]
    fn metrics_table_matches_known_sm() {
        let page = build_stub_data_page();
        // SM_CXSCREEN = 0 → 1024
        assert_eq!(&page[0..4], &1024_u32.to_le_bytes());
        // SM_CYSCREEN = 1 → 768
        assert_eq!(&page[4..8], &768_u32.to_le_bytes());
    }

    #[test]
    fn classify_langid_and_not_virtual_protect() {
        let cfg = GuestStubConfig::CLASSIFY_ONLY;
        assert!(matches!(
            classify_guest_stub("KERNEL32.dll", "GetSystemDefaultLangID", &cfg),
            Some(GuestStubKind::ReturnImm32(0x0409))
        ));
        assert!(classify_guest_stub("KERNEL32.dll", "VirtualProtect", &cfg).is_none());
        assert!(classify_guest_stub("KERNEL32.dll", "VirtualQuery", &cfg).is_none());
        assert!(classify_guest_stub("KERNEL32.dll", "LocalAlloc", &cfg).is_none());
        // MT.1: CS must not be VoidRet guest stubs.
        assert!(classify_guest_stub("KERNEL32.dll", "EnterCriticalSection", &cfg).is_none());
        assert!(classify_guest_stub("KERNEL32.dll", "LeaveCriticalSection", &cfg).is_none());
        assert!(classify_guest_stub("KERNEL32.dll", "DeleteCriticalSection", &cfg).is_none());
    }

    /// Every B5 clock API classifies to a table-reading stub at the matching
    /// slot offset, and every such stub embeds the table VA (so it is
    /// re-derived with the real config at plant time).
    #[test]
    fn clock_stubs_classify_to_table_slots() {
        let cfg = GuestStubConfig::from_layout(&crate::memory::DEFAULT_LAYOUT);
        let base = cfg.clock_table_va;
        assert_ne!(base, 0, "clock table must live at a real guest VA");

        let tick = classify_guest_stub("KERNEL32.dll", "GetTickCount", &cfg)
            .expect("GetTickCount classifies");
        assert_eq!(tick, GuestStubKind::LoadZx32FromVa(base));

        let tick64 = classify_guest_stub("KERNEL32.dll", "GetTickCount64", &cfg)
            .expect("GetTickCount64 classifies");
        assert_eq!(
            tick64,
            GuestStubKind::LoadZx64FromVa(base.saturating_add(CLOCK_TABLE_SLOT_TICK64))
        );

        let time =
            classify_guest_stub("winmm.dll", "timeGetTime", &cfg).expect("timeGetTime classifies");
        assert_eq!(
            time,
            GuestStubKind::LoadZx32FromVa(base.saturating_add(CLOCK_TABLE_SLOT_TIME))
        );

        let ft = classify_guest_stub("KERNEL32.dll", "GetSystemTimeAsFileTime", &cfg)
            .expect("GetSystemTimeAsFileTime classifies");
        assert_eq!(
            ft,
            GuestStubKind::CopyU64FromVaToRcxPtr {
                slot_va: base.saturating_add(CLOCK_TABLE_SLOT_FILETIME),
            }
        );

        let qpc = classify_guest_stub("KERNEL32.dll", "QueryPerformanceCounter", &cfg)
            .expect("QueryPerformanceCounter classifies");
        assert_eq!(
            qpc,
            GuestStubKind::CopyU64FromVaToRcxPtrRetOne {
                slot_va: base.saturating_add(CLOCK_TABLE_SLOT_QPC),
            }
        );

        let qpf = classify_guest_stub("KERNEL32.dll", "QueryPerformanceFrequency", &cfg)
            .expect("QueryPerformanceFrequency classifies");
        assert_eq!(
            qpf,
            GuestStubKind::CopyU64FromVaToRcxPtrRetOne {
                slot_va: base.saturating_add(CLOCK_TABLE_SLOT_QPC_FREQ),
            }
        );

        for kind in [tick, tick64, time, ft, qpc, qpf] {
            assert!(
                kind.needs_real_guest_addresses(),
                "{kind:?} embeds the table VA and must be re-derived at plant time"
            );
            // Encoded bodies are self-contained machine code ending in `ret`.
            let body = kind.encode();
            assert_eq!(body.last(), Some(&0xc3), "{kind:?} must end in ret");
        }
    }

    /// End-to-end refresh: the 6×u64 table lands in guest memory at the layout
    /// VA, advances across a 10 ms sleep, and never moves backwards.
    #[test]
    fn refresh_clock_table_writes_advancing_slots() {
        use wie_cpu::CpuBackend;
        let backend = wie_cpu::open_cpu().expect("cpu backend opens");
        let (mut engine, _shared, _guest_mem) = match backend {
            CpuBackend::Jit { engine, shared } => (engine, Some(shared), None),
            CpuBackend::Iced { engine, guest_mem } => (engine, None, Some(guest_mem)),
        };
        let layout = crate::memory::DEFAULT_LAYOUT;
        engine
            .mem_map(
                layout.clock_table_va,
                layout.clock_table_size,
                wie_cpu::RwxPerms::READ_WRITE,
            )
            .expect("map clock table");

        let read_slot = |engine: &mut dyn wie_cpu::CpuEngine, slot: u64| -> u64 {
            let mut buf = [0_u8; 8];
            let _ = engine.mem_read(layout.clock_table_va.saturating_add(slot), &mut buf);
            u64::from_le_bytes(buf)
        };

        refresh_clock_table(&mut *engine, layout.clock_table_va).expect("first refresh");
        let first_tick = read_slot(&mut *engine, CLOCK_TABLE_SLOT_TICK64);
        let first_qpc = read_slot(&mut *engine, CLOCK_TABLE_SLOT_QPC);
        assert_eq!(
            read_slot(&mut *engine, CLOCK_TABLE_SLOT_QPC_FREQ),
            10_000_000,
            "QPC frequency slot must be the fixed 10 MHz base"
        );

        std::thread::sleep(std::time::Duration::from_millis(10));
        refresh_clock_table(&mut *engine, layout.clock_table_va).expect("second refresh");
        let second_tick = read_slot(&mut *engine, CLOCK_TABLE_SLOT_TICK64);
        let second_qpc = read_slot(&mut *engine, CLOCK_TABLE_SLOT_QPC);

        assert!(
            second_tick >= first_tick.saturating_add(8),
            "tick_count_64 slot did not advance: {first_tick} -> {second_tick}"
        );
        assert!(
            second_qpc >= first_qpc,
            "qpc slot went backwards: {second_qpc} < {first_qpc}"
        );
    }

    /// Every known guest-stub classification: if the `CLASSIFY_ONLY` body differs
    /// from the real-config body, the kind **must** declare that it needs
    /// re-classification via [`GuestStubKind::needs_real_guest_addresses`].
    ///
    /// This catches cases where a new stub kind embeds a config-dependent guest
    /// address but the author forgets to add the variant to the `matches!` list
    /// in `needs_real_guest_addresses`. Without this guard the stub would be
    /// planted with address zero for all cfg-derived addresses.
    #[test]
    fn every_stub_needing_real_addresses_is_listed() {
        // Every (library, name) pair that `classify_guest_stub` can return `Some` for.
        // When a new stub is added to `classify_guest_stub`, add it here too.
        let stubs: &[(&str, &str)] = &[
            // UCRT / CRT
            ("api-ms-win-crt-runtime-l1-1-0.dll", "__acrt_iob_func"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "_initterm"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "_initterm_e"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "fflush"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "setvbuf"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "_crt_atexit"),
            (
                "api-ms-win-crt-runtime-l1-1-0.dll",
                "_set_invalid_parameter_handler",
            ),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "_set_app_type"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "_set_new_mode"),
            (
                "api-ms-win-crt-runtime-l1-1-0.dll",
                "_configure_narrow_argv",
            ),
            (
                "api-ms-win-crt-runtime-l1-1-0.dll",
                "_initialize_narrow_environment",
            ),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "__setusermatherr"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "_configthreadlocale"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "_cexit"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "signal"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "__p__environ"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "__p___argv"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "__p___argc"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "__p__commode"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "__p__fmode"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "__p__acmdln"),
            // USER32
            ("USER32.dll", "GetSystemMetrics"),
            ("USER32.dll", "GetSysColor"),
            ("USER32.dll", "GetSysColorBrush"),
            ("USER32.dll", "GetDesktopWindow"),
            ("USER32.dll", "DialogBoxParamA"),
            ("USER32.dll", "DialogBoxParamW"),
            // KERNEL32 / ntdll
            ("KERNEL32.dll", "EncodePointer"),
            ("KERNEL32.dll", "DecodePointer"),
            ("KERNEL32.dll", "GetLastError"),
            ("KERNEL32.dll", "SetLastError"),
            ("KERNEL32.dll", "FlsGetValue"),
            ("KERNEL32.dll", "FlsSetValue"),
            ("KERNEL32.dll", "SetHandleCount"),
            ("KERNEL32.dll", "OutputDebugStringA"),
            ("KERNEL32.dll", "OutputDebugStringW"),
            ("KERNEL32.dll", "GetCurrentProcessId"),
            ("KERNEL32.dll", "GetCurrentThreadId"),
            ("KERNEL32.dll", "IsDebuggerPresent"),
            ("KERNEL32.dll", "GetACP"),
            ("KERNEL32.dll", "GetOEMCP"),
            ("KERNEL32.dll", "GetSystemDefaultLangID"),
            ("KERNEL32.dll", "GetUserDefaultLangID"),
            ("KERNEL32.dll", "GetCurrentProcess"),
            ("KERNEL32.dll", "GetProcessHeap"),
            ("KERNEL32.dll", "GetCommandLineA"),
            ("KERNEL32.dll", "GetCommandLineW"),
            ("KERNEL32.dll", "GetCurrentDirectoryW"),
            // B5: clock stubs read the host-written guest clock table.
            ("KERNEL32.dll", "GetTickCount"),
            ("KERNEL32.dll", "GetTickCount64"),
            ("KERNEL32.dll", "GetSystemTimeAsFileTime"),
            ("KERNEL32.dll", "QueryPerformanceCounter"),
            ("KERNEL32.dll", "QueryPerformanceFrequency"),
            ("winmm.dll", "timeGetTime"),
        ];

        let real_cfg = GuestStubConfig::from_layout(&crate::memory::DEFAULT_LAYOUT);

        for &(library, name) in stubs {
            let kind0 = classify_guest_stub(library, name, &GuestStubConfig::CLASSIFY_ONLY);
            let kind1 = classify_guest_stub(library, name, &real_cfg);

            match (kind0, kind1) {
                (Some(k0), Some(k1)) => {
                    let body0 = k0.encode();
                    let body1 = k1.encode();
                    if body0 != body1 {
                        assert!(
                            k0.needs_real_guest_addresses(),
                            "GuestStubKind variant for {library}!{name} produces \
                             different machine code with CLASSIFY_ONLY vs real config \
                             (k0={k0:?}, k1={k1:?}). \
                             Add this variant to `needs_real_guest_addresses()`.",
                        );
                    }
                }
                (Some(_), None) => {
                    panic!("{library}!{name}: classifies with CLASSIFY_ONLY but not with real cfg");
                }
                (None, Some(_)) => {
                    panic!("{library}!{name}: classifies with real cfg but not with CLASSIFY_ONLY");
                }
                (None, None) => {
                    panic!(
                        "{library}!{name}: no longer classifies as a guest stub; remove from test"
                    );
                }
            }
        }
    }
}
