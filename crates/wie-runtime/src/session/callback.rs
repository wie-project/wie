//! Guest WndProc bridging types.
//!
//! The enter/complete logic lives in `pump.rs` (the primary-thread hook state
//! owns the pending-callback stack); this module keeps the two frame types the
//! pump manipulates. The frame installation itself is in `crate::guest_callback`.

use std::sync::Arc;
use wie_winapi::OuterReturn;

/// Saved frame for one in-flight guest window-procedure call.
#[derive(Debug, Clone)]
pub(super) struct PendingGuestCallback {
    /// `RSP` at the outer host API entry (return address of the caller).
    pub(super) dispatch_rsp: u64,
    /// Original callback request metadata.
    pub(super) request: wie_winapi::GuestCallbackRequest,
    /// Outer host API that requested the callback (`DispatchMessageA`, `SendMessageA`, …).
    pub(super) outer_library: Arc<str>,
    /// Outer host API export name.
    pub(super) outer_name: Arc<str>,
    /// Fake VA of the outer host API entry.
    pub(super) outer_fake_va: u64,
    /// Controls what the outer API returns after the WndProc completes.
    pub(super) outer_return: OuterReturn,
    /// Host-side font-enumeration state key, when this callback is one item of
    /// a multi-item `EnumFontFamiliesEx*` iteration (`None` = one-shot WndProc).
    pub(super) enumeration_id: Option<u64>,
}

/// Result of finishing one bridged guest WndProc call.
pub(super) struct GuestCallbackCompletion {
    pub(super) outer_library: Arc<str>,
    pub(super) outer_name: Arc<str>,
    pub(super) outer_fake_va: u64,
    pub(super) return_value: u64,
    pub(super) return_address: u64,
}
