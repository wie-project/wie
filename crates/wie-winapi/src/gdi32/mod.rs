//! GDI32 API handlers — organised by concern (blit, fonts, pixels, state,
//! text). Re-exports the handler functions so `crate::gdi32::handle_*`
//! resolves the same way it did when gdi32 was a single flat module.

mod blit;
mod font_system;
mod pixel;
mod print;
mod state;
mod text;

// Re-export every public handler function from the submodules.
pub use blit::*;
pub use font_system::*;
pub use print::*;
pub use state::*;
pub use text::*;

/// Soft dispatch for GDI32 exports beyond the dense table (DIB round-trips,
/// regions, font enumeration, SetPixel). String path only — hot APIs stay
/// dense.
pub fn dispatch_gdi32_extra(
    ctx: &mut crate::HandlerContext<'_>,
    name: &str,
) -> anyhow::Result<Option<crate::WinApiHandlerResult>> {
    let _ = (ctx, name);
    Ok(None)
}
