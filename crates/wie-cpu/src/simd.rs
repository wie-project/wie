//! SIMD pixel helpers for the GUI present path.
//!
//! NEON on aarch64 (the winit frontend's host), portable scalar fallback
//! elsewhere.  Intrinsics lower to real instructions even in debug builds,
//! so the per-pixel conversion stays fast regardless of the host profile.

/// 0RGB mask: clears the alpha byte of a little-endian BGRA pixel
/// (`0xAARRGGBB` → `0x00RRGGBB`).
const O_RGB_MASK: u32 = 0x00FF_FFFF;

/// Mask the alpha byte of little-endian BGRA pixels (`0xAARRGGBB`) into
/// present-format 0RGB (`0x00RRGGBB`).
///
/// `src` is a byte view of BGRA pixels (a 32-bpp DIB); `dst` receives the
/// masked 0RGB values.  Copies `min(dst.len(), src.len() / 4)` pixels.
///
/// The pointer cast tolerates any alignment (NEON `vld1q` loads and the
/// scalar tail's `read_unaligned` both accept unaligned addresses), and the
/// loop counters are bounded by `n <= src.len() / 4` so they cannot
/// overflow.
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

            let mask = vdupq_n_u32(O_RGB_MASK);
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
                dst_p.add(i).write(p & O_RGB_MASK);
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
                dst_p.add(i).write(p & O_RGB_MASK);
            }
        }
    }
}

/// Nearest-neighbour stretch of `src` into `buf`.
///
/// Precomputes the source row/column index maps once (w+h divisions total)
/// so the inner loop has ZERO divisions, then gathers with unchecked
/// indexing — the indices are provably in range, so the bounds checks would
/// only cost time, not correctness.  Dimensions are `u32` (winit window
/// sizes); converted to `usize` internally.
///
/// The arithmetic is provably overflow-free: every product is bounded by
/// `dst * src < 2^32`-sized dimensions × index, and the unchecked gathers
/// are in-range by construction (see SAFETY on the inner block).
pub fn stretch_nearest(
    buf: &mut [u32],
    src: &[u32],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
) {
    stretch_nearest_strided(buf, dst_w, src, src_w, src_h, dst_w, dst_h);
}

/// [`stretch_nearest`] with an explicit destination row pitch: `buf` holds
/// `dst_stride * dst_h` words and row `y` starts at `y * dst_stride`, so a
/// pitch-padded present surface (ADR-0001: 64-pixel stride for zero-copy GPU
/// uploads) can be the destination directly. The `dst_stride - dst_w` tail of
/// each row is left untouched (padding carries no content).
///
/// # Panics
/// Never panics; out-of-range rows/columns are skipped (same degradation as
/// the checked variant).
pub fn stretch_nearest_strided(
    buf: &mut [u32],
    dst_stride: u32,
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
    let dst_stride = usize::try_from(dst_stride).unwrap_or(0).max(dst_w);
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return;
    }
    let sy: Vec<usize> = (0..dst_h).map(|y| (y * src_h) / dst_h).collect();
    let sx: Vec<usize> = (0..dst_w).map(|x| (x * src_w) / dst_w).collect();
    // Fast path: when both buffers provably hold their full pitched extent,
    // every index below is in range by construction (`sy[y] < src_h`,
    // `sx[x] < src_w`, rows < dst_h) and the bounds checks would only cost
    // time. Otherwise fall back to the checked row loop.
    let src_full = src_w.checked_mul(src_h).is_some_and(|n| src.len() >= n);
    let dst_full = dst_stride
        .checked_mul(dst_h)
        .is_some_and(|n| buf.len() >= n);
    if src_full && dst_full {
        // SAFETY: `sy[y] < src_h` and `sx[x] < src_w` by construction, so every
        // read is within `src`; `dst` is fully covered by the y/x loops and
        // `dst_stride >= dst_w` keeps row starts in range.
        #[expect(unsafe_code)]
        unsafe {
            for y in 0..dst_h {
                let src_row = *sy.get_unchecked(y) * src_w;
                let dst_row = y * dst_stride;
                for x in 0..dst_w {
                    *buf.get_unchecked_mut(dst_row + x) =
                        *src.get_unchecked(src_row + *sx.get_unchecked(x));
                }
            }
        }
        return;
    }
    for (y, &sy_y) in sy.iter().enumerate().take(dst_h) {
        let src_row = sy_y.wrapping_mul(src_w);
        let dst_row = y.wrapping_mul(dst_stride);
        let (Some(src_row_slice), Some(dst_row_slice)) = (
            src.get(src_row..src_row.saturating_add(src_w)),
            buf.get_mut(dst_row..dst_row.saturating_add(dst_w)),
        ) else {
            continue;
        };
        for (dst_px, &sx_x) in dst_row_slice.iter_mut().zip(sx.iter()) {
            if let Some(&src_px) = src_row_slice.get(sx_x) {
                *dst_px = src_px;
            }
        }
    }
}

/// Write one `0RGB` color to 4 consecutive pixels.
///
/// The scalar tail / fallback is a plain store loop; on aarch64 a single
/// `vst1q` stores the duplicated vector. `dst.len()` must be ≥ 4 (callers
/// gate on runs of 4).
pub fn fill_0rgb_4x(dst: &mut [u32], color: u32) {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: `dst.len() >= 4` is the caller contract; 4 words are within
        // `dst` and unaligned `vst1q` stores are accepted by the ISA.
        #[expect(unsafe_code)]
        unsafe {
            use std::arch::aarch64::{vdupq_n_u32, vst1q_u32};
            let v = vdupq_n_u32(color);
            vst1q_u32(dst.as_mut_ptr(), v);
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        for px in dst.iter_mut().take(4) {
            *px = color;
        }
    }
}

/// 4-pixel SRCALPHA/INVSRCALPHA/ADD blend, byte-identical to the D3D9 FFP
/// fragment math in `d3d9_render.rs::blend_fragment` for
/// `SRCBLEND=SRCALPHA, DESTBLEND=INVSRCALPHA, BLENDOP=ADD`:
/// `out = (src * a + dst * (255 - a)) >> 8`, clamped to 0..255 per channel.
///
/// `src`/`dst` are 4 independent `0RGB` pixels (gather/scatter — the accepted
/// pixels need not be contiguous in the backbuffer); `src_alpha` the four
/// alpha bytes. Products are ≤ 255·255 = 65025 and sums ≤ 130050, so the
/// u32 lanes cannot overflow and the result equals the scalar `>>8` exactly.
pub fn blend_0rgb_4x(dst: &mut [u32; 4], src: &[u32; 4], src_alpha: &[u8; 4]) {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: all loads/stores are 16-byte aligned to the stack arrays or
        // caller-provided 4-word arrays (unaligned-safe); every lane is
        // masked to 8 bits before the fixed-point math, so no lane can
        // overflow u32.
        #[expect(unsafe_code)]
        unsafe {
            use std::arch::aarch64::{
                vaddq_u32, vandq_u32, vdupq_n_u32, vld1q_u32, vminq_u32, vmulq_u32, vorrq_u32,
                vshlq_n_u32, vshrq_n_u32, vst1q_u32, vsubq_u32,
            };
            let ff = vdupq_n_u32(0xFF);
            let src_v = vld1q_u32(src.as_ptr());
            let dst_v = vld1q_u32(dst.as_ptr());
            // Build the alpha vector from the 4 bytes (no OOB: vld1q_u32 on
            // a 4-byte array would read 16 bytes).
            let [a0, a1, a2, a3] = *src_alpha;
            let alpha_arr = [u32::from(a0), u32::from(a1), u32::from(a2), u32::from(a3)];
            let alpha_v = vld1q_u32(alpha_arr.as_ptr());
            let inv_alpha_v = vsubq_u32(vdupq_n_u32(255), alpha_v);

            // Per-channel: (src_ch * a + dst_ch * (255 - a)) >> 8, clamped.
            let src_r = vandq_u32(vshrq_n_u32(src_v, 16), ff);
            let dst_r = vandq_u32(vshrq_n_u32(dst_v, 16), ff);
            let r = vminq_u32(
                vshrq_n_u32(
                    vaddq_u32(vmulq_u32(src_r, alpha_v), vmulq_u32(dst_r, inv_alpha_v)),
                    8,
                ),
                ff,
            );
            let src_g = vandq_u32(vshrq_n_u32(src_v, 8), ff);
            let dst_g = vandq_u32(vshrq_n_u32(dst_v, 8), ff);
            let g = vminq_u32(
                vshrq_n_u32(
                    vaddq_u32(vmulq_u32(src_g, alpha_v), vmulq_u32(dst_g, inv_alpha_v)),
                    8,
                ),
                ff,
            );
            let src_b = vandq_u32(src_v, ff);
            let dst_b = vandq_u32(dst_v, ff);
            let b = vminq_u32(
                vshrq_n_u32(
                    vaddq_u32(vmulq_u32(src_b, alpha_v), vmulq_u32(dst_b, inv_alpha_v)),
                    8,
                ),
                ff,
            );

            let out = vorrq_u32(vshlq_n_u32(r, 16), vorrq_u32(vshlq_n_u32(g, 8), b));
            vst1q_u32(dst.as_mut_ptr(), out);
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        let [mut d0, mut d1, mut d2, mut d3] = *dst;
        let [s0, s1, s2, s3] = *src;
        let [a0, a1, a2, a3] = *src_alpha;
        let blend = |s: u32, d: &mut u32, a: u8| {
            let a = u32::from(a);
            let inv = 255_u32.saturating_sub(a);
            let ch = |sc: u32, dc: u32| {
                (sc.saturating_mul(a).saturating_add(dc.saturating_mul(inv)) >> 8).min(255)
            };
            *d = (ch((s >> 16) & 0xFF, (*d >> 16) & 0xFF) << 16)
                | (ch((s >> 8) & 0xFF, (*d >> 8) & 0xFF) << 8)
                | ch(s & 0xFF, *d & 0xFF);
        };
        blend(s0, &mut d0, a0);
        blend(s1, &mut d1, a1);
        blend(s2, &mut d2, a2);
        blend(s3, &mut d3, a3);
        *dst = [d0, d1, d2, d3];
    }
}

/// 4-pixel MODULATE color-op multiply, byte-identical to
/// `d3d9_render.rs::eval_color_op` for `D3DTOP_MODULATE`:
/// `out = (a * b) >> 8` per channel, clamped to 0..255.
pub fn mul_0rgb_4x(dst: &mut [u32; 4], src: &[u32; 4]) {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: as `blend_0rgb_4x` — 16-byte loads/stores over 4-word
        // arrays, lanes masked to 8 bits, products ≤ 65025 fit u32.
        #[expect(unsafe_code)]
        unsafe {
            use std::arch::aarch64::{
                vandq_u32, vdupq_n_u32, vld1q_u32, vminq_u32, vmulq_u32, vorrq_u32, vshlq_n_u32,
                vshrq_n_u32, vst1q_u32,
            };
            let ff = vdupq_n_u32(0xFF);
            let src_v = vld1q_u32(src.as_ptr());
            let dst_v = vld1q_u32(dst.as_ptr());
            let r = vminq_u32(
                vshrq_n_u32(
                    vmulq_u32(
                        vandq_u32(vshrq_n_u32(src_v, 16), ff),
                        vandq_u32(vshrq_n_u32(dst_v, 16), ff),
                    ),
                    8,
                ),
                ff,
            );
            let g = vminq_u32(
                vshrq_n_u32(
                    vmulq_u32(
                        vandq_u32(vshrq_n_u32(src_v, 8), ff),
                        vandq_u32(vshrq_n_u32(dst_v, 8), ff),
                    ),
                    8,
                ),
                ff,
            );
            let b = vminq_u32(
                vshrq_n_u32(vmulq_u32(vandq_u32(src_v, ff), vandq_u32(dst_v, ff)), 8),
                ff,
            );
            let out = vorrq_u32(vshlq_n_u32(r, 16), vorrq_u32(vshlq_n_u32(g, 8), b));
            vst1q_u32(dst.as_mut_ptr(), out);
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        let [mut d0, mut d1, mut d2, mut d3] = *dst;
        let [s0, s1, s2, s3] = *src;
        let mul = |s: u32, d: &mut u32| {
            let ch = |sc: u32, dc: u32| (sc.saturating_mul(dc) >> 8).min(255);
            *d = (ch((s >> 16) & 0xFF, (*d >> 16) & 0xFF) << 16)
                | (ch((s >> 8) & 0xFF, (*d >> 8) & 0xFF) << 8)
                | ch(s & 0xFF, *d & 0xFF);
        };
        mul(s0, &mut d0);
        mul(s1, &mut d1);
        mul(s2, &mut d2);
        mul(s3, &mut d3);
        *dst = [d0, d1, d2, d3];
    }
}

#[cfg(test)]
mod tests {
    use super::{blend_0rgb_4x, fill_0rgb_4x, mask_bgra_to_0rgb, mul_0rgb_4x, stretch_nearest};

    /// Deterministic 32-bit LCG (no external dep in tests).
    fn lcg(state: &mut u32) -> u32 {
        *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *state
    }

    /// Reference SRCALPHA/INVSRCALPHA/ADD blend — the exact scalar formula
    /// `d3d9_render.rs::blend_fragment` uses for these factors.
    fn blend_ref(s: u32, d: u32, a: u8) -> u32 {
        let a = u32::from(a);
        let inv = 255_u32.saturating_sub(a);
        let ch = |sc: u32, dc: u32| {
            (sc.saturating_mul(a).saturating_add(dc.saturating_mul(inv)) >> 8).min(255)
        };
        (ch((s >> 16) & 0xFF, (d >> 16) & 0xFF) << 16)
            | (ch((s >> 8) & 0xFF, (d >> 8) & 0xFF) << 8)
            | ch(s & 0xFF, d & 0xFF)
    }

    /// Reference MODULATE `(a*b)>>8` per channel.
    fn mul_ref(s: u32, d: u32) -> u32 {
        let ch = |sc: u32, dc: u32| (sc.saturating_mul(dc) >> 8).min(255);
        (ch((s >> 16) & 0xFF, (d >> 16) & 0xFF) << 16)
            | (ch((s >> 8) & 0xFF, (d >> 8) & 0xFF) << 8)
            | ch(s & 0xFF, d & 0xFF)
    }

    #[test]
    fn blend_4x_matches_reference_formula() {
        let mut state = 0x00C0_FFEE_u32;
        for _ in 0..500 {
            let src = [
                lcg(&mut state) & 0x00FF_FFFF,
                lcg(&mut state) & 0x00FF_FFFF,
                lcg(&mut state) & 0x00FF_FFFF,
                lcg(&mut state) & 0x00FF_FFFF,
            ];
            let dst = [
                lcg(&mut state) & 0x00FF_FFFF,
                lcg(&mut state) & 0x00FF_FFFF,
                lcg(&mut state) & 0x00FF_FFFF,
                lcg(&mut state) & 0x00FF_FFFF,
            ];
            let alpha = [
                u8::try_from(lcg(&mut state) & 0xFF).unwrap_or(0),
                u8::try_from(lcg(&mut state) & 0xFF).unwrap_or(0),
                u8::try_from(lcg(&mut state) & 0xFF).unwrap_or(0),
                u8::try_from(lcg(&mut state) & 0xFF).unwrap_or(0),
            ];
            let mut got = dst;
            blend_0rgb_4x(&mut got, &src, &alpha);
            for i in 0..4 {
                let want = blend_ref(
                    src.get(i).copied().unwrap_or(0),
                    dst.get(i).copied().unwrap_or(0),
                    alpha.get(i).copied().unwrap_or(0),
                );
                let g = got.get(i).copied().unwrap_or(0);
                assert_eq!(g, want, "blend mismatch: px {i}");
            }
        }
    }

    #[test]
    fn mul_4x_matches_reference_formula() {
        let mut state = 0x0000_F00D_u32;
        for _ in 0..500 {
            let src = [
                lcg(&mut state) & 0x00FF_FFFF,
                lcg(&mut state) & 0x00FF_FFFF,
                lcg(&mut state) & 0x00FF_FFFF,
                lcg(&mut state) & 0x00FF_FFFF,
            ];
            let dst = [
                lcg(&mut state) & 0x00FF_FFFF,
                lcg(&mut state) & 0x00FF_FFFF,
                lcg(&mut state) & 0x00FF_FFFF,
                lcg(&mut state) & 0x00FF_FFFF,
            ];
            let mut got = dst;
            mul_0rgb_4x(&mut got, &src);
            for i in 0..4 {
                let want = mul_ref(
                    src.get(i).copied().unwrap_or(0),
                    dst.get(i).copied().unwrap_or(0),
                );
                let g = got.get(i).copied().unwrap_or(0);
                assert_eq!(g, want, "mul mismatch: px {i}");
            }
        }
    }

    #[test]
    fn fill_4x_writes_all_four() {
        let mut buf = [0x0011_2233u32, 0x0044_5566, 0x0077_8899, 0x00AA_BBCC];
        fill_0rgb_4x(&mut buf, 0x00DE_ADBE);
        assert_eq!(buf, [0x00DE_ADBE; 4]);
    }

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
        tracing::error!(
            "mask_bgra_to_0rgb: {px} px in {elapsed:?} ({:.2} ns/px)",
            elapsed.as_secs_f64() * 1e9 / px_f
        );
        // Sanity: every pixel's alpha byte cleared, RGB preserved.
        assert_eq!(dst.first(), Some(&0x0012_1212));
        assert_eq!(dst.last(), Some(&0x0012_1212));
    }
}
