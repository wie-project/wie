//! Winit-to-Win32 input event mapping.
//!
//! Win32 message constants and helpers for posting mouse/keyboard/resize
//! messages via [`GuestHandle::post_message`].

// ---------------------------------------------------------------------------
// Win32 message constants
//
// Single source of truth: the values live in `wie-winapi::user32::wm`; these
// `pub(crate) const` aliases keep the host-side type `u32` (the
// `post_message` boundary) while referencing the same constants the emulator
// core dispatches on.
// ---------------------------------------------------------------------------

use wie_winapi::user32::Dimension;
use wie_winapi::user32::wm::WinMsg;

pub(crate) const WM_KEYDOWN: u32 = WinMsg::WM_KEYDOWN.as_u32();
pub(crate) const WM_KEYUP: u32 = WinMsg::WM_KEYUP.as_u32();
pub(crate) const WM_CHAR: u32 = WinMsg::WM_CHAR.as_u32();
pub(crate) const WM_SYSKEYDOWN: u32 = WinMsg::WM_SYSKEYDOWN.as_u32();
pub(crate) const WM_SYSKEYUP: u32 = WinMsg::WM_SYSKEYUP.as_u32();
#[allow(dead_code)]
pub(crate) const WM_SYSCHAR: u32 = WinMsg::WM_SYSCHAR.as_u32();

/// Posted by the host menu bar (and control activation) with
/// `wParam = MAKEWPARAM(item_id, 0)`.
pub(crate) const WM_COMMAND: u32 = WinMsg::WM_COMMAND.as_u32();

pub(crate) const WM_MOUSEMOVE: u32 = WinMsg::WM_MOUSEMOVE.as_u32();
pub(crate) const WM_LBUTTONDOWN: u32 = WinMsg::WM_LBUTTONDOWN.as_u32();
pub(crate) const WM_LBUTTONUP: u32 = WinMsg::WM_LBUTTONUP.as_u32();
pub(crate) const WM_LBUTTONDBLCLK: u32 = WinMsg::WM_LBUTTONDBLCLK.as_u32();
pub(crate) const WM_RBUTTONDOWN: u32 = WinMsg::WM_RBUTTONDOWN.as_u32();
pub(crate) const WM_RBUTTONUP: u32 = WinMsg::WM_RBUTTONUP.as_u32();
pub(crate) const WM_MBUTTONDOWN: u32 = WinMsg::WM_MBUTTONDOWN.as_u32();
pub(crate) const WM_MBUTTONUP: u32 = WinMsg::WM_MBUTTONUP.as_u32();
pub(crate) const WM_MOUSEWHEEL: u32 = WinMsg::WM_MOUSEWHEEL.as_u32();
pub(crate) const WM_MOUSEHWHEEL: u32 = WinMsg::WM_MOUSEHWHEEL.as_u32();

pub(crate) const WM_SIZE: u32 = WinMsg::WM_SIZE.as_u32();
pub(crate) const WM_MOVE: u32 = WinMsg::WM_MOVE.as_u32();
pub(crate) const WM_PAINT: u32 = WinMsg::WM_PAINT.as_u32();
pub(crate) const WM_CLOSE: u32 = WinMsg::WM_CLOSE.as_u32();
pub(crate) const WM_SETFOCUS: u32 = WinMsg::WM_SETFOCUS.as_u32();
pub(crate) const WM_KILLFOCUS: u32 = WinMsg::WM_KILLFOCUS.as_u32();
pub(crate) const WM_MOUSEHOVER: u32 = WinMsg::WM_MOUSEHOVER.as_u32();
pub(crate) const WM_MOUSELEAVE: u32 = WinMsg::WM_MOUSELEAVE.as_u32();

/// `WM_DROPFILES` — wParam is the fake HDROP; lParam is the drop point.
pub(crate) const WM_DROPFILES: u32 = WinMsg::WM_DROPFILES.as_u32();

/// Mouse-key state flags (wParam of mouse messages).
pub(crate) const MK_LBUTTON: u16 = 0x0001;
pub(crate) const MK_RBUTTON: u16 = 0x0002;
pub(crate) const MK_SHIFT: u16 = 0x0004;
pub(crate) const MK_CONTROL: u16 = 0x0008;
pub(crate) const MK_MBUTTON: u16 = 0x0010;

/// The double-click time window (ms) the host uses to synthesize
/// `WM_LBUTTONDBLCLK` from two rapid presses — the Windows
/// `GetDoubleClickTime` default.
pub(crate) const DOUBLE_CLICK_TIME_MS: u64 = 500;

/// The double-click slop: the second press must land within this many LOGICAL
/// 96-DPI pixels of the first press (the Windows `SM_CXDOUBLECLK` /
/// `SM_CYDOUBLECLK` default is 4 px). Winit reports cursor positions in
/// PHYSICAL pixels, so the comparison scales this by the window's device
/// scale factor via [`double_click_slop`].
pub(crate) const DOUBLE_CLICK_SLOP_PX: f64 = 4.0;

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
// Device-scale-factor conversion
//
// The guest window is a LOGICAL 96-DPI surface (the DIB the guest paints);
// winit reports PHYSICAL pixels. One rounding rule — round half away from
// zero, `f64::round` — is applied at every physical↔logical boundary so a
// round-trip never drifts between call sites. At scale factor 1.0 every
// conversion is the identity for integer input, i.e. byte-identical to the
// pre-scale-factor behavior.
// ---------------------------------------------------------------------------

/// Convert a winit PHYSICAL pixel value to a guest LOGICAL (96-DPI) pixel
/// value: `physical ÷ scale_factor`, rounded half away from zero.
#[must_use]
pub(crate) fn physical_to_logical(physical: f64, scale_factor: f64) -> f64 {
    (physical / scale_factor).round()
}

/// Convert a guest LOGICAL (96-DPI) pixel value to winit PHYSICAL pixels:
/// `logical × scale_factor`, rounded half away from zero.
#[must_use]
pub(crate) fn logical_to_physical(logical: f64, scale_factor: f64) -> f64 {
    (logical * scale_factor).round()
}

/// Convert a winit PHYSICAL window extent to the guest LOGICAL (96-DPI)
/// [`Dimension`] — the size the window record and WM_SIZE carry. One
/// conversion point for the resize path; callers never re-derive the pair.
#[must_use]
pub(crate) fn physical_size_to_dimension(width: u32, height: u32, scale_factor: f64) -> Dimension {
    Dimension::new(
        logical_pixel(width, scale_factor),
        logical_pixel(height, scale_factor),
    )
}

/// One logical pixel: `physical ÷ scale_factor`, rounded, in the i32 domain
/// the window record stores (`physical_to_logical` is already integral after
/// `.round()`, so the `as u32` cast is exact — the same style the rest of
/// this module's scale conversions use).
#[must_use]
fn logical_pixel(physical: u32, scale_factor: f64) -> i32 {
    i32::try_from(physical_to_logical(f64::from(physical), scale_factor) as u32).unwrap_or(0)
}

/// Pack a guest [`Dimension`] into a `WM_SIZE` LPARAM (`MAKELPARAM(w, h)`).
#[must_use]
pub(crate) fn size_to_wm_size_lparam(size: Dimension) -> u64 {
    make_lparam(
        u16::try_from(size.width.max(0)).unwrap_or(0),
        u16::try_from(size.height.max(0)).unwrap_or(0),
    )
}

/// Convert a PHYSICAL pixel DELTA (a scroll delta, a drag distance) to
/// LOGICAL 96-DPI units: `delta ÷ scale_factor`, WITHOUT rounding.
///
/// Deltas accumulate across events (the wheel-notch accumulator, drag
/// distance), so rounding each event would alias away sub-pixel motion —
/// a 1.5 px event must stay 1.5, not bounce between 1 and 2. The rounded
/// [`physical_to_logical`] is for discrete coordinates, which land on
/// integer guest pixels.
#[must_use]
pub(crate) fn physical_delta_to_logical(delta: f64, scale_factor: f64) -> f64 {
    delta / scale_factor
}

/// Convert a LOGICAL pixel DELTA to PHYSICAL units, WITHOUT rounding (the
/// unrounded counterpart of [`logical_to_physical`]; see
/// [`physical_delta_to_logical`]).
#[must_use]
pub(crate) fn logical_delta_to_physical(delta: f64, scale_factor: f64) -> f64 {
    delta * scale_factor
}

/// The double-click slop in PHYSICAL pixels at `scale_factor`: winit cursor
/// positions are physical, while [`DOUBLE_CLICK_SLOP_PX`] is a logical
/// 96-DPI distance.
#[must_use]
pub(crate) fn double_click_slop(scale_factor: f64) -> f64 {
    logical_delta_to_physical(DOUBLE_CLICK_SLOP_PX, scale_factor)
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

#[cfg(test)]
mod tests {
    use super::{
        DOUBLE_CLICK_SLOP_PX, double_click_slop, logical_delta_to_physical, logical_to_physical,
        physical_delta_to_logical, physical_to_logical,
    };

    /// At scale factor 1.0 the conversions are the identity — the
    /// pre-scale-factor behavior — for the integer values real input
    /// produces.
    #[test]
    fn unit_scale_factor_is_identity() {
        for p in [0.0, 1.0, 4.0, 100.0, 480.0, 640.0, 1280.0] {
            assert_eq!(physical_to_logical(p, 1.0), p);
            assert_eq!(logical_to_physical(p, 1.0), p);
        }
        assert_eq!(double_click_slop(1.0), DOUBLE_CLICK_SLOP_PX);
    }

    /// logical == physical ÷ sf for exact divisions.
    #[test]
    fn logical_is_physical_divided_by_scale() {
        assert_eq!(physical_to_logical(1280.0, 2.0), 640.0);
        assert_eq!(physical_to_logical(960.0, 2.0), 480.0);
        assert_eq!(physical_to_logical(300.0, 1.5), 200.0);
        assert_eq!(logical_to_physical(640.0, 2.0), 1280.0);
        assert_eq!(logical_to_physical(480.0, 2.0), 960.0);
    }

    /// Ties round half away from zero (320.5 → 321, not 320).
    #[test]
    fn rounding_is_half_away_from_zero() {
        assert_eq!(physical_to_logical(641.0, 2.0), 321.0);
        assert_eq!(physical_to_logical(643.0, 2.0), 322.0);
        assert_eq!(physical_to_logical(5.0, 2.0), 3.0);
    }

    /// Round-tripping logical → physical → logical at a 2× factor restores
    /// the original value.
    #[test]
    fn round_trip_at_retina_scale() {
        for l in [0.0, 1.0, 100.0, 479.0, 480.0, 640.0] {
            let physical = logical_to_physical(l, 2.0);
            assert_eq!(physical_to_logical(physical, 2.0), l);
        }
    }

    /// The slop scales linearly with the device scale factor.
    #[test]
    fn double_click_slop_scales_with_factor() {
        assert_eq!(double_click_slop(2.0), 8.0);
        assert_eq!(double_click_slop(1.5), 6.0);
    }

    /// Deltas convert WITHOUT rounding — a 3-px physical delta at 2× is
    /// 1.5 logical, not 2 (the wheel accumulator depends on the fraction
    /// surviving).
    #[test]
    fn delta_conversions_preserve_fractions() {
        assert_eq!(physical_delta_to_logical(3.0, 2.0), 1.5);
        assert_eq!(physical_delta_to_logical(1.0, 1.5), 1.0 / 1.5);
        assert_eq!(logical_delta_to_physical(1.5, 2.0), 3.0);
        assert_eq!(logical_delta_to_physical(1.0, 1.5), 1.5);
    }

    /// Delta conversions round-trip exactly at any factor.
    #[test]
    fn delta_conversions_round_trip() {
        for sf in [1.0, 1.5, 2.0, 3.0] {
            for d in [0.5, 1.0, 40.0, 120.0, 250.0] {
                assert_eq!(
                    logical_delta_to_physical(physical_delta_to_logical(d, sf), sf),
                    d
                );
            }
        }
    }
}
