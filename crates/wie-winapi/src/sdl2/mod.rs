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
//! The palette-expanding case needs one thing SDL would have done first: a
//! `SDL_LowerBlit` whose source palette was recoloured is a *stale mapping*, and
//! SDL rebuilds it with `SDL_MapSurface` → `Map1toN` before blitting. A guest
//! that animates its palette therefore rebuilds that 256-entry table on every
//! frame. [`palette`] reproduces `Map1toN` in place instead, which is what turns
//! "the blit is stale every frame, so every frame is guest code" into "every
//! frame is host code" — see that module for the exactness argument.
//!
//! Not served, on purpose: `SDL_UpdateTexture` / `SDL_LockTexture` /
//! `SDL_UpdateWindowSurface` and anything else that reaches into
//! `SDL_Renderer` / `SDL_Texture` internals. Those structures are renderer
//! -private (a `native`/`yuv`/`driverdata` triangle whose meaning depends on
//! which renderer SDL picked), so a host implementation would be guessing at
//! state it does not own. They stay guest code; the profile shows what is left.

pub(crate) mod blit;
pub(crate) mod fill;
pub(crate) mod palette;
pub(crate) mod surface;

use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use wie_cpu::CpuEngine;

use crate::gdi32::{ArgReg, read_arg};
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

/// Per-export call counters.
///
/// The report question "did the interception actually fire on this guest's
/// path?" cannot be answered from frame counts — a slow run and a
/// non-intercepting run look identical. These two numbers answer it directly:
/// `calls` counts dispatches into the host handler, `declined` counts the ones
/// the handler handed back to the guest's own implementation. `calls >
/// declined` is host work actually done; `declined == calls` means the
/// interception fires but never accelerates anything.
#[derive(Debug, Default)]
struct ExportStats {
    calls: AtomicU64,
    declined: AtomicU64,
}

impl ExportStats {
    const fn new() -> Self {
        Self {
            calls: AtomicU64::new(0),
            declined: AtomicU64::new(0),
        }
    }

    fn bump(counter: &AtomicU64) {
        // Relaxed: a diagnostic tally, never a synchronisation point.
        let _previous = counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// One counter pair per [`EXPORTS`] entry, in the same order.
static STATS: [ExportStats; EXPORTS.len()] = [const { ExportStats::new() }; EXPORTS.len()];

/// Position of `name` in [`EXPORTS`], accepting the `SDL_BlitSurface` alias
/// (`SDL_surface.h` defines it as `SDL_UpperBlit`, so it shares that handler).
fn export_index(name: &str) -> Option<usize> {
    if let Some(index) = EXPORTS.iter().position(|e| e.eq_ignore_ascii_case(name)) {
        return Some(index);
    }
    if name.eq_ignore_ascii_case("SDL_BlitSurface") {
        return EXPORTS
            .iter()
            .position(|e| e.eq_ignore_ascii_case("SDL_UpperBlit"));
    }
    None
}

/// Hand a call back to the guest's own implementation, tallying the decline.
///
/// Every decline goes through here rather than calling
/// [`crate::host_exports::fall_through`] directly, so the counter cannot drift
/// from the behaviour it describes.
fn declined(ctx: &mut HandlerContext<'_>, name: &str) -> Result<WinApiHandlerResult> {
    if let Some(index) = export_index(name)
        && let Some(stats) = STATS.get(index)
    {
        ExportStats::bump(&stats.declined);
    }
    crate::host_exports::fall_through(ctx, LIBRARY, name)
}

/// Why a blit handler refused a call.
///
/// The counters exist because "it fired and declined every call" and "it never
/// fired" look identical in a frame count, and even `calls/declined` does not
/// say *which* gate refused. Each variant is one `return` path in
/// [`blit::lower_blit`](crate::sdl2::blit), so a run's tally names the exact
/// condition the guest's call failed, and therefore what to implement next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub(super) enum Decline {
    /// The source surface has no `SDL_BlitMap`.
    MapMissing = 0,
    /// `map->dst` is not this destination — SDL is about to rebuild the map.
    MapStale,
    /// A source palette was recoloured since the map was built
    /// (`SDL_SetPaletteColors`) and the rebuild it needs is not one this module
    /// reproduces (a palettized destination, or a map with no table yet).
    PaletteStale,
    /// The *destination* palette was recoloured, so SDL would run `Map1to1`
    /// (nearest colour in the destination palette) — a search, not arithmetic.
    DstPaletteStale,
    /// A format's masks disagree with its losses/shifts, so a table built from
    /// them would be wrong (`Format::channels_are_consistent`).
    FormatInconsistent,
    /// The map's format/pitch mirrors disagree with the surfaces.
    MapMirror,
    /// A pixel format or flag combination outside the reproduced set.
    UnsupportedKind,
    /// A surface whose pixels or pitch this module cannot address directly.
    NotDirect,
    /// The rects do not describe an addressable blit.
    Geometry,
    /// `src->locked` / `dst->locked` — SDL refuses these itself.
    SurfaceLocked,
    /// A NULL surface or rect.
    NullArgument,
    /// A pending stretch (`SDL_COPY_NEAREST`).
    NearestPending,
    /// The rects ask for scaling: SDL would pick a `*Scale` blitter, whose
    /// fixed-point stepping this module does not reproduce.
    Scaled,
}

impl Decline {
    /// Number of variants — the counter array length.
    pub(super) const COUNT: usize = 13;

    /// Stable report label.
    #[must_use]
    const fn label(self) -> &'static str {
        match self {
            Self::MapMissing => "map_missing",
            Self::MapStale => "map_stale",
            Self::PaletteStale => "palette_stale",
            Self::DstPaletteStale => "dst_palette_stale",
            Self::FormatInconsistent => "format_inconsistent",
            Self::MapMirror => "map_mirror",
            Self::UnsupportedKind => "unsupported_kind",
            Self::NotDirect => "not_direct",
            Self::Geometry => "geometry",
            Self::SurfaceLocked => "surface_locked",
            Self::NullArgument => "null_argument",
            Self::NearestPending => "nearest_pending",
            Self::Scaled => "scaled",
        }
    }
}

/// Palette tables rebuilt in place — the count that says the in-place path ran,
/// as opposed to "the call was served and the guest's table happened to be
/// fresh already". Reported only when nonzero.
static TABLE_REBUILDS: AtomicU64 = AtomicU64::new(0);

/// Tally one in-place palette table rebuild.
pub(super) fn note_table_rebuilt() {
    ExportStats::bump(&TABLE_REBUILDS);
}

/// One tally per [`Decline`] variant.
static DECLINE_COUNTS: [AtomicU64; Decline::COUNT] = [const { AtomicU64::new(0) }; Decline::COUNT];

/// Tally one refusal. Called from the handler's own `return` paths.
pub(super) fn note_decline(reason: Decline) {
    if let Some(counter) = DECLINE_COUNTS.get(reason as usize) {
        ExportStats::bump(counter);
    }
}

/// Why a blit handler refused a call — the reason of `Ok(None)`.
pub(super) fn decline_count(reason: Decline) -> u64 {
    DECLINE_COUNTS
        .get(reason as usize)
        .map(|counter| counter.load(Ordering::Relaxed))
        .unwrap_or(0)
}

/// `(export, calls, declined)` for every served export, in [`EXPORTS`] order.
#[must_use]
pub(crate) fn stats() -> Vec<(&'static str, u64, u64)> {
    EXPORTS
        .iter()
        .zip(STATS.iter())
        .map(|(name, stats)| {
            (
                *name,
                stats.calls.load(Ordering::Relaxed),
                stats.declined.load(Ordering::Relaxed),
            )
        })
        .collect()
}

/// Report lines for the `WIE_RUNTIME_PROFILE` block.
///
/// `None` while nothing was dispatched: a guest that never touches SDL2 should
/// not get an SDL2 section in its profile.
#[must_use]
pub fn report_lines() -> Vec<String> {
    if !crate::host_exports::enabled() {
        return vec![
            "sdl2_host_export: disabled (WIE_HOST_EXPORTS=0); SDL2 ran entirely as guest code"
                .to_owned(),
        ];
    }
    let stats = stats();
    if stats.iter().all(|(_, calls, _)| *calls == 0) {
        return Vec::new();
    }
    let rendered = stats
        .iter()
        .filter(|(_, calls, _)| *calls > 0)
        .map(|(name, calls, declined)| format!("{name}={calls}/{declined}"))
        .collect::<Vec<_>>()
        .join(" ");
    let mut lines = vec![format!(
        "sdl2_host_export: calls/declined {rendered} \
         (native = calls - declined; a nonzero declined with zero native means \
         the interception fires but every case fell through)"
    )];
    let reasons = [
        Decline::MapMissing,
        Decline::MapStale,
        Decline::PaletteStale,
        Decline::DstPaletteStale,
        Decline::FormatInconsistent,
        Decline::MapMirror,
        Decline::UnsupportedKind,
        Decline::NotDirect,
        Decline::Geometry,
        Decline::SurfaceLocked,
        Decline::NullArgument,
        Decline::NearestPending,
        Decline::Scaled,
    ]
    .into_iter()
    .filter(|reason| decline_count(*reason) > 0)
    .map(|reason| format!("{}={}", reason.label(), decline_count(reason)))
    .collect::<Vec<_>>();
    if !reasons.is_empty() {
        lines.push(format!(
            "sdl2_blit_declines: {} (the gate each refused blit failed)",
            reasons.join(" ")
        ));
    }
    let rebuilt = TABLE_REBUILDS.load(Ordering::Relaxed);
    if rebuilt > 0 {
        lines.push(format!(
            "sdl2_palette_tables: {rebuilt} rebuilt in place \
             (Map1toN reimplemented host-side; SDL's own rebuild skipped)"
        ));
    }
    lines
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
    let Some(index) = export_index(name) else {
        return Ok(None);
    };
    if let Some(stats) = STATS.get(index) {
        ExportStats::bump(&stats.calls);
    }
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
        return declined(ctx, "SDL_LockSurface");
    };
    if surface.must_lock() {
        // The lock un-RLEs the surface (`SDL_UnRLESurface`) — guest work that
        // rewrites the pixel buffer.
        return declined(ctx, "SDL_LockSurface");
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
        return declined(ctx, "SDL_UnlockSurface");
    };
    if surface.must_lock() {
        return declined(ctx, "SDL_UnlockSurface");
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
