//! Host-side clipboard bridge (Task 2.6).
//!
//! The clipboard state itself — a host `String` in
//! [`crate::state::ClipboardState`] behind a shared `DllStateMap` slot —
//! lives in `state/mod.rs`; this module owns the small USER32 API surface
//! that reads it. macOS NSPasteboard integration is explicitly YAGNI for the
//! notepad milestone: the internal state satisfies the guest contract
//! (WM_COPY → WM_PASTE / `IsClipboardFormatAvailable` within the process) and
//! cross-app paste can follow.

use anyhow::{Context, Result};

use crate::HandlerContext;
use crate::WinApiHandlerResult;

/// `CF_TEXT` — plain ANSI text, the only clipboard format this milestone
/// emulates (winuser.h).
pub(crate) const CF_TEXT: u32 = 1;

/// Handles `USER32.dll!IsClipboardFormatAvailable`.
///
/// Win64 ABI: `rcx` = the clipboard format. Returns TRUE when the clipboard
/// state holds text and `fmt` is `CF_TEXT` (the only format that exists
/// here), FALSE otherwise. notepad's Edit menu calls it with `CF_TEXT` to
/// enable/disable the Paste command.
pub fn handle_is_clipboard_format_available(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let format = engine
        .read_rcx()
        .context("failed to read RCX for IsClipboardFormatAvailable")?;
    let available = format == u64::from(CF_TEXT) && ctx.state.clipboard().has_text();
    ctx.finish(u64::from(available))
}
