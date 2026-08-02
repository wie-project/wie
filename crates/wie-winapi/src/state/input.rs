//! Input state (keyboard).

/// Wrapper for the 256-byte keyboard state array. Exists so [`WindowState`]
/// can use `#[derive(Default)]` — bare `[u8; 256]` does not implement `Default`.
#[derive(Debug, Clone)]
pub struct KeyboardState(pub [u8; 256]);

impl Default for KeyboardState {
    fn default() -> Self {
        Self([0; 256])
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
