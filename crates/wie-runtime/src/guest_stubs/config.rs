//! Guest-visible layout and offsets for the data-backed in-guest stubs.

/// Guest-visible layout for the data-backed in-guest stubs.
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
    /// Guest VA of the host-written clock table: 6 × u64 slots refreshed
    /// on every host stop; in-guest clock stubs read it with no host stop.
    pub clock_table_va: u64,
    /// Guest VA of the planted file-dialog modal-loop body (see
    /// [`super::encode::StubCtx::encode_file_dialog_loop`]).
    pub file_dialog_loop_va: u64,
    /// Guest VA of the planted file-dialog proc stub (see
    /// [`super::encode::encode_file_dialog_proc`]).
    pub file_dialog_proc_va: u64,
}

/// Offset of the dialog-result slot inside the guest stub data page.
pub(crate) const DIALOG_RESULT_OFFSET: u64 = 0x1000;
/// Offset of the file-dialog modal-loop body inside the guest stub data page.
pub(crate) const FILE_DIALOG_LOOP_OFFSET: u64 = 0x1100;
/// Offset of the file-dialog proc stub body inside the guest stub data page.
pub(crate) const FILE_DIALOG_PROC_OFFSET: u64 = 0x1180;

/// Slot offsets into the 6×u64 guest clock table.
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
        file_dialog_loop_va: 0,
        file_dialog_proc_va: 0,
    };

    #[must_use]
    pub(crate) fn from_layout(layout: &crate::memory::RuntimeMemoryLayout) -> Self {
        let base = layout.guest_stub_data.base;
        Self {
            fls_table_va: layout.guest_fls_table.base,
            metrics_table_va: base,
            colors_table_va: base + OFFSET_COLORS,
            cwd_blob_va: base + OFFSET_CWD,
            command_line_a_va: layout.env_data.base + 0x100,
            command_line_w_va: layout.env_data.base + 0x200,
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
            clock_table_va: layout.clock_table.base,
            file_dialog_loop_va: base + FILE_DIALOG_LOOP_OFFSET,
            file_dialog_proc_va: base + FILE_DIALOG_PROC_OFFSET,
        }
    }
}

/// Metrics table length (SM_* indices fit in a byte for common queries).
pub const METRICS_COUNT: usize = 256;
/// SysColor table length (COLOR_* indices used by host handler).
pub const COLOR_COUNT: usize = 32;
/// Max UTF-16 code units stored for cwd (excluding NUL).
pub const CWD_MAX_CHARS: usize = 260;

pub(super) const OFFSET_COLORS: u64 = 0x400;
const OFFSET_CWD: u64 = 0x500;
/// Bytes: u32 count + (CWD_MAX_CHARS+1) * u16
pub const CWD_BLOB_SIZE: usize = 4 + (CWD_MAX_CHARS + 1) * 2;

/// LANGID for en-US (Microsoft Learn primary language + sublanguage).
pub(super) const LANG_EN_US: u32 = 0x0409;
/// Fake desktop HWND (matches `wie_winapi::user32` FAKE_DESKTOP_WINDOW_HANDLE).
pub(super) const FAKE_DESKTOP_WINDOW: u64 = 0x0000_0000_6600_0110;
/// Fake system-color brush base (matches user32).
pub(super) const FAKE_SYSCOLOR_BRUSH_BASE: u64 = 0x0000_0000_6601_0000;
