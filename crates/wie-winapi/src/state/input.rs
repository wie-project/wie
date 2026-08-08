//! Input state (keyboard).

/// Wrapper for the 256-byte keyboard state array. Exists so [`WindowState`]
/// can use `#[derive(Default)]` — bare `[u8; 256]` does not implement `Default`.
///
/// The array itself is private; handlers read/write single keys through
/// [`Self::get`] / [`Self::set`]. The `Deref`/`DerefMut` impls remain for the
/// bulk read/write APIs (`GetKeyboardState` / `SetKeyboardState` buffers) and
/// the runtime's per-key `get_mut` updates.
#[derive(Debug, Clone)]
pub struct KeyboardState([u8; 256]);

impl Default for KeyboardState {
    fn default() -> Self {
        Self([0; 256])
    }
}

impl KeyboardState {
    /// Read one virtual-key slot (0 outside `0..256`).
    #[must_use]
    pub(crate) fn get(&self, index: usize) -> u8 {
        self.0.get(index).copied().unwrap_or(0)
    }

    /// Write one virtual-key slot (no-op outside `0..256`).
    ///
    /// No production handler writes a single key today (guests bulk-write via
    /// `SetKeyboardState`); the accessor exists for tests and future handlers.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn set(&mut self, index: usize, value: u8) {
        if let Some(slot) = self.0.get_mut(index) {
            *slot = value;
        }
    }
}

impl std::ops::Deref for KeyboardState {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl std::ops::DerefMut for KeyboardState {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.0
    }
}
