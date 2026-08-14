//! Minimal `ole32.dll` stubs for COM-touching CLI tools (e.g. 7-Zip).
//!
//! The class-registration table is real (CoRegisterClassObject → CoCreateInstance
//! finds the class); the resulting class objects are fake opaque pointers.
//!
//! The OLE clipboard / drag-drop lane (OleInitialize, OleSet/GetClipboard,
//! the host-synthesized `IDataObject`, DoDragDrop stubs) lives in the
//! `ole_clipboard` submodule; the string arms below dispatch to it.

use crate::clipboard::ClipboardStore;
use crate::guest_string::write_utf16_units;
use crate::{HandlerContext, WinApiHandlerResult};
use ahash::HashMap;
use ahash::HashMapExt;
use anyhow::{Context, Result};
use std::io::Read;

/// `S_OK`
const S_OK: u64 = 0;
/// `S_FALSE` — COM already initialized on this thread (acceptable).
#[allow(dead_code)]
const S_FALSE: u64 = 1;
/// `REGDB_E_CLASSNOTREG` — class not registered (no real COM servers).
const REGDB_E_CLASSNOTREG: u64 = 0x8004_0154;
/// `CO_E_OBJNOTREG` — CoRevokeClassObject cookie not found.
const CO_E_OBJNOTREG: u64 = 0x8004_01FE;
/// `CO_E_CLASSSTRING` — CLSIDFromString could not parse the string.
const CO_E_CLASSSTRING: u64 = 0x8004_01F3;
/// Fake `IUnknown*` handed to guests for registered classes (opaque).
const FAKE_IUNKNOWN: u64 = 0x5200_0001;

/// The IDataObject vtable ABI. Order IS the vtable slot (0..12), matching the
/// Windows SDK `IDataObjectVtbl`; the session preplants these names into the
/// soft table at stable indices (see `dynamic_apis::PREPLANTED_SOFT_APIS`) so
/// each slot encodes to a resolvable fake VA via `encode_unresolved`.
pub(crate) const IDATAOBJECT_METHODS: [&str; 12] = [
    "IDataObject::QueryInterface",
    "IDataObject::AddRef",
    "IDataObject::Release",
    "IDataObject::GetData",
    "IDataObject::GetDataHere",
    "IDataObject::QueryGetData",
    "IDataObject::GetCanonicalFormatEtc",
    "IDataObject::SetData",
    "IDataObject::EnumFormatEtc",
    "IDataObject::DAdvise",
    "IDataObject::DUnadvise",
    "IDataObject::EnumDAdvise",
];

/// `IDataObject` vtable + COM object allocation: 12 slot pointers + the
/// object struct (a single `lpVtbl` pointer).
pub(crate) const IDATAOBJECT_ALLOCATION_SIZE: u64 = 13 * 8;

/// OLE clipboard / drag-drop lane (the `ole_clipboard` submodule; kept here
/// behind a `#[path]` because `lib.rs` is frozen and cannot declare it).
#[path = "ole_clipboard.rs"]
mod ole_clipboard;

/// COM class-registration table (`CoRegisterClassObject`), owned by this
/// module and heap-allocated on first load via `DllId::Ole32`.
#[derive(Debug)]
pub struct OleState {
    /// Registered classes: 16-byte CLSID → registration cookie.
    classes: HashMap<[u8; 16], u32>,
    /// Next registration cookie (starts at 1; 0 is never handed out).
    next_cookie: u32,
    /// The guest-side clipboard (classic USER32 + OLE share this store; see
    /// `crate::clipboard`). Owned here because `DllId`/state accessors are
    /// frozen — this is the only shared-state slot this lane may grow.
    pub clipboard: ClipboardStore,
    /// `OleInitialize` per-process flag.
    pub ole_initialized: bool,
    /// Guest `IDataObject*` from `OleSetClipboard` (0 = none stored).
    pub clipboard_data_object: u64,
    /// Host-synthesized `IDataObject` guest block, created on first
    /// `OleGetClipboard` when no guest object is stored.
    pub synthesized_data_object: u64,
    /// `RegisterDragDrop` table: hwnd → guest `IDropTarget*`.
    pub drop_targets: HashMap<u64, u64>,
}

impl Default for OleState {
    fn default() -> Self {
        Self {
            classes: HashMap::new(),
            next_cookie: 1,
            clipboard: ClipboardStore::default(),
            ole_initialized: false,
            clipboard_data_object: 0,
            synthesized_data_object: 0,
            drop_targets: HashMap::new(),
        }
    }
}

fn finish(engine: &mut dyn wie_cpu::CpuEngine, value: u64) -> Result<WinApiHandlerResult> {
    let return_address = engine
        .return_from_win64_api(value)
        .context("ole32 return")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: value,
    })
}

/// Read the 5th Win64 stack argument (`[RSP+0x28]` at handler entry).
fn stack_arg5(engine: &mut dyn wie_cpu::CpuEngine) -> Result<u64> {
    let rsp = engine.read_rsp()?;
    let mut bytes = [0_u8; 8];
    engine.mem_read(rsp.wrapping_add(0x28), &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

/// Read the 16-byte GUID at `va` (all-zero when `va` is null).
fn read_guid(engine: &mut dyn wie_cpu::CpuEngine, va: u64) -> Result<[u8; 16]> {
    let mut guid = [0_u8; 16];
    if va != 0 {
        engine.mem_read(va, &mut guid)?;
    }
    Ok(guid)
}

/// Soft dispatch for `ole32.dll` exports used by real tools.
pub fn dispatch_ole32(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "coinitialize" => Ok(Some(handle_co_initialize(ctx)?)),
        "coinitializeex" => Ok(Some(handle_co_initialize_ex(ctx)?)),
        "couninitialize" => Ok(Some(handle_co_uninitialize(ctx)?)),
        "cocreateinstance" => Ok(Some(handle_co_create_instance(ctx)?)),
        "coregisterclassobject" => Ok(Some(handle_co_register_class_object(ctx)?)),
        "corevokeclassobject" => Ok(Some(handle_co_revoke_class_object(ctx)?)),
        "cocreateguid" => Ok(Some(handle_co_create_guid(ctx)?)),
        "cotaskmemalloc" => Ok(Some(handle_co_task_mem_alloc(ctx)?)),
        "cotaskmemfree" => Ok(Some(handle_co_task_mem_free(ctx)?)),
        "cotaskmemrealloc" => Ok(Some(handle_co_task_mem_realloc(ctx)?)),
        "stringfromclsid" => Ok(Some(handle_string_from_clsid(ctx)?)),
        "clsidfromstring" => Ok(Some(handle_clsid_from_string(ctx)?)),
        "cogetclassobject" => Ok(Some(handle_co_get_class_object(ctx)?)),
        // Phase-3 stub wave.
        "propvariantclear" => Ok(Some(handle_prop_variant_clear(ctx)?)),
        // ── OLE clipboard / drag-drop lane ─────────────────────────────
        "oleinitialize" => Ok(Some(ole_clipboard::handle_ole_initialize(ctx)?)),
        "oleuninitialize" => Ok(Some(ole_clipboard::handle_ole_uninitialize(ctx)?)),
        "olesetclipboard" => Ok(Some(ole_clipboard::handle_ole_set_clipboard(ctx)?)),
        "olegetclipboard" => Ok(Some(ole_clipboard::handle_ole_get_clipboard(ctx)?)),
        "oleflushclipboard" => Ok(Some(ole_clipboard::handle_ole_flush_clipboard(ctx)?)),
        "registerdragdrop" => Ok(Some(ole_clipboard::handle_register_drag_drop(ctx)?)),
        "revokedragdrop" => Ok(Some(ole_clipboard::handle_revoke_drag_drop(ctx)?)),
        "dodragdrop" => Ok(Some(ole_clipboard::handle_do_drag_drop(ctx)?)),
        // Host-synthesized IDataObject vtable slots (soft-dispatch names —
        // the session preplants them; see dynamic_apis.rs).
        "idataobject::queryinterface" => Ok(Some(ole_clipboard::handle_query_interface(ctx)?)),
        "idataobject::addref" => Ok(Some(ole_clipboard::handle_add_ref(ctx)?)),
        "idataobject::release" => Ok(Some(ole_clipboard::handle_release(ctx)?)),
        "idataobject::getdata" => Ok(Some(ole_clipboard::handle_get_data(ctx)?)),
        "idataobject::setdata" => Ok(Some(ole_clipboard::handle_set_data(ctx)?)),
        "idataobject::enumformatetc" => Ok(Some(ole_clipboard::handle_enum_format_etc(ctx)?)),
        "idataobject::getdatahere"
        | "idataobject::querygetdata"
        | "idataobject::getcanonicalformatetc"
        | "idataobject::dadvise"
        | "idataobject::dunadvise"
        | "idataobject::enumdadvise" => Ok(Some(ole_clipboard::handle_e_notimpl(ctx)?)),
        _ => Ok(None),
    }
}

/// The fake VA for IDataObject vtable slot `slot` (0..12).
///
/// The slot encodes as a soft-table unresolved VA: the session preplants the
/// `IDataObject::*` names at stable indices (dynamic_apis.rs), so the slot's
/// call stops in the fake-API window and resolves to `ole32.dll!IDataObject::X`,
/// which `dispatch_ole32` routes to the `ole_clipboard` lane.
pub(crate) fn idataobject_method_va(slot: usize) -> Result<u64> {
    let name = IDATAOBJECT_METHODS
        .get(slot)
        .with_context(|| format!("IDataObject method slot {slot} out of range"))?;
    let idx = crate::dynamic_apis::PREPLANTED_SOFT_APIS
        .iter()
        .position(|entry| {
            entry.library.eq_ignore_ascii_case("OLE32.dll") && entry.name.eq_ignore_ascii_case(name)
        })
        .with_context(|| {
            format!("IDataObject slot {slot} ({name}) missing from PREPLANTED_SOFT_APIS")
        })?;
    let idx = u16::try_from(idx).context("IDataObject soft-table index does not fit u16")?;
    Ok(crate::fake_va::encode_unresolved(idx))
}

/// `HRESULT CoInitialize(LPVOID pvReserved)`
fn handle_co_initialize(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _reserved = engine.read_rcx()?;
    finish(engine, S_OK)
}

/// `HRESULT CoInitializeEx(LPVOID, DWORD)`
fn handle_co_initialize_ex(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _reserved = engine.read_rcx()?;
    let _coinit = engine.read_rdx()?;
    finish(engine, S_OK)
}

/// `void CoUninitialize(void)`
fn handle_co_uninitialize(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    finish(engine, 0)
}

/// `HRESULT CoCreateInstance(rclsid, pUnkOuter, dwClsContext, riid, ppv)`
///
/// Registered classes get a fake `IUnknown*`; everything else is
/// `REGDB_E_CLASSNOTREG` with `*ppv = NULL` so callers take the non-COM path.
fn handle_co_create_instance(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let clsid_va = engine.read_rcx()?;
    let _outer = engine.read_rdx()?;
    let _ctx = engine.read_r8()?;
    let _iid = engine.read_r9()?;
    let ppv = stack_arg5(engine)?;
    let guid = read_guid(engine, clsid_va)?;
    if ctx.state.ole32().classes.contains_key(&guid) {
        if ppv != 0 {
            engine.mem_write(ppv, &FAKE_IUNKNOWN.to_le_bytes())?;
        }
        finish(engine, S_OK)
    } else {
        if ppv != 0 {
            engine.mem_write(ppv, &0_u64.to_le_bytes())?;
        }
        finish(engine, REGDB_E_CLASSNOTREG)
    }
}

/// `HRESULT CoRegisterClassObject(rclsid, dwClsContext, pUnk, dwRegClsContext, lpdwRegister)`
///
/// Registers the CLSID and writes the cookie to `*lpdwRegister`.
fn handle_co_register_class_object(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let clsid_va = engine.read_rcx()?;
    let _cls_ctx = engine.read_rdx()?;
    let _punk = engine.read_r8()?;
    let _reg_cls_ctx = engine.read_r9()?;
    let lpdw_register = stack_arg5(engine)?;
    let guid = read_guid(engine, clsid_va)?;
    let state = ctx.state.ole32();
    let cookie = state.next_cookie;
    state.next_cookie = state.next_cookie.saturating_add(1);
    state.classes.insert(guid, cookie);
    if lpdw_register != 0 {
        engine.mem_write(lpdw_register, &cookie.to_le_bytes())?;
    }
    finish(engine, S_OK)
}

/// `HRESULT CoRevokeClassObject(DWORD dwRegister)`
fn handle_co_revoke_class_object(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let cookie = u32::try_from(engine.read_rcx()? & 0xffff_ffff).unwrap_or(0);
    let state = ctx.state.ole32();
    let mut revoke_key: Option<[u8; 16]> = None;
    for (key, value) in &state.classes {
        if *value == cookie {
            revoke_key = Some(*key);
            break;
        }
    }
    if let Some(key) = revoke_key {
        state.classes.remove(&key);
        finish(engine, S_OK)
    } else {
        finish(engine, CO_E_OBJNOTREG)
    }
}

/// `HRESULT CoCreateGuid(GUID *pguid)` — 16 random bytes from `/dev/urandom`.
fn handle_co_create_guid(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let pguid = engine.read_rcx()?;
    if pguid != 0 {
        let mut rng = std::fs::File::open("/dev/urandom").context("open /dev/urandom")?;
        let mut bytes = [0_u8; 16];
        rng.read_exact(&mut bytes).context("read /dev/urandom")?;
        engine.mem_write(pguid, &bytes)?;
    }
    finish(engine, S_OK)
}

/// `LPVOID CoTaskMemAlloc(SIZE_T cb)` — process-heap allocation, returned
/// as a raw payload VA so `CoTaskMemFree` (heap-free by pointer) matches.
fn handle_co_task_mem_alloc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let cb = engine.read_rcx()?;
    let va = ctx.state.heap_state.heap.alloc_coherent(engine, cb);
    finish(engine, va)
}

/// `void CoTaskMemFree(LPVOID pv)`
fn handle_co_task_mem_free(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let pv = engine.read_rcx()?;
    if pv != 0 {
        let _ = ctx.state.heap_state.heap.free_coherent(engine, pv);
    }
    finish(engine, 0)
}

/// `LPVOID CoTaskMemRealloc(LPVOID pv, SIZE_T cb)`
///
/// Reallocates through the guest heap: in-place when the existing block is
/// large enough, otherwise allocate-copy-free.
fn handle_co_task_mem_realloc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let pv = engine.read_rcx()?;
    let cb = engine.read_rdx()?;
    let heap = &mut ctx.state.heap_state.heap;
    if pv == 0 {
        let va = heap.alloc_coherent(engine, cb);
        return finish(engine, va);
    }
    if cb == 0 {
        // Real CoTaskMemRealloc(pv, 0) frees and returns NULL.
        let _ = heap.free_coherent(engine, pv);
        return finish(engine, 0);
    }
    if let Some(same) = heap.try_realloc_in_place(pv, cb) {
        return finish(engine, same);
    }
    let old_size = heap.size_of(pv).unwrap_or(0);
    let new_va = heap.alloc_coherent(engine, cb);
    if new_va == 0 {
        return finish(engine, 0);
    }
    let copy_len = usize::try_from(old_size.min(cb)).unwrap_or(0);
    if copy_len > 0 {
        let mut buf = vec![0_u8; copy_len];
        engine.mem_read(pv, &mut buf)?;
        engine.mem_write(new_va, &buf)?;
    }
    let _ = heap.free_coherent(engine, pv);
    finish(engine, new_va)
}

/// `HRESULT StringFromCLSID(REFCLSID rclsid, LPOLESTR *lplpsz)`
///
/// Allocates the formatted string through the guest heap (CoTaskMemAlloc
/// semantics) and stores the pointer in `*lplpsz` so the caller can
/// `CoTaskMemFree` it.
fn handle_string_from_clsid(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let clsid_va = engine.read_rcx()?;
    let lplpsz = engine.read_rdx()?;
    let guid = read_guid(engine, clsid_va)?;
    let s = format_guid(&guid);
    let units: Vec<u16> = s.encode_utf16().collect();
    let byte_len = u64::try_from(units.len().saturating_mul(2)).unwrap_or(0);
    // NUL terminator + alignment slop; the payload VA is the CoTaskMemFree key.
    let total = byte_len.saturating_add(2).saturating_add(8);
    let data = ctx.state.heap_state.heap.alloc_coherent(engine, total);
    if data != 0 {
        write_utf16_units(engine, data, &units)?;
        engine.mem_write(data.wrapping_add(byte_len), &0_u16.to_le_bytes())?;
        if lplpsz != 0 {
            engine.mem_write(lplpsz, &data.to_le_bytes())?;
        }
    }
    finish(engine, S_OK)
}

/// `HRESULT CLSIDFromString(LPCOLESTR lpsz, LPCLSID pclsid)`
fn handle_clsid_from_string(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let lpsz = engine.read_rcx()?;
    let pclsid = engine.read_rdx()?;
    let s = crate::guest_string::read_utf16_lossy(engine, lpsz, 64).unwrap_or_default();
    let Some(guid) = parse_guid(&s) else {
        return finish(engine, CO_E_CLASSSTRING);
    };
    if pclsid != 0 {
        engine.mem_write(pclsid, &guid)?;
    }
    finish(engine, S_OK)
}

/// `HRESULT CoGetClassObject(rclsid, dwClsContext, reserved, riid, ppv)`
fn handle_co_get_class_object(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let clsid_va = engine.read_rcx()?;
    let _cls_ctx = engine.read_rdx()?;
    let _reserved = engine.read_r8()?;
    let _riid = engine.read_r9()?;
    let ppv = stack_arg5(engine)?;
    let guid = read_guid(engine, clsid_va)?;
    if ctx.state.ole32().classes.contains_key(&guid) {
        if ppv != 0 {
            engine.mem_write(ppv, &FAKE_IUNKNOWN.to_le_bytes())?;
        }
        finish(engine, S_OK)
    } else {
        if ppv != 0 {
            engine.mem_write(ppv, &0_u64.to_le_bytes())?;
        }
        finish(engine, REGDB_E_CLASSNOTREG)
    }
}

/// `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}` (uppercase hex, Windows layout:
/// Data1/Data2/Data3 as little-endian values, Data4 in byte order).
fn format_guid(guid: &[u8; 16]) -> String {
    let b = |i: usize| guid.get(i).copied().unwrap_or(0);
    let d1 = u32::from_le_bytes([b(0), b(1), b(2), b(3)]);
    let d2 = u16::from_le_bytes([b(4), b(5)]);
    let d3 = u16::from_le_bytes([b(6), b(7)]);
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        d1,
        d2,
        d3,
        b(8),
        b(9),
        b(10),
        b(11),
        b(12),
        b(13),
        b(14),
        b(15)
    )
}

/// Parse the `StringFromCLSID` output back into the 16 CLSID bytes.
///
/// Data1/Data2/Data3 appear in the string as big-endian hex of the u32/u16
/// values but are stored little-endian in the GUID; Data4 is byte order.
fn parse_guid(s: &str) -> Option<[u8; 16]> {
    let hex = s.trim().strip_prefix('{')?.strip_suffix('}')?;
    let parts: Vec<&str> = hex.split('-').collect();
    if parts.len() != 5 {
        return None;
    }
    let g1 = parts.first()?;
    let g2 = parts.get(1)?;
    let g3 = parts.get(2)?;
    let g4 = parts.get(3)?;
    let g5 = parts.get(4)?;
    if g1.len() != 8 || g2.len() != 4 || g3.len() != 4 || g4.len() != 4 || g5.len() != 12 {
        return None;
    }
    let mut out = [0_u8; 16];
    let d1 = u32::from_str_radix(g1, 16).ok()?;
    let d2 = u16::from_str_radix(g2, 16).ok()?;
    let d3 = u16::from_str_radix(g3, 16).ok()?;
    for (i, v) in d1.to_le_bytes().iter().enumerate() {
        *out.get_mut(i)? = *v;
    }
    for (i, v) in d2.to_le_bytes().iter().enumerate() {
        *out.get_mut(4 + i)? = *v;
    }
    for (i, v) in d3.to_le_bytes().iter().enumerate() {
        *out.get_mut(6 + i)? = *v;
    }
    for (i, pair) in g4.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(pair.first().copied()?)?;
        let lo = hex_nibble(pair.get(1).copied()?)?;
        *out.get_mut(8 + i)? = (hi << 4) | lo;
    }
    for (i, pair) in g5.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(pair.first().copied()?)?;
        let lo = hex_nibble(pair.get(1).copied()?)?;
        *out.get_mut(10 + i)? = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// `HRESULT PropVariantClear(PROPVARIANT *pvar)` — zeroes the 16-byte
/// `PROPVARIANT` (the union's `vt` is set to `VT_EMPTY` = 0) and returns
/// `S_OK`. No heap payloads are owned, so the clear is a plain zero.
fn handle_prop_variant_clear(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let variant_va = engine
        .read_rcx()
        .context("failed to read RCX for PropVariantClear")?;
    if variant_va != 0 {
        engine
            .mem_write(variant_va, &[0_u8; 16])
            .context("failed to zero PROPVARIANT")?;
    }
    finish(engine, S_OK)
}
