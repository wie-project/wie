//! Cross-thread GUI presenter: winit window + wgpu (Metal) surface.

pub(crate) mod app;
pub(crate) mod headless;
pub(crate) mod input;
#[cfg(target_os = "macos")]
pub(crate) mod menu_bar;
pub(crate) mod present_wgpu;
