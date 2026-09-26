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

/// WIE's five mouse buttons, one row each: `(VK code, Win32 MK_* bit,
/// DIMOUSESTATE2::rgbButtons slot index)`.
///
/// This is the single home of the VK↔MK mapping, because there are two
/// spellings of "is the left button down" in Win32 and they must not drift
/// apart: a `DIMOUSESTATE2` byte array (DirectInput) and a virtual-key slot
/// (`GetKeyState` / `GetAsyncKeyState` / `GetKeyboardState`). The host pushes
/// exactly one thing — the `MK_*` level mask on the presenter mirror
/// (`GuestHandle::set_mouse_buttons`) — so both readers derive from that mask:
///
/// - The five `VK_*` slots are *not* stored in [`KeyboardState`]. The host
///   never wrote them, so a stored copy could only ever be a second,
///   permanently-wrong source of truth; they are projected from the mirror at
///   read time instead (see `crate::user32::input`).
/// - `MK_MBUTTON` is 0x0010, *not* bit 2, which is why the mask cannot be
///   scattered straight into the button byte array.
///
/// The rows are in `rgbButtons` index order, and the `VK_*` codes happen to run
/// 0x01..=0x06 in that same order; both facts are asserted by the handler
/// tests rather than assumed here.
pub(crate) const MOUSE_BUTTON_SLOTS: [(u8, u16, usize); 5] = [
    (0x01, 0x0001, 0), // VK_LBUTTON / MK_LBUTTON
    (0x02, 0x0002, 1), // VK_RBUTTON / MK_RBUTTON
    (0x04, 0x0010, 2), // VK_MBUTTON / MK_MBUTTON (not bit 2)
    (0x05, 0x0020, 3), // VK_XBUTTON1 / MK_XBUTTON1
    (0x06, 0x0040, 4), // VK_XBUTTON2 / MK_XBUTTON2
];

/// The `MK_*` bit a mouse virtual-key slot reads, or `None` for a real key.
///
/// `None` is the common case and is what makes this a lookup rather than a
/// default: the keyboard array stays the single read path for real keys, and
/// only the five mouse slots divert to the mirror.
#[must_use]
pub(crate) fn mouse_button_bit(vk: u8) -> Option<u16> {
    MOUSE_BUTTON_SLOTS
        .iter()
        .find(|(code, ..)| *code == vk)
        .map(|(_, bit, _)| *bit)
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
