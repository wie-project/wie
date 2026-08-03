//! Name → id resolution table and lookups for the dense WinAPI dispatch.

use super::WinApiId;

/// Static (library, name, id) rows for one-time resolution.
static WINAPI_NAME_ROWS: &[(&str, &str, WinApiId)] = &[
    (
        "kernel32.dll",
        "getversionexa",
        WinApiId::Kernel32Getversionexa,
    ),
    (
        "kernel32.dll",
        "getmodulehandlea",
        WinApiId::Kernel32Getmodulehandlea,
    ),
    (
        "kernel32.dll",
        "getcommandlinea",
        WinApiId::Kernel32Getcommandlinea,
    ),
    (
        "kernel32.dll",
        "getcommandlinew",
        WinApiId::Kernel32Getcommandlinew,
    ),
    (
        "kernel32.dll",
        "getstartupinfoa",
        WinApiId::Kernel32Getstartupinfoa,
    ),
    (
        // Row placed with the appended variant (406), not after getstartupinfoa,
        // so the id table and the name rows stay in the same order.
        "kernel32.dll",
        "getstartupinfow",
        WinApiId::Kernel32Getstartupinfow,
    ),
    (
        "kernel32.dll",
        "getprocessheap",
        WinApiId::Kernel32Getprocessheap,
    ),
    (
        "kernel32.dll",
        "getsystemtimeasfiletime",
        WinApiId::Kernel32Getsystemtimeasfiletime,
    ),
    (
        "kernel32.dll",
        "getcurrentprocessid",
        WinApiId::Kernel32Getcurrentprocessid,
    ),
    (
        "kernel32.dll",
        "getcurrentthreadid",
        WinApiId::Kernel32Getcurrentthreadid,
    ),
    (
        "kernel32.dll",
        "gettickcount",
        WinApiId::Kernel32Gettickcount,
    ),
    (
        "kernel32.dll",
        "queryperformancecounter",
        WinApiId::Kernel32Queryperformancecounter,
    ),
    ("kernel32.dll", "heapalloc", WinApiId::Kernel32Heapalloc),
    ("kernel32.dll", "heapfree", WinApiId::Kernel32Heapfree),
    ("kernel32.dll", "heaprealloc", WinApiId::Kernel32Heaprealloc),
    ("kernel32.dll", "heapcreate", WinApiId::Kernel32Heapcreate),
    (
        "kernel32.dll",
        "heapsetinformation",
        WinApiId::Kernel32Heapsetinformation,
    ),
    (
        "kernel32.dll",
        "initializecriticalsection",
        WinApiId::Kernel32Initializecriticalsection,
    ),
    (
        "kernel32.dll",
        "entercriticalsection",
        WinApiId::Kernel32Entercriticalsection,
    ),
    (
        "kernel32.dll",
        "leavecriticalsection",
        WinApiId::Kernel32Leavecriticalsection,
    ),
    (
        "kernel32.dll",
        "deletecriticalsection",
        WinApiId::Kernel32Deletecriticalsection,
    ),
    ("kernel32.dll", "flsalloc", WinApiId::Kernel32Flsalloc),
    ("kernel32.dll", "flsfree", WinApiId::Kernel32Flsfree),
    ("kernel32.dll", "flssetvalue", WinApiId::Kernel32Flssetvalue),
    ("kernel32.dll", "flsgetvalue", WinApiId::Kernel32Flsgetvalue),
    (
        "kernel32.dll",
        "getstdhandle",
        WinApiId::Kernel32Getstdhandle,
    ),
    ("kernel32.dll", "getfiletype", WinApiId::Kernel32Getfiletype),
    (
        "kernel32.dll",
        "sethandlecount",
        WinApiId::Kernel32Sethandlecount,
    ),
    (
        "kernel32.dll",
        "getenvironmentstringsw",
        WinApiId::Kernel32Getenvironmentstringsw,
    ),
    (
        "kernel32.dll",
        "freeenvironmentstringsw",
        WinApiId::Kernel32Freeenvironmentstringsw,
    ),
    (
        "kernel32.dll",
        "widechartomultibyte",
        WinApiId::Kernel32Widechartomultibyte,
    ),
    (
        "kernel32.dll",
        "getlasterror",
        WinApiId::Kernel32Getlasterror,
    ),
    (
        "kernel32.dll",
        "setlasterror",
        WinApiId::Kernel32Setlasterror,
    ),
    ("kernel32.dll", "getacp", WinApiId::Kernel32Getacp),
    ("kernel32.dll", "getoemcp", WinApiId::Kernel32Getoemcp),
    ("kernel32.dll", "getcpinfo", WinApiId::Kernel32Getcpinfo),
    (
        "kernel32.dll",
        "isvalidcodepage",
        WinApiId::Kernel32Isvalidcodepage,
    ),
    (
        "kernel32.dll",
        "getstringtypew",
        WinApiId::Kernel32Getstringtypew,
    ),
    (
        "kernel32.dll",
        "multibytetowidechar",
        WinApiId::Kernel32Multibytetowidechar,
    ),
    (
        "kernel32.dll",
        "lcmapstringw",
        WinApiId::Kernel32Lcmapstringw,
    ),
    (
        "kernel32.dll",
        "getmodulefilenamea",
        WinApiId::Kernel32Getmodulefilenamea,
    ),
    (
        "kernel32.dll",
        "getmodulefilenamew",
        WinApiId::Kernel32Getmodulefilenamew,
    ),
    (
        "kernel32.dll",
        "setunhandledexceptionfilter",
        WinApiId::Kernel32Setunhandledexceptionfilter,
    ),
    ("kernel32.dll", "heapsize", WinApiId::Kernel32Heapsize),
    (
        "advapi32.dll",
        "regcreatekeyexa",
        WinApiId::Advapi32Regcreatekeyexa,
    ),
    (
        "advapi32.dll",
        "regopenkeyexa",
        WinApiId::Advapi32Regopenkeyexa,
    ),
    (
        "advapi32.dll",
        "regqueryvalueexa",
        WinApiId::Advapi32Regqueryvalueexa,
    ),
    (
        "advapi32.dll",
        "regqueryvalueexw",
        WinApiId::Advapi32Regqueryvalueexw,
    ),
    (
        "advapi32.dll",
        "regsetvalueexa",
        WinApiId::Advapi32Regsetvalueexa,
    ),
    (
        "advapi32.dll",
        "regsetvalueexw",
        WinApiId::Advapi32Regsetvalueexw,
    ),
    (
        "advapi32.dll",
        "regdeletevaluea",
        WinApiId::Advapi32Regdeletevaluea,
    ),
    ("advapi32.dll", "regclosekey", WinApiId::Advapi32Regclosekey),
    (
        "advapi32.dll",
        "initializesecuritydescriptor",
        WinApiId::Advapi32Initializesecuritydescriptor,
    ),
    (
        "advapi32.dll",
        "setsecuritydescriptordacl",
        WinApiId::Advapi32Setsecuritydescriptordacl,
    ),
    (
        "kernel32.dll",
        "loadlibrarya",
        WinApiId::Kernel32Loadlibrarya,
    ),
    (
        "kernel32.dll",
        "loadlibraryw",
        WinApiId::Kernel32Loadlibraryw,
    ),
    ("kernel32.dll", "freelibrary", WinApiId::Kernel32Freelibrary),
    (
        "kernel32.dll",
        "getprocaddress",
        WinApiId::Kernel32Getprocaddress,
    ),
    (
        "kernel32.dll",
        "getfileattributesa",
        WinApiId::Kernel32Getfileattributesa,
    ),
    (
        "kernel32.dll",
        "getfileattributesw",
        WinApiId::Kernel32Getfileattributesw,
    ),
    (
        "kernel32.dll",
        "findfirstfilew",
        WinApiId::Kernel32Findfirstfilew,
    ),
    (
        "kernel32.dll",
        "findfirstfilea",
        WinApiId::Kernel32Findfirstfilea,
    ),
    (
        "kernel32.dll",
        "findnextfilew",
        WinApiId::Kernel32Findnextfilew,
    ),
    (
        "kernel32.dll",
        "findnextfilea",
        WinApiId::Kernel32Findnextfilea,
    ),
    ("kernel32.dll", "findclose", WinApiId::Kernel32Findclose),
    (
        "user32.dll",
        "getasynckeystate",
        WinApiId::User32Getasynckeystate,
    ),
    ("user32.dll", "peekmessagea", WinApiId::User32Peekmessagea),
    (
        "kernel32.dll",
        "loadlibraryexa",
        WinApiId::Kernel32Loadlibraryexa,
    ),
    (
        "kernel32.dll",
        "loadlibraryexw",
        WinApiId::Kernel32Loadlibraryexw,
    ),
    (
        "kernel32.dll",
        "findresourcea",
        WinApiId::Kernel32Findresourcea,
    ),
    (
        "kernel32.dll",
        "loadresource",
        WinApiId::Kernel32Loadresource,
    ),
    (
        "kernel32.dll",
        "lockresource",
        WinApiId::Kernel32Lockresource,
    ),
    (
        "kernel32.dll",
        "sizeofresource",
        WinApiId::Kernel32Sizeofresource,
    ),
    (
        "kernel32.dll",
        "getsystemdefaultlangid",
        WinApiId::Kernel32Getsystemdefaultlangid,
    ),
    (
        "kernel32.dll",
        "getuserdefaultlangid",
        WinApiId::Kernel32Getuserdefaultlangid,
    ),
    (
        // Row placed with the appended variant (407) next to its family sibling,
        // not at the table end, so id table and name rows stay in the same order.
        "kernel32.dll",
        "getuserdefaultuilanguage",
        WinApiId::Kernel32Getuserdefaultuilanguage,
    ),
    (
        "kernel32.dll",
        "globalmemorystatus",
        WinApiId::Kernel32Globalmemorystatus,
    ),
    (
        "kernel32.dll",
        "getlocaltime",
        WinApiId::Kernel32Getlocaltime,
    ),
    ("user32.dll", "loadicona", WinApiId::User32Loadicona),
    ("user32.dll", "loadcursora", WinApiId::User32Loadcursora),
    (
        "user32.dll",
        "registerclassexw",
        WinApiId::User32Registerclassexw,
    ),
    (
        "user32.dll",
        "registerclassexa",
        WinApiId::User32Registerclassexa,
    ),
    ("kernel32.dll", "createfilew", WinApiId::Kernel32Createfilew),
    ("kernel32.dll", "createfilea", WinApiId::Kernel32Createfilea),
    ("kernel32.dll", "closehandle", WinApiId::Kernel32Closehandle),
    ("user32.dll", "messageboxw", WinApiId::User32Messageboxw),
    ("user32.dll", "messageboxa", WinApiId::User32Messageboxa),
    (
        "kernel32.dll",
        "getfileinformationbyhandle",
        WinApiId::Kernel32Getfileinformationbyhandle,
    ),
    (
        "kernel32.dll",
        "filetimetolocalfiletime",
        WinApiId::Kernel32Filetimetolocalfiletime,
    ),
    (
        "kernel32.dll",
        "filetimetosystemtime",
        WinApiId::Kernel32Filetimetosystemtime,
    ),
    (
        "kernel32.dll",
        "gettimezoneinformation",
        WinApiId::Kernel32Gettimezoneinformation,
    ),
    ("kernel32.dll", "getfiletime", WinApiId::Kernel32Getfiletime),
    (
        "kernel32.dll",
        "setfilepointer",
        WinApiId::Kernel32Setfilepointer,
    ),
    ("kernel32.dll", "getfilesize", WinApiId::Kernel32Getfilesize),
    (
        "kernel32.dll",
        "encodepointer",
        WinApiId::Kernel32Encodepointer,
    ),
    (
        "kernel32.dll",
        "decodepointer",
        WinApiId::Kernel32Decodepointer,
    ),
    (
        "kernel32.dll",
        "initializecriticalsectionandspincount",
        WinApiId::Kernel32Initializecriticalsectionandspincount,
    ),
    (
        "user32.dll",
        "setprocessdpiaware",
        WinApiId::User32Setprocessdpiaware,
    ),
    (
        "user32.dll",
        "trackmouseevent",
        WinApiId::User32Trackmouseevent,
    ),
    (
        "comctl32.dll",
        "dllgetversion",
        WinApiId::Comctl32Dllgetversion,
    ),
    ("kernel32.dll", "readfile", WinApiId::Kernel32Readfile),
    ("kernel32.dll", "writefile", WinApiId::Kernel32Writefile),
    ("user32.dll", "getcursorpos", WinApiId::User32Getcursorpos),
    (
        "user32.dll",
        "getsystemmetrics",
        WinApiId::User32Getsystemmetrics,
    ),
    (
        "user32.dll",
        "monitorfromwindow",
        WinApiId::User32Monitorfromwindow,
    ),
    (
        "user32.dll",
        "getmonitorinfoa",
        WinApiId::User32Getmonitorinfoa,
    ),
    (
        "user32.dll",
        "getmonitorinfow",
        WinApiId::User32Getmonitorinfow,
    ),
    (
        "user32.dll",
        "enumdisplaymonitors",
        WinApiId::User32Enumdisplaymonitors,
    ),
    (
        "user32.dll",
        "enumdisplaydevicesa",
        WinApiId::User32Enumdisplaydevicesa,
    ),
    (
        "user32.dll",
        "enumdisplaydevicesw",
        WinApiId::User32Enumdisplaydevicesw,
    ),
    (
        "user32.dll",
        "monitorfrompoint",
        WinApiId::User32Monitorfrompoint,
    ),
    ("comctl32.dll", "ordinal 17", WinApiId::Comctl32Ordinal17),
    ("user32.dll", "getwindowrect", WinApiId::User32Getwindowrect),
    (
        "user32.dll",
        "getdpiforwindow",
        WinApiId::User32Getdpiforwindow,
    ),
    ("user32.dll", "postmessagea", WinApiId::User32Postmessagea),
    (
        "user32.dll",
        "getsystemmetricsfordpi",
        WinApiId::User32Getsystemmetricsfordpi,
    ),
    (
        "user32.dll",
        "adjustwindowrectexfordpi",
        WinApiId::User32Adjustwindowrectexfordpi,
    ),
    ("user32.dll", "setwindowpos", WinApiId::User32Setwindowpos),
    ("user32.dll", "setscrollinfo", WinApiId::User32Setscrollinfo),
    (
        "user32.dll",
        "scrollwindowex",
        WinApiId::User32Scrollwindowex,
    ),
    ("user32.dll", "scrolldc", WinApiId::User32Scrolldc),
    ("user32.dll", "beginpaint", WinApiId::User32Beginpaint),
    ("user32.dll", "endpaint", WinApiId::User32Endpaint),
    ("user32.dll", "clipcursor", WinApiId::User32Clipcursor),
    ("user32.dll", "getclipcursor", WinApiId::User32Getclipcursor),
    (
        "user32.dll",
        "callmsgfiltera",
        WinApiId::User32Callmsgfiltera,
    ),
    (
        "user32.dll",
        "callmsgfilterw",
        WinApiId::User32Callmsgfilterw,
    ),
    ("user32.dll", "getdc", WinApiId::User32Getdc),
    ("user32.dll", "sendmessagea", WinApiId::User32Sendmessagea),
    ("user32.dll", "sendmessagew", WinApiId::User32Sendmessagew),
    (
        "comdlg32.dll",
        "getopenfilenamea",
        WinApiId::Comdlg32Getopenfilenamea,
    ),
    (
        "comdlg32.dll",
        "getopenfilenamew",
        WinApiId::Comdlg32Getopenfilenamew,
    ),
    (
        "comdlg32.dll",
        "getsavefilenamea",
        WinApiId::Comdlg32Getsavefilenamea,
    ),
    (
        "comdlg32.dll",
        "getsavefilenamew",
        WinApiId::Comdlg32Getsavefilenamew,
    ),
    (
        "comdlg32.dll",
        "commdlgextendederror",
        WinApiId::Comdlg32Commdlgextendederror,
    ),
    (
        "comdlg32.dll",
        "choosecolora",
        WinApiId::Comdlg32Choosecolora,
    ),
    ("gdi32.dll", "selectobject", WinApiId::Gdi32Selectobject),
    (
        "gdi32.dll",
        "gettextextentpoint32a",
        WinApiId::Gdi32Gettextextentpoint32a,
    ),
    (
        "gdi32.dll",
        "gettextextentpoint32w",
        WinApiId::Gdi32Gettextextentpoint32w,
    ),
    ("gdi32.dll", "exttextoutw", WinApiId::Gdi32Exttextoutw),
    ("user32.dll", "releasedc", WinApiId::User32Releasedc),
    (
        "kernel32.dll",
        "getcurrentdirectoryw",
        WinApiId::Kernel32Getcurrentdirectoryw,
    ),
    (
        "kernel32.dll",
        "setcurrentdirectoryw",
        WinApiId::Kernel32Setcurrentdirectoryw,
    ),
    ("user32.dll", "loadimagea", WinApiId::User32Loadimagea),
    ("user32.dll", "loadimagew", WinApiId::User32Loadimagew),
    (
        "comctl32.dll",
        "initcommoncontrolsex",
        WinApiId::Comctl32Initcommoncontrolsex,
    ),
    (
        "uxtheme.dll",
        "setwindowtheme",
        WinApiId::UxthemeSetwindowtheme,
    ),
    (
        "user32.dll",
        "setwindowlongptrw",
        WinApiId::User32Setwindowlongptrw,
    ),
    (
        "user32.dll",
        "getwindowlongptra",
        WinApiId::User32Getwindowlongptra,
    ),
    (
        "user32.dll",
        "getwindowlongptrw",
        WinApiId::User32Getwindowlongptrw,
    ),
    ("gdi32.dll", "getobjecta", WinApiId::Gdi32Getobjecta),
    (
        "comctl32.dll",
        "imagelist_create",
        WinApiId::Comctl32ImagelistCreate,
    ),
    (
        "gdi32.dll",
        "createcompatibledc",
        WinApiId::Gdi32Createcompatibledc,
    ),
    (
        "gdi32.dll",
        "createdibsection",
        WinApiId::Gdi32Createdibsection,
    ),
    (
        "gdi32.dll",
        "createcompatiblebitmap",
        WinApiId::Gdi32Createcompatiblebitmap,
    ),
    ("gdi32.dll", "getdevicecaps", WinApiId::Gdi32Getdevicecaps),
    ("gdi32.dll", "createfonta", WinApiId::Gdi32Createfonta),
    ("gdi32.dll", "createfontw", WinApiId::Gdi32Createfontw),
    (
        "gdi32.dll",
        "createfontindirecta",
        WinApiId::Gdi32Createfontindirecta,
    ),
    (
        "gdi32.dll",
        "gettextmetricsa",
        WinApiId::Gdi32Gettextmetricsa,
    ),
    ("gdi32.dll", "settextcolor", WinApiId::Gdi32Settextcolor),
    ("gdi32.dll", "setbkcolor", WinApiId::Gdi32Setbkcolor),
    ("gdi32.dll", "setbkmode", WinApiId::Gdi32Setbkmode),
    ("gdi32.dll", "textouta", WinApiId::Gdi32Textouta),
    ("gdi32.dll", "bitblt", WinApiId::Gdi32Bitblt),
    ("gdi32.dll", "stretchblt", WinApiId::Gdi32Stretchblt),
    ("gdi32.dll", "patblt", WinApiId::Gdi32Patblt),
    ("gdi32.dll", "getpixel", WinApiId::Gdi32Getpixel),
    ("gdi32.dll", "deletedc", WinApiId::Gdi32Deletedc),
    (
        "comctl32.dll",
        "imagelist_addmasked",
        WinApiId::Comctl32ImagelistAddmasked,
    ),
    (
        "comctl32.dll",
        "imagelist_setbkcolor",
        WinApiId::Comctl32ImagelistSetbkcolor,
    ),
    (
        "comctl32.dll",
        "imagelist_destroy",
        WinApiId::Comctl32ImagelistDestroy,
    ),
    ("gdi32.dll", "deleteobject", WinApiId::Gdi32Deleteobject),
    ("user32.dll", "destroyicon", WinApiId::User32Destroyicon),
    ("user32.dll", "iswindow", WinApiId::User32Iswindow),
    (
        "user32.dll",
        "iswindowvisible",
        WinApiId::User32Iswindowvisible,
    ),
    (
        "user32.dll",
        "iswindowenabled",
        WinApiId::User32Iswindowenabled,
    ),
    ("user32.dll", "getparent", WinApiId::User32Getparent),
    (
        "user32.dll",
        "getactivewindow",
        WinApiId::User32Getactivewindow,
    ),
    (
        "user32.dll",
        "getforegroundwindow",
        WinApiId::User32Getforegroundwindow,
    ),
    ("user32.dll", "showwindow", WinApiId::User32Showwindow),
    ("user32.dll", "enablewindow", WinApiId::User32Enablewindow),
    (
        "user32.dll",
        "setforegroundwindow",
        WinApiId::User32Setforegroundwindow,
    ),
    (
        "user32.dll",
        "setactivewindow",
        WinApiId::User32Setactivewindow,
    ),
    ("user32.dll", "setfocus", WinApiId::User32Setfocus),
    ("user32.dll", "getfocus", WinApiId::User32Getfocus),
    ("user32.dll", "setcapture", WinApiId::User32Setcapture),
    ("user32.dll", "getcapture", WinApiId::User32Getcapture),
    (
        "user32.dll",
        "releasecapture",
        WinApiId::User32Releasecapture,
    ),
    ("user32.dll", "setcursor", WinApiId::User32Setcursor),
    ("user32.dll", "updatewindow", WinApiId::User32Updatewindow),
    (
        "user32.dll",
        "invalidaterect",
        WinApiId::User32Invalidaterect,
    ),
    ("user32.dll", "redrawwindow", WinApiId::User32Redrawwindow),
    (
        "user32.dll",
        "setwindowtexta",
        WinApiId::User32Setwindowtexta,
    ),
    (
        "user32.dll",
        "setwindowtextw",
        WinApiId::User32Setwindowtextw,
    ),
    (
        "user32.dll",
        "getwindowtexta",
        WinApiId::User32Getwindowtexta,
    ),
    (
        "user32.dll",
        "getwindowtextw",
        WinApiId::User32Getwindowtextw,
    ),
    ("user32.dll", "getclientrect", WinApiId::User32Getclientrect),
    ("user32.dll", "movewindow", WinApiId::User32Movewindow),
    (
        "user32.dll",
        "screentoclient",
        WinApiId::User32Screentoclient,
    ),
    (
        "user32.dll",
        "clienttoscreen",
        WinApiId::User32Clienttoscreen,
    ),
    (
        "user32.dll",
        "getdesktopwindow",
        WinApiId::User32Getdesktopwindow,
    ),
    ("user32.dll", "getsyscolor", WinApiId::User32Getsyscolor),
    (
        "user32.dll",
        "getsyscolorbrush",
        WinApiId::User32Getsyscolorbrush,
    ),
    (
        "user32.dll",
        "getdialogbaseunits",
        WinApiId::User32Getdialogbaseunits,
    ),
    ("user32.dll", "setrect", WinApiId::User32Setrect),
    ("user32.dll", "isiconic", WinApiId::User32Isiconic),
    ("user32.dll", "iszoomed", WinApiId::User32Iszoomed),
    (
        "user32.dll",
        "getwindowthreadprocessid",
        WinApiId::User32Getwindowthreadprocessid,
    ),
    ("user32.dll", "getdlgctrlid", WinApiId::User32Getdlgctrlid),
    (
        "kernel32.dll",
        "getcurrentprocess",
        WinApiId::Kernel32Getcurrentprocess,
    ),
    ("kernel32.dll", "sleep", WinApiId::Kernel32Sleep),
    ("winmm.dll", "timegettime", WinApiId::WinmmTimegettime),
    ("kernel32.dll", "localalloc", WinApiId::Kernel32Localalloc),
    ("kernel32.dll", "localfree", WinApiId::Kernel32Localfree),
    ("kernel32.dll", "globalalloc", WinApiId::Kernel32Globalalloc),
    ("kernel32.dll", "globalfree", WinApiId::Kernel32Globalfree),
    ("kernel32.dll", "globallock", WinApiId::Kernel32Globallock),
    (
        "kernel32.dll",
        "globalunlock",
        WinApiId::Kernel32Globalunlock,
    ),
    ("kernel32.dll", "globalsize", WinApiId::Kernel32Globalsize),
    ("kernel32.dll", "muldiv", WinApiId::Kernel32Muldiv),
    ("user32.dll", "getcursor", WinApiId::User32Getcursor),
    ("user32.dll", "ischild", WinApiId::User32Ischild),
    ("user32.dll", "getwindow", WinApiId::User32Getwindow),
    (
        "user32.dll",
        "setkeyboardstate",
        WinApiId::User32Setkeyboardstate,
    ),
    (
        "user32.dll",
        "getkeyboardstate",
        WinApiId::User32Getkeyboardstate,
    ),
    ("user32.dll", "getkeystate", WinApiId::User32Getkeystate),
    (
        "user32.dll",
        "mapvirtualkeya",
        WinApiId::User32Mapvirtualkeya,
    ),
    (
        "user32.dll",
        "setwindowlongptra",
        WinApiId::User32Setwindowlongptra,
    ),
    ("user32.dll", "settimer", WinApiId::User32Settimer),
    ("user32.dll", "killtimer", WinApiId::User32Killtimer),
    (
        "user32.dll",
        "adjustwindowrectex",
        WinApiId::User32Adjustwindowrectex,
    ),
    (
        "kernel32.dll",
        "globaladdatoma",
        WinApiId::Kernel32Globaladdatoma,
    ),
    (
        "kernel32.dll",
        "globaldeleteatom",
        WinApiId::Kernel32Globaldeleteatom,
    ),
    (
        "user32.dll",
        "setwindowshookexw",
        WinApiId::User32Setwindowshookexw,
    ),
    (
        "user32.dll",
        "unhookwindowshookex",
        WinApiId::User32Unhookwindowshookex,
    ),
    (
        "user32.dll",
        "callnexthookex",
        WinApiId::User32Callnexthookex,
    ),
    (
        "kernel32.dll",
        "getfullpathnamew",
        WinApiId::Kernel32Getfullpathnamew,
    ),
    (
        "kernel32.dll",
        "getfullpathnamea",
        WinApiId::Kernel32Getfullpathnamea,
    ),
    (
        "kernel32.dll",
        "getcurrentdirectorya",
        WinApiId::Kernel32Getcurrentdirectorya,
    ),
    (
        "kernel32.dll",
        "setcurrentdirectorya",
        WinApiId::Kernel32Setcurrentdirectorya,
    ),
    (
        "kernel32.dll",
        "createdirectoryw",
        WinApiId::Kernel32Createdirectoryw,
    ),
    (
        "kernel32.dll",
        "createdirectorya",
        WinApiId::Kernel32Createdirectorya,
    ),
    (
        "kernel32.dll",
        "removedirectoryw",
        WinApiId::Kernel32Removefirectoryw,
    ),
    (
        "kernel32.dll",
        "removedirectorya",
        WinApiId::Kernel32Removefirectorya,
    ),
    ("kernel32.dll", "deletefilew", WinApiId::Kernel32Deletefilew),
    ("kernel32.dll", "deletefilea", WinApiId::Kernel32Deletefilea),
    ("kernel32.dll", "movefilew", WinApiId::Kernel32Movefilew),
    ("kernel32.dll", "movefilea", WinApiId::Kernel32Movefilea),
    (
        "kernel32.dll",
        "gettemppathw",
        WinApiId::Kernel32Gettemppathw,
    ),
    (
        "kernel32.dll",
        "gettemppatha",
        WinApiId::Kernel32Gettemppatha,
    ),
    (
        "kernel32.dll",
        "gettempfilenamew",
        WinApiId::Kernel32Gettempfilenamew,
    ),
    (
        "kernel32.dll",
        "gettempfilenamea",
        WinApiId::Kernel32Gettempfilenamea,
    ),
    (
        "kernel32.dll",
        "getdrivetypew",
        WinApiId::Kernel32Getdrivetypew,
    ),
    (
        "kernel32.dll",
        "getdrivetypea",
        WinApiId::Kernel32Getdrivetypea,
    ),
    (
        "kernel32.dll",
        "getlogicaldrives",
        WinApiId::Kernel32Getlogicaldrives,
    ),
    (
        "kernel32.dll",
        "getsystemdirectoryw",
        WinApiId::Kernel32Getsystemdirectoryw,
    ),
    (
        "kernel32.dll",
        "getsystemdirectorya",
        WinApiId::Kernel32Getsystemdirectorya,
    ),
    (
        "kernel32.dll",
        "getwindowsdirectoryw",
        WinApiId::Kernel32Getwindowsdirectoryw,
    ),
    (
        "kernel32.dll",
        "getwindowsdirectorya",
        WinApiId::Kernel32Getwindowsdirectorya,
    ),
    (
        "kernel32.dll",
        "getfilesizeex",
        WinApiId::Kernel32Getfilesizeex,
    ),
    (
        "kernel32.dll",
        "setfilepointerex",
        WinApiId::Kernel32Setfilepointerex,
    ),
    (
        "kernel32.dll",
        "setendoffile",
        WinApiId::Kernel32Setendoffile,
    ),
    (
        "kernel32.dll",
        "flushfilebuffers",
        WinApiId::Kernel32Flushfilebuffers,
    ),
    ("d3d9.dll", "direct3dcreate9", WinApiId::D3d9Direct3dcreate9),
    (
        "d3d9.dll",
        "idirect3d9::getadaptercount",
        WinApiId::D3d9Idirect3d9Getadaptercount,
    ),
    (
        "d3d9.dll",
        "idirect3d9::getadaptermonitor",
        WinApiId::D3d9Idirect3d9Getadaptermonitor,
    ),
    (
        "d3d9.dll",
        "idirect3d9::getdevicecaps",
        WinApiId::D3d9Idirect3d9Getdevicecaps,
    ),
    (
        "d3d9.dll",
        "idirect3d9::getadapterdisplaymode",
        WinApiId::D3d9Idirect3d9Getadapterdisplaymode,
    ),
    (
        "d3d9.dll",
        "idirect3d9::createdevice",
        WinApiId::D3d9Idirect3d9Createdevice,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::setvertexshader",
        WinApiId::D3d9Idirect3ddevice9Setvertexshader,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::setfvf",
        WinApiId::D3d9Idirect3ddevice9Setfvf,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::setrenderstate",
        WinApiId::D3d9Idirect3ddevice9Setrenderstate,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::settexturestagestate",
        WinApiId::D3d9Idirect3ddevice9Settexturestagestate,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::setsamplerstate",
        WinApiId::D3d9Idirect3ddevice9Setsamplerstate,
    ),
    (
        "user32.dll",
        "enablemenuitem",
        WinApiId::User32Enablemenuitem,
    ),
    ("user32.dll", "checkmenuitem", WinApiId::User32Checkmenuitem),
    ("user32.dll", "getmessagea", WinApiId::User32Getmessagea),
    (
        "user32.dll",
        "translatemessage",
        WinApiId::User32Translatemessage,
    ),
    (
        "user32.dll",
        "defwindowproca",
        WinApiId::User32Defwindowproca,
    ),
    (
        "user32.dll",
        "defwindowprocw",
        WinApiId::User32Defwindowprocw,
    ),
    ("user32.dll", "defframeproca", WinApiId::User32Defframeproca),
    ("user32.dll", "defframeprocw", WinApiId::User32Defframeprocw),
    (
        "user32.dll",
        "defmdichildproca",
        WinApiId::User32Defmdichildproca,
    ),
    (
        "user32.dll",
        "defmdichildprocw",
        WinApiId::User32Defmdichildprocw,
    ),
    ("user32.dll", "createmenu", WinApiId::User32Createmenu),
    (
        "user32.dll",
        "createpopupmenu",
        WinApiId::User32Createpopupmenu,
    ),
    ("user32.dll", "appendmenua", WinApiId::User32Appendmenua),
    ("user32.dll", "appendmenuw", WinApiId::User32Appendmenuw),
    ("user32.dll", "setmenu", WinApiId::User32Setmenu),
    ("user32.dll", "destroymenu", WinApiId::User32Destroymenu),
    ("user32.dll", "removemenu", WinApiId::User32Removemenu),
    ("user32.dll", "deletemenu", WinApiId::User32Deletemenu),
    ("user32.dll", "modifymenua", WinApiId::User32Modifymenua),
    ("user32.dll", "modifymenuw", WinApiId::User32Modifymenuw),
    ("user32.dll", "getsystemmenu", WinApiId::User32Getsystemmenu),
    (
        "user32.dll",
        "trackpopupmenu",
        WinApiId::User32Trackpopupmenu,
    ),
    (
        "user32.dll",
        "getmenuiteminfoa",
        WinApiId::User32Getmenuiteminfoa,
    ),
    (
        "user32.dll",
        "getmenuiteminfow",
        WinApiId::User32Getmenuiteminfow,
    ),
    (
        "user32.dll",
        "setmenuiteminfoa",
        WinApiId::User32Setmenuiteminfoa,
    ),
    (
        "user32.dll",
        "setmenuiteminfow",
        WinApiId::User32Setmenuiteminfow,
    ),
    (
        "user32.dll",
        "checkmenuradioitem",
        WinApiId::User32Checkmenuradioitem,
    ),
    (
        "user32.dll",
        "dispatchmessagea",
        WinApiId::User32Dispatchmessagea,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::release",
        WinApiId::D3d9Idirect3ddevice9Release,
    ),
    (
        "d3d9.dll",
        "idirect3d9::release",
        WinApiId::D3d9Idirect3d9Release,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::present",
        WinApiId::D3d9Idirect3ddevice9Present,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::beginscene",
        WinApiId::D3d9Idirect3ddevice9Beginscene,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::endscene",
        WinApiId::D3d9Idirect3ddevice9Endscene,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::clear",
        WinApiId::D3d9Idirect3ddevice9Clear,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::settransform",
        WinApiId::D3d9Idirect3ddevice9Settransform,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::setviewport",
        WinApiId::D3d9Idirect3ddevice9Setviewport,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::getviewport",
        WinApiId::D3d9Idirect3ddevice9Getviewport,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::drawprimitive",
        WinApiId::D3d9Idirect3ddevice9Drawprimitive,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::drawindexedprimitive",
        WinApiId::D3d9Idirect3ddevice9Drawindexedprimitive,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::drawprimitiveup",
        WinApiId::D3d9Idirect3ddevice9Drawprimitiveup,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::drawindexedprimitiveup",
        WinApiId::D3d9Idirect3ddevice9Drawindexedprimitiveup,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::setstreamsource",
        WinApiId::D3d9Idirect3ddevice9Setstreamsource,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::setindices",
        WinApiId::D3d9Idirect3ddevice9Setindices,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::createvertexbuffer",
        WinApiId::D3d9Idirect3ddevice9Createvertexbuffer,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::createindexbuffer",
        WinApiId::D3d9Idirect3ddevice9Createindexbuffer,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::createtexture",
        WinApiId::D3d9Idirect3ddevice9Createtexture,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::settexture",
        WinApiId::D3d9Idirect3ddevice9Settexture,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::gettexture",
        WinApiId::D3d9Idirect3ddevice9Gettexture,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::gettexturestagestate",
        WinApiId::D3d9Idirect3ddevice9Gettexturestagestate,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::getsamplerstate",
        WinApiId::D3d9Idirect3ddevice9Getsamplerstate,
    ),
    (
        "d3d9.dll",
        "idirect3dtexture9::getlevelcount",
        WinApiId::D3d9Idirect3dtexture9Getlevelcount,
    ),
    (
        "d3d9.dll",
        "idirect3dtexture9::getsurfacelevel",
        WinApiId::D3d9Idirect3dtexture9Getsurfacelevel,
    ),
    (
        "d3d9.dll",
        "idirect3dtexture9::lockrect",
        WinApiId::D3d9Idirect3dtexture9Lockrect,
    ),
    (
        "d3d9.dll",
        "idirect3dtexture9::unlockrect",
        WinApiId::D3d9Idirect3dtexture9Unlockrect,
    ),
    (
        "d3d9.dll",
        "idirect3dtexture9::release",
        WinApiId::D3d9Idirect3dtexture9Release,
    ),
    (
        "d3d9.dll",
        "idirect3dsurface9::lockrect",
        WinApiId::D3d9Idirect3dsurface9Lockrect,
    ),
    (
        "d3d9.dll",
        "idirect3dsurface9::unlockrect",
        WinApiId::D3d9Idirect3dsurface9Unlockrect,
    ),
    (
        "d3d9.dll",
        "idirect3dsurface9::release",
        WinApiId::D3d9Idirect3dsurface9Release,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::createdepthstencilsurface",
        WinApiId::D3d9Idirect3ddevice9Createdepthstencilsurface,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::setdepthstencilsurface",
        WinApiId::D3d9Idirect3ddevice9Setdepthstencilsurface,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::getdepthstencilsurface",
        WinApiId::D3d9Idirect3ddevice9Getdepthstencilsurface,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::getrenderstate",
        WinApiId::D3d9Idirect3ddevice9Getrenderstate,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::createvertexshader",
        WinApiId::D3d9Idirect3ddevice9Createvertexshader,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::getvertexshader",
        WinApiId::D3d9Idirect3ddevice9Getvertexshader,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::setvertexshaderconstantf",
        WinApiId::D3d9Idirect3ddevice9Setvertexshaderconstantf,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::getvertexshaderconstantf",
        WinApiId::D3d9Idirect3ddevice9Getvertexshaderconstantf,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::createpixelshader",
        WinApiId::D3d9Idirect3ddevice9Createpixelshader,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::setpixelshader",
        WinApiId::D3d9Idirect3ddevice9Setpixelshader,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::getpixelshader",
        WinApiId::D3d9Idirect3ddevice9Getpixelshader,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::setpixelshaderconstantf",
        WinApiId::D3d9Idirect3ddevice9Setpixelshaderconstantf,
    ),
    (
        "d3d9.dll",
        "idirect3ddevice9::getpixelshaderconstantf",
        WinApiId::D3d9Idirect3ddevice9Getpixelshaderconstantf,
    ),
    (
        "d3d9.dll",
        "idirect3dpixelshader9::release",
        WinApiId::D3d9Idirect3dpixelshader9Release,
    ),
    (
        "d3d9.dll",
        "idirect3dvertexshader9::release",
        WinApiId::D3d9Idirect3dvertexshader9Release,
    ),
    ("user32.dll", "getmenu", WinApiId::User32Getmenu),
    ("gdi32.dll", "getstockobject", WinApiId::Gdi32Getstockobject),
    (
        "kernel32.dll",
        "writeconsolew",
        WinApiId::Kernel32Writeconsolew,
    ),
    (
        "kernel32.dll",
        "writeconsolea",
        WinApiId::Kernel32Writeconsolea,
    ),
    (
        "kernel32.dll",
        "readconsolew",
        WinApiId::Kernel32Readconsolew,
    ),
    (
        "kernel32.dll",
        "readconsolea",
        WinApiId::Kernel32Readconsolea,
    ),
    (
        "kernel32.dll",
        "getconsolemode",
        WinApiId::Kernel32Getconsolemode,
    ),
    (
        "kernel32.dll",
        "setconsolemode",
        WinApiId::Kernel32Setconsolemode,
    ),
    (
        "kernel32.dll",
        "writeconsoleoutputw",
        WinApiId::Kernel32Writeconsoleoutputw,
    ),
    (
        "kernel32.dll",
        "fillconsoleoutputcharacterw",
        WinApiId::Kernel32Fillconsoleoutputcharacterw,
    ),
    (
        "kernel32.dll",
        "setconsolecursorposition",
        WinApiId::Kernel32Setconsolecursorposition,
    ),
    (
        "kernel32.dll",
        "setconsoletextattribute",
        WinApiId::Kernel32Setconsoletextattribute,
    ),
    (
        "kernel32.dll",
        "gettickcount64",
        WinApiId::Kernel32Gettickcount64,
    ),
    (
        "kernel32.dll",
        "getenvironmentvariablew",
        WinApiId::Kernel32Getenvironmentvariablew,
    ),
    (
        "kernel32.dll",
        "setenvironmentvariablew",
        WinApiId::Kernel32Setenvironmentvariablew,
    ),
    (
        "user32.dll",
        "createwindowexa",
        WinApiId::User32Createwindowexa,
    ),
    (
        "user32.dll",
        "createwindowexw",
        WinApiId::User32Createwindowexw,
    ),
    ("user32.dll", "destroywindow", WinApiId::User32Destroywindow),
    (
        "user32.dll",
        "postquitmessage",
        WinApiId::User32Postquitmessage,
    ),
    ("user32.dll", "getmessagew", WinApiId::User32Getmessagew),
    ("user32.dll", "peekmessagew", WinApiId::User32Peekmessagew),
    (
        "user32.dll",
        "dispatchmessagew",
        WinApiId::User32Dispatchmessagew,
    ),
    ("user32.dll", "postmessagew", WinApiId::User32Postmessagew),
    (
        "user32.dll",
        "registerclassa",
        WinApiId::User32Registerclassa,
    ),
    (
        "user32.dll",
        "registerclassw",
        WinApiId::User32Registerclassw,
    ),
    (
        "user32.dll",
        "unregisterclassa",
        WinApiId::User32Unregisterclassa,
    ),
    (
        "user32.dll",
        "unregisterclassw",
        WinApiId::User32Unregisterclassw,
    ),
    ("user32.dll", "validaterect", WinApiId::User32Validateirect),
    (
        "user32.dll",
        "setwindowlonga",
        WinApiId::User32Setwindowlonga,
    ),
    (
        "user32.dll",
        "setwindowlongw",
        WinApiId::User32Setwindowlongw,
    ),
    ("user32.dll", "getwindowdc", WinApiId::User32Getwindowdc),
    ("user32.dll", "getclassnamea", WinApiId::User32Getclassnamea),
    ("user32.dll", "getclassnamew", WinApiId::User32Getclassnamew),
    (
        "user32.dll",
        "getclasslongptra",
        WinApiId::User32Getclasslongptra,
    ),
    (
        "user32.dll",
        "getclasslongptrw",
        WinApiId::User32Getclasslongptrw,
    ),
    (
        "user32.dll",
        "setclasslongptra",
        WinApiId::User32Setclasslongptra,
    ),
    (
        "user32.dll",
        "setclasslongptrw",
        WinApiId::User32Setclasslongptrw,
    ),
    ("user32.dll", "drawmenubar", WinApiId::User32Drawmenubar),
    ("user32.dll", "getmenustate", WinApiId::User32Getmenustate),
    (
        "gdi32.dll",
        "createsolidbrush",
        WinApiId::Gdi32Createsolidbrush,
    ),
    ("user32.dll", "fillrect", WinApiId::User32Fillrect),
    ("gdi32.dll", "createpen", WinApiId::Gdi32Createpen),
    ("gdi32.dll", "textoutw", WinApiId::Gdi32Textoutw),
    ("user32.dll", "drawtexta", WinApiId::Gdi32Drawtexta),
    ("user32.dll", "drawtextw", WinApiId::Gdi32Drawtextw),
    (
        "user32.dll",
        "createdialogparama",
        WinApiId::User32Createdialogparama,
    ),
    (
        "user32.dll",
        "createdialogparamw",
        WinApiId::User32Createdialogparamw,
    ),
    (
        "user32.dll",
        "isdialogmessagea",
        WinApiId::User32Isdialogmessagea,
    ),
    (
        "user32.dll",
        "isdialogmessagew",
        WinApiId::User32Isdialogmessagew,
    ),
    ("user32.dll", "enddialog", WinApiId::User32Enddialog),
    // mingw's import lib provides the bare `GetDlgItem` alias (no A/W
    // suffix) — the demo PE imports it as-is, so resolve it to the ANSI
    // handler like real Windows does for the macro-less name.
    ("user32.dll", "getdlgitem", WinApiId::User32Getdlgitema),
    ("user32.dll", "getdlgitema", WinApiId::User32Getdlgitema),
    ("user32.dll", "getdlgitemw", WinApiId::User32Getdlgitemw),
    (
        "user32.dll",
        "getdlgitemtexta",
        WinApiId::User32Getdlgitemtexta,
    ),
    (
        "user32.dll",
        "getdlgitemtextw",
        WinApiId::User32Getdlgitemtextw,
    ),
    (
        "user32.dll",
        "setdlgitemtexta",
        WinApiId::User32Setdlgitemtexta,
    ),
    (
        "user32.dll",
        "setdlgitemtextw",
        WinApiId::User32Setdlgitemtextw,
    ),
    ("user32.dll", "defdlgproca", WinApiId::User32Defdlgproca),
    ("user32.dll", "defdlgprocw", WinApiId::User32Defdlgprocw),
    (
        // Rows placed at the table end with the appended variants (408/409),
        // so the id table and the name rows stay in the same order.
        "user32.dll",
        "registerwindowmessagea",
        WinApiId::User32Registerwindowmessagea,
    ),
    (
        "user32.dll",
        "registerwindowmessagew",
        WinApiId::User32Registerwindowmessagew,
    ),
    (
        // Rows placed at the table end with the appended variants (410/411),
        // so the id table and the name rows stay in the same order.
        "user32.dll",
        "loadstringa",
        WinApiId::User32Loadstringa,
    ),
    ("user32.dll", "loadstringw", WinApiId::User32Loadstringw),
    (
        // Rows placed at the table end with the appended variants (412/413),
        // so the id table and the name rows stay in the same order.
        "advapi32.dll",
        "regopenkeya",
        WinApiId::Advapi32Regopenkeya,
    ),
    ("advapi32.dll", "regopenkeyw", WinApiId::Advapi32Regopenkeyw),
    (
        // Row placed at the table end with the appended variant (414),
        // so the id table and the name rows stay in the same order.
        "gdi32.dll",
        "createfontindirectw",
        WinApiId::Gdi32Createfontindirectw,
    ),
    (
        // Row placed at the table end with the appended variant (415),
        // so the id table and the name rows stay in the same order.
        "user32.dll",
        "loadiconw",
        WinApiId::User32Loadiconw,
    ),
    (
        // Row placed at the table end with the appended variant (416),
        // so the id table and the name rows stay in the same order.
        "user32.dll",
        "loadcursorw",
        WinApiId::User32Loadcursorw,
    ),
    (
        // Row placed at the table end with the appended variant (417),
        // so the id table and the name rows stay in the same order.
        "shell32.dll",
        "dragacceptfiles",
        WinApiId::Shell32Dragacceptfiles,
    ),
    (
        // Rows placed at the table end with the appended variants (418/419),
        // so the id table and the name rows stay in the same order.
        "comctl32.dll",
        "createstatuswindowa",
        WinApiId::Comctl32Createstatuswindowa,
    ),
    (
        "comctl32.dll",
        "createstatuswindoww",
        WinApiId::Comctl32Createstatuswindoww,
    ),
    (
        // Rows placed at the table end with the appended variants (420/421),
        // so the id table and the name rows stay in the same order.
        "comdlg32.dll",
        "getfiletitlea",
        WinApiId::Comdlg32Getfiletitlea,
    ),
    (
        "comdlg32.dll",
        "getfiletitlew",
        WinApiId::Comdlg32Getfiletitlew,
    ),
    (
        // Rows placed at the table end with the appended variants (422/423),
        // so the id table and the name rows stay in the same order.
        "user32.dll",
        "getwindowtextlengtha",
        WinApiId::User32Getwindowtextlengtha,
    ),
    (
        "user32.dll",
        "getwindowtextlengthw",
        WinApiId::User32Getwindowtextlengthw,
    ),
    (
        // Rows placed at the table end with the appended variants (424/425),
        // so the id table and the name rows stay in the same order.
        "user32.dll",
        "getwindowplacement",
        WinApiId::User32Getwindowplacement,
    ),
    (
        "user32.dll",
        "setwindowplacement",
        WinApiId::User32Setwindowplacement,
    ),
    (
        // Rows placed at the table end with the appended variants (426-429),
        // so the id table and the name rows stay in the same order.
        "user32.dll",
        "loadacceleratorsa",
        WinApiId::User32Loadacceleratorsa,
    ),
    (
        "user32.dll",
        "loadacceleratorsw",
        WinApiId::User32Loadacceleratorsw,
    ),
    (
        "user32.dll",
        "translateacceleratora",
        WinApiId::User32Translateacceleratora,
    ),
    (
        "user32.dll",
        "translateacceleratorw",
        WinApiId::User32Translateacceleratorw,
    ),
    (
        // Row placed at the table end with the appended variant (430), so
        // the id table and the name rows stay in the same order.
        "user32.dll",
        "destroyacceleratortable",
        WinApiId::User32Destroyacceleratortable,
    ),
    (
        // Rows placed at the table end with the appended variants (431/432),
        // so the id table and the name rows stay in the same order.
        "user32.dll",
        "loadmenua",
        WinApiId::User32Loadmenua,
    ),
    ("user32.dll", "loadmenuw", WinApiId::User32Loadmenuw),
    (
        // Row placed at the table end with the appended variant (433), so
        // the id table and the name rows stay in the same order.
        "user32.dll",
        "isclipboardformatavailable",
        WinApiId::User32Isclipboardformatavailable,
    ),
    (
        // Rows placed at the table end with the appended variants (434-437),
        // so the id table and the name rows stay in the same order.
        "shell32.dll",
        "dragqueryfilew",
        WinApiId::Shell32Dragqueryfilew,
    ),
    (
        "shell32.dll",
        "dragqueryfilea",
        WinApiId::Shell32Dragqueryfilea,
    ),
    (
        "shell32.dll",
        "dragquerypoint",
        WinApiId::Shell32Dragquerypoint,
    ),
    ("shell32.dll", "dragfinish", WinApiId::Shell32Dragfinish),
    (
        // Rows placed at the table end with the appended variants (438/439),
        // so the id table and the name rows stay in the same order.
        "kernel32.dll",
        "locallock",
        WinApiId::Kernel32Locallock,
    ),
    ("kernel32.dll", "localunlock", WinApiId::Kernel32Localunlock),
    (
        // Rows placed at the table end with the appended variants (440/441),
        // so the id table and the name rows stay in the same order.
        "kernel32.dll",
        "gettimeformatw",
        WinApiId::Kernel32Gettimeformatw,
    ),
    (
        "kernel32.dll",
        "getdateformatw",
        WinApiId::Kernel32Getdateformatw,
    ),
    (
        // Rows placed at the table end with the appended variants (442-444),
        // so the id table and the name rows stay in the same order.
        "user32.dll",
        "getdlgitemint",
        WinApiId::User32Getdlgitemint,
    ),
    ("user32.dll", "setdlgitemint", WinApiId::User32Setdlgitemint),
    (
        "user32.dll",
        "senddlgitemmessagew",
        WinApiId::User32Senddlgitemmessagew,
    ),
    (
        // Rows placed at the table end with the appended variants (445/446),
        // so the id table and the name rows stay in the same order.
        "shell32.dll",
        "shellaboutw",
        WinApiId::Shell32Shellaboutw,
    ),
    (
        "shell32.dll",
        "shellexecutew",
        WinApiId::Shell32Shellexecutew,
    ),
];

/// Resolve library/export to id. Case-insensitive, allocation-free.
/// Intended for session setup (once per import), not the hot emu loop.
#[must_use]
pub fn resolve_winapi_id(library: &str, name: &str) -> Option<WinApiId> {
    for &(lib, export, id) in WINAPI_NAME_ROWS {
        if lib.eq_ignore_ascii_case(library) && export.eq_ignore_ascii_case(name) {
            return Some(id);
        }
    }
    None
}

/// Reverse lookup: dense id → (`library`, `export`) as stored in the name table.
///
/// Names are lowercase (as in `WINAPI_NAME_ROWS`). Used for trace/profile only.
#[must_use]
pub fn winapi_id_export(id: WinApiId) -> Option<(&'static str, &'static str)> {
    for &(lib, export, row_id) in WINAPI_NAME_ROWS {
        if row_id == id {
            return Some((lib, export));
        }
    }
    None
}

#[must_use]
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
            "shgetfolderpathw" | "shgetpathfromidlistw" | "shbrowseforfolderw"
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
                | "openfilemappingw"
                | "openfilemappinga"
        );
    }
    false
}
