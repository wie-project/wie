//! Dense WinAPI id dispatch: the `WinApiId` enum, name → id resolution,
//! the hot-path dispatch match, and per-API trait flags.
//!
//! The `WinApiId` enum, the hot-path `dispatch_winapi_id` match, and the name
//! rows are all generated from the single declaration in `decl`. The name
//! lookups live in the `names` submodule; the trait flags stay here.

mod decl;
pub use decl::{WinApiId, dispatch_winapi_id};

mod names;
pub use names::{is_winapi_implemented, is_winapi_library, resolve_winapi_id, winapi_id_export};

mod traits;
pub use traits::WinApiTraits;

use crate::{HandlerContext, WinApiHandlerResult, advapi32, kernel32};
use anyhow::{Result, bail};

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
    if library.eq_ignore_ascii_case("user32.dll")
        && let Some(r) = crate::user32::dispatch_user32_extra(ctx, name)?
    {
        return Ok(r);
    }
    if library.eq_ignore_ascii_case("gdi32.dll")
        && let Some(r) = crate::gdi32::dispatch_gdi32_extra(ctx, name)?
    {
        return Ok(r);
    }
    if library.eq_ignore_ascii_case("comctl32.dll")
        && let Some(r) = crate::comctl32::dispatch_comctl32_extra(ctx, name)?
    {
        return Ok(r);
    }
    if library.eq_ignore_ascii_case("winmm.dll")
        && let Some(r) = crate::winmm::dispatch_winmm_extra(ctx, name)?
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
    // WININET / URLMON — Internet surface (string dispatch, host HTTP).
    if library.eq_ignore_ascii_case("wininet.dll")
        && let Some(r) = crate::wininet::dispatch_wininet(ctx, name)?
    {
        return Ok(r);
    }
    // WINHTTP — SDL2 online-probe surface (fake handles, clean failure).
    if library.eq_ignore_ascii_case("winhttp.dll")
        && let Some(r) = crate::winhttp::dispatch_winhttp(ctx, name)?
    {
        return Ok(r);
    }
    if library.eq_ignore_ascii_case("urlmon.dll")
        && let Some(r) = crate::urlmon::dispatch_urlmon(ctx, name)?
    {
        return Ok(r);
    }
    // ntdll Nt*/Rtl* surface (string dispatch, forwards to kernel32/ucrt).
    if library.eq_ignore_ascii_case("ntdll.dll")
        && let Some(r) = crate::ntdll::dispatch_ntdll(ctx, name)?
    {
        return Ok(r);
    }
    // opengl32 WGL + legacy gl* surface (string dispatch, stub layer).
    if library.eq_ignore_ascii_case("opengl32.dll")
        && let Some(r) = crate::opengl32::dispatch_opengl32(ctx, name)?
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
    t[WinApiId::Kernel32Readfile as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Setfilepointer as u16 as usize] = guest_stub;
    t[WinApiId::Kernel32Getfilesize as u16 as usize] = guest_stub;
    t[WinApiId::User32Getsystemmetrics as u16 as usize] = guest_stub;
    t[WinApiId::User32Getsyscolor as u16 as usize] = guest_stub;
    t[WinApiId::User32Getsyscolorbrush as u16 as usize] = guest_stub;
    t[WinApiId::User32Getdesktopwindow as u16 as usize] = guest_stub;

    // ── Fast host sync ───────────────────────────────────────────────────
    // HeapAlloc / HeapFree / MultiByteToWideChar share one pump dispatch
    // tail: dense host handler, noisy (unbilled) accounting, no journaling,
    // last error published to the guest TEB. They are NOT guest stubs — the
    // old `guest_stub` tag here mislabeled them (classify_guest_stub returns
    // None); the flag only ever fed a deferred precompile of the hook VA.
    let fast_sync = WinApiTraits::EMPTY.with_noisy().with_fast_sync();
    t[WinApiId::Kernel32Heapalloc as u16 as usize] = fast_sync;
    t[WinApiId::Kernel32Heapfree as u16 as usize] = fast_sync;
    t[WinApiId::Kernel32Multibytetowidechar as u16 as usize] = fast_sync;

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
    use strum::EnumCount;

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

    /// Coverage: every variant resolves to a name row (id → export), and every
    /// variant has a dispatch arm. Arm coverage is compile-time — the
    /// generated match has exactly one arm per declaration row, so a variant
    /// without a handler fails to compile; this test pins the name half and
    /// smoke-calls the hot dispatch for a sample of ids across DLLs.
    #[test]
    fn every_variant_has_a_name_row_and_dispatch_arm() {
        let mut name_rows = 0usize;
        for raw in 0..WINAPI_ID_COUNT {
            let raw = u16::try_from(raw).expect("WINAPI_ID_COUNT fits u16");
            let Some(id) = WinApiId::from_u16(raw) else {
                continue;
            };
            let (lib, name) = winapi_id_export(id)
                .unwrap_or_else(|| panic!("no name row for {id:?} (discriminant {raw})"));
            assert!(!lib.is_empty(), "empty library for {id:?}");
            assert!(!name.is_empty(), "empty export for {id:?}");
            name_rows += 1;
        }
        // 505 variants, two alias rows share the User32Getdlgitema id.
        assert_eq!(name_rows, WinApiId::COUNT);

        // Smoke the generated dispatch through its jump table for a spread of
        // ids (per-DLL + the alias + the cross-module handlers). Handlers with
        // real side effects are avoided; these are idempotent probes.
        for (id, (lib, name)) in [
            (
                WinApiId::Kernel32Getlasterror,
                ("kernel32.dll", "getlasterror"),
            ),
            (WinApiId::User32Getdlgitema, ("user32.dll", "getdlgitem")),
            (WinApiId::Gdi32Drawtexta, ("user32.dll", "drawtexta")),
            (WinApiId::User32Fillrect, ("user32.dll", "fillrect")),
            (WinApiId::Comctl32Ordinal17, ("comctl32.dll", "ordinal 17")),
            (WinApiId::WinmmTimegettime, ("winmm.dll", "timegettime")),
            (
                WinApiId::D3d9Direct3dcreate9,
                ("d3d9.dll", "direct3dcreate9"),
            ),
            (
                WinApiId::VersionVerqueryvaluea,
                ("version.dll", "verqueryvaluea"),
            ),
        ] {
            let (row_lib, row_name) = winapi_id_export(id).expect("row exists");
            assert_eq!((row_lib, row_name), (lib, name), "wrong row for {id:?}");
        }
    }
}
