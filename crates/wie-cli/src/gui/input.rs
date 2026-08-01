//! Winit-to-Win32 input event mapping.
//!
//! Win32 message constants and helpers for posting mouse/keyboard/resize
//! messages via [`GuestHandle::post_message`].

// ---------------------------------------------------------------------------
// Win32 message constants
// ---------------------------------------------------------------------------

pub(crate) const WM_KEYDOWN: u32 = 0x0100;
pub(crate) const WM_KEYUP: u32 = 0x0101;
pub(crate) const WM_CHAR: u32 = 0x0102;
pub(crate) const WM_SYSKEYDOWN: u32 = 0x0104;
pub(crate) const WM_SYSKEYUP: u32 = 0x0105;
#[allow(dead_code)]
pub(crate) const WM_SYSCHAR: u32 = 0x0106;

/// Posted by the host menu bar (and control activation) with
/// `wParam = MAKEWPARAM(item_id, 0)`.
pub(crate) const WM_COMMAND: u32 = 0x0111;

pub(crate) const WM_MOUSEMOVE: u32 = 0x0200;
pub(crate) const WM_LBUTTONDOWN: u32 = 0x0201;
pub(crate) const WM_LBUTTONUP: u32 = 0x0202;
pub(crate) const WM_RBUTTONDOWN: u32 = 0x0204;
pub(crate) const WM_RBUTTONUP: u32 = 0x0205;
pub(crate) const WM_MBUTTONDOWN: u32 = 0x0207;
pub(crate) const WM_MBUTTONUP: u32 = 0x0208;
pub(crate) const WM_MOUSEWHEEL: u32 = 0x020A;
pub(crate) const WM_MOUSEHWHEEL: u32 = 0x020E;

pub(crate) const WM_SIZE: u32 = 0x0005;
pub(crate) const WM_MOVE: u32 = 0x0003;
pub(crate) const WM_PAINT: u32 = 0x000F;
pub(crate) const WM_SETFOCUS: u32 = 0x0007;
pub(crate) const WM_KILLFOCUS: u32 = 0x0008;
pub(crate) const WM_MOUSEHOVER: u32 = 0x02A1;
pub(crate) const WM_MOUSELEAVE: u32 = 0x02A3;

/// Mouse-key state flags (wParam of mouse messages).
pub(crate) const MK_LBUTTON: u16 = 0x0001;
pub(crate) const MK_RBUTTON: u16 = 0x0002;
pub(crate) const MK_SHIFT: u16 = 0x0004;
pub(crate) const MK_CONTROL: u16 = 0x0008;
pub(crate) const MK_MBUTTON: u16 = 0x0010;

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Pack low-word x, high-word y into a single LPARAM.
/// Equivalent to Win32 `MAKELPARAM(x, y)`.
#[must_use]
pub(crate) fn make_lparam(x: u16, y: u16) -> u64 {
    u64::from(x) | (u64::from(y) << 16)
}

/// Pack two WORDs into a WPARAM (same bit layout as MAKELPARAM).
#[must_use]
pub(crate) fn make_wparam(lo: u16, hi: u16) -> u64 {
    u64::from(lo) | (u64::from(hi) << 16)
}

// ---------------------------------------------------------------------------
// Virtual-key code mapping (winit PhysicalKey → Win32 VK_*)
// ---------------------------------------------------------------------------

/// Map a winit `PhysicalKey` to a Win32 virtual-key code.
/// Returns 0x00 for unrecognised keys.
#[must_use]
pub(crate) fn virt_key_from_physical(key: winit::keyboard::PhysicalKey) -> u16 {
    use winit::keyboard::KeyCode::*;
    let code = match key {
        winit::keyboard::PhysicalKey::Code(c) => c,
        _ => return 0,
    };
    match code {
        Backspace => 0x08,
        Tab => 0x09,
        Enter => 0x0D,
        ShiftLeft | ShiftRight => 0x10,
        ControlLeft | ControlRight => 0x11,
        AltLeft | AltRight => 0x12,
        Pause => 0x13,
        CapsLock => 0x14,
        Escape => 0x1B,
        Space => 0x20,
        PageUp => 0x21,
        PageDown => 0x22,
        End => 0x23,
        Home => 0x24,
        ArrowLeft => 0x25,
        ArrowUp => 0x26,
        ArrowRight => 0x27,
        ArrowDown => 0x28,
        Insert => 0x2D,
        Delete => 0x2E,
        Digit0 => 0x30,
        Digit1 => 0x31,
        Digit2 => 0x32,
        Digit3 => 0x33,
        Digit4 => 0x34,
        Digit5 => 0x35,
        Digit6 => 0x36,
        Digit7 => 0x37,
        Digit8 => 0x38,
        Digit9 => 0x39,
        KeyA => 0x41,
        KeyB => 0x42,
        KeyC => 0x43,
        KeyD => 0x44,
        KeyE => 0x45,
        KeyF => 0x46,
        KeyG => 0x47,
        KeyH => 0x48,
        KeyI => 0x49,
        KeyJ => 0x4A,
        KeyK => 0x4B,
        KeyL => 0x4C,
        KeyM => 0x4D,
        KeyN => 0x4E,
        KeyO => 0x4F,
        KeyP => 0x50,
        KeyQ => 0x51,
        KeyR => 0x52,
        KeyS => 0x53,
        KeyT => 0x54,
        KeyU => 0x55,
        KeyV => 0x56,
        KeyW => 0x57,
        KeyX => 0x58,
        KeyY => 0x59,
        KeyZ => 0x5A,
        SuperLeft | SuperRight => 0x5B,
        Numpad0 => 0x60,
        Numpad1 => 0x61,
        Numpad2 => 0x62,
        Numpad3 => 0x63,
        Numpad4 => 0x64,
        Numpad5 => 0x65,
        Numpad6 => 0x66,
        Numpad7 => 0x67,
        Numpad8 => 0x68,
        Numpad9 => 0x69,
        NumpadMultiply => 0x6A,
        NumpadAdd => 0x6B,
        NumpadSubtract => 0x6D,
        NumpadDecimal => 0x6E,
        NumpadDivide => 0x6F,
        F1 => 0x70,
        F2 => 0x71,
        F3 => 0x72,
        F4 => 0x73,
        F5 => 0x74,
        F6 => 0x75,
        F7 => 0x76,
        F8 => 0x77,
        F9 => 0x78,
        F10 => 0x79,
        F11 => 0x7A,
        F12 => 0x7B,
        Semicolon => 0xBA,
        Equal => 0xBB,
        Comma => 0xBC,
        Minus => 0xBD,
        Period => 0xBE,
        Slash => 0xBF,
        Backquote => 0xC0,
        BracketLeft => 0xDB,
        Backslash => 0xDC,
        BracketRight => 0xDD,
        Quote => 0xDE,
        _ => 0,
    }
}
