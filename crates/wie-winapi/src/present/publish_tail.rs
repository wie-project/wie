//! Shared blit tail for the D3D9 capture and legacy in-handler present paths.

/// Blit a whole 0RGB frame into a destination surface buffer.
///
/// A frame sized exactly like the destination copies row-major (one
/// `copy_from_slice` when the pitch matches, per-row otherwise — the surface
/// pitch may be 64-padded, the source frame is not); anything else is
/// nearest-neighbour stretched to the destination dimensions.
pub(crate) fn blit_frame_into(
    dst: &mut [u32],
    dst_stride: u32,
    dst_width: u32,
    dst_height: u32,
    frame: &[u32],
    frame_width: u32,
    frame_height: u32,
) {
    if frame_width == dst_width && frame_height == dst_height {
        if dst_stride == dst_width {
            let n = dst.len().min(frame.len());
            if let (Some(d), Some(s)) = (dst.get_mut(..n), frame.get(..n)) {
                d.copy_from_slice(s);
            }
        } else {
            // Pitched destination: copy each logical row at its stride.
            let stride = usize::try_from(dst_stride).unwrap_or(0);
            let width = usize::try_from(dst_width).unwrap_or(0);
            let height = usize::try_from(dst_height).unwrap_or(0);
            for row in 0..height {
                let src_start = row.saturating_mul(width);
                let dst_start = row.saturating_mul(stride);
                let (Some(src), Some(d)) = (
                    frame.get(src_start..src_start.saturating_add(width)),
                    dst.get_mut(dst_start..dst_start.saturating_add(width)),
                ) else {
                    break;
                };
                d.copy_from_slice(src);
            }
        }
    } else {
        wie_cpu::stretch_nearest_strided(
            dst,
            dst_stride,
            frame,
            frame_width,
            frame_height,
            dst_width,
            dst_height,
        );
    }
}
