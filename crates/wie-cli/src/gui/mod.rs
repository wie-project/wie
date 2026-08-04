//! Cross-thread GUI presenter: winit window + wgpu (Metal) surface.

pub(crate) mod app;
pub(crate) mod file_dialog;
pub(crate) mod find_dialog;
pub(crate) mod font_dialog;
pub(crate) mod headless;
pub(crate) mod input;
pub(crate) mod input_script;
#[cfg(target_os = "macos")]
pub(crate) mod menu_bar;
pub(crate) mod present_wgpu;
