// Pixel format conversion helpers and blit clipping.

/// Clip a blit rect to source/destination dimensions.
///
/// Returns `None` if the rect is completely out of bounds.  Otherwise returns
/// adjusted `(dest_x, dest_y, src_x, src_y, width, height)`.
// Wide signature: pure geometry helper — 4 dims + 2 rects + size is the natural input.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub(crate) fn clip_blit_rect(
    dest_w: i32,
    dest_h: i32,
    src_w: i32,
    src_h: i32,
    mut dest_x: i32,
    mut dest_y: i32,
    mut src_x: i32,
    mut src_y: i32,
    mut width: i32,
    mut height: i32,
) -> Option<(i32, i32, i32, i32, i32, i32)> {
    // Clip left.
    if dest_x < 0 {
        // dest_x is negative here: subtract adds its magnitude, add reduces.
        src_x = src_x.saturating_sub(dest_x);
        width = width.saturating_add(dest_x);
        dest_x = 0;
    }
    if src_x < 0 {
        dest_x = dest_x.saturating_sub(src_x);
        width = width.saturating_add(src_x);
        src_x = 0;
    }
    // Clip top.
    if dest_y < 0 {
        src_y = src_y.saturating_sub(dest_y);
        height = height.saturating_add(dest_y);
        dest_y = 0;
    }
    if src_y < 0 {
        dest_y = dest_y.saturating_sub(src_y);
        height = height.saturating_add(src_y);
        src_y = 0;
    }
    // Clip right.
    let dest_right = dest_x.saturating_add(width).min(dest_w);
    width = dest_right.saturating_sub(dest_x);
    let src_right = src_x.saturating_add(width).min(src_w);
    width = src_right.saturating_sub(src_x);
    // Clip bottom.
    let dest_bottom = dest_y.saturating_add(height).min(dest_h);
    height = dest_bottom.saturating_sub(dest_y);
    let src_bottom = src_y.saturating_add(height).min(src_h);
    height = src_bottom.saturating_sub(src_y);

    if width <= 0 || height <= 0 {
        return None;
    }
    Some((dest_x, dest_y, src_x, src_y, width, height))
}
