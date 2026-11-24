//! The palette expansion table, rebuilt in place.
//!
//! # Why the host rebuilds it at all
//!
//! `SDL_LowerBlit` treats a mapping whose `src_palette_version` does not match
//! the source palette's `version` as stale and calls `SDL_MapSurface`, which
//! rebuilds the 256-entry expansion table (`Map1toN`) and then blits. A guest
//! that recolours its palette every frame — as a palette-animated game does —
//! therefore pays for a table rebuild *and* a 256K-pixel per-pixel loop on
//! every frame.
//!
//! The host can do both, but only if it reproduces the table exactly: a wrong
//! mask or shift changes every colour in the frame. So this is a direct port of
//! `src/video/SDL_pixels.c`'s `Map1toN` plus `src/video/SDL_blit.h`'s
//! `ASSEMBLE_RGBA`, including its two asymmetries:
//!
//! - the channel value is `(palette_byte * mod) / 255`, truncating, per channel,
//!   with the mods taken from the *source surface's* `map->info.r/g/b/a`;
//! - for a 3-byte destination `ASSEMBLE_RGBA` writes the **raw** R/G/B bytes at
//!   `Rshift / 8` and friends, applying no `loss` and no mask, and drops alpha
//!   entirely — unlike the 1/2/4-byte cases, which pack
//!   `(c >> loss) << shift`.
//!
//! Entries at or beyond `palette->ncolors` are left alone: SDL's table came
//! from `SDL_calloc`, and `Map1toN` only writes the entries it has colours for.
//!
//! # No copy
//!
//! The table is rebuilt where SDL keeps it (`map->info.table`) and the version
//! is stamped afterwards, so the mapping is genuinely current for both the host
//! blitter and any guest code that looks at it. Nothing is staged.
//!
//! # What SDL does that this does not, and why it is unobservable
//!
//! The skip is `SDL_MapSurface`, which does four things beyond building the
//! table: `SDL_InvalidateMap` frees the old table and unlinks the map from the
//! destination's `list_blitmap`, `Map1toN` allocates a new one, the map is
//! re-registered on that list, and `SDL_CalculateBlit` picks the blitter again.
//!
//! - The *contents* are what the table is for, and they are identical (that is
//!   what the byte-comparison tests are for).
//! - The table *address* stays the same instead of moving to a fresh
//!   allocation. Nothing guest-visible reads that address: SDL's blitters
//!   index through it, and `SDL_InvalidateMap`/`SDL_FreeBlitMap` free it — so
//!   the pointer being the same block SDL itself allocated keeps every free
//!   balanced. A guest reading `map->info.table` sees a live, correct table.
//! - The list registration is unchanged rather than removed-and-re-added: the
//!   list maps one destination to the maps built for it, and the set of members
//!   is the same.
//! - `SDL_CalculateBlit` would select the same blitter, because neither format,
//!   flag nor alpha changed — which is why only the palette *version* can make
//!   the mapping stale in the first place.

use anyhow::{Context, Result};
use wie_cpu::CpuEngine;

use super::surface::{BlitMap, Format, MAX_PALETTE_COLORS, read_palette_colors};

/// Table stride for one destination pixel: `Map1toN` rounds 3 up to 4.
#[must_use]
fn lane(dst_bytes_per_pixel: u8) -> usize {
    if dst_bytes_per_pixel == 3 {
        4
    } else {
        usize::from(dst_bytes_per_pixel)
    }
}

/// One table entry, exactly as SDL's `ASSEMBLE_RGBA` writes it.
///
/// The returned array is the entry's bytes in guest order (little endian, WIE's
/// only target); only `lane()` bytes of it are meaningful.
fn assemble_rgba(bytes_per_pixel: u8, fmt: &Format, r: u8, g: u8, b: u8, a: u8) -> [u8; 4] {
    // SDL guarantees `loss <= 8` and `shift < 32`; the masks keep a corrupt
    // format from panicking in a shift.
    let pack = |channel: u8, loss: u8, shift: u8| -> u32 {
        (u32::from(channel) >> u32::from(loss & 31)) << u32::from(shift & 31)
    };
    match bytes_per_pixel {
        1 => [
            u8::try_from(
                pack(r, fmt.rloss, fmt.rshift)
                    | pack(g, fmt.gloss, fmt.gshift)
                    | pack(b, fmt.bloss, fmt.bshift)
                    | pack(a, fmt.aloss, fmt.ashift),
            )
            .unwrap_or(0),
            0,
            0,
            0,
        ],
        2 => {
            let pixel = u16::try_from(
                pack(r, fmt.rloss, fmt.rshift)
                    | pack(g, fmt.gloss, fmt.gshift)
                    | pack(b, fmt.bloss, fmt.bshift)
                    | pack(a, fmt.aloss, fmt.ashift),
            )
            .unwrap_or(0);
            let mut entry = [0_u8; 4];
            entry[..2].copy_from_slice(&pixel.to_le_bytes());
            entry
        }
        3 => {
            // Raw bytes at `shift / 8`; alpha is not written at all.
            let mut entry = [0_u8; 4];
            let mut put = |offset: u8, value: u8| {
                let at = usize::from(offset / 8);
                if let Some(slot) = entry.get_mut(at) {
                    *slot = value;
                }
            };
            put(fmt.rshift, r);
            put(fmt.gshift, g);
            put(fmt.bshift, b);
            entry
        }
        _ => {
            let pixel = pack(r, fmt.rloss, fmt.rshift)
                | pack(g, fmt.gloss, fmt.gshift)
                | pack(b, fmt.bloss, fmt.bshift)
                | pack(a, fmt.aloss, fmt.ashift);
            pixel.to_le_bytes()
        }
    }
}

/// The channel modulation `Map1toN` is called with: the source surface's
/// `map->info.{r,g,b,a}`.
#[derive(Debug, Clone, Copy)]
struct Mods {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}

/// `(channel * mod) / 255`, truncating — `Map1toN`'s own arithmetic.
#[must_use]
fn modulate(channel: u8, mod_: u8) -> u8 {
    u8::try_from((u32::from(channel) * u32::from(mod_)) / 255).unwrap_or(0)
}

/// Rebuild `map->info.table` in place for a palette-expanding mapping.
///
/// Returns `false` (and writes nothing) when the table cannot be reproduced —
/// no table pointer, an empty or oversized palette, a palette read failure —
/// in which case the caller must decline and let SDL rebuild the mapping.
pub(super) fn rebuild_table(
    engine: &mut dyn CpuEngine,
    map: &BlitMap,
    src_fmt: &Format,
    dst_fmt: &Format,
) -> Result<bool> {
    if map.table == 0 || src_fmt.palette == 0 {
        return Ok(false);
    }
    let Some(palette) = read_palette_colors(engine, src_fmt.palette)? else {
        return Ok(false);
    };
    // `read_palette_colors` already rejects an empty palette, one larger than
    // SDL's 256-entry limit, and a palette with no colour array.
    if palette.ncolors == 0 || palette.ncolors > MAX_PALETTE_COLORS {
        return Ok(false);
    }
    let stride = lane(dst_fmt.bytes_per_pixel);
    let mods = Mods {
        r: map.r,
        g: map.g,
        b: map.b,
        a: map.a,
    };
    let mut staged = [0_u8; 256 * 4];
    for (index, color) in palette.iter().enumerate() {
        let entry = assemble_rgba(
            dst_fmt.bytes_per_pixel,
            dst_fmt,
            modulate(color.r, mods.r),
            modulate(color.g, mods.g),
            modulate(color.b, mods.b),
            modulate(color.a, mods.a),
        );
        let at = index * stride;
        let Some(slot) = staged.get_mut(at..at + stride) else {
            return Ok(false);
        };
        slot.copy_from_slice(&entry[..stride]);
    }
    let written = palette.ncolors * stride;
    let Some(bytes) = staged.get(..written) else {
        return Ok(false);
    };
    // Write straight into the guest's own table: this is the buffer both the
    // host blitter and any guest code reading `map->info.table` will see.
    if let Some(span) = engine.host_slice_mut(map.table, written) {
        span.copy_from_slice(bytes);
    } else {
        engine
            .mem_write(map.table, bytes)
            .context("failed to write a rebuilt SDL palette table")?;
    }
    // Stamp the version the table now reflects. This is what SDL would have
    // written after its own rebuild, and it is why the next call starts clean.
    crate::guest_memory::write_u32(engine, map.src_palette_version_va(), palette.version)
        .context("failed to stamp the SDL blit mapping's palette version")?;
    Ok(true)
}

/// Drive [`rebuild_table`] the way the blit handler does, from guest memory
/// alone.
///
/// This exists so the tests can compare 256 table entries against an
/// independent reference without constructing a `BlitMap` by hand — the fields
/// a `BlitMap` carries are exactly the ones a hand-built literal could get
/// wrong, which would defeat the comparison.
#[cfg(test)]
pub(crate) fn rebuild_table_from_guest(
    engine: &mut dyn CpuEngine,
    map_va: u64,
    src_fmt_va: u64,
    dst_fmt_va: u64,
) -> Result<bool> {
    let Some(map) = super::surface::read_blit_map(engine, map_va)? else {
        return Ok(false);
    };
    let src_fmt = super::surface::read_format(engine, src_fmt_va)?;
    let dst_fmt = super::surface::read_format(engine, dst_fmt_va)?;
    rebuild_table(engine, &map, &src_fmt, &dst_fmt)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The packing rule, per destination width, against values worked out by
    /// hand. `ARGB8888` is the format DOOM Retro's window surface uses; the
    /// others are the shapes that make a loss/shift bug visible (a 16-bit
    /// format has non-zero losses, a 3-byte one bypasses the packing entirely).
    #[test]
    fn assemble_matches_sdl_for_every_width() {
        // ARGB8888: Rshift 16, Gshift 8, Bshift 0, Ashift 24, all losses 0.
        let argb = Format {
            format: 0x1636_2004,
            palette: 0,
            bits_per_pixel: 32,
            bytes_per_pixel: 4,
            rmask: 0x00ff_0000,
            gmask: 0x0000_ff00,
            bmask: 0x0000_00ff,
            amask: 0xff00_0000,
            rloss: 0,
            gloss: 0,
            bloss: 0,
            aloss: 0,
            rshift: 16,
            gshift: 8,
            bshift: 0,
            ashift: 24,
        };
        assert_eq!(
            assemble_rgba(4, &argb, 0x12, 0x34, 0x56, 0x78),
            [0x56, 0x34, 0x12, 0x78]
        );
        // RGB565: Rloss 3 / Rshift 11, Gloss 2 / Gshift 5, Bloss 3 / Bshift 0.
        let rgb565 = Format {
            format: 0x1515_1022,
            palette: 0,
            bits_per_pixel: 16,
            bytes_per_pixel: 2,
            rmask: 0xf800,
            gmask: 0x07e0,
            bmask: 0x001f,
            amask: 0,
            rloss: 3,
            gloss: 2,
            bloss: 3,
            aloss: 8,
            rshift: 11,
            gshift: 5,
            bshift: 0,
            ashift: 0,
        };
        // 0xFF>>3 = 0x1F, 0xFF>>2 = 0x3F, 0xFF>>3 = 0x1F → 0xFFFF.
        assert_eq!(
            &assemble_rgba(2, &rgb565, 0xff, 0xff, 0xff, 0xff)[..2],
            &[0xff, 0xff]
        );
        // 0x80>>3 = 0x10, 0x40>>2 = 0x10, 0x20>>3 = 0x04 → 0x8000|0x0200|0x04.
        let partial = assemble_rgba(2, &rgb565, 0x80, 0x40, 0x20, 0xff);
        let value = u16::from_le_bytes([partial[0], partial[1]]);
        assert_eq!(value, (0x10 << 11) | (0x10 << 5) | 0x04);
        // 24-byte destination: raw bytes at shift/8, alpha dropped.
        let rgb24 = Format {
            format: 0x1710_1806,
            bytes_per_pixel: 3,
            rmask: 0x00ff_0000,
            gmask: 0x0000_ff00,
            bmask: 0x0000_00ff,
            amask: 0,
            rloss: 0,
            gloss: 0,
            bloss: 0,
            aloss: 8,
            rshift: 16,
            gshift: 8,
            bshift: 0,
            ashift: 0,
            bits_per_pixel: 24,
            palette: 0,
        };
        let entry = assemble_rgba(3, &rgb24, 0x11, 0x22, 0x33, 0x44);
        assert_eq!(
            entry,
            [0x33, 0x22, 0x11, 0x00],
            "byte order at shift/8, no alpha"
        );
    }

    /// `Map1toN`'s channel arithmetic: truncating, and a zero mod is black.
    #[test]
    fn modulation_truncates_like_map_1_to_n() {
        assert_eq!(modulate(255, 255), 255);
        assert_eq!(modulate(255, 0), 0);
        // 255 * 128 / 255 = 128 exactly; 200 * 128 / 255 = 100.39 → 100.
        assert_eq!(modulate(255, 128), 128);
        assert_eq!(modulate(200, 128), 100);
        assert_eq!(modulate(1, 128), 0);
    }

    /// `Map1toN` allocates `256 * ((bpp == 3) ? 4 : bpp)` and writes `ncolors`
    /// entries; both facts are load-bearing for the in-place rebuild.
    #[test]
    fn the_table_stride_rounds_three_up_to_four() {
        assert_eq!(lane(1), 1);
        assert_eq!(lane(2), 2);
        assert_eq!(lane(3), 4);
        assert_eq!(lane(4), 4);
    }
}
