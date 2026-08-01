//! Cross-thread GUI presenter: winit window + softbuffer surface.

#[cfg(feature = "gui")]
pub(crate) mod app;
#[cfg(feature = "gui")]
pub(crate) mod headless;
#[cfg(feature = "gui")]
pub(crate) mod input;
#[cfg(all(feature = "gui", target_os = "macos"))]
pub(crate) mod menu_bar;
