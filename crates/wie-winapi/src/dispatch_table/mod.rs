//! Dense WinAPI id dispatch: the `WinApiId` enum, name → id resolution,
//! the hot-path dispatch match, and per-API trait flags.
//!
//! The name table and its lookups live in the `names` submodule; everything
//! else (the enum, dispatch, traits) stays here.

mod names;
pub use names::{is_winapi_implemented, resolve_winapi_id, winapi_id_export};

mod traits;
pub use traits::WinApiTraits;

use crate::{
    HandlerContext, WinApiHandlerResult, advapi32, comctl32, comdlg32, d3d9, gdi32, kernel32,
    shell32, user32, uxtheme, version, winmm,
};
use anyhow::{Result, bail};
use strum::{EnumCount, FromRepr};

/// Dense handler identifier. Resolved once when building the fake-API table.
///
/// `FromRepr` replaces the old hand-checked `u16` → `Self` transmute;
/// `EnumCount` supplies `COUNT` (the variant count, see `WINAPI_ID_COUNT` for
/// why that is one less than the discriminant span).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, FromRepr, EnumCount)]
#[repr(u16)]
pub enum WinApiId {
    Kernel32Getversionexa = 0,
    Kernel32Getmodulehandlea = 1,
    Kernel32Getcommandlinea = 2,
    Kernel32Getcommandlinew = 3,
    Kernel32Getstartupinfoa = 4,
    Kernel32Getprocessheap = 5,
    Kernel32Getsystemtimeasfiletime = 6,
    Kernel32Getcurrentprocessid = 7,
    Kernel32Getcurrentthreadid = 8,
    Kernel32Gettickcount = 9,
    Kernel32Queryperformancecounter = 10,
    Kernel32Heapalloc = 11,
    Kernel32Heapfree = 12,
    Kernel32Heaprealloc = 13,
    Kernel32Heapcreate = 14,
    Kernel32Heapsetinformation = 15,
    Kernel32Initializecriticalsection = 16,
    Kernel32Entercriticalsection = 17,
    Kernel32Leavecriticalsection = 18,
    Kernel32Deletecriticalsection = 19,
    Kernel32Flsalloc = 20,
    Kernel32Flsfree = 21,
    Kernel32Flssetvalue = 22,
    Kernel32Flsgetvalue = 23,
    Kernel32Getstdhandle = 24,
    Kernel32Getfiletype = 25,
    Kernel32Sethandlecount = 26,
    Kernel32Getenvironmentstringsw = 27,
    Kernel32Freeenvironmentstringsw = 28,
    Kernel32Widechartomultibyte = 29,
    Kernel32Getlasterror = 30,
    Kernel32Setlasterror = 31,
    Kernel32Getacp = 32,
    Kernel32Getoemcp = 33,
    Kernel32Getcpinfo = 34,
    Kernel32Isvalidcodepage = 35,
    Kernel32Getstringtypew = 36,
    Kernel32Multibytetowidechar = 37,
    Kernel32Lcmapstringw = 38,
    Kernel32Getmodulefilenamea = 39,
    Kernel32Getmodulefilenamew = 40,
    Kernel32Setunhandledexceptionfilter = 41,
    Kernel32Heapsize = 42,
    Advapi32Regcreatekeyexa = 43,
    Advapi32Regopenkeyexa = 44,
    Advapi32Regqueryvalueexa = 45,
    Advapi32Regqueryvalueexw = 46,
    Advapi32Regsetvalueexa = 47,
    Advapi32Regsetvalueexw = 48,
    Advapi32Regdeletevaluea = 49,
    Advapi32Regclosekey = 50,
    Advapi32Initializesecuritydescriptor = 51,
    Advapi32Setsecuritydescriptordacl = 52,
    Kernel32Loadlibrarya = 53,
    Kernel32Loadlibraryw = 54,
    Kernel32Freelibrary = 55,
    Kernel32Getprocaddress = 56,
    Kernel32Getfileattributesa = 57,
    Kernel32Getfileattributesw = 58,
    Kernel32Findfirstfilew = 59,
    Kernel32Findfirstfilea = 60,
    Kernel32Findnextfilew = 61,
    Kernel32Findnextfilea = 62,
    Kernel32Findclose = 63,
    User32Getasynckeystate = 64,
    User32Peekmessagea = 65,
    Kernel32Loadlibraryexa = 66,
    Kernel32Loadlibraryexw = 67,
    Kernel32Findresourcea = 68,
    Kernel32Loadresource = 69,
    Kernel32Lockresource = 70,
    Kernel32Sizeofresource = 71,
    Kernel32Getsystemdefaultlangid = 72,
    Kernel32Getuserdefaultlangid = 73,
    Kernel32Globalmemorystatus = 74,
    Kernel32Getlocaltime = 75,
    User32Loadicona = 76,
    User32Loadcursora = 77,
    User32Registerclassexw = 78,
    User32Registerclassexa = 79,
    Kernel32Createfilew = 80,
    Kernel32Createfilea = 81,
    Kernel32Closehandle = 82,
    User32Messageboxw = 83,
    User32Messageboxa = 84,
    Kernel32Getfileinformationbyhandle = 85,
    Kernel32Filetimetolocalfiletime = 86,
    Kernel32Filetimetosystemtime = 87,
    Kernel32Gettimezoneinformation = 88,
    Kernel32Getfiletime = 89,
    Kernel32Setfilepointer = 90,
    Kernel32Getfilesize = 91,
    Kernel32Encodepointer = 92,
    Kernel32Decodepointer = 93,
    Kernel32Initializecriticalsectionandspincount = 94,
    User32Setprocessdpiaware = 95,
    User32Trackmouseevent = 96,
    Comctl32Dllgetversion = 97,
    Kernel32Readfile = 98,
    Kernel32Writefile = 99,
    User32Getcursorpos = 100,
    User32Getsystemmetrics = 101,
    User32Monitorfromwindow = 102,
    User32Getmonitorinfoa = 103,
    User32Getmonitorinfow = 104,
    User32Enumdisplaymonitors = 105,
    User32Enumdisplaydevicesa = 106,
    User32Enumdisplaydevicesw = 107,
    User32Monitorfrompoint = 108,
    Comctl32Ordinal17 = 111,
    User32Getwindowrect = 112,
    User32Getdpiforwindow = 113,
    User32Postmessagea = 114,
    User32Getsystemmetricsfordpi = 115,
    User32Adjustwindowrectexfordpi = 116,
    User32Setwindowpos = 117,
    User32Setscrollinfo = 118,
    User32Scrollwindowex = 119,
    User32Scrolldc = 120,
    User32Beginpaint = 121,
    User32Endpaint = 122,
    User32Clipcursor = 123,
    User32Getclipcursor = 124,
    User32Callmsgfiltera = 125,
    User32Callmsgfilterw = 126,
    User32Getdc = 127,
    User32Sendmessagea = 128,
    User32Sendmessagew = 129,
    Comdlg32Getopenfilenamea = 130,
    Comdlg32Getopenfilenamew = 131,
    Comdlg32Getsavefilenamea = 132,
    Comdlg32Getsavefilenamew = 133,
    Comdlg32Commdlgextendederror = 134,
    Comdlg32Choosecolora = 135,
    Gdi32Selectobject = 136,
    Gdi32Gettextextentpoint32a = 137,
    Gdi32Gettextextentpoint32w = 138,
    Gdi32Exttextoutw = 139,
    User32Releasedc = 140,
    Kernel32Getcurrentdirectoryw = 141,
    Kernel32Setcurrentdirectoryw = 142,
    User32Loadimagea = 143,
    User32Loadimagew = 144,
    Comctl32Initcommoncontrolsex = 145,
    UxthemeSetwindowtheme = 146,
    User32Setwindowlongptrw = 147,
    User32Getwindowlongptra = 148,
    User32Getwindowlongptrw = 149,
    Gdi32Getobjecta = 150,
    Comctl32ImagelistCreate = 151,
    Gdi32Createcompatibledc = 152,
    Gdi32Createdibsection = 153,
    Gdi32Createcompatiblebitmap = 154,
    Gdi32Getdevicecaps = 155,
    Gdi32Createfonta = 156,
    Gdi32Createfontw = 157,
    Gdi32Createfontindirecta = 158,
    Gdi32Gettextmetricsa = 159,
    Gdi32Settextcolor = 160,
    Gdi32Setbkcolor = 161,
    Gdi32Setbkmode = 162,
    Gdi32Textouta = 163,
    Gdi32Bitblt = 164,
    Gdi32Stretchblt = 165,
    Gdi32Patblt = 166,
    Gdi32Getpixel = 167,
    Gdi32Deletedc = 168,
    Comctl32ImagelistAddmasked = 169,
    Comctl32ImagelistSetbkcolor = 170,
    Comctl32ImagelistDestroy = 171,
    Gdi32Deleteobject = 172,
    User32Destroyicon = 173,
    User32Iswindow = 174,
    User32Iswindowvisible = 175,
    User32Iswindowenabled = 176,
    User32Getparent = 177,
    User32Getactivewindow = 178,
    User32Getforegroundwindow = 179,
    User32Showwindow = 180,
    User32Enablewindow = 181,
    User32Setforegroundwindow = 182,
    User32Setactivewindow = 183,
    User32Setfocus = 184,
    User32Getfocus = 185,
    User32Setcapture = 186,
    User32Getcapture = 187,
    User32Releasecapture = 188,
    User32Setcursor = 189,
    User32Updatewindow = 190,
    User32Invalidaterect = 191,
    User32Redrawwindow = 192,
    User32Setwindowtexta = 193,
    User32Setwindowtextw = 194,
    User32Getwindowtexta = 195,
    User32Getwindowtextw = 196,
    User32Getclientrect = 197,
    User32Movewindow = 198,
    User32Screentoclient = 199,
    User32Clienttoscreen = 200,
    User32Getdesktopwindow = 201,
    User32Getsyscolor = 202,
    User32Getsyscolorbrush = 203,
    User32Getdialogbaseunits = 204,
    User32Setrect = 205,
    User32Isiconic = 206,
    User32Iszoomed = 207,
    User32Getwindowthreadprocessid = 208,
    User32Getdlgctrlid = 209,
    Kernel32Getcurrentprocess = 210,
    Kernel32Sleep = 211,
    WinmmTimegettime = 212,
    Kernel32Localalloc = 213,
    Kernel32Localfree = 214,
    Kernel32Globalalloc = 215,
    Kernel32Globalfree = 216,
    Kernel32Globallock = 217,
    Kernel32Globalunlock = 218,
    Kernel32Globalsize = 219,
    Kernel32Muldiv = 220,
    User32Getcursor = 221,
    User32Ischild = 222,
    User32Getwindow = 223,
    User32Setkeyboardstate = 224,
    User32Getkeyboardstate = 225,
    User32Getkeystate = 226,
    User32Mapvirtualkeya = 227,
    User32Setwindowlongptra = 228,
    User32Settimer = 229,
    User32Killtimer = 230,
    User32Adjustwindowrectex = 231,
    Kernel32Globaladdatoma = 232,
    Kernel32Globaldeleteatom = 233,
    User32Setwindowshookexw = 234,
    User32Unhookwindowshookex = 235,
    User32Callnexthookex = 236,
    Kernel32Getfullpathnamew = 237,
    D3d9Direct3dcreate9 = 238,
    D3d9Idirect3d9Getadaptercount = 239,
    D3d9Idirect3d9Getadaptermonitor = 240,
    D3d9Idirect3d9Getdevicecaps = 241,
    D3d9Idirect3d9Getadapterdisplaymode = 242,
    D3d9Idirect3d9Createdevice = 243,
    D3d9Idirect3ddevice9Setvertexshader = 244,
    D3d9Idirect3ddevice9Setfvf = 245,
    D3d9Idirect3ddevice9Setrenderstate = 246,
    D3d9Idirect3ddevice9Settexturestagestate = 247,
    D3d9Idirect3ddevice9Setsamplerstate = 248,
    User32Enablemenuitem = 249,
    User32Checkmenuitem = 250,
    User32Getmessagea = 251,
    User32Translatemessage = 252,
    User32Defwindowproca = 253,
    User32Defwindowprocw = 254,
    User32Defframeproca = 255,
    User32Defframeprocw = 256,
    User32Defmdichildproca = 257,
    User32Defmdichildprocw = 258,
    User32Createmenu = 259,
    User32Createpopupmenu = 260,
    User32Appendmenua = 261,
    User32Appendmenuw = 262,
    User32Setmenu = 263,
    User32Destroymenu = 264,
    User32Removemenu = 265,
    User32Deletemenu = 266,
    User32Modifymenua = 267,
    User32Modifymenuw = 268,
    User32Getsystemmenu = 269,
    User32Trackpopupmenu = 270,
    User32Getmenuiteminfoa = 271,
    User32Getmenuiteminfow = 272,
    User32Setmenuiteminfoa = 273,
    User32Setmenuiteminfow = 274,
    User32Checkmenuradioitem = 275,
    User32Dispatchmessagea = 276,
    D3d9Idirect3ddevice9Release = 277,
    D3d9Idirect3d9Release = 278,
    Kernel32Getfullpathnamea = 279,
    Kernel32Getcurrentdirectorya = 280,
    Kernel32Setcurrentdirectorya = 281,
    Kernel32Createdirectoryw = 282,
    Kernel32Createdirectorya = 283,
    Kernel32Removefirectoryw = 284,
    Kernel32Removefirectorya = 285,
    Kernel32Deletefilew = 286,
    Kernel32Deletefilea = 287,
    Kernel32Movefilew = 288,
    Kernel32Movefilea = 289,
    Kernel32Gettemppathw = 290,
    Kernel32Gettemppatha = 291,
    Kernel32Gettempfilenamew = 292,
    Kernel32Gettempfilenamea = 293,
    Kernel32Getdrivetypew = 294,
    Kernel32Getdrivetypea = 295,
    Kernel32Getlogicaldrives = 296,
    Kernel32Getsystemdirectoryw = 297,
    Kernel32Getsystemdirectorya = 298,
    Kernel32Getwindowsdirectoryw = 299,
    Kernel32Getwindowsdirectorya = 300,
    Kernel32Getfilesizeex = 301,
    Kernel32Setfilepointerex = 302,
    Kernel32Setendoffile = 303,
    Kernel32Flushfilebuffers = 304,
    User32Getmenu = 305,
    Gdi32Getstockobject = 306,
    Kernel32Writeconsolew = 307,
    Kernel32Writeconsolea = 308,
    Kernel32Readconsolew = 309,
    Kernel32Readconsolea = 310,
    Kernel32Getconsolemode = 311,
    Kernel32Setconsolemode = 312,
    Kernel32Writeconsoleoutputw = 313,
    Kernel32Fillconsoleoutputcharacterw = 314,
    Kernel32Setconsolecursorposition = 315,
    Kernel32Setconsoletextattribute = 316,
    Kernel32Gettickcount64 = 317,
    Kernel32Getenvironmentvariablew = 318,
    Kernel32Setenvironmentvariablew = 319,
    User32Createwindowexa = 320,
    User32Createwindowexw = 321,
    User32Destroywindow = 322,
    User32Postquitmessage = 323,
    User32Getmessagew = 324,
    User32Peekmessagew = 325,
    User32Dispatchmessagew = 326,
    User32Postmessagew = 327,
    User32Registerclassa = 328,
    User32Registerclassw = 329,
    User32Unregisterclassa = 330,
    User32Unregisterclassw = 331,
    User32Validateirect = 332,
    User32Setwindowlonga = 333,
    User32Setwindowlongw = 334,
    User32Getwindowdc = 335,
    User32Getclassnamea = 336,
    User32Getclassnamew = 337,
    User32Getclasslongptra = 338,
    User32Getclasslongptrw = 339,
    User32Setclasslongptra = 340,
    User32Setclasslongptrw = 341,
    User32Drawmenubar = 342,
    User32Getmenustate = 343,
    Gdi32Createsolidbrush = 344,
    User32Fillrect = 345,
    Gdi32Createpen = 346,
    Gdi32Textoutw = 347,
    Gdi32Drawtexta = 348,
    Gdi32Drawtextw = 349,
    User32Createdialogparama = 350,
    User32Createdialogparamw = 351,
    User32Isdialogmessagea = 352,
    User32Isdialogmessagew = 353,
    User32Enddialog = 354,
    User32Getdlgitema = 355,
    User32Getdlgitemw = 356,
    User32Getdlgitemtexta = 357,
    User32Getdlgitemtextw = 358,
    User32Setdlgitemtexta = 359,
    User32Setdlgitemtextw = 360,
    User32Defdlgproca = 361,
    User32Defdlgprocw = 362,
    D3d9Idirect3ddevice9Present = 363,
    D3d9Idirect3ddevice9Beginscene = 364,
    D3d9Idirect3ddevice9Endscene = 365,
    D3d9Idirect3ddevice9Clear = 366,
    D3d9Idirect3ddevice9Settransform = 367,
    D3d9Idirect3ddevice9Setviewport = 368,
    D3d9Idirect3ddevice9Getviewport = 369,
    D3d9Idirect3ddevice9Drawprimitive = 370,
    D3d9Idirect3ddevice9Drawindexedprimitive = 371,
    D3d9Idirect3ddevice9Drawprimitiveup = 372,
    D3d9Idirect3ddevice9Drawindexedprimitiveup = 373,
    D3d9Idirect3ddevice9Setstreamsource = 374,
    D3d9Idirect3ddevice9Setindices = 375,
    D3d9Idirect3ddevice9Createvertexbuffer = 376,
    D3d9Idirect3ddevice9Createindexbuffer = 377,
    D3d9Idirect3ddevice9Createtexture = 378,
    D3d9Idirect3ddevice9Settexture = 379,
    D3d9Idirect3ddevice9Gettexture = 380,
    D3d9Idirect3ddevice9Gettexturestagestate = 381,
    D3d9Idirect3ddevice9Getsamplerstate = 382,
    D3d9Idirect3dtexture9Getlevelcount = 383,
    D3d9Idirect3dtexture9Getsurfacelevel = 384,
    D3d9Idirect3dtexture9Lockrect = 385,
    D3d9Idirect3dtexture9Unlockrect = 386,
    D3d9Idirect3dtexture9Release = 387,
    D3d9Idirect3dsurface9Lockrect = 388,
    D3d9Idirect3dsurface9Unlockrect = 389,
    D3d9Idirect3dsurface9Release = 390,
    D3d9Idirect3ddevice9Createdepthstencilsurface = 391,
    D3d9Idirect3ddevice9Setdepthstencilsurface = 392,
    D3d9Idirect3ddevice9Getdepthstencilsurface = 393,
    D3d9Idirect3ddevice9Getrenderstate = 394,
    D3d9Idirect3ddevice9Createvertexshader = 395,
    D3d9Idirect3ddevice9Getvertexshader = 396,
    D3d9Idirect3ddevice9Setvertexshaderconstantf = 397,
    D3d9Idirect3ddevice9Getvertexshaderconstantf = 398,
    D3d9Idirect3ddevice9Createpixelshader = 399,
    D3d9Idirect3ddevice9Setpixelshader = 400,
    D3d9Idirect3ddevice9Getpixelshader = 401,
    D3d9Idirect3ddevice9Setpixelshaderconstantf = 402,
    D3d9Idirect3ddevice9Getpixelshaderconstantf = 403,
    D3d9Idirect3dpixelshader9Release = 404,
    D3d9Idirect3dvertexshader9Release = 405,
    /// Appended here (not adjacent to Kernel32Getstartupinfoa) to avoid renumbering the entire enum.
    Kernel32Getstartupinfow = 406,
    /// Appended here (not adjacent to Kernel32Getuserdefaultlangid) to avoid renumbering the entire enum.
    Kernel32Getuserdefaultuilanguage = 407,
    /// Appended here (not adjacent to User32Getdlgitemw) to avoid renumbering the entire enum.
    User32Registerwindowmessagea = 408,
    /// Appended here (not adjacent to User32Registerwindowmessagea) to avoid renumbering the entire enum.
    User32Registerwindowmessagew = 409,
    /// Appended here (not adjacent to User32Registerwindowmessagew) to avoid renumbering the entire enum.
    User32Loadstringa = 410,
    /// Appended here (not adjacent to User32Loadstringa) to avoid renumbering the entire enum.
    User32Loadstringw = 411,
    /// Appended here (not adjacent to the Ex variants) to avoid renumbering the entire enum.
    Advapi32Regopenkeya = 412,
    /// Appended here (not adjacent to Advapi32Regopenkeya) to avoid renumbering the entire enum.
    Advapi32Regopenkeyw = 413,
    /// Appended here (not adjacent to Gdi32Createfontindirecta) to avoid renumbering the entire enum.
    Gdi32Createfontindirectw = 414,
    /// Appended here (not adjacent to User32Loadicona) to avoid renumbering the entire enum.
    User32Loadiconw = 415,
    /// Appended here (not adjacent to User32Loadcursora) to avoid renumbering the entire enum.
    User32Loadcursorw = 416,
    /// Appended here (no adjacent shell32 variants exist) to avoid renumbering the entire enum.
    Shell32Dragacceptfiles = 417,
    /// Appended here (not adjacent to Comctl32Imagelistdestroy) to avoid renumbering the entire enum.
    Comctl32Createstatuswindowa = 418,
    /// Appended here (not adjacent to Comctl32Createstatuswindowa) to avoid renumbering the entire enum.
    Comctl32Createstatuswindoww = 419,
    /// Appended here (not adjacent to Comdlg32Choosecolora) to avoid renumbering the entire enum.
    Comdlg32Getfiletitlea = 420,
    /// Appended here (not adjacent to Comdlg32Getfiletitlea) to avoid renumbering the entire enum.
    Comdlg32Getfiletitlew = 421,
    /// Appended here (not adjacent to User32Getwindowtextw) to avoid renumbering the entire enum.
    User32Getwindowtextlengtha = 422,
    /// Appended here (not adjacent to User32Getwindowtextlengtha) to avoid renumbering the entire enum.
    User32Getwindowtextlengthw = 423,
    /// Appended here (not adjacent to User32Getwindowtextlengthw) to avoid renumbering the entire enum.
    User32Getwindowplacement = 424,
    /// Appended here (not adjacent to User32Getwindowplacement) to avoid renumbering the entire enum.
    User32Setwindowplacement = 425,
    /// Appended here (not adjacent to User32Setwindowplacement) to avoid renumbering the entire enum.
    User32Loadacceleratorsa = 426,
    /// Appended here (not adjacent to User32Loadacceleratorsa) to avoid renumbering the entire enum.
    User32Loadacceleratorsw = 427,
    /// Appended here (not adjacent to User32Loadacceleratorsw) to avoid renumbering the entire enum.
    User32Translateacceleratora = 428,
    /// Appended here (not adjacent to User32Translateacceleratora) to avoid renumbering the entire enum.
    User32Translateacceleratorw = 429,
    /// Appended here (not adjacent to User32Translateacceleratorw) to avoid renumbering the entire enum.
    User32Destroyacceleratortable = 430,
    /// Appended here (not adjacent to User32Destroyacceleratortable) to avoid renumbering the entire enum.
    User32Loadmenua = 431,
    /// Appended here (not adjacent to User32Loadmenua) to avoid renumbering the entire enum.
    User32Loadmenuw = 432,
    /// Appended here (not adjacent to User32Loadmenuw) to avoid renumbering the entire enum.
    User32Isclipboardformatavailable = 433,
    /// Appended here (not adjacent to Shell32Dragacceptfiles) to avoid renumbering the entire enum.
    Shell32Dragqueryfilew = 434,
    /// Appended here (not adjacent to Shell32Dragqueryfilew) to avoid renumbering the entire enum.
    Shell32Dragqueryfilea = 435,
    /// Appended here (not adjacent to Shell32Dragqueryfilea) to avoid renumbering the entire enum.
    Shell32Dragquerypoint = 436,
    /// Appended here (not adjacent to Shell32Dragquerypoint) to avoid renumbering the entire enum.
    Shell32Dragfinish = 437,
    /// Appended here (not adjacent to Kernel32Localfree) to avoid renumbering the entire enum.
    Kernel32Locallock = 438,
    /// Appended here (not adjacent to Kernel32Locallock) to avoid renumbering the entire enum.
    Kernel32Localunlock = 439,
    /// Appended here (not adjacent to Kernel32Gettimezoneinformation) to avoid renumbering the entire enum.
    Kernel32Gettimeformatw = 440,
    /// Appended here (not adjacent to Kernel32Gettimeformatw) to avoid renumbering the entire enum.
    Kernel32Getdateformatw = 441,
    /// Appended here (not adjacent to User32Setdlgitemtextw) to avoid renumbering the entire enum.
    User32Getdlgitemint = 442,
    /// Appended here (not adjacent to User32Getdlgitemint) to avoid renumbering the entire enum.
    User32Setdlgitemint = 443,
    /// Appended here (not adjacent to User32Sendmessagew) to avoid renumbering the entire enum.
    User32Senddlgitemmessagew = 444,
    /// Appended here (not adjacent to Shell32Dragacceptfiles) to avoid renumbering the entire enum.
    Shell32Shellaboutw = 445,
    /// Appended here (not adjacent to Shell32Shellaboutw) to avoid renumbering the entire enum.
    Shell32Shellexecutew = 446,
    /// Appended here (not adjacent to Shell32Shellexecutew) to avoid renumbering the entire enum.
    User32Callwindowproca = 447,
    /// Appended here (not adjacent to User32Callwindowproca) to avoid renumbering the entire enum.
    User32Callwindowprocw = 448,
    /// Appended here (not adjacent to Comdlg32Getfiletitlew) to avoid renumbering the entire enum.
    Comdlg32Findtextw = 449,
    /// Appended here (not adjacent to Comdlg32Findtextw) to avoid renumbering the entire enum.
    Comdlg32Replacetextw = 450,
    /// Appended here (not adjacent to Comdlg32Replacetextw) to avoid renumbering the entire enum.
    Comdlg32Choosefontw = 451,
    /// Appended here (not adjacent to Comdlg32Choosefontw) to avoid renumbering the entire enum.
    Comdlg32Printdlgw = 452,
    /// Appended here (not adjacent to Comdlg32Printdlgw) to avoid renumbering the entire enum.
    Comdlg32Pagesetupdlgw = 453,
    /// Appended here (not adjacent to Comdlg32Pagesetupdlgw) to avoid renumbering the entire enum.
    Gdi32Startdocw = 454,
    /// Appended here (not adjacent to Gdi32Startdocw) to avoid renumbering the entire enum.
    Gdi32Startpage = 455,
    /// Appended here (not adjacent to Gdi32Startpage) to avoid renumbering the entire enum.
    Gdi32Endpage = 456,
    /// Appended here (not adjacent to Gdi32Endpage) to avoid renumbering the entire enum.
    Gdi32Enddoc = 457,
    /// Appended here (not adjacent to Gdi32Enddoc) to avoid renumbering the entire enum.
    Gdi32Abortdoc = 458,
    /// Appended here (not adjacent to Gdi32Abortdoc) to avoid renumbering the entire enum.
    Gdi32Createdcw = 459,
    /// Appended here (not adjacent to Gdi32Createdcw) to avoid renumbering the entire enum.
    Gdi32Gettextmetricsw = 460,
    /// Appended here (not adjacent to Gdi32Gettextmetricsw) to avoid renumbering the entire enum.
    Gdi32Setmapmode = 461,
    /// Appended here (not adjacent to Gdi32Setmapmode) to avoid renumbering the entire enum.
    Gdi32Rectangle = 462,
    /// Appended here (not adjacent to Gdi32Rectangle) to avoid renumbering the entire enum.
    User32Inflaterect = 463,
    /// Appended here (not adjacent to the other D3D9 ids) to avoid renumbering the entire enum.
    D3d9Idirect3dvertexbuffer9Queryinterface = 464,
    /// Appended here (not adjacent to the other D3D9 ids) to avoid renumbering the entire enum.
    D3d9Idirect3dvertexbuffer9Addref = 465,
    /// Appended here (not adjacent to the other D3D9 ids) to avoid renumbering the entire enum.
    D3d9Idirect3dvertexbuffer9Release = 466,
    /// Appended here (not adjacent to the other D3D9 ids) to avoid renumbering the entire enum.
    D3d9Idirect3dvertexbuffer9Lock = 467,
    /// Appended here (not adjacent to the other D3D9 ids) to avoid renumbering the entire enum.
    D3d9Idirect3dvertexbuffer9Unlock = 468,
    /// Appended here (not adjacent to the other D3D9 ids) to avoid renumbering the entire enum.
    D3d9Idirect3dvertexbuffer9Getdesc = 469,
    /// Appended here (not adjacent to the other D3D9 ids) to avoid renumbering the entire enum.
    D3d9Idirect3dindexbuffer9Queryinterface = 470,
    /// Appended here (not adjacent to the other D3D9 ids) to avoid renumbering the entire enum.
    D3d9Idirect3dindexbuffer9Addref = 471,
    /// Appended here (not adjacent to the other D3D9 ids) to avoid renumbering the entire enum.
    D3d9Idirect3dindexbuffer9Release = 472,
    /// Appended here (not adjacent to the other D3D9 ids) to avoid renumbering the entire enum.
    D3d9Idirect3dindexbuffer9Lock = 473,
    /// Appended here (not adjacent to the other D3D9 ids) to avoid renumbering the entire enum.
    D3d9Idirect3dindexbuffer9Unlock = 474,
    /// Appended here (not adjacent to the other D3D9 ids) to avoid renumbering the entire enum.
    D3d9Idirect3dindexbuffer9Getdesc = 475,
    /// Appended here (not adjacent to the other D3D9 ids) to avoid renumbering the entire enum.
    D3d9Idirect3ddevice9Getstreamsource = 476,
    /// Appended here (not adjacent to the other D3D9 ids) to avoid renumbering the entire enum.
    D3d9Idirect3ddevice9Getindices = 477,
    /// Appended here (not adjacent to the other DLL ids) to avoid renumbering the entire enum.
    VersionGetfileversioninfosizew = 478,
    /// Appended here (not adjacent to the other DLL ids) to avoid renumbering the entire enum.
    VersionGetfileversioninfosizea = 479,
    /// Appended here (not adjacent to the other DLL ids) to avoid renumbering the entire enum.
    VersionGetfileversioninfosizeexw = 480,
    /// Appended here (not adjacent to the other DLL ids) to avoid renumbering the entire enum.
    VersionGetfileversioninfosizeexa = 481,
    /// Appended here (not adjacent to the other DLL ids) to avoid renumbering the entire enum.
    VersionGetfileversioninfow = 482,
    /// Appended here (not adjacent to the other DLL ids) to avoid renumbering the entire enum.
    VersionGetfileversioninfoa = 483,
    /// Appended here (not adjacent to the other DLL ids) to avoid renumbering the entire enum.
    VersionGetfileversioninfoexw = 484,
    /// Appended here (not adjacent to the other DLL ids) to avoid renumbering the entire enum.
    VersionGetfileversioninfoexa = 485,
    /// Appended here (not adjacent to the other DLL ids) to avoid renumbering the entire enum.
    VersionVerqueryvaluew = 486,
    /// Appended here (not adjacent to the other DLL ids) to avoid renumbering the entire enum.
    VersionVerqueryvaluea = 487,
    /// Appended here (not adjacent to the other DLL ids) to avoid renumbering the entire enum.
    VersionGetfileversioninfobyhandlew = 488,
    /// Appended here (not adjacent to the other DLL ids) to avoid renumbering the entire enum.
    VersionGetfileversioninfobyhandlea = 489,
    /// Appended here (not adjacent to the other DLL ids) to avoid renumbering the entire enum.
    VersionVerlanguagenamew = 490,
    /// Appended here (not adjacent to the other DLL ids) to avoid renumbering the entire enum.
    VersionVerlanguagenamea = 491,
    /// Appended here (not adjacent to the other DLL ids) to avoid renumbering the entire enum.
    VersionVerfindfilew = 492,
    /// Appended here (not adjacent to the other DLL ids) to avoid renumbering the entire enum.
    VersionVerfindfilea = 493,
    /// Appended here (not adjacent to the other DLL ids) to avoid renumbering the entire enum.
    VersionVerinstallfilew = 494,
    /// Appended here (not adjacent to the other DLL ids) to avoid renumbering the entire enum.
    VersionVerinstallfilea = 495,
    /// Appended here (not adjacent to the other D3D9 ids) to avoid renumbering the entire enum.
    D3d9Idirect3ddevice9Gettransform = 496,
    /// Appended here (not adjacent to the other D3D9 ids) to avoid renumbering the entire enum.
    D3d9Idirect3ddevice9Multiplytransform = 497,
    /// Appended here (not adjacent to the other D3D9 ids) to avoid renumbering the entire enum.
    D3d9Idirect3ddevice9Setscissorrect = 498,
    /// Appended here (not adjacent to the other D3D9 ids) to avoid renumbering the entire enum.
    D3d9Idirect3ddevice9Setvertexshaderconstanti = 499,
    /// Appended here (not adjacent to the other D3D9 ids) to avoid renumbering the entire enum.
    D3d9Idirect3ddevice9Getvertexshaderconstanti = 500,
    /// Appended here (not adjacent to the other D3D9 ids) to avoid renumbering the entire enum.
    D3d9Idirect3ddevice9Setvertexshaderconstantb = 501,
    /// Appended here (not adjacent to the other D3D9 ids) to avoid renumbering the entire enum.
    D3d9Idirect3ddevice9Getvertexshaderconstantb = 502,
    /// Appended here (not adjacent to the other D3D9 ids) to avoid renumbering the entire enum.
    D3d9Idirect3ddevice9Createrendertarget = 503,
    /// Appended here (not adjacent to the other D3D9 ids) to avoid renumbering the entire enum.
    D3d9Idirect3ddevice9Setrendertarget = 504,
    /// Appended here (not adjacent to the other D3D9 ids) to avoid renumbering the entire enum.
    D3d9Idirect3ddevice9Getrendertarget = 505,
    /// Appended here (not adjacent to the other D3D9 ids) to avoid renumbering the entire enum.
    D3d9Idirect3dsurface9Getdesc = 506,
}

/// Number of dense [`WinApiId`] discriminants, i.e. one past the highest one.
///
/// This is the size every discriminant-indexed array (`WINAPI_TRAITS`) needs,
/// and it is NOT the same as strum's variant count: the enum has two
/// discriminant holes (109/110, from APIs removed before the appends began),
/// so `WinApiId::COUNT` is lower. Deriving from the final variant keeps the
/// alias correct by construction: appending a variant with a higher
/// discriminant updates this constant with it.
pub const WINAPI_ID_COUNT: usize = WinApiId::D3d9Idirect3dsurface9Getdesc.to_u16() as usize + 1;

impl WinApiId {
    /// Discriminant as `u16` (`#[repr(u16)]`).
    #[must_use]
    #[allow(unsafe_code)]
    pub const fn to_u16(self) -> u16 {
        // SAFETY: `#[repr(u16)]` guarantees the discriminant is exactly a u16
        // value, and every variant is valid for transmute.
        unsafe { core::mem::transmute::<Self, u16>(self) }
    }

    /// Reconstruct from the discriminant.
    ///
    /// Delegates to strum's generated `from_repr` match. This is strictly
    /// tighter than the old transmute: the two discriminant holes (109/110)
    /// now return `None` instead of fabricating a value that has no variant —
    /// previously latent UB that no name resolution could actually reach.
    #[must_use]
    pub fn from_u16(raw: u16) -> Option<Self> {
        WinApiId::from_repr(raw)
    }
}

/// Hot-path dispatch: dense `u16` match over the `WinApiId` discriminant
/// (LLVM jump table) with no string comparison — runs once per host API stop.
pub fn dispatch_winapi_id(
    ctx: &mut HandlerContext<'_>,
    id: WinApiId,
) -> Result<WinApiHandlerResult> {
    match id {
        WinApiId::Kernel32Getversionexa => kernel32::handle_get_version_ex_a(ctx),
        WinApiId::Kernel32Getmodulehandlea => kernel32::handle_get_module_handle_a(ctx),
        WinApiId::Kernel32Getcommandlinea => kernel32::handle_get_command_line_a(ctx),
        WinApiId::Kernel32Getcommandlinew => kernel32::handle_get_command_line_w(ctx),
        WinApiId::Kernel32Getstartupinfoa => kernel32::handle_get_startup_info_a(ctx),
        WinApiId::Kernel32Getstartupinfow => kernel32::handle_get_startup_info_w(ctx),
        WinApiId::Kernel32Getuserdefaultuilanguage => {
            kernel32::handle_get_user_default_ui_language(ctx)
        }
        WinApiId::Kernel32Getprocessheap => kernel32::handle_get_process_heap(ctx),
        WinApiId::Kernel32Getsystemtimeasfiletime => {
            kernel32::handle_get_system_time_as_file_time(ctx)
        }
        WinApiId::Kernel32Getcurrentprocessid => kernel32::handle_get_current_process_id(ctx),
        WinApiId::Kernel32Getcurrentthreadid => kernel32::handle_get_current_thread_id(ctx),
        WinApiId::Kernel32Gettickcount => kernel32::handle_get_tick_count(ctx),
        WinApiId::Kernel32Queryperformancecounter => {
            kernel32::handle_query_performance_counter(ctx)
        }
        WinApiId::Kernel32Heapalloc => kernel32::handle_heap_alloc(ctx),
        WinApiId::Kernel32Heapfree => kernel32::handle_heap_free(ctx),
        WinApiId::Kernel32Heaprealloc => kernel32::handle_heap_realloc(ctx),
        WinApiId::Kernel32Heapcreate => kernel32::handle_heap_create(ctx),
        WinApiId::Kernel32Heapsetinformation => kernel32::handle_heap_set_information(ctx),
        WinApiId::Kernel32Initializecriticalsection => {
            kernel32::handle_initialize_critical_section(ctx)
        }
        WinApiId::Kernel32Entercriticalsection => kernel32::handle_enter_critical_section(ctx),
        WinApiId::Kernel32Leavecriticalsection => kernel32::handle_leave_critical_section(ctx),
        WinApiId::Kernel32Deletecriticalsection => kernel32::handle_delete_critical_section(ctx),
        WinApiId::Kernel32Flsalloc => kernel32::handle_fls_alloc(ctx),
        WinApiId::Kernel32Flsfree => kernel32::handle_fls_free(ctx),
        WinApiId::Kernel32Flssetvalue => kernel32::handle_fls_set_value(ctx),
        WinApiId::Kernel32Flsgetvalue => kernel32::handle_fls_get_value(ctx),
        WinApiId::Kernel32Getstdhandle => kernel32::handle_get_std_handle(ctx),
        WinApiId::Kernel32Getfiletype => kernel32::handle_get_file_type(ctx),
        WinApiId::Kernel32Sethandlecount => kernel32::handle_set_handle_count(ctx),
        WinApiId::Kernel32Getenvironmentstringsw => kernel32::handle_get_environment_strings_w(ctx),
        WinApiId::Kernel32Freeenvironmentstringsw => {
            kernel32::handle_free_environment_strings_w(ctx)
        }
        WinApiId::Kernel32Widechartomultibyte => kernel32::handle_wide_char_to_multi_byte(ctx),
        WinApiId::Kernel32Getlasterror => kernel32::handle_get_last_error(ctx),
        WinApiId::Kernel32Setlasterror => kernel32::handle_set_last_error(ctx),
        WinApiId::Kernel32Getacp => kernel32::handle_get_acp(ctx),
        WinApiId::Kernel32Getoemcp => kernel32::handle_get_oem_cp(ctx),
        WinApiId::Kernel32Getcpinfo => kernel32::handle_get_cp_info(ctx),
        WinApiId::Kernel32Isvalidcodepage => kernel32::handle_is_valid_code_page(ctx),
        WinApiId::Kernel32Getstringtypew => kernel32::handle_get_string_type_w(ctx),
        WinApiId::Kernel32Multibytetowidechar => kernel32::handle_multi_byte_to_wide_char(ctx),
        WinApiId::Kernel32Lcmapstringw => kernel32::handle_lc_map_string_w(ctx),
        WinApiId::Kernel32Getmodulefilenamea => kernel32::handle_get_module_file_name_a(ctx),
        WinApiId::Kernel32Getmodulefilenamew => kernel32::handle_get_module_file_name_w(ctx),
        WinApiId::Kernel32Setunhandledexceptionfilter => {
            kernel32::handle_set_unhandled_exception_filter(ctx)
        }
        WinApiId::Kernel32Heapsize => kernel32::handle_heap_size(ctx),
        WinApiId::Advapi32Regcreatekeyexa => advapi32::handle_reg_create_key_ex_a(ctx),
        WinApiId::Advapi32Regopenkeyexa => advapi32::handle_reg_open_key_ex_a(ctx),
        WinApiId::Advapi32Regqueryvalueexa => advapi32::handle_reg_query_value_ex_a(ctx),
        WinApiId::Advapi32Regqueryvalueexw => advapi32::handle_reg_query_value_ex_w(ctx),
        WinApiId::Advapi32Regsetvalueexa => advapi32::handle_reg_set_value_ex_a(ctx),
        WinApiId::Advapi32Regsetvalueexw => advapi32::handle_reg_set_value_ex_w(ctx),
        WinApiId::Advapi32Regdeletevaluea => advapi32::handle_reg_delete_value_a(ctx),
        WinApiId::Advapi32Regclosekey => advapi32::handle_reg_close_key(ctx),
        WinApiId::Advapi32Initializesecuritydescriptor => {
            advapi32::handle_initialize_security_descriptor(ctx)
        }
        WinApiId::Advapi32Setsecuritydescriptordacl => {
            advapi32::handle_set_security_descriptor_dacl(ctx)
        }
        WinApiId::Kernel32Loadlibrarya => kernel32::handle_load_library_a(ctx),
        WinApiId::Kernel32Loadlibraryw => kernel32::handle_load_library_w(ctx),
        WinApiId::Kernel32Freelibrary => kernel32::handle_free_library(ctx),
        WinApiId::Kernel32Getprocaddress => kernel32::handle_get_proc_address(ctx),
        WinApiId::Kernel32Getfileattributesa => kernel32::handle_get_file_attributes_a(ctx),
        WinApiId::Kernel32Getfileattributesw => kernel32::handle_get_file_attributes_w(ctx),
        WinApiId::Kernel32Findfirstfilew => kernel32::handle_find_first_file_w(ctx),
        WinApiId::Kernel32Findfirstfilea => kernel32::handle_find_first_file_a(ctx),
        WinApiId::Kernel32Findnextfilew => kernel32::handle_find_next_file_w(ctx),
        WinApiId::Kernel32Findnextfilea => kernel32::handle_find_next_file_a(ctx),
        WinApiId::Kernel32Findclose => kernel32::handle_find_close(ctx),
        WinApiId::User32Getasynckeystate => user32::handle_get_async_key_state(ctx),
        WinApiId::User32Peekmessagea => user32::handle_peek_message_a(ctx),
        WinApiId::Kernel32Loadlibraryexa => kernel32::handle_load_library_ex_a(ctx),
        WinApiId::Kernel32Loadlibraryexw => kernel32::handle_load_library_ex_w(ctx),
        WinApiId::Kernel32Findresourcea => kernel32::handle_find_resource_a(ctx),
        WinApiId::Kernel32Loadresource => kernel32::handle_load_resource(ctx),
        WinApiId::Kernel32Lockresource => kernel32::handle_lock_resource(ctx),
        WinApiId::Kernel32Sizeofresource => kernel32::handle_sizeof_resource(ctx),
        WinApiId::Kernel32Getsystemdefaultlangid => {
            kernel32::handle_get_system_default_lang_id(ctx)
        }
        WinApiId::Kernel32Getuserdefaultlangid => kernel32::handle_get_user_default_lang_id(ctx),
        WinApiId::Kernel32Globalmemorystatus => kernel32::handle_global_memory_status(ctx),
        WinApiId::Kernel32Getlocaltime => kernel32::handle_get_local_time(ctx),
        WinApiId::User32Loadicona => user32::handle_load_icon_a(ctx),
        WinApiId::User32Loadcursora => user32::handle_load_cursor_a(ctx),
        WinApiId::User32Registerclassexw => user32::handle_register_class_ex_w(ctx),
        WinApiId::User32Registerclassexa => user32::handle_register_class_ex_a(ctx),
        WinApiId::Kernel32Createfilew => kernel32::handle_create_file_w(ctx),
        WinApiId::Kernel32Createfilea => kernel32::handle_create_file_a(ctx),
        WinApiId::Kernel32Closehandle => kernel32::handle_close_handle(ctx),
        WinApiId::User32Messageboxw => user32::handle_message_box_w(ctx),
        WinApiId::User32Messageboxa => user32::handle_message_box_a(ctx),
        WinApiId::Kernel32Getfileinformationbyhandle => {
            kernel32::handle_get_file_information_by_handle(ctx)
        }
        WinApiId::Kernel32Filetimetolocalfiletime => {
            kernel32::handle_file_time_to_local_file_time(ctx)
        }
        WinApiId::Kernel32Filetimetosystemtime => kernel32::handle_file_time_to_system_time(ctx),
        WinApiId::Kernel32Gettimezoneinformation => kernel32::handle_get_time_zone_information(ctx),
        WinApiId::Kernel32Getfiletime => kernel32::handle_get_file_time(ctx),
        WinApiId::Kernel32Setfilepointer => kernel32::handle_set_file_pointer(ctx),
        WinApiId::Kernel32Getfilesize => kernel32::handle_get_file_size(ctx),
        WinApiId::Kernel32Encodepointer => kernel32::handle_encode_pointer(ctx),
        WinApiId::Kernel32Decodepointer => kernel32::handle_decode_pointer(ctx),
        WinApiId::Kernel32Initializecriticalsectionandspincount => {
            kernel32::handle_initialize_critical_section_and_spin_count(ctx)
        }
        WinApiId::User32Setprocessdpiaware => user32::handle_set_process_dpi_aware(ctx),
        WinApiId::User32Trackmouseevent => user32::handle_track_mouse_event(ctx),
        WinApiId::Comctl32Dllgetversion => comctl32::handle_dll_get_version(ctx),
        WinApiId::Kernel32Readfile => kernel32::handle_read_file(ctx),
        WinApiId::Kernel32Writefile => kernel32::handle_write_file(ctx),
        WinApiId::User32Getcursorpos => user32::handle_get_cursor_pos(ctx),
        WinApiId::User32Getsystemmetrics => user32::handle_get_system_metrics(ctx),
        WinApiId::User32Monitorfromwindow => user32::handle_monitor_from_window(ctx),
        WinApiId::User32Getmonitorinfoa => user32::handle_get_monitor_info_a(ctx),
        WinApiId::User32Getmonitorinfow => user32::handle_get_monitor_info_w(ctx),
        WinApiId::User32Enumdisplaymonitors => user32::handle_enum_display_monitors(ctx),
        WinApiId::User32Enumdisplaydevicesa => user32::handle_enum_display_devices_a(ctx),
        WinApiId::User32Enumdisplaydevicesw => user32::handle_enum_display_devices_w(ctx),
        WinApiId::User32Monitorfrompoint => user32::handle_monitor_from_point(ctx),
        WinApiId::Comctl32Ordinal17 => comctl32::handle_init_common_controls(ctx),
        WinApiId::User32Getwindowrect => user32::handle_get_window_rect(ctx),
        WinApiId::User32Getdpiforwindow => user32::handle_get_dpi_for_window(ctx),
        WinApiId::User32Postmessagea => user32::handle_post_message_a(ctx),
        WinApiId::User32Getsystemmetricsfordpi => user32::handle_get_system_metrics_for_dpi(ctx),
        WinApiId::User32Adjustwindowrectexfordpi => {
            user32::handle_adjust_window_rect_ex_for_dpi(ctx)
        }
        WinApiId::User32Setwindowpos => user32::handle_set_window_pos(ctx),
        WinApiId::User32Setscrollinfo => user32::handle_set_scroll_info(ctx),
        WinApiId::User32Scrollwindowex => user32::handle_scroll_window_ex(ctx),
        WinApiId::User32Scrolldc => user32::handle_scroll_dc(ctx),
        WinApiId::User32Beginpaint => user32::handle_begin_paint(ctx),
        WinApiId::User32Endpaint => user32::handle_end_paint(ctx),
        WinApiId::User32Clipcursor => user32::handle_clip_cursor(ctx),
        WinApiId::User32Getclipcursor => user32::handle_get_clip_cursor(ctx),
        WinApiId::User32Callmsgfiltera => user32::handle_call_msg_filter(ctx, "CallMsgFilterA"),
        WinApiId::User32Callmsgfilterw => user32::handle_call_msg_filter(ctx, "CallMsgFilterW"),
        WinApiId::User32Getdc => user32::handle_get_dc(ctx),
        WinApiId::User32Sendmessagea => user32::handle_send_message_a(ctx),
        WinApiId::User32Sendmessagew => user32::handle_send_message_w(ctx),
        WinApiId::Comdlg32Getopenfilenamea => comdlg32::handle_get_open_file_name_a(ctx),
        WinApiId::Comdlg32Getopenfilenamew => comdlg32::handle_get_open_file_name_w(ctx),
        WinApiId::Comdlg32Getsavefilenamea => comdlg32::handle_get_save_file_name_a(ctx),
        WinApiId::Comdlg32Getsavefilenamew => comdlg32::handle_get_save_file_name_w(ctx),
        WinApiId::Comdlg32Commdlgextendederror => comdlg32::handle_comm_dlg_extended_error(ctx),
        WinApiId::Comdlg32Choosecolora => comdlg32::handle_choose_color_a(ctx),
        WinApiId::Comdlg32Choosefontw => comdlg32::handle_choose_font_w(ctx),
        WinApiId::Comdlg32Printdlgw => comdlg32::handle_print_dlg_w(ctx),
        WinApiId::Comdlg32Pagesetupdlgw => comdlg32::handle_page_setup_dlg_w(ctx),
        WinApiId::Gdi32Selectobject => gdi32::handle_select_object(ctx),
        WinApiId::Gdi32Gettextextentpoint32a => gdi32::handle_get_text_extent_point_32_a(ctx),
        WinApiId::Gdi32Gettextextentpoint32w => gdi32::handle_get_text_extent_point_32_w(ctx),
        WinApiId::Gdi32Exttextoutw => gdi32::handle_ext_text_out_w(ctx),
        WinApiId::User32Releasedc => user32::handle_release_dc(ctx),
        WinApiId::Kernel32Getcurrentdirectoryw => kernel32::handle_get_current_directory_w(ctx),
        WinApiId::Kernel32Setcurrentdirectoryw => kernel32::handle_set_current_directory_w(ctx),
        WinApiId::User32Loadimagea => user32::handle_load_image_a(ctx),
        WinApiId::User32Loadimagew => user32::handle_load_image_w(ctx),
        WinApiId::Comctl32Initcommoncontrolsex => comctl32::handle_init_common_controls_ex(ctx),
        WinApiId::UxthemeSetwindowtheme => uxtheme::handle_set_window_theme(ctx),
        WinApiId::User32Setwindowlongptrw => user32::handle_set_window_long_ptr_w(ctx),
        WinApiId::User32Getwindowlongptra => user32::handle_get_window_long_ptr_a(ctx),
        WinApiId::User32Getwindowlongptrw => user32::handle_get_window_long_ptr_w(ctx),
        WinApiId::Gdi32Getobjecta => gdi32::handle_get_object_a(ctx),
        WinApiId::Comctl32ImagelistCreate => comctl32::handle_image_list_create(ctx),
        WinApiId::Gdi32Createcompatibledc => gdi32::handle_create_compatible_dc(ctx),
        WinApiId::Gdi32Createdibsection => gdi32::handle_create_dib_section(ctx),
        WinApiId::Gdi32Createcompatiblebitmap => gdi32::handle_create_compatible_bitmap(ctx),
        WinApiId::Gdi32Getdevicecaps => gdi32::handle_get_device_caps(ctx),
        WinApiId::Gdi32Createfonta => gdi32::handle_create_font_a(ctx),
        WinApiId::Gdi32Createfontw => gdi32::handle_create_font_w(ctx),
        WinApiId::Gdi32Createfontindirecta => gdi32::handle_create_font_indirect_a(ctx),
        WinApiId::Gdi32Gettextmetricsa => gdi32::handle_get_text_metrics_a(ctx),
        WinApiId::Gdi32Settextcolor => gdi32::handle_set_text_color(ctx),
        WinApiId::Gdi32Setbkcolor => gdi32::handle_set_bk_color(ctx),
        WinApiId::Gdi32Setbkmode => gdi32::handle_set_bk_mode(ctx),
        WinApiId::Gdi32Textouta => gdi32::handle_text_out_a(ctx),
        WinApiId::Gdi32Bitblt => gdi32::handle_bit_blt(ctx),
        WinApiId::Gdi32Stretchblt => gdi32::handle_stretch_blt(ctx),
        WinApiId::Gdi32Patblt => gdi32::handle_pat_blt(ctx),
        WinApiId::Gdi32Getpixel => gdi32::handle_get_pixel(ctx),
        WinApiId::Gdi32Deletedc => gdi32::handle_delete_dc(ctx),
        WinApiId::Comctl32ImagelistAddmasked => comctl32::handle_image_list_add_masked(ctx),
        WinApiId::Comctl32ImagelistSetbkcolor => comctl32::handle_image_list_set_bk_color(ctx),
        WinApiId::Comctl32ImagelistDestroy => comctl32::handle_image_list_destroy(ctx),
        WinApiId::Gdi32Deleteobject => gdi32::handle_delete_object(ctx),
        WinApiId::User32Destroyicon => user32::handle_destroy_icon(ctx),
        WinApiId::User32Iswindow => user32::handle_is_window(ctx),
        WinApiId::User32Iswindowvisible => user32::handle_is_window_visible(ctx),
        WinApiId::User32Iswindowenabled => user32::handle_is_window_enabled(ctx),
        WinApiId::User32Getparent => user32::handle_get_parent(ctx),
        WinApiId::User32Getactivewindow => user32::handle_get_active_window(ctx),
        WinApiId::User32Getforegroundwindow => user32::handle_get_foreground_window(ctx),
        WinApiId::User32Showwindow => user32::handle_show_window(ctx),
        WinApiId::User32Enablewindow => user32::handle_enable_window(ctx),
        WinApiId::User32Setforegroundwindow => user32::handle_set_foreground_window(ctx),
        WinApiId::User32Setactivewindow => user32::handle_set_active_window(ctx),
        WinApiId::User32Setfocus => user32::handle_set_focus(ctx),
        WinApiId::User32Getfocus => user32::handle_get_focus(ctx),
        WinApiId::User32Setcapture => user32::handle_set_capture(ctx),
        WinApiId::User32Getcapture => user32::handle_get_capture(ctx),
        WinApiId::User32Releasecapture => user32::handle_release_capture(ctx),
        WinApiId::User32Setcursor => user32::handle_set_cursor(ctx),
        WinApiId::User32Updatewindow => user32::handle_update_window(ctx),
        WinApiId::User32Invalidaterect => user32::handle_invalidate_rect(ctx),
        WinApiId::User32Redrawwindow => user32::handle_redraw_window(ctx),
        WinApiId::User32Setwindowtexta => user32::handle_set_window_text_a(ctx),
        WinApiId::User32Setwindowtextw => user32::handle_set_window_text_w(ctx),
        WinApiId::User32Getwindowtexta => user32::handle_get_window_text_a(ctx),
        WinApiId::User32Getwindowtextw => user32::handle_get_window_text_w(ctx),
        WinApiId::User32Getclientrect => user32::handle_get_client_rect(ctx),
        WinApiId::User32Movewindow => user32::handle_move_window(ctx),
        WinApiId::User32Screentoclient => user32::handle_screen_to_client(ctx),
        WinApiId::User32Clienttoscreen => user32::handle_client_to_screen(ctx),
        WinApiId::User32Getdesktopwindow => user32::handle_get_desktop_window(ctx),
        WinApiId::User32Getsyscolor => user32::handle_get_sys_color(ctx),
        WinApiId::User32Getsyscolorbrush => user32::handle_get_sys_color_brush(ctx),
        WinApiId::User32Getdialogbaseunits => user32::handle_get_dialog_base_units(ctx),
        WinApiId::User32Setrect => user32::handle_set_rect(ctx),
        WinApiId::User32Isiconic => user32::handle_is_iconic(ctx),
        WinApiId::User32Iszoomed => user32::handle_is_zoomed(ctx),
        WinApiId::User32Getwindowthreadprocessid => {
            user32::handle_get_window_thread_process_id(ctx)
        }
        WinApiId::User32Getdlgctrlid => user32::handle_get_dlg_ctrl_id(ctx),
        WinApiId::Kernel32Getcurrentprocess => kernel32::handle_get_current_process(ctx),
        WinApiId::Kernel32Sleep => kernel32::handle_sleep(ctx),
        WinApiId::WinmmTimegettime => winmm::handle_time_get_time(ctx),
        WinApiId::Kernel32Localalloc => kernel32::handle_local_alloc(ctx),
        WinApiId::Kernel32Localfree => kernel32::handle_local_free(ctx),
        WinApiId::Kernel32Globalalloc => kernel32::handle_global_alloc(ctx),
        WinApiId::Kernel32Globalfree => kernel32::handle_global_free(ctx),
        WinApiId::Kernel32Globallock => kernel32::handle_global_lock(ctx),
        WinApiId::Kernel32Globalunlock => kernel32::handle_global_unlock(ctx),
        WinApiId::Kernel32Globalsize => kernel32::handle_global_size(ctx),
        WinApiId::Kernel32Muldiv => kernel32::handle_mul_div(ctx),
        WinApiId::User32Getcursor => user32::handle_get_cursor(ctx),
        WinApiId::User32Ischild => user32::handle_is_child(ctx),
        WinApiId::User32Getwindow => user32::handle_get_window(ctx),
        WinApiId::User32Setkeyboardstate => user32::handle_set_keyboard_state(ctx),
        WinApiId::User32Getkeyboardstate => user32::handle_get_keyboard_state(ctx),
        WinApiId::User32Getkeystate => user32::handle_get_key_state(ctx),
        WinApiId::User32Mapvirtualkeya => user32::handle_map_virtual_key_a(ctx),
        WinApiId::User32Setwindowlongptra => user32::handle_set_window_long_ptr_a(ctx),
        WinApiId::User32Settimer => user32::handle_set_timer(ctx),
        WinApiId::User32Killtimer => user32::handle_kill_timer(ctx),
        WinApiId::User32Adjustwindowrectex => user32::handle_adjust_window_rect_ex(ctx),
        WinApiId::Kernel32Globaladdatoma => kernel32::handle_global_add_atom_a(ctx),
        WinApiId::Kernel32Globaldeleteatom => kernel32::handle_global_delete_atom(ctx),
        WinApiId::User32Setwindowshookexw => user32::handle_set_windows_hook_ex_w(ctx),
        WinApiId::User32Unhookwindowshookex => user32::handle_unhook_windows_hook_ex(ctx),
        WinApiId::User32Callnexthookex => user32::handle_call_next_hook_ex(ctx),
        WinApiId::Kernel32Getfullpathnamew => kernel32::handle_get_full_path_name_w(ctx),
        WinApiId::Kernel32Getfullpathnamea => kernel32::handle_get_full_path_name_a(ctx),
        WinApiId::Kernel32Getcurrentdirectorya => kernel32::handle_get_current_directory_a(ctx),
        WinApiId::Kernel32Setcurrentdirectorya => kernel32::handle_set_current_directory_a(ctx),
        WinApiId::Kernel32Createdirectoryw => kernel32::handle_create_directory_w(ctx),
        WinApiId::Kernel32Createdirectorya => kernel32::handle_create_directory_a(ctx),
        WinApiId::Kernel32Removefirectoryw => kernel32::handle_remove_directory_w(ctx),
        WinApiId::Kernel32Removefirectorya => kernel32::handle_remove_directory_a(ctx),
        WinApiId::Kernel32Deletefilew => kernel32::handle_delete_file_w(ctx),
        WinApiId::Kernel32Deletefilea => kernel32::handle_delete_file_a(ctx),
        WinApiId::Kernel32Movefilew => kernel32::handle_move_file_w(ctx),
        WinApiId::Kernel32Movefilea => kernel32::handle_move_file_a(ctx),
        WinApiId::Kernel32Gettemppathw => kernel32::handle_get_temp_path_w(ctx),
        WinApiId::Kernel32Gettemppatha => kernel32::handle_get_temp_path_a(ctx),
        WinApiId::Kernel32Gettempfilenamew => kernel32::handle_get_temp_file_name_w(ctx),
        WinApiId::Kernel32Gettempfilenamea => kernel32::handle_get_temp_file_name_a(ctx),
        WinApiId::Kernel32Getdrivetypew => kernel32::handle_get_drive_type_w(ctx),
        WinApiId::Kernel32Getdrivetypea => kernel32::handle_get_drive_type_a(ctx),
        WinApiId::Kernel32Getlogicaldrives => kernel32::handle_get_logical_drives(ctx),
        WinApiId::Kernel32Getsystemdirectoryw => kernel32::handle_get_system_directory_w(ctx),
        WinApiId::Kernel32Getsystemdirectorya => kernel32::handle_get_system_directory_a(ctx),
        WinApiId::Kernel32Getwindowsdirectoryw => kernel32::handle_get_windows_directory_w(ctx),
        WinApiId::Kernel32Getwindowsdirectorya => kernel32::handle_get_windows_directory_a(ctx),
        WinApiId::Kernel32Getfilesizeex => kernel32::handle_get_file_size_ex(ctx),
        WinApiId::Kernel32Setfilepointerex => kernel32::handle_set_file_pointer_ex(ctx),
        WinApiId::Kernel32Setendoffile => kernel32::handle_set_end_of_file(ctx),
        WinApiId::Kernel32Flushfilebuffers => kernel32::handle_flush_file_buffers(ctx),
        WinApiId::D3d9Direct3dcreate9 => d3d9::handle_direct3d_create9(ctx),
        WinApiId::D3d9Idirect3d9Getadaptercount => d3d9::handle_get_adapter_count(ctx),
        WinApiId::D3d9Idirect3d9Getadaptermonitor => d3d9::handle_get_adapter_monitor(ctx),
        WinApiId::D3d9Idirect3d9Getdevicecaps => d3d9::handle_get_device_caps(ctx),
        WinApiId::D3d9Idirect3d9Getadapterdisplaymode => d3d9::handle_get_adapter_display_mode(ctx),
        WinApiId::D3d9Idirect3d9Createdevice => d3d9::handle_create_device(ctx),
        WinApiId::D3d9Idirect3ddevice9Setvertexshader => d3d9::handle_set_vertex_shader(ctx),
        WinApiId::D3d9Idirect3ddevice9Setfvf => d3d9::handle_set_fvf(ctx),
        WinApiId::D3d9Idirect3ddevice9Setrenderstate => d3d9::handle_set_render_state(ctx),
        WinApiId::D3d9Idirect3ddevice9Settexturestagestate => {
            d3d9::handle_set_texture_stage_state(ctx)
        }
        WinApiId::D3d9Idirect3ddevice9Setsamplerstate => d3d9::handle_set_sampler_state(ctx),
        WinApiId::D3d9Idirect3ddevice9Present => d3d9::handle_present(ctx),
        WinApiId::D3d9Idirect3ddevice9Beginscene => d3d9::handle_begin_scene(ctx),
        WinApiId::D3d9Idirect3ddevice9Endscene => d3d9::handle_end_scene(ctx),
        WinApiId::D3d9Idirect3ddevice9Clear => d3d9::handle_clear(ctx),
        WinApiId::D3d9Idirect3ddevice9Settransform => d3d9::handle_set_transform(ctx),
        WinApiId::D3d9Idirect3ddevice9Gettransform => d3d9::handle_get_transform(ctx),
        WinApiId::D3d9Idirect3ddevice9Multiplytransform => d3d9::handle_multiply_transform(ctx),
        WinApiId::D3d9Idirect3ddevice9Setscissorrect => d3d9::handle_set_scissor_rect(ctx),
        WinApiId::D3d9Idirect3ddevice9Setviewport => d3d9::handle_set_viewport(ctx),
        WinApiId::D3d9Idirect3ddevice9Getviewport => d3d9::handle_get_viewport(ctx),
        WinApiId::D3d9Idirect3ddevice9Drawprimitive => d3d9::handle_draw_primitive(ctx),
        WinApiId::D3d9Idirect3ddevice9Drawindexedprimitive => {
            d3d9::handle_draw_indexed_primitive(ctx)
        }
        WinApiId::D3d9Idirect3ddevice9Drawprimitiveup => d3d9::handle_draw_primitive_up(ctx),
        WinApiId::D3d9Idirect3ddevice9Drawindexedprimitiveup => {
            d3d9::handle_draw_indexed_primitive_up(ctx)
        }
        WinApiId::D3d9Idirect3ddevice9Setstreamsource => d3d9::handle_set_stream_source(ctx),
        WinApiId::D3d9Idirect3ddevice9Setindices => d3d9::handle_set_indices(ctx),
        WinApiId::D3d9Idirect3ddevice9Createvertexbuffer => d3d9::handle_create_vertex_buffer(ctx),
        WinApiId::D3d9Idirect3ddevice9Createindexbuffer => d3d9::handle_create_index_buffer(ctx),
        WinApiId::D3d9Idirect3ddevice9Createtexture => d3d9::handle_create_texture(ctx),
        WinApiId::D3d9Idirect3ddevice9Settexture => d3d9::handle_set_texture(ctx),
        WinApiId::D3d9Idirect3ddevice9Gettexture => d3d9::handle_get_texture(ctx),
        WinApiId::D3d9Idirect3ddevice9Gettexturestagestate => {
            d3d9::handle_get_texture_stage_state(ctx)
        }
        WinApiId::D3d9Idirect3ddevice9Getsamplerstate => d3d9::handle_get_sampler_state(ctx),
        WinApiId::D3d9Idirect3dtexture9Getlevelcount => d3d9::handle_texture_get_level_count(ctx),
        WinApiId::D3d9Idirect3dtexture9Getsurfacelevel => {
            d3d9::handle_texture_get_surface_level(ctx)
        }
        WinApiId::D3d9Idirect3dtexture9Lockrect => d3d9::handle_texture_lock_rect(ctx),
        WinApiId::D3d9Idirect3dtexture9Unlockrect => d3d9::handle_texture_unlock_rect(ctx),
        WinApiId::D3d9Idirect3dtexture9Release => d3d9::handle_texture_release(ctx),
        WinApiId::D3d9Idirect3dsurface9Lockrect => d3d9::handle_surface_lock_rect(ctx),
        WinApiId::D3d9Idirect3dsurface9Unlockrect => d3d9::handle_surface_unlock_rect(ctx),
        WinApiId::D3d9Idirect3dsurface9Release => d3d9::handle_surface_release(ctx),
        WinApiId::D3d9Idirect3ddevice9Createdepthstencilsurface => {
            d3d9::handle_create_depth_stencil_surface(ctx)
        }
        WinApiId::D3d9Idirect3ddevice9Setdepthstencilsurface => {
            d3d9::handle_set_depth_stencil_surface(ctx)
        }
        WinApiId::D3d9Idirect3ddevice9Getdepthstencilsurface => {
            d3d9::handle_get_depth_stencil_surface(ctx)
        }
        WinApiId::D3d9Idirect3ddevice9Getrenderstate => d3d9::handle_get_render_state(ctx),
        WinApiId::D3d9Idirect3ddevice9Createvertexshader => d3d9::handle_create_vertex_shader(ctx),
        WinApiId::D3d9Idirect3ddevice9Getvertexshader => d3d9::handle_get_vertex_shader(ctx),
        WinApiId::D3d9Idirect3ddevice9Setvertexshaderconstantf => {
            d3d9::handle_set_vertex_shader_constant_f(ctx)
        }
        WinApiId::D3d9Idirect3ddevice9Getvertexshaderconstantf => {
            d3d9::handle_get_vertex_shader_constant_f(ctx)
        }
        WinApiId::D3d9Idirect3ddevice9Setvertexshaderconstanti => {
            d3d9::handle_set_vertex_shader_constant_i(ctx)
        }
        WinApiId::D3d9Idirect3ddevice9Getvertexshaderconstanti => {
            d3d9::handle_get_vertex_shader_constant_i(ctx)
        }
        WinApiId::D3d9Idirect3ddevice9Setvertexshaderconstantb => {
            d3d9::handle_set_vertex_shader_constant_b(ctx)
        }
        WinApiId::D3d9Idirect3ddevice9Getvertexshaderconstantb => {
            d3d9::handle_get_vertex_shader_constant_b(ctx)
        }
        WinApiId::D3d9Idirect3ddevice9Createrendertarget => d3d9::handle_create_render_target(ctx),
        WinApiId::D3d9Idirect3ddevice9Setrendertarget => d3d9::handle_set_render_target(ctx),
        WinApiId::D3d9Idirect3ddevice9Getrendertarget => d3d9::handle_get_render_target(ctx),
        WinApiId::D3d9Idirect3dsurface9Getdesc => d3d9::handle_surface_get_desc(ctx),
        WinApiId::D3d9Idirect3ddevice9Createpixelshader => d3d9::handle_create_pixel_shader(ctx),
        WinApiId::D3d9Idirect3ddevice9Setpixelshader => d3d9::handle_set_pixel_shader(ctx),
        WinApiId::D3d9Idirect3ddevice9Getpixelshader => d3d9::handle_get_pixel_shader(ctx),
        WinApiId::D3d9Idirect3ddevice9Setpixelshaderconstantf => {
            d3d9::handle_set_pixel_shader_constant_f(ctx)
        }
        WinApiId::D3d9Idirect3ddevice9Getpixelshaderconstantf => {
            d3d9::handle_get_pixel_shader_constant_f(ctx)
        }
        WinApiId::D3d9Idirect3dpixelshader9Release => d3d9::handle_pixel_shader_release(ctx),
        WinApiId::D3d9Idirect3dvertexshader9Release => d3d9::handle_vertex_shader_release(ctx),
        WinApiId::D3d9Idirect3dvertexbuffer9Queryinterface => {
            d3d9::handle_vertex_buffer_query_interface(ctx)
        }
        WinApiId::D3d9Idirect3dvertexbuffer9Addref => d3d9::handle_vertex_buffer_add_ref(ctx),
        WinApiId::D3d9Idirect3dvertexbuffer9Release => d3d9::handle_vertex_buffer_release(ctx),
        WinApiId::D3d9Idirect3dvertexbuffer9Lock => d3d9::handle_vertex_buffer_lock(ctx),
        WinApiId::D3d9Idirect3dvertexbuffer9Unlock => d3d9::handle_vertex_buffer_unlock(ctx),
        WinApiId::D3d9Idirect3dvertexbuffer9Getdesc => d3d9::handle_vertex_buffer_get_desc(ctx),
        WinApiId::D3d9Idirect3dindexbuffer9Queryinterface => {
            d3d9::handle_index_buffer_query_interface(ctx)
        }
        WinApiId::D3d9Idirect3dindexbuffer9Addref => d3d9::handle_index_buffer_add_ref(ctx),
        WinApiId::D3d9Idirect3dindexbuffer9Release => d3d9::handle_index_buffer_release(ctx),
        WinApiId::D3d9Idirect3dindexbuffer9Lock => d3d9::handle_index_buffer_lock(ctx),
        WinApiId::D3d9Idirect3dindexbuffer9Unlock => d3d9::handle_index_buffer_unlock(ctx),
        WinApiId::D3d9Idirect3dindexbuffer9Getdesc => d3d9::handle_index_buffer_get_desc(ctx),
        WinApiId::D3d9Idirect3ddevice9Getstreamsource => d3d9::handle_get_stream_source(ctx),
        WinApiId::D3d9Idirect3ddevice9Getindices => d3d9::handle_get_indices(ctx),
        WinApiId::VersionGetfileversioninfosizew => {
            version::handle_get_file_version_info_size_w(ctx)
        }
        WinApiId::VersionGetfileversioninfosizea => {
            version::handle_get_file_version_info_size_a(ctx)
        }
        WinApiId::VersionGetfileversioninfosizeexw => {
            version::handle_get_file_version_info_size_ex_w(ctx)
        }
        WinApiId::VersionGetfileversioninfosizeexa => {
            version::handle_get_file_version_info_size_ex_a(ctx)
        }
        WinApiId::VersionGetfileversioninfow => version::handle_get_file_version_info_w(ctx),
        WinApiId::VersionGetfileversioninfoa => version::handle_get_file_version_info_a(ctx),
        WinApiId::VersionGetfileversioninfoexw => version::handle_get_file_version_info_ex_w(ctx),
        WinApiId::VersionGetfileversioninfoexa => version::handle_get_file_version_info_ex_a(ctx),
        WinApiId::VersionVerqueryvaluew => version::handle_ver_query_value_w(ctx),
        WinApiId::VersionVerqueryvaluea => version::handle_ver_query_value_a(ctx),
        WinApiId::VersionGetfileversioninfobyhandlew => {
            version::handle_get_file_version_info_by_handle_w(ctx)
        }
        WinApiId::VersionGetfileversioninfobyhandlea => {
            version::handle_get_file_version_info_by_handle_a(ctx)
        }
        WinApiId::VersionVerlanguagenamew => version::handle_ver_language_name_w(ctx),
        WinApiId::VersionVerlanguagenamea => version::handle_ver_language_name_a(ctx),
        WinApiId::VersionVerfindfilew => version::handle_ver_find_file_w(ctx),
        WinApiId::VersionVerfindfilea => version::handle_ver_find_file_a(ctx),
        WinApiId::VersionVerinstallfilew => version::handle_ver_install_file_w(ctx),
        WinApiId::VersionVerinstallfilea => version::handle_ver_install_file_a(ctx),
        WinApiId::User32Enablemenuitem => user32::handle_enable_menu_item(ctx),
        WinApiId::User32Checkmenuitem => user32::handle_check_menu_item(ctx),
        WinApiId::User32Getmessagea => user32::handle_get_message_a(ctx),
        WinApiId::User32Translatemessage => user32::handle_translate_message(ctx),
        WinApiId::User32Defwindowproca => user32::handle_def_window_proc_a(ctx),
        WinApiId::User32Defwindowprocw => user32::handle_def_window_proc_w(ctx),
        WinApiId::User32Defframeproca => user32::handle_def_frame_proc_a(ctx),
        WinApiId::User32Defframeprocw => user32::handle_def_frame_proc_w(ctx),
        WinApiId::User32Defmdichildproca => user32::handle_def_mdi_child_proc_a(ctx),
        WinApiId::User32Defmdichildprocw => user32::handle_def_mdi_child_proc_w(ctx),
        WinApiId::User32Createmenu => user32::handle_create_menu(ctx),
        WinApiId::User32Createpopupmenu => user32::handle_create_popup_menu(ctx),
        WinApiId::User32Appendmenua => user32::handle_append_menu_a(ctx),
        WinApiId::User32Appendmenuw => user32::handle_append_menu_w(ctx),
        WinApiId::User32Setmenu => user32::handle_set_menu(ctx),
        WinApiId::User32Destroymenu => user32::handle_destroy_menu(ctx),
        WinApiId::User32Removemenu => user32::handle_remove_menu(ctx),
        WinApiId::User32Deletemenu => user32::handle_delete_menu(ctx),
        WinApiId::User32Modifymenua => user32::handle_modify_menu_a(ctx),
        WinApiId::User32Modifymenuw => user32::handle_modify_menu_w(ctx),
        WinApiId::User32Getsystemmenu => user32::handle_get_system_menu(ctx),
        WinApiId::User32Trackpopupmenu => user32::handle_track_popup_menu(ctx),
        WinApiId::User32Getmenuiteminfoa => user32::handle_get_menu_item_info_a(ctx),
        WinApiId::User32Getmenuiteminfow => user32::handle_get_menu_item_info_w(ctx),
        WinApiId::User32Setmenuiteminfoa => user32::handle_set_menu_item_info_a(ctx),
        WinApiId::User32Setmenuiteminfow => user32::handle_set_menu_item_info_w(ctx),
        WinApiId::User32Checkmenuradioitem => user32::handle_check_menu_radio_item(ctx),
        WinApiId::User32Dispatchmessagea => user32::handle_dispatch_message_a(ctx),
        WinApiId::D3d9Idirect3ddevice9Release => d3d9::handle_device_release(ctx),
        WinApiId::D3d9Idirect3d9Release => d3d9::handle_direct3d9_release(ctx),
        WinApiId::User32Getmenu => user32::handle_get_menu(ctx),
        WinApiId::Gdi32Getstockobject => gdi32::handle_get_stock_object(ctx),
        WinApiId::Kernel32Writeconsolew => kernel32::console::handle_write_console_w(ctx),
        WinApiId::Kernel32Writeconsolea => kernel32::console::handle_write_console_a(ctx),
        WinApiId::Kernel32Readconsolew => kernel32::console::handle_read_console_w(ctx),
        WinApiId::Kernel32Readconsolea => kernel32::console::handle_read_console_a(ctx),
        WinApiId::Kernel32Getconsolemode => kernel32::console::handle_get_console_mode(ctx),
        WinApiId::Kernel32Setconsolemode => kernel32::console::handle_set_console_mode(ctx),
        WinApiId::Kernel32Writeconsoleoutputw => {
            kernel32::console_cells::handle_write_console_output_w(ctx)
        }
        WinApiId::Kernel32Fillconsoleoutputcharacterw => {
            kernel32::console_cells::handle_fill_console_output_character_w(ctx)
        }
        WinApiId::Kernel32Setconsolecursorposition => {
            kernel32::console_cells::handle_set_console_cursor_position(ctx)
        }
        WinApiId::Kernel32Setconsoletextattribute => {
            kernel32::console_cells::handle_set_console_text_attribute(ctx)
        }
        WinApiId::Kernel32Gettickcount64 => kernel32::misc::handle_get_tick_count_64(ctx),
        WinApiId::Kernel32Getenvironmentvariablew => {
            kernel32::environment::handle_get_environment_variable_w(ctx)
        }
        WinApiId::Kernel32Setenvironmentvariablew => {
            kernel32::environment::handle_set_environment_variable_w(ctx)
        }
        WinApiId::User32Createwindowexa => user32::handle_create_window_ex_a(ctx),
        WinApiId::User32Createwindowexw => user32::handle_create_window_ex_w(ctx),
        WinApiId::User32Destroywindow => user32::handle_destroy_window(ctx),
        WinApiId::User32Postquitmessage => user32::handle_post_quit_message(ctx),
        WinApiId::User32Getmessagew => user32::handle_get_message_w(ctx),
        WinApiId::User32Peekmessagew => user32::handle_peek_message_w(ctx),
        WinApiId::User32Dispatchmessagew => user32::handle_dispatch_message_w(ctx),
        WinApiId::User32Postmessagew => user32::handle_post_message_w(ctx),
        WinApiId::User32Registerclassa => user32::handle_register_class_a(ctx),
        WinApiId::User32Registerclassw => user32::handle_register_class_w(ctx),
        WinApiId::User32Unregisterclassa => user32::handle_unregister_class_a(ctx),
        WinApiId::User32Unregisterclassw => user32::handle_unregister_class_w(ctx),
        WinApiId::User32Validateirect => user32::handle_validate_rect(ctx),
        WinApiId::User32Setwindowlonga => user32::handle_set_window_long_a(ctx),
        WinApiId::User32Setwindowlongw => user32::handle_set_window_long_w(ctx),
        WinApiId::User32Getwindowdc => user32::handle_get_window_dc(ctx),
        WinApiId::User32Getclassnamea => user32::handle_get_class_name_a(ctx),
        WinApiId::User32Getclassnamew => user32::handle_get_class_name_w(ctx),
        WinApiId::User32Getclasslongptra => user32::handle_get_class_long_ptr_a(ctx),
        WinApiId::User32Getclasslongptrw => user32::handle_get_class_long_ptr_w(ctx),
        WinApiId::User32Setclasslongptra => user32::handle_set_class_long_ptr_a(ctx),
        WinApiId::User32Setclasslongptrw => user32::handle_set_class_long_ptr_w(ctx),
        WinApiId::User32Drawmenubar => user32::handle_draw_menu_bar(ctx),
        WinApiId::User32Getmenustate => user32::handle_get_menu_state(ctx),
        WinApiId::Gdi32Createsolidbrush => gdi32::handle_create_solid_brush(ctx),
        WinApiId::User32Fillrect => gdi32::handle_fill_rect(ctx),
        WinApiId::Gdi32Createpen => gdi32::handle_create_pen(ctx),
        WinApiId::Gdi32Textoutw => gdi32::handle_text_out_w(ctx),
        WinApiId::Gdi32Drawtexta => gdi32::handle_draw_text_a(ctx),
        WinApiId::Gdi32Drawtextw => gdi32::handle_draw_text_w(ctx),
        WinApiId::User32Createdialogparama => user32::handle_create_dialog_param_a(ctx),
        WinApiId::User32Createdialogparamw => user32::handle_create_dialog_param_w(ctx),
        WinApiId::User32Isdialogmessagea => user32::handle_is_dialog_message_a(ctx),
        WinApiId::User32Isdialogmessagew => user32::handle_is_dialog_message_w(ctx),
        WinApiId::User32Enddialog => user32::handle_end_dialog(ctx),
        WinApiId::User32Getdlgitema => user32::handle_get_dlg_item_a(ctx),
        WinApiId::User32Getdlgitemw => user32::handle_get_dlg_item_w(ctx),
        WinApiId::User32Getdlgitemtexta => user32::handle_get_dlg_item_text_a(ctx),
        WinApiId::User32Getdlgitemtextw => user32::handle_get_dlg_item_text_w(ctx),
        WinApiId::User32Setdlgitemtexta => user32::handle_set_dlg_item_text_a(ctx),
        WinApiId::User32Setdlgitemtextw => user32::handle_set_dlg_item_text_w(ctx),
        WinApiId::User32Defdlgproca => user32::handle_def_dlg_proc_a(ctx),
        WinApiId::User32Defdlgprocw => user32::handle_def_dlg_proc_w(ctx),
        WinApiId::User32Registerwindowmessagea => user32::handle_register_window_message_a(ctx),
        WinApiId::User32Registerwindowmessagew => user32::handle_register_window_message_w(ctx),
        WinApiId::User32Loadstringa => user32::handle_load_string_a(ctx),
        WinApiId::User32Loadstringw => user32::handle_load_string_w(ctx),
        WinApiId::Advapi32Regopenkeya => advapi32::handle_reg_open_key_a(ctx),
        WinApiId::Advapi32Regopenkeyw => advapi32::handle_reg_open_key_w(ctx),
        WinApiId::Gdi32Createfontindirectw => gdi32::handle_create_font_indirect_w(ctx),
        WinApiId::User32Loadiconw => user32::handle_load_icon_w(ctx),
        WinApiId::User32Loadcursorw => user32::handle_load_cursor_w(ctx),
        WinApiId::Shell32Dragacceptfiles => shell32::handle_drag_accept_files(ctx),
        WinApiId::Comctl32Createstatuswindowa => comctl32::handle_create_status_window_a(ctx),
        WinApiId::Comctl32Createstatuswindoww => comctl32::handle_create_status_window_w(ctx),
        WinApiId::Comdlg32Getfiletitlea => comdlg32::handle_get_file_title_a(ctx),
        WinApiId::Comdlg32Getfiletitlew => comdlg32::handle_get_file_title_w(ctx),
        WinApiId::User32Getwindowtextlengtha => user32::handle_get_window_text_length_a(ctx),
        WinApiId::User32Getwindowtextlengthw => user32::handle_get_window_text_length_w(ctx),
        WinApiId::User32Getwindowplacement => user32::handle_get_window_placement(ctx),
        WinApiId::User32Setwindowplacement => user32::handle_set_window_placement(ctx),
        WinApiId::User32Loadacceleratorsa => user32::handle_load_accelerators_a(ctx),
        WinApiId::User32Loadacceleratorsw => user32::handle_load_accelerators_w(ctx),
        WinApiId::User32Translateacceleratora => user32::handle_translate_accelerator_a(ctx),
        WinApiId::User32Translateacceleratorw => user32::handle_translate_accelerator_w(ctx),
        WinApiId::User32Destroyacceleratortable => user32::handle_destroy_accelerator_table(ctx),
        WinApiId::User32Loadmenua => user32::handle_load_menu_a(ctx),
        WinApiId::User32Loadmenuw => user32::handle_load_menu_w(ctx),
        WinApiId::User32Isclipboardformatavailable => {
            crate::clipboard::handle_is_clipboard_format_available(ctx)
        }
        WinApiId::Shell32Dragqueryfilew => user32::handle_drag_query_file_w(ctx),
        WinApiId::Shell32Dragqueryfilea => user32::handle_drag_query_file_a(ctx),
        WinApiId::Shell32Dragquerypoint => user32::handle_drag_query_point(ctx),
        WinApiId::Shell32Dragfinish => user32::handle_drag_finish(ctx),
        WinApiId::Kernel32Locallock => kernel32::handle_local_lock(ctx),
        WinApiId::Kernel32Localunlock => kernel32::handle_local_unlock(ctx),
        WinApiId::Kernel32Gettimeformatw => kernel32::handle_get_time_format_w(ctx),
        WinApiId::Kernel32Getdateformatw => kernel32::handle_get_date_format_w(ctx),
        WinApiId::User32Getdlgitemint => user32::handle_get_dlg_item_int(ctx),
        WinApiId::User32Setdlgitemint => user32::handle_set_dlg_item_int(ctx),
        WinApiId::User32Senddlgitemmessagew => user32::handle_send_dlg_item_message_w(ctx),
        WinApiId::Shell32Shellaboutw => shell32::handle_shell_about_w(ctx),
        WinApiId::Shell32Shellexecutew => shell32::handle_shell_execute_w(ctx),
        WinApiId::User32Callwindowproca => user32::handle_call_window_proc_a(ctx),
        WinApiId::User32Callwindowprocw => user32::handle_call_window_proc_w(ctx),
        WinApiId::Comdlg32Findtextw => comdlg32::handle_find_text_w(ctx),
        WinApiId::Comdlg32Replacetextw => comdlg32::handle_replace_text_w(ctx),
        WinApiId::Gdi32Startdocw => gdi32::handle_start_doc_w(ctx),
        WinApiId::Gdi32Startpage => gdi32::handle_start_page(ctx),
        WinApiId::Gdi32Endpage => gdi32::handle_end_page(ctx),
        WinApiId::Gdi32Enddoc => gdi32::handle_end_doc(ctx),
        WinApiId::Gdi32Abortdoc => gdi32::handle_abort_doc(ctx),
        WinApiId::Gdi32Createdcw => gdi32::handle_create_dc_w(ctx),
        WinApiId::Gdi32Gettextmetricsw => gdi32::handle_get_text_metrics_w(ctx),
        WinApiId::Gdi32Setmapmode => gdi32::handle_set_map_mode(ctx),
        WinApiId::Gdi32Rectangle => gdi32::handle_rectangle(ctx),
        WinApiId::User32Inflaterect => user32::handle_inflate_rect(ctx),
    }
}

/// Cold-path wrapper for callers that only have library/name strings.
///
/// Both current callers (session / worker) already checked `resolved.winapi_id`
/// and only fall through here when the API is NOT in the dense id table, so we
/// skip the redundant `resolve_winapi_id` scan and go straight to the UCRT /
/// per-library fallbacks. `resolve_winapi_id` is still available for callers
/// that don't have a pre-resolved id.
pub fn dispatch_winapi(
    ctx: &mut HandlerContext<'_>,
    library: &str,
    name: &str,
) -> Result<WinApiHandlerResult> {
    // UCRT API sets (api-ms-win-crt-*.dll) + ucrtbase/msvcrt — CRT-linked PEs.
    if crate::ucrt::is_ucrt_library(library) {
        return crate::ucrt::dispatch_ucrt(ctx, name);
    }
    // Kernel32 CRT-deps not yet in the dense id table (Virtual*, Tls*).
    if library.eq_ignore_ascii_case("KERNEL32.dll")
        && let Some(r) = kernel32::dispatch_kernel32_extra(ctx, name)?
    {
        return Ok(r);
    }
    if library.eq_ignore_ascii_case("ole32.dll")
        && let Some(r) = crate::ole32::dispatch_ole32(ctx, name)?
    {
        return Ok(r);
    }
    if library.eq_ignore_ascii_case("shell32.dll")
        && let Some(r) = crate::shell32::dispatch_shell32(ctx, name)?
    {
        return Ok(r);
    }
    if library.eq_ignore_ascii_case("advapi32.dll")
        && let Some(r) = advapi32::dispatch_advapi32_extra(ctx, name)?
    {
        return Ok(r);
    }
    if library.eq_ignore_ascii_case("oleaut32.dll")
        && let Some(r) = crate::oleaut32::dispatch_oleaut32(ctx, name)?
    {
        return Ok(r);
    }
    if library.eq_ignore_ascii_case("ws2_32.dll")
        && let Some(r) = crate::ws2_32::dispatch_ws2(ctx, name)?
    {
        return Ok(r);
    }
    if library.eq_ignore_ascii_case("crypt32.dll")
        && let Some(r) = crate::crypt32::dispatch_crypt32(ctx, name)?
    {
        return Ok(r);
    }
    if library.eq_ignore_ascii_case("msimg32.dll")
        && let Some(r) = crate::msimg32::dispatch_msimg32(ctx, name)?
    {
        return Ok(r);
    }
    if library.eq_ignore_ascii_case("uxtheme.dll")
        && let Some(r) = crate::uxtheme::dispatch_uxtheme(ctx, name)?
    {
        return Ok(r);
    }
    if library.eq_ignore_ascii_case("imm32.dll")
        && let Some(r) = crate::imm32::dispatch_imm32(ctx, name)?
    {
        return Ok(r);
    }
    if (library.eq_ignore_ascii_case("setupapi.dll")
        || library.eq_ignore_ascii_case("cfgmgr32.dll"))
        && let Some(r) = crate::setupapi::dispatch_setupapi(ctx, name)?
    {
        return Ok(r);
    }
    if (library.eq_ignore_ascii_case("dbghelp.dll") || library.eq_ignore_ascii_case("imagehlp.dll"))
        && let Some(r) = crate::dbghelp::dispatch_dbghelp(ctx, name)?
    {
        return Ok(r);
    }
    // Mingw runtime DLLs (pthread, libstdc++).
    if library.eq_ignore_ascii_case("libwinpthread-1.dll")
        || library.eq_ignore_ascii_case("libwinpthread-1")
        || library.starts_with("libwinpthread")
    {
        return crate::mingw_dispatch::dispatch_pthread(ctx, name);
    }
    if library.eq_ignore_ascii_case("libstdc++-6.dll")
        || library.eq_ignore_ascii_case("libstdc++-6")
        || library.starts_with("libstdc++")
    {
        return crate::mingw_dispatch::dispatch_stdcpp(ctx, name);
    }
    bail!("unsupported WinAPI call: {library}!{name}");
}

/// Per-API trait flags indexed by [`WinApiId`] discriminant (zero-cost lookup).
static WINAPI_TRAITS: [WinApiTraits; WINAPI_ID_COUNT] = {
    let mut t = [WinApiTraits::EMPTY; WINAPI_ID_COUNT];

    // ── CS host-handler requirement (no in-guest / fast-void-sync) ──────
    t[WinApiId::Kernel32Entercriticalsection as u16 as usize] = WinApiTraits::EMPTY.with_noisy();
    t[WinApiId::Kernel32Leavecriticalsection as u16 as usize] = WinApiTraits::EMPTY.with_noisy();

    // ── In-guest stubs / guest-accelerated ──────────────────────────────
    let guest_stub = WinApiTraits::EMPTY.with_noisy().with_guest_stub();
    t[WinApiId::Kernel32Encodepointer as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Decodepointer as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Gettickcount as u16 as usize] = guest_stub;
    // B5: clock APIs read the host-written guest clock table in-guest.
    t[WinApiId::Kernel32Gettickcount64 as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Getsystemtimeasfiletime as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Queryperformancecounter as u16 as usize] = guest_stub;
    t[WinApiId::WinmmTimegettime as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Getcurrentprocessid as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Getcurrentthreadid as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Sleep as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Getacp as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Getoemcp as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Getsystemdefaultlangid as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Getuserdefaultlangid as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Getcommandlinea as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Getcommandlinew as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Getcurrentdirectoryw as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Getlasterror as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Setlasterror as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Flsgetvalue as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Flssetvalue as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Heapalloc as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Heapfree as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Readfile as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Setfilepointer as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Getfilesize as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Multibytetowidechar as u16 as usize] = guest_stub;
    t[WinApiId::User32Getsystemmetrics as u16 as usize] = guest_stub;
    t[WinApiId::User32Getsyscolor as u16 as usize] = guest_stub;
    t[WinApiId::User32Getsyscolorbrush as u16 as usize] = guest_stub;
    t[WinApiId::User32Getdesktopwindow as u16 as usize] = guest_stub;

    // ── Host-only noisy (tracing) ──────────────────────────────────────
    let noisy = WinApiTraits::EMPTY.with_noisy();
    t[WinApiId::Kernel32Getfileinformationbyhandle as u16 as usize] = noisy;
    t[WinApiId::Kernel32Getfiletype as u16 as usize] = noisy;
    t[WinApiId::Kernel32Getprocaddress as u16 as usize] = noisy;
    t[WinApiId::Kernel32Heaprealloc as u16 as usize] = noisy;
    t[WinApiId::Kernel32Heapsize as u16 as usize] = noisy;
    t[WinApiId::Kernel32Writefile as u16 as usize] = noisy;

    t
};

impl WinApiId {
    /// Lookup the trait flags for this API (constant-time array access).
    #[must_use]
    pub fn traits(self) -> WinApiTraits {
        WINAPI_TRAITS[usize::from(self.to_u16())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reject anything outside the valid discriminant domain: one past the
    /// highest variant, the `u16` ceiling, and the two holes left by removed
    /// APIs (which the old transmute would have fabricated into invalid enum
    /// values).
    #[test]
    fn from_u16_out_of_range_returns_none() {
        let count = u16::try_from(WINAPI_ID_COUNT).expect("count fits u16");
        assert!(WinApiId::from_u16(count).is_none());
        assert!(WinApiId::from_u16(u16::MAX).is_none());
        assert!(WinApiId::from_u16(109).is_none());
        assert!(WinApiId::from_u16(110).is_none());
        // The strum derive must agree with the wrapper's bounds semantics.
        assert!(WinApiId::from_repr(count).is_none());
        assert!(WinApiId::from_repr(u16::MAX).is_none());
    }

    /// The alias is one past the highest discriminant (the size every
    /// discriminant-indexed array needs) — not the strum variant count, which
    /// is lower because of the two holes. Both quantities are pinned here so
    /// an append/renumber/backfill shows up as a test diff.
    #[test]
    fn count_is_pinned_to_enum() {
        let last_plus_one = usize::from(WinApiId::D3d9Idirect3dsurface9Getdesc.to_u16()) + 1;
        assert_eq!(WINAPI_ID_COUNT, last_plus_one);
        assert_eq!(WINAPI_ID_COUNT, 507);
        assert_eq!(WinApiId::COUNT, 505); // 505 variants, two discriminant holes
    }

    /// Every discriminant is either a round-trippable variant (its `to_u16`
    /// reproduces it) or one of the two known holes.
    #[test]
    fn discriminant_round_trip() {
        for raw in 0..WINAPI_ID_COUNT {
            let raw = u16::try_from(raw).expect("WINAPI_ID_COUNT fits u16");
            match WinApiId::from_u16(raw) {
                Some(id) => assert_eq!(id.to_u16(), raw),
                None => assert!(
                    raw == 109 || raw == 110,
                    "unexpected hole at discriminant {raw}"
                ),
            }
        }
        // The final variant is exactly the highest discriminant.
        let last = u16::try_from(WINAPI_ID_COUNT - 1).expect("count fits u16");
        assert_eq!(WinApiId::D3d9Idirect3dsurface9Getdesc.to_u16(), last);
    }
}
