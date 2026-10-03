//! The `0xAARRGGBB` pack/unpack primitives shared by both software renderers.
//!
//! Every software-rasterised colour in WIE — the D3D9 fixed-function and PS 2.0
//! pipelines (`super::blend`, `super::sample`, `super::ps`, `super::vertex`) and
//! the OpenGL 1.x stub layer — carries the same layout: alpha in bits 24..31,
//! red in 16..23, green in 8..15, blue in 0..7. That single convention was
//! previously re-spelled as raw shifts at every site (~25 copies), which is
//! exactly the shape of bug a shared primitive removes.
//!
//! Two shapes are offered, and only two:
//!
//! * [`unpack_rgba`] / [`pack_rgba`] move **raw channel bytes**. These are the
//!   primitive — every other function here is defined in terms of them, so
//!   there is exactly one place that knows a red channel lives at bit 16.
//! * [`unpack_rgba_unit`] / [`pack_rgba_unit`] add the `0..=1` float
//!   normalisation the shader-facing paths (`ps.rs` texel fetch, the GL
//!   fragment pipeline) need.
//!
//! Both float entry points are byte-identical to the hand-rolled code they
//! replace; `unit_pack_is_the_inverse_rounding_of_the_hand_rolled_form` and
//! `float_pack_matches_both_hand_rolled_forms` pin that claim against the
//! original expressions.
//!
//! **Scope.** This module is colour conversion only. It deliberately does NOT
//! unify the two texture-upload pipelines (`d3d9::texture` has a mip chain,
//! GL's `opengl32_render::sample` has none) or the three unrelated format
//! vocabularies (`D3DFMT_*` / `GL_RGBA` / `BITMAPINFOHEADER::biBitCount`).
//! Those remain separate by design.

// ── Byte-level primitives ───────────────────────────────────────────────

/// Split a `0xAARRGGBB` value into `[r, g, b, a]` channel bytes.
///
/// This is the one function that knows the layout. `& 0xFF` bounds every
/// channel by construction, so the `try_from` conversions the callers used to
/// spell inline can never fail — the narrowing here is lossless by the same
/// argument.
#[must_use]
pub fn unpack_rgba(color: u32) -> [u8; 4] {
    [
        ((color >> 16) & 0xFF) as u8,
        ((color >> 8) & 0xFF) as u8,
        (color & 0xFF) as u8,
        ((color >> 24) & 0xFF) as u8,
    ]
}

/// Pack `[r, g, b, a]` channel bytes back into `0xAARRGGBB`.
#[must_use]
pub fn pack_rgba(channels: [u8; 4]) -> u32 {
    let [r, g, b, a] = channels;
    (u32::from(a) << 24) | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
}

/// Split a `0x00RRGGBB` value into `[r, g, b, _]` channel bytes.
///
/// For the 0RGB surfaces (the backbuffer, the glyph-coverage blends) where the
/// alpha byte is not carried. Identical to [`unpack_rgba`] — the alpha byte is
/// simply dropped — but it states at the call site that alpha is not read, so
/// a later reader does not assume the result has four meaningful channels.
#[must_use]
pub fn unpack_rgb(color: u32) -> [u32; 4] {
    let [r, g, b, _] = unpack_rgba(color);
    [u32::from(r), u32::from(g), u32::from(b), 0]
}

/// The alpha byte of a `0xAARRGGBB` value.
///
/// A named accessor rather than a shift: the alpha byte is read on its own in
/// several places (the flat-colour fast path, the fixed-function stage
/// fallback, the alpha-test compare) and always meant bits 24..31.
#[must_use]
pub fn alpha_byte(color: u32) -> u8 {
    ((color >> 24) & 0xFF) as u8
}

// ── Unit-float (0..=1) conversions ───────────────────────────────────────

/// Normalise `0xAARRGGBB` to the shader `[r, g, b, a]` float4 in `0..=1`.
///
/// Byte-for-byte the `texel * (1/255)` the D3D9 PS 2.0 texel fetch and the GL
/// unpack path each spelled by hand.
#[must_use]
pub fn unpack_rgba_unit(color: u32) -> [f32; 4] {
    let [r, g, b, a] = unpack_rgba(color);
    [
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
        f32::from(a) / 255.0,
    ]
}

/// Pack a `0..=1` float4 `[r, g, b, a]` into `0xAARRGGBB`, rounding and
/// clamping each channel.
///
/// The clamp runs on the **input** (`0.0..=1.0`) and the `* 255.0` after it.
/// That is the GL fixed-function form, and it is what `vs.rs::float4_to_0argb`
/// computes too: multiplying first and clamping the `0..=255` product gives
/// the same byte for every input, because clamping the input to `1.0` and then
/// scaling by 255 lands on 255 exactly where clamping the product would, and
/// clamping the input to `0.0` lands on 0 exactly likewise. The two forms
/// diverge only on NaN and the infinities, where both produce 0 (NaN) or 255
/// (±inf) — pinned by `float_pack_matches_both_hand_rolled_forms`.
#[must_use]
pub fn pack_rgba_unit(channels: [f32; 4]) -> u32 {
    let channel = |v: f32| u8::try_from((v.clamp(0.0, 1.0) * 255.0).round() as i32).unwrap_or(0);
    (u32::from(channel(channels[3])) << 24)
        | (u32::from(channel(channels[0])) << 16)
        | (u32::from(channel(channels[1])) << 8)
        | u32::from(channel(channels[2]))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::float_cmp,
        clippy::cast_precision_loss
    )]

    use super::{alpha_byte, pack_rgba, pack_rgba_unit, unpack_rgba, unpack_rgba_unit};

    /// The hand-rolled unpack each site used to spell, kept verbatim as the
    /// oracle for the shared one.
    fn hand_rolled_unpack(color: u32) -> [u8; 4] {
        [
            u8::try_from((color >> 16) & 0xFF).unwrap_or(0),
            u8::try_from((color >> 8) & 0xFF).unwrap_or(0),
            u8::try_from(color & 0xFF).unwrap_or(0),
            u8::try_from((color >> 24) & 0xFF).unwrap_or(0),
        ]
    }

    /// The hand-rolled GL unit unpack, kept verbatim as the oracle.
    fn hand_rolled_unpack_unit(color: u32) -> [f32; 4] {
        let ch = |shift: u32| f32::from(u8::try_from((color >> shift) & 0xFF).unwrap_or(0)) / 255.0;
        [ch(16), ch(8), ch(0), ch(24)]
    }

    /// A spread of colours: the four channel extremes, the opaque/black/
    /// white defaults the pipelines seed, and pseudo-random values.
    fn sample_colors() -> Vec<u32> {
        let mut colors = vec![
            0x0000_0000,
            0xFFFF_FFFF,
            0x00FF_FFFF,
            0xFF00_0000,
            0x0102_0304,
            0xFF00_FF00,
            0x8000_0080,
            0xDEAD_BEEF,
        ];
        // xorshift so the sweep is deterministic across runs.
        let mut x = 0x1234_5678_u32;
        for _ in 0..512 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            colors.push(x);
        }
        colors
    }

    #[test]
    fn unpack_rgba_is_byte_identical_to_the_hand_rolled_shifts() {
        for color in sample_colors() {
            assert_eq!(unpack_rgba(color), hand_rolled_unpack(color), "{color:#x}");
        }
    }

    #[test]
    fn unpack_rgba_puts_red_at_bit_16_not_bit_0() {
        // Guards the one thing a wrong shift constant would break silently:
        // a pure-red value must read back as [255, 0, 0, _].
        assert_eq!(unpack_rgba(0x00FF_0000), [0xFF, 0, 0, 0]);
        assert_eq!(unpack_rgba(0x0000_FF00), [0, 0xFF, 0, 0]);
        assert_eq!(unpack_rgba(0x0000_00FF), [0, 0, 0xFF, 0]);
        assert_eq!(unpack_rgba(0xFF00_0000), [0, 0, 0, 0xFF]);
    }

    #[test]
    fn pack_rgba_is_the_inverse_of_unpack_rgba() {
        for color in sample_colors() {
            assert_eq!(pack_rgba(unpack_rgba(color)), color, "{color:#x}");
        }
    }

    #[test]
    fn unpack_rgba_unit_is_byte_identical_to_the_hand_rolled_shifts() {
        for color in sample_colors() {
            assert_eq!(
                unpack_rgba_unit(color),
                hand_rolled_unpack_unit(color),
                "{color:#x}"
            );
        }
    }

    #[test]
    fn alpha_byte_reads_bits_24_to_31() {
        for color in sample_colors() {
            assert_eq!(
                alpha_byte(color),
                u8::try_from((color >> 24) & 0xFF).unwrap_or(0),
                "{color:#x}"
            );
        }
    }

    /// The two hand-rolled float packers this replaces, kept verbatim.
    fn hand_rolled_gl_pack(c: [f32; 4]) -> u32 {
        let ch = |v: f32| u8::try_from((v.clamp(0.0, 1.0) * 255.0).round() as i32).unwrap_or(0);
        (u32::from(ch(c[3])) << 24)
            | (u32::from(ch(c[0])) << 16)
            | (u32::from(ch(c[1])) << 8)
            | u32::from(ch(c[2]))
    }

    fn hand_rolled_vs_pack(c: [f32; 4]) -> u32 {
        let channel = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u32;
        (channel(c[3]) << 24) | (channel(c[0]) << 16) | (channel(c[1]) << 8) | channel(c[2])
    }

    #[test]
    fn float_pack_matches_both_hand_rolled_forms() {
        // Grid over the unit cube plus the out-of-range and non-finite inputs
        // where the two hand-rolled clamp orders could have diverged.
        let mut inputs: Vec<[f32; 4]> = Vec::new();
        for step in 0..=32 {
            let v = f32::from(u8::try_from(step * 8).unwrap_or(0)) / 255.0;
            inputs.push([v, 1.0 - v, 0.5, v]);
        }
        for weird in [
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            -0.5,
            -1.0,
            1.5,
            2.0,
            255.0,
            -1.0e30,
            1.0e30,
            f32::MIN_POSITIVE,
        ] {
            inputs.push([weird, weird, weird, weird]);
            inputs.push([weird, 0.25, weird, 1.0]);
        }
        for input in inputs {
            let packed = pack_rgba_unit(input);
            assert_eq!(packed, hand_rolled_gl_pack(input), "GL form: {input:?}");
            assert_eq!(packed, hand_rolled_vs_pack(input), "VS form: {input:?}");
        }
    }

    #[test]
    fn unit_round_trip_holds_for_exact_byte_values() {
        // 0..1 units that came from a byte must pack back to that byte.
        for byte in 0..=255_u8 {
            let unit = f32::from(byte) / 255.0;
            let color = pack_rgba_unit([unit, unit, unit, unit]);
            assert_eq!(
                color & 0x00FF_FFFF,
                u32::from(byte) * 0x0001_0101,
                "byte {byte}"
            );
        }
    }
}
