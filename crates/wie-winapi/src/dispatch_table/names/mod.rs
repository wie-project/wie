//! Name → id resolution table and lookups for the dense WinAPI dispatch.
//!
//! The `(library, name, id)` rows are generated from the single declaration in
//! `super::decl`; this module implements the lookups over that flat row list.
//!
//! The soft (non-dense) export lists are NOT kept here: each dispatch module
//! owns a `pub(crate) const EXPORTS` beside the `dispatch_*` match it describes
//! (ADR-0009), and this module only consumes them — in
//! [`is_winapi_implemented`] and in the `SOFT_EXPORT_CENSUS` table the census
//! drift guard walks.

use super::decl::WINAPI_NAME_ROWS;
use crate::dispatch_table::WinApiId;

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
/// Names are lowercase (as in the generated rows). Returns the first row for
/// the id (export aliases share the id). Used for trace/profile only.
#[must_use]
pub fn winapi_id_export(id: WinApiId) -> Option<(&'static str, &'static str)> {
    for &(lib, export, row_id) in WINAPI_NAME_ROWS {
        if row_id == id {
            return Some((lib, export));
        }
    }
    None
}

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
            | "dinput8.dll"
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
        return crate::ucrt::EXPORTS.contains(&n.as_str());
    }
    let n = name.to_ascii_lowercase();
    let n = n.as_str();
    match library.to_ascii_lowercase().as_str() {
        "ole32.dll" => crate::ole32::EXPORTS.contains(&n),
        "shell32.dll" => crate::shell32::EXPORTS.contains(&n),
        "oleaut32.dll" => crate::oleaut32::EXPORTS.contains(&n),
        "user32.dll" => crate::user32::EXPORTS.contains(&n),
        "gdi32.dll" => crate::gdi32::EXPORTS.contains(&n),
        "comctl32.dll" => crate::comctl32::EXPORTS.contains(&n),
        "winmm.dll" => crate::winmm::EXPORTS.contains(&n),
        "advapi32.dll" => crate::advapi32::EXPORTS.contains(&n),
        "kernel32.dll" => crate::kernel32::EXPORTS.contains(&n),
        "ws2_32.dll" => crate::ws2_32::EXPORTS.contains(&n),
        "crypt32.dll" => crate::crypt32::EXPORTS.contains(&n),
        "msimg32.dll" => crate::msimg32::EXPORTS.contains(&n),
        "imm32.dll" => crate::imm32::EXPORTS.contains(&n),
        "uxtheme.dll" => crate::uxtheme::EXPORTS.contains(&n),
        "setupapi.dll" | "cfgmgr32.dll" => crate::setupapi::EXPORTS.contains(&n),
        "dbghelp.dll" | "imagehlp.dll" => crate::dbghelp::EXPORTS.contains(&n),
        "wininet.dll" => crate::wininet::is_export(n),
        "winhttp.dll" => crate::winhttp::EXPORTS.contains(&n),
        "urlmon.dll" => crate::urlmon::is_export(n),
        "ntdll.dll" => crate::ntdll::is_export(n),
        "opengl32.dll" => crate::opengl32::is_export(n),
        "dinput8.dll" => crate::dinput::EXPORTS.contains(&n),
        _ => false,
    }
}

/// Every soft (string-dispatched) export census, paired with the library it
/// belongs to — the single table the cross-registry drift guard walks.
///
/// One row per `(library, census list)` pair, in the same order as the arms of
/// [`is_winapi_implemented`]. Every list is a module's `EXPORTS` const, so the
/// table cannot drift from the oracle: both read the same constant. The guard
/// in `dispatch_table/census_tests` sweeps each list against the dispatch arms
/// that are supposed to claim it.
#[cfg(test)]
pub(crate) const SOFT_EXPORT_CENSUS: &[(&str, &[&str])] = &[
    ("ucrtbase.dll", crate::ucrt::EXPORTS),
    ("ole32.dll", crate::ole32::EXPORTS),
    ("shell32.dll", crate::shell32::EXPORTS),
    ("oleaut32.dll", crate::oleaut32::EXPORTS),
    ("user32.dll", crate::user32::EXPORTS),
    ("gdi32.dll", crate::gdi32::EXPORTS),
    ("comctl32.dll", crate::comctl32::EXPORTS),
    ("winmm.dll", crate::winmm::EXPORTS),
    ("advapi32.dll", crate::advapi32::EXPORTS),
    ("kernel32.dll", crate::kernel32::EXPORTS),
    ("ws2_32.dll", crate::ws2_32::EXPORTS),
    ("crypt32.dll", crate::crypt32::EXPORTS),
    ("msimg32.dll", crate::msimg32::EXPORTS),
    ("imm32.dll", crate::imm32::EXPORTS),
    ("uxtheme.dll", crate::uxtheme::EXPORTS),
    ("winhttp.dll", crate::winhttp::EXPORTS),
    ("setupapi.dll", crate::setupapi::EXPORTS),
    ("cfgmgr32.dll", crate::setupapi::EXPORTS),
    ("dbghelp.dll", crate::dbghelp::EXPORTS),
    ("imagehlp.dll", crate::dbghelp::EXPORTS),
    ("dinput8.dll", crate::dinput::EXPORTS),
];

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
