//! Crate-internal newtypes for guest address-space values (ADR-003).
//!
//! Zero-cost wrappers that prevent accidental mixing of guest VAs, stack
//! pointers, window handles and thread IDs. Crate-internal only — the public
//! getters on [`super::RuntimeSession`] convert back to raw `u64` / `u32` at
//! the API boundary.

/// A guest virtual address (always soft-translated; never a host pointer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct GuestVa(pub(crate) u64);

/// A guest stack pointer (a `GuestVa` in its own wrapper: it flows through
/// different code paths and mixing the two would be a latent bug).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct GuestStackPtr(pub(crate) u64);

/// A guest window handle (the raw `HWND` value as seen by the guest).
///
/// Only consumer today is [`super::RuntimeSession::post_window_message`],
/// an unused internal seam — `dead_code` until a caller lands.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct GuestHwnd(pub(crate) u64);

/// A guest thread ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct GuestTid(pub(crate) u32);

impl GuestTid {
    /// The primary (first) guest thread ID — a fixed magic value (0x5678)
    /// shared with `wie_winapi::PRIMARY_THREAD_ID`.
    pub(crate) const PRIMARY: GuestTid = GuestTid(wie_winapi::PRIMARY_THREAD_ID);
}
