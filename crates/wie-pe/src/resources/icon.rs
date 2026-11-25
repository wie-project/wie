//! `RT_ICON` / `RT_GROUP_ICON` parsing and icon-payload decoding.
//!
//! An icon group (`RT_GROUP_ICON`, type **14**) is a `GRPICONDIR`: `WORD
//! idReserved` (0), `WORD idType` (1 = icon, 2 = cursor), `WORD idCount`, then
//! `idCount` 14-byte `GRPICONDIRENTRY` records — `bWidth`, `bHeight`,
//! `bColorCount`, `bReserved`, `wPlanes`, `wBitCount`, `dwBytesInRes`, `nID`.
//! Each record names one `RT_ICON` (type **3**) payload by `nID`.
//!
//! The payload is either
//!
//! * a `BITMAPINFOHEADER` DIB with no `BITMAPFILEHEADER`: an XOR bitmap
//!   followed by the 1-bpp AND mask, which is why `biHeight` is **twice** the
//!   icon's height. `biSize` is the palette offset (the DIB starts at the
//!   header, unlike a `.bmp` file), rows are bottom-up and 4-byte aligned;
//! * or, for large entries since Vista, a PNG stream (recognised by its
//!   signature and handed through untouched — decoding it needs a deflate
//!   implementation this crate does not carry, and the only consumer is the
//!   macOS presenter, which has the platform decoder).
//!
//! Decoding is best-effort like the sibling resource parsers: a malformed,
//! truncated or unsupported payload yields `None`, never a panic or a partial
//! allocation driven by a hostile header.

use crate::PeSectionMap;

use super::common::{parse_resource_type, read_u16_at, read_u32_at};

/// Type id of `RT_ICON` resources (`winuser.h`: `MAKEINTRESOURCE(3)`).
pub(crate) const RT_ICON: u16 = 3;

/// Type id of `RT_GROUP_ICON` resources (`MAKEINTRESOURCE(14)`).
pub(crate) const RT_GROUP_ICON: u16 = 14;

/// Side length a zero `bWidth`/`bHeight` byte stands for: the `GRPICONDIR`
/// entry field is a `BYTE`, so 256 px is spelled `0`.
const ICON_SIDE_256: u32 = 256;

/// `GRPICONDIR::idType` for an icon group (2 is a cursor group, which this
/// module does not decode).
const GROUP_TYPE_ICON: u16 = 1;

/// Fixed `GRPICONDIR` header: three `WORD`s.
const GRPICONDIR_SIZE: usize = 6;

/// One `GRPICONDIRENTRY` record.
const GRPICONDIRENTRY_SIZE: usize = 14;

/// `BITMAPINFOHEADER::biSize` — the minimum DIB header accepted. The palette
/// (for ≤8-bpp bitmaps) starts at this offset.
const BITMAPINFOHEADER_SIZE: u32 = 40;

/// `biCompression` value `BI_RGB`. RLE and `BI_BITFIELDS` headers are not
/// decoded (real icons are `BI_RGB`).
const BI_RGB: u32 = 0;

/// Largest icon side accepted from a resource. A hostile `biWidth`/`biHeight`
/// must not drive an unbounded allocation; real icon entries top out at 256.
const MAX_ICON_SIDE: u32 = 4096;

/// PNG stream signature (the first `RT_ICON` payload form without a DIB
/// header).
const PNG_SIGNATURE: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// One `GRPICONDIRENTRY`: a size/format record that names an `RT_ICON` by id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IconGroupEntry {
    /// `bWidth`; `0` means 256 (see [`Self::width_px`]).
    pub width: u8,
    /// `bHeight`; `0` means 256 (see [`Self::height_px`]).
    pub height: u8,
    /// `bColorCount` (0 for ≥8-bpp entries).
    pub color_count: u8,
    /// `wBitCount` — bits per pixel of the payload.
    pub bit_count: u16,
    /// `dwBytesInRes` — declared payload size.
    pub bytes_in_res: u32,
    /// `nID` — the `RT_ICON` resource id this entry selects.
    pub id: u16,
}

impl IconGroupEntry {
    /// The entry's width in pixels (`bWidth == 0` is 256).
    #[must_use]
    pub fn width_px(self) -> u32 {
        if self.width == 0 {
            ICON_SIDE_256
        } else {
            u32::from(self.width)
        }
    }

    /// The entry's height in pixels (`bHeight == 0` is 256).
    #[must_use]
    pub fn height_px(self) -> u32 {
        if self.height == 0 {
            ICON_SIDE_256
        } else {
            u32::from(self.height)
        }
    }
}

/// One parsed `RT_GROUP_ICON` group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconGroup {
    /// Entries in resource order (the `rc.exe` order is 16/32/48/256).
    pub entries: Vec<IconGroupEntry>,
    /// Language id of the group (the language-level directory entry key).
    pub lang: u16,
}

/// One `RT_ICON` payload exactly as stored in the resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconImage {
    /// Resource id (the group entry's `nID`).
    pub id: u16,
    /// Language id of the payload.
    pub lang: u16,
    /// Raw payload bytes (DIB or PNG).
    pub bytes: Vec<u8>,
}

/// A decoded, non-premultiplied RGBA8 image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbaImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes, top-down, `[R, G, B, A]` per pixel.
    pub pixels: Vec<u8>,
}

impl RgbaImage {
    /// Whether every pixel has the same value — a blank (uniform) decode.
    ///
    /// Callers use this to reject a payload that decoded to nothing visible
    /// instead of presenting a flat colour block.
    #[must_use]
    pub fn is_uniform(&self) -> bool {
        let (pixels, _) = self.pixels.as_chunks::<4>();
        let Some(first) = pixels.first() else {
            return true;
        };
        pixels.iter().all(|pixel| pixel == first)
    }
}

/// An icon payload, in the form its consumer can use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IconData {
    /// A DIB payload decoded to RGBA8.
    Rgba(RgbaImage),
    /// A PNG payload, still compressed. The platform decoder (AppKit on macOS)
    /// turns it into an image; re-encoding it here would need a deflate
    /// implementation this crate does not depend on.
    Png(Vec<u8>),
}

/// Parse every `RT_GROUP_ICON` group in `image`, in directory order (named
/// resources first, then ascending id — the same order the Windows shell uses
/// when it picks an application's icon).
#[must_use]
pub fn parse_pe_icon_groups(image: &[u8], sections: &[PeSectionMap]) -> Vec<IconGroup> {
    parse_resource_type(image, sections, RT_GROUP_ICON, parse_icon_group)
}

/// Parse every `RT_ICON` payload in `image`, in directory order.
#[must_use]
pub fn parse_pe_icons(image: &[u8], sections: &[PeSectionMap]) -> Vec<IconImage> {
    parse_resource_type(image, sections, RT_ICON, |id, lang, bytes| {
        Some(IconImage {
            id,
            lang,
            bytes: bytes.to_vec(),
        })
    })
}

/// Parse one `GRPICONDIR` block.
///
/// A cursor group (`idType == 2`) is rejected — its payloads are cursor DIBs
/// with a hotspot, not icons. Entries truncated by the block end stop the
/// walk; a group with no entry at all is treated as absent.
fn parse_icon_group(_id: u16, lang: u16, bytes: &[u8]) -> Option<IconGroup> {
    if read_u16_at(bytes, 0)? != 0 || read_u16_at(bytes, 2)? != GROUP_TYPE_ICON {
        return None;
    }
    let count = usize::from(read_u16_at(bytes, 4)?);
    let mut entries = Vec::with_capacity(count.min(bytes.len() / GRPICONDIRENTRY_SIZE));
    for index in 0..count {
        let offset = GRPICONDIR_SIZE.checked_add(index.checked_mul(GRPICONDIRENTRY_SIZE)?)?;
        let Some(entry) = icon_group_entry(bytes, offset) else {
            break;
        };
        entries.push(entry);
    }
    if entries.is_empty() {
        return None;
    }
    Some(IconGroup { entries, lang })
}

/// Read one 14-byte `GRPICONDIRENTRY` at `offset`.
fn icon_group_entry(bytes: &[u8], offset: usize) -> Option<IconGroupEntry> {
    let raw = bytes.get(offset..offset.checked_add(GRPICONDIRENTRY_SIZE)?)?;
    Some(IconGroupEntry {
        width: *raw.first()?,
        height: *raw.get(1)?,
        color_count: *raw.get(2)?,
        bit_count: read_u16_at(raw, 6)?,
        bytes_in_res: read_u32_at(raw, 8)?,
        id: read_u16_at(raw, 12)?,
    })
}

/// Pick the entry to present at 128–512 px.
///
/// The largest side wins (a 256 px PNG entry beats a 48 px DIB, which the
/// window server would only have to upscale); ties break on the higher bit
/// depth, then on the lower id so the choice is deterministic.
#[must_use]
pub fn choose_group_entry(entries: &[IconGroupEntry]) -> Option<&IconGroupEntry> {
    entries.iter().max_by_key(|entry| {
        (
            entry.width_px().max(entry.height_px()),
            entry.bit_count,
            std::cmp::Reverse(entry.id),
        )
    })
}

/// Decode the payload [`choose_group_entry`] selects out of `group`.
///
/// The payload is looked up by the entry's `nID`, preferring the group's own
/// language and falling back to any language that carries that id.
#[must_use]
pub fn decode_icon_group(icons: &[IconImage], group: &IconGroup) -> Option<IconData> {
    let chosen = choose_group_entry(&group.entries)?;
    let icon = icons
        .iter()
        .find(|icon| icon.id == chosen.id && icon.lang == group.lang)
        .or_else(|| icons.iter().find(|icon| icon.id == chosen.id))?;
    decode_icon(&icon.bytes)
}

/// Decode one `RT_ICON` payload: PNG streams pass through, DIBs become RGBA8.
#[must_use]
pub fn decode_icon(bytes: &[u8]) -> Option<IconData> {
    if bytes.starts_with(PNG_SIGNATURE) {
        return Some(IconData::Png(bytes.to_vec()));
    }
    decode_dib_to_rgba(bytes).map(IconData::Rgba)
}

/// Decode an icon DIB (XOR bitmap + AND mask, no `BITMAPFILEHEADER`) to RGBA8.
///
/// Supports 1-, 4-, 8-, 24- and 32-bpp `BI_RGB` payloads. For 32-bpp
/// payloads the alpha channel is authoritative when the file fills it (any
/// non-zero alpha byte); otherwise — old `rc.exe` output leaves it zero — the
/// AND mask supplies transparency, exactly as Windows does.
#[must_use]
pub fn decode_dib_to_rgba(bytes: &[u8]) -> Option<RgbaImage> {
    let header_size = read_u32_at(bytes, 0)?;
    if header_size < BITMAPINFOHEADER_SIZE {
        return None;
    }
    let width = u32::try_from(i32::try_from(read_u32_at(bytes, 4)?).ok()?).ok()?;
    let double_height = i32::try_from(read_u32_at(bytes, 8)?).ok()?;
    if double_height <= 0 || double_height % 2 != 0 {
        return None;
    }
    let height = u32::try_from(double_height / 2).ok()?;
    if width == 0 || height == 0 || width > MAX_ICON_SIDE || height > MAX_ICON_SIDE {
        return None;
    }
    if read_u16_at(bytes, 12)? != 1 || read_u32_at(bytes, 16)? != BI_RGB {
        return None;
    }
    let bit_count = read_u16_at(bytes, 14)?;

    // Palette: `biClrUsed` entries, or the full 2^bpp range when it is zero
    // (and always at most 2^bpp — a hostile count must not read past it).
    let palette_entries = match bit_count {
        1 | 4 | 8 => {
            let full = 1_u32.checked_shl(u32::from(bit_count))?;
            let declared = read_u32_at(bytes, 32)?;
            let used = if declared == 0 || declared > full {
                full
            } else {
                declared
            };
            usize::try_from(used).ok()?
        }
        _ => 0,
    };
    let palette_start = usize::try_from(header_size).ok()?;
    let xor_start = palette_start.checked_add(palette_entries.checked_mul(4)?)?;
    let xor_stride = usize::try_from(dib_stride(width, u32::from(bit_count))?).ok()?;
    let xor_len = xor_stride.checked_mul(usize::try_from(height).ok()?)?;
    let xor = bytes.get(xor_start..xor_start.checked_add(xor_len)?)?;
    let palette = bytes.get(palette_start..xor_start)?;

    // The AND mask is optional for a truncated resource: without it every
    // pixel stays opaque (better than dropping a decodable icon).
    let mask_start = xor_start.checked_add(xor_len)?;
    let mask_stride = usize::try_from(dib_stride(width, 1)?).ok()?;
    let mask_len = mask_stride.checked_mul(usize::try_from(height).ok()?)?;
    let mask = bytes.get(mask_start..mask_start.checked_add(mask_len)?);

    let has_alpha = bit_count == 32
        && xor
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel.last() != Some(&0));
    let width_px = usize::try_from(width).ok()?;
    let height_px = usize::try_from(height).ok()?;
    let mut pixels = Vec::with_capacity(width_px.checked_mul(height_px)?.checked_mul(4)?);
    for row in 0..height_px {
        // DIB rows are bottom-up: the first row in the payload is the last
        // output row.
        let flip = height_px.checked_sub(1)?.checked_sub(row)?;
        let xor_row = xor_stride.checked_mul(flip)?;
        let mask_row = mask_stride.checked_mul(flip)?;
        for x in 0..width_px {
            let [red, green, blue] = dib_rgb(xor, xor_row, x, bit_count, palette)?;
            let alpha = if has_alpha {
                dib_alpha32(xor, xor_row, x)?
            } else if mask_opaque(mask, mask_row, x) {
                0xFF
            } else {
                0
            };
            pixels.extend_from_slice(&[red, green, blue, alpha]);
        }
    }
    Some(RgbaImage {
        width,
        height,
        pixels,
    })
}

/// Row stride of a DIB bitmap: `width * bpp` bits rounded up to 4-byte
/// alignment (the `BITMAPINFOHEADER` row contract).
fn dib_stride(width: u32, bits_per_pixel: u32) -> Option<u32> {
    width
        .checked_mul(bits_per_pixel)?
        .checked_add(31)?
        .checked_div(32)?
        .checked_mul(4)
}

/// One palette index's RGB, from a `RGBQUAD` palette (`B, G, R, 0` per entry).
fn palette_rgb(palette: &[u8], index: usize) -> Option<[u8; 3]> {
    let base = index.checked_mul(4)?;
    Some([
        *palette.get(base.checked_add(2)?)?,
        *palette.get(base.checked_add(1)?)?,
        *palette.get(base)?,
    ])
}

/// The RGB of one XOR-bitmap pixel.
fn dib_rgb(xor: &[u8], row: usize, x: usize, bit_count: u16, palette: &[u8]) -> Option<[u8; 3]> {
    match bit_count {
        32 | 24 => {
            let bytes_per_pixel = usize::from(bit_count / 8);
            let pos = row.checked_add(x.checked_mul(bytes_per_pixel)?)?;
            Some([
                *xor.get(pos.checked_add(2)?)?,
                *xor.get(pos.checked_add(1)?)?,
                *xor.get(pos)?,
            ])
        }
        8 => palette_rgb(palette, usize::from(*xor.get(row.checked_add(x)?)?)),
        4 => {
            let byte = *xor.get(row.checked_add(x / 2)?)?;
            let index = if x.is_multiple_of(2) {
                byte >> 4
            } else {
                byte & 0x0F
            };
            palette_rgb(palette, usize::from(index))
        }
        1 => {
            let byte = *xor.get(row.checked_add(x / 8)?)?;
            let index = usize::from((byte >> (7 - x % 8)) & 1);
            palette_rgb(palette, index)
        }
        _ => None,
    }
}

/// The alpha byte of one 32-bpp XOR pixel (the DIB stores `B, G, R, A`).
fn dib_alpha32(xor: &[u8], row: usize, x: usize) -> Option<u8> {
    xor.get(row.checked_add(x.checked_mul(4)?)?.checked_add(3)?)
        .copied()
}

/// Whether the AND mask marks a pixel opaque (bit 1). A missing mask keeps
/// the pixel opaque.
fn mask_opaque(mask: Option<&[u8]>, row: usize, x: usize) -> bool {
    match mask {
        None => true,
        Some(bytes) => match row.checked_add(x / 8).and_then(|pos| bytes.get(pos)) {
            Some(byte) => (byte >> (7 - x % 8)) & 1 == 1,
            None => true,
        },
    }
}

/// Synthetic-image builders and the parser tests.
#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::super::common::test_util::{copy_into, fake_rsrc_section, put_u16, put_u32};
    use super::*;
    use crate::PeSectionMap;
    use std::path::PathBuf;

    /// A `GRPICONDIR` with one entry: `side` px, `bpp`, `bytes`, `id`.
    fn group_bytes(side: u8, bit_count: u16, bytes: u32, id: u16) -> Vec<u8> {
        let mut group = Vec::new();
        put_u16(&mut group, 0); // idReserved
        put_u16(&mut group, 1); // idType = icon
        put_u16(&mut group, 1); // idCount
        group.extend_from_slice(&[side, side, 0, 0]);
        put_u16(&mut group, 1); // planes
        put_u16(&mut group, bit_count);
        put_u32(&mut group, bytes);
        put_u16(&mut group, id);
        group
    }

    /// Emit an `IMAGE_RESOURCE_DIRECTORY` header followed by `entries`.
    fn push_dir(buf: &mut Vec<u8>, entries: &[(u32, u32)]) {
        put_u32(buf, 0); // characteristics
        put_u32(buf, 0); // timestamp
        put_u16(buf, 0); // major version
        put_u16(buf, 0); // minor version
        put_u16(buf, 0); // named entries
        put_u16(buf, u16::try_from(entries.len()).expect("fits"));
        for (name, offset) in entries {
            put_u32(buf, *name);
            put_u32(buf, *offset);
        }
    }

    /// A synthetic PE with one `RT_ICON` (id 1, lang 0x0409) and one
    /// `RT_GROUP_ICON` (id 1, lang 0x0409) whose single entry names id 1.
    ///
    /// Layout (relative to the resource root at file 0x200 / RVA 0x1000):
    /// root 0x00, type-3 dir 0x20, type-14 dir 0x38, lang dirs 0x50/0x68,
    /// data entries 0x80/0x90; payloads at RVA 0x1100 / 0x1200.
    fn synthetic_image(icon_payload: &[u8], group: &[u8]) -> (Vec<u8>, Vec<PeSectionMap>) {
        let sections = vec![fake_rsrc_section()];
        let mut image = vec![0_u8; 0x1400];

        let mut root = Vec::new();
        push_dir(&mut root, &[(3, 0x8000_0020), (14, 0x8000_0038)]);
        copy_into(&mut image, 0x200, &root);

        let mut type_icon = Vec::new();
        push_dir(&mut type_icon, &[(1, 0x8000_0050)]);
        copy_into(&mut image, 0x220, &type_icon);

        let mut type_group = Vec::new();
        push_dir(&mut type_group, &[(1, 0x8000_0068)]);
        copy_into(&mut image, 0x238, &type_group);

        let mut lang_icon = Vec::new();
        push_dir(&mut lang_icon, &[(0x0409, 0x80)]);
        copy_into(&mut image, 0x250, &lang_icon);

        let mut lang_group = Vec::new();
        push_dir(&mut lang_group, &[(0x0409, 0x90)]);
        copy_into(&mut image, 0x268, &lang_group);

        let mut icon_entry = Vec::new();
        put_u32(&mut icon_entry, 0x1100);
        put_u32(
            &mut icon_entry,
            u32::try_from(icon_payload.len()).expect("fits"),
        );
        put_u32(&mut icon_entry, 0);
        put_u32(&mut icon_entry, 0);
        copy_into(&mut image, 0x280, &icon_entry);

        let mut group_entry = Vec::new();
        put_u32(&mut group_entry, 0x1200);
        put_u32(&mut group_entry, u32::try_from(group.len()).expect("fits"));
        put_u32(&mut group_entry, 0);
        put_u32(&mut group_entry, 0);
        copy_into(&mut image, 0x290, &group_entry);

        copy_into(&mut image, 0x300, icon_payload);
        copy_into(&mut image, 0x400, group);
        (image, sections)
    }

    /// A 32-bpp icon DIB: `alpha` per source pixel (bottom-up rows), `mask`
    /// per source pixel (1 = opaque). `R` is 255, `B`/`G` encode x/y so a
    /// correct decode is visibly non-uniform.
    fn dib32(width: u32, height: u32, alpha: &[u8], mask: &[u8]) -> Vec<u8> {
        let mut dib = Vec::new();
        put_u32(&mut dib, 40); // biSize
        put_u32(&mut dib, width);
        put_u32(&mut dib, height * 2);
        put_u16(&mut dib, 1);
        put_u16(&mut dib, 32);
        put_u32(&mut dib, 0); // BI_RGB
        put_u32(&mut dib, 0); // sizeImage
        put_u32(&mut dib, 0); // x ppm
        put_u32(&mut dib, 0); // y ppm
        put_u32(&mut dib, 0); // clrUsed
        put_u32(&mut dib, 0); // clrImportant
        for y in 0..height {
            for x in 0..width {
                let index = usize::try_from(y * width + x).expect("fits");
                let a = alpha.get(index).copied().unwrap_or(0xFF);
                dib.extend_from_slice(&[
                    u8::try_from(x * 16).expect("fits"),
                    u8::try_from(y * 16).expect("fits"),
                    255,
                    a,
                ]);
            }
        }
        for y in 0..height {
            let mut row = 0_u8;
            for x in 0..width {
                let index = usize::try_from(y * width + x).expect("fits");
                if mask.get(index).copied().unwrap_or(1) != 0 {
                    row |= 0x80 >> x;
                }
            }
            dib.push(row);
            while dib.len() % 4 != 0 {
                dib.push(0);
            }
        }
        dib
    }

    #[test]
    fn dib_32bpp_uses_the_alpha_channel_when_present() {
        // Every alpha byte is non-zero, so the AND mask must be ignored — it
        // marks the whole bitmap transparent.
        let alpha: Vec<u8> = (0..16)
            .map(|i| u8::try_from(10 + i).expect("fits"))
            .collect();
        let mask = vec![0_u8; 16];
        let image = dib32(4, 4, &alpha, &mask);
        let decoded = decode_dib_to_rgba(&image).expect("decodes");
        assert_eq!((decoded.width, decoded.height), (4, 4));
        assert_eq!(decoded.pixels.len(), 4 * 4 * 4);
        assert!(!decoded.is_uniform(), "the pattern is not flat");
        // Top-left output pixel = the LAST source row (DIBs are bottom-up):
        // source (x=0, y=3) → B=0, G=48, R=255, A = alpha[12] = 22.
        assert_eq!(&decoded.pixels[0..4], &[255, 48, 0, 22]);
        // Bottom-left = the first source row → alpha[0] = 10.
        assert_eq!(&decoded.pixels[48..52], &[255, 0, 0, 10]);
    }

    #[test]
    fn dib_32bpp_falls_back_to_the_and_mask_when_alpha_is_empty() {
        // Old rc.exe output: a zero alpha channel, transparency in the mask.
        let mut mask = vec![0_u8; 16];
        mask[0] = 1; // source pixel (0, 0), the bottom-left output pixel
        let image = dib32(4, 4, &[0_u8; 16], &mask);
        let decoded = decode_dib_to_rgba(&image).expect("decodes");
        // Bottom-left (source row 0) is opaque...
        assert_eq!(&decoded.pixels[48..52], &[255, 0, 0, 0xFF]);
        // ...and the top-left (source row 3, masked out) is transparent.
        assert_eq!(decoded.pixels[3], 0);
    }

    #[test]
    fn dib_8bpp_reads_the_palette() {
        let mut dib = Vec::new();
        put_u32(&mut dib, 40);
        put_u32(&mut dib, 2);
        put_u32(&mut dib, 4); // 2 px, two rows for the mask
        put_u16(&mut dib, 1);
        put_u16(&mut dib, 8);
        put_u32(&mut dib, 0);
        put_u32(&mut dib, 0);
        put_u32(&mut dib, 0);
        put_u32(&mut dib, 0);
        put_u32(&mut dib, 2); // clrUsed
        put_u32(&mut dib, 0);
        // RGBQUADs: red, then green (B, G, R, 0).
        dib.extend_from_slice(&[0, 0, 255, 0, 0, 255, 0, 0]);
        // XOR rows (bottom-up): row 0 = [1, 0], row 1 = [0, 1].
        dib.extend_from_slice(&[1, 0, 0, 0]);
        dib.extend_from_slice(&[0, 1, 0, 0]);
        // AND mask rows, all opaque.
        dib.extend_from_slice(&[0xC0, 0, 0, 0]);
        dib.extend_from_slice(&[0xC0, 0, 0, 0]);
        let decoded = decode_dib_to_rgba(&dib).expect("decodes");
        assert_eq!((decoded.width, decoded.height), (2, 2));
        // Top-left = source row 1, x=0 → palette index 0 (red).
        assert_eq!(&decoded.pixels[0..4], &[255, 0, 0, 0xFF]);
        // Top-right → palette index 1 (green).
        assert_eq!(&decoded.pixels[4..8], &[0, 255, 0, 0xFF]);
    }

    #[test]
    fn png_payload_passes_through_unchanged() {
        let mut png = PNG_SIGNATURE.to_vec();
        png.extend_from_slice(b"not a real file, only the signature matters");
        match decode_icon(&png).expect("recognised") {
            IconData::Png(bytes) => assert_eq!(bytes, png),
            other => panic!("expected a PNG payload, got {other:?}"),
        }
    }

    #[test]
    fn truncated_payloads_fall_back_instead_of_panicking() {
        let dib = dib32(4, 4, &[0xFF; 16], &[1; 16]);
        // A valid DIB cut short at every length must decode to None (or, for
        // the header-complete cases, still respect the bounds) — never panic.
        for cut in 0..dib.len() {
            let _decoded = decode_dib_to_rgba(&dib[..cut]);
        }
        assert!(decode_dib_to_rgba(&[]).is_none());
        assert!(decode_dib_to_rgba(&[40, 0]).is_none());
        // A header claiming a 4096×4096 bitmap with no pixels behind it.
        let mut huge = dib32(4, 4, &[0xFF; 16], &[1; 16]);
        huge[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_dib_to_rgba(&huge).is_none());

        // Group icon blocks: empty, header-only, and a truncated entry.
        assert!(parse_icon_group(1, 0x0409, &[]).is_none());
        assert!(parse_icon_group(1, 0x0409, &[0, 0, 1, 0]).is_none());
        let group = group_bytes(4, 32, 120, 1);
        assert!(parse_icon_group(1, 0x0409, &group).is_some());
        assert!(parse_icon_group(1, 0x0409, &group[..10]).is_none());

        // A resource section with no icon tree at all.
        let (mut image, sections) = synthetic_image(&dib, &group);
        for byte in image.iter_mut().skip(0x200 + 0x0C).take(4) {
            *byte = 0; // zero the root directory's entry counts
        }
        assert!(parse_pe_icon_groups(&image, &sections).is_empty());
        assert!(parse_pe_icons(&image, &sections).is_empty());
    }

    #[test]
    fn choose_group_entry_prefers_the_largest_side() {
        let entry = |width: u8, bit_count: u16, id: u16| IconGroupEntry {
            width,
            height: width,
            color_count: 0,
            bit_count,
            bytes_in_res: 0,
            id,
        };
        let entries = vec![
            entry(16, 32, 4),
            entry(0, 8, 5), // 256 px, 8 bpp
            entry(48, 32, 2),
            entry(0, 32, 6), // 256 px, 32 bpp — the tie-break winner
        ];
        assert_eq!(choose_group_entry(&entries).map(|e| e.id), Some(6));
        // Identical sizes and depths: the lower id wins, deterministically.
        let tied = vec![entry(48, 32, 9), entry(48, 32, 7)];
        assert_eq!(choose_group_entry(&tied).map(|e| e.id), Some(7));
        assert!(choose_group_entry(&[]).is_none());
    }

    #[test]
    fn synthetic_image_decodes_end_to_end() {
        let dib = dib32(4, 4, &[0xFF; 16], &[1; 16]);
        let group = group_bytes(4, 32, u32::try_from(dib.len()).expect("fits"), 1);
        let (image, sections) = synthetic_image(&dib, &group);
        let groups = parse_pe_icon_groups(&image, &sections);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].lang, 0x0409);
        assert_eq!(groups[0].entries.len(), 1);
        let icons = parse_pe_icons(&image, &sections);
        assert_eq!(icons.len(), 1);
        assert_eq!(icons[0].id, 1);
        match decode_icon_group(&icons, &groups[0]).expect("decodes") {
            IconData::Rgba(image) => {
                assert_eq!((image.width, image.height), (4, 4));
                assert!(!image.is_uniform());
            }
            other => panic!("expected a DIB, got {other:?}"),
        }
    }

    /// The local `real_exes/` fixtures are not tracked by git; skip when the
    /// checkout does not carry them.
    fn real_exe(name: &str) -> Option<PathBuf> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../real_exes")
            .join(name);
        path.is_file().then_some(path)
    }

    #[test]
    fn doomretro_icon_is_a_256_png_with_dib_fallbacks() {
        let Some(path) = real_exe("doomretro/doomretro.exe") else {
            return;
        };
        let bytes = std::fs::read(&path).expect("read doomretro.exe");
        let plan = crate::pe_map_plan_from_bytes(&bytes).expect("PE map plan");
        let groups = parse_pe_icon_groups(&bytes, &plan.sections);
        let group = groups.first().expect("a group icon");
        assert_eq!(group.entries.len(), 4);
        let chosen = choose_group_entry(&group.entries).expect("an entry");
        assert_eq!((chosen.width_px(), chosen.height_px()), (256, 256));

        let icons = parse_pe_icons(&bytes, &plan.sections);
        assert_eq!(icons.len(), 4);
        match decode_icon_group(&icons, group).expect("decodes") {
            IconData::Png(png) => assert!(png.starts_with(PNG_SIGNATURE)),
            other => panic!("expected the 256 px PNG entry, got {other:?}"),
        }
        // The 48×48 DIB entry is the alternative: real pixel data, not a flat
        // block.
        let dib_entry = group
            .entries
            .iter()
            .find(|entry| entry.width_px() == 48)
            .expect("a 48x48 entry");
        let dib = icons
            .iter()
            .find(|icon| icon.id == dib_entry.id)
            .expect("the 48x48 payload");
        match decode_icon(dib.bytes.as_slice()).expect("decodes") {
            IconData::Rgba(image) => {
                assert_eq!((image.width, image.height), (48, 48));
                assert!(!image.is_uniform());
            }
            other => panic!("expected a DIB, got {other:?}"),
        }
    }

    #[test]
    fn notepad_icon_is_a_dib_with_alpha() {
        let Some(path) = real_exe("notepad.exe") else {
            return;
        };
        let bytes = std::fs::read(&path).expect("read notepad.exe");
        let plan = crate::pe_map_plan_from_bytes(&bytes).expect("PE map plan");
        let groups = parse_pe_icon_groups(&bytes, &plan.sections);
        let group = groups.first().expect("a group icon");
        // 9 entries: 48/32/16 px in 4-, 8- and 32-bpp variants.
        assert_eq!(group.entries.len(), 9);
        let chosen = choose_group_entry(&group.entries).expect("an entry");
        assert_eq!((chosen.width_px(), chosen.bit_count), (48, 32));
        let icons = parse_pe_icons(&bytes, &plan.sections);
        match decode_icon_group(&icons, group).expect("decodes") {
            IconData::Rgba(image) => {
                assert_eq!((image.width, image.height), (48, 48));
                assert!(!image.is_uniform());
            }
            other => panic!("expected a DIB, got {other:?}"),
        }
    }
}
