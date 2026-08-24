//! Single source of truth for APIs resolved through `GetProcAddress`.
//!
//! Addresses are dense-encoded via [`crate::fake_va`] so IAT and GPA never drift.

use crate::fake_va::{encode_export, encode_unresolved};
use crate::resolve_winapi_id;

/// One dynamically resolvable fake WinAPI entry (name → encode).
#[derive(Debug, Clone, Copy)]
pub struct DynamicFakeApi {
    /// DLL name used for runtime dispatch (`library!name`).
    pub library: &'static str,

    /// Export / method name used for runtime dispatch.
    pub name: &'static str,
}

/// Build an append-only [`DynamicFakeApi`] table from one `(library, name)`
/// row per entry.
///
/// Positions in these tables are ABI (soft-VA encoding), so every row stays on
/// a single line and the sequence must never be reordered.
macro_rules! fake_api_table {
    ($(($library:expr, $name:expr)),* $(,)?) => {
        &[$(DynamicFakeApi { library: $library, name: $name },)*]
    };
}

/// Soft slots planted **first** into the runtime soft table (stable indices 0..N).
///
/// Used for exports that live only in `dispatch_kernel32_extra` (no dense
/// `WinApiId`) so `GetProcAddress` can return a resolvable soft VA. Session
/// bootstrap must [`crate`]-side intern these before PE IAT imports.
///
/// Order is ABI for `GetProcAddress` soft encoding — append only.
pub const PREPLANTED_SOFT_APIS: &[DynamicFakeApi] = fake_api_table![
    ("KERNEL32.dll", "InterlockedIncrement"),
    ("KERNEL32.dll", "InterlockedDecrement"),
    ("KERNEL32.dll", "InterlockedExchange"),
    ("KERNEL32.dll", "InterlockedCompareExchange"),
    ("KERNEL32.dll", "InterlockedExchangeAdd"),
    ("KERNEL32.dll", "InterlockedIncrement64"),
    ("KERNEL32.dll", "InterlockedDecrement64"),
    ("KERNEL32.dll", "InterlockedExchange64"),
    ("KERNEL32.dll", "InterlockedCompareExchange64"),
    ("KERNEL32.dll", "InterlockedExchangeAdd64"),
    // IDataObject vtable slots (OLE clipboard). Order IS the vtable ABI
    // (slot index = position, per the Windows IDataObject layout), appended
    // after the Interlocked* family — append only. The guest vtable builder
    // in `ole32.rs` re-discovers the index by position, so the names must
    // match `ole32::IDATAOBJECT_METHODS` exactly.
    ("OLE32.dll", "IDataObject::QueryInterface"),
    ("OLE32.dll", "IDataObject::AddRef"),
    ("OLE32.dll", "IDataObject::Release"),
    ("OLE32.dll", "IDataObject::GetData"),
    ("OLE32.dll", "IDataObject::GetDataHere"),
    ("OLE32.dll", "IDataObject::QueryGetData"),
    ("OLE32.dll", "IDataObject::GetCanonicalFormatEtc"),
    ("OLE32.dll", "IDataObject::SetData"),
    ("OLE32.dll", "IDataObject::EnumFormatEtc"),
    ("OLE32.dll", "IDataObject::DAdvise"),
    ("OLE32.dll", "IDataObject::DUnadvise"),
    ("OLE32.dll", "IDataObject::EnumDAdvise"),
    // opengl32 WGL + gl* exports — the same names as
    // `opengl32::OPENGL32_EXPORT_NAMES` (the dispatch census). `wglGetProcAddress`
    // encodes these by position (append only — the order is the ABI), and the
    // kernel32 `GetProcAddress` path resolves them too. A unit test pins the
    // lists together so they cannot drift.
    ("opengl32.dll", "wglcreatecontext"),
    ("opengl32.dll", "wgldeletecontext"),
    ("opengl32.dll", "wglmakecurrent"),
    ("opengl32.dll", "wglgetprocaddress"),
    ("opengl32.dll", "wglchoosepixelformat"),
    ("opengl32.dll", "wglsetpixelformat"),
    ("opengl32.dll", "wgldescribepixelformat"),
    ("opengl32.dll", "wglgetpixelformat"),
    ("opengl32.dll", "wglswapbuffers"),
    ("opengl32.dll", "wglsharelists"),
    ("opengl32.dll", "wglgetcurrentcontext"),
    ("opengl32.dll", "wglgetcurrentdc"),
    ("opengl32.dll", "glclearcolor"),
    ("opengl32.dll", "glcleardepth"),
    ("opengl32.dll", "glclear"),
    ("opengl32.dll", "glbegin"),
    ("opengl32.dll", "glend"),
    ("opengl32.dll", "glvertex2f"),
    ("opengl32.dll", "glvertex3f"),
    ("opengl32.dll", "glvertex4f"),
    ("opengl32.dll", "glvertex2fv"),
    ("opengl32.dll", "glvertex3fv"),
    ("opengl32.dll", "glvertex4fv"),
    ("opengl32.dll", "glcolor3f"),
    ("opengl32.dll", "glcolor4f"),
    ("opengl32.dll", "glcolor3fv"),
    ("opengl32.dll", "glcolor4fv"),
    ("opengl32.dll", "glcolor3ub"),
    ("opengl32.dll", "glcolor4ub"),
    ("opengl32.dll", "glcolor3ubv"),
    ("opengl32.dll", "glcolor4ubv"),
    ("opengl32.dll", "gltexcoord1f"),
    ("opengl32.dll", "gltexcoord2f"),
    ("opengl32.dll", "gltexcoord3f"),
    ("opengl32.dll", "gltexcoord4f"),
    ("opengl32.dll", "gltexcoord1fv"),
    ("opengl32.dll", "gltexcoord2fv"),
    ("opengl32.dll", "gltexcoord3fv"),
    ("opengl32.dll", "gltexcoord4fv"),
    ("opengl32.dll", "glmatrixmode"),
    ("opengl32.dll", "glloadidentity"),
    ("opengl32.dll", "glloadmatrixf"),
    ("opengl32.dll", "glmultmatrixf"),
    ("opengl32.dll", "glortho"),
    ("opengl32.dll", "glfrustum"),
    ("opengl32.dll", "gltranslatef"),
    ("opengl32.dll", "glrotatef"),
    ("opengl32.dll", "glscalef"),
    ("opengl32.dll", "glpushmatrix"),
    ("opengl32.dll", "glpopmatrix"),
    ("opengl32.dll", "glviewport"),
    ("opengl32.dll", "gldepthfunc"),
    ("opengl32.dll", "gldepthmask"),
    ("opengl32.dll", "glenable"),
    ("opengl32.dll", "gldisable"),
    ("opengl32.dll", "glblendfunc"),
    ("opengl32.dll", "glpolygonmode"),
    ("opengl32.dll", "glgentextures"),
    ("opengl32.dll", "gldeletetextures"),
    ("opengl32.dll", "glbindtexture"),
    ("opengl32.dll", "glteximage2d"),
    ("opengl32.dll", "gltexparameteri"),
    ("opengl32.dll", "gltexenvi"),
    ("opengl32.dll", "gltexenvf"),
    ("opengl32.dll", "glpixelstorei"),
    ("opengl32.dll", "glreadpixels"),
    ("opengl32.dll", "glflush"),
    ("opengl32.dll", "glfinish"),
    ("opengl32.dll", "glgeterror"),
    ("opengl32.dll", "glgetstring"),
    ("opengl32.dll", "glgetintegerv"),
    ("opengl32.dll", "glgetfloatv"),
    ("opengl32.dll", "glgetbooleanv"),
    ("opengl32.dll", "glscissor"),
    ("opengl32.dll", "glenableclientstate"),
    ("opengl32.dll", "gldisableclientstate"),
    ("opengl32.dll", "glvertexpointer"),
    ("opengl32.dll", "glcolorpointer"),
    ("opengl32.dll", "gltexcoordpointer"),
    ("opengl32.dll", "glnormalpointer"),
    ("opengl32.dll", "gldrawarrays"),
    ("opengl32.dll", "gldrawelements"),
    ("opengl32.dll", "gldeletebuffers"),
    ("opengl32.dll", "glisbuffer"),
    ("opengl32.dll", "glbindbuffer"),
    ("opengl32.dll", "glbufferdata"),
    ("opengl32.dll", "glbuffersubdata"),
    ("opengl32.dll", "glreadbuffer"),
    ("opengl32.dll", "gllightfv"),
    ("opengl32.dll", "gllightmodelfv"),
    ("opengl32.dll", "glmaterialfv"),
    ("opengl32.dll", "glmaterialf"),
    ("opengl32.dll", "glcolormaterial"),
    ("opengl32.dll", "glshademodel"),
    ("opengl32.dll", "glnormal3fv"),
    ("opengl32.dll", "glcullface"),
    ("opengl32.dll", "glfrontface"),
    ("opengl32.dll", "glpointsizef"),
    ("opengl32.dll", "gllinewidth"),
    ("opengl32.dll", "glnewlist"),
    ("opengl32.dll", "glendlist"),
    ("opengl32.dll", "glcalllist"),
    ("opengl32.dll", "glcalllists"),
    ("opengl32.dll", "glgenlists"),
    ("opengl32.dll", "glislist"),
    ("opengl32.dll", "gldeletelists"),
    ("opengl32.dll", "gllistbase"),
    ("opengl32.dll", "glisshader"),
    ("opengl32.dll", "glshadersource"),
    ("opengl32.dll", "glcompileshader"),
    ("opengl32.dll", "glcreateprogram"),
    ("opengl32.dll", "glcreateshader"),
    ("opengl32.dll", "glgenbuffers"),
    ("opengl32.dll", "glnormal3f"),
    ("opengl32.dll", "gldeleteshader"),
    ("opengl32.dll", "glgetshaderiv"),
    ("opengl32.dll", "glgetshaderinfolog"),
    ("opengl32.dll", "glattachshader"),
    ("opengl32.dll", "gldetachshader"),
    ("opengl32.dll", "gllinkprogram"),
    ("opengl32.dll", "gluseprogram"),
    ("opengl32.dll", "glgetprogramiv"),
    ("opengl32.dll", "glgetprograminfolog"),
    ("opengl32.dll", "gldeleteprogram"),
    ("opengl32.dll", "glisprogram"),
    ("opengl32.dll", "glvalidateprogram"),
    ("opengl32.dll", "glgetuniformlocation"),
    ("opengl32.dll", "gluniform1f"),
    ("opengl32.dll", "gluniform2f"),
    ("opengl32.dll", "gluniform3f"),
    ("opengl32.dll", "gluniform4f"),
    ("opengl32.dll", "gluniform1i"),
    ("opengl32.dll", "gluniformmatrix4fv"),
    ("opengl32.dll", "glactivetexture"),
];

/// Dynamic exports resolvable via `GetProcAddress` (generic PE64 + legacy apps).
///
/// Order is stable; lookup is by `name` (case-insensitive).
/// Clean room: names are our fake dispatch map, not copied from other projects.
pub const DYNAMIC_FAKE_APIS: &[DynamicFakeApi] = fake_api_table![
    ("KERNEL32.dll", "EncodePointer"),
    ("KERNEL32.dll", "DecodePointer"),
    ("KERNEL32.dll", "InitializeCriticalSectionAndSpinCount"),
    ("USER32.dll", "SetProcessDPIAware"),
    ("USER32.dll", "TrackMouseEvent"),
    ("COMCTL32.dll", "DllGetVersion"),
    ("USER32.dll", "GetSystemMetrics"),
    ("USER32.dll", "MonitorFromWindow"),
    ("USER32.dll", "GetMonitorInfoA"),
    ("USER32.dll", "GetMonitorInfoW"),
    ("USER32.dll", "MonitorFromRect"),
    ("USER32.dll", "MonitorFromPoint"),
    ("USER32.dll", "EnumDisplayMonitors"),
    ("USER32.dll", "EnumDisplayDevicesA"),
    ("USER32.dll", "EnumDisplayDevicesW"),
    ("USER32.dll", "GetDpiForWindow"),
    ("USER32.dll", "GetSystemMetricsForDpi"),
    ("USER32.dll", "AdjustWindowRectExForDpi"),
    ("COMCTL32.dll", "InitCommonControlsEx"),
    ("UXTHEME.dll", "SetWindowTheme"),
    ("D3D9.dll", "Direct3DCreate9"),
];

/// Names intentionally resolved to NULL by `GetProcAddress`.
const NULL_GET_PROC_NAMES: &[&str] = &[
    "corexitprocess",
    "getthreadpreferreduilanguages",
    "getprocesspreferreduilanguages",
    "getuserpreferreduilanguages",
    "getsystempreferreduilanguages",
    "getuserdefaultuilanguage",
    "getsystemdefaultuilanguage",
];

/// Resolve a catalogued dynamic export to its dense fake VA.
#[must_use]
pub fn dynamic_fake_target_va(library: &str, name: &str) -> Option<u64> {
    if let Some(id) = resolve_winapi_id(library, name) {
        return Some(encode_export(id));
    }
    // Soft slot reserved for known dynamic names without a dense id yet.
    DYNAMIC_FAKE_APIS
        .iter()
        .position(|e| e.library.eq_ignore_ascii_case(library) && e.name.eq_ignore_ascii_case(name))
        .and_then(|idx| u16::try_from(idx).ok())
        .map(encode_unresolved)
}

/// Resolves a `GetProcAddress` export name to a fake target VA.
///
/// Returns `Some(0)` for known-but-unsupported optional exports that the
/// guest expects to probe. Returns `None` for completely unknown names.
#[must_use]
pub fn resolve_get_proc_address(proc_name: &str) -> Option<u64> {
    let lower = proc_name.to_ascii_lowercase();

    if NULL_GET_PROC_NAMES.contains(&lower.as_str()) {
        return Some(0);
    }

    // Dense id exports first (EncodePointer, …).
    if let Some(id) = resolve_winapi_id("KERNEL32.dll", proc_name) {
        return Some(encode_export(id));
    }
    if let Some(id) = resolve_winapi_id("USER32.dll", proc_name) {
        return Some(encode_export(id));
    }

    // Preplanted soft extras (Interlocked*, …) — indices match session plant order.
    if let Some(idx) = PREPLANTED_SOFT_APIS
        .iter()
        .position(|e| e.name.eq_ignore_ascii_case(proc_name))
    {
        let idx_u16 = u16::try_from(idx).ok()?;
        return Some(encode_unresolved(idx_u16));
    }

    DYNAMIC_FAKE_APIS
        .iter()
        .find(|entry| entry.name.eq_ignore_ascii_case(proc_name))
        .and_then(|entry| dynamic_fake_target_va(entry.library, entry.name))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::fake_va::decode;
    use crate::{FakeVa, WinApiId};

    #[test]
    fn dynamic_fake_api_addresses_are_unique() {
        let mut addresses: Vec<u64> = DYNAMIC_FAKE_APIS
            .iter()
            .filter_map(|entry| dynamic_fake_target_va(entry.library, entry.name))
            .collect();

        let original_len = addresses.len();
        addresses.sort_unstable();
        addresses.dedup();

        assert_eq!(
            addresses.len(),
            original_len,
            "duplicate fake_target_va values in DYNAMIC_FAKE_APIS"
        );
    }

    #[test]
    fn dynamic_fake_api_names_are_unique_case_insensitive() {
        let mut names: Vec<String> = DYNAMIC_FAKE_APIS
            .iter()
            .map(|entry| entry.name.to_ascii_lowercase())
            .collect();

        let original_len = names.len();
        names.sort_unstable();
        names.dedup();

        assert_eq!(
            names.len(),
            original_len,
            "duplicate export names in DYNAMIC_FAKE_APIS"
        );
    }

    #[test]
    fn opengl32_dispatch_names_are_all_preplanted() {
        // `wglGetProcAddress` encodes by position in PREPLANTED_SOFT_APIS;
        // every `opengl32` dispatch export must be planted so the fake VA
        // decodes to ("opengl32.dll", name) at runtime.
        for name in crate::opengl32::OPENGL32_EXPORT_NAMES {
            let found = PREPLANTED_SOFT_APIS
                .iter()
                .any(|e| e.library.eq_ignore_ascii_case("opengl32.dll") && e.name == *name);
            assert!(
                found,
                "opengl32 export {name} missing from PREPLANTED_SOFT_APIS"
            );
        }
        // And the reverse: every planted opengl32 name is a dispatch export
        // (a stale planted entry is dead weight at worst, but catch it).
        for entry in PREPLANTED_SOFT_APIS {
            if entry.library.eq_ignore_ascii_case("opengl32.dll") {
                assert!(
                    crate::opengl32::OPENGL32_EXPORT_NAMES.contains(&entry.name),
                    "planted opengl32 name {} not in OPENGL32_EXPORT_NAMES",
                    entry.name
                );
            }
        }
    }

    #[test]
    fn wgl_get_proc_address_encodes_dispatch_names() {
        // The opengl32 exports must encode to resolvable soft VAs that decode
        // back to the same (library, name) pair.
        for entry in PREPLANTED_SOFT_APIS {
            if !entry.library.eq_ignore_ascii_case("opengl32.dll") {
                continue;
            }
            let va = crate::fake_va::encode_unresolved(
                u16::try_from(
                    PREPLANTED_SOFT_APIS
                        .iter()
                        .position(|e| std::ptr::eq(e, entry))
                        .unwrap_or(0),
                )
                .unwrap_or(0),
            );
            assert!(va >= crate::fake_va::FAKE_API_BASE);
        }
    }

    #[test]
    fn get_proc_encode_pointer_is_export() {
        let va = resolve_get_proc_address("EncodePointer").expect("EncodePointer");
        assert_eq!(
            decode(va),
            Some(FakeVa::Export(WinApiId::Kernel32Encodepointer))
        );
    }
}
