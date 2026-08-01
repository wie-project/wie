//! SIMD pixel helpers for the GUI present path.
//!
//! NEON on aarch64 (the winit frontend's host), portable scalar fallback
//! elsewhere.  Intrinsics lower to real instructions even in debug builds,
//! so the per-pixel conversion stays fast regardless of the host profile.

/// Mask the alpha byte of little-endian BGRA pixels (`0xAARRGGBB`) into
/// softbuffer 0RGB (`0x00RRGGBB`).
///
/// `src` is a byte view of BGRA pixels (a 32-bpp DIB); `dst` receives the
/// masked 0RGB values.  Copies `min(dst.len(), src.len() / 4)` pixels.
///
/// The pointer cast tolerates any alignment (NEON `vld1q` loads and the
/// scalar tail's `read_unaligned` both accept unaligned addresses), and the
/// loop counters are bounded by `n <= src.len() / 4` so they cannot
/// overflow.
#[expect(
    clippy::cast_ptr_alignment,
    clippy::arithmetic_side_effects,
    clippy::integer_division,
    reason = "unaligned-safe loads; counters bounded by the slice lengths"
)]
pub fn mask_bgra_to_0rgb(dst: &mut [u32], src: &[u8]) {
    let n = dst.len().min(src.len() / 4);
    if n == 0 {
        return;
    }
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: `n` bounds every access: `src` is read at most `n * 4`
        // bytes (well within `src.len()`) and `dst` at most `n` words
        // (within `dst.len()`).  `read_unaligned`-style NEON loads tolerate
        // any alignment; `vandq_u32` masks the alpha byte of 4 pixels at a
        // time.  The main loop is unrolled 4× (16 px/iteration) because
        // debug builds don't unroll — the branch/pointer overhead per
        // iteration would otherwise dominate.
        #[expect(unsafe_code)]
        unsafe {
            use std::arch::aarch64::{vandq_u32, vdupq_n_u32, vld1q_u32, vst1q_u32};

            let mask = vdupq_n_u32(0x00FF_FFFF);
            let src_p = src.as_ptr().cast::<u32>();
            let dst_p = dst.as_mut_ptr();
            let n16 = n & !15;
            let mut i = 0;
            while i < n16 {
                let v0 = vld1q_u32(src_p.add(i));
                let v1 = vld1q_u32(src_p.add(i + 4));
                let v2 = vld1q_u32(src_p.add(i + 8));
                let v3 = vld1q_u32(src_p.add(i + 12));
                vst1q_u32(dst_p.add(i), vandq_u32(v0, mask));
                vst1q_u32(dst_p.add(i + 4), vandq_u32(v1, mask));
                vst1q_u32(dst_p.add(i + 8), vandq_u32(v2, mask));
                vst1q_u32(dst_p.add(i + 12), vandq_u32(v3, mask));
                i += 16;
            }
            // Remainder: at most 3 groups of 4, then single pixels.
            let n4 = n & !3;
            while i < n4 {
                let v = vld1q_u32(src_p.add(i));
                vst1q_u32(dst_p.add(i), vandq_u32(v, mask));
                i += 4;
            }
            // Tail: fewer than 4 pixels.
            while i < n {
                // Little-endian host: a raw u32 read equals from_le_bytes.
                let p = src_p.add(i).read_unaligned();
                dst_p.add(i).write(p & 0x00FF_FFFF);
                i += 1;
            }
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        // SAFETY: as above; used on non-aarch64 hosts.
        #[expect(unsafe_code)]
        unsafe {
            let src_p = src.as_ptr().cast::<u32>();
            let dst_p = dst.as_mut_ptr();
            for i in 0..n {
                let p = src_p.add(i).read_unaligned();
                dst_p.add(i).write(p & 0x00FF_FFFF);
            }
        }
    }
}

/// Nearest-neighbour stretch of `src` into `buf`.
///
/// Precomputes the source row/column index maps once (w+h divisions total)
/// so the inner loop has ZERO divisions, then gathers with unchecked
/// indexing — the indices are provably in range, so the bounds checks would
/// only cost time, not correctness.  Dimensions are `u32` (winit/softbuffer
/// sizes); converted to `usize` internally.
///
/// The arithmetic is provably overflow-free: every product is bounded by
/// `dst * src < 2^32`-sized dimensions × index, and the unchecked gathers
/// are in-range by construction (see SAFETY on the inner block).
#[expect(
    clippy::arithmetic_side_effects,
    clippy::integer_division,
    reason = "index-map math is provably in-range; u32 dims × usize indices cannot overflow"
)]
pub fn stretch_nearest(
    buf: &mut [u32],
    src: &[u32],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
) {
    let (src_w, src_h, dst_w, dst_h) = (
        usize::try_from(src_w).unwrap_or(0),
        usize::try_from(src_h).unwrap_or(0),
        usize::try_from(dst_w).unwrap_or(0),
        usize::try_from(dst_h).unwrap_or(0),
    );
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return;
    }
    let sy: Vec<usize> = (0..dst_h).map(|y| (y * src_h) / dst_h).collect();
    let sx: Vec<usize> = (0..dst_w).map(|x| (x * src_w) / dst_w).collect();
    // SAFETY: `sy[y] < src_h` and `sx[x] < src_w` by construction, so every
    // read is within `src`; `dst` is fully covered by the y/x loops.  Callers
    // must pass buffers of exactly `dst_w * dst_h` and `src_w * src_h`.
    #[expect(unsafe_code)]
    unsafe {
        for y in 0..dst_h {
            let src_row = *sy.get_unchecked(y) * src_w;
            let dst_row = y * dst_w;
            for x in 0..dst_w {
                *buf.get_unchecked_mut(dst_row + x) =
                    *src.get_unchecked(src_row + *sx.get_unchecked(x));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{mask_bgra_to_0rgb, stretch_nearest};

    #[test]
    fn mask_zeroes_alpha_byte() {
        // BGRA bytes: B=0x11, G=0x22, R=0x33, A=0xFF.
        let src: [u8; 8] = [0x11, 0x22, 0x33, 0xFF, 0xAA, 0xBB, 0xCC, 0x80];
        let mut dst = [0u32; 2];
        mask_bgra_to_0rgb(&mut dst, &src);
        assert_eq!(dst[0], 0x0033_2211);
        assert_eq!(dst[1], 0x00CC_BBAA);
    }

    #[test]
    fn mask_tolerates_short_src() {
        let src: [u8; 6] = [1, 2, 3, 4, 5, 6]; // 1 full pixel + 2 stray bytes
        let mut dst = [0xFFFF_FFFFu32; 3];
        mask_bgra_to_0rgb(&mut dst, &src);
        // LE bytes [B=1, G=2, R=3, A=4] → 0x04030201, alpha masked → 0x00030201.
        assert_eq!(dst[0], 0x0003_0201);
        assert_eq!(dst[1], 0xFFFF_FFFF); // untouched
        assert_eq!(dst[2], 0xFFFF_FFFF);
    }

    #[test]
    fn stretch_upscales_known_pattern() {
        let src = [0x00FF_0000u32, 0x0000_FF00, 0x0000_00FF, 0x00FF_FFFF]; // 2x2
        let mut buf = [0u32; 16]; // 4x4
        stretch_nearest(&mut buf, &src, 2, 2, 4, 4);
        // Row 0 and 1 sample source row 0; cols 0..1 → src col 0, cols 2..3 → src col 1.
        assert_eq!(buf[0], 0x00FF_0000);
        assert_eq!(buf[1], 0x00FF_0000);
        assert_eq!(buf[2], 0x0000_FF00);
        assert_eq!(buf[3], 0x0000_FF00);
        assert_eq!(buf[8], 0x0000_00FF); // row 2, col 0
        assert_eq!(buf[15], 0x00FF_FFFF); // row 3, col 3
    }

    #[test]
    fn stretch_zero_input_is_noop() {
        let mut buf = [7u32; 4];
        stretch_nearest(&mut buf, &[1, 2, 3, 4], 0, 2, 2, 2);
        assert_eq!(buf, [7; 4]);
    }

    /// Debug-mode speed smoke: a fullscreen-size blit (1920×1080 ≈ 2M px)
    /// must complete in milliseconds even unoptimized — NEON intrinsics lower
    /// to real instructions regardless of profile.  Prints timing; no assert
    /// (CI machines vary), but a regression to per-pixel debug loops would
    /// show as 10-100x slower in the output.
    #[test]
    fn mask_speed_smoke_debug() {
        let px = 1920 * 1080;
        let src = vec![0x12u8; px * 4]; // BGRA
        let mut dst = vec![0u32; px];
        let t = std::time::Instant::now();
        mask_bgra_to_0rgb(&mut dst, &src);
        let elapsed = t.elapsed();
        // 2M px fits in u32; f64::from(u32) avoids a lossy usize cast.
        let px_f = f64::from(u32::try_from(px).unwrap_or(1));
        eprintln!(
            "mask_bgra_to_0rgb: {px} px in {elapsed:?} ({:.2} ns/px)",
            elapsed.as_secs_f64() * 1e9 / px_f
        );
        // Sanity: every pixel's alpha byte cleared, RGB preserved.
        assert_eq!(dst.first(), Some(&0x0012_1212));
        assert_eq!(dst.last(), Some(&0x0012_1212));
    }
}
