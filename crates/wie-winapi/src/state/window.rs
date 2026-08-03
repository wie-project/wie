//! Window, UI, hook, timer, resource, and control-signal state types.

use crate::vfs;
use ahash::HashMapExt;

use super::input::KeyboardState;
use super::process::FileDialogPolicy;

/// Window, UI, and input state.
///
/// Fields are `pub(crate)` except the ones the runtime reads directly through
/// `WinApiState::window_state()` (windows, capture/focus handles, menus,
/// dialog/file-dialog plumbing, keyboard state).
#[derive(Debug, Clone)]
pub struct WindowState {
    pub(crate) window_long_ptr_values: Vec<(u64, i64, u64)>,
    pub(crate) image_list_counts: Vec<(u64, u64)>,
    pub(crate) image_list_background_colors: Vec<(u64, u32)>,
    pub(crate) window_visible: bool,
    pub(crate) window_enabled: bool,
    pub(crate) active_window_handle: crate::handles::Hwnd,
    pub(crate) foreground_window_handle: crate::handles::Hwnd,
    pub focus_window_handle: crate::handles::Hwnd,
    pub capture_window_handle: crate::handles::Hwnd,
    pub(crate) cursor_handle: u64,
    pub(crate) window_title: String,
    pub(crate) window_x: i32,
    pub(crate) window_y: i32,
    pub(crate) window_width: i32,
    pub(crate) window_height: i32,
    pub(crate) tick_count: u64,
    pub keyboard_state: KeyboardState,
    pub(crate) next_timer_id: u64,
    pub(crate) timers: Vec<TimerRecord>,
    pub(crate) next_global_atom: u16,
    pub(crate) global_atoms: Vec<GlobalAtomRecord>,
    /// Next id to hand out for `RegisterWindowMessageA/W` (starts at 0xC000,
    /// the first id of the Windows-reserved range).
    pub(crate) next_registered_message: u32,
    /// Per-session registered-message cache: lowercased name → message id.
    ///
    /// Ids are stable for the lifetime of the session and shared across the A
    /// and W variants of the same name (mirrors real Windows).
    pub(crate) registered_messages: ahash::HashMap<String, u32>,
    pub(crate) next_windows_hook_handle: crate::handles::HookHandle,
    pub(crate) windows_hooks: Vec<WindowsHookRecord>,
    /// All fake USER32 menus; each owns its items as a tree via `Popup`
    /// submenu links (`menu.rs`). Read by the runtime for the host menu bar.
    pub menus: Vec<crate::user32::menu::MenuRecord>,
    /// Set by any menu mutation so the host menu-bar sync rebuilds its cached
    /// tree instead of reconstructing it every frame. Read by the runtime.
    pub menu_dirty: bool,
    /// Class-level `SetClassLongPtr` values keyed by (class atom, signed index).
    pub(crate) class_long_ptr_values: Vec<(u16, i64, u64)>,
    pub message_queue_idle_policy: MessageQueueIdlePolicy,
    pub(crate) next_window_class_atom: u16,
    pub(crate) window_classes: Vec<WindowClassRecord>,
    pub(crate) next_window_handle: crate::handles::Hwnd,
    pub windows: Vec<WindowRecord>,
    /// Per-window UI state for built-in controls (pressed/focus/items).
    pub(crate) control_states:
        ahash::HashMap<crate::handles::Hwnd, crate::user32::controls::ControlState>,
    pub file_dialog_policy: FileDialogPolicy,
    pub last_file_dialog_path: Option<String>,
    pub(crate) comm_dlg_extended_error: u32,
    pub(crate) next_menu_handle: crate::handles::Hmenu,
    /// Guest VA of the modal-dialog result slot (`u32`), set by session init.
    ///
    /// `EndDialog` writes the result here; the in-guest `DialogBoxParam` stub
    /// reads it after its `WM_QUIT`. Zero when no dialog machinery is wired.
    pub dialog_result_va: u64,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            dialog_result_va: 0,
            next_window_class_atom: 0xC000,
            next_window_handle: crate::handles::Hwnd::from(0x0000_0000_6610_0000),
            next_menu_handle: crate::handles::Hmenu::from(0x0000_0000_6620_0000),
            next_windows_hook_handle: crate::handles::HookHandle::from(0x0000_0000_6630_0000),
            next_global_atom: 0xC000,
            next_registered_message: 0xC000,
            registered_messages: ahash::HashMap::new(),
            next_timer_id: 1,
            window_long_ptr_values: Vec::new(),
            image_list_counts: Vec::new(),
            image_list_background_colors: Vec::new(),
            window_visible: false,
            window_enabled: false,
            active_window_handle: crate::handles::Hwnd::NULL,
            foreground_window_handle: crate::handles::Hwnd::NULL,
            focus_window_handle: crate::handles::Hwnd::NULL,
            capture_window_handle: crate::handles::Hwnd::NULL,
            cursor_handle: 0,
            window_title: String::new(),
            window_x: 0,
            window_y: 0,
            window_width: 0,
            window_height: 0,
            tick_count: 0,
            keyboard_state: KeyboardState::default(),
            timers: Vec::new(),
            global_atoms: Vec::new(),
            windows_hooks: Vec::new(),
            class_long_ptr_values: Vec::new(),
            message_queue_idle_policy: MessageQueueIdlePolicy::default(),
            window_classes: Vec::new(),
            windows: Vec::new(),
            control_states: ahash::HashMap::new(),
            file_dialog_policy: FileDialogPolicy::default(),
            last_file_dialog_path: None,
            comm_dlg_extended_error: 0,
            menus: Vec::new(),
            menu_dirty: false,
        }
    }
}

/// Registered fake USER32 window class.
#[derive(Debug, Clone)]
pub struct WindowClassRecord {
    /// Atom returned by `RegisterClassExA/W`.
    pub atom: u16,

    /// Registered class name.
    pub class_name: String,

    /// Guest address of the class window procedure.
    pub window_proc: u64,

    /// Class style flags.
    pub style: u32,

    /// Module instance associated with the class.
    pub instance_handle: u64,

    /// Default icon handle.
    pub icon_handle: u64,

    /// Default cursor handle.
    pub cursor_handle: u64,

    /// Background brush handle.
    pub background_brush: u64,

    /// Small icon handle.
    pub small_icon_handle: u64,

    /// Whether the class was registered through the Unicode API.
    pub unicode: bool,
}

/// Packed window-state flags for [`WindowRecord`].
///
/// The bits that the runtime crate does not read through `WinApiState`
/// (`visible`, `invalidated`, `mouse_tracking` stay plain bools there) live in
/// one `u16` so `WindowRecord` keeps a handful of independently documented
/// fields rather than a bool per flag.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct WindowFlags(u16);

impl WindowFlags {
    /// The window is shown.
    pub const VISIBLE: Self = Self(1 << 0);
    /// The window is enabled for mouse/keyboard input.
    pub const ENABLED: Self = Self(1 << 1);
    /// The window has been invalidated and needs a repaint.
    pub const INVALIDATED: Self = Self(1 << 2);
    /// The pending repaint cycle must erase the background first.
    pub const ERASE_BACKGROUND: Self = Self(1 << 3);
    /// `TrackMouseEvent` armed hover/leave tracking for this window.
    pub const MOUSE_TRACKING: Self = Self(1 << 4);
    /// Mouse press tracking shared by every control kind.
    pub const PRESSED: Self = Self(1 << 5);
    /// Keyboard focus tracking shared by every control kind.
    pub const FOCUSED: Self = Self(1 << 6);

    /// Whether `flag` is set.
    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 != 0
    }

    /// Set `flag`.
    pub const fn insert(&mut self, flag: Self) {
        self.0 |= flag.0;
    }

    /// Clear `flag`.
    pub const fn remove(&mut self, flag: Self) {
        self.0 &= !flag.0;
    }

    /// The raw `u16` bit pattern.
    pub const fn bits(self) -> u16 {
        self.0
    }
}

/// USER32 window created inside the compatibility runtime.
#[derive(Debug, Clone, Default)]
pub struct WindowRecord {
    /// Runtime-owned fake HWND.
    pub handle: crate::handles::Hwnd,

    /// Registered class atom.
    pub class_atom: u16,

    /// Registered class name.
    pub class_name: String,

    /// Guest address of the window procedure.
    pub window_proc: u64,

    /// Whether the class uses the Unicode window procedure contract.
    pub unicode: bool,

    /// Window title.
    pub title: String,

    /// Standard window style flags.
    pub style: u32,

    /// Extended window style flags.
    pub extended_style: u32,

    /// Parent or owner window.
    pub parent_handle: crate::handles::Hwnd,

    /// Menu handle or child-window identifier.
    pub menu_handle: u64,

    /// Module instance passed to `CreateWindowExA/W`.
    pub instance_handle: u64,

    /// Initial horizontal position.
    pub x: i32,

    /// Initial vertical position.
    pub y: i32,

    /// Initial width.
    pub width: i32,

    /// Initial height.
    pub height: i32,

    /// Current visibility state.
    ///
    /// Kept as a plain bool because the runtime crate reads it directly.
    pub visible: bool,

    /// Packed window-state flags (enabled / erase-background / press / focus).
    pub flags: WindowFlags,

    /// Whether the window has been invalidated and needs a repaint.
    ///
    /// Kept as a plain bool because the runtime crate writes it directly.
    pub invalidated: bool,

    /// Whether `TrackMouseEvent` armed hover/leave tracking for this window.
    ///
    /// Kept as a plain bool because the runtime crate reads it directly.
    pub mouse_tracking: bool,

    /// Client rectangle (left, top, right, bottom).
    pub client_rect: (i32, i32, i32, i32),

    /// Built-in control class this window belongs to (None for normal
    /// application windows). Controls have no guest WndProc; the runtime
    /// dispatches their messages through `dispatch_control_proc`.
    pub control_kind: Option<crate::user32::controls::ControlClassKind>,

    /// Text buffer for built-in controls (WM_GETTEXT / WM_SETTEXT / painting).
    pub control_text: String,

    /// Guest dialog procedure (`DialogBoxParam` `lpDialogFunc`); 0 for normal
    /// windows. Dialog windows have no guest WndProc — the runtime bridges
    /// `WM_INITDIALOG` / `WM_COMMAND` to this address instead.
    pub dialog_proc: u64,

    /// Whether the dialog procedure uses the Unicode contract.
    pub dialog_unicode: bool,
}

/// Controls what value the outer API returns after a guest WndProc completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OuterReturn {
    /// Return whatever the WndProc returned (default for DispatchMessage etc.).
    Passthrough,
    /// Return the given HWND (used by CreateWindowExA/W — unless WM_CREATE returned -1).
    CreateWindow(u64),
    /// Always return a fixed value (used by DestroyWindow after WM_DESTROY).
    Fixed(u64),
}

/// Request to invoke a function located inside guest executable code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestCallbackRequest {
    /// Guest address of the callback function.
    pub callback_address: u64,

    /// Target runtime-owned window handle.
    pub window_handle: u64,

    /// Numeric Windows message identifier.
    pub message: u32,

    /// Message word parameter.
    pub word_parameter: u64,

    /// Message long parameter.
    pub long_parameter: u64,

    /// Whether the target window class uses the Unicode contract.
    pub unicode: bool,

    /// Controls what the outer API returns after the WndProc completes.
    pub outer_return: OuterReturn,
}

/// A queued fake USER32 message.
#[derive(Debug, Clone)]
pub struct QueuedWindowMessage {
    /// Target window handle.
    pub window_handle: crate::handles::Hwnd,

    /// Numeric Windows message identifier.
    pub message: u32,

    /// Message word parameter.
    pub word_parameter: u64,

    /// Message long parameter.
    pub long_parameter: u64,

    /// Deterministic fake message timestamp.
    pub time: u32,

    /// Fake cursor X coordinate.
    pub point_x: i32,

    /// Fake cursor Y coordinate.
    pub point_y: i32,
}

/// Registered fake USER32 hook.
#[derive(Debug, Clone)]
pub struct WindowsHookRecord {
    /// Fake hook handle returned to the guest.
    pub handle: u64,

    /// Hook type such as `WH_CBT` or `WH_CALLWNDPROC`.
    pub hook_type: i32,

    /// Guest hook procedure address.
    pub callback_address: u64,

    /// Optional module handle supplied by the guest.
    pub module_handle: u64,

    /// Target thread identifier, or zero for a global hook.
    pub thread_id: u32,
}

/// Fake global atom table entry.
#[derive(Debug, Clone)]
pub struct GlobalAtomRecord {
    /// Atom identifier.
    pub atom: u16,

    /// Stored ANSI atom name.
    pub name: String,
}

/// Fake USER32 timer record.
#[derive(Debug, Clone)]
pub struct TimerRecord {
    /// Window associated with the timer, or zero for a thread timer.
    pub window_handle: crate::handles::Hwnd,

    /// Timer identifier.
    pub timer_id: u64,

    /// Requested timer interval in milliseconds.
    pub interval_ms: u32,

    /// Optional guest timer callback address.
    pub callback_address: u64,

    /// Host-clock deadline for the next `WM_TIMER` synthesis.
    ///
    /// Timers are the one message source driven by the host clock rather than
    /// the deterministic fake `next_message_time` scheme — real Windows timers
    /// are clock-driven too.
    pub next_fire: std::time::Instant,
}

/// Fake resource record.
#[derive(Debug, Clone)]
pub struct ResourceRecord {
    /// Fake resource handle.
    pub handle: u64,

    /// Fake loaded resource handle.
    pub loaded_handle: u64,

    /// Pointer to fake resource bytes.
    pub data_ptr: u64,

    /// Resource size.
    pub size: u32,
}

/// Fake find-file handle (materialized directory enumeration).
///
/// `remaining` is a `VecDeque` so `FindNextFile` pops in O(1) via `pop_front`.
/// Was `Vec<DirEntry>` + `remove(0)`, i.e. O(n) shift per FindNext — scanning a
/// directory with N files became O(N²).
#[derive(Debug, Clone)]
pub struct FindHandle {
    /// Fake find handle.
    pub handle: u64,

    /// Search pattern/path as provided by the guest.
    pub pattern: String,

    /// Remaining entries after the one returned by FindFirst (FindNext consumes).
    pub remaining: std::collections::VecDeque<vfs::DirEntry>,
}

/// Behavior of `GetMessageA` when no matching message is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageQueueIdlePolicy {
    #[default]
    /// Produce a synthetic `WM_QUIT`.
    ///
    /// This preserves the deterministic bootstrap regression path.
    ExitOnIdle,

    /// Yield execution back to the runtime without modifying the guest `MSG`.
    ///
    /// This will be used by the persistent interactive runtime.
    YieldOnIdle,
}

/// Non-error control signal emitted by a WinAPI handler.
#[derive(Debug, Clone, Copy, thiserror::Error)]
pub enum WinApiControlSignal {
    /// `GetMessageA` cannot continue until a message becomes available.
    #[error("waiting for a window message")]
    WaitingForMessage,

    /// `DispatchMessageA/W` requires execution of a guest window procedure.
    #[error("guest window callback requested: {request:?}")]
    GuestCallbackRequested {
        /// Description of the pending guest callback.
        request: GuestCallbackRequest,
    },

    /// Host thread must park (drop CPU lock) then retry / continue (MT.2/3).
    #[error("host park: {reason:?}")]
    HostPark {
        /// Why the host thread is parking.
        reason: HostParkReason,
    },

    /// Guest `ExitThread` — worker run loop should terminate this host thread.
    #[error("exit thread code={code}")]
    ExitThread {
        /// Thread exit code.
        code: u32,
    },
}

/// Reason for [`WinApiControlSignal::HostPark`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostParkReason {
    /// Waiting to enter critical section at guest VA.
    CriticalSection {
        /// Guest `RTL_CRITICAL_SECTION*`.
        cs: u64,
    },
    /// Waiting on a pthread object.
    PthreadWait,
    /// `WaitForSingleObject` (or similar) on a kernel handle.
    WaitObject {
        /// Kernel handle.
        handle: u64,
        /// Timeout in ms (`INFINITE` = forever).
        timeout_ms: u32,
    },
    /// `WaitForMultipleObjects` — handles live in [`SyncState::multi_wait`].
    ///
    /// Kept small/`Copy` so [`WinApiControlSignal`] stays compact; the handle
    /// list is stored on process sync state for the duration of the park.
    WaitMultiple,
}
