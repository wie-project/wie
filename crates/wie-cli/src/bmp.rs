//! Minimal BMP writer for `--screenshot`.
//!
//! Writes a 14-byte `BITMAPFILEHEADER` + 40-byte `BITMAPINFOHEADER` + raw
//! bottom-up BGRA pixel rows.  Zero dependencies; the output opens in Preview.

use std::io::{self, Write};

/// Write a 0RGB surface (top-down, row-major `u32`) as a bottom-up 32-bpp BMP.
///
/// `src_stride` is the input's row pitch in PIXELS (a present frame's row
/// pitch is 64-padded, so it may exceed `width`); rows are read at that
/// pitch and written packed. The `bitmap_file_header` + `bitmap_info_header`
/// + pixel data are written directly to `writer`.
pub fn write_bmp<W: Write>(
    mut writer: W,
    width: u32,
    height: u32,
    src_stride: u32,
    pixels: &[u32],
) -> io::Result<()> {
    let row_bytes = width.checked_mul(4).unwrap_or(0);
    // BMP rows are 4-byte aligned (already true for 32-bpp).
    let stride = row_bytes;
    let pixel_data_size = stride.checked_mul(height).unwrap_or(0);
    let file_size = 14 + 40 + pixel_data_size;

    // BITMAPFILEHEADER (14 bytes).
    writer.write_all(b"BM")?; // bfType
    writer.write_all(&file_size.to_le_bytes())?; // bfSize
    writer.write_all(&[0u8; 4])?; // bfReserved1/2
    writer.write_all(&(14u32 + 40).to_le_bytes())?; // bfOffBits

    // BITMAPINFOHEADER (40 bytes).
    writer.write_all(&(40u32).to_le_bytes())?; // biSize
    writer.write_all(&width.to_le_bytes())?; // biWidth
    writer.write_all(&height.to_le_bytes())?; // biHeight (positive = bottom-up)
    writer.write_all(&(1u16).to_le_bytes())?; // biPlanes
    writer.write_all(&(32u16).to_le_bytes())?; // biBitCount
    writer.write_all(&[0u8; 24])?; // compression=BI_RGB, size, resolution, colors

    // Pixel data: write rows bottom-up (BMP format).
    // The input is top-down 0RGB u32; BMP expects bottom-up BGRA.
    let pitch = usize::try_from(src_stride.max(width)).unwrap_or(0);
    for y in (0..height).rev() {
        let row_start = usize::try_from(y).unwrap_or(0).saturating_mul(pitch);
        for &pixel in pixels
            .get(row_start..row_start.saturating_add(usize::try_from(width).unwrap_or(0)))
            .unwrap_or(&[])
        {
            // Convert 0RGB to BGRA: R@16, G@8, B@0, A=255.
            let b = (pixel & 0x0000_00FF) as u8;
            let g = ((pixel >> 8) & 0xFF) as u8;
            let r = ((pixel >> 16) & 0xFF) as u8;
            writer.write_all(&[b, g, r, 0xFF])?;
        }
    }

    Ok(())
}
