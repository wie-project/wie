//! Typed fake-handle newtypes for USER32/GDI32 objects.
//!
//! The guest-visible ABI is frozen: each of these wraps the same `u64` handle
//! value that has always crossed the register boundary, and the disjoint base
//! ranges the allocators emit (`gdi32/state.rs`) already keep namespaces apart.
//! The newtypes exist so the *host-side* stores (window records, maps, DC
//! selections) cannot mix handle namespaces — passing an `Hwnd` where an
//! `Hbrush` belongs is now a compile error instead of a silent `u64` alias.
//!
//! Conversions happen exactly where a `u64` meets a typed store:
//! `Hwnd::from(x)` going in, `x.as_u64()` going out. Handler signatures
//! (`read_rcx` / `return_from_win64_api`) keep `u64` — the guest never sees
//! these types.
//!
//! Each newtype is an instance of the shared `handle_newtype!` template
//! (defined in `state::mod`).

use crate::state::handle_newtype;

handle_newtype! {
    /// A `HWND` — runtime-owned fake window handle.
    Hwnd
}

handle_newtype! {
    /// A `HMENU` — fake menu handle.
    Hmenu
}

handle_newtype! {
    /// An `HDC` — fake device-context handle.
    Hdc
}

handle_newtype! {
    /// An `HFONT` — fake font handle.
    Hfont
}

handle_newtype! {
    /// An `HBRUSH` — fake brush handle.
    Hbrush
}

handle_newtype! {
    /// An `HPEN` — fake pen handle.
    Hpen
}

handle_newtype! {
    /// An `HBITMAP` — fake bitmap/DIB handle.
    Hbitmap
}

handle_newtype! {
    /// A `HHOOK` — fake Windows hook handle (`SetWindowsHookEx`).
    HookHandle
}

handle_newtype! {
    /// A `HACCEL` — fake accelerator-table handle (`LoadAcceleratorsA/W`).
    Haccel
}
