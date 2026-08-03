//! Win32 window-message newtype (`WinMsg`) and typed payload decoders.
//!
//! The message VALUE is guest-visible — guests post arbitrary `WM_USER+`
//! values, and `MSG`/`QueuedWindowMessage` store raw `u32`/`u64`. Storage
//! therefore stays raw; the newtype adds exhaustiveness at the dispatch sites
//! that matter (controls, dialogs, DefWindowProc) and the decoders name the
//! bitfield semantics of `wParam`/`lParam` per message instead of hand
//! re-decoding them at every handler.
//!
//! This module is the **single source of truth** for the `WM_*` message
//! values: `user32/mod.rs` and `wie-cli/src/gui/input.rs` derive their `u32`
//! constants from `WinMsg::WM_*` (so host GUI code references the same values
//! as the emulator core).

/// A Win32 window-message value (`u32`).
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WinMsg(u32);

impl WinMsg {
    /// The raw `u32` message value.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self.0
    }

    // ── Handled WM_* values (winuser.h) — the only place the literals live ──
    pub const WM_CREATE: Self = Self(0x0001);
    pub const WM_DESTROY: Self = Self(0x0002);
    pub const WM_MOVE: Self = Self(0x0003);
    pub const WM_SIZE: Self = Self(0x0005);
    pub const WM_ACTIVATE: Self = Self(0x0006);
    pub const WM_SETFOCUS: Self = Self(0x0007);
    pub const WM_KILLFOCUS: Self = Self(0x0008);
    pub const WM_SETTEXT: Self = Self(0x000C);
    pub const WM_GETTEXT: Self = Self(0x000D);
    pub const WM_GETTEXTLENGTH: Self = Self(0x000E);
    pub const WM_PAINT: Self = Self(0x000F);
    pub const WM_CLOSE: Self = Self(0x0010);
    pub const WM_QUIT: Self = Self(0x0012);
    pub const WM_ERASEBKGND: Self = Self(0x0014);
    pub const WM_SHOWWINDOW: Self = Self(0x0018);
    pub const WM_SETCURSOR: Self = Self(0x0020);
    pub const WM_GETMINMAXINFO: Self = Self(0x0024);
    pub const WM_CONTEXTMENU: Self = Self(0x007B);
    pub const WM_NCCREATE: Self = Self(0x0081);
    pub const WM_NCDESTROY: Self = Self(0x0082);
    pub const WM_NCCALCSIZE: Self = Self(0x0083);
    pub const WM_GETDLGCODE: Self = Self(0x0087);
    pub const WM_KEYDOWN: Self = Self(0x0100);
    pub const WM_KEYUP: Self = Self(0x0101);
    pub const WM_CHAR: Self = Self(0x0102);
    pub const WM_DEADCHAR: Self = Self(0x0103);
    pub const WM_SYSKEYDOWN: Self = Self(0x0104);
    pub const WM_SYSKEYUP: Self = Self(0x0105);
    pub const WM_SYSCHAR: Self = Self(0x0106);
    pub const WM_SYSDEADCHAR: Self = Self(0x0107);
    pub const WM_INITDIALOG: Self = Self(0x0110);
    pub const WM_COMMAND: Self = Self(0x0111);
    pub const WM_SYSCOMMAND: Self = Self(0x0112);
    pub const WM_TIMER: Self = Self(0x0113);
    pub const WM_MOUSEMOVE: Self = Self(0x0200);
    pub const WM_LBUTTONDOWN: Self = Self(0x0201);
    pub const WM_LBUTTONUP: Self = Self(0x0202);
    pub const WM_RBUTTONDOWN: Self = Self(0x0204);
    pub const WM_RBUTTONUP: Self = Self(0x0205);
    pub const WM_MBUTTONDOWN: Self = Self(0x0207);
    pub const WM_MBUTTONUP: Self = Self(0x0208);
    pub const WM_MOUSEWHEEL: Self = Self(0x020A);
    pub const WM_MOUSEHWHEEL: Self = Self(0x020E);
    pub const WM_MDICREATE: Self = Self(0x0220);
    pub const WM_MOUSEHOVER: Self = Self(0x02A1);
    pub const WM_MOUSELEAVE: Self = Self(0x02A3);

    // ── Control messages (winuser.h): EDIT / BUTTON / LISTBOX ──────────
    pub const EM_GETSEL: Self = Self(0x00B0);
    pub const EM_SETSEL: Self = Self(0x00B1);
    // Multiline EDIT messages (Task 2.1). The line-metric messages operate on
    // the host `String` with `\n` as the internal separator.
    pub const EM_SCROLLCARET: Self = Self(0x00B7);
    pub const EM_GETMODIFY: Self = Self(0x00B8);
    pub const EM_SETMODIFY: Self = Self(0x00B9);
    pub const EM_GETLINECOUNT: Self = Self(0x00BA);
    pub const EM_LINEINDEX: Self = Self(0x00BB);
    pub const EM_SETHANDLE: Self = Self(0x00BC);
    pub const EM_GETHANDLE: Self = Self(0x00BD);
    pub const EM_LINELENGTH: Self = Self(0x00C1);
    pub const EM_REPLACESEL: Self = Self(0x00C2);
    pub const EM_GETLINE: Self = Self(0x00C4);
    pub const EM_LIMITTEXT: Self = Self(0x00C5);
    pub const EM_LINEFROMCHAR: Self = Self(0x00C9);
    pub const EM_SETTABSTOPS: Self = Self(0x00CB);
    pub const EM_GETFIRSTVISIBLELINE: Self = Self(0x00CE);
    pub const EM_GETLIMITTEXT: Self = Self(0x00D5);
    pub const EM_POSFROMCHAR: Self = Self(0x00D6);
    pub const EM_SELECTIONTYPE: Self = Self(0x00E1);
    pub const BM_GETSTATE: Self = Self(0x00F2);
    pub const BM_SETSTATE: Self = Self(0x00F3);
    pub const BM_CLICK: Self = Self(0x00F5);
    pub const LB_ADDSTRING: Self = Self(0x0180);
    pub const LB_SETCURSEL: Self = Self(0x0186);
    pub const LB_GETCURSEL: Self = Self(0x0187);
    pub const LB_GETTEXT: Self = Self(0x0189);
    pub const LB_GETCOUNT: Self = Self(0x018B);

    // ── Control messages (winuser.h): COMBOBOX ─────────────────────────
    // CB_* has its own message numbers; the semantics are identical to the
    // shared LB_* set (a ComboBox is a list + edit), so the control dispatch
    // aliases them.
    pub const CB_ADDSTRING: Self = Self(0x0143);
    pub const CB_GETCOUNT: Self = Self(0x0146);
    pub const CB_GETCURSEL: Self = Self(0x0147);
    pub const CB_GETLBTEXT: Self = Self(0x0148);
    pub const CB_SETCURSEL: Self = Self(0x014E);
}

impl From<u32> for WinMsg {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

impl From<WinMsg> for u32 {
    fn from(value: WinMsg) -> Self {
        value.0
    }
}

/// Low 16 bits of a `u64` (a `WORD`), for the `wParam`/`lParam` decoders.
#[must_use]
fn low_word(value: u64) -> u16 {
    u16::try_from(value & 0xFFFF).unwrap_or(0)
}

/// High 16 bits of a `u64` (a `WORD`), for the `wParam`/`lParam` decoders.
#[must_use]
fn high_word(value: u64) -> u16 {
    u16::try_from((value >> 16) & 0xFFFF).unwrap_or(0)
}

/// `WM_COMMAND` payload: `wParam = MAKEWPARAM(id, notify)`, `lParam` is the
/// child control's HWND (0 for menus / accelerators).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandPayload {
    /// The command id (low word of `wParam`).
    pub id: u16,
    /// The notification code (high word of `wParam`).
    pub notify: u16,
    /// The control that sent the command (`lParam`; plain `u64` until the
    /// `Hwnd` newtype lane).
    pub control: u64,
}

impl CommandPayload {
    /// Decode a `WM_COMMAND` `wParam`/`lParam` pair.
    #[must_use]
    pub fn decode(wparam: u64, lparam: u64) -> Self {
        Self {
            id: low_word(wparam),
            notify: high_word(wparam),
            control: lparam,
        }
    }
}

/// Pack a `WM_COMMAND` `wParam` (`MAKEWPARAM(id, notify)`).
#[must_use]
pub fn make_command_wparam(id: u64, notify: u64) -> u64 {
    (id & 0xFFFF) | (notify << 16)
}

/// `WM_KEYDOWN` / `WM_KEYUP` payload.
///
/// The virtual-key code is the low word of `wParam`; the repeat count and the
/// extended-key / Alt bits come from `lParam`. All Win32 VK codes fit in a
/// byte, so the low-word extraction matches the low-byte checks the dispatch
/// sites historically used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyDownPayload {
    /// The virtual-key code (low word of `wParam`).
    pub vk: u16,
    /// The repeat count (low word of `lParam`).
    pub repeat: u16,
    /// Whether the extended-key bit (bit 24 of `lParam`) is set.
    pub extended: bool,
    /// Whether the Alt key is down (bit 29 of `lParam`).
    pub alt_down: bool,
}

impl KeyDownPayload {
    /// Decode a `WM_KEYDOWN`/`WM_KEYUP` `wParam`/`lParam` pair.
    #[must_use]
    pub fn decode(wparam: u64, lparam: u64) -> Self {
        Self {
            vk: low_word(wparam),
            repeat: low_word(lparam),
            extended: lparam & (1 << 24) != 0,
            alt_down: lparam & (1 << 29) != 0,
        }
    }
}

/// `WM_SIZE` payload: `lParam = MAKELPARAM(width, height)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizePayload {
    /// The client width (low word of `lParam`).
    pub width: u16,
    /// The client height (high word of `lParam`).
    pub height: u16,
}

impl SizePayload {
    /// Decode a `WM_SIZE` `lParam`.
    #[must_use]
    pub fn decode(lparam: u64) -> Self {
        Self {
            width: low_word(lparam),
            height: high_word(lparam),
        }
    }
}
