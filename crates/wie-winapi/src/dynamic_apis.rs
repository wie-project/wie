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

/// Soft slots planted **first** into the runtime soft table (stable indices 0..N).
///
/// Used for exports that live only in `dispatch_kernel32_extra` (no dense
/// `WinApiId`) so `GetProcAddress` can return a resolvable soft VA. Session
/// bootstrap must [`crate`]-side intern these before PE IAT imports.
///
/// Order is ABI for `GetProcAddress` soft encoding — append only.
pub const PREPLANTED_SOFT_APIS: &[DynamicFakeApi] = &[
    DynamicFakeApi {
        library: "KERNEL32.dll",
        name: "InterlockedIncrement",
    },
    DynamicFakeApi {
        library: "KERNEL32.dll",
        name: "InterlockedDecrement",
    },
    DynamicFakeApi {
        library: "KERNEL32.dll",
        name: "InterlockedExchange",
    },
    DynamicFakeApi {
        library: "KERNEL32.dll",
        name: "InterlockedCompareExchange",
    },
    DynamicFakeApi {
        library: "KERNEL32.dll",
        name: "InterlockedExchangeAdd",
    },
    DynamicFakeApi {
        library: "KERNEL32.dll",
        name: "InterlockedIncrement64",
    },
    DynamicFakeApi {
        library: "KERNEL32.dll",
        name: "InterlockedDecrement64",
    },
    DynamicFakeApi {
        library: "KERNEL32.dll",
        name: "InterlockedExchange64",
    },
    DynamicFakeApi {
        library: "KERNEL32.dll",
        name: "InterlockedCompareExchange64",
    },
    DynamicFakeApi {
        library: "KERNEL32.dll",
        name: "InterlockedExchangeAdd64",
    },
    // IDataObject vtable slots (OLE clipboard). Order IS the vtable ABI
    // (slot index = position, per the Windows IDataObject layout), appended
    // after the Interlocked* family — append only. The guest vtable builder
    // in `ole32.rs` re-discovers the index by position, so the names must
    // match `ole32::IDATAOBJECT_METHODS` exactly.
    DynamicFakeApi {
        library: "OLE32.dll",
        name: "IDataObject::QueryInterface",
    },
    DynamicFakeApi {
        library: "OLE32.dll",
        name: "IDataObject::AddRef",
    },
    DynamicFakeApi {
        library: "OLE32.dll",
        name: "IDataObject::Release",
    },
    DynamicFakeApi {
        library: "OLE32.dll",
        name: "IDataObject::GetData",
    },
    DynamicFakeApi {
        library: "OLE32.dll",
        name: "IDataObject::GetDataHere",
    },
    DynamicFakeApi {
        library: "OLE32.dll",
        name: "IDataObject::QueryGetData",
    },
    DynamicFakeApi {
        library: "OLE32.dll",
        name: "IDataObject::GetCanonicalFormatEtc",
    },
    DynamicFakeApi {
        library: "OLE32.dll",
        name: "IDataObject::SetData",
    },
    DynamicFakeApi {
        library: "OLE32.dll",
        name: "IDataObject::EnumFormatEtc",
    },
    DynamicFakeApi {
        library: "OLE32.dll",
        name: "IDataObject::DAdvise",
    },
    DynamicFakeApi {
        library: "OLE32.dll",
        name: "IDataObject::DUnadvise",
    },
    DynamicFakeApi {
        library: "OLE32.dll",
        name: "IDataObject::EnumDAdvise",
    },
    // opengl32 WGL + gl* exports — the same names as
    // `opengl32::OPENGL32_EXPORT_NAMES` (the dispatch census). `wglGetProcAddress`
    // encodes these by position (append only — the order is the ABI), and the
    // kernel32 `GetProcAddress` path resolves them too. A unit test pins the
    // lists together so they cannot drift.
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "wglcreatecontext",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "wgldeletecontext",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "wglmakecurrent",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "wglgetprocaddress",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "wglchoosepixelformat",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "wglsetpixelformat",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "wgldescribepixelformat",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "wglgetpixelformat",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "wglswapbuffers",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "wglsharelists",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "wglgetcurrentcontext",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "wglgetcurrentdc",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glclearcolor",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glcleardepth",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glclear",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glbegin",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glend",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glvertex2f",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glvertex3f",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glvertex4f",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glvertex2fv",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glvertex3fv",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glvertex4fv",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glcolor3f",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glcolor4f",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glcolor3fv",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glcolor4fv",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glcolor3ub",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glcolor4ub",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glcolor3ubv",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glcolor4ubv",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gltexcoord1f",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gltexcoord2f",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gltexcoord3f",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gltexcoord4f",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gltexcoord1fv",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gltexcoord2fv",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gltexcoord3fv",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gltexcoord4fv",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glmatrixmode",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glloadidentity",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glloadmatrixf",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glmultmatrixf",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glortho",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glfrustum",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gltranslatef",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glrotatef",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glscalef",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glpushmatrix",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glpopmatrix",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glviewport",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gldepthfunc",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gldepthmask",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glenable",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gldisable",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glblendfunc",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glpolygonmode",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glgentextures",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gldeletetextures",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glbindtexture",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glteximage2d",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gltexparameteri",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gltexenvi",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gltexenvf",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glpixelstorei",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glreadpixels",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glflush",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glfinish",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glgeterror",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glgetstring",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glgetintegerv",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glgetfloatv",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glgetbooleanv",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glscissor",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glenableclientstate",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gldisableclientstate",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glvertexpointer",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glcolorpointer",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gltexcoordpointer",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glnormalpointer",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gldrawarrays",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gldrawelements",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gldeletebuffers",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glisbuffer",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glbindbuffer",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glbufferdata",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glbuffersubdata",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glreadbuffer",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gllightfv",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gllightmodelfv",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glmaterialfv",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glmaterialf",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glcolormaterial",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glshademodel",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glnormal3fv",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glcullface",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glfrontface",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glpointsizef",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gllinewidth",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glnewlist",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glendlist",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glcalllist",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glcalllists",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glgenlists",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glislist",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gldeletelists",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gllistbase",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glisshader",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glshadersource",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glcompileshader",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glcreateprogram",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glcreateshader",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glgenbuffers",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glnormal3f",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gldeleteshader",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glgetshaderiv",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glgetshaderinfolog",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glattachshader",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gldetachshader",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gllinkprogram",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gluseprogram",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glgetprogramiv",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glgetprograminfolog",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gldeleteprogram",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glisprogram",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glvalidateprogram",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glgetuniformlocation",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gluniform1f",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gluniform2f",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gluniform3f",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gluniform4f",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gluniform1i",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "gluniformmatrix4fv",
    },
    DynamicFakeApi {
        library: "opengl32.dll",
        name: "glactivetexture",
    },
];

/// Dynamic exports resolvable via `GetProcAddress` (generic PE64 + legacy apps).
///
/// Order is stable; lookup is by `name` (case-insensitive).
/// Clean room: names are our fake dispatch map, not copied from other projects.
pub const DYNAMIC_FAKE_APIS: &[DynamicFakeApi] = &[
    DynamicFakeApi {
        library: "KERNEL32.dll",
        name: "EncodePointer",
    },
    DynamicFakeApi {
        library: "KERNEL32.dll",
        name: "DecodePointer",
    },
    DynamicFakeApi {
        library: "KERNEL32.dll",
        name: "InitializeCriticalSectionAndSpinCount",
    },
    DynamicFakeApi {
        library: "USER32.dll",
        name: "SetProcessDPIAware",
    },
    DynamicFakeApi {
        library: "USER32.dll",
        name: "TrackMouseEvent",
    },
    DynamicFakeApi {
        library: "COMCTL32.dll",
        name: "DllGetVersion",
    },
    DynamicFakeApi {
        library: "USER32.dll",
        name: "GetSystemMetrics",
    },
    DynamicFakeApi {
        library: "USER32.dll",
        name: "MonitorFromWindow",
    },
    DynamicFakeApi {
        library: "USER32.dll",
        name: "GetMonitorInfoA",
    },
    DynamicFakeApi {
        library: "USER32.dll",
        name: "GetMonitorInfoW",
    },
    DynamicFakeApi {
        library: "USER32.dll",
        name: "MonitorFromRect",
    },
    DynamicFakeApi {
        library: "USER32.dll",
        name: "MonitorFromPoint",
    },
    DynamicFakeApi {
        library: "USER32.dll",
        name: "EnumDisplayMonitors",
    },
    DynamicFakeApi {
        library: "USER32.dll",
        name: "EnumDisplayDevicesA",
    },
    DynamicFakeApi {
        library: "USER32.dll",
        name: "EnumDisplayDevicesW",
    },
    DynamicFakeApi {
        library: "USER32.dll",
        name: "GetDpiForWindow",
    },
    DynamicFakeApi {
        library: "USER32.dll",
        name: "GetSystemMetricsForDpi",
    },
    DynamicFakeApi {
        library: "USER32.dll",
        name: "AdjustWindowRectExForDpi",
    },
    DynamicFakeApi {
        library: "COMCTL32.dll",
        name: "InitCommonControlsEx",
    },
    DynamicFakeApi {
        library: "UXTHEME.dll",
        name: "SetWindowTheme",
    },
    DynamicFakeApi {
        library: "D3D9.dll",
        name: "Direct3DCreate9",
    },
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
