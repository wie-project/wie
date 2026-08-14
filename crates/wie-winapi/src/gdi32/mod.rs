//! GDI32 API handlers — organised by concern (blit, fonts, pixels, state,
//! text). Re-exports the handler functions so `crate::gdi32::handle_*`
//! resolves the same way it did when gdi32 was a single flat module.

mod blit;
mod dib;
pub mod enumerate;
mod font_system;
mod pixel;
mod print;
mod regions;
mod state;
mod text;

// Re-export every public handler function from the submodules.
pub use blit::*;
pub use dib::*;
pub use enumerate::*;
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
        // Font enumeration, glyph outlines and font resources (soft-dispatch).
        "enumfontfamiliesexw" => Ok(Some(enumerate::handle_enum_font_families_ex_w(ctx)?)),
        "enumfontfamiliesexa" => Ok(Some(enumerate::handle_enum_font_families_ex_a(ctx)?)),
        "enumfontfamiliesw" => Ok(Some(enumerate::handle_enum_font_families_w(ctx)?)),
        "enumfontfamiliesa" => Ok(Some(enumerate::handle_enum_font_families_a(ctx)?)),
        "enumfontsw" => Ok(Some(enumerate::handle_enum_fonts_w(ctx)?)),
        "enumfontsa" => Ok(Some(enumerate::handle_enum_fonts_a(ctx)?)),
        "getglyphoutlinew" => Ok(Some(enumerate::handle_get_glyph_outline_w(ctx)?)),
        "getglyphoutlinea" => Ok(Some(enumerate::handle_get_glyph_outline_a(ctx)?)),
        "addfontresourcew" => Ok(Some(enumerate::handle_add_font_resource_w(ctx)?)),
        "addfontresourcea" => Ok(Some(enumerate::handle_add_font_resource_a(ctx)?)),
        "removefontresourcew" => Ok(Some(enumerate::handle_remove_font_resource_w(ctx)?)),
        "removefontresourcea" => Ok(Some(enumerate::handle_remove_font_resource_a(ctx)?)),
        "setpixel" => Ok(Some(ctx.finish(1)?)),
        // Phase-3 stub wave: pixel-format delegation + gamma/bitmap/ICM.
        "choosepixelformat" => Ok(Some(handle_choose_pixel_format(ctx)?)),
        "setpixelformat" => Ok(Some(handle_set_pixel_format(ctx)?)),
        "getpixelformat" => Ok(Some(handle_get_pixel_format(ctx)?)),
        "describepixelformat" => Ok(Some(handle_describe_pixel_format(ctx)?)),
        "swapbuffers" => Ok(Some(handle_swap_buffers(ctx)?)),
        "createbitmap" => Ok(Some(handle_create_bitmap(ctx)?)),
        "setdevicegammaramp" => Ok(Some(handle_set_device_gamma_ramp(ctx)?)),
        "getdevicegammaramp" => Ok(Some(handle_get_device_gamma_ramp(ctx)?)),
        "geticmprofilew" => Ok(Some(handle_get_icm_profile_w(ctx)?)),
        _ => Ok(None),
    }
}

// ── Phase-3 stub wave ────────────────────────────────────────────────────

/// Handles `GDI32.dll!ChoosePixelFormat` — delegates to the same pixel-format
/// state `wglChoosePixelFormat` uses (the format id is a stub constant).
pub fn handle_choose_pixel_format(
    ctx: &mut crate::HandlerContext<'_>,
) -> anyhow::Result<crate::WinApiHandlerResult> {
    crate::opengl32::wgl::handle_wgl_choose_pixel_format(ctx)
}
/// Handles `GDI32.dll!SetPixelFormat` — records the format on the HDC
/// (same state as `wglSetPixelFormat`).
pub fn handle_set_pixel_format(
    ctx: &mut crate::HandlerContext<'_>,
) -> anyhow::Result<crate::WinApiHandlerResult> {
    crate::opengl32::wgl::handle_wgl_set_pixel_format(ctx)
}
/// Handles `GDI32.dll!GetPixelFormat` — the format stored on the HDC.
pub fn handle_get_pixel_format(
    ctx: &mut crate::HandlerContext<'_>,
) -> anyhow::Result<crate::WinApiHandlerResult> {
    crate::opengl32::wgl::handle_wgl_get_pixel_format(ctx)
}
/// Handles `GDI32.dll!DescribePixelFormat` — writes the descriptor and
/// reports one pixel format (same as `wglDescribePixelFormat`).
pub fn handle_describe_pixel_format(
    ctx: &mut crate::HandlerContext<'_>,
) -> anyhow::Result<crate::WinApiHandlerResult> {
    crate::opengl32::wgl::handle_wgl_describe_pixel_format(ctx)
}
/// Handles `GDI32.dll!SwapBuffers` — publishes the rendered backbuffer
/// (same present path as `wglSwapBuffers`).
pub fn handle_swap_buffers(
    ctx: &mut crate::HandlerContext<'_>,
) -> anyhow::Result<crate::WinApiHandlerResult> {
    crate::opengl32::wgl::handle_wgl_swap_buffers(ctx)
}
/// Handles `GDI32.dll!CreateBitmap` — a fake HBITMAP handle like
/// `CreateCompatibleBitmap`; zero dimensions return NULL.
pub fn handle_create_bitmap(
    ctx: &mut crate::HandlerContext<'_>,
) -> anyhow::Result<crate::WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let width = engine.read_rcx()?;
    let height = engine.read_rdx()?;
    let _planes = engine.read_r8()?;
    let _bits_per_pixel = engine.read_r9()?;
    let _bits = engine.read_rsp()?.wrapping_add(0x28); // lpvBits on the stack
    let handle = if width == 0 || height == 0 {
        0
    } else {
        crate::gdi32::state::objects::next_gdi_bitmap_handle(state)?
    };
    ctx.finish(handle)
}
/// Handles `GDI32.dll!SetDeviceGammaRamp` — accepted no-op TRUE (SDL's
/// brightness probe stays happy; the ramp is not applied to the present).
pub fn handle_set_device_gamma_ramp(
    ctx: &mut crate::HandlerContext<'_>,
) -> anyhow::Result<crate::WinApiHandlerResult> {
    let _hdc = ctx.engine.read_rcx()?;
    let _ramp = ctx.engine.read_rdx()?;
    ctx.finish(1)
}
/// Handles `GDI32.dll!GetDeviceGammaRamp` — leaves the ramp untouched and
/// returns FALSE (no gamma state is kept).
pub fn handle_get_device_gamma_ramp(
    ctx: &mut crate::HandlerContext<'_>,
) -> anyhow::Result<crate::WinApiHandlerResult> {
    let _hdc = ctx.engine.read_rcx()?;
    let _ramp = ctx.engine.read_rdx()?;
    ctx.finish(0)
}
/// Handles `GDI32.dll!GetICMProfileW` — no color management; FALSE.
pub fn handle_get_icm_profile_w(
    ctx: &mut crate::HandlerContext<'_>,
) -> anyhow::Result<crate::WinApiHandlerResult> {
    let _hdc = ctx.engine.read_rcx()?;
    let _size = ctx.engine.read_rdx()?;
    let _name = ctx.engine.read_r8()?;
    ctx.finish(0)
}
