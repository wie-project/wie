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

/// A `HWND` — runtime-owned fake window handle.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Hwnd(u64);

impl Hwnd {
    /// The `NULL` window handle (`0`).
    pub const NULL: Self = Self(0);

    /// Escape point: the raw handle value (return values, `MSG.hwnd`, …).
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl From<u64> for Hwnd {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<Hwnd> for u64 {
    fn from(value: Hwnd) -> Self {
        value.0
    }
}

/// A `HMENU` — fake menu handle.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Hmenu(u64);

impl Hmenu {
    /// The `NULL` menu handle (`0`).
    pub const NULL: Self = Self(0);

    /// Escape point: the raw handle value (return values, …).
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl From<u64> for Hmenu {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<Hmenu> for u64 {
    fn from(value: Hmenu) -> Self {
        value.0
    }
}

/// An `HDC` — fake device-context handle.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Hdc(u64);

impl Hdc {
    /// The `NULL` DC handle (`0`).
    pub const NULL: Self = Self(0);

    /// Escape point: the raw handle value (return values, …).
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl From<u64> for Hdc {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<Hdc> for u64 {
    fn from(value: Hdc) -> Self {
        value.0
    }
}

/// An `HFONT` — fake font handle.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Hfont(u64);

impl Hfont {
    /// The `NULL` font handle (`0`).
    pub const NULL: Self = Self(0);

    /// Escape point: the raw handle value (return values, …).
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl From<u64> for Hfont {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<Hfont> for u64 {
    fn from(value: Hfont) -> Self {
        value.0
    }
}

/// An `HBRUSH` — fake brush handle.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Hbrush(u64);

impl Hbrush {
    /// The `NULL` brush handle (`0`).
    pub const NULL: Self = Self(0);

    /// Escape point: the raw handle value (return values, …).
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl From<u64> for Hbrush {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<Hbrush> for u64 {
    fn from(value: Hbrush) -> Self {
        value.0
    }
}

/// An `HPEN` — fake pen handle.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Hpen(u64);

impl Hpen {
    /// The `NULL` pen handle (`0`).
    pub const NULL: Self = Self(0);

    /// Escape point: the raw handle value (return values, …).
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl From<u64> for Hpen {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<Hpen> for u64 {
    fn from(value: Hpen) -> Self {
        value.0
    }
}

/// An `HBITMAP` — fake bitmap/DIB handle.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Hbitmap(u64);

impl Hbitmap {
    /// The `NULL` bitmap handle (`0`).
    pub const NULL: Self = Self(0);

    /// Escape point: the raw handle value (return values, …).
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl From<u64> for Hbitmap {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<Hbitmap> for u64 {
    fn from(value: Hbitmap) -> Self {
        value.0
    }
}
