//! Guest WndProc bridging: host API entry → Win64-convention guest callback.

use anyhow::{Context, Result};
use std::borrow::Cow;
use std::sync::Arc;
use wie_winapi::OuterReturn;

/// Saved frame for one in-flight guest window-procedure call.
#[derive(Debug, Clone)]
pub(super) struct PendingGuestCallback {
    /// `RSP` at the outer host API entry (return address of the caller).
    dispatch_rsp: u64,
    /// Original callback request metadata.
    request: wie_winapi::GuestCallbackRequest,
    /// Outer host API that requested the callback (`DispatchMessageA`, `SendMessageA`, …).
    outer_library: Arc<str>,
    /// Outer host API export name.
    outer_name: Arc<str>,
    /// Fake VA of the outer host API entry.
    outer_fake_va: u64,
    /// Controls what the outer API returns after the WndProc completes.
    outer_return: OuterReturn,
    /// Host-side font-enumeration state key, when this callback is one item of
    /// a multi-item `EnumFontFamiliesEx*` iteration (`None` = one-shot WndProc).
    enumeration_id: Option<u64>,
}

impl super::RuntimeSession {
    /// Sets up Win64 calling convention and transfers control to a guest WndProc.
    ///
    /// Stack layout below the original `DispatchMessageA` frame:
    /// ```text
    /// [dispatch_rsp]      return address of DispatchMessageA caller
    /// [dispatch_rsp-8]    alignment padding
    /// [dispatch_rsp-0x28] 32-byte shadow space
    /// [dispatch_rsp-0x30] trampoline return address  ← new RSP / WndProc entry
    /// ```
    pub(super) fn begin_guest_callback(
        &mut self,
        request: wie_winapi::GuestCallbackRequest,
        outer_library: Arc<str>,
        outer_name: Arc<str>,
        outer_fake_va: u64,
    ) -> Result<()> {
        let trampoline = self.process.layout().callback_return_trampoline_va;
        let dispatch_rsp = self.process.with_mut(|engine, _| {
            crate::guest_callback::install_guest_callback_frame(engine, &request, trampoline)
        })?;

        self.pending_callbacks.push(PendingGuestCallback {
            dispatch_rsp,
            request,
            outer_library,
            outer_name,
            outer_fake_va,
            outer_return: request.outer_return,
            enumeration_id: None,
        });

        Ok(())
    }

    /// Begins a guest `FONTENUMPROC` callback for one item of a font
    /// enumeration.
    ///
    /// `dispatch_rsp` is the RSP the frame is installed relative to: the
    /// current RSP for the first item, or the ORIGINAL outer API RSP for a
    /// continuation (so the final completion restores the true outer frame
    /// even though each re-entry grows the stack down by 0x30).
    pub(super) fn begin_guest_enum_callback(
        &mut self,
        request: wie_winapi::GuestCallbackRequest,
        enumeration_id: u64,
        outer_library: Arc<str>,
        outer_name: Arc<str>,
        outer_fake_va: u64,
        dispatch_rsp: u64,
    ) -> Result<()> {
        let trampoline = self.process.layout().callback_return_trampoline_va;
        self.process.with_mut(|engine, _| {
            crate::guest_callback::install_guest_enum_callback_frame(
                engine,
                &request,
                trampoline,
                dispatch_rsp,
            )
        })?;

        self.pending_callbacks.push(PendingGuestCallback {
            dispatch_rsp,
            request,
            outer_library,
            outer_name,
            outer_fake_va,
            outer_return: request.outer_return,
            enumeration_id: Some(enumeration_id),
        });

        Ok(())
    }

    /// Returns a shareable `Arc<str>` for a callback-outer API name, caching
    /// the first conversion so every subsequent bridged window message clones
    /// a refcounted string (refcount bump) instead of allocating a fresh
    /// `Arc` box + string copy per message.
    ///
    /// `Cow::Owned` (the rare soft-table path) is adopted with its `String`
    /// buffer reused by `Arc::from(String)`; the result is still cached by
    /// content for repeat callers.
    pub(super) fn intern_outer_api_name(&mut self, name: Cow<'static, str>) -> Arc<str> {
        if let Some(arc) = self.outer_api_names.get(name.as_ref()) {
            return Arc::clone(arc);
        }
        let arc: Arc<str> = Arc::from(name);
        self.outer_api_names
            .insert(arc.to_string(), Arc::clone(&arc));
        arc
    }

    /// Completes the most recent guest WndProc and returns from the outer host API.
    pub(super) fn complete_guest_callback(&mut self) -> Result<GuestCallbackCompletion> {
        let pending = self
            .pending_callbacks
            .pop()
            .context("callback trampoline hit without a pending guest callback")?;

        // Read the callback's return value (RAX) without restoring the frame
        // yet — a font-enumeration continuation needs it to decide whether to
        // re-enter with the next item.
        let lresult = self
            .process
            .with_mut(|engine, _| engine.read_rax())
            .context("failed to read guest callback return value")?;

        // Full-iteration continuation: while the callback returns non-zero and
        // more items remain, re-enter with the next item instead of completing
        // the outer API. One-shot callers (EnumWindows, WndProc dispatch) have
        // `enumeration_id == None` and always fall through to completion.
        if let Some(enumeration_id) = pending.enumeration_id
            && lresult != 0
        {
            let next = self.process.with_mut(|engine, winapi_state| {
                wie_winapi::gdi32::enumerate::advance_enumeration(
                    engine,
                    winapi_state,
                    enumeration_id,
                )
            })?;
            if let Some(next_request) = next {
                // Re-enter with the next item, preserving the ORIGINAL outer
                // frame RSP so the final completion restores it correctly.
                self.begin_guest_enum_callback(
                    next_request,
                    enumeration_id,
                    Arc::clone(&pending.outer_library),
                    Arc::clone(&pending.outer_name),
                    pending.outer_fake_va,
                    pending.dispatch_rsp,
                )?;
                return Ok(GuestCallbackCompletion {
                    outer_library: pending.outer_library,
                    outer_name: pending.outer_name,
                    outer_fake_va: pending.outer_fake_va,
                    return_value: lresult,
                    // Not used for a continuation: pump re-enters the loop and
                    // the next callback runs on the following iteration.
                    return_address: 0,
                });
            }
        }

        let (return_value, return_address) = self.process.with_mut(|engine, _| {
            crate::guest_callback::finish_guest_callback(
                engine,
                pending.dispatch_rsp,
                pending.outer_return,
            )
        })?;

        tracing::debug!(
            outer = %format!("{}!{}", pending.outer_library.as_ref(), pending.outer_name.as_ref()),
            callback = pending.request.callback_address,
            hwnd = pending.request.window_handle,
            message = pending.request.message,
            return_value,
            resume = return_address,
            "completed guest window callback"
        );

        Ok(GuestCallbackCompletion {
            outer_library: pending.outer_library,
            outer_name: pending.outer_name,
            outer_fake_va: pending.outer_fake_va,
            return_value,
            return_address,
        })
    }
}

/// Result of finishing one bridged guest WndProc call.
pub(super) struct GuestCallbackCompletion {
    pub(super) outer_library: Arc<str>,
    pub(super) outer_name: Arc<str>,
    pub(super) outer_fake_va: u64,
    pub(super) return_value: u64,
    pub(super) return_address: u64,
}
