//! Shell drag-drop support: the host-held drop list behind `WM_DROPFILES`.
//!
//! The winit file-drop event (host thread) stores the dropped paths + drop
//! point here through `WinApiState::drag_drop` and posts `WM_DROPFILES` with
//! the fake `HDROP` as `wParam`; the guest's `WM_DROPFILES` handler then reads
//! the list back through `DragQueryFileA/W`, `DragQueryPoint` and
//! `DragFinish`. The list is single-slot: a new drop replaces the previous
//! one, and `DragFinish` clears it (Windows frees the HDROP global memory).

use crate::gdi32::{ArgReg, read_arg};
use crate::guest_memory::write_i32;
use crate::guest_string::{write_ansi_c_string, write_utf16_c_string};
use crate::{HandlerContext, WinApiHandlerResult, WinApiState};
use anyhow::{Context, Result};

/// The fake `HDROP` the host passes as the `WM_DROPFILES` `wParam`.
///
/// A single constant is enough: the drop list is single-slot, so any non-NULL
/// handle names the current list and `DragFinish` clears it. Kept in the
/// reserved `0x6600_00xx` fake-handle range (unused by icon/cursor/monitor).
pub const FAKE_HDROP: u64 = 0x0000_0000_6600_00D0;

/// Host drop list for the current `WM_DROPFILES` (Task 5.2).
///
/// Lives in a [`DllStateMap`](crate::DllStateMap) slot so both the host winit
/// thread (via `WinApiState::drag_drop`) and the guest-thread handlers can
/// reach it under the big state mutex — the same seam `DragAcceptFiles` uses
/// for the per-window drop flag, mirrored at process scope.
#[derive(Debug, Clone, Default)]
pub struct DragDropState {
    /// Guest-visible paths (`C:\…` / `D:\…`) of the current drop.
    files: Vec<String>,
    /// Drop point in the target window's client coordinates — the value
    /// `DragQueryPoint` returns.
    point: (i32, i32),
}

impl DragDropState {
    /// Replace the drop list with `files` (guest paths) and the drop `point`.
    pub fn set_drop(&mut self, files: Vec<String>, point: (i32, i32)) {
        self.files = files;
        self.point = point;
    }

    /// The guest-visible paths of the current drop.
    #[must_use]
    pub fn files(&self) -> &[String] {
        &self.files
    }

    /// The drop point in client coordinates.
    #[must_use]
    pub fn point(&self) -> (i32, i32) {
        self.point
    }

    /// Clear the list (`DragFinish` — the drop is consumed).
    pub fn clear(&mut self) {
        self.files.clear();
        self.point = (0, 0);
    }
}

/// `UINT DragQueryFileW(HDROP hDrop, UINT iFile, LPWSTR lpszFile, UINT cch)`
///
/// `iFile == (UINT)-1` returns the number of files; otherwise the path for
/// that index is copied into `lpszFile` (up to `cch` characters incl. NUL)
/// and the copied length is returned. `lpszFile == NULL` (or `cch == 0`)
/// returns the required buffer size in characters, including the NUL —
/// Windows buffer-size-query semantics.
pub fn handle_drag_query_file_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let h_drop = read_arg(engine, ArgReg::Rcx, "DragQueryFileW")?;
    let i_file = read_arg(engine, ArgReg::Rdx, "DragQueryFileW")?;
    let lpsz_file = read_arg(engine, ArgReg::R8, "DragQueryFileW")?;
    let cch = read_arg(engine, ArgReg::R9, "DragQueryFileW")?;

    match drag_query_core(state, h_drop, i_file) {
        Some(DragQueryResolution::Count) => {
            let count = state.drag_drop().files().len();
            tracing::info!(count, "DragQueryFileW: count query");
            ctx.finish(u64::try_from(count).unwrap_or(u64::MAX))
        }
        Some(DragQueryResolution::Path(path)) => {
            tracing::info!(index = i_file, path, "DragQueryFileW: path query");
            if lpsz_file == 0 || cch == 0 {
                // Buffer-size query: required chars incl. the terminating NUL.
                let required = path.encode_utf16().count().saturating_add(1);
                return ctx.finish(u64::try_from(required).unwrap_or(u64::MAX));
            }
            let max_chars = usize::try_from(cch).unwrap_or(0);
            let copied = write_utf16_c_string(engine, lpsz_file, max_chars, path)
                .context("failed to write DragQueryFileW path")?;
            ctx.finish(u64::try_from(copied).unwrap_or(u64::MAX))
        }
        None => {
            tracing::info!(
                h_drop,
                index = i_file,
                "DragQueryFileW: miss (foreign handle or empty list)"
            );
            ctx.finish(0)
        }
    }
}

/// `UINT DragQueryFileA(HDROP hDrop, UINT iFile, LPSTR lpszFile, UINT cch)`
///
/// ANSI variant of [`handle_drag_query_file_w`]; the copied length is in
/// bytes. WIE A-strings are UTF-8, so the stored guest path is written as-is.
pub fn handle_drag_query_file_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let h_drop = read_arg(engine, ArgReg::Rcx, "DragQueryFileA")?;
    let i_file = read_arg(engine, ArgReg::Rdx, "DragQueryFileA")?;
    let lpsz_file = read_arg(engine, ArgReg::R8, "DragQueryFileA")?;
    let cch = read_arg(engine, ArgReg::R9, "DragQueryFileA")?;

    match drag_query_core(state, h_drop, i_file) {
        Some(DragQueryResolution::Count) => {
            let count = state.drag_drop().files().len();
            tracing::info!(count, "DragQueryFileA: count query");
            ctx.finish(u64::try_from(count).unwrap_or(u64::MAX))
        }
        Some(DragQueryResolution::Path(path)) => {
            tracing::info!(index = i_file, path, "DragQueryFileA: path query");
            if lpsz_file == 0 || cch == 0 {
                // Buffer-size query: required bytes incl. the terminating NUL.
                let required = path.len().saturating_add(1);
                return ctx.finish(u64::try_from(required).unwrap_or(u64::MAX));
            }
            let max_bytes = usize::try_from(cch).unwrap_or(0);
            let copied = write_ansi_c_string(engine, lpsz_file, max_bytes, path)
                .context("failed to write DragQueryFileA path")?;
            ctx.finish(u64::try_from(copied).unwrap_or(u64::MAX))
        }
        None => {
            tracing::info!(
                h_drop,
                index = i_file,
                "DragQueryFileA: miss (foreign handle or empty list)"
            );
            ctx.finish(0)
        }
    }
}

/// `BOOL DragQueryPoint(HDROP hDrop, LPPOINT ppt)`
///
/// Writes the drop point (`POINT` = two `i32`) into `ppt` and returns TRUE —
/// the drop always landed in the window's client area. Returns FALSE (and
/// writes nothing) for an invalid handle.
pub fn handle_drag_query_point(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let h_drop = read_arg(engine, ArgReg::Rcx, "DragQueryPoint")?;
    let ppt = read_arg(engine, ArgReg::Rdx, "DragQueryPoint")?;
    if h_drop != FAKE_HDROP {
        return ctx.finish(0); // FALSE — not our drop list
    }
    let (x, y) = state.drag_drop().point();
    if ppt != 0 {
        write_i32(engine, ppt, x).context("failed to write DragQueryPoint x")?;
        write_i32(engine, ppt.wrapping_add(4), y).context("failed to write DragQueryPoint y")?;
    }
    ctx.finish(1) // TRUE — dropped inside the client area
}

/// `void DragFinish(HDROP hDrop)`
///
/// Consumes the drop list (the guest is done reading it). Returns non-zero
/// like the other void handlers in this crate.
pub fn handle_drag_finish(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let h_drop = read_arg(engine, ArgReg::Rcx, "DragFinish")?;
    if h_drop == FAKE_HDROP {
        tracing::info!("DragFinish: drop consumed");
        state.drag_drop().clear();
    }
    ctx.finish(1)
}

/// Shared `DragQueryFile` core: validate the handle and resolve `iFile`.
///
/// `None` is a foreign `hDrop` or an out-of-range index (both return 0).
fn drag_query_core<'a>(
    state: &'a mut WinApiState,
    h_drop: u64,
    i_file: u64,
) -> Option<DragQueryResolution<'a>> {
    if h_drop != FAKE_HDROP {
        return None;
    }
    let index = i_file & 0xffff_ffff;
    if index == 0xffff_ffff {
        // Count query — the caller returns the file count.
        return Some(DragQueryResolution::Count);
    }
    let path = state
        .drag_drop()
        .files()
        .get(usize::try_from(index).unwrap_or(usize::MAX))?;
    Some(DragQueryResolution::Path(path.as_str()))
}

/// Outcome of a [`DragQueryFile`](`handle_drag_query_file_w`) index query.
enum DragQueryResolution<'a> {
    /// `iFile == (UINT)-1` — the caller returns the number of files.
    Count,
    /// A valid index — the path to copy into the caller's buffer.
    Path(&'a str),
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::guest_string::{read_ansi_lossy, read_utf16_lossy};
    use crate::{
        FileHandle, FindFileHandle, GuestHeap, GuestStdinMode, HeapState, KernelState,
        ModuleHandle, ModuleState, ProcessState, RegistryKeyHandle, ResourceHandle, ThreadState,
        WinApiEnvironment, WinApiState,
    };
    use ahash::HashMap;
    use ahash::HashMapExt;
    use std::sync::{Arc, Mutex};
    use wie_cpu::{CpuEngine, IcedCpu, RwxPerms};

    use crate::sync_obj::SyncState;
    use crate::vfs::VolumeConfig;

    const STACK_VA: u64 = 0x100_0000;
    const STACK_SIZE: usize = 0x1_0000;
    // STACK_VA + STACK_SIZE - 0x100 (leave room for a dummy return address).
    const STACK_TOP: u64 = 0x100_FF00;
    /// A writable buffer address inside the mapped test memory.
    const BUF: u64 = 0x3000;

    /// Minimal engine for handler unit tests: maps guest pages and a stack
    /// with a valid return address (`return_from_win64_api` reads it).
    fn test_engine() -> IcedCpu {
        let mut cpu = IcedCpu::open_x86_64();
        cpu.mem_map(0x1000, 0x10_0000, RwxPerms::ALL)
            .expect("map test memory");
        cpu.mem_map(STACK_VA, STACK_SIZE, RwxPerms::ALL)
            .expect("map test stack");
        cpu.mem_write(STACK_TOP, &0_u64.to_le_bytes())
            .expect("write return address");
        cpu.write_rsp(STACK_TOP).ok();
        cpu
    }

    fn write_regs(cpu: &mut IcedCpu, rcx: u64, rdx: u64, r8: u64, r9: u64) {
        cpu.write_rcx(rcx).ok();
        cpu.write_rdx(rdx).ok();
        cpu.write_r8(r8).ok();
        cpu.write_r9(r9).ok();
        cpu.write_rsp(STACK_TOP).ok();
    }

    fn test_environment() -> WinApiEnvironment {
        WinApiEnvironment {
            image_base: 0,
            command_line_a_ptr: 0,
            command_line_w_ptr: 0,
            environment_strings_w_ptr: 0,
            module_file_name_a_ptr: 0,
            module_file_name_w_ptr: 0,
            process_heap_handle: 0,
        }
    }

    /// Zero-heavy default state; the drag-drop handlers only touch the
    /// `DllStateMap` slot behind `WinApiState::drag_drop`.
    fn test_state() -> WinApiState {
        let mut heap = GuestHeap::new(0x2000, 0x10000);
        heap.attach_guest_control(0x2000);
        WinApiState {
            heap_state: HeapState {
                heap,
                next_fls_index: 0,
                fls_slots: Vec::new(),
                guest_fls_table_va: 0,
            },
            file_io: crate::FileIoState {
                executable_file_size: 0,
                executable_file_bytes: Arc::new(Vec::new()),
                executable_file_cursor: 0,
                next_find_handle: FindFileHandle::from(0),
                find_handles: Vec::new(),
                host_file_mounts: Vec::new(),
                virtual_files: Vec::new(),
                open_files: HashMap::new(),
                next_file_handle: FileHandle::from(0),
                next_resource_handle: ResourceHandle::from(0),
                resources: Vec::new(),
                current_directory_wide: Vec::new(),
                bottle_root: None,
                volumes: VolumeConfig::default(),
                guest_file_data_next: 0,
                guest_io: None,
                stdin_bytes: Vec::new(),
                stdin_cursor: 0,
                stdin_mode: GuestStdinMode::InjectOnly,
                ucrt_files: HashMap::new(),
                ucrt_next_file_va: 0x0000_0000_6900_0000,
                cached_streams: HashMap::new(),
            },
            process: ProcessState {
                last_error: 0,
                next_registry_key_handle: RegistryKeyHandle::from(0),
                registry_keys: Vec::new(),
                main_module_file_name: String::new(),
                main_module_path: String::new(),
                main_module_host_dir: None,
                error_mode: 0,
                suspended_threads: HashMap::new(),
                environment: Vec::new(),
                main_module_dialogs: Vec::new(),
                main_module_menus: Vec::new(),
                main_module_strings: Vec::new(),
                main_module_accelerators: Vec::new(),
            },
            kernel: KernelState {
                threads: ThreadState::primary(),
                sync: SyncState::new(),
                seh_pending: HashMap::new(),
            },
            dll_states: crate::DllStateMap::new(),
            message_queue: Arc::new(Mutex::new(crate::present::MessageQueue::default())),
            module_state: ModuleState {
                loaded_modules: HashMap::new(),
                import_resolver: None,
                get_proc_address_cache: HashMap::new(),
                next_module_handle: ModuleHandle::from(crate::dll_loader::REAL_MODULE_HANDLE_BASE),
            },
        }
    }

    /// Full dispatch path: name resolution (names.rs) → dense id → handler arm.
    fn dispatch(engine: &mut IcedCpu, state: &mut WinApiState, library: &str, name: &str) -> u64 {
        let id = crate::resolve_winapi_id(library, name)
            .unwrap_or_else(|| panic!("{library}!{name} must resolve to a WinApiId"));
        crate::dispatch_winapi_id(
            &mut HandlerContext::new(engine, test_environment(), state),
            id,
        )
        .expect("handler must dispatch")
        .return_value
    }

    #[test]
    fn drag_query_file_w_returns_count_then_paths() {
        let mut engine = test_engine();
        let mut state = test_state();
        state.drag_drop().set_drop(
            vec![r"C:\App\a.txt".to_owned(), r"C:\App\b.txt".to_owned()],
            (0, 0),
        );

        // Count query: iFile = (UINT)-1 → 2 files.
        write_regs(&mut engine, FAKE_HDROP, 0xffff_ffff, 0, 0);
        assert_eq!(
            dispatch(&mut engine, &mut state, "shell32.dll", "DragQueryFileW"),
            2
        );

        // First path (iFile = 0) into a guest buffer; return = chars copied.
        write_regs(&mut engine, FAKE_HDROP, 0, BUF, 64);
        assert_eq!(
            dispatch(&mut engine, &mut state, "shell32.dll", "DragQueryFileW"),
            12,
            r#"C:\App\a.txt is 12 characters"#
        );
        assert_eq!(
            read_utf16_lossy(&mut engine, BUF, 64).expect("read back path"),
            r"C:\App\a.txt"
        );

        // Second path (iFile = 1).
        write_regs(&mut engine, FAKE_HDROP, 1, BUF, 64);
        assert_eq!(
            dispatch(&mut engine, &mut state, "shell32.dll", "DragQueryFileW"),
            12,
            r#"C:\App\b.txt is 12 characters"#
        );
        assert_eq!(
            read_utf16_lossy(&mut engine, BUF, 64).expect("read back second path"),
            r"C:\App\b.txt"
        );
    }

    #[test]
    fn drag_query_file_w_buffer_size_query_returns_required_chars() {
        let mut engine = test_engine();
        let mut state = test_state();
        state
            .drag_drop()
            .set_drop(vec![r"C:\App\out.txt".to_owned()], (0, 0));

        // NULL buffer → required size in chars INCLUDING the terminating NUL.
        write_regs(&mut engine, FAKE_HDROP, 0, 0, 0);
        assert_eq!(
            dispatch(&mut engine, &mut state, "shell32.dll", "DragQueryFileW"),
            15,
            r#"C:\App\out.txt (14 chars) + NUL = 15"#
        );
    }

    #[test]
    fn drag_query_file_a_writes_ansi_path() {
        let mut engine = test_engine();
        let mut state = test_state();
        state
            .drag_drop()
            .set_drop(vec![r"C:\App\out.txt".to_owned()], (0, 0));

        write_regs(&mut engine, FAKE_HDROP, 0, BUF, 64);
        assert_eq!(
            dispatch(&mut engine, &mut state, "shell32.dll", "DragQueryFileA"),
            14,
            r#"C:\App\out.txt is 14 bytes"#
        );
        assert_eq!(
            read_ansi_lossy(&mut engine, BUF, 64).expect("read back ANSI path"),
            r"C:\App\out.txt"
        );
    }

    #[test]
    fn drag_query_point_writes_the_stored_point() {
        let mut engine = test_engine();
        let mut state = test_state();
        state
            .drag_drop()
            .set_drop(vec![r"C:\App\a.txt".to_owned()], (12, 34));

        let ppt = BUF;
        write_regs(&mut engine, FAKE_HDROP, ppt, 0, 0);
        assert_eq!(
            dispatch(&mut engine, &mut state, "shell32.dll", "DragQueryPoint"),
            1,
            "drop in the client area → TRUE"
        );
        let mut bytes = [0_u8; 8];
        engine.mem_read(ppt, &mut bytes).expect("read POINT");
        assert_eq!(
            i32::from_le_bytes(bytes[0..4].try_into().expect("x slice")),
            12
        );
        assert_eq!(
            i32::from_le_bytes(bytes[4..8].try_into().expect("y slice")),
            34
        );
    }

    #[test]
    fn drag_finish_clears_the_list() {
        let mut engine = test_engine();
        let mut state = test_state();
        state.drag_drop().set_drop(
            vec![r"C:\App\a.txt".to_owned(), r"C:\App\b.txt".to_owned()],
            (1, 2),
        );

        write_regs(&mut engine, FAKE_HDROP, 0, 0, 0);
        dispatch(&mut engine, &mut state, "shell32.dll", "DragFinish");

        assert!(state.drag_drop().files().is_empty(), "list must be cleared");
        assert_eq!(state.drag_drop().point(), (0, 0));
        // A count query on the finished drop returns 0.
        write_regs(&mut engine, FAKE_HDROP, 0xffff_ffff, 0, 0);
        assert_eq!(
            dispatch(&mut engine, &mut state, "shell32.dll", "DragQueryFileW"),
            0
        );
    }

    #[test]
    fn foreign_handle_and_out_of_range_index_return_zero() {
        let mut engine = test_engine();
        let mut state = test_state();
        state
            .drag_drop()
            .set_drop(vec![r"C:\App\a.txt".to_owned()], (0, 0));

        // Foreign handle: count query must NOT leak the real count.
        write_regs(&mut engine, 0x1234, 0xffff_ffff, 0, 0);
        assert_eq!(
            dispatch(&mut engine, &mut state, "shell32.dll", "DragQueryFileW"),
            0
        );

        // Index past the end of the list.
        write_regs(&mut engine, FAKE_HDROP, 5, BUF, 64);
        assert_eq!(
            dispatch(&mut engine, &mut state, "shell32.dll", "DragQueryFileW"),
            0
        );

        // DragQueryPoint with a foreign handle → FALSE.
        write_regs(&mut engine, 0x1234, BUF, 0, 0);
        assert_eq!(
            dispatch(&mut engine, &mut state, "shell32.dll", "DragQueryPoint"),
            0
        );
    }
}
