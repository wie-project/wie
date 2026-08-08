//! Name → id resolution table and lookups for the dense WinAPI dispatch.
//!
//! The `(library, name, id)` rows live per-DLL in the sibling modules; this
//! module assembles the lookup table from their slices and implements the
//! lookups. Row order mirrors the dense `WinApiId` enumeration.

mod advapi32;
mod comctl32;
mod comdlg32;
mod d3d9;
mod gdi32;
mod kernel32;
mod shell32;
mod user32;
mod uxtheme;
mod version;
mod winmm;

use crate::dispatch_table::WinApiId;

/// All `(library, name, id)` rows, as per-DLL slices.
///
/// Slices are listed in first-appearance order of the original dense table;
/// within a slice the rows keep the original order. The dense id↔name
/// correspondence (every `WinApiId` discriminant → its original row) is pinned
/// by the tests below, so the slice order here is not load-bearing.
static WINAPI_NAME_ROWS: &[&[(&str, &str, WinApiId)]] = &[
    kernel32::ROWS,
    advapi32::ROWS,
    user32::ROWS,
    comctl32::ROWS,
    comdlg32::ROWS,
    gdi32::ROWS,
    uxtheme::ROWS,
    winmm::ROWS,
    d3d9::ROWS,
    shell32::ROWS,
    version::ROWS,
];

/// Resolve library/export to id. Case-insensitive, allocation-free.
/// Intended for session setup (once per import), not the hot emu loop.
#[must_use]
pub fn resolve_winapi_id(library: &str, name: &str) -> Option<WinApiId> {
    for rows in WINAPI_NAME_ROWS {
        for &(lib, export, id) in *rows {
            if lib.eq_ignore_ascii_case(library) && export.eq_ignore_ascii_case(name) {
                return Some(id);
            }
        }
    }
    None
}

/// Reverse lookup: dense id → (`library`, `export`) as stored in the name table.
///
/// Names are lowercase (as in the per-DLL `ROWS` slices). Used for trace/profile only.
#[must_use]
pub fn winapi_id_export(id: WinApiId) -> Option<(&'static str, &'static str)> {
    for rows in WINAPI_NAME_ROWS {
        for &(lib, export, row_id) in *rows {
            if row_id == id {
                return Some((lib, export));
            }
        }
    }
    None
}

pub fn is_winapi_implemented(library: &str, name: &str) -> bool {
    if resolve_winapi_id(library, name).is_some() {
        return true;
    }
    // Soft UCRT / msvcrt path (string dispatch, not dense WinApiId).
    if crate::ucrt::is_ucrt_library(library) {
        if crate::ucrt::crt_data_import_va(name).is_some() {
            return true;
        }
        // Mirror dispatch_ucrt arms that are callable exports.
        let n = name.to_ascii_lowercase();
        return matches!(
            n.as_str(),
            "__acrt_iob_func"
                | "fwrite"
                | "fflush"
                | "setvbuf"
                | "_vsnwprintf"
                | "_vsnprintf"
                | "__stdio_common_vfprintf"
                | "malloc"
                | "calloc"
                | "free"
                | "_set_new_mode"
                | "__p__environ"
                | "__p__acmdln"
                | "__p___argc"
                | "__p___argv"
                | "__p___wargv"
                | "__p__wenviron"
                | "__p__commode"
                | "__p__fmode"
                | "_configthreadlocale"
                | "__setusermatherr"
                | "__c_specific_handler"
                | "memcpy"
                | "memmove"
                | "memcmp"
                | "memset"
                | "strlen"
                | "strncmp"
                | "_initterm"
                | "_initterm_e"
                | "_configure_narrow_argv"
                | "_initialize_narrow_environment"
                | "_configure_wide_argv"
                | "_initialize_wide_environment"
                | "_fpreset"
                | "_crt_atexit"
                | "_set_app_type"
                | "__set_app_type"
                | "_set_invalid_parameter_handler"
                | "__getmainargs"
                | "_xcptfilter"
                | "_cexit"
                | "_c_exit"
                | "signal"
                | "exit"
                | "_exit"
                | "abort"
                | "realloc"
                | "_isatty"
                | "_get_osfhandle"
                | "fputc"
                | "putchar"
                | "getchar"
                | "fputs"
                | "fgetc"
                | "strcmp"
                | "strncpy"
                | "wcscmp"
                | "wcsstr"
                | "wcslen"
                | "wcscat"
                | "wcscpy"
                | "wcsncmp"
                | "wcsncpy"
                | "_wcsnicmp"
                | "towupper"
                | "wcsrchr"
                | "_onexit"
                | "__dllonexit"
                | "_beginthreadex"
                | "_endthreadex"
                | "_purecall"
                | "perror"
                | "iswctype"
        );
    }
    if library.eq_ignore_ascii_case("ole32.dll") {
        let n = name.to_ascii_lowercase();
        return matches!(
            n.as_str(),
            "coinitialize" | "coinitializeex" | "couninitialize" | "cocreateinstance"
        );
    }
    if library.eq_ignore_ascii_case("shell32.dll") {
        let n = name.to_ascii_lowercase();
        return matches!(
            n.as_str(),
            "shgetfolderpathw"
                | "shgetpathfromidlistw"
                | "shbrowseforfolderw"
                | "shaddtorecentdocs"
        );
    }
    if library.eq_ignore_ascii_case("oleaut32.dll") {
        let n = name.to_ascii_lowercase();
        // Ordinals: 2 Alloc, 4 AllocLen, 6 Free, 7 StringLen, 8 Init,
        // 9 Clear, 10 Copy (Wine/Windows OLEAUT32).
        return matches!(
            n.as_str(),
            "sysallocstring"
                | "sysallocstringlen"
                | "sysfreestring"
                | "sysstringlen"
                | "sysstringbyteslen"
                | "variantinit"
                | "variantclear"
                | "variantcopy"
                | "ordinal 2"
                | "ordinal 4"
                | "ordinal 6"
                | "ordinal 7"
                | "ordinal 8"
                | "ordinal 9"
                | "ordinal 10"
                | "ordinal 11"
                | "ordinal 149"
        );
    }
    if library.eq_ignore_ascii_case("KERNEL32.dll") {
        let n = name.to_ascii_lowercase();
        return matches!(
            n.as_str(),
            "getversion"
                | "getmodulehandlew"
                | "lstrlenw"
                | "lstrcpyw"
                | "lstrcatw"
                | "virtualalloc"
                | "virtualfree"
                | "virtualprotect"
                | "virtualquery"
                | "flushinstructioncache"
                | "tlsgetvalue"
                | "tlssetvalue"
                | "tlsalloc"
                | "tlsfree"
                | "createthread"
                | "exitthread"
                | "getexitcodethread"
                | "waitforsingleobject"
                | "createeventa"
                | "createeventw"
                | "setevent"
                | "resetevent"
                | "getcurrentthread"
                | "setconsolectrlhandler"
                | "getconsolemode"
                | "setconsolemode"
                | "getconsolescreenbufferinfo"
                | "writeconsolew"
                | "writeconsolea"
                | "readconsolew"
                | "readconsolea"
                | "getconsolecp"
                | "getconsoleoutputcp"
                | "setconsolecp"
                | "setconsoleoutputcp"
                | "setconsoletitlew"
                | "setconsoletitlea"
                | "getconsoletitlew"
                | "getconsoletitlea"
                | "allocconsole"
                | "freeconsole"
                | "attachconsole"
                | "getconsolewindow"
                | "getlargestconsolewindowsize"
                | "getnumberofconsolemousebuttons"
                | "gettickcount64"
                | "getenvironmentvariablea"
                | "getenvironmentvariablew"
                | "setenvironmentvariablea"
                | "setenvironmentvariablew"
                | "expandenvironmentstringsa"
                | "expandenvironmentstringsw"
                | "setfileapistooem"
                | "queryperformancefrequency"
                | "getsysteminfo"
                | "isprocessorfeaturepresent"
                | "globalmemorystatusex"
                | "getprocesstimes"
                | "getlargepageminimum"
                | "getprocessaffinitymask"
                | "setprocessaffinitymask"
                | "setthreadaffinitymask"
                | "comparefiletime"
                | "localfiletimetofiletime"
                | "filetimetodosdatetime"
                | "dosdatetimetofiletime"
                | "getdiskfreespaceexw"
                | "getdiskfreespacew"
                | "getlogicaldrivestringsw"
                | "setfileattributesw"
                | "setfiletime"
                | "formatmessagew"
                | "resumethread"
                | "createsemaphorew"
                | "createsemaphorea"
                | "releasesemaphore"
                | "openeventw"
                | "openeventa"
                | "waitformultipleobjects"
                | "movefilewithprogressw"
                | "createhardlinkw"
                | "findfirststreamw"
                | "findnextstreamw"
                | "deviceiocontrol"
                | "mapviewoffile"
                | "unmapviewoffile"
                | "createfilemappingw"
                | "openfilemappingw"
                | "openfilemappinga"
        );
    }
    if library.eq_ignore_ascii_case("ws2_32.dll") {
        let n = name.to_ascii_lowercase();
        return matches!(
            n.as_str(),
            "wsastartup"
                | "wsacleanup"
                | "wsagetlasterror"
                | "wsasetlasterror"
                | "socket"
                | "closesocket"
                | "bind"
                | "listen"
                | "accept"
                | "connect"
                | "send"
                | "recv"
                | "select"
                | "getaddrinfo"
                | "freeaddrinfo"
                | "gethostbyname"
                | "inet_addr"
                | "inet_ntoa"
                | "htons"
                | "ntohs"
                | "getsockname"
                | "getpeername"
                | "setsockopt"
                | "getsockopt"
                | "shutdown"
                | "ioctlsocket"
        );
    }
    if library.eq_ignore_ascii_case("crypt32.dll") {
        let n = name.to_ascii_lowercase();
        return matches!(
            n.as_str(),
            "cryptacquirecontexta"
                | "cryptacquirecontextw"
                | "cryptreleasecontext"
                | "cryptgenrandom"
                | "cryptcreatehash"
                | "cryptdestroyhash"
                | "crypthashdata"
                | "cryptgethashparam"
        );
    }
    if library.eq_ignore_ascii_case("msimg32.dll") {
        let n = name.to_ascii_lowercase();
        return matches!(
            n.as_str(),
            "alphablend" | "transparentblt" | "gradientfill"
        );
    }
    if library.eq_ignore_ascii_case("imm32.dll") {
        let n = name.to_ascii_lowercase();
        return matches!(
            n.as_str(),
            "immgetcontext" | "immgetopenstatus" | "immreleasecontext" | "immgetcompositionstringw"
        );
    }
    if library.eq_ignore_ascii_case("setupapi.dll")
        || library.eq_ignore_ascii_case("cfgmgr32.dll")
    {
        let n = name.to_ascii_lowercase();
        return matches!(
            n.as_str(),
            "setupdigetclassdevsw"
                | "setupdigetclassdevsa"
                | "setupdienumdeviceinfo"
                | "setupdidestroydeviceinfolist"
                | "cm_get_device_id_listw"
                | "cm_get_device_id_lista"
        );
    }
    if library.eq_ignore_ascii_case("dbghelp.dll")
        || library.eq_ignore_ascii_case("imagehlp.dll")
    {
        let n = name.to_ascii_lowercase();
        return matches!(
            n.as_str(),
            "syminitializew"
                | "syminitialize"
                | "symcleanup"
                | "symfromaddrw"
                | "symfromaddr"
                | "mapfileandchecksumw"
                | "mapfileandchecksuma"
        );
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `WinApiId` discriminant resolves to exactly one `(library, name)`
    /// row that maps straight back to the same id (the two holes are skipped).
    /// Catches a row added to the wrong DLL, a missing row, or an id mismatch
    /// after an enum append.
    #[test]
    fn every_discriminant_round_trips_through_the_name_table() {
        for raw in 0..crate::dispatch_table::WINAPI_ID_COUNT {
            let raw = u16::try_from(raw).expect("count fits u16");
            let Some(id) = WinApiId::from_u16(raw) else {
                continue;
            };
            let (lib, name) = winapi_id_export(id).unwrap_or_else(|| panic!("no row for {id:?}"));
            assert_eq!(
                resolve_winapi_id(lib, name),
                Some(id),
                "{lib}!{name} must resolve back to {id:?}"
            );
        }
    }

    /// Pins the exact pre-split correspondence: each discriminant's row is the
    /// one it had before the split. Expected values derive from the variant
    /// identifier (lowercased suffix after the DLL prefix, `::` before D3D9
    /// interface methods) plus the historical exceptions, so any rename,
    /// retarget, or move of a row fails here.
    #[test]
    fn name_rows_pin_the_original_dense_order() {
        for raw in 0..crate::dispatch_table::WINAPI_ID_COUNT {
            let raw = u16::try_from(raw).expect("count fits u16");
            let Some(id) = WinApiId::from_u16(raw) else {
                continue;
            };
            let (lib, name) = winapi_id_export(id).expect("every discriminant has a row");
            let (exp_lib, exp_name) = expected_export(id);
            assert_eq!(
                (lib, name),
                (exp_lib.as_str(), exp_name.as_str()),
                "row for {id:?} drifted from the original table"
            );
        }
        // The bare `GetDlgItem` alias shares the ANSI id and must still resolve.
        assert_eq!(
            resolve_winapi_id("user32.dll", "getdlgitema"),
            Some(WinApiId::User32Getdlgitema)
        );
    }

    /// Expected `(library, name)` for a variant, derived from its identifier.
    fn expected_export(id: WinApiId) -> (String, String) {
        // Historical exceptions: the stored row deviates from the identifier.
        let exception = match id {
            WinApiId::Comctl32Ordinal17 => Some(("comctl32.dll", "ordinal 17")),
            WinApiId::Comctl32ImagelistCreate => Some(("comctl32.dll", "imagelist_create")),
            WinApiId::Comctl32ImagelistAddmasked => Some(("comctl32.dll", "imagelist_addmasked")),
            WinApiId::Comctl32ImagelistSetbkcolor => Some(("comctl32.dll", "imagelist_setbkcolor")),
            WinApiId::Comctl32ImagelistDestroy => Some(("comctl32.dll", "imagelist_destroy")),
            WinApiId::Kernel32Removefirectoryw => Some(("kernel32.dll", "removedirectoryw")),
            WinApiId::Kernel32Removefirectorya => Some(("kernel32.dll", "removedirectorya")),
            WinApiId::User32Validateirect => Some(("user32.dll", "validaterect")),
            WinApiId::Gdi32Drawtexta => Some(("user32.dll", "drawtexta")),
            WinApiId::Gdi32Drawtextw => Some(("user32.dll", "drawtextw")),
            WinApiId::User32Getdlgitema => Some(("user32.dll", "getdlgitem")),
            _ => None,
        };
        if let Some((lib, name)) = exception {
            return (lib.to_owned(), name.to_owned());
        }
        // General rule: strip the DLL prefix, lowercase the suffix, and insert
        // `::` at the camel-case boundary of a D3D9 interface method.
        const DLL_PREFIXES: &[&str] = &[
            "Kernel32", "User32", "Gdi32", "Advapi32", "Comctl32", "Comdlg32", "Shell32",
            "Uxtheme", "Winmm", "D3d9", "Version",
        ];
        let debug = format!("{id:?}");
        let (prefix, suffix) = DLL_PREFIXES
            .iter()
            .find_map(|p| debug.strip_prefix(p).map(|rest| (*p, rest)))
            .expect("variant starts with a known DLL prefix");
        let library = format!("{}.dll", prefix.to_ascii_lowercase());
        let bytes = suffix.as_bytes();
        let split = (0..bytes.len().saturating_sub(1)).rev().find(|&i| {
            let lower_or_digit = bytes[i].is_ascii_lowercase() || bytes[i].is_ascii_digit();
            lower_or_digit && bytes[i + 1].is_ascii_uppercase()
        });
        let name = match split {
            Some(i) => format!(
                "{}::{}",
                suffix[..=i].to_ascii_lowercase(),
                suffix[i + 1..].to_ascii_lowercase()
            ),
            None => suffix.to_ascii_lowercase(),
        };
        (library, name)
    }
}
