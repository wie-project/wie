//! GDI32 API handlers — organised by concern (blit, fonts, pixels, state,
//! text). Re-exports the handler functions so `crate::gdi32::handle_*`
//! resolves the same way it did when gdi32 was a single flat module.

mod blit;
mod font_system;
mod pixel;
mod state;
mod text;

// Re-export every public handler function from the submodules.
pub use blit::*;
pub use font_system::*;
pub use state::*;
pub use text::*;
