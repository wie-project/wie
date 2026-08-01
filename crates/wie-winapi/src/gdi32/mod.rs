// GDI32 API handlers — organised by concern.

mod blit;
mod font_system;
mod pixel;
mod state;
mod text;

// Re-export all public handler functions so `crate::gdi32::handle_*` resolves
// the same way it did when gdi32 was a single flat module.
pub use blit::*;
pub use font_system::*;
pub use state::*;
pub use text::*;
