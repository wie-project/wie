//! Guest-DLL exports served by host handlers.
//!
//! # Why this exists
//!
//! A *guest* DLL (SDL2.dll, libogg-0.dll, …) is mapped from disk and its
//! exports resolve to guest code, so its hot inner loops are ordinary guest
//! instructions. Some of those loops are bulk pixel work whose result is a
//! plain memory transform — work the host can do word-wise instead of
//! instruction-by-instruction. That needs a seam: a way to say "this export of
//! this guest module runs on the host", without the guest noticing.
//!
//! This module is that seam, and it is deliberately **not** an SDL special
//! case. [`is_host_export`] consults a registry of `(module, export)` pairs
//! ([`crate::sdl2`] is the only registrant today); everything below is generic
//! over guest modules:
//!
//! - [`record`] remembers, for one module export, the guest's own entry point,
//!   the fake VA the host handler dispatches on, and the module's dispatch
//!   slot when the build has one.
//! - [`arm_and_fall_through`] is the exactness escape hatch: the guest VA a
//!   handler must tail-jump to for the cases it does not reproduce.
//!
//! # Two ways a caller reaches an export
//!
//! A guest DLL has two populations of callers, and an import-table patch only
//! reaches the first:
//!
//! 1. **Other modules** (`doomretro.exe`) import the export: the IAT slot is
//!    whatever the loader wrote. Pointing it at a fake VA intercepts these.
//! 2. **The module itself** (`SDL_render_sw.c` calling `SDL_FillRect`) calls
//!    its own code directly. Nothing in the IAT is involved.
//!
//! Many SDL2-era builds are linked with a *dynamic API* trampoline: every
//! exported name is a stub that reads a writable function pointer out of
//! `.data` and `jmp`s through it, which is why an export can be redirected for
//! intra-module callers too — write the fake VA into that slot and the
//! module's own `call` lands on the host handler. The slot is derived from the
//! guest's own bytes at load time ([`derive_slot`]) rather than from a
//! hard-coded address, so it works for any build with that shape and degrades
//! to "IAT only" for any build without it.
//!
//! # Why the slot value is remembered, not assumed
//!
//! SDL2's trampoline initialises the slot lazily on the first API call: the
//! slot first holds a bootstrap thunk that runs one-time init and *rewrites
//! every slot*. So a planted fake VA can be erased after we wrote it. Rather
//! than guess when init has run, the fall-through path re-reads the slot on
//! every call: a value that is not ours and still points inside the module is
//! the live implementation, and is both remembered as the fall-through target
//! and re-armed. That self-heals within one call of the rewrite, and it can
//! never loop: the stored target is always a guest address we did not write.

use anyhow::{Context, Result};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use wie_cpu::CpuEngine;

use crate::guest_memory::{read_u64, write_u64};
use crate::{HandlerContext, WinApiHandlerResult, WinApiState};

/// x86-64 `jmp qword ptr [rip+disp32]` — the shape of an export trampoline.
const JMP_QWORD_RIP: [u8; 3] = [0x48, 0xff, 0x25];

/// One guest-module export the host serves.
///
/// Recorded once per `(module, export)` when the import is resolved, so a
/// handler that cannot reproduce a case can still hand control back to the
/// module's own implementation.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HostExportSite {
    /// Guest VA of the module's own entry point for this export (the export
    /// stub from the PE export directory).
    pub export_va: u64,
    /// Fake VA the host handler dispatches on.
    pub fake_va: u64,
    /// Guest base of the module image (bounds check for the dispatch slot).
    pub image_base: u64,
    /// Mapped size of the module image.
    pub image_size: usize,
    /// Guest VA of the module's dispatch slot, when the export entry is a
    /// `jmp qword ptr [rip+disp]` trampoline over a writable pointer. `0` when
    /// the module has no such slot and only import-table callers are served.
    pub slot_va: u64,
    /// Live value of [`Self::slot_va`] before the host planted [`Self::fake_va`]
    /// — the tail-jump target for a fall-through.
    pub slot_original: u64,
    /// Whether this export's *internal* copy must also be intercepted (see
    /// [`install_detour`]): true for an export the module itself calls by
    /// direct address rather than through the dispatch slot.
    pub detour_wanted: bool,
    /// The installed entry patch, once it has been planted.
    pub detour: Option<IntraModuleDetour>,
    /// This site's detour was refused and must not be retried. A refusal is a
    /// property of the prologue, so it cannot become patchable later — retrying
    /// would only re-read the same bytes on every dispatched call.
    pub refused: bool,
    /// For an entry named explicitly rather than derived (see
    /// [`set_internal_entry`]): the exact bytes the audited function starts
    /// with. A patch is planted only if they match, so a build where that RVA
    /// now holds a different function is refused instead of silently hooked.
    pub expected_prologue: Option<[u8; PATCH_LEN]>,
    /// This site is waiting for its module to publish the body behind a thunk.
    /// Distinct from [`Self::refused`] because it can still resolve.
    pub unresolved: bool,
}

/// An entry patch on a **directly called** internal function.
///
/// A guest module that calls its own export by address (`callq 0x1800d8040`
/// from `SDL_PrivateLowerBlitScaled`) never touches the dispatch slot, so
/// planting the fake VA in the slot cannot reach it — the call goes straight to
/// the function body. The only interceptable surface is the body itself:
///
/// ```text
/// entry_va:   48 B8 <fake_va> FF E0      mov rax, fake; jmp rax   (12 bytes)
///             ↓ fall-through
/// trampoline: <the 12 original bytes>    the prologue we displaced
///             48 B8 <entry+12> FF E0      then continue in the function
/// ```
///
/// The trampoline lives in the module's own image tail, which the loader maps
/// `RwxPerms::ALL` in the same page class as every other guest code byte —
/// nothing new is mapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IntraModuleDetour {
    /// Guest VA of the patched function entry.
    pub entry_va: u64,
    /// Guest VA of the relocated prologue (+ the jump back into the function).
    pub trampoline_va: u64,
}

impl HostExportSite {
    /// Whether `va` points inside the module image (a plausible code pointer).
    #[must_use]
    fn contains(&self, va: u64) -> bool {
        let size = u64::try_from(self.image_size).unwrap_or(u64::MAX);
        va >= self.image_base && va < self.image_base.saturating_add(size)
    }
}

/// Is `library!name` a guest-DLL export that a host handler serves?
///
/// The registry is [`crate::sdl2`]'s export list today. Adding another guest
/// module means adding another arm here — the mechanism itself (recording a
/// site, planting the dispatch slot, falling through) is library-agnostic.
///
/// `WIE_HOST_EXPORTS=0` (or the older `WIE_SDL2_NATIVE=0`) turns every host
/// export off, which is the A/B switch: the same scene, twice, once with the
/// handlers and once with none. It has to be a knob rather than a build flag
/// because the comparison is a *rendered frame*, and the only way to know the
/// handlers agree with SDL is to diff two frames the guest produced.
#[must_use]
pub(crate) fn is_host_export(library: &str, name: &str) -> bool {
    if !enabled() {
        return false;
    }
    crate::sdl2::is_host_export(library, name)
}

/// Whether host exports are enabled at all (see [`is_host_export`]).
///
/// Read once: it decides how every import is resolved and cannot change
/// mid-session.
#[must_use]
pub(crate) fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        let value = std::env::var("WIE_HOST_EXPORTS")
            .or_else(|_| std::env::var("WIE_SDL2_NATIVE"))
            .ok();
        knob_value(value.as_deref())
    })
}

/// Whether the handlers should run their native work *and* let the guest do
/// the same work, so the two can be compared (`WIE_HOST_EXPORTS=shadow`).
///
/// In this mode a handler computes its result, stashes it, and then declines, so
/// the guest's own code produces the pixels the user sees. That makes the
/// comparison deterministic and frame-preserving at the same time: same inputs,
/// same process, no clock.
#[must_use]
pub(crate) fn shadow_mode() -> bool {
    static SHADOW: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SHADOW.get_or_init(|| {
        let value = std::env::var("WIE_HOST_EXPORTS")
            .or_else(|_| std::env::var("WIE_SDL2_NATIVE"))
            .ok();
        value.is_some_and(|value| value.trim().eq_ignore_ascii_case("shadow"))
    })
}

/// Whether a knob value means "enabled". Unset means enabled; only an explicit
/// false spelling disables.
#[must_use]
fn knob_value(value: Option<&str>) -> bool {
    match value {
        Some(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "off" | "false" | "no"
        ),
        None => true,
    }
}

/// Table key for one guest-module export (`"sdl2.dll!sdl_fillrect"`).
fn key(library: &str, name: &str) -> String {
    let mut out = String::with_capacity(library.len() + name.len() + 1);
    for c in library.chars() {
        out.push(c.to_ascii_lowercase());
    }
    out.push('!');
    for c in name.chars() {
        out.push(c.to_ascii_lowercase());
    }
    out
}

/// Remember that the host serves `library!name`, and try to arm its slot.
///
/// Called from the two places a guest export can be resolved: static (IAT)
/// import patching and `GetProcAddress`. Both already have the module loaded,
/// so the site is recorded with everything a handler needs later.
///
/// Arming here is best-effort: SDL2's one-time init may rewrite the slot
/// afterwards, and [`arm_and_fall_through`] re-arms on the next call.
pub(crate) fn record(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
    library: &str,
    name: &str,
    fake_va: u64,
) {
    let Some(module) = find_module(state, library) else {
        return;
    };
    let (image_base, image_size, export_va) = (
        module.image_base,
        module.image_size,
        module.get_export_va(name).unwrap_or(0),
    );
    let mut site = HostExportSite {
        export_va,
        fake_va,
        image_base,
        image_size,
        slot_va: 0,
        slot_original: 0,
        detour_wanted: crate::sdl2::detours_its_internal_copy(name),
        detour: None,
        refused: false,
        expected_prologue: None,
        unresolved: false,
    };
    arm(engine, &mut site);
    state
        .module_state
        .host_exports
        .insert(key(library, name), site);
}

/// Point a recorded site at an **explicitly named** in-module entry.
///
/// Some internal functions are reachable from neither an import table nor an
/// export thunk: SDL2's `SDL_SoftStretch` body at `+0xd8040` is called by a
/// direct `callq` from `SDL_PrivateLowerBlitScaled`, while the *exported*
/// `SDL_SoftStretch` is a dynapi thunk over a slot holding a generated wrapper
/// somewhere else in the image. The audited body therefore has to be named, and
/// naming an address is only safe with a guard: [`Self::expected_prologue`]
/// pins the bytes it must start with, so a different build refuses the patch
/// rather than hooking whatever now lives at that RVA.
pub(crate) fn set_internal_entry(
    state: &mut WinApiState,
    library: &str,
    name: &str,
    entry_va: u64,
    expected_prologue: [u8; PATCH_LEN],
) {
    let k = key(library, name);
    let Some(mut site) = state.module_state.host_exports.get(&k).copied() else {
        return;
    };
    site.export_va = entry_va;
    // The named entry is called directly; any slot the *export* had is a
    // different code path and is left as the loader armed it.
    site.slot_va = 0;
    site.slot_original = 0;
    site.detour = None;
    site.refused = false;
    site.unresolved = false;
    site.expected_prologue = Some(expected_prologue);
    state.module_state.host_exports.insert(k, site);
}

/// Resolve `library!name` to the fake VA a host handler dispatches on, using
/// the session's import resolver, and record the site.
///
/// The `GetProcAddress` half of the interception: a guest that loads a module
/// and looks a symbol up at run time must get the same answer as its static
/// imports did.
///
/// # Errors
///
/// The session has no import resolver (only test fixtures), or the resolver
/// refused the name.
pub(crate) fn resolve_fake_va(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
    library: &str,
    name: &str,
) -> Result<Option<u64>> {
    if let Some(site) = state.module_state.host_exports.get(&key(library, name)) {
        return Ok(Some(site.fake_va));
    }
    // `resolve` takes `&mut self`; cloning the handle keeps `state` free for
    // `record` below (the resolver body holds no state of its own).
    let Some(mut resolver) = state.module_state.import_resolver.clone() else {
        return Ok(None);
    };
    let fake_va = resolver
        .resolve(library, name, 0)
        .context("failed to resolve a host-export fake VA")?;
    record(engine, state, library, name, fake_va);
    Ok(Some(fake_va))
}

/// The export's tail-jump target, re-arming the dispatch slot if the module
/// rewrote it.
///
/// # Errors
///
/// The site was never recorded (a handler reached without an intercepted
/// import) — the caller must not be able to fall through, so it is an error
/// rather than a silent return to the wrong address.
pub(crate) fn arm_and_fall_through(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
    library: &str,
    name: &str,
) -> Result<u64> {
    let k = key(library, name);
    let Some(mut site) = state.module_state.host_exports.get(&k).copied() else {
        anyhow::bail!("host export {library}!{name} was never resolved; no fall-through target");
    };
    // Re-read the slot: SDL2's one-time dynapi init (and an SDL_DYNAMIC_API
    // override) rewrite every slot after we may have planted ours.
    if site.slot_va != 0
        && let Ok(current) = read_u64(engine, site.slot_va)
        && current != site.fake_va
        && site.contains(current)
    {
        site.slot_original = current;
        write_u64(engine, site.slot_va, site.fake_va)
            .context("failed to re-arm a guest export dispatch slot")?;
        state.module_state.host_exports.insert(k.clone(), site);
    }
    // The module's own init (SDL2's dynapi) rewrites the slots on the first API
    // call, which is the first moment a slot holds the *real* body address —
    // and the real body is what a direct intra-module `call` targets. This is
    // therefore the earliest point at which a detour can be planted, and it is
    // paid once per module.
    install_wanted_detours(engine, state);
    // Re-read: the install above may have *just* planted this site's patch, and
    // the copy taken before it still says `detour: None`. Falling through on the
    // stale copy would return the entry we have just overwritten with a jump
    // back to this handler — infinite recursion, not a slow path.
    let site = state.module_state.host_exports.get(&k).copied();
    // A detoured site resumes in the relocated prologue: the function's own
    // entry is now the patch that sent us here, so jumping back to it would
    // loop. The trampoline replays the displaced bytes and rejoins the body.
    if let Some(detour) = site.and_then(|site| site.detour) {
        return Ok(detour.trampoline_va);
    }
    if site.is_none_or(|site| site.slot_va == 0) {
        return Ok(site.map_or(0, |site| site.export_va));
    }
    Ok(site.map_or(0, |site| site.slot_original))
}

/// Tail-jump a handler to the guest implementation it declined to reproduce.
///
/// A tail jump, not a call: the guest's argument registers and return address
/// are untouched (the handler has not popped the return address), so the
/// module's own code returns to the original caller exactly as if the host had
/// never been involved. `return_address` carries the new RIP so the profile's
/// journal entry names where control actually went.
///
/// # Errors
///
/// No recorded site (see [`arm_and_fall_through`]).
pub(crate) fn fall_through(
    ctx: &mut HandlerContext<'_>,
    library: &str,
    name: &str,
) -> Result<WinApiHandlerResult> {
    let target = arm_and_fall_through(ctx.engine, ctx.state, library, name)?;
    let rax = ctx
        .engine
        .read_rax()
        .context("failed to read RAX before a host-export fall-through")?;
    // The guest `ret`s to the caller's address; we only redirect RIP.
    ctx.engine
        .write_rip(target)
        .context("failed to redirect RIP to a guest export implementation")?;
    Ok(WinApiHandlerResult {
        return_address: target,
        return_value: rax,
    })
}

/// The loaded module for `library` (by normalised base name).
fn find_module<'a>(
    state: &'a WinApiState,
    library: &str,
) -> Option<&'a crate::dll_loader::LoadedModule> {
    let norm = crate::dll_loader::normalize_dll_name(library);
    state.module_state.loaded_modules.get(&norm)
}

/// Derive and plant the module's dispatch slot for `site`, if it has one.
///
/// Structure read from the guest's own image, not assumed:
///
/// ```text
/// export_va: 48 FF 25 <disp32>       jmp qword ptr [rip+disp32]
///              ↓
/// slot_va:   <qword>                 live implementation pointer
/// ```
///
/// The slot is only armed when it already holds a pointer *inside the module*
/// — a writable slot that does not look like a code pointer (relocation not
/// applied, a data field, a non-trampoline export) is left alone. That check
/// is what makes this safe on a build whose exports are plain function bodies:
/// there is no slot to find, so nothing is written.
fn arm(engine: &mut dyn CpuEngine, site: &mut HostExportSite) {
    if site.slot_va == 0
        && let Some(slot_va) = derive_slot(engine, site.export_va)
        && slot_va != 0
        && let Ok(current) = read_u64(engine, slot_va)
        && current != site.fake_va
        && site.contains(current)
    {
        site.slot_va = slot_va;
        site.slot_original = current;
    }
    if site.slot_va == 0 {
        return;
    }
    // Keep the slot pointing at the host handler. A write failure leaves the
    // slot alone: the guest keeps its own implementation, which is correct —
    // just not accelerated.
    drop(write_u64(engine, site.slot_va, site.fake_va));
}

/// Bytes of a guest function entry replaced by the detour jump.
const PATCH_LEN: usize = 12;
/// `mov rax, imm64; jmp rax` — the same abs64 jump `guest_rewire` plants.
const JMP_ABS64_LEN: usize = 12;
/// Space reserved per trampoline in the module's image tail.
const TRAMPOLINE_STRIDE: u64 = 32;
/// How many tail slots to try before giving up (`SDL2.dll`'s tail is ~6.7 KB,
/// so this is a small fraction of it).
const TRAMPOLINE_SCAN: u64 = 8;

/// Entry patches planted, and sites whose prologue was refused.
///
/// The report question "does the interception reach the module's own caller?"
/// is otherwise unanswerable: a planted detour and a refused one both leave the
/// guest working, and if the module's block is never reached at all the two are
/// indistinguishable from frame counts. `planted == 0` with a large `refused`
/// means the mechanism is reaching a prologue it cannot relocate; both zero
/// means the site was never even recorded.
static PLANTED: AtomicU64 = AtomicU64::new(0);
static REFUSED: AtomicU64 = AtomicU64::new(0);
static UNRESOLVED: AtomicU64 = AtomicU64::new(0);
/// Why the last refused site was refused, as `"<entry>: <reason>"` per
/// candidate. A refusal that names an address is diagnosable; "refused" is not.
static LAST_REFUSAL: Mutex<Option<String>> = Mutex::new(None);

/// Why the last detour was refused, for the profile report.
#[must_use]
pub(crate) fn last_refusal() -> Option<String> {
    LAST_REFUSAL.lock().ok().and_then(|last| last.clone())
}

/// `(planted, refused, unresolved)` intra-module entry patches, for the profile
/// report. `unresolved` means a site is waiting for its module to publish the
/// body behind a dispatch thunk — pending, not refused.
#[must_use]
pub(crate) fn detour_tally() -> (u64, u64, u64) {
    (
        PLANTED.load(Ordering::Relaxed),
        REFUSED.load(Ordering::Relaxed),
        UNRESOLVED.load(Ordering::Relaxed),
    )
}

/// The in-image addresses this module's own code can reach for the export, in
/// the order they are worth patching.
///
/// The export entry comes first, and getting this order right is what makes the
/// mechanism reach anything: a `call` inside the module targets the export's own
/// VA, and the export the detour exists for is by definition one the guest
/// neither imports nor looks up — so it has **no dispatch slot**, and deriving
/// the patch target from the slot makes the whole path unreachable for exactly
/// the case it was added for.
///
/// The slot body is the fallback for an export whose entry is a jump thunk
/// (`jmp qword ptr [rip+disp]`, which the relocator must refuse because the
/// displacement is address-dependent). Both are candidates rather than one
/// choice, because only the prologue bytes say whether an entry is patchable.
fn detour_entries(engine: &mut dyn CpuEngine, site: &HostExportSite) -> [Option<u64>; 2] {
    let mut entries = [None, None];
    if site.export_va != 0 && site.contains(site.export_va) {
        entries[0] = Some(site.export_va);
    }
    // The thunk's target is only meaningful once the module has initialised its
    // dispatch table, which happens *after* the export is recorded: SDL2's
    // `SDL_SoftStretch` entry is `jmp qword ptr [rip+disp]` over a slot in its
    // dynapi arena, and that slot still holds zero at load time. Trusting the
    // value seen at record time leaves the body unknown and the whole site
    // unpatchable, so it is re-read here, on the first served call.
    let body = if site.slot_original != 0 && site.contains(site.slot_original) {
        Some(site.slot_original)
    } else {
        derive_slot(engine, site.export_va)
            .and_then(|slot| read_u64(engine, slot).ok())
            .filter(|current| site.contains(*current))
    };
    if let Some(body) = body
        && entries[0] != Some(body)
    {
        entries[1] = Some(body);
    }
    entries
}

/// Plant the entry patches for every site that asked for one and has none yet.
///
/// Called from the fall-through path, which is the first moment a slot can hold
/// a real body address (the module's own init rewrites the slots before its
/// first API call). Every failure is a refusal, not an error: patching guest
/// code is an optimisation and an unpatched function still works, so a module
/// whose prologue cannot be relocated keeps its own implementation.
fn install_wanted_detours(engine: &mut dyn CpuEngine, state: &mut WinApiState) {
    let pending: Vec<String> = state
        .module_state
        .host_exports
        .iter()
        .filter(|(_, site)| site.detour_wanted && site.detour.is_none() && !site.refused)
        .map(|(key, _)| key.clone())
        .collect();
    let mut tail_used: ahash::HashMap<u64, u64> = ahash::HashMap::default();
    for k in pending {
        let Some(mut site) = state.module_state.host_exports.get(&k).copied() else {
            continue;
        };
        // Try each reachable entry in preference order and take the first whose
        // prologue actually relocates. A slot can also move twice — SDL2's
        // bootstrap thunk first, the real body after its one-time init — so a
        // site already patched at one of them is re-planted at the other when
        // that is where the live body ended up.
        let entries = detour_entries(engine, &site);
        // Whether the body behind a thunk entry is known yet. `SDL_SoftStretch`
        // is exported as `jmp qword ptr [rip+disp]` over a slot in SDL's dynapi
        // arena, and that arena is published by SDL's own init — which can run
        // after the first served call. Until it is, the only candidate is a
        // thunk the relocator refuses by design, and that refusal says nothing
        // about the body.
        let body_known = entries[1].is_some();
        let mut planted = None;
        let mut reasons: Vec<String> = Vec::new();
        for entry_va in entries.into_iter().flatten() {
            if site
                .detour
                .is_some_and(|detour| detour.entry_va == entry_va)
            {
                continue;
            }
            let used = tail_used.entry(site.image_base).or_insert(0);
            match plant_detour(engine, &site, entry_va, *used) {
                Ok(detour) => {
                    // Advance past whichever tail slot was actually used.
                    let spent = site
                        .image_base
                        .saturating_add(site.image_size as u64)
                        .saturating_sub(detour.trampoline_va)
                        / TRAMPOLINE_STRIDE;
                    *used = (*used).max(spent);
                    planted = Some(detour);
                    break;
                }
                Err(reason) => reasons.push(reason),
            }
        }
        let Some(detour) = planted else {
            if body_known {
                // A refusal is final for this site: retrying it on every
                // dispatched call would re-read a prologue we have already
                // proved unpatchable.
                site.refused = true;
                if let Ok(mut last) = LAST_REFUSAL.lock() {
                    *last = Some(reasons.join("; "));
                }
                REFUSED.fetch_add(1, Ordering::Relaxed);
                state.module_state.host_exports.insert(k, site);
            } else if !site.unresolved {
                // Still pending, and counted once so the report can say which
                // of the two it is. Retrying costs two memo reads per call. The
                // reason is recorded even here: an unresolved site is usually a
                // refusing candidate behind an unknown body, and losing that
                // message makes the next run as blind as this one was.
                if let Ok(mut last) = LAST_REFUSAL.lock() {
                    *last = Some(reasons.join("; "));
                }
                site.unresolved = true;
                UNRESOLVED.fetch_add(1, Ordering::Relaxed);
                state.module_state.host_exports.insert(k, site);
            }
            continue;
        };
        site.detour = Some(detour);
        state.module_state.host_exports.insert(k, site);
        PLANTED.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            entry = detour.entry_va,
            trampoline = detour.trampoline_va,
            "planted an intra-module detour"
        );
    }
}

/// Plant any pending detours now (the SDL dispatcher calls this once per served
/// call, so the patch is in place before the module's own first direct call).
pub(crate) fn install_pending_detours(engine: &mut dyn CpuEngine, state: &mut WinApiState) {
    install_wanted_detours(engine, state);
}

/// Relocate `entry_va`'s prologue and patch its entry.
///
/// `entry_va` is the address the module's own code calls (see
/// [`detour_entry`]); the module's dispatch slot is not involved.
///
/// Returns `None` — leaving the guest's code untouched — when the entry is not
/// patchable: too few relocatable bytes, an instruction this module does not
/// decode, a branch, a RIP-relative operand, or no free trampoline space. A
/// wrong relocation would corrupt the guest silently, so every one of those is
/// a refusal.
fn plant_detour(
    engine: &mut dyn CpuEngine,
    site: &HostExportSite,
    entry_va: u64,
    index: u64,
) -> std::result::Result<IntraModuleDetour, String> {
    // Never patch an address the module does not own (a jump into another
    // module, a thunk in a shared arena).
    if !site.contains(entry_va) {
        return Err(format!("{entry_va:#x}: outside the module image"));
    }
    let mut prologue = [0_u8; PATCH_LEN];
    engine
        .mem_read(entry_va, &mut prologue)
        .map_err(|error| format!("{entry_va:#x}: unreadable prologue ({error})"))?;
    if let Some(expected) = site.expected_prologue
        && expected != prologue
    {
        return Err(format!(
            "{entry_va:#x}: prologue {prologue:02x?} is not the audited function \
             ({expected:02x?})"
        ));
    }
    let relocatable = relocatable_prologue(&prologue)
        .ok_or_else(|| format!("{entry_va:#x}: undecodable prologue"))?;
    if relocatable != PATCH_LEN {
        return Err(format!(
            "{entry_va:#x}: prologue has {relocatable} of {PATCH_LEN} relocatable bytes \
             (first bytes {prologue:02x?})"
        ));
    }
    // Trampolines are taken from the top of the image, downwards: the tail of a
    // mapped image is page padding past the last section, so it belongs to no
    // section and holds nothing. The all-zero check is what proves that for
    // this image rather than assuming it.
    // Scan downwards for a free slot: one occupied 32-byte slot in the tail must
    // not block every detour in this image. The all-zero check is what keeps the
    // claim "this space belongs to no section" true for each slot chosen.
    let image_top = site
        .image_base
        .checked_add(u64::try_from(site.image_size).map_err(|error| error.to_string())?)
        .ok_or_else(|| format!("{entry_va:#x}: image tail address overflow"))?;
    let mut chosen = None;
    let mut last_problem = String::from("no trampoline space left");
    for offset in 0..TRAMPOLINE_SCAN {
        let reserve =
            TRAMPOLINE_STRIDE.saturating_mul(index.saturating_add(offset).saturating_add(1));
        let Some(candidate) = image_top.checked_sub(reserve) else {
            last_problem = format!("{entry_va:#x}: image tail address overflow");
            break;
        };
        if candidate < site.image_base.saturating_add(0x1000) {
            break;
        }
        let mut existing = [0_u8; TRAMPOLINE_STRIDE as usize];
        match engine.mem_read(candidate, &mut existing) {
            Ok(()) => {}
            Err(error) => {
                last_problem = format!("{candidate:#x}: unreadable trampoline area ({error})");
                continue;
            }
        }
        if existing.iter().all(|byte| *byte == 0) {
            chosen = Some(candidate);
            break;
        }
        last_problem = format!(
            "{candidate:#x}: image tail slot is not empty ({} non-zero bytes)",
            existing.iter().filter(|byte| **byte != 0).count()
        );
    }
    let Some(trampoline_va) = chosen else {
        return Err(format!("{entry_va:#x}: {last_problem}"));
    };
    let mut trampoline = Vec::with_capacity(TRAMPOLINE_STRIDE as usize);
    trampoline.extend_from_slice(&prologue);
    trampoline.extend_from_slice(&jmp_abs64(
        entry_va.checked_add(PATCH_LEN as u64).unwrap_or(0),
    ));
    engine
        .mem_write(trampoline_va, &trampoline)
        .map_err(|error| format!("{trampoline_va:#x}: trampoline write failed ({error})"))?;
    engine
        .mem_write(entry_va, &jmp_abs64(site.fake_va))
        .map_err(|error| format!("{entry_va:#x}: entry patch write failed ({error})"))?;
    Ok(IntraModuleDetour {
        entry_va,
        trampoline_va,
    })
}

/// `mov rax, imm64; jmp rax`.
fn jmp_abs64(target: u64) -> [u8; JMP_ABS64_LEN] {
    let mut bytes = [0_u8; JMP_ABS64_LEN];
    bytes[0] = 0x48; // REX.W
    bytes[1] = 0xb8; // mov rax, imm64
    bytes[2..10].copy_from_slice(&target.to_le_bytes());
    bytes[10] = 0xff;
    bytes[11] = 0xe0; // jmp rax
    bytes
}

/// Length of the relocatable instruction at `bytes[0]`, or `None`.
///
/// Deliberately a whitelist rather than an x86 decoder: it accepts the forms a
/// compiled function prologue is made of (pushes, stack adjustment, register
/// saves into the shadow space, register moves, small immediates) and refuses
/// everything else — including every branch and every RIP-relative operand,
/// which are the two things that cannot be copied to another address unchanged.
#[must_use]
fn relocatable_instruction(bytes: &[u8]) -> Option<usize> {
    let at = |i: usize| bytes.get(i).copied();
    match (at(0)?, at(1)) {
        // push r64
        (0x50..=0x57, _) => Some(1),
        // REX.B + push r64
        (0x41, Some(0x50..=0x57)) => Some(2),
        // nop
        (0x90, _) => Some(1),
        // mov dword [rsp+disp8], imm32 — a shadow-space/local store, the other
        // half of every compiled prologue (and the second half of
        // `SDL_SoftStretch`'s own 12-byte prologue).
        (0xc7, Some(0x44)) => {
            let sib = at(2)?;
            if sib & 0x07 == 0b100 && (sib >> 3) & 0x07 == 0b100 {
                Some(8)
            } else {
                None
            }
        }
        // xor r32, r32 (mod=11 only: a memory operand could be RIP-relative)
        (op @ (0x31 | 0x33), Some(modrm)) if modrm & 0xc0 == 0xc0 => {
            let _ = op;
            Some(2)
        }
        // REX.W-prefixed forms
        (0x48, Some(op)) => match op {
            // sub rsp, imm8 / add rsp, imm8
            0x83 => match at(2)? {
                0xec | 0xc4 => Some(4),
                _ => None,
            },
            // sub rsp, imm32 / add rsp, imm32
            0x81 => match at(2)? {
                0xec | 0xc4 => Some(7),
                _ => None,
            },
            // mov r64, imm32 (C7 /0) or mov r64, imm64 (B8+r)
            0xc7 => match at(2)? {
                0xc0..=0xc7 => Some(7),
                _ => None,
            },
            0xb8..=0xbf => Some(10),
            // mov/lea with a ModRM: only the [rsp+disp8] and register forms.
            0x89 | 0x8b | 0x8d => {
                let modrm = at(2)?;
                let (md, rm) = (modrm >> 6, modrm & 0x07);
                match md {
                    // register-to-register
                    0b11 => Some(3),
                    // [rsp+disp8]: rm=100 (SIB), SIB base = 100 (rsp), index=none
                    0b01 if rm == 0b100 => {
                        let sib = at(3)?;
                        if sib & 0x07 == 0b100 && (sib >> 3) & 0x07 == 0b100 {
                            Some(5)
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// Expose the relocator to the tests that pin its refusals.
#[cfg(test)]
pub(crate) fn relocatable_prologue_for_test(bytes: &[u8]) -> Option<usize> {
    relocatable_prologue(bytes)
}

/// Length of the whole relocatable run at `bytes`, or `None` if any instruction
/// in it is not on the whitelist (or the run stops early).
fn relocatable_prologue(bytes: &[u8]) -> Option<usize> {
    let mut offset = 0_usize;
    while offset < bytes.len() {
        let rest = bytes.get(offset..)?;
        let len = relocatable_instruction(rest)?;
        if len == 0 {
            return None;
        }
        offset = offset.checked_add(len)?;
    }
    (offset == PATCH_LEN).then_some(offset)
}

/// Parse `jmp qword ptr [rip+disp32]` at `va` into the slot address.
fn derive_slot(engine: &mut dyn CpuEngine, va: u64) -> Option<u64> {
    if va == 0 {
        return None;
    }
    // `48 FF 25 disp32` is seven bytes, and the disp is RIP-relative to the
    // *end* of the instruction.
    let mut head = [0_u8; 7];
    engine.mem_read(va, &mut head).ok()?;
    if head[..3] != JMP_QWORD_RIP {
        return None;
    }
    let disp = i32::from_le_bytes([head[3], head[4], head[5], head[6]]);
    let end = va.checked_add(7)?;
    Some(end.wrapping_add_signed(i64::from(disp)))
}

/// Record a site directly, bypassing module loading — test fixtures only.
///
/// The production path reaches [`record`] through the loader or
/// `GetProcAddress`; a handler test has no PE image, so it plants the same
/// record by hand.
#[cfg(test)]
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors HostExportSite field for field; a struct here would only \
              restate the fields a test already names"
)]
pub(crate) fn record_for_test(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
    library: &str,
    name: &str,
    fake_va: u64,
    export_va: u64,
    image_base: u64,
    image_size: usize,
) {
    let mut site = HostExportSite {
        export_va,
        fake_va,
        image_base,
        image_size,
        slot_va: 0,
        slot_original: 0,
        detour_wanted: crate::sdl2::detours_its_internal_copy(name),
        detour: None,
        refused: false,
        expected_prologue: None,
        unresolved: false,
    };
    arm(engine, &mut site);
    state
        .module_state
        .host_exports
        .insert(key(library, name), site);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The A/B switch must be off only when asked in so many words: a typo in
    /// the environment must not silently disable the whole acceleration lane
    /// for a run that then looks mysteriously slow.
    #[test]
    fn only_an_explicit_false_spelling_disables_host_exports() {
        for off in ["0", "off", "FALSE", " no ", "No"] {
            assert!(!knob_value(Some(off)), "{off:?} must disable host exports");
        }
        for on in ["1", "on", "true", "yes", "", "sdl2"] {
            assert!(knob_value(Some(on)), "{on:?} must not disable host exports");
        }
        assert!(knob_value(None), "unset must mean enabled");
    }
}
