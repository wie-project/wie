//! Per-API trait flags for the runtime loop (split from `mod.rs` — the
//! bitflag type and its builder/accessor helpers; the per-API table stays in
//! the parent next to the dispatch match it serves).

/// Fast classification for the runtime loop (no string compares per call).
///
/// Packed bitflags instead of four separate bools (avoids excessive-bools lint
/// and keeps the hot-path struct one byte).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WinApiTraits {
    bits: u8,
}

impl WinApiTraits {
    const NOISY: u8 = 1 << 0;
    const FAST_VOID_SYNC: u8 = 1 << 1;
    const EXIT_PROCESS: u8 = 1 << 2;
    const GUEST_STUB: u8 = 1 << 3;
    const FAST_SYNC: u8 = 1 << 4;

    /// No flags set.
    pub const EMPTY: Self = Self { bits: 0 };

    #[must_use]
    pub const fn with_noisy(self) -> Self {
        Self {
            bits: self.bits | Self::NOISY,
        }
    }
    #[must_use]
    pub const fn with_fast_void_sync(self) -> Self {
        Self {
            bits: self.bits | Self::FAST_VOID_SYNC,
        }
    }
    #[must_use]
    pub const fn with_exit_process(self) -> Self {
        Self {
            bits: self.bits | Self::EXIT_PROCESS,
        }
    }
    #[must_use]
    pub const fn with_guest_stub(self) -> Self {
        Self {
            bits: self.bits | Self::GUEST_STUB,
        }
    }
    #[must_use]
    pub const fn with_fast_sync(self) -> Self {
        Self {
            bits: self.bits | Self::FAST_SYNC,
        }
    }

    #[must_use]
    pub const fn noisy(self) -> bool {
        self.bits & Self::NOISY != 0
    }
    #[must_use]
    pub const fn fast_void_sync(self) -> bool {
        self.bits & Self::FAST_VOID_SYNC != 0
    }
    #[must_use]
    pub const fn exit_process(self) -> bool {
        self.bits & Self::EXIT_PROCESS != 0
    }
    #[must_use]
    pub const fn guest_stub(self) -> bool {
        self.bits & Self::GUEST_STUB != 0
    }
    #[must_use]
    pub const fn fast_sync(self) -> bool {
        self.bits & Self::FAST_SYNC != 0
    }

    pub fn set_noisy(&mut self, on: bool) {
        if on {
            self.bits |= Self::NOISY;
        } else {
            self.bits &= !Self::NOISY;
        }
    }

    pub fn set_guest_stub(&mut self, on: bool) {
        if on {
            self.bits |= Self::GUEST_STUB;
        } else {
            self.bits &= !Self::GUEST_STUB;
        }
    }
}
