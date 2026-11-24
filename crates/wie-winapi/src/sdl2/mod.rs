//! Host handlers for the pixel-moving part of the guest's `SDL2.dll`.
//!
//! # Why SDL2 gets host code at all
//!
//! SDL2 is a *guest* module: WIE maps the real DLL and its blits run as guest
//! instructions. That is usually right — it is the guest's own code. It is
//! wrong for the few entry points whose entire job is a bulk memory transform:
//! a 4-instruction guest loop run once per pixel is ~256K blocks per frame for
//! a 640x480 surface, and it dominates the profile of any palette-era game.
//! Those calls are re-implemented here against SDL2's real ABI, so the work
//! becomes word-wise host stores into the *same* guest buffer the presenter
//! reads — no intermediate surface, no copy back.
//!
//! # The contract: the guest must not be able to tell
//!
//! Every handler either reproduces the call bit-exactly (same pixels, same
//! return value, same surface state) or declines *before touching anything* and
//! tail-jumps to the module's own implementation through
//! [`crate::host_exports::fall_through`]. The decline is the important half:
//! it makes partial coverage safe, so a format, flag combination or SDL
//! internal this module does not model costs speed, never correctness.
//!
//! Declining is also how error paths stay exact: SDL reports failures through
//! `SDL_SetError`, which writes a guest-side string. Rather than reconstruct
//! those strings, every error case falls through and the guest produces its own.
//!
//! # Served exports
//!
//! - [`SDL_LockSurface`](https://wiki.libsdl.org/SDL2/SDL_LockSurface) /
//!   `SDL_UnlockSurface` — the lock counter; pixels are already host memory.
//! - `SDL_FillRect` / `SDL_FillRects` — clipped, pattern-replicated fills.
//! - `SDL_UpperBlit` (what `SDL_BlitSurface` is defined as) / `SDL_LowerBlit` —
//!   `SDL_BlitCopy` and the palette-expanding `Blit1toN[Key]` cases.
//!
//! Not served, on purpose: `SDL_UpdateTexture` / `SDL_LockTexture` /
//! `SDL_UpdateWindowSurface` and anything else that reaches into
//! `SDL_Renderer` / `SDL_Texture` internals. Those structures are renderer
//! -private (a `native`/`yuv`/`driverdata` triangle whose meaning depends on
//! which renderer SDL picked), so a host implementation would be guessing at
//! state it does not own. They stay guest code; the profile shows what is left.

pub(crate) mod blit;
pub(crate) mod fill;
pub(crate) mod surface;

use anyhow::{Context, Result};
use wie_cpu::CpuEngine;

use crate::gdi32::{ArgReg, read_arg};
use crate::host_exports::fall_through;
use crate::{HandlerContext, WinApiHandlerResult};

/// Library name as it appears in the guest import table.
pub(super) const LIBRARY: &str = "SDL2.dll";

/// The exports this module serves.
///
/// Order is alphabetical; lookup is a linear case-insensitive scan, called once
/// per import (not per call). Every name here must also have an arm in
/// [`dispatch`] — the census test at the bottom of this file is the guard.
pub(crate) const EXPORTS: &[&str] = &[
    "SDL_FillRect",
    "SDL_FillRects",
    "SDL_LockSurface",
    "SDL_LowerBlit",
    "SDL_UnlockSurface",
    "SDL_UpperBlit",
];

/// Is `library!name` a guest-DLL export served here?
#[must_use]
pub(crate) fn is_host_export(library: &str, name: &str) -> bool {
    library.eq_ignore_ascii_case(LIBRARY) && EXPORTS.iter().any(|e| e.eq_ignore_ascii_case(name))
}

/// Read one Win64 integer argument register.
pub(super) fn arg(engine: &mut dyn CpuEngine, reg: ArgReg, api: &str) -> Result<u64> {
    read_arg(engine, reg, api)
}

/// Read up to 1 KiB of guest bytes for a table or row the handler needs.
///
/// `Ok(None)` when `len` exceeds the cap (the caller falls through rather than
/// allocating an unbounded buffer from a guest-supplied length).
pub(super) fn read_scratch(
    engine: &mut dyn CpuEngine,
    va: u64,
    len: usize,
) -> Result<Option<Vec<u8>>> {
    const CAP: usize = 1024;
    if len > CAP {
        return Ok(None);
    }
    let mut bytes = vec![0_u8; len];
    engine
        .mem_read(va, &mut bytes)
        .with_context(|| format!("failed to read {len} guest bytes for an SDL2 handler"))?;
    Ok(Some(bytes))
}

/// Dispatch one `SDL2.dll` export served by a host handler.
///
/// `Ok(None)` means "not ours" — the caller reports the usual unsupported-API
/// error, exactly as before this module existed.
pub(crate) fn dispatch(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let lower = name.to_ascii_lowercase();
    let handled = match lower.as_str() {
        "sdl_locksurface" => lock_surface(ctx)?,
        "sdl_unlocksurface" => unlock_surface(ctx)?,
        "sdl_fillrect" => fill::handle_fill_rect(ctx)?,
        "sdl_fillrects" => fill::handle_fill_rects(ctx)?,
        "sdl_upperblit" | "sdl_blitsurface" => blit::handle_upper_blit(ctx)?,
        "sdl_lowerblit" => blit::handle_lower_blit(ctx)?,
        _ => return Ok(None),
    };
    Ok(Some(handled))
}

/// `int SDL_LockSurface(SDL_Surface *surface)`.
///
/// `SDL_LockSurface` exists for RLE-accelerated and hardware surfaces. A plain
/// software surface's pixels are already host memory here, so the entire
/// observable effect is the recursive lock counter.
pub(crate) fn lock_surface(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let surface_va = arg(engine, ArgReg::Rcx, "SDL_LockSurface")?;
    let Some(surface) = surface::read_surface(engine, surface_va)? else {
        // SDL would dereference NULL and fault; let the guest produce that.
        return fall_through(ctx, LIBRARY, "SDL_LockSurface");
    };
    if surface.must_lock() {
        // The lock un-RLEs the surface (`SDL_UnRLESurface`) — guest work that
        // rewrites the pixel buffer.
        return fall_through(ctx, LIBRARY, "SDL_LockSurface");
    }
    surface::write_locked(engine, surface.va, surface.locked.wrapping_add(1))?;
    ctx.finish(0)
}

/// `void SDL_UnlockSurface(SDL_Surface *surface)`.
///
/// Mirror of [`lock_surface`]: SDL only re-encodes on the transition to zero,
/// and only for an RLE surface, so a plain surface just decrements.
pub(crate) fn unlock_surface(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let surface_va = arg(engine, ArgReg::Rcx, "SDL_UnlockSurface")?;
    let Some(surface) = surface::read_surface(engine, surface_va)? else {
        return fall_through(ctx, LIBRARY, "SDL_UnlockSurface");
    };
    if surface.must_lock() {
        return fall_through(ctx, LIBRARY, "SDL_UnlockSurface");
    }
    if surface.locked == 0 {
        // SDL returns immediately; nothing observable happens.
        return ctx.finish(0);
    }
    surface::write_locked(engine, surface.va, surface.locked - 1)?;
    ctx.finish(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every served export must resolve to an arm, and `is_host_export` must be
    /// case-insensitive in both halves. A name added to `EXPORTS` without an
    /// arm would intercept a call and then report "unsupported", which is worse
    /// than not intercepting it at all.
    #[test]
    fn every_served_export_is_dispatchable() {
        for name in EXPORTS {
            assert!(
                is_host_export(LIBRARY, name),
                "{name} must be reported as served"
            );
            assert!(
                is_host_export("sdl2.DLL", &name.to_lowercase()),
                "{name} must match case-insensitively"
            );
            let lower = name.to_ascii_lowercase();
            assert!(
                matches!(
                    lower.as_str(),
                    "sdl_locksurface"
                        | "sdl_unlocksurface"
                        | "sdl_fillrect"
                        | "sdl_fillrects"
                        | "sdl_upperblit"
                        | "sdl_lowerblit"
                ),
                "{name} has no dispatch arm"
            );
        }
        // The neighbouring library must not be captured: SDL2_mixer calls into
        // SDL2 through the same names and is ordinary guest code.
        assert!(!is_host_export("SDL2_mixer.dll", "SDL_FillRect"));
        assert!(!is_host_export(LIBRARY, "SDL_RenderCopy"));
    }
}
