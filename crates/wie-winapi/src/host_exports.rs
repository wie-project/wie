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
#[must_use]
pub(crate) fn is_host_export(library: &str, name: &str) -> bool {
    crate::sdl2::is_host_export(library, name)
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
    };
    arm(engine, &mut site);
    state
        .module_state
        .host_exports
        .insert(key(library, name), site);
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
        state.module_state.host_exports.insert(k, site);
    }
    if site.slot_va == 0 {
        return Ok(site.export_va);
    }
    Ok(site.slot_original)
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
    };
    arm(engine, &mut site);
    state
        .module_state
        .host_exports
        .insert(key(library, name), site);
}
