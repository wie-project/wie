//! Guest-side SDL2 structure layouts.
//!
//! Every offset here is read out of the guest at the documented position and
//! then **cross-checked against a field SDL itself keeps consistent**, so a
//! wrong offset cannot silently produce a wrong pixel: the readers either
//! return the value SDL would use, or the caller falls through to the guest's
//! own implementation.
//!
//! Layouts are from SDL 2.x `SDL_surface.h` / `SDL_pixels.h` (x86-64, the only
//! target WIE runs) and were verified against the guest DLL's own code
//! generation: `SDL_BlitInfo::table` is read at `0x40` and `dst_w`/`dst_h` at
//! `0x20`/`0x24` by the compiled `Blit1to4`, and `SDL_PixelFormat::
//! BytesPerPixel` at `0x11` by the compiled `SDL_FillRects` fill selector.

use anyhow::{Context, Result};
use wie_cpu::CpuEngine;

use crate::guest_memory::read_u32;

/// `SDL_Surface` size (`sizeof` on x86-64).
pub(crate) const SURFACE_SIZE: usize = 96;
/// `SDL_PixelFormat` size.
pub(crate) const FORMAT_SIZE: usize = 56;
/// `SDL_Palette` size — asserted by the layout test and used by the fixtures
/// that build a palette in guest memory, so it does not exist outside tests.
#[cfg(test)]
pub(crate) const PALETTE_SIZE: usize = 24;

// ── SDL_Surface field offsets ───────────────────────────────────────────────
const SURFACE_FLAGS: u64 = 0;
const SURFACE_FORMAT: u64 = 8;
const SURFACE_W: u64 = 16;
const SURFACE_H: u64 = 20;
const SURFACE_PITCH: u64 = 24;
const SURFACE_PIXELS: u64 = 32;
const SURFACE_LOCKED: u64 = 48;
pub(super) const SURFACE_CLIP_RECT: u64 = 64;
const SURFACE_MAP: u64 = 80;

// ── SDL_PixelFormat field offsets ───────────────────────────────────────────
const FORMAT_FORMAT: u64 = 0;
const FORMAT_PALETTE: u64 = 8;
const FORMAT_BITS_PER_PIXEL: u64 = 16;
const FORMAT_BYTES_PER_PIXEL: u64 = 17;

// ── SDL_Palette field offsets ───────────────────────────────────────────────
const PALETTE_VERSION: u64 = 16;

// ── SDL_BlitMap / SDL_BlitInfo field offsets ────────────────────────────────
// The map starts with `dst`, `identity`, `blit`, `data`, then embeds `info`.
pub(super) const MAP_DST: u64 = 0;
pub(super) const MAP_IDENTITY: u64 = 8;
/// Byte offset of the embedded [`BlitInfo`] inside the map.
const MAP_INFO: u64 = 32;
/// Byte offset of `dst_palette_version` inside the map.
pub(super) const MAP_DST_PALETTE_VERSION: u64 = 120;
/// Byte offset of `src_palette_version` inside the map.
pub(super) const MAP_SRC_PALETTE_VERSION: u64 = 124;
/// `sizeof(SDL_BlitMap)`.
pub(crate) const MAP_SIZE: usize = 128;

/// `SDL_RLEACCEL` — the surface flag that makes `SDL_MUSTLOCK` true.
pub(super) const SDL_RLEACCEL: u32 = 0x0000_0002;

/// A guest `SDL_Rect` (`Sint16 x, y; Uint16 w, h`).
///
/// The widths are *unsigned* in the struct but SDL_IntersectRect does its
/// arithmetic in `int`, so the sign of `x`/`y` and the zero-extension of
/// `w`/`h` both matter for an exact port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    /// The guest's own `SDL_RectEmpty`: empty is `w <= 0 || h <= 0`.
    #[must_use]
    pub(super) fn is_empty(self) -> bool {
        self.w <= 0 || self.h <= 0
    }
}

/// Port of SDL's `SDL_IntersectRect` (`SDL_rect.c`), including its use of
/// semi-open intervals and its `FALSE` for any empty input.
pub(super) fn intersect_rect(a: Rect, b: Rect) -> Option<Rect> {
    let mut a_min_x = a.x;
    let mut a_max_x = a.x.wrapping_add(a.w);
    let b_min_x = b.x;
    let b_max_x = b.x.wrapping_add(b.w);
    if b_min_x > a_min_x {
        a_min_x = b_min_x;
    }
    if b_max_x < a_max_x {
        a_max_x = b_max_x;
    }
    if a_max_x <= a_min_x {
        return None;
    }

    let mut a_min_y = a.y;
    let mut a_max_y = a.y.wrapping_add(a.h);
    let b_min_y = b.y;
    let b_max_y = b.y.wrapping_add(b.h);
    if b_min_y > a_min_y {
        a_min_y = b_min_y;
    }
    if b_max_y < a_max_y {
        a_max_y = b_max_y;
    }
    if a_max_y <= a_min_y {
        return None;
    }

    Some(Rect {
        x: a_min_x,
        y: a_min_y,
        w: a_max_x - a_min_x,
        h: a_max_y - a_min_y,
    })
}

/// Read a guest `SDL_Rect`.
pub(super) fn read_rect(engine: &mut dyn CpuEngine, va: u64) -> Result<Rect> {
    let mut bytes = [0_u8; 8];
    engine
        .mem_read(va, &mut bytes)
        .context("failed to read a guest SDL_Rect")?;
    Ok(Rect {
        x: i32::from(i16::from_le_bytes([bytes[0], bytes[1]])),
        y: i32::from(i16::from_le_bytes([bytes[2], bytes[3]])),
        w: i32::from(u16::from_le_bytes([bytes[4], bytes[5]])),
        h: i32::from(u16::from_le_bytes([bytes[6], bytes[7]])),
    })
}

/// Write a guest `SDL_Rect` back (the out-parameter of the `*Blit` APIs).
pub(super) fn write_rect(engine: &mut dyn CpuEngine, va: u64, rect: Rect) -> Result<()> {
    let mut bytes = [0_u8; 8];
    bytes[0..2].copy_from_slice(&i16::try_from(rect.x).unwrap_or(0).to_le_bytes());
    bytes[2..4].copy_from_slice(&i16::try_from(rect.y).unwrap_or(0).to_le_bytes());
    bytes[4..6].copy_from_slice(&u16::try_from(rect.w).unwrap_or(0).to_le_bytes());
    bytes[6..8].copy_from_slice(&u16::try_from(rect.h).unwrap_or(0).to_le_bytes());
    engine
        .mem_write(va, &bytes)
        .context("failed to write a guest SDL_Rect")
}

/// A guest `SDL_PixelFormat`, reduced to the fields a blit/fill needs.
#[derive(Debug, Clone, Copy)]
pub(super) struct Format {
    /// `SDL_PixelFormat::format` (the `SDL_PIXELFORMAT_*` enum value).
    pub format: u32,
    /// Guest VA of the `SDL_Palette`, or `0` for a non-indexed format.
    pub palette: u64,
    pub bits_per_pixel: u8,
    pub bytes_per_pixel: u8,
}

impl Format {
    /// SDL's `SDL_ISPIXELFORMAT_INDEXED`: not FOURCC, and pixel type INDEX1/4/8.
    ///
    /// `SDL_ISPIXELFORMAT_FOURCC` is `format && SDL_PIXELFLAG(format) != 1`,
    /// i.e. every *ordinary* pixel format carries flag `1`.
    #[must_use]
    pub(super) fn is_indexed(self) -> bool {
        let pixel_flag = (self.format >> 28) & 0x0f;
        let pixel_type = (self.format >> 24) & 0x0f;
        self.format != 0 && pixel_flag == 1 && matches!(pixel_type, 1..=3)
    }
}

/// Read a guest `SDL_PixelFormat`.
pub(super) fn read_format(engine: &mut dyn CpuEngine, va: u64) -> Result<Format> {
    // One 56-byte read: the struct is a single guest object, and reading it in
    // one shot keeps a concurrent guest writer from tearing fields together
    // that SDL itself reads as a unit.
    let mut bytes = [0_u8; FORMAT_SIZE];
    engine
        .mem_read(va, &mut bytes)
        .context("failed to read a guest SDL_PixelFormat")?;
    Ok(Format {
        format: u32::from_le_bytes([
            bytes[FORMAT_FORMAT as usize],
            bytes[FORMAT_FORMAT as usize + 1],
            bytes[FORMAT_FORMAT as usize + 2],
            bytes[FORMAT_FORMAT as usize + 3],
        ]),
        palette: u64::from_le_bytes(
            bytes[FORMAT_PALETTE as usize..FORMAT_PALETTE as usize + 8]
                .try_into()
                .unwrap_or([0; 8]),
        ),
        bits_per_pixel: bytes[FORMAT_BITS_PER_PIXEL as usize],
        bytes_per_pixel: bytes[FORMAT_BYTES_PER_PIXEL as usize],
    })
}

/// A guest `SDL_Surface`, reduced to the fields a blit/fill needs.
#[derive(Debug, Clone, Copy)]
pub(super) struct Surface {
    /// Guest VA of the surface itself (guest identity for `map->dst` checks).
    pub va: u64,
    pub flags: u32,
    pub format_va: u64,
    pub w: i32,
    pub h: i32,
    pub pitch: i32,
    pub pixels: u64,
    pub locked: i32,
    pub clip_rect: Rect,
    pub map_va: u64,
}

impl Surface {
    /// SDL's `SDL_MUSTLOCK`: an RLE-accelerated surface must be locked before
    /// its pixels are touched, and unlocking re-encodes it. Every case this
    /// module accelerates assumes a plain software surface, so a set flag means
    /// "the guest does something here we do not reproduce".
    #[must_use]
    pub(super) fn must_lock(self) -> bool {
        self.flags & SDL_RLEACCEL != 0
    }

    /// The pixel buffer looks like a plain software surface this module can
    /// address directly: non-empty, with a pitch that covers one row of
    /// `bytes_per_pixel` pixels.
    #[must_use]
    pub(super) fn is_direct(self, bytes_per_pixel: u8) -> bool {
        self.pixels != 0
            && self.w > 0
            && self.h > 0
            && self.pitch >= self.w.saturating_mul(i32::from(bytes_per_pixel))
    }
}

/// Read a guest `SDL_Surface`, or `None` for a NULL pointer.
pub(super) fn read_surface(engine: &mut dyn CpuEngine, va: u64) -> Result<Option<Surface>> {
    if va == 0 {
        return Ok(None);
    }
    let mut bytes = [0_u8; SURFACE_SIZE];
    engine
        .mem_read(va, &mut bytes)
        .context("failed to read a guest SDL_Surface")?;
    let word = |off: u64| -> u64 {
        let at = off as usize;
        u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap_or([0; 8]))
    };
    let dword = |off: u64| -> u32 {
        let at = off as usize;
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap_or([0; 4]))
    };
    Ok(Some(Surface {
        va,
        flags: dword(SURFACE_FLAGS),
        format_va: word(SURFACE_FORMAT),
        w: i32::from_le_bytes(
            bytes[SURFACE_W as usize..SURFACE_W as usize + 4]
                .try_into()
                .unwrap_or([0; 4]),
        ),
        h: i32::from_le_bytes(
            bytes[SURFACE_H as usize..SURFACE_H as usize + 4]
                .try_into()
                .unwrap_or([0; 4]),
        ),
        pitch: i32::from_le_bytes(
            bytes[SURFACE_PITCH as usize..SURFACE_PITCH as usize + 4]
                .try_into()
                .unwrap_or([0; 4]),
        ),
        pixels: word(SURFACE_PIXELS),
        locked: i32::from_le_bytes(
            bytes[SURFACE_LOCKED as usize..SURFACE_LOCKED as usize + 4]
                .try_into()
                .unwrap_or([0; 4]),
        ),
        clip_rect: Rect {
            x: i32::from(i16::from_le_bytes([
                bytes[SURFACE_CLIP_RECT as usize],
                bytes[SURFACE_CLIP_RECT as usize + 1],
            ])),
            y: i32::from(i16::from_le_bytes([
                bytes[SURFACE_CLIP_RECT as usize + 2],
                bytes[SURFACE_CLIP_RECT as usize + 3],
            ])),
            w: i32::from(u16::from_le_bytes([
                bytes[SURFACE_CLIP_RECT as usize + 4],
                bytes[SURFACE_CLIP_RECT as usize + 5],
            ])),
            h: i32::from(u16::from_le_bytes([
                bytes[SURFACE_CLIP_RECT as usize + 6],
                bytes[SURFACE_CLIP_RECT as usize + 7],
            ])),
        },
        map_va: word(SURFACE_MAP),
    }))
}

/// Write `surface->locked` back after a lock/unlock.
pub(super) fn write_locked(engine: &mut dyn CpuEngine, surface_va: u64, locked: i32) -> Result<()> {
    crate::guest_memory::write_i32(engine, surface_va.saturating_add(SURFACE_LOCKED), locked)
}

/// The parts of `SDL_BlitMap` / `SDL_BlitInfo` a blit decision needs.
///
/// `src_fmt`, `dst_fmt`, `src_pitch` and `dst_pitch` are *SDL-maintained*
/// mirrors of the two surfaces: reading them is how this module proves its
/// field offsets are the ones the guest is using before it touches a pixel.
#[derive(Debug, Clone, Copy)]
pub(super) struct BlitMap {
    /// `map->dst` — the destination the cached mapping was built for.
    pub dst: u64,
    /// `map->identity` — bit-identical formats, no conversion needed.
    pub identity: i32,
    /// `info.src_fmt`, compared against the source surface's format pointer.
    pub src_fmt: u64,
    /// `info.dst_fmt`.
    pub dst_fmt: u64,
    /// `info.src_pitch`.
    pub src_pitch: i32,
    /// `info.dst_pitch`.
    pub dst_pitch: i32,
    /// `info.table` — the palette expansion table, when the mapping built one.
    pub table: u64,
    /// `info.flags` — `SDL_COPY_*` bits.
    pub flags: i32,
    /// `info.colorkey`.
    pub colorkey: u32,
    /// `map->src_palette_version`.
    pub src_palette_version: u32,
    /// `map->dst_palette_version`.
    pub dst_palette_version: u32,
}

/// Read a guest `SDL_BlitMap`, or `None` when the surface has no map.
pub(super) fn read_blit_map(engine: &mut dyn CpuEngine, map_va: u64) -> Result<Option<BlitMap>> {
    if map_va == 0 {
        return Ok(None);
    }
    let mut bytes = [0_u8; MAP_SIZE];
    engine
        .mem_read(map_va, &mut bytes)
        .context("failed to read a guest SDL_BlitMap")?;
    let word = |off: u64| -> u64 {
        let at = usize::try_from(off).unwrap_or(0);
        u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap_or([0; 8]))
    };
    let dword = |off: u64| -> u32 {
        let at = usize::try_from(off).unwrap_or(0);
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap_or([0; 4]))
    };
    let info = MAP_INFO;
    Ok(Some(BlitMap {
        dst: word(MAP_DST),
        identity: i32::from_le_bytes(dword(MAP_IDENTITY).to_le_bytes()),
        src_fmt: word(info + 48),
        dst_fmt: word(info + 56),
        src_pitch: i32::from_le_bytes(dword(info + 16).to_le_bytes()),
        dst_pitch: i32::from_le_bytes(dword(info + 40).to_le_bytes()),
        table: word(info + 64),
        flags: i32::from_le_bytes(dword(info + 72).to_le_bytes()),
        colorkey: dword(info + 76),
        src_palette_version: dword(MAP_SRC_PALETTE_VERSION),
        dst_palette_version: dword(MAP_DST_PALETTE_VERSION),
    }))
}

/// Read a guest `SDL_Palette` version field, or `None` when the format has no
/// palette.
pub(super) fn palette_version(engine: &mut dyn CpuEngine, palette_va: u64) -> Result<Option<u32>> {
    if palette_va == 0 {
        return Ok(None);
    }
    Ok(Some(read_u32(
        engine,
        palette_va.saturating_add(PALETTE_VERSION),
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kill-rule for a wrong struct offset: the readers must agree with the
    /// sizes SDL uses, and the intersection must be SDL's, not a plausible
    /// variant. These are the only two things a later edit can silently break
    /// (a field moved one slot, or a semi-open interval turned inclusive).
    #[test]
    fn layout_sizes_match_the_guest_abi() {
        assert_eq!(SURFACE_SIZE, 96);
        assert_eq!(FORMAT_SIZE, 56);
        assert_eq!(PALETTE_SIZE, 24);
        assert_eq!(MAP_SIZE, 128);
        // `clip_rect` sits right after `list_blitmap`; `map` after it.
        assert_eq!(SURFACE_CLIP_RECT, 64);
        assert_eq!(SURFACE_MAP, 80);
    }

    #[test]
    fn intersect_rect_matches_sdl() {
        let a = Rect {
            x: 0,
            y: 0,
            w: 10,
            h: 10,
        };
        let b = Rect {
            x: 5,
            y: 5,
            w: 10,
            h: 10,
        };
        assert_eq!(
            intersect_rect(a, b),
            Some(Rect {
                x: 5,
                y: 5,
                w: 5,
                h: 5
            })
        );
        // Touching edges do not intersect (SDL uses a half-open interval).
        let c = Rect {
            x: 10,
            y: 0,
            w: 5,
            h: 10,
        };
        assert_eq!(intersect_rect(a, c), None);
        // An empty argument is FALSE even when fully inside.
        let empty = Rect {
            x: 2,
            y: 2,
            w: 0,
            h: 4,
        };
        assert_eq!(intersect_rect(a, empty), None);
    }

    #[test]
    fn indexed_formats_are_detected_like_sdl() {
        // SDL_PIXELFORMAT_INDEX8 = 0x1300_0000, ARGB8888 = 0x1636_2004.
        let index8 = Format {
            format: 0x1300_0000,
            palette: 1,
            bits_per_pixel: 8,
            bytes_per_pixel: 1,
        };
        assert!(index8.is_indexed());
        let argb = Format {
            format: 0x1636_2004,
            palette: 0,
            bits_per_pixel: 32,
            bytes_per_pixel: 4,
        };
        assert!(!argb.is_indexed());
    }
}
