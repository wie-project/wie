//! `user32.dll` name → id rows for the dense WinAPI dispatch.
//!
//! Row order mirrors the `WinApiId` enum's `User32*` variants and
//! matches the pre-split dense table exactly, so each row keeps its original
//! discriminant mapping.

use crate::dispatch_table::WinApiId;

/// `user32.dll` rows in dense `WinApiId` order.
pub(super) const ROWS: &[(&str, &str, WinApiId)] = &[
    (
        "user32.dll",
        "getasynckeystate",
        WinApiId::User32Getasynckeystate,
    ),
    ("user32.dll", "peekmessagea", WinApiId::User32Peekmessagea),
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
    ("user32.dll", "messageboxw", WinApiId::User32Messageboxw),
    ("user32.dll", "messageboxa", WinApiId::User32Messageboxa),
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
    ("user32.dll", "releasedc", WinApiId::User32Releasedc),
    ("user32.dll", "loadimagea", WinApiId::User32Loadimagea),
    ("user32.dll", "loadimagew", WinApiId::User32Loadimagew),
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
    ("user32.dll", "getmenu", WinApiId::User32Getmenu),
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
    ("user32.dll", "fillrect", WinApiId::User32Fillrect),
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
        // Rows placed at the table end with the appended variants (447/448),
        // so the id table and the name rows stay in the same order.
        "user32.dll",
        "callwindowproca",
        WinApiId::User32Callwindowproca,
    ),
    (
        "user32.dll",
        "callwindowprocw",
        WinApiId::User32Callwindowprocw,
    ),
    ("user32.dll", "inflaterect", WinApiId::User32Inflaterect),
];
