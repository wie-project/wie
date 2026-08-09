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

/// Soft-dispatched UCRT/msvcrt callable exports (mirror of `dispatch_ucrt` arms).
const UCRT_CALLABLE: &[&str] = &[
    "??1type_info@@ueaa@xz",
    "?terminate@@yaxxz",
    "__acrt_iob_func",
    "__c_specific_handler",
    "__dllonexit",
    "__getmainargs",
    "__p___argc",
    "__p___argv",
    "__p___wargv",
    "__p__acmdln",
    "__p__commode",
    "__p__environ",
    "__p__fmode",
    "__p__wenviron",
    "__set_app_type",
    "__setusermatherr",
    "__stdio_common_vfprintf",
    "__stdio_common_vfscanf",
    "__stdio_common_vsprintf",
    "__stdio_common_vsscanf",
    "_beginthreadex",
    "_c_exit",
    "_cexit",
    "_configthreadlocale",
    "_configure_narrow_argv",
    "_configure_wide_argv",
    "_crt_atexit",
    "_cxxthrowexception",
    "_endthreadex",
    "_errno",
    "_exit",
    "_fpreset",
    "_get_osfhandle",
    "_getch",
    "_initialize_narrow_environment",
    "_initialize_wide_environment",
    "_initterm",
    "_initterm_e",
    "_isatty",
    "_kbhit",
    "_localtime64",
    "_onexit",
    "_purecall",
    "_set_app_type",
    "_set_invalid_parameter_handler",
    "_set_new_mode",
    "_time64",
    "_vsnprintf",
    "_vsnwprintf",
    "_wcsnicmp",
    "_xcptfilter",
    "abort",
    "atoi",
    "atol",
    "calloc",
    "exit",
    "fclose",
    "fflush",
    "fgetc",
    "fgets",
    "fopen",
    "fputc",
    "fputs",
    "free",
    "fwrite",
    "getchar",
    "getenv",
    "isalnum",
    "isalpha",
    "isdigit",
    "islower",
    "isspace",
    "isupper",
    "iswctype",
    "malloc",
    "memcmp",
    "memcpy",
    "memmove",
    "memset",
    "perror",
    "putchar",
    "puts",
    "rand",
    "realloc",
    "setlocale",
    "setvbuf",
    "signal",
    "srand",
    "strchr",
    "strcmp",
    "strerror",
    "strlen",
    "strncmp",
    "strncpy",
    "strstr",
    "strtod",
    "strtof",
    "strtok",
    "strtol",
    "strtoul",
    "system",
    "tolower",
    "toupper",
    "towupper",
    "wcscat",
    "wcscmp",
    "wcscpy",
    "wcslen",
    "wcsncmp",
    "wcsncpy",
    "wcsrchr",
    "wcsstr",
];

/// Soft-dispatched `ole32.dll` exports (mirror of the `dispatch_*` fallback arms).
const OLE32_DLL_EXPORTS: &[&str] = &[
    "clsidfromstring",
    "cocreateguid",
    "cocreateinstance",
    "cogetclassobject",
    "coinitialize",
    "coinitializeex",
    "coregisterclassobject",
    "corevokeclassobject",
    "cotaskmemalloc",
    "cotaskmemfree",
    "cotaskmemrealloc",
    "couninitialize",
    "stringfromclsid",
];

/// Soft-dispatched `shell32.dll` exports (mirror of the `dispatch_*` fallback arms).
const SHELL32_DLL_EXPORTS: &[&str] = &[
    "commandlinetoargvw",
    "shaddtorecentdocs",
    "shbrowseforfolderw",
    "shellexecuteexw",
    "shgetfileinfoa",
    "shgetfileinfow",
    "shgetfolderpathw",
    "shgetpathfromidlistw",
    "shgetspecialfolderpathw",
];

/// Soft-dispatched `oleaut32.dll` exports (mirror of the `dispatch_*` fallback arms).
const OLEAUT32_DLL_EXPORTS: &[&str] = &[
    "dispgetidsofnames",
    "dispinvoke",
    "ordinal 10",
    "ordinal 11",
    "ordinal 149",
    "ordinal 2",
    "ordinal 4",
    "ordinal 6",
    "ordinal 7",
    "ordinal 8",
    "ordinal 9",
    "safearrayaccessdata",
    "safearraycreate",
    "safearraydestroy",
    "safearraygetdim",
    "safearraygetelement",
    "safearraygetlbound",
    "safearraygetubound",
    "safearrayputelement",
    "safearrayunaccessdata",
    "sysallocstring",
    "sysallocstringlen",
    "sysfreestring",
    "sysstringbyteslen",
    "sysstringlen",
    "variantclear",
    "variantcopy",
    "variantinit",
];

/// Soft-dispatched `user32.dll` exports (mirror of the `dispatch_*` fallback arms).
const USER32_DLL_EXPORTS: &[&str] = &[
    "createcaret",
    "destroycaret",
    "drawicona",
    "drawiconex",
    "drawiconw",
    "enumchildwindows",
    "enumwindows",
    "findwindowa",
    "findwindoww",
    "getcaretpos",
    "hidecaret",
    "setcaretpos",
    "showcaret",
];

/// Soft-dispatched `gdi32.dll` exports (mirror of the `dispatch_*` fallback arms).
const GDI32_DLL_EXPORTS: &[&str] = &[
    "combinergn",
    "createellipticrgn",
    "createpolygonrgn",
    "createrectrgn",
    "enumfontfamiliesexa",
    "enumfontfamiliesexw",
    "getdibits",
    "getrgnbox",
    "setdibits",
    "setpixel",
    "setrectrgn",
];

/// Soft-dispatched `comctl32.dll` exports (mirror of the `dispatch_*` fallback arms).
const COMCTL32_DLL_EXPORTS: &[&str] = &[
    "createtoolbarex",
    "imagelist_add",
    "imagelist_draw",
    "imagelist_geticonsize",
    "imagelist_getimagecount",
    "imagelist_getimageinfo",
    "imagelist_seticonsize",
];

/// Soft-dispatched `winmm.dll` exports (mirror of the `dispatch_*` fallback arms).
const WINMM_DLL_EXPORTS: &[&str] = &[
    "timekillevent",
    "timesetevent",
    "waveoutclose",
    "waveoutgetnumdevs",
    "waveoutopen",
    "waveoutprepareheader",
    "waveoutunprepareheader",
    "waveoutwrite",
];

/// Soft-dispatched `advapi32.dll` exports (mirror of the `dispatch_*` fallback arms).
const ADVAPI32_DLL_EXPORTS: &[&str] = &[
    "adjusttokenprivileges",
    "getfilesecuritya",
    "getfilesecurityw",
    "lookupprivilegevaluea",
    "lookupprivilegevaluew",
    "openprocesstoken",
    "regcreatekeyexw",
    "regdeletekeya",
    "regdeletekeyw",
    "regdeletevaluew",
    "regenumkeyexa",
    "regenumkeyexw",
    "regenumvaluea",
    "regenumvaluew",
    "regflushkey",
    "regopenkeyexw",
    "regsavekeya",
    "regsavekeyw",
    "setfilesecuritya",
    "setfilesecurityw",
    "systemfunction036",
];

/// Soft-dispatched `kernel32.dll` exports (mirror of the `dispatch_*` fallback arms).
const KERNEL32_DLL_EXPORTS: &[&str] = &[
    "allocconsole",
    "assignprocesstojobobject",
    "attachconsole",
    "backupread",
    "backupseek",
    "backupwrite",
    "comparefiletime",
    "createconsolescreenbuffer",
    "createeventa",
    "createeventw",
    "createfilemappingw",
    "createhardlinkw",
    "createjobobjecta",
    "createjobobjectw",
    "createsemaphorea",
    "createsemaphorew",
    "createthread",
    "debugbreak",
    "deviceiocontrol",
    "dosdatetimetofiletime",
    "duplicatehandle",
    "exitthread",
    "expandenvironmentstringsa",
    "expandenvironmentstringsw",
    "filetimetodosdatetime",
    "fillconsoleoutputattribute",
    "fillconsoleoutputcharactera",
    "fillconsoleoutputcharacterw",
    "findfirststreamw",
    "findnextstreamw",
    "flushconsoleinputbuffer",
    "flushinstructioncache",
    "formatmessagew",
    "freeconsole",
    "getcompressedfilesizea",
    "getcompressedfilesizew",
    "getcomputernamea",
    "getcomputernameexw",
    "getcomputernamew",
    "getconsolecp",
    "getconsolecursorinfo",
    "getconsolemode",
    "getconsoleoutputcp",
    "getconsolescreenbufferinfo",
    "getconsoletitlea",
    "getconsoletitlew",
    "getconsolewindow",
    "getcurrentthread",
    "getdiskfreespaceexw",
    "getdiskfreespacew",
    "getenvironmentvariablea",
    "getenvironmentvariablew",
    "getexitcodethread",
    "getfileattributesexa",
    "getfileattributesexw",
    "getlargepageminimum",
    "getlargestconsolewindowsize",
    "getlogicaldrivestringsw",
    "getlongpathnamea",
    "getlongpathnamew",
    "getmodulehandlew",
    "getnumberofconsoleinputevents",
    "getnumberofconsolemousebuttons",
    "getprocessaffinitymask",
    "getprocesstimes",
    "getshortpathnamea",
    "getshortpathnamew",
    "getsysteminfo",
    "getthreadpriority",
    "gettickcount64",
    "getusernamea",
    "getusernamew",
    "getuserprofiledirectorya",
    "getuserprofiledirectoryw",
    "getversion",
    "getvolumeinformationa",
    "getvolumeinformationw",
    "globalmemorystatusex",
    "interlockedcompareexchange",
    "interlockedcompareexchange64",
    "interlockeddecrement",
    "interlockeddecrement64",
    "interlockedexchange",
    "interlockedexchange64",
    "interlockedexchangeadd",
    "interlockedexchangeadd64",
    "interlockedincrement",
    "interlockedincrement64",
    "isdebuggerpresent",
    "isprocessorfeaturepresent",
    "localfiletimetofiletime",
    "lockfile",
    "lstrcatw",
    "lstrcpyw",
    "lstrlenw",
    "mapviewoffile",
    "movefilewithprogressw",
    "openeventa",
    "openeventw",
    "openfilemappinga",
    "openfilemappingw",
    "openthread",
    "outputdebugstringa",
    "outputdebugstringw",
    "peekconsoleinputw",
    "queryfullprocessimagenamea",
    "queryfullprocessimagenamew",
    "queryperformancefrequency",
    "raiseexception",
    "readconsolea",
    "readconsoleinputw",
    "readconsolew",
    "releasesemaphore",
    "resetevent",
    "resumethread",
    "rtlcapturecontext",
    "rtlunwindex",
    "scrollconsolescreenbufferw",
    "setconsoleactivescreenbuffer",
    "setconsolecp",
    "setconsolectrlhandler",
    "setconsolecursorinfo",
    "setconsolecursorposition",
    "setconsolemode",
    "setconsoleoutputcp",
    "setconsolescreenbuffersize",
    "setconsoletitlea",
    "setconsoletitlew",
    "setconsolewindowinfo",
    "setenvironmentvariablea",
    "setenvironmentvariablew",
    "seterrormode",
    "setevent",
    "setfileapistooem",
    "setfileattributesw",
    "setfiletime",
    "setfilevaliddata",
    "setprocessaffinitymask",
    "setthreadaffinitymask",
    "setthreaderrormode",
    "signalobjectandwait",
    "suspendthread",
    "terminateprocess",
    "terminatethread",
    "tlsalloc",
    "tlsfree",
    "tlsgetvalue",
    "tlssetvalue",
    "unlockfile",
    "unmapviewoffile",
    "virtualalloc",
    "virtualfree",
    "virtualprotect",
    "virtualquery",
    "waitformultipleobjects",
    "waitforsingleobject",
    "writeconsolea",
    "writeconsoleoutputattribute",
    "writeconsoleoutputcharactera",
    "writeconsoleoutputcharacterw",
    "writeconsolew",
];

/// Soft-dispatched `ws2_32.dll` exports (mirror of the `dispatch_*` fallback arms).
const WS2_32_DLL_EXPORTS: &[&str] = &[
    "accept",
    "bind",
    "closesocket",
    "connect",
    "freeaddrinfo",
    "getaddrinfo",
    "gethostbyname",
    "getpeername",
    "getsockname",
    "getsockopt",
    "htons",
    "inet_addr",
    "inet_ntoa",
    "ioctlsocket",
    "listen",
    "ntohs",
    "recv",
    "select",
    "send",
    "setsockopt",
    "shutdown",
    "socket",
    "wsacleanup",
    "wsagetlasterror",
    "wsasetlasterror",
    "wsastartup",
];

/// Soft-dispatched `crypt32.dll` exports (mirror of the `dispatch_*` fallback arms).
const CRYPT32_DLL_EXPORTS: &[&str] = &[
    "cryptacquirecontexta",
    "cryptacquirecontextw",
    "cryptcreatehash",
    "cryptdestroyhash",
    "cryptgenrandom",
    "cryptgethashparam",
    "crypthashdata",
    "cryptreleasecontext",
];

/// Soft-dispatched `msimg32.dll` exports (mirror of the `dispatch_*` fallback arms).
const MSIMG32_DLL_EXPORTS: &[&str] = &["alphablend", "gradientfill", "transparentblt"];

/// Soft-dispatched `imm32.dll` exports (mirror of the `dispatch_*` fallback arms).
const IMM32_DLL_EXPORTS: &[&str] = &[
    "immgetcompositionstringw",
    "immgetcontext",
    "immgetopenstatus",
    "immreleasecontext",
];

/// Soft-dispatched `uxtheme.dll` exports (mirror of the `dispatch_*` fallback arms).
const UXTHEME_DLL_EXPORTS: &[&str] = &[
    "closethemedata",
    "getwindowtheme",
    "isthemeactive",
    "openthemedata",
];

/// Soft-dispatched `setupapi.dll` exports (mirror of the `dispatch_*` fallback arms).
const SETUPAPI_CFGMGR32: &[&str] = &[
    "cm_get_device_id_lista",
    "cm_get_device_id_listw",
    "setupdidestroydeviceinfolist",
    "setupdienumdeviceinfo",
    "setupdigetclassdevsa",
    "setupdigetclassdevsw",
];

/// Soft-dispatched `dbghelp.dll` exports (mirror of the `dispatch_*` fallback arms).
const DBGHELP_IMAGEHLP: &[&str] = &[
    "mapfileandchecksuma",
    "mapfileandchecksumw",
    "symcleanup",
    "symfromaddr",
    "symfromaddrw",
    "syminitialize",
    "syminitializew",
];
pub fn is_winapi_implemented(library: &str, name: &str) -> bool {
    if resolve_winapi_id(library, name).is_some() {
        return true;
    }
    // Soft UCRT / msvcrt path (string dispatch, not dense WinApiId).
    if crate::ucrt::is_ucrt_library(library) {
        if crate::ucrt::crt_data_import_va(name).is_some() {
            return true;
        }
        let n = name.to_ascii_lowercase();
        return UCRT_CALLABLE.contains(&n.as_str());
    }
    let n = name.to_ascii_lowercase();
    let n = n.as_str();
    match library.to_ascii_lowercase().as_str() {
        "ole32.dll" => OLE32_DLL_EXPORTS.contains(&n),
        "shell32.dll" => SHELL32_DLL_EXPORTS.contains(&n),
        "oleaut32.dll" => OLEAUT32_DLL_EXPORTS.contains(&n),
        "user32.dll" => USER32_DLL_EXPORTS.contains(&n),
        "gdi32.dll" => GDI32_DLL_EXPORTS.contains(&n),
        "comctl32.dll" => COMCTL32_DLL_EXPORTS.contains(&n),
        "winmm.dll" => WINMM_DLL_EXPORTS.contains(&n),
        "advapi32.dll" => ADVAPI32_DLL_EXPORTS.contains(&n),
        "kernel32.dll" => KERNEL32_DLL_EXPORTS.contains(&n),
        "ws2_32.dll" => WS2_32_DLL_EXPORTS.contains(&n),
        "crypt32.dll" => CRYPT32_DLL_EXPORTS.contains(&n),
        "msimg32.dll" => MSIMG32_DLL_EXPORTS.contains(&n),
        "imm32.dll" => IMM32_DLL_EXPORTS.contains(&n),
        "uxtheme.dll" => UXTHEME_DLL_EXPORTS.contains(&n),
        "setupapi.dll" | "cfgmgr32.dll" => SETUPAPI_CFGMGR32.contains(&n),
        "dbghelp.dll" | "imagehlp.dll" => DBGHELP_IMAGEHLP.contains(&n),
        _ => false,
    }
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
