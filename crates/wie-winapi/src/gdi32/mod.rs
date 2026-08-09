//! GDI32 API handlers — organised by concern (blit, fonts, pixels, state,
//! text). Re-exports the handler functions so `crate::gdi32::handle_*`
//! resolves the same way it did when gdi32 was a single flat module.

mod blit;
mod dib;
mod font_system;
mod pixel;
mod print;
mod regions;
mod state;
mod text;

// Re-export every public handler function from the submodules.
pub use blit::*;
pub use dib::*;
pub use font_system::*;
pub use print::*;
pub use regions::*;
pub use state::*;
pub use text::*;

/// Soft dispatch for GDI32 exports beyond the dense table (DIB round-trips,
/// regions, font enumeration, SetPixel). String path only — hot APIs stay
/// dense.
pub fn dispatch_gdi32_extra(
    ctx: &mut crate::HandlerContext<'_>,
    name: &str,
) -> anyhow::Result<Option<crate::WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "getdibits" => Ok(Some(dib::handle_get_dib_bits(ctx)?)),
        "setdibits" => Ok(Some(dib::handle_set_dib_bits(ctx)?)),
        "createrectrgn" => Ok(Some(regions::handle_create_rect_rgn(ctx)?)),
        "createellipticrgn" => Ok(Some(regions::handle_create_elliptic_rgn(ctx)?)),
        "createpolygonrgn" => Ok(Some(regions::handle_create_polygon_rgn(ctx)?)),
        "combinergn" => Ok(Some(regions::handle_combine_rgn(ctx)?)),
        "setrectrgn" => Ok(Some(regions::handle_set_rect_rgn(ctx)?)),
        "getrgnbox" => Ok(Some(regions::handle_get_rgn_box(ctx)?)),
        // Font enumeration and SetPixel are decorative for the Tier-2
        // milestone: report success (TRUE) without doing the work. A real
        // EnumFontFamiliesExW would need the guest-callback re-entry machine
        // (pthread's call_guest pattern); enumeration is not worth that risk
        // until a guest actually needs the family list.
        "enumfontfamiliesexw" | "enumfontfamiliesexa" => Ok(Some(ctx.finish(1)?)),
        "setpixel" => Ok(Some(ctx.finish(1)?)),
        _ => Ok(None),
    }
}
