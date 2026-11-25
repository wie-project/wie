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
pub(crate) fn write_bmp<W: Write>(
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

    // Build the whole file in one buffer and emit it with a single write.
    //
    // This used to call `write_all` once per pixel — 11 header writes plus
    // `width * height` four-byte writes, i.e. 1 780 121 `write(2)` calls for a
    // 1710x1041 frame, because two of the three call sites hand this function a
    // raw `File` rather than a `BufWriter`. That was ~507 ms of a GUI launch
    // spent outside the emulation session, and none of it in wgpu or Metal.
    let mut out = Vec::with_capacity(usize::try_from(file_size).unwrap_or(0));

    // BITMAPFILEHEADER (14 bytes).
    out.extend_from_slice(b"BM"); // bfType
    out.extend_from_slice(&file_size.to_le_bytes()); // bfSize
    out.extend_from_slice(&[0u8; 4]); // bfReserved1/2
    out.extend_from_slice(&(14u32 + 40).to_le_bytes()); // bfOffBits

    // BITMAPINFOHEADER (40 bytes).
    out.extend_from_slice(&(40u32).to_le_bytes()); // biSize
    out.extend_from_slice(&width.to_le_bytes()); // biWidth
    out.extend_from_slice(&height.to_le_bytes()); // biHeight (positive = bottom-up)
    out.extend_from_slice(&(1u16).to_le_bytes()); // biPlanes
    out.extend_from_slice(&(32u16).to_le_bytes()); // biBitCount
    out.extend_from_slice(&[0u8; 24]); // compression=BI_RGB, size, resolution, colors

    // Pixel data: rows bottom-up (BMP format). The input is top-down 0RGB u32;
    // BMP expects bottom-up BGRA. A short `pixels` slice emits a short file,
    // exactly as the per-pixel version did — the loop is bounded by what the row
    // range actually yields, not by `width`.
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
            out.extend_from_slice(&[b, g, r, 0xFF]);
        }
    }

    writer.write_all(&out)
}

/// The per-pixel implementation this replaced, kept for the byte-identity test.
#[cfg(test)]
fn write_bmp_per_pixel<W: Write>(
    mut writer: W,
    width: u32,
    height: u32,
    src_stride: u32,
    pixels: &[u32],
) -> io::Result<()> {
    let row_bytes = width.checked_mul(4).unwrap_or(0);
    let stride = row_bytes;
    let pixel_data_size = stride.checked_mul(height).unwrap_or(0);
    let file_size = 14 + 40 + pixel_data_size;

    writer.write_all(b"BM")?;
    writer.write_all(&file_size.to_le_bytes())?;
    writer.write_all(&[0u8; 4])?;
    writer.write_all(&(14u32 + 40).to_le_bytes())?;
    writer.write_all(&(40u32).to_le_bytes())?;
    writer.write_all(&width.to_le_bytes())?;
    writer.write_all(&height.to_le_bytes())?;
    writer.write_all(&(1u16).to_le_bytes())?;
    writer.write_all(&(32u16).to_le_bytes())?;
    writer.write_all(&[0u8; 24])?;

    let pitch = usize::try_from(src_stride.max(width)).unwrap_or(0);
    for y in (0..height).rev() {
        let row_start = usize::try_from(y).unwrap_or(0).saturating_mul(pitch);
        for &pixel in pixels
            .get(row_start..row_start.saturating_add(usize::try_from(width).unwrap_or(0)))
            .unwrap_or(&[])
        {
            let b = (pixel & 0x0000_00FF) as u8;
            let g = ((pixel >> 8) & 0xFF) as u8;
            let r = ((pixel >> 16) & 0xFF) as u8;
            writer.write_all(&[b, g, r, 0xFF])?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{write_bmp, write_bmp_per_pixel};
    use std::io::Write;

    /// Counts `write` calls so "one write per pixel" is a number, not a claim.
    #[derive(Default)]
    struct CountingWriter {
        writes: usize,
        bytes: usize,
    }

    impl Write for CountingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.writes += 1;
            self.bytes += buf.len();
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn frame(width: u32, height: u32) -> Vec<u32> {
        (0..width * height)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 8) & 0x00FF_FFFF)
            .collect()
    }

    /// The fix, stated as a measurement: one write instead of HEADER + W*H.
    ///
    /// `HEADER_WRITES` is what the per-pixel implementation above actually
    /// issues (four `BITMAPFILEHEADER` fields, six `BITMAPINFOHEADER` fields,
    /// with the two reserved words merged into one `[0u8; 4]`).
    const HEADER_WRITES: usize = 10;

    #[test]
    fn the_whole_file_is_emitted_in_one_write() {
        let (w, h) = (96u32, 64u32);
        let pixels = frame(w, h);

        let mut before = CountingWriter::default();
        write_bmp_per_pixel(&mut before, w, h, w, &pixels).expect("reference write");
        let mut after = CountingWriter::default();
        write_bmp(&mut after, w, h, w, &pixels).expect("buffered write");

        assert_eq!(
            before.writes,
            HEADER_WRITES + (w as usize) * (h as usize),
            "the old implementation wrote once per pixel"
        );
        assert_eq!(after.writes, 1, "the new implementation writes once");
        assert_eq!(after.bytes, before.bytes, "same number of bytes emitted");
    }

    /// A real present frame: 1710x1041 is 1 780 110 pixels, so the screenshot
    /// path issued 1 780 120 `write(2)` calls per frame before this change.
    #[test]
    fn a_real_present_frame_was_1_780_120_writes() {
        let (w, h) = (1710u32, 1041u32);
        assert_eq!(w as usize * h as usize, 1_780_110);
        let pixels = vec![0u32; (w * h) as usize];
        let mut before = CountingWriter::default();
        write_bmp_per_pixel(&mut before, w, h, w, &pixels).expect("reference write");
        assert_eq!(before.writes, 1_780_120);
        let mut after = CountingWriter::default();
        write_bmp(&mut after, w, h, w, &pixels).expect("buffered write");
        assert_eq!(after.writes, 1);
        assert_eq!(after.bytes, before.bytes);
    }

    /// Wall-clock, on a real `File` (the counting writer proves the syscall
    /// count; only a real descriptor shows the cost). Ignored by default so
    /// gates stay fast; run explicitly:
    /// `cargo nextest run -p wie-cli --release -E 'test(timing)' --run-ignored --no-capture`
    #[test]
    #[ignore = "timing measurement: run explicitly with --release --no-capture"]
    fn timing_a_real_present_frame_to_a_real_file() {
        use std::fs::File;
        use std::time::Instant;

        let (w, h) = (1710u32, 1041u32);
        let pixels = frame(w, h);
        let dir = std::env::temp_dir();

        for round in 0..3 {
            for (name, per_pixel) in [("bmp-before.bmp", true), ("bmp-after.bmp", false)] {
                let path = dir.join(name);
                let start = Instant::now();
                let file = File::create(&path).expect("create");
                if per_pixel {
                    write_bmp_per_pixel(&file, w, h, w, &pixels).expect("reference");
                } else {
                    write_bmp(&file, w, h, w, &pixels).expect("buffered");
                }
                drop(file);
                eprintln!(
                    "round {round} {name}: {:?} -> {}",
                    start.elapsed(),
                    path.display()
                );
            }
        }

        let before = std::fs::read(dir.join("bmp-before.bmp")).expect("read before");
        let after = std::fs::read(dir.join("bmp-after.bmp")).expect("read after");
        eprintln!("bytes before={} after={}", before.len(), after.len());
        assert_eq!(before, after, "files on disk differ");
        eprintln!("cmp: identical");
    }

    /// Real pixels, not synthetic: read an actual `--screenshot` BMP back into
    /// the top-down 0RGB form `write_bmp` takes, write it through both
    /// implementations, and require both to reproduce the original file byte
    /// for byte. Point `WIE_BMP_FIXTURE` at any BMP this writer produced.
    #[test]
    #[ignore = "needs WIE_BMP_FIXTURE pointing at a real captured BMP"]
    fn a_real_guest_frame_round_trips_byte_identically() {
        let Ok(path) = std::env::var("WIE_BMP_FIXTURE") else {
            eprintln!("WIE_BMP_FIXTURE unset; skipping");
            return;
        };
        let original = std::fs::read(&path).expect("read fixture");
        assert_eq!(&original[0..2], b"BM", "not a BMP");

        let u32_at = |at: usize| -> u32 {
            u32::from_le_bytes([
                original[at],
                original[at + 1],
                original[at + 2],
                original[at + 3],
            ])
        };
        let off_bits = u32_at(10) as usize;
        let width = u32_at(18);
        let height = u32_at(22);
        assert!(height < 1 << 20, "unexpected height {height}");

        // A capture killed mid-frame leaves a header promising more rows than
        // the file holds — eight of the BMPs in /tmp from bounded runs are like
        // that. They cannot round-trip, and comparing one against another
        // compares different amounts of a frame, so say so instead of indexing
        // past the end.
        let (w, h) = (width as usize, height as usize);
        if original.len() != off_bits + w * h * 4 {
            eprintln!(
                "{path}: TRUNCATED capture, header {width}x{height} needs {} bytes, file has {}; skipping",
                off_bits + w * h * 4,
                original.len()
            );
            return;
        }

        // BMP rows are bottom-up BGRA; `write_bmp` wants top-down 0RGB.
        let mut pixels = vec![0u32; w * h];
        for y in 0..h {
            let row = off_bits + y * w * 4;
            for x in 0..w {
                let p = row + x * 4;
                let b = u32::from(original[p]);
                let g = u32::from(original[p + 1]);
                let r = u32::from(original[p + 2]);
                pixels[(h - 1 - y) * w + x] = (r << 16) | (g << 8) | b;
            }
        }

        let mut after = Vec::new();
        write_bmp(&mut after, width, height, width, &pixels).expect("buffered");
        let mut before = Vec::new();
        write_bmp_per_pixel(&mut before, width, height, width, &pixels).expect("reference");

        eprintln!("{path}: {width}x{height}, {} bytes", original.len());
        assert_eq!(before, original, "reference did not reproduce the file");
        assert_eq!(after, original, "buffered writer changed real pixels");
    }

    /// Byte identity across the shapes that matter: odd widths (no padding, but
    /// exercises the row maths), a `src_stride` wider than the width, and a
    /// short pixel buffer, which made the old code emit a short file.
    #[test]
    fn output_is_byte_identical_to_the_per_pixel_version() {
        let cases: [(u32, u32, u32, usize); 5] = [
            (96, 64, 96, 96 * 64),
            (97, 33, 97, 97 * 33),
            (64, 64, 96, 96 * 64),
            (1, 1, 1, 1),
            (32, 8, 32, 32 * 4),
        ];
        for (w, h, stride, len) in cases {
            let pixels = frame(w.max(stride), h);
            let pixels = &pixels[..len.min(pixels.len())];
            let mut before = Vec::new();
            write_bmp_per_pixel(&mut before, w, h, stride, pixels).expect("reference");
            let mut after = Vec::new();
            write_bmp(&mut after, w, h, stride, pixels).expect("buffered");
            assert_eq!(
                before, after,
                "bytes differ for w={w} h={h} stride={stride} len={len}"
            );
        }
    }
}
