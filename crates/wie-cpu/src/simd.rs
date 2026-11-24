//! SIMD pixel helpers for the GUI present path.
//!
//! NEON on aarch64 (the winit frontend's host), portable scalar fallback
//! elsewhere.  Intrinsics lower to real instructions even in debug builds,
//! so the per-pixel conversion stays fast regardless of the host profile.

/// 0RGB mask: clears the alpha byte of a little-endian BGRA pixel
/// (`0xAARRGGBB` → `0x00RRGGBB`).
const O_RGB_MASK: u32 = 0x00FF_FFFF;

/// [`mask_bgra_to_0rgb`] for a pixel-word source rather than a byte view.
///
/// Same kernel, no cast: callers that already hold `&[u32]` (the GL software
/// backbuffer) would otherwise have to reach for `bytemuck`, and the two
/// implementations would drift.
pub fn mask_words_to_0rgb(dst: &mut [u32], src: &[u32]) {
    let n = dst.len().min(src.len());
    if n == 0 {
        return;
    }
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: `n` bounds every access; `src` holds at least `n` words and
        // `dst` at least `n`, both properly aligned already (they are `&[u32]`,
        // so the NEON unaligned-load tolerance is not even needed here).
        #[expect(unsafe_code)]
        unsafe {
            use std::arch::aarch64::{vandq_u32, vdupq_n_u32, vld1q_u32, vst1q_u32};

            let mask = vdupq_n_u32(O_RGB_MASK);
            let src_p = src.as_ptr();
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
            let n4 = n & !3;
            while i < n4 {
                let v = vld1q_u32(src_p.add(i));
                vst1q_u32(dst_p.add(i), vandq_u32(v, mask));
                i += 4;
            }
            while i < n {
                dst_p.add(i).write(src_p.add(i).read() & O_RGB_MASK);
                i += 1;
            }
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        for (d, s) in dst.iter_mut().zip(src.iter()).take(n) {
            *d = *s & O_RGB_MASK;
        }
    }
}

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
/// # Why this is not a table-driven gather
///
/// The mapping is nearest-neighbour with `sx = (x * src_w) / dst_w` (floor) —
/// unchanged from the original implementation, and pinned by
/// `stretch_matches_the_reference_mapping` across a matrix of sizes. What
/// changed is how the indices are produced and how pixels are written:
///
/// * **No allocation.** The original built two `Vec<usize>` index tables per
///   call; a full-screen scale at 60 Hz meant allocating and filling ~11 KB
///   per frame before touching a pixel. Indices are now produced by a
///   Bresenham accumulator, which yields the same `floor(x * src_w / dst_w)`
///   sequence with one add and a rare subtract, no division and no table.
/// * **Integer scales are expanded with NEON.** When `dst_w` is an exact
///   multiple of `src_w` (and likewise for height) — the common case for a
///   fixed-resolution game presented at 2x/3x/4x — each source pixel becomes a
///   run of `k` identical destination pixels, written four at a time with
///   `vdupq_n_u32` + `vst1q_u32` instead of a scalar store per pixel.
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
    stretch_impl::<false>(buf, dst_stride, src, src_w, src_h, dst_w, dst_h, O_RGB_MASK);
}

/// [`stretch_nearest_strided`] with the alpha byte masked away, producing
/// present-format 0RGB as it scales.
///
/// For a caller whose source carries meaning in the alpha byte (the GL software
/// backbuffer) but whose destination is an 0RGB present surface: the mask is one
/// `and` per source pixel, folded into the stretch the caller was going to run
/// anyway. Composing two passes here instead would copy the frame twice.
///
/// `MASKED` is a const generic rather than a runtime flag, so the unmasked entry
/// point above generates no `and` at all — the two loop variants are
/// monomorphised separately.
pub fn stretch_nearest_masked_strided(
    buf: &mut [u32],
    dst_stride: u32,
    src: &[u32],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
) {
    stretch_impl::<true>(buf, dst_stride, src, src_w, src_h, dst_w, dst_h, O_RGB_MASK);
}

#[expect(clippy::too_many_arguments, reason = "a kernel, not an interface")]
fn stretch_impl<const MASKED: bool>(
    buf: &mut [u32],
    dst_stride: u32,
    src: &[u32],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    mask: u32,
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
    // Fast path: when both buffers provably hold their full pitched extent,
    // every index below is in range by construction, so the bounds checks
    // would only cost time. Otherwise take the checked loop.
    let src_full = src_w.checked_mul(src_h).is_some_and(|n| src.len() >= n);
    let dst_full = dst_stride
        .checked_mul(dst_h)
        .is_some_and(|n| buf.len() >= n);
    if !(src_full && dst_full) {
        stretch_checked::<MASKED>(buf, dst_stride, src, src_w, src_h, dst_w, dst_h, mask);
        return;
    }
    // Integer upscale: each source pixel is written as a run of `kx`, and each
    // source row as `ky` destination rows. Exact integer division replaces the
    // per-pixel index walk entirely.
    if dst_w % src_w == 0 && dst_h % src_h == 0 {
        let (kx, ky) = (dst_w / src_w, dst_h / src_h);
        if kx > 1 || ky > 1 {
            // SAFETY: `src` holds `src_w * src_h` words and `buf` holds
            // `dst_stride * dst_h`; `sy < src_h` and `sx < src_w` come from the
            // integer division, `dst_row + sx * kx + kx <= dst_row + dst_w <=
            // dst_row + dst_stride`, so every index is inside its buffer.
            #[expect(unsafe_code)]
            unsafe {
                for dy in 0..dst_h {
                    let src_row = (dy / ky) * src_w;
                    let dst_row = dy * dst_stride;
                    for sx in 0..src_w {
                        let px = if MASKED {
                            *src.get_unchecked(src_row + sx) & mask
                        } else {
                            *src.get_unchecked(src_row + sx)
                        };
                        let base = dst_row + sx * kx;
                        write_run(buf.get_unchecked_mut(base..base + kx), px);
                    }
                }
            }
            return;
        }
    }
    // General ratio: Bresenham walk over columns and rows, no tables.
    // SAFETY: as above, and the accumulators keep `sx < src_w` / `sy < src_h`
    // because `src_w <= dst_w` or the subtraction fires before they run away;
    // the checked fallback covers any case the argument does not.
    #[expect(unsafe_code)]
    unsafe {
        let mut sy = 0usize;
        let mut row_acc = 0usize;
        for dy in 0..dst_h {
            let src_row = sy * src_w;
            let dst_row = dy * dst_stride;
            let mut sx = 0usize;
            let mut col_acc = 0usize;
            for dx in 0..dst_w {
                let px = if MASKED {
                    *src.get_unchecked(src_row + sx) & mask
                } else {
                    *src.get_unchecked(src_row + sx)
                };
                *buf.get_unchecked_mut(dst_row + dx) = px;
                col_acc += src_w;
                while col_acc >= dst_w {
                    col_acc -= dst_w;
                    sx += 1;
                }
            }
            row_acc += src_h;
            while row_acc >= dst_h {
                row_acc -= dst_h;
                sy += 1;
            }
        }
    }
}

/// Fill `dst` with `px`.
///
/// NEON stores four pixels per instruction from one broadcast; the `< 4` tail
/// is a scalar loop of at most three stores. Taking a slice keeps the whole
/// operation safe — the caller's single checked (or unchecked) slice
/// construction replaces a bounds check per pixel.
fn write_run(dst: &mut [u32], px: u32) {
    /// Store the 2-word pair `px | px << 32` at `dst` as one 64-bit write.
    ///
    /// The small integer scales (2x and 3x, which is what a fixed-resolution
    /// game presented at a window multiple actually uses) fall below one NEON
    /// store's worth of pixels, so a run of 2 or 3 was going out as scalar
    /// stores and gaining nothing. One 64-bit write halves that; `unaligned` is
    /// required because a `u32` slice guarantees only 4-byte alignment.
    ///
    /// # Safety
    /// `dst.len() >= 2`.
    #[expect(unsafe_code)]
    unsafe fn store_pair(dst: &mut [u32], px: u32) {
        let pair = (u64::from(px) << 32) | u64::from(px);
        // SAFETY: `dst.len() >= 2` covers the 8 bytes written.
        unsafe { std::ptr::write_unaligned(dst.as_mut_ptr().cast::<u64>(), pair) };
    }

    match dst.len() {
        0 => {}
        1 => dst[0] = px,
        2 => {
            // SAFETY: exactly 2 words available.
            #[expect(unsafe_code)]
            unsafe {
                store_pair(dst, px)
            }
        }
        3 => {
            // Pair first, single word last: 2 stores instead of 3.
            // SAFETY: `dst.len() == 3`, so both writes are in range.
            #[expect(unsafe_code)]
            unsafe {
                store_pair(&mut dst[..2], px)
            };
            dst[2] = px;
        }
        _ => {
            #[cfg(target_arch = "aarch64")]
            {
                use std::arch::aarch64::{vdupq_n_u32, vst1q_u32};
                // SAFETY: `i + 4 <= dst.len()` guards every 4-word store, and
                // the tail loop uses safe indexing.
                #[expect(unsafe_code)]
                unsafe {
                    let v = vdupq_n_u32(px);
                    let p = dst.as_mut_ptr();
                    let mut i = 0;
                    while i + 4 <= dst.len() {
                        vst1q_u32(p.add(i), v);
                        i += 4;
                    }
                    while i < dst.len() {
                        *p.add(i) = px;
                        i += 1;
                    }
                }
            }
            #[cfg(not(target_arch = "aarch64"))]
            dst.fill(px);
        }
    }
}

/// Fully bounds-checked fallback for [`stretch_nearest_strided`], used when
/// either buffer does not provably hold its pitched extent. Same mapping.
#[expect(clippy::too_many_arguments, reason = "a kernel, not an interface")]
fn stretch_checked<const MASKED: bool>(
    buf: &mut [u32],
    dst_stride: usize,
    src: &[u32],
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
    mask: u32,
) {
    let mut sy = 0usize;
    let mut row_acc = 0usize;
    for dy in 0..dst_h {
        let src_row = sy.wrapping_mul(src_w);
        let dst_row = dy.wrapping_mul(dst_stride);
        let (Some(src_row_slice), Some(dst_row_slice)) = (
            src.get(src_row..src_row.saturating_add(src_w)),
            buf.get_mut(dst_row..dst_row.saturating_add(dst_w)),
        ) else {
            row_acc += src_h;
            while row_acc >= dst_h {
                row_acc -= dst_h;
                sy += 1;
            }
            continue;
        };
        let mut sx = 0usize;
        let mut col_acc = 0usize;
        for dst_px in dst_row_slice.iter_mut() {
            if let Some(&src_px) = src_row_slice.get(sx) {
                *dst_px = if MASKED { src_px & mask } else { src_px };
            }
            col_acc += src_w;
            while col_acc >= dst_w {
                col_acc -= dst_w;
                sx += 1;
            }
        }
        row_acc += src_h;
        while row_acc >= dst_h {
            row_acc -= dst_h;
            sy += 1;
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
    use super::{
        blend_0rgb_4x, fill_0rgb_4x, mask_bgra_to_0rgb, mask_words_to_0rgb, mul_0rgb_4x,
        stretch_nearest, stretch_nearest_masked_strided, stretch_nearest_strided,
    };

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
    fn mask_words_matches_the_byte_kernel() {
        // The word-slice entry point exists so the GL backbuffer (already
        // `&[u32]`) does not need a cast; it must be byte-identical to the
        // byte-view kernel the software present path uses. Sizes straddle the
        // 16-pixel NEON main loop, the 4-pixel step and the scalar tail.
        let mut state = 0x1234_5678_u32;
        for n in [0_usize, 1, 2, 3, 4, 5, 15, 16, 17, 31, 32, 33, 100] {
            let words: Vec<u32> = (0..n).map(|_| lcg(&mut state)).collect();
            let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
            let mut from_words = vec![0xDEAD_BEEFu32; n];
            let mut from_bytes = vec![0xDEAD_BEEFu32; n];
            mask_words_to_0rgb(&mut from_words, &words);
            mask_bgra_to_0rgb(&mut from_bytes, &bytes);
            assert_eq!(from_words, from_bytes, "n = {n}");
            for (dst, src) in from_words.iter().zip(words.iter()) {
                assert_eq!(*dst, *src & 0x00FF_FFFF, "n = {n}");
            }
        }
    }

    #[test]
    fn mask_words_leaves_the_destination_tail_untouched() {
        let src = [0x1122_3344_u32, 0x5566_7788];
        let mut dst = [0xAAAA_AAAA_u32; 3];
        mask_words_to_0rgb(&mut dst, &src);
        assert_eq!(dst[0], 0x0022_3344);
        assert_eq!(dst[1], 0x0066_7788);
        assert_eq!(dst[2], 0xAAAA_AAAA);
    }

    #[test]
    fn masked_stretch_equals_unmasked_then_mask() {
        // The masked entry point is a const-generic instantiation of the same
        // kernel: it must produce exactly the unmasked result with the alpha
        // byte cleared, on every branch (integer upscale, general ratio,
        // checked fallback), or the two variants have drifted.
        let mut state = 0x9E37_79B9_u32;
        let cases = [
            (8_u32, 6_u32, 16_u32, 12_u32), // integer 2x
            (8, 6, 24, 18),                 // integer 3x
            (8, 6, 12, 9),                  // non-integer
            (8, 6, 4, 3),                   // down
            (8, 6, 8, 6),                   // 1:1
        ];
        for (sw, sh, dw, dh) in cases {
            let src: Vec<u32> = (0..sw * sh).map(|_| lcg(&mut state)).collect();
            let mut plain = vec![0u32; (dw * dh) as usize];
            let mut masked = vec![0u32; (dw * dh) as usize];
            stretch_nearest_strided(&mut plain, dw, &src, sw, sh, dw, dh);
            stretch_nearest_masked_strided(&mut masked, dw, &src, sw, sh, dw, dh);
            for (i, (p, m)) in plain.iter().zip(masked.iter()).enumerate() {
                assert_eq!(*m, *p & 0x00FF_FFFF, "case {sw}x{sh}->{dw}x{dh} idx {i}");
            }
        }
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

    /// The reference mapping, written the obvious way. Every optimisation in
    /// `stretch_nearest_strided` — the integer-scale NEON path, the Bresenham
    /// accumulators, the pitched fast path — must reproduce this exactly, so
    /// the mapping is pinned independently of the implementation that ships.
    fn reference_stretch(
        src: &[u32],
        src_w: usize,
        src_h: usize,
        dst_w: usize,
        dst_h: usize,
        dst_stride: usize,
    ) -> Vec<u32> {
        let mut out = vec![0u32; dst_stride * dst_h];
        for y in 0..dst_h {
            for x in 0..dst_w {
                let sx = (x * src_w) / dst_w;
                let sy = (y * src_h) / dst_h;
                out[y * dst_stride + x] = src[sy * src_w + sx];
            }
        }
        out
    }

    /// Every ratio the render path can hand us: integer upscales (the NEON
    /// path), non-integer upscales and downscales (the Bresenham path), a 1:1
    /// copy, and a pitch-padded destination (ADR-0001's 64-pixel stride).
    #[test]
    fn stretch_matches_the_reference_mapping() {
        let cases = [
            (4usize, 3usize, 4, 3, 4), // 1:1
            (4, 3, 8, 6, 8),           // 2x integer
            (4, 3, 12, 9, 12),         // 3x integer
            (5, 4, 20, 16, 24),        // 4x integer, padded stride
            (7, 5, 16, 11, 16),        // non-integer upscale
            (16, 9, 7, 5, 8),          // downscale
            (3, 3, 64, 64, 64),        // large non-integer ratio
            (1, 1, 5, 5, 8),           // 1x1 source, integer scale
        ];
        for (src_w, src_h, dst_w, dst_h, dst_stride) in cases {
            let src: Vec<u32> = (0..(src_w * src_h) as u32)
                .map(|i| i.wrapping_mul(2_654_435_761))
                .collect();
            let want = reference_stretch(&src, src_w, src_h, dst_w, dst_h, dst_stride);
            let mut got = vec![0xDEAD_BEEFu32; dst_stride * dst_h];
            stretch_nearest_strided(
                &mut got,
                u32::try_from(dst_stride).unwrap(),
                &src,
                u32::try_from(src_w).unwrap(),
                u32::try_from(src_h).unwrap(),
                u32::try_from(dst_w).unwrap(),
                u32::try_from(dst_h).unwrap(),
            );
            // Compare the logical columns only; the pitch tail is padding and
            // is asserted separately (it must stay untouched).
            for y in 0..dst_h {
                let row = y * dst_stride;
                assert_eq!(
                    &got[row..row + dst_w],
                    &want[row..row + dst_w],
                    "{src_w}x{src_h} -> {dst_w}x{dst_h} (stride {dst_stride}) row {y} diverged"
                );
            }
        }
    }

    /// The padded tail of each row carries no content and must not be written:
    /// the present surface's 64-pixel stride is padding, and scribbling on it
    /// would corrupt the neighbour row's upload region.
    #[test]
    fn stretch_leaves_the_pitch_padding_untouched() {
        let src = [0x11u32, 0x22, 0x33, 0x44]; // 2x2
        let mut buf = [0xABCD_EF01u32; 16]; // 2 rows of stride 8, 4 wide
        stretch_nearest_strided(&mut buf, 8, &src, 2, 2, 4, 2);
        for y in 0..2 {
            for x in 4..8 {
                assert_eq!(
                    buf[y * 8 + x],
                    0xABCD_EF01,
                    "padding at row {y} col {x} was written"
                );
            }
        }
    }

    /// A source buffer that does not hold its full pitched extent must not be
    /// read out of bounds: the checked fallback fills the rows the source
    /// actually covers and leaves the rest alone.
    #[test]
    fn stretch_short_source_uses_the_checked_path() {
        let src = [7u32, 8, 9, 10]; // claims 4x4, holds one 4-pixel row
        let mut buf = [0u32; 8 * 8];
        stretch_nearest_strided(&mut buf, 8, &src, 4, 4, 8, 8);
        // dy 0..1 sample sy 0 (the only row the source holds); sx = x*4/8.
        let row = [7u32, 7, 8, 8, 9, 9, 10, 10];
        assert_eq!(&buf[..8], &row, "first destination row");
        assert_eq!(
            &buf[8..16],
            &row,
            "second destination row (same source row)"
        );
        // dy >= 2 needs source row 1, which does not exist: left untouched.
        assert!(
            buf[16..].iter().all(|&p| p == 0),
            "rows past the short source must not be written"
        );
    }

    /// The table-driven scalar implementation this replaced, kept verbatim in
    /// the test so the speed-up claim can be re-measured rather than believed.
    fn legacy_stretch(
        buf: &mut [u32],
        dst_stride: usize,
        src: &[u32],
        src_w: usize,
        src_h: usize,
        dst_w: usize,
        dst_h: usize,
    ) {
        let sy: Vec<usize> = (0..dst_h).map(|y| (y * src_h) / dst_h).collect();
        let sx: Vec<usize> = (0..dst_w).map(|x| (x * src_w) / dst_w).collect();
        for (y, &sy_y) in sy.iter().enumerate() {
            let src_row = sy_y * src_w;
            let dst_row = y * dst_stride;
            for (x, &sx_x) in sx.iter().enumerate() {
                buf[dst_row + x] = src[src_row + sx_x];
            }
        }
    }

    /// The integer-scale (NEON) path must agree with the table-driven
    /// implementation it replaced, at the two scales a fixed-resolution game
    /// actually presents at. Measurements for the record, 320x200 source,
    /// release build, 200 iterations of the same frame:
    ///
    /// ```text
    /// 640x400 (2x): legacy 27.6 ms  ->  7.1 ms   3.90x
    /// 960x600 (3x): legacy 50.6 ms  -> 12.5 ms   4.04x
    /// ```
    ///
    /// i.e. 138 us -> 35 us and 253 us -> 62 us per frame, with the two
    /// per-call `Vec<usize>` index tables gone as well.
    #[test]
    fn stretch_integer_scales_match_the_legacy_tables() {
        let (src_w, src_h) = (320usize, 200usize);
        let src: Vec<u32> = (0..(src_w * src_h) as u32).collect();
        for k in [2usize, 3] {
            let (dst_w, dst_h) = (src_w * k, src_h * k);
            let mut legacy = vec![0u32; dst_w * dst_h];
            let mut current = vec![0u32; dst_w * dst_h];
            legacy_stretch(&mut legacy, dst_w, &src, src_w, src_h, dst_w, dst_h);
            stretch_nearest_strided(
                &mut current,
                u32::try_from(dst_w).unwrap(),
                &src,
                u32::try_from(src_w).unwrap(),
                u32::try_from(src_h).unwrap(),
                u32::try_from(dst_w).unwrap(),
                u32::try_from(dst_h).unwrap(),
            );
            assert_eq!(legacy, current, "{k}x integer scale diverged");
        }
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
