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
    "__iob_func",
    "_amsg_exit",
    "_fdopen",
    "_filelengthi64",
    "_fileno",
    "_lock",
    "_unlock",
    "_wfopen",
    "_wfreopen",
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
    "__stdio_common_vsprintf_s",
    "__stdio_common_vsscanf",
    "__stdio_common_vswprintf_s",
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
    "_snprintf_s",
    "_strtok_s",
    "_time64",
    "_vsnprintf",
    "_vsnprintf_s",
    "_vsnwprintf",
    "_vsnwprintf_s",
    "_wcsnicmp",
    "_xcptfilter",
    "abort",
    "atoi",
    "atol",
    "calloc",
    "exit",
    "fclose",
    "feof",
    "ferror",
    "fgetpos",
    "fprintf",
    "fread",
    "fseek",
    "fsetpos",
    "ftell",
    "fflush",
    "fgetc",
    "fgets",
    "fopen",
    "fopen_s",
    "fputc",
    "fputs",
    "free",
    "freopen_s",
    "fwrite",
    "getchar",
    "getenv",
    "isalnum",
    "isalpha",
    "isprint",
    "isdigit",
    "islower",
    "isspace",
    "isupper",
    "iswctype",
    "malloc",
    "memchr",
    "memcmp",
    "memcpy",
    "memcpy_s",
    "memmove",
    "memmove_s",
    "memset",
    "memset_s",
    "perror",
    "putchar",
    "puts",
    "qsort_s",
    "qsort",
    "log10",
    "rand",
    "realloc",
    "setlocale",
    "setvbuf",
    "signal",
    "snprintf_s",
    "sprintf_s",
    "srand",
    "strcat_s",
    "strchr",
    "strcpy",
    "strrchr",
    "strcmp",
    "strcpy_s",
    "strerror",
    "strlen",
    "strncat_s",
    "strncmp",
    "strncpy",
    "strncpy_s",
    "strstr",
    "strtod",
    "strtof",
    "strtok",
    "strtok_s",
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
    "dodragdrop",
    "oleflushclipboard",
    "olegetclipboard",
    "oleinitialize",
    "oleuninitialize",
    "olesetclipboard",
    "propvariantclear",
    "registerdragdrop",
    "revokedragdrop",
    "stringfromclsid",
];

/// Soft-dispatched `shell32.dll` exports (mirror of the `dispatch_*` fallback arms).
const SHELL32_DLL_EXPORTS: &[&str] = &[
    "commandlinetoargvw",
    "shaddtorecentdocs",
    "shbrowseforfolderw",
    "shellexecutea",
    "shellexecuteexa",
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
    "attachthreadinput",
    "bringwindowtotop",
    "changedisplaysettingsexw",
    "charprevexa",
    "charupperw",
    "copyimage",
    "createcaret",
    "createiconfromresource",
    "createiconindirect",
    "destroycaret",
    "dialogboxindirectparamw",
    "dialogboxparamw",
    "drawicona",
    "drawiconex",
    "drawiconw",
    "enumchildwindows",
    "enumdisplaysettingsw",
    "enumwindows",
    "findwindowa",
    "findwindoww",
    "flashwindowex",
    "getcaretpos",
    "getclassinfoexw",
    "getclipboardsequencenumber",
    "getdoubleclicktime",
    "getkeyboardlayout",
    "getmessageextrainfo",
    "getmessagetime",
    "getpropw",
    "getrawinputdata",
    "getrawinputdeviceinfoa",
    "getrawinputdevicelist",
    "getupdaterect",
    "getwindowlongw",
    "hidecaret",
    "intersectrect",
    "keybd_event",
    "mapvirtualkeyw",
    "monitorfromrect",
    "msgwaitformultipleobjects",
    "postthreadmessagew",
    "ptinrect",
    "registerdevicenotificationw",
    "registerhotkey",
    "registerrawinputdevices",
    "removepropw",
    "setcaretpos",
    "setcursorpos",
    "setlayeredwindowattributes",
    "setprocessdefaultlayout",
    "setpropw",
    "setwindowrgn",
    "showcaret",
    "systemparametersinfoa",
    "systemparametersinfow",
    "tounicode",
    "unregisterdevicenotification",
    "unregisterhotkey",
    "waitforinputidle",
    "winhelpw",
    "wsprintfw",
];

/// Soft-dispatched `gdi32.dll` exports (mirror of the `dispatch_*` fallback arms).
const GDI32_DLL_EXPORTS: &[&str] = &[
    "choosepixelformat",
    "combinergn",
    "createbitmap",
    "createellipticrgn",
    "createpolygonrgn",
    "createrectrgn",
    "describepixelformat",
    "enumfontfamiliesexa",
    "enumfontfamiliesexw",
    "getdevicegammaramp",
    "getdibits",
    "geticmprofilew",
    "getpixelformat",
    "getrgnbox",
    "setdevicegammaramp",
    "setdibits",
    "setpixel",
    "setpixelformat",
    "setrectrgn",
    "swapbuffers",
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
    "midioutgetdevcapsa",
    "midioutgeterrortexta",
    "midioutprepareheader",
    "midioutreset",
    "midioutsetvolume",
    "midioutshortmsg",
    "midioutunprepareheader",
    "midistreamclose",
    "midistreamopen",
    "midistreamout",
    "midistreampause",
    "midistreamproperty",
    "midistreamrestart",
    "midistreamstop",
    "timebeginperiod",
    "timeendperiod",
    "timekillevent",
    "timesetevent",
    "waveinaddbuffer",
    "waveinclose",
    "waveingetdevcapsw",
    "waveingetnumdevs",
    "waveinopen",
    "waveinprepareheader",
    "waveinreset",
    "waveinstart",
    "waveinunprepareheader",
    "waveoutclose",
    "waveoutgetdevcapsw",
    "waveoutgeterrortextw",
    "waveoutgetnumdevs",
    "waveoutopen",
    "waveoutprepareheader",
    "waveoutreset",
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
    "cancelio",
    "comparefiletime",
    "comparestringa",
    "comparestringw",
    "createconsolescreenbuffer",
    "createeventa",
    "createeventw",
    "createfilemappingw",
    "createhardlinkw",
    "createjobobjecta",
    "createjobobjectw",
    "createmutexa",
    "createsemaphorea",
    "createsemaphorew",
    "createthread",
    "debugbreak",
    "deviceiocontrol",
    "dosdatetimetofiletime",
    "duplicatehandle",
    "enumresourcenamesw",
    "exitthread",
    "expandenvironmentstringsa",
    "expandenvironmentstringsw",
    "filetimetodosdatetime",
    "fillconsoleoutputattribute",
    "fillconsoleoutputcharactera",
    "fillconsoleoutputcharacterw",
    "findfirstfileexw",
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
    "getlocaleinfoa",
    "getlogicaldrivestringsw",
    "getlongpathnamea",
    "getlongpathnamew",
    "getmodulehandleexw",
    "getmodulehandlew",
    "getnumberofconsoleinputevents",
    "getnumberofconsolemousebuttons",
    "getoverlappedresult",
    "getprocessaffinitymask",
    "getprocessid",
    "getprocesstimes",
    "getshortpathnamea",
    "getshortpathnamew",
    "getsysteminfo",
    "getsystempowerstatus",
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
    "initializecriticalsectionex",
    "initializeslisthead",
    "interlockedcompareexchange",
    "interlockedcompareexchange64",
    "interlockeddecrement",
    "interlockeddecrement64",
    "interlockedexchange",
    "interlockedexchange64",
    "interlockedexchangeadd",
    "interlockedexchangeadd64",
    "interlockedflushslist",
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
    "movefileexw",
    "movefilewithprogressw",
    "openeventa",
    "openeventw",
    "openfilemappinga",
    "openfilemappingw",
    "openthread",
    "outputdebugstringa",
    "outputdebugstringw",
    "peekconsoleinputw",
    "peeknamedpipe",
    "queryfullprocessimagenamea",
    "queryfullprocessimagenamew",
    "queryperformancefrequency",
    "raiseexception",
    "readconsolea",
    "readconsoleinputw",
    "readconsolew",
    "releasemutex",
    "releasesemaphore",
    "resetevent",
    "resumethread",
    "rtlcapturecontext",
    "rtllookupfunctionentry",
    "rtlpctofileheader",
    "rtlunwind",
    "rtlunwindex",
    "rtlvirtualunwind",
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
    "setnamedpipehandlestate",
    "setprocessaffinitymask",
    "setstdhandle",
    "setthreadaffinitymask",
    "setthreaderrormode",
    "setthreadexecutionstate",
    "setthreadpriority",
    "signalobjectandwait",
    "suspendthread",
    "systemtimetotzspecificlocaltime",
    "terminateprocess",
    "terminatethread",
    "tlsalloc",
    "tlsfree",
    "tlsgetvalue",
    "tlssetvalue",
    "tryentercriticalsection",
    "unhandledexceptionfilter",
    "unlockfile",
    "unmapviewoffile",
    "versetconditionmask",
    "verifyversioninfow",
    "virtualalloc",
    "virtualfree",
    "virtualprotect",
    "virtualquery",
    "waitformultipleobjects",
    "waitforsingleobject",
    "waitforsingleobjectex",
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
    "immassociatecontext",
    "immgetcandidatelistw",
    "immgetcompositionstringw",
    "immgetcontext",
    "immgetimefilenamea",
    "immgetopenstatus",
    "immnotifyime",
    "immreleasecontext",
    "immsetcandidatewindow",
    "immsetcompositionstringw",
    "immsetcompositionwindow",
];

/// Soft-dispatched `uxtheme.dll` exports (mirror of the `dispatch_*` fallback arms).
const UXTHEME_DLL_EXPORTS: &[&str] = &[
    "closethemedata",
    "getwindowtheme",
    "isthemeactive",
    "openthemedata",
];

/// Soft-dispatched `winhttp.dll` exports (mirror of the `dispatch_*` fallback arms).
const WINHTTP_DLL_EXPORTS: &[&str] = &[
    "winhttpaddrequestheaders",
    "winhttpclosehandle",
    "winhttpconnect",
    "winhttpopen",
    "winhttpopenrequest",
    "winhttpquerydataavailable",
    "winhttpreaddata",
    "winhttpreceiveresponse",
    "winhttpsendrequest",
];

/// Soft-dispatched `setupapi.dll` exports (mirror of the `dispatch_*` fallback arms).
const SETUPAPI_CFGMGR32: &[&str] = &[
    "cm_get_device_ida",
    "cm_get_device_id_lista",
    "cm_get_device_id_listw",
    "cm_get_parent",
    "cm_locate_devnodea",
    "setupdidestroydeviceinfolist",
    "setupdienumdeviceinfo",
    "setupdienumdeviceinterfaces",
    "setupdigetclassdevsa",
    "setupdigetclassdevsw",
    "setupdigetdeviceinterfacedetaila",
    "setupdigetdeviceregistrypropertya",
];

/// Soft-dispatched `dbghelp.dll` exports (mirror of the `dispatch_*` fallback arms).
const DBGHELP_IMAGEHLP: &[&str] = &[
    "mapfileandchecksuma",
    "mapfileandchecksumw",
    "minidumpwritedump",
    "stackwalk64",
    "symcleanup",
    "symfromaddr",
    "symfromaddrw",
    "symfunctiontableaccess64",
    "symgetlinefromaddr64",
    "symgetmodulebase64",
    "symgetmoduleinfo64",
    "syminitialize",
    "syminitializew",
    "symsetoptions",
];
/// Known WinAPI DLL libraries — handled by WIE's dense/soft dispatch, never
/// loaded as guest modules.
///
/// A static import from one of these libraries that is NOT in the dispatch
/// tables is a genuine missing-WinAPI-stub case (soft placeholder +
/// "unsupported API" stop); an import from any other library is a plausible
/// guest DLL (SDL2.dll, libogg-0.dll, …) eligible for static loading.
#[must_use]
pub fn is_winapi_library(library: &str) -> bool {
    if crate::ucrt::is_ucrt_library(library) {
        return true;
    }
    let lower = library.to_ascii_lowercase();
    let lower = lower.as_str();
    // Dense dispatch-table DLLs, string-dispatch DLLs, virtual API-set
    // prefixes, and the mingw runtime DLLs. Anything else may exist on disk
    // as a loadable guest module.
    matches!(
        lower,
        "kernel32.dll"
            | "advapi32.dll"
            | "user32.dll"
            | "comctl32.dll"
            | "comdlg32.dll"
            | "gdi32.dll"
            | "uxtheme.dll"
            | "winmm.dll"
            | "d3d9.dll"
            | "shell32.dll"
            | "version.dll"
            | "ole32.dll"
            | "oleaut32.dll"
            | "ws2_32.dll"
            | "crypt32.dll"
            | "msimg32.dll"
            | "imm32.dll"
            | "setupapi.dll"
            | "cfgmgr32.dll"
            | "dbghelp.dll"
            | "imagehlp.dll"
            | "wininet.dll"
            | "urlmon.dll"
            | "ntdll.dll"
            | "opengl32.dll"
            | "winhttp.dll"
    ) || lower.starts_with("libwinpthread")
        || lower.starts_with("libstdc++")
        || lower.starts_with("api-ms-win-")
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
        "wininet.dll" => crate::wininet::is_export(n),
        "winhttp.dll" => WINHTTP_DLL_EXPORTS.contains(&n),
        "urlmon.dll" => crate::urlmon::is_export(n),
        "ntdll.dll" => crate::ntdll::is_export(n),
        "opengl32.dll" => crate::opengl32::is_export(n),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winapi_library_classification_keeps_guest_dlls_out() {
        // Dense table, string dispatch, UCRT family, mingw runtime, API sets.
        for lib in [
            "KERNEL32.dll",
            "kernel32.dll",
            "USER32.dll",
            "gdi32.dll",
            "winmm.dll",
            "ole32.dll",
            "oleaut32.dll",
            "ws2_32.dll",
            "ntdll.dll",
            "opengl32.dll",
            "version.dll",
            "d3d9.dll",
            "setupapi.dll",
            "cfgmgr32.dll",
            "dbghelp.dll",
            "imagehlp.dll",
            "wininet.dll",
            "urlmon.dll",
            "winhttp.dll",
            "msvcrt.dll",
            "ucrtbase.dll",
            "api-ms-win-crt-runtime-l1-1-0.dll",
            "libwinpthread-1.dll",
            "libstdc++-6.dll",
        ] {
            assert!(is_winapi_library(lib), "{lib} should classify as WinAPI");
        }
        // Guest DLLs are not WinAPI libraries — they are loadable modules.
        for lib in [
            "SDL2.dll",
            "SDL2_mixer.dll",
            "libogg-0.dll",
            "libopus-0.dll",
            "dll_static_import_funcs.dll",
            "my_plugin.dll",
            "LIBOGG-0.DLL",
        ] {
            assert!(
                !is_winapi_library(lib),
                "{lib} should classify as guest DLL"
            );
        }
    }

    /// Every library that can carry a dispatched export must classify as a
    /// WinAPI library — otherwise pass 1 would treat a genuine WinAPI import
    /// as a loadable guest DLL and pass 2 would search for (and potentially
    /// load) a host file that is not app payload.
    ///
    /// Enforces the coupling between the dispatch surface and
    /// [`is_winapi_library`]: adding a dense row for a new DLL, or a
    /// string-dispatch arm, without classifying the library fails here.
    #[test]
    fn every_dispatched_library_is_winapi_classified() {
        // Dense dispatch rows: every WinApiId's library must classify as WinAPI.
        for raw in 0..crate::dispatch_table::WINAPI_ID_COUNT {
            let raw = u16::try_from(raw).expect("count fits u16");
            let Some(id) = WinApiId::from_u16(raw) else {
                continue;
            };
            let (lib, _name) = winapi_id_export(id).expect("every discriminant has a row");
            assert!(
                is_winapi_library(lib),
                "dense row library {lib} must classify as WinAPI"
            );
        }
        // String-dispatch DLLs without dense rows (dispatch_winapi arms).
        for lib in [
            "ole32.dll",
            "oleaut32.dll",
            "ws2_32.dll",
            "crypt32.dll",
            "msimg32.dll",
            "imm32.dll",
            "setupapi.dll",
            "cfgmgr32.dll",
            "dbghelp.dll",
            "imagehlp.dll",
            "wininet.dll",
            "urlmon.dll",
            "ntdll.dll",
            "opengl32.dll",
            "winhttp.dll",
        ] {
            assert!(is_winapi_library(lib), "{lib} must classify as WinAPI");
        }
        // UCRT family (is_winapi_library delegates to is_ucrt_library) and
        // the mingw runtime DLLs.
        for lib in [
            "msvcrt.dll",
            "msvcr140.dll",
            "msvcp140.dll",
            "ucrtbase.dll",
            "api-ms-win-crt-runtime-l1-1-0.dll",
            "libwinpthread-1.dll",
            "libstdc++-6.dll",
        ] {
            assert!(is_winapi_library(lib), "{lib} must classify as WinAPI");
        }
    }

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
