//! The census-vs-dispatch drift guard (T3.6).
//!
//! ## The hole this closes
//!
//! WIE keeps its WinAPI surface in four hand-maintained registries that no test
//! tied together before this module:
//!
//! 1. the soft export census lists in [`names`] (`SOFT_EXPORT_CENSUS`, plus
//!    `is_winapi_library` / `is_winapi_implemented`),
//! 2. the per-DLL `*_EXPORTS` lists with their own oracles (`ntdll::NTDL_EXPORTS`,
//!    `urlmon::URLMON_EXPORTS`, `opengl32::OPENGL32_EXPORT_NAMES`, `wininet`),
//! 3. `PREPLANTED_SOFT_APIS` / `DYNAMIC_FAKE_APIS` in [`crate::dynamic_apis`],
//! 4. the per-DLL `dispatch_*` match arms in the handler modules.
//!
//! Forgetting a row in (1)–(3) produces **no runtime failure at all**:
//! `is_winapi_implemented` is not consulted on the production dispatch path
//! (`dispatch_winapi` walks its own `if library == … && dispatch_x(ctx, name)?`
//! chain), so a census gap only degrades `wie inspect` output. Forgetting an
//! arm in (4) is *louder* — the guest hits
//! `bail!("unsupported WinAPI call: {library}!{name}")` — but only if some
//! guest actually imports that export.
//!
//! ## What is asserted
//!
//! For every `(library, name)` in every registry above, dispatching it must
//! **not** fall through to the unsupported-API bail. That is the reachable
//! oracle, and it is the one that matches the production path.
//!
//! The dispatch is driven with all-zero registers, which is the same technique
//! the pre-existing `ntdll::tests::every_reported_export_dispatches` uses. Two
//! outcomes count as "reachable":
//!
//! * `Ok(Some(_))` — the handler ran and returned.
//! * `Err(_)` — the handler ran and rejected the (bogus) arguments. The error
//!   is the handler's own, not the dispatcher's fall-through.
//!
//! Only `Ok(None)` is a failure: that is precisely "no arm matched", which is
//! what `dispatch_winapi` turns into the bail. Distinguishing the two `Err`
//! cases is done by message, with the bail's exact format string as the marker.

use crate::WinApiHandlerResult;
use crate::state::tests::winapi_state_default_with_bump_heap;
use crate::state::{HandlerContext, WinApiEnvironment, WinApiState};
use wie_cpu::{CpuEngine, IcedCpu, RwxPerms};

use super::names::SOFT_EXPORT_CENSUS;

const STACK_VA: u64 = 0x100_0000;
const STACK_SIZE: usize = 0x1_0000;
// STACK_VA + STACK_SIZE - 0x100 — leave room for the return address.
const STACK_TOP: u64 = 0x100_FF00;
/// The guest-heap control page the runtime seeds at 0x2000; the bump-cursor
/// initialisers read it, so an unmapped read would trap.
const HEAP_CTRL: u64 = 0x2000;

/// The exact text `dispatch_winapi` bails with when nothing matched. Matched
/// by `contains`, so a future reformat of the message does not silently turn
/// the guard into a no-op — an unmatched bail still fails the assertion.
const UNSUPPORTED_MARKER: &str = "unsupported WinAPI call";

/// Sentinel `Err` for "the matched handler panicked". Deliberately distinct
/// from [`UNSUPPORTED_MARKER`]: a panic proves an arm matched, the bail proves
/// one did not.
const PANIC_MARKER: &str = "<handler panicked on the zero-register probe>";

/// Minimal engine: guest pages, the heap-control page, and a stack with a
/// valid return address (every handler ends in `return_from_win64_api`, which
/// pops from it).
fn test_engine() -> IcedCpu {
    let mut cpu = IcedCpu::open_x86_64();
    cpu.mem_map(0x1000, 0x10_0000, RwxPerms::ALL)
        .expect("map test memory");
    cpu.mem_map(STACK_VA, STACK_SIZE, RwxPerms::ALL)
        .expect("map test stack");
    cpu.mem_write(STACK_TOP, &0_u64.to_le_bytes())
        .expect("write return address");
    cpu.mem_write(HEAP_CTRL, &HEAP_CTRL.to_le_bytes())
        .expect("seed heap bump cursor");
    cpu.write_rsp(STACK_TOP).ok();
    cpu
}

fn test_environment() -> WinApiEnvironment {
    WinApiEnvironment {
        image_base: 0x0000_0000_1400_0000,
        command_line_a_ptr: 0,
        command_line_w_ptr: 0,
        environment_strings_w_ptr: 0,
        module_file_name_a_ptr: 0,
        module_file_name_w_ptr: 0,
        process_heap_handle: 1,
    }
}

/// The shared fixture already seeds the guest heap's control block at 0x2000.
fn test_state() -> WinApiState {
    winapi_state_default_with_bump_heap()
}

/// A minimal per-call fixture: fresh engine + state, all-zero registers, and
/// the two stack-passed argument slots zeroed (Win64 passes args 5+ on the
/// stack; an unwritten slot is uninitialised host memory).
struct Call {
    engine: IcedCpu,
    state: WinApiState,
}

impl Call {
    fn new() -> Self {
        Self {
            engine: test_engine(),
            state: test_state(),
        }
    }

    /// Zero every argument register and the two stack argument slots, then
    /// invoke `f` with a context bound to this call's engine + state.
    fn run(
        &mut self,
        f: impl FnOnce(&mut HandlerContext<'_>, &str) -> Result<Option<WinApiHandlerResult>, String>,
        name: &str,
    ) -> Result<Option<WinApiHandlerResult>, String> {
        let Self { engine, state } = self;
        engine.write_rcx(0).ok();
        engine.write_rdx(0).ok();
        engine.write_r8(0).ok();
        engine.write_r9(0).ok();
        engine.write_rsp(STACK_TOP).ok();
        for slot in [STACK_TOP + 0x28, STACK_TOP + 0x30] {
            engine
                .mem_write(slot, &0_u64.to_le_bytes())
                .expect("zero stack argument");
        }
        let mut ctx = HandlerContext::new(engine, test_environment(), state);
        // A handler that does not survive all-zero registers (string parsing,
        // division by a guest value) panics here. That still PROVES an arm
        // matched — the dispatchers are plain `match`es with no panicking
        // prologue — so the panic is caught and reported as reachability
        // rather than allowed to abort the sweep. The hook is silenced so one
        // fragile handler cannot bury the rest of the sweep's output.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&mut ctx, name)));
        std::panic::set_hook(previous);
        outcome.unwrap_or_else(|_| Err(PANIC_MARKER.to_owned()))
    }
}

/// Whether a dispatch outcome means "a handler claimed this name".
///
/// `Ok(None)` is the ONLY failure: it is the dispatcher's "no arm matched",
/// which `dispatch_winapi` converts into `bail!(UNSUPPORTED_MARKER)`. An
/// `Err` is the handler's own rejection of the all-zero arguments and is
/// expected — but the bail is also an `Err`, so its message is checked.
fn reachable(outcome: &Result<Option<WinApiHandlerResult>, String>) -> bool {
    match outcome {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(message) => !message.contains(UNSUPPORTED_MARKER),
    }
}

/// Probes deliberately skipped because the handler BLOCKS on the zeroed
/// arguments, and so would hang the sweep rather than report.
///
/// A general per-probe timeout was rejected on purpose: it would also swallow a
/// genuine hang in any future handler, turning this guard into a slow no-op.
/// One row, named and reasoned, is auditable in a way a blanket timeout is not.
///
/// `ws2_32!select` is the case: `read_select_timeout` maps a NULL `timeval*`
/// (argument 5, zeroed by the probe) to "no deadline", which is exactly what
/// real `select()` does — block until a socket is ready. With no candidate
/// sockets the loop is well-defined and eternal. The export's arm is instead
/// covered by `ws2_32_exports_are_reported_by_the_oracle`, and the presence of
/// the arm is a compile-time fact of the `match` in `ws2_32::dispatch_ws2`.
const BLOCKING_PROBES: &[(&str, &str, &str)] = &[(
    "ws2_32.dll",
    "select",
    "NULL timeval* means block-forever, which is the real select() contract",
)];

/// Report every unreachable `(library, name)` in one failure message, rather
/// than stopping at the first — a registry-wide drift is usually several rows.
fn assert_all_reachable(library: &str, names: &[&str]) {
    let unreachable: Vec<&str> = names
        .iter()
        .copied()
        .filter(|name| {
            // A FRESH fixture per name, deliberately. `with_font_engine` takes
            // the font engine out of the shared `WinApiState` mutex and puts it
            // back on the normal path only — so a name whose handler panics
            // would strand that mutex and deadlock every later name sharing the
            // state. One fixture per probe costs a few microseconds and keeps
            // the sweep order-independent.
            if BLOCKING_PROBES.iter().any(|(lib, blocked_name, _)| {
                lib.eq_ignore_ascii_case(library) && *blocked_name == *name
            }) {
                return false;
            }
            let mut call = Call::new();
            let outcome = call.run(|ctx, name| dispatch_soft(ctx, library, name), name);
            !reachable(&outcome)
        })
        .collect();
    assert!(
        unreachable.is_empty(),
        "{} census name(s) on {library} have no dispatch arm: {unreachable:?}",
        unreachable.len()
    );
}

/// The per-DLL string-dispatch entry point for `library`, or `None` when the
/// library has no string dispatch (dense-only, or handled by the shared
/// `dispatch_winapi` prologue).
fn dispatch_soft(
    ctx: &mut HandlerContext<'_>,
    library: &str,
    name: &str,
) -> Result<Option<WinApiHandlerResult>, String> {
    // The result type is `anyhow::Result` in the handlers; this wrapper keeps
    // the error as a `String` so the guard never has to name the error type.
    fn e(
        r: anyhow::Result<Option<WinApiHandlerResult>>,
    ) -> Result<Option<WinApiHandlerResult>, String> {
        r.map_err(|err| err.to_string())
    }
    match library {
        "kernel32.dll" => e(crate::kernel32::dispatch_kernel32_extra(ctx, name)),
        "ole32.dll" => e(crate::ole32::dispatch_ole32(ctx, name)),
        "shell32.dll" => e(crate::shell32::dispatch_shell32(ctx, name)),
        "oleaut32.dll" => e(crate::oleaut32::dispatch_oleaut32(ctx, name)),
        "user32.dll" => e(crate::user32::dispatch_user32_extra(ctx, name)),
        "gdi32.dll" => e(crate::gdi32::dispatch_gdi32_extra(ctx, name)),
        "comctl32.dll" => e(crate::comctl32::dispatch_comctl32_extra(ctx, name)),
        "winmm.dll" => e(crate::winmm::dispatch_winmm_extra(ctx, name)),
        "advapi32.dll" => e(crate::advapi32::dispatch_advapi32_extra(ctx, name)),
        "ws2_32.dll" => e(crate::ws2_32::dispatch_ws2(ctx, name)),
        "crypt32.dll" => e(crate::crypt32::dispatch_crypt32(ctx, name)),
        "msimg32.dll" => e(crate::msimg32::dispatch_msimg32(ctx, name)),
        "imm32.dll" => e(crate::imm32::dispatch_imm32(ctx, name)),
        "uxtheme.dll" => e(crate::uxtheme::dispatch_uxtheme(ctx, name)),
        "winhttp.dll" => e(crate::winhttp::dispatch_winhttp(ctx, name)),
        "setupapi.dll" | "cfgmgr32.dll" => e(crate::setupapi::dispatch_setupapi(ctx, name)),
        "dbghelp.dll" | "imagehlp.dll" => e(crate::dbghelp::dispatch_dbghelp(ctx, name)),
        "wininet.dll" => e(crate::wininet::dispatch_wininet(ctx, name)),
        "urlmon.dll" => e(crate::urlmon::dispatch_urlmon(ctx, name)),
        "ntdll.dll" => e(crate::ntdll::dispatch_ntdll(ctx, name)),
        "opengl32.dll" => e(crate::opengl32::dispatch_opengl32(ctx, name)),
        // UCRT is namespace-wide by export name: the family classification
        // lives in `is_ucrt_library`, and `dispatch_ucrt` takes the name alone.
        "ucrtbase.dll" | "api-ms-win-crt-*.dll" => {
            e(crate::ucrt::dispatch_ucrt(ctx, name).map(Some))
        }
        // dinput8 has two entry points (the `DirectInput8Create` export and the
        // two COM interfaces); a census name reaching either is reachable.
        "dinput8.dll" => {
            if let Ok(Some(r)) = e(crate::dinput::dispatch_dinput8(ctx, name)) {
                return Ok(Some(r));
            }
            e(crate::dinput::dispatch_object_method(ctx, name))
        }
        other => panic!("{other} is in SOFT_EXPORT_CENSUS but has no dispatch_soft arm"),
    }
}

// ── The soft export census lists ───────────────────────────────────────

/// Every name in every `*_DLL_EXPORTS` census list resolves to a handler.
///
/// This is the headline contract: a name in a census list that no dispatch arm
/// claims is exactly the drift that silently degrades `wie inspect` without
/// breaking a single guest run.
#[test]
fn every_census_name_has_a_dispatch_arm() {
    for (library, names) in SOFT_EXPORT_CENSUS {
        assert!(!names.is_empty(), "{library} census list is empty");
        assert_all_reachable(library, names);
    }
}

/// The census table and the oracle must agree on *coverage*: every census name
/// must be reported implemented for its library. This is the direction the
/// oracle exists for, and it is the half `is_winapi_implemented` can check on
/// its own.
#[test]
fn every_census_name_is_reported_by_the_oracle() {
    for (library, names) in SOFT_EXPORT_CENSUS {
        for name in *names {
            assert!(
                crate::is_winapi_implemented(library, name),
                "{library}!{name} is in the census but the oracle reports it unimplemented"
            );
            assert!(
                crate::is_winapi_library(library),
                "{library} is in the census table but is not classified as a WinAPI library"
            );
        }
    }
}

/// The skip list must not become a hiding place: every row names a library
/// that exists and a name that is actually in that library's census.
#[test]
fn the_blocking_probe_skip_list_stays_minimal() {
    assert!(
        BLOCKING_PROBES.len() <= 4,
        "BLOCKING_PROBES has {} rows; a growing skip list makes this guard progressively less trustworthy",
        BLOCKING_PROBES.len()
    );
    for (library, name, reason) in BLOCKING_PROBES {
        let in_census = SOFT_EXPORT_CENSUS
            .iter()
            .any(|(lib, names)| lib.eq_ignore_ascii_case(library) && names.contains(name));
        assert!(
            in_census,
            "{library}!{name} is skipped but is not in any census list"
        );
        assert!(!reason.is_empty(), "{library}!{name} needs a stated reason");
    }
}

/// No duplicates within a census list — a repeated row is harmless at runtime
/// but hides a merge mistake and inflates the coverage numbers above.
#[test]
fn census_lists_have_no_duplicate_names() {
    for (library, names) in SOFT_EXPORT_CENSUS {
        let mut sorted = names.to_vec();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(
            before,
            sorted.len(),
            "{library} census list has duplicate names"
        );
    }
}

// ── The per-DLL census lists with their own oracles ────────────────────

/// `ntdll::NTDL_EXPORTS` — every reported ntdll export dispatches.
///
/// (The in-module twin of this lives in `ntdll/tests.rs`; this row is here so
/// the per-DLL lists are covered by ONE guard a reader can find.)
#[test]
fn ntdll_census_names_all_dispatch() {
    assert_all_reachable("ntdll.dll", crate::ntdll::NTDL_EXPORTS);
}

/// `urlmon` — every export its own oracle reports dispatches.
#[test]
fn urlmon_reported_exports_all_dispatch() {
    let names: Vec<&str> = crate::urlmon::URLMON_EXPORTS
        .iter()
        .map(|(export, _)| *export)
        .collect();
    assert!(!names.is_empty());
    assert_all_reachable("urlmon.dll", &names);
}

/// `wininet` — every export its own oracle reports dispatches.
#[test]
fn wininet_reported_exports_all_dispatch() {
    let names: Vec<&str> = crate::wininet::WININET_EXPORTS
        .iter()
        .map(|(export, _)| *export)
        .collect();
    assert!(!names.is_empty());
    assert_all_reachable("wininet.dll", &names);
}

/// `opengl32` — every name in its export-name list dispatches.
///
/// This is the registry with the most rows by far (134 of the 156
/// `PREPLANTED_SOFT_APIS` rows are opengl32), so it is where a census gap is
/// most likely to hide.
#[test]
fn opengl32_export_names_all_dispatch() {
    assert_all_reachable("opengl32.dll", crate::opengl32::OPENGL32_EXPORT_NAMES);
}

// ── The dynamic-API registries ─────────────────────────────────────────

/// Every `PREPLANTED_SOFT_APIS` row must resolve: these are the soft slots
/// planted into the runtime's table, so a row with no handler hands the guest
/// a fake VA that decodes to an unimplemented name.
#[test]
fn every_preplanted_soft_api_dispatches() {
    let unreachable: Vec<(&str, &str)> = crate::PREPLANTED_SOFT_APIS
        .iter()
        .filter(|entry| {
            // Fresh fixture per row — see `assert_all_reachable` for why.
            let mut call = Call::new();
            let outcome = call.run(
                |ctx, name| dispatch_soft(ctx, entry.library, name),
                entry.name,
            );
            !reachable(&outcome)
        })
        .map(|entry| (entry.library, entry.name))
        .collect();
    assert!(
        unreachable.is_empty(),
        "{} PREPLANTED_SOFT_APIS row(s) have no dispatch arm: {unreachable:?}",
        unreachable.len()
    );
}

/// Every `DYNAMIC_FAKE_APIS` row must resolve for the same reason.
#[test]
fn every_dynamic_fake_api_dispatches() {
    let unreachable: Vec<(&str, &str)> = crate::DYNAMIC_FAKE_APIS
        .iter()
        .filter(|entry| {
            let mut call = Call::new();
            let outcome = call.run(
                |ctx, name| dispatch_soft(ctx, entry.library, name),
                entry.name,
            );
            !reachable(&outcome)
        })
        .map(|entry| (entry.library, entry.name))
        .collect();
    assert!(
        unreachable.is_empty(),
        "{} DYNAMIC_FAKE_APIS row(s) have no dispatch arm: {unreachable:?}",
        unreachable.len()
    );
}

// ── The oracle's own shape ─────────────────────────────────────────────

/// Every library in `is_winapi_library` must have an `is_winapi_implemented`
/// arm — otherwise the two hand-kept lists drift and `inspect` reports an
/// implemented DLL's exports as missing.
///
/// Two exception sets are recorded explicitly below so they are visible rather
/// than implicit: libraries whose exports are ALL dense (resolved by
/// `resolve_winapi_id` before the match) and libraries whose census lives in
/// their own module.
#[test]
fn every_winapi_library_has_an_oracle_arm() {
    /// Libraries with no soft census list: their exports resolve through the
    /// dense `WinApiId` table, so `is_winapi_implemented`'s early
    /// `resolve_winapi_id` check covers them and the match needs no arm.
    const DENSE_ONLY: &[&str] = &["d3d9.dll", "version.dll", "comdlg32.dll"];

    /// Libraries whose census lives in their own module rather than in
    /// `names::SOFT_EXPORT_CENSUS`.
    const MODULE_OWN_CENSUS: &[&str] = &["wininet.dll", "urlmon.dll", "ntdll.dll", "opengl32.dll"];
    for library in [
        "kernel32.dll",
        "advapi32.dll",
        "user32.dll",
        "comctl32.dll",
        "comdlg32.dll",
        "gdi32.dll",
        "uxtheme.dll",
        "winmm.dll",
        "shell32.dll",
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
        "dinput8.dll",
        "d3d9.dll",
        "version.dll",
    ] {
        assert!(
            crate::is_winapi_library(library),
            "{library} must classify as a WinAPI library"
        );
        let covered = SOFT_EXPORT_CENSUS
            .iter()
            .any(|(lib, _)| lib.eq_ignore_ascii_case(library))
            // These four keep their census in their OWN module (each pairs the
            // name with its handler, so the list IS the dispatch table) and
            // expose it through a per-DLL `is_export` oracle rather than a
            // `*_DLL_EXPORTS` const in `names`. They are swept by the four
            // `*_census_names_all_dispatch` / `*_reported_exports_all_dispatch`
            // tests above.
            || MODULE_OWN_CENSUS.contains(&library)
            || DENSE_ONLY.contains(&library);
        assert!(
            covered,
            "{library} is classified as a WinAPI library but has neither a soft census \
             list, a MODULE_OWN_CENSUS row, nor a DENSE_ONLY exemption in \
             census_tests::every_winapi_library_has_an_oracle_arm"
        );
    }
}
