//! OLE clipboard + drag-drop lane (`ole32.dll`).
//!
//! Wire layer: `ole32.rs` dispatches the `OleInitialize` / `OleSetClipboard` /
//! `OleGetClipboard` / `OleFlushClipboard` / `DoDragDrop` /
//! `RegisterDragDrop` / `RevokeDragDrop` exports and the host-synthesized
//! `IDataObject` vtable slots (names `IDataObject::X` — see
//! [`super::IDATAOBJECT_METHODS`]) to this module.
//!
//! # Data model
//!
//! The OLE clipboard is layered on the classic clipboard, like Windows:
//! [`crate::clipboard::ClipboardStore`] (a field of `OleState`) holds the
//! formats → bytes payloads. `OleGetClipboard` returns either the guest
//! `IDataObject*` stored by `OleSetClipboard`, or — when none — a
//! host-synthesized `IDataObject` whose vtable lives in a guest heap block and
//! whose methods (GetData/SetData/…) read and write the same store. That is
//! what makes the in-emulator round trip work: the guest calls `GetData` /
//! `SetData` on a host-provided object, exactly like the OLE micro does.
//!
//! Host drag-and-drop (`DoDragDrop` driving the mouse) is NOT implemented —
//! `DoDragDrop` returns `DRAGDROP_S_CANCEL` with `DROPEFFECT_NONE` so
//! well-behaved callers (Qt) take the "cancelled" path. `RegisterDragDrop` /
//! `RevokeDragDrop` only record the per-window `IDropTarget*` pointers.
//!
//! # Layouts (verified against the Windows SDK headers)
//!
//! `FORMATETC` (32 bytes): `cfFormat` i16 @0 (read as u32), 6 bytes padding,
//! `ptd` @8, `dwAspect` u32 @16, `lindex` i32 @20, `tymed` u32 @24.
//! `STGMEDIUM` (24 bytes): `tymed` u32 @0, 4 bytes padding, `hGlobal` @8,
//! `pUnkForRelease` @16.
//! `IDataObject` vtable: 3 `IUnknown` slots then GetData(3) GetDataHere(4)
//! QueryGetData(5) GetCanonicalFormatEtc(6) SetData(7) EnumFormatEtc(8)
//! DAdvise(9) DUnadvise(10) EnumDAdvise(11).

use anyhow::{Context, Result};

use crate::HandlerContext;
use crate::WinApiHandlerResult;
use crate::gdi32::{ArgReg, read_arg};

/// `S_OK`
const S_OK: u64 = 0;
/// `E_POINTER` — a required out pointer was NULL.
const E_POINTER: u64 = 0x8000_4003;
/// `E_NOTIMPL` — vtable slot / method deliberately unimplemented.
const E_NOTIMPL: u64 = 0x8000_4001;
/// `E_OUTOFMEMORY` — the guest heap could not grow the payload block.
const E_OUTOFMEMORY: u64 = 0x8007_000E;
/// `DV_E_FORMATETC` — format/aspect/tymed not supported by GetData/SetData.
const DV_E_FORMATETC: u64 = 0x8004_0064;
/// `DRAGDROP_S_CANCEL` — DoDragDrop was cancelled (host drag unimplemented).
const DRAGDROP_S_CANCEL: u64 = 1;
/// `DROPEFFECT_NONE` — written to `*pdwEffect` by the DoDragDrop stub.
const DROPEFFECT_NONE: u64 = 0;
/// `DVASPECT_CONTENT` — the only aspect GetData accepts.
const DVASPECT_CONTENT: u32 = 1;
/// `TYMED_HGLOBAL` — the only storage medium both methods accept.
const TYMED_HGLOBAL: u32 = 1;

/// `HRESULT OleInitialize(LPVOID pvReserved)` — records the per-process flag.
pub(super) fn handle_ole_initialize(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _reserved = read_arg(engine, ArgReg::Rcx, "OleInitialize")?;
    ctx.state.ole32().ole_initialized = true;
    ctx.finish(S_OK)
}

/// `void OleUninitialize(void)`
pub(super) fn handle_ole_uninitialize(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ctx.state.ole32().ole_initialized = false;
    ctx.finish(0)
}

/// `HRESULT OleSetClipboard(IDataObject *pDataObj)`
///
/// Stores the guest `IDataObject*`. NULL means flush semantics: the data is
/// already persisted in the clipboard store, so only the transient source
/// object pointer is cleared.
pub(super) fn handle_ole_set_clipboard(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let data_object = read_arg(engine, ArgReg::Rcx, "OleSetClipboard")?;
    let state = &mut *ctx.state;
    if data_object == 0 {
        state.ole32().clipboard_data_object = 0;
    } else {
        state.ole32().clipboard_data_object = data_object;
    }
    ctx.finish(S_OK)
}

/// `HRESULT OleGetClipboard(IDataObject **ppDataObj)`
///
/// Returns the stored guest `IDataObject*`, or synthesizes a host
/// `IDataObject` over the clipboard store (cached in `synthesized_data_object`).
pub(super) fn handle_ole_get_clipboard(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let pp_data_obj = read_arg(engine, ArgReg::Rcx, "OleGetClipboard")?;
    if pp_data_obj == 0 {
        return ctx.finish(E_POINTER);
    }
    let state = &mut *ctx.state;
    let object = {
        let ole = state.ole32();
        if ole.clipboard_data_object != 0 {
            ole.clipboard_data_object
        } else if ole.synthesized_data_object != 0 {
            ole.synthesized_data_object
        } else {
            0
        }
    };
    let object = if object != 0 {
        object
    } else {
        let fresh = synthesize_idataobject(engine, state)?;
        state.ole32().synthesized_data_object = fresh;
        fresh
    };
    engine.mem_write(pp_data_obj, &object.to_le_bytes())?;
    ctx.finish(S_OK)
}

/// `HRESULT OleFlushClipboard(void)`
///
/// The data already lives in the clipboard store (every SetData wrote it
/// there), so flushing only drops the transient source-object pointer.
pub(super) fn handle_ole_flush_clipboard(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    ctx.state.ole32().clipboard_data_object = 0;
    ctx.finish(S_OK)
}

/// `HRESULT RegisterDragDrop(HWND hwnd, IDropTarget *pDropTarget)` — records
/// the per-window target; no host drag-drop.
pub(super) fn handle_register_drag_drop(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let hwnd = read_arg(engine, ArgReg::Rcx, "RegisterDragDrop")?;
    let drop_target = read_arg(engine, ArgReg::Rdx, "RegisterDragDrop")?;
    if hwnd != 0 && drop_target != 0 {
        ctx.state.ole32().drop_targets.insert(hwnd, drop_target);
    }
    ctx.finish(S_OK)
}

/// `HRESULT RevokeDragDrop(HWND hwnd)`
pub(super) fn handle_revoke_drag_drop(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let hwnd = read_arg(engine, ArgReg::Rcx, "RevokeDragDrop")?;
    ctx.state.ole32().drop_targets.remove(&hwnd);
    ctx.finish(S_OK)
}

/// `HRESULT DoDragDrop(IDataObject*, IDropSource*, DWORD, DWORD *pdwEffect)`
///
/// Stub: host-side drag is not implemented, so every drag is "cancelled".
/// `*pdwEffect` is written `DROPEFFECT_NONE` and the result is
/// `DRAGDROP_S_CANCEL` (1) — Qt and similar frameworks handle a cancelled
/// drag gracefully.
pub(super) fn handle_do_drag_drop(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _data_object = read_arg(engine, ArgReg::Rcx, "DoDragDrop")?;
    let _drop_source = read_arg(engine, ArgReg::Rdx, "DoDragDrop")?;
    let _ok_effects = read_arg(engine, ArgReg::R8, "DoDragDrop")?;
    let pdw_effect = read_arg(engine, ArgReg::R9, "DoDragDrop")?;
    if pdw_effect != 0 {
        engine.mem_write(pdw_effect, &DROPEFFECT_NONE.to_le_bytes())?;
    }
    ctx.finish(DRAGDROP_S_CANCEL)
}

// ── IDataObject vtable methods ─────────────────────────────────────────

/// `QueryInterface`: any IID answers with the same object pointer (the object
/// is host-synthesized; there is only one interface).
pub(super) fn handle_query_interface(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let this = read_arg(engine, ArgReg::Rcx, "IDataObject::QueryInterface")?;
    let _riid = read_arg(engine, ArgReg::Rdx, "IDataObject::QueryInterface")?;
    let ppv = read_arg(engine, ArgReg::R8, "IDataObject::QueryInterface")?;
    if ppv != 0 {
        engine.mem_write(ppv, &this.to_le_bytes())?;
    }
    ctx.finish(S_OK)
}

/// `AddRef` — the synthesized object is not reference-counted.
pub(super) fn handle_add_ref(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _this = read_arg(engine, ArgReg::Rcx, "IDataObject::AddRef")?;
    ctx.finish(1)
}

/// `Release` — the synthesized object is not reference-counted.
pub(super) fn handle_release(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _this = read_arg(engine, ArgReg::Rcx, "IDataObject::Release")?;
    ctx.finish(1)
}

/// `GetData(FORMATETC *pformatetcIn, STGMEDIUM *pmedium)`
///
/// Reads CF_UNICODETEXT / CF_TEXT (or any stored format) from the clipboard
/// store and returns an HGLOBAL payload in `*pmedium`. Unknown formats answer
/// `DV_E_FORMATETC`.
pub(super) fn handle_get_data(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _this = read_arg(engine, ArgReg::Rcx, "IDataObject::GetData")?;
    let pformatetc = read_arg(engine, ArgReg::Rdx, "IDataObject::GetData")?;
    let pmedium = read_arg(engine, ArgReg::R8, "IDataObject::GetData")?;
    if pmedium == 0 {
        return ctx.finish(E_POINTER);
    }
    if pformatetc == 0 {
        return ctx.finish(E_POINTER);
    }
    let fe = read_formatetc(engine, pformatetc)?;
    if fe.dw_aspect != DVASPECT_CONTENT || fe.tymed & TYMED_HGLOBAL == 0 {
        return ctx.finish(DV_E_FORMATETC);
    }
    let Some(bytes) = crate::clipboard::clipboard_format_bytes(ctx.state, fe.cf_format) else {
        return ctx.finish(DV_E_FORMATETC);
    };
    let byte_len = u64::try_from(bytes.len()).context("payload length overflow")?;
    let hglobal = ctx
        .state
        .heap_state
        .heap
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .alloc_coherent(engine, byte_len.max(16));
    if hglobal == 0 {
        return ctx.finish(E_OUTOFMEMORY);
    }
    engine.mem_write(hglobal, &bytes)?;
    write_stgmedium(engine, pmedium, TYMED_HGLOBAL, hglobal)?;
    ctx.finish(S_OK)
}

/// `SetData(FORMATETC *pformatetc, STGMEDIUM *pmedium, BOOL fRelease)`
///
/// Copies the `hGlobal` payload into the clipboard store under `cfFormat`
/// (fRelease is ignored — the bytes are copied, so the caller keeps its block).
pub(super) fn handle_set_data(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _this = read_arg(engine, ArgReg::Rcx, "IDataObject::SetData")?;
    let pformatetc = read_arg(engine, ArgReg::Rdx, "IDataObject::SetData")?;
    let pmedium = read_arg(engine, ArgReg::R8, "IDataObject::SetData")?;
    let _f_release = read_arg(engine, ArgReg::R9, "IDataObject::SetData")?;
    if pformatetc == 0 || pmedium == 0 {
        return ctx.finish(E_POINTER);
    }
    let fe = read_formatetc(engine, pformatetc)?;
    let (medium_tymed, hglobal) = read_stgmedium(engine, pmedium)?;
    if fe.tymed & TYMED_HGLOBAL == 0 || medium_tymed & TYMED_HGLOBAL == 0 || hglobal == 0 {
        return ctx.finish(DV_E_FORMATETC);
    }
    let bytes = crate::clipboard::read_hglobal_bytes(engine, ctx.state, hglobal, fe.cf_format)?;
    crate::clipboard::store_clipboard_format_bytes(ctx.state, fe.cf_format, bytes);
    ctx.finish(S_OK)
}

/// `EnumFormatEtc(DATADIR, IEnumFORMATETC **)` — not implemented: there is no
/// enumerator object. Honest `E_NOTIMPL`; the micro and Qt take the fallback.
pub(super) fn handle_enum_format_etc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_e_notimpl(ctx)
}

/// The unmodeled IDataObject methods (GetDataHere, QueryGetData,
/// GetCanonicalFormatEtc, DAdvise, DUnadvise, EnumDAdvise).
pub(super) fn handle_e_notimpl(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _this = read_arg(engine, ArgReg::Rcx, "IDataObject method")?;
    ctx.finish(E_NOTIMPL)
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Decoded `FORMATETC` (cfFormat @0, ptd @8, dwAspect @16, lindex @20,
/// tymed @24 — see the module header).
struct FormatEtc {
    cf_format: u32,
    dw_aspect: u32,
    _lindex: i32,
    tymed: u32,
}

fn read_formatetc(engine: &mut dyn wie_cpu::CpuEngine, va: u64) -> Result<FormatEtc> {
    Ok(FormatEtc {
        cf_format: crate::guest_memory::read_u32(engine, va)
            .context("failed to read FORMATETC.cfFormat")?,
        dw_aspect: crate::guest_memory::read_u32(engine, va.wrapping_add(16))
            .context("failed to read FORMATETC.dwAspect")?,
        _lindex: crate::guest_memory::read_i32(engine, va.wrapping_add(20))
            .context("failed to read FORMATETC.lindex")?,
        tymed: crate::guest_memory::read_u32(engine, va.wrapping_add(24))
            .context("failed to read FORMATETC.tymed")?,
    })
}

/// Read `{ tymed @0, hGlobal @8 }` from a guest `STGMEDIUM`.
fn read_stgmedium(engine: &mut dyn wie_cpu::CpuEngine, va: u64) -> Result<(u32, u64)> {
    let tymed =
        crate::guest_memory::read_u32(engine, va).context("failed to read STGMEDIUM.tymed")?;
    let hglobal = crate::guest_memory::read_u64(engine, va.wrapping_add(8))
        .context("failed to read STGMEDIUM.hGlobal")?;
    Ok((tymed, hglobal))
}

/// Write `{ tymed @0, hGlobal @8, pUnkForRelease @16 = NULL }`.
fn write_stgmedium(
    engine: &mut dyn wie_cpu::CpuEngine,
    va: u64,
    tymed: u32,
    hglobal: u64,
) -> Result<()> {
    crate::guest_memory::write_u32(engine, va, tymed)?;
    crate::guest_memory::write_u64(engine, va.wrapping_add(8), hglobal)?;
    crate::guest_memory::write_u64(engine, va.wrapping_add(16), 0)?;
    Ok(())
}

/// Allocate a guest block holding the IDataObject vtable (12 fake-VA slot
/// pointers) plus the object struct (one `lpVtbl` pointer), like the D3D9
/// objects. Returns 0 when the heap is exhausted.
fn synthesize_idataobject(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut crate::WinApiState,
) -> Result<u64> {
    let block = state
        .heap_state
        .heap
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .alloc_coherent(engine, super::IDATAOBJECT_ALLOCATION_SIZE);
    if block == 0 {
        return Ok(0);
    }
    for (slot, _name) in super::IDATAOBJECT_METHODS.iter().enumerate() {
        let method_va = super::idataobject_method_va(slot)?;
        let offset = u64::try_from(slot)
            .context("IDataObject vtable slot does not fit u64")?
            .checked_mul(8)
            .context("IDataObject vtable offset overflow")?;
        let entry = block
            .checked_add(offset)
            .context("IDataObject vtable entry address overflow")?;
        engine.mem_write(entry, &method_va.to_le_bytes())?;
    }
    // The COM object struct is one lpVtbl pointer after the 12-entry vtable.
    let object = block
        .checked_add(96)
        .context("IDataObject object address overflow")?;
    engine.mem_write(object, &block.to_le_bytes())?;
    Ok(object)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch_table::resolve_winapi_id;
    use crate::fake_va::FakeVa;
    use crate::sync_obj::SyncState;
    use crate::thread::ThreadState;
    use crate::vfs::VolumeConfig;
    use crate::{
        DEFAULT_ENVIRONMENT, DllStateMap, FileIoState, HeapState, KernelState, ModuleState,
        ProcessState, WinApiEnvironment, WinApiState,
    };
    use ahash::HashMap;
    use ahash::HashMapExt;
    use std::sync::{Arc, Mutex};
    use wie_cpu::{CpuEngine, IcedCpu, RwxPerms};

    const STACK_VA: u64 = 0x100_0000;
    const STACK_SIZE: usize = 0x1_0000;
    const STACK_TOP: u64 = 0x100_FF00;
    const FORMATETC_VA: u64 = 0x3000;
    const STGMEDIUM_VA: u64 = 0x4000;
    const BUF_VA: u64 = 0x5000;
    const OUT_VA: u64 = 0x6000;

    fn test_engine() -> IcedCpu {
        let mut cpu = IcedCpu::open_x86_64();
        cpu.mem_map(0x1000, 0x10_0000, RwxPerms::ALL)
            .expect("map test memory");
        // Initialize the guest heap control block: bump = heap base, freelist
        // heads stay zeroed (mirrors session/init.rs guest_heap_accel).
        cpu.mem_write(0x2000, &0x2000_u64.to_le_bytes())
            .expect("write heap bump");
        cpu.mem_map(STACK_VA, STACK_SIZE, RwxPerms::ALL)
            .expect("map test stack");
        cpu.mem_write(STACK_TOP, &0_u64.to_le_bytes())
            .expect("write return address");
        cpu.write_rsp(STACK_TOP).ok();
        cpu
    }

    fn default_env() -> WinApiEnvironment {
        WinApiEnvironment {
            image_base: 0x0000_0000_1400_0000,
            command_line_a_ptr: 0,
            command_line_w_ptr: 0,
            environment_strings_w_ptr: 0,
            module_file_name_a_ptr: 0,
            module_file_name_w_ptr: 0,
            process_heap_handle: 1,
        }
    }

    fn winapi_state_default() -> WinApiState {
        let heap = std::sync::Arc::new(std::sync::Mutex::new(crate::guest_heap::GuestHeap::new(
            0x2000, 0x10000,
        )));
        heap.lock()
            .unwrap_or_else(|e| e.into_inner())
            .attach_guest_control(0x2000);
        WinApiState {
            display: crate::DisplayMetrics::default(),
            heap_state: HeapState {
                heap,
                next_fls_index: 0,
                fls_slots: Vec::new(),
                guest_fls_table_va: 0,
            },
            file_io: FileIoState {
                executable_file_size: 0,
                executable_file_bytes: Arc::new(Vec::new()),
                executable_file_cursor: 0,
                next_find_handle: crate::FindFileHandle::from(0),
                find_handles: Vec::new(),
                host_file_mounts: Vec::new(),
                virtual_files: Vec::new(),
                open_files: HashMap::new(),
                next_file_handle: crate::FileHandle::from(0),
                next_resource_handle: crate::ResourceHandle::from(0),
                resources: Vec::new(),
                current_directory_wide: Vec::new(),
                bottle_root: None,
                volumes: VolumeConfig::default(),
                guest_file_data_next: 0,
                guest_io: None,
                stdin_bytes: Vec::new(),
                stdin_cursor: 0,
                stdin_mode: crate::GuestStdinMode::InjectOnly,
                ucrt_files: HashMap::new(),
                ucrt_next_file_va: 0x0000_0000_6900_0000,
                cached_streams: HashMap::new(),
            },
            process: ProcessState {
                last_error: 0,
                next_registry_key_handle: crate::RegistryKeyHandle::from(0),
                registry_keys: Vec::new(),
                main_module_file_name: String::new(),
                main_module_path: String::new(),
                main_module_host_dir: None,
                error_mode: 0,
                suspended_threads: HashMap::new(),
                environment: DEFAULT_ENVIRONMENT
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                    .collect(),
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
            dll_states: DllStateMap::new(),
            message_queue: Arc::new(Mutex::new(crate::present::MessageQueue::default())),
            module_state: ModuleState {
                loaded_modules: HashMap::new(),
                import_resolver: None,
                get_proc_address_cache: HashMap::new(),
                next_module_handle: crate::ModuleHandle::from(
                    crate::dll_loader::REAL_MODULE_HANDLE_BASE,
                ),
            },
        }
    }

    fn write_regs(cpu: &mut IcedCpu, rcx: u64, rdx: u64, r8: u64, r9: u64) {
        cpu.write_rcx(rcx).ok();
        cpu.write_rdx(rdx).ok();
        cpu.write_r8(r8).ok();
        cpu.write_r9(r9).ok();
        cpu.write_rsp(STACK_TOP).ok();
    }

    /// Run one ole32 string-dispatch name and return its RAX value.
    fn ole(engine: &mut IcedCpu, state: &mut WinApiState, name: &str) -> u64 {
        let mut ctx = HandlerContext::new(engine, default_env(), state);
        let r = crate::ole32::dispatch_ole32(&mut ctx, name)
            .expect("handler must dispatch")
            .expect("dispatch arm must match");
        r.return_value
    }

    /// Run one clipboard string-dispatch name and return its RAX value.
    fn clip(engine: &mut IcedCpu, state: &mut WinApiState, name: &str) -> u64 {
        let mut ctx = HandlerContext::new(engine, default_env(), state);
        let r = crate::clipboard::dispatch_clipboard(&mut ctx, name)
            .expect("handler must dispatch")
            .expect("dispatch arm must match");
        r.return_value
    }

    fn read_guest_bytes(engine: &mut IcedCpu, addr: u64, len: usize) -> Vec<u8> {
        let mut bytes = vec![0_u8; len];
        engine.mem_read(addr, &mut bytes).expect("read guest bytes");
        bytes
    }

    fn write_guest_bytes(engine: &mut IcedCpu, addr: u64, bytes: &[u8]) {
        engine.mem_write(addr, bytes).expect("write guest bytes");
    }

    /// Write a guest FORMATETC (cfFormat @0, ptd @8, dwAspect @16, lindex @20,
    /// tymed @24).
    fn write_formatetc(engine: &mut IcedCpu, va: u64, cf_format: u32) {
        let mut bytes = vec![0_u8; 32];
        bytes[0..4].copy_from_slice(&cf_format.to_le_bytes());
        bytes[16..20].copy_from_slice(&1_u32.to_le_bytes()); // DVASPECT_CONTENT
        bytes[20..24].copy_from_slice(&(-1_i32).to_le_bytes()); // lindex = -1
        bytes[24..28].copy_from_slice(&1_u32.to_le_bytes()); // TYMED_HGLOBAL
        write_guest_bytes(engine, va, &bytes);
    }

    /// Write a guest STGMEDIUM (tymed @0, hGlobal @8, pUnkForRelease @16).
    fn write_stgmedium_test(engine: &mut IcedCpu, va: u64, hglobal: u64) {
        let mut bytes = vec![0_u8; 24];
        bytes[0..4].copy_from_slice(&1_u32.to_le_bytes()); // TYMED_HGLOBAL
        bytes[8..16].copy_from_slice(&hglobal.to_le_bytes());
        write_guest_bytes(engine, va, &bytes);
    }

    // ── vtable encoding pins ───────────────────────────────────────────

    #[test]
    fn idataobject_vtable_slots_encode_to_matching_preplanted_soft_entries() {
        for (slot, name) in crate::ole32::IDATAOBJECT_METHODS.iter().enumerate() {
            let va = crate::ole32::idataobject_method_va(slot).expect("slot must encode");
            match crate::decode_fake_va(va) {
                Some(FakeVa::Unresolved(idx)) => {
                    let entry = crate::dynamic_apis::PREPLANTED_SOFT_APIS
                        .get(usize::from(idx))
                        .expect("soft index must exist");
                    assert!(entry.library.eq_ignore_ascii_case("OLE32.dll"));
                    assert!(entry.name.eq_ignore_ascii_case(name));
                }
                other => panic!("slot {slot} must decode to Unresolved, got {other:?}"),
            }
        }
    }

    #[test]
    fn idataobject_method_names_have_no_winapi_id_collision() {
        // The soft names must stay out of the dense table so they route to
        // dispatch_ole32 (string path), never to a dense WinApiId.
        for name in crate::ole32::IDATAOBJECT_METHODS {
            assert!(
                resolve_winapi_id("ole32.dll", name).is_none(),
                "{name} must not resolve to a dense WinApiId"
            );
        }
    }

    // ── OLE clipboard round trip ───────────────────────────────────────

    #[test]
    fn ole_clipboard_setdata_getdata_round_trip() {
        let mut engine = test_engine();
        let mut state = winapi_state_default();

        assert_eq!(ole(&mut engine, &mut state, "OleInitialize"), S_OK);
        assert!(state.ole32().ole_initialized);

        // OleGetClipboard synthesizes the host object.
        write_regs(&mut engine, OUT_VA, 0, 0, 0);
        assert_eq!(ole(&mut engine, &mut state, "OleGetClipboard"), S_OK);
        let object = read_guest_bytes(&mut engine, OUT_VA, 8);
        let object = u64::from_le_bytes(object.try_into().expect("8 bytes"));
        assert_ne!(object, 0, "OleGetClipboard must return an object");
        let vtable = read_guest_bytes(&mut engine, object, 8);
        let vtable = u64::from_le_bytes(vtable.try_into().expect("8 bytes"));
        assert_eq!(vtable, object - 96, "lpVtbl points at the vtable block");

        // SetData(CF_TEXT, "hello ole") → GetData must round-trip.
        write_guest_bytes(&mut engine, BUF_VA, b"hello ole\0");
        write_formatetc(&mut engine, FORMATETC_VA, crate::clipboard::CF_TEXT);
        write_stgmedium_test(&mut engine, STGMEDIUM_VA, BUF_VA);
        write_regs(&mut engine, object, FORMATETC_VA, STGMEDIUM_VA, 1);
        assert_eq!(ole(&mut engine, &mut state, "IDataObject::SetData"), S_OK);
        assert!(
            state
                .ole32()
                .clipboard
                .has_format(crate::clipboard::CF_TEXT),
            "SetData must store under CF_TEXT"
        );

        write_regs(&mut engine, object, FORMATETC_VA, STGMEDIUM_VA, 0);
        assert_eq!(ole(&mut engine, &mut state, "IDataObject::GetData"), S_OK);
        let medium = read_guest_bytes(&mut engine, STGMEDIUM_VA, 24);
        let hglobal = u64::from_le_bytes(medium[8..16].try_into().expect("8 bytes"));
        assert_ne!(hglobal, 0, "GetData must allocate an HGLOBAL");
        let got = read_guest_bytes(&mut engine, hglobal, 10);
        assert_eq!(&got, b"hello ole\0", "GetData payload must match SetData");

        // QI returns the same object; AddRef/Release return 1.
        write_regs(&mut engine, object, 0x7777, OUT_VA, 0);
        assert_eq!(
            ole(&mut engine, &mut state, "IDataObject::QueryInterface"),
            S_OK
        );
        let qi = u64::from_le_bytes(read_guest_bytes(&mut engine, OUT_VA, 8).try_into().unwrap());
        assert_eq!(qi, object);
        write_regs(&mut engine, object, 0, 0, 0);
        assert_eq!(ole(&mut engine, &mut state, "IDataObject::AddRef"), 1);
        assert_eq!(ole(&mut engine, &mut state, "IDataObject::Release"), 1);
        assert_eq!(
            ole(&mut engine, &mut state, "IDataObject::EnumFormatEtc"),
            E_NOTIMPL
        );

        assert_eq!(ole(&mut engine, &mut state, "OleFlushClipboard"), S_OK);
        assert_eq!(ole(&mut engine, &mut state, "OleUninitialize"), 0);
        assert!(!state.ole32().ole_initialized);
    }

    #[test]
    fn ole_get_clipboard_prefers_stored_guest_object() {
        let mut engine = test_engine();
        let mut state = winapi_state_default();
        let guest_obj = 0x1234_5678;
        write_regs(&mut engine, guest_obj, 0, 0, 0);
        assert_eq!(ole(&mut engine, &mut state, "OleSetClipboard"), S_OK);
        write_regs(&mut engine, OUT_VA, 0, 0, 0);
        assert_eq!(ole(&mut engine, &mut state, "OleGetClipboard"), S_OK);
        let got = u64::from_le_bytes(read_guest_bytes(&mut engine, OUT_VA, 8).try_into().unwrap());
        assert_eq!(got, guest_obj, "OleGetClipboard returns the stored object");

        // NULL clears the stored object; a fresh synthesized one is returned.
        write_regs(&mut engine, 0, 0, 0, 0);
        assert_eq!(ole(&mut engine, &mut state, "OleSetClipboard"), S_OK);
        write_regs(&mut engine, OUT_VA, 0, 0, 0);
        assert_eq!(ole(&mut engine, &mut state, "OleGetClipboard"), S_OK);
        let got = u64::from_le_bytes(read_guest_bytes(&mut engine, OUT_VA, 8).try_into().unwrap());
        assert_ne!(got, guest_obj, "cleared object must not be returned");
        assert_eq!(got, state.ole32().synthesized_data_object);
    }

    #[test]
    fn get_data_unknown_format_answers_dv_e_formatetc() {
        let mut engine = test_engine();
        let mut state = winapi_state_default();
        let object = {
            write_regs(&mut engine, OUT_VA, 0, 0, 0);
            assert_eq!(ole(&mut engine, &mut state, "OleGetClipboard"), S_OK);
            u64::from_le_bytes(read_guest_bytes(&mut engine, OUT_VA, 8).try_into().unwrap())
        };
        write_formatetc(&mut engine, FORMATETC_VA, 0x1234); // never stored
        write_regs(&mut engine, object, FORMATETC_VA, STGMEDIUM_VA, 0);
        assert_eq!(
            ole(&mut engine, &mut state, "IDataObject::GetData"),
            DV_E_FORMATETC
        );
        // The medium must not have been written.
        let medium = read_guest_bytes(&mut engine, STGMEDIUM_VA, 24);
        assert_eq!(u64::from_le_bytes(medium[8..16].try_into().unwrap()), 0);
    }

    #[test]
    fn drag_drop_stub_cancels_with_none_effect() {
        let mut engine = test_engine();
        let mut state = winapi_state_default();
        write_regs(&mut engine, 0x1000, 0x2000, 0x0f, OUT_VA);
        assert_eq!(
            ole(&mut engine, &mut state, "DoDragDrop"),
            DRAGDROP_S_CANCEL
        );
        let effect =
            u64::from_le_bytes(read_guest_bytes(&mut engine, OUT_VA, 8).try_into().unwrap());
        assert_eq!(effect, DROPEFFECT_NONE);

        // Register/Revoke record (and drop) the per-window target.
        write_regs(&mut engine, 0xaaaa, 0xbbbb, 0, 0);
        assert_eq!(ole(&mut engine, &mut state, "RegisterDragDrop"), S_OK);
        assert_eq!(*state.ole32().drop_targets.get(&0xaaaa).unwrap(), 0xbbbb);
        write_regs(&mut engine, 0xaaaa, 0, 0, 0);
        assert_eq!(ole(&mut engine, &mut state, "RevokeDragDrop"), S_OK);
        assert!(!state.ole32().drop_targets.contains_key(&0xaaaa));
    }

    // ── Classic clipboard lane (dispatch_clipboard) ────────────────────

    #[test]
    fn classic_clipboard_register_open_set_get_enum_round_trip() {
        let mut engine = test_engine();
        let mut state = winapi_state_default();

        // RegisterClipboardFormatW: UTF-16 "WIE_TEST_FORMAT" → 0xC000.
        let units: Vec<u8> = "WIE_TEST_FORMAT"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        write_guest_bytes(&mut engine, BUF_VA, &units);
        write_guest_bytes(&mut engine, BUF_VA + 64, &[0, 0]); // NUL unit
        write_regs(&mut engine, BUF_VA, 0, 0, 0);
        let id = clip(&mut engine, &mut state, "RegisterClipboardFormatW");
        assert_eq!(id, 0xC000);
        // Same name again → same id.
        let id2 = clip(&mut engine, &mut state, "RegisterClipboardFormatW");
        assert_eq!(id2, 0xC000);

        // OpenClipboard once succeeds; a second open is denied.
        write_regs(&mut engine, 0xdead, 0, 0, 0);
        assert_eq!(clip(&mut engine, &mut state, "OpenClipboard"), 1);
        assert_eq!(clip(&mut engine, &mut state, "OpenClipboard"), 0);
        assert_eq!(state.process.last_error, 5);

        // SetClipboardData(CF_TEXT, buf) → GetClipboardData returns the bytes.
        write_guest_bytes(&mut engine, BUF_VA + 0x100, b"hello ole\0");
        write_regs(
            &mut engine,
            u64::from(crate::clipboard::CF_TEXT),
            BUF_VA + 0x100,
            0,
            0,
        );
        let returned = clip(&mut engine, &mut state, "SetClipboardData");
        assert_eq!(returned, BUF_VA + 0x100, "SetClipboardData returns hMem");
        assert!(
            state.clipboard().has_text(),
            "CF_TEXT must mirror into the edit slot"
        );

        write_regs(&mut engine, u64::from(crate::clipboard::CF_TEXT), 0, 0, 0);
        let hglobal = clip(&mut engine, &mut state, "GetClipboardData");
        assert_ne!(hglobal, 0);
        assert_eq!(read_guest_bytes(&mut engine, hglobal, 10), b"hello ole\0");

        // EnumClipboardFormats: 0 → CF_TEXT(1); a registered id with data
        // follows; then 0 at the end.
        write_regs(&mut engine, 0, 0, 0, 0);
        assert_eq!(clip(&mut engine, &mut state, "EnumClipboardFormats"), 1);
        // Store data under the registered id (0xC000) so it is enumerated.
        write_guest_bytes(&mut engine, BUF_VA + 0x200, b"custom\0");
        write_regs(&mut engine, 0xC000, BUF_VA + 0x200, 0, 0);
        assert_eq!(
            clip(&mut engine, &mut state, "SetClipboardData"),
            BUF_VA + 0x200
        );
        write_regs(&mut engine, 1, 0, 0, 0);
        assert_eq!(
            clip(&mut engine, &mut state, "EnumClipboardFormats"),
            0xC000
        );
        write_regs(&mut engine, 0xC000, 0, 0, 0);
        assert_eq!(clip(&mut engine, &mut state, "EnumClipboardFormats"), 0);

        // IsClipboardFormatAvailable sees the stored format.
        write_regs(&mut engine, u64::from(crate::clipboard::CF_TEXT), 0, 0, 0);
        assert_eq!(
            crate::clipboard::handle_is_clipboard_format_available(&mut HandlerContext::new(
                &mut engine,
                default_env(),
                &mut state,
            ))
            .expect("dispatch")
            .return_value,
            1
        );

        // CloseClipboard; ops after close are denied.
        assert_eq!(clip(&mut engine, &mut state, "CloseClipboard"), 1);
        write_regs(&mut engine, u64::from(crate::clipboard::CF_TEXT), 0, 0, 0);
        assert_eq!(clip(&mut engine, &mut state, "GetClipboardData"), 0);
        assert_eq!(state.process.last_error, 5);

        // EmptyClipboard clears payloads but keeps the registry.
        write_regs(&mut engine, 0, 0, 0, 0);
        assert_eq!(clip(&mut engine, &mut state, "OpenClipboard"), 1);
        assert_eq!(clip(&mut engine, &mut state, "EmptyClipboard"), 1);
        assert!(
            !state
                .ole32()
                .clipboard
                .has_format(crate::clipboard::CF_TEXT)
        );
        assert!(
            state
                .ole32()
                .clipboard
                .registered_format_id("wie_test_format")
                .is_some(),
            "registered names survive EmptyClipboard"
        );
        assert_eq!(clip(&mut engine, &mut state, "CloseClipboard"), 1);
    }

    // ── ClipboardStore pure logic ──────────────────────────────────────

    #[test]
    fn clipboard_store_registry_and_open_semantics() {
        let mut store = crate::clipboard::ClipboardStore::default();
        assert_eq!(store.register_format("WIE_A"), 0xC000);
        assert_eq!(store.register_format("wie_a"), 0xC000, "case-insensitive");
        assert_eq!(store.register_format("WIE_B"), 0xC001);
        assert!(!store.is_open());
        assert!(store.open_with(7));
        assert!(!store.open_with(8), "open-while-open fails");
        assert!(store.is_open());
        assert!(store.close());
        assert!(!store.close(), "close-while-closed fails");

        store.set_data(1, b"x\0".to_vec());
        store.set_data(0xC000, vec![1, 2, 3]);
        assert!(store.has_format(1));
        assert_eq!(store.next_format(0), 1);
        assert_eq!(store.next_format(1), 0xC000);
        assert_eq!(store.next_format(0xC000), 0);
        store.clear_data();
        assert!(!store.has_format(1));
        assert!(store.registered_format_id("WIE_A").is_some());
    }

    #[test]
    fn ole_uninitialize_and_dispatch_unknown_names() {
        let mut engine = test_engine();
        let mut state = winapi_state_default();
        let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
        assert!(
            crate::ole32::dispatch_ole32(&mut ctx, "NoSuchOleExport")
                .expect("dispatch must not error")
                .is_none(),
            "unknown ole32 names must fall through"
        );
        assert!(
            crate::clipboard::dispatch_clipboard(&mut ctx, "NoSuchClipboardExport")
                .expect("dispatch must not error")
                .is_none()
        );
    }
}
