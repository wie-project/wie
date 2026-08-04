//! Window, UI, hook, timer, resource, and control-signal state types.

use crate::vfs;
use ahash::HashMapExt;

use super::input::KeyboardState;
use super::process::{FileDialogPolicy, FontDialogPolicy};

/// A parsed `OPENFILENAME.lpstrFilter` group: a display name and its extension
/// globs (`"Text Documents"` → `["*.txt"]`).
///
/// Only simple `*.ext` globs survive the comdlg32 parse — a wrong native
/// filter grays out every file on macOS, which is worse than showing all
/// files, so anything complex is dropped wholesale.
#[derive(Debug, Clone)]
pub struct FileDialogFilter {
    /// The filter's display name (rfd shows it on Windows/Linux only).
    pub name: String,
    /// Simple `*.ext` globs, e.g. `*.txt`.
    pub patterns: Vec<String>,
}

/// One host file-dialog invocation: everything the native panel starts from.
///
/// Built by the comdlg32 handler from the guest's `OPENFILENAME`; consumed by
/// the host bridge registered via `GuestHandle::set_file_dialog_bridge`.
#[derive(Debug, Clone)]
pub struct FileDialogRequest {
    /// Host directory the panel starts in — the guest's initial directory
    /// mapped into the bottle (`C:\…` → `{root}/drive_c/…`). `None` when no
    /// guest-visible directory maps (the panel picks its own default).
    pub initial_host_dir: Option<std::path::PathBuf>,
    /// The `lpstrFile` basename, seeding the panel's name field (a save panel
    /// proposes it as the default file name).
    pub default_file_name: Option<String>,
    /// Whether this is a Save panel (`GetSaveFileName`): the host shows an
    /// NSSavePanel, whose overwrite confirmation is native.
    pub is_save: bool,
    /// Best-effort parse of the guest's `lpstrFilter` (empty when complex or
    /// absent — the panel then shows every file).
    pub filters: Vec<FileDialogFilter>,
}

/// A successful native file-dialog pick: the selected HOST path.
///
/// The comdlg32 handler maps it back into a guest-visible path at accept and
/// refuses (cancel) any pick outside the bottle — the guest filesystem cannot
/// see the file otherwise.
#[derive(Debug, Clone)]
pub struct FileDialogPick {
    /// The host path the user picked in the native panel.
    pub host_path: std::path::PathBuf,
}

/// One host MessageBox invocation: everything the native alert starts from.
///
/// Built by the `MessageBoxA/W` handlers from the guest arguments; consumed by
/// the host bridge registered via `GuestHandle::set_message_box_bridge`.
#[derive(Debug, Clone)]
pub struct MessageBoxRequest {
    /// The dialog caption (the `lpCaption` argument).
    pub caption: String,
    /// The message text (the `lpText` argument).
    pub text: String,
    /// The `MB_*` flag bits verbatim (button set, icon, …) — the host bridge
    /// maps them to its own button/level sets, keeping rfd types out of this
    /// crate.
    pub message_box_type: u32,
}

/// One in-flight NATIVE MessageBox (host alert).
///
/// The `MessageBoxA/W` handlers record this on their first entry (state lock
/// held) and return [`WinApiControlSignal::MessageBoxBridgeRequested`]; the
/// runtime then runs the bridge WITHOUT the shared lock (the winit event loop
/// needs that lock while the alert is up — holding it across the modal
/// session deadlocks into the beachball) and stores the chosen Win32 id back
/// here. The engine re-executes the fake API, the handler re-enters, takes
/// this record, and returns the id to the guest.
#[derive(Debug, Clone)]
pub struct PendingNativeMessageBox {
    /// The Win32 id the user chose (IDOK/IDCANCEL/IDYES/IDNO). `None` = the
    /// bridge never answered (it vanished mid-call — a racing teardown must
    /// not hang the guest); the handler falls back to IDCANCEL.
    pub pick: Option<i32>,
}

/// One in-flight NATIVE (host-panel) file dialog.
///
/// The `GetOpenFileName`/`GetSaveFileName` handler records this on its first
/// entry (state lock held) and returns
/// [`WinApiControlSignal::FileDialogBridgeRequested`]; the runtime then runs
/// the bridge WITHOUT the shared lock (the winit event loop needs that lock
/// while the panel is up — holding it across the modal session deadlocks into
/// the beachball) and stores the pick back here. The engine re-executes the
/// fake API, the handler re-enters, takes this record, and writes the pick
/// into the guest `OPENFILENAME` buffer.
#[derive(Debug, Clone)]
pub struct PendingNativeFileDialog {
    /// Guest VA of the `OPENFILENAME` structure.
    pub ofn_ptr: u64,
    /// `OPENFILENAME.lpstrFile` buffer VA.
    pub file_buffer_ptr: u64,
    /// `OPENFILENAME.nMaxFile` (buffer capacity in TCHARs).
    pub max_file: u32,
    /// `OPENFILENAME.lpstrFileTitle` buffer VA (0 = none).
    pub file_title_ptr: u64,
    /// `OPENFILENAME.nMaxFileTitle`.
    pub max_file_title: u32,
    /// Whether the W (UTF-16) variant is in use.
    pub unicode: bool,
    /// The bridge's pick (the runtime records it; `None` = user cancelled or
    /// the bridge vanished mid-call).
    pub pick: Option<FileDialogPick>,
}

/// Host file-dialog callback: `(request) → pick, or `None` (user cancelled)`.
///
/// Registered by the GUI presenter via `GuestHandle::set_file_dialog_bridge`;
/// invoked by the `GetOpenFileNameA/W` / `GetSaveFileNameA/W` handlers on the
/// guest thread, which blocks until the native panel closes (dialog
/// semantics — the same seam as the MessageBox bridge).
pub type FileDialogBridge = Box<dyn Fn(&FileDialogRequest) -> Option<FileDialogPick> + Send>;

/// One in-flight interactive file dialog (`GetOpenFileName` / `GetSaveFileName`).
///
/// Created by the comdlg32 handler when [`FileDialogPolicy::Interactive`] is
/// set: it builds the file-dialog window + controls and stores everything the
/// write-back needs here. Consumed by the `EndDialog` handler when the dialog
/// closes — the chosen path is written into the guest's `OPENFILENAME` buffer
/// there, before the modal loop returns to the caller. `None` when no file
/// dialog is open.
#[derive(Debug, Clone)]
pub struct FileDialogSession {
    /// The file-dialog window handle (a "FileDialog"-class window carrying the
    /// file-dialog proc stub as its `dialog_proc`).
    pub dialog_hwnd: u64,
    /// The single-line EDIT child holding the typed path.
    pub edit_hwnd: u64,
    /// Guest VA of the `OPENFILENAME` structure.
    pub ofn_ptr: u64,
    /// `OPENFILENAME.lpstrFile` buffer VA.
    pub file_buffer_ptr: u64,
    /// `OPENFILENAME.nMaxFile` (buffer capacity in TCHARs).
    pub max_file: u32,
    /// `OPENFILENAME.lpstrFileTitle` buffer VA (0 = none).
    pub file_title_ptr: u64,
    /// `OPENFILENAME.nMaxFileTitle`.
    pub max_file_title: u32,
    /// Whether the W (UTF-16) variant is in use.
    pub unicode: bool,
    /// Guest directory the dialog lists (Windows style, e.g. `C:\work`).
    pub initial_dir: String,
    /// `OPENFILENAME.lpstrDefExt`, appended when the typed name has no dot.
    pub default_extension: Option<String>,
}

/// One in-flight host-owned modeless Find/Replace dialog
/// (`FindTextW` / `ReplaceTextW`, comdlg32).
///
/// Unlike the modal file dialog, the find dialog never runs an in-guest loop:
/// the guest's main loop keeps pumping (modeless), so the dialog is just a
/// window whose controls dispatch host-side — button clicks are handled in
/// `comdlg32::handle_find_dialog_command`, which writes the user's choices
/// back into the guest `FINDREPLACE` struct and posts `FINDMSGSTRING` to the
/// owner window. `None`-free: `WindowState::find_dialogs` is a `Vec` because
/// a guest may hold a Find and a Replace dialog open at once.
#[derive(Debug, Clone)]
pub struct FindDialogSession {
    /// The find-dialog window handle (a "FindDialog"-class window).
    pub dialog_hwnd: u64,
    /// Guest VA of the `FINDREPLACE` structure — the `FINDMSGSTRING` lParam.
    pub fr_ptr: u64,
    /// `FINDREPLACE.hwndOwner` — the window `FINDMSGSTRING` is posted to.
    pub owner_hwnd: u64,
    /// Guest VA of `FINDREPLACE.lpstrFindWhat` (the search-string buffer).
    pub find_what_ptr: u64,
    /// `FINDREPLACE.wFindWhatLen` (buffer capacity in UTF-16 units).
    pub find_what_len: u32,
    /// Guest VA of `FINDREPLACE.lpstrReplaceWith` (0 in find mode).
    pub replace_with_ptr: u64,
    /// `FINDREPLACE.wReplaceWithLen`.
    pub replace_with_len: u32,
    /// The find-what EDIT control (seeded from `lpstrFindWhat`).
    pub find_edit_hwnd: u64,
    /// The replace-with EDIT control (0 in find mode).
    pub replace_edit_hwnd: u64,
    /// The "Match case" checkbox button.
    pub match_case_hwnd: u64,
    /// The "Match whole word" checkbox button.
    pub whole_word_hwnd: u64,
    /// Host-side checkbox state, mirrored into `FINDREPLACE.Flags` on submit.
    pub match_case_checked: bool,
    /// Host-side checkbox state, mirrored into `FINDREPLACE.Flags` on submit.
    pub whole_word_checked: bool,
    /// Whether this is a Replace dialog (`ReplaceTextW`) vs a Find dialog.
    pub replace_mode: bool,
}

/// One in-flight modal font dialog (`ChooseFontW`).
///
/// Created by the comdlg32 handler when [`FontDialogPolicy::Interactive`] is
/// set: it builds the font-dialog window (family LISTBOX, size LISTBOX,
/// Strikeout/Underline effects buttons, OK/Cancel) and runs the file dialog's
/// in-guest modal loop. The `EndDialog` handler writes the selection back
/// into the guest `LOGFONTW` (via `lpLogFont`) and the `CHOOSEFONTW` fields;
/// the effects buttons close through the shared dialog-proc stub with sentinel
/// results, which this session translates into checkbox toggles (the dialog
/// stays open). `None` when no font dialog is open.
#[derive(Debug, Clone)]
pub struct FontDialogSession {
    /// The font-dialog window handle (a "FontDialog"-class window carrying the
    /// file-dialog proc stub as its `dialog_proc`).
    pub dialog_hwnd: u64,
    /// Guest VA of the `CHOOSEFONTW` structure.
    pub cf_ptr: u64,
    /// Guest VA of the `LOGFONTW` the selection is written back into.
    pub log_font_ptr: u64,
    /// Guest `CHOOSEFONTW.rgbColors` (preserved; no color picker).
    pub rgb_colors: u32,
    /// Guest `CHOOSEFONTW.Flags`, preserved and OR'ed with `CF_SCREENFONTS`.
    pub flags: u32,
    /// The family LISTBOX control.
    pub family_list_hwnd: u64,
    /// The size LISTBOX control.
    pub size_list_hwnd: u64,
    /// The Strikeout effects button (`[x]` / `[ ]` caption carries the state).
    pub strikeout_hwnd: u64,
    /// The Underline effects button.
    pub underline_hwnd: u64,
    /// Host-side Strikeout toggle state.
    pub strikeout_checked: bool,
    /// Host-side Underline toggle state.
    pub underline_checked: bool,
    /// Selected family (seeded from the guest `LOGFONTW.lfFaceName`).
    pub selected_family: String,
    /// Selected point size in tenths of points (seeded from `lfHeight`).
    pub selected_point_size: i32,
}

/// Window, UI, and input state.
///
/// Fields are `pub(crate)` except the ones the runtime reads directly through
/// `WinApiState::window_state()` (windows, capture/focus handles, menus,
/// dialog/file-dialog plumbing, keyboard state).
///
/// Deliberately NOT `Debug`/`Clone`: the host file-dialog bridge
/// ([`FileDialogBridge`]) is a `Box<dyn Fn …>`, which is neither — and no
/// caller snapshots the whole window state.
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
    /// Pending guest-requested host-window geometry change, for the winit
    /// window.
    ///
    /// `(x, y, width, height)` in screen coordinates (rcNormalPosition
    /// semantics). Set by the `SetWindowPlacement` handler when the applied
    /// rect differs from the current one; consumed and cleared by
    /// `GuestHandle::take_host_geometry_request` (the wie-runtime session
    /// window layer) so the host presenter can move the real window. `None`
    /// when no geometry change is pending.
    pub host_geometry_request: Option<(i32, i32, i32, i32)>,
    /// The hwnd the pending [`Self::host_geometry_request`] was applied to.
    ///
    /// Kept as a sibling slot (not folded into the tuple) so the winapi
    /// handler and its tests keep reading `host_geometry_request` unchanged;
    /// the runtime's `take_host_geometry_request` returns both, letting the
    /// host apply the move to the matching winit window when the guest has
    /// more than one top-level.
    pub host_geometry_hwnd: Option<u64>,
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
    /// In-flight interactive file dialog, when [`FileDialogPolicy::Interactive`]
    /// is set and a dialog is open. See [`FileDialogSession`].
    pub file_dialog: Option<FileDialogSession>,
    /// Optional host native file-dialog bridge, registered by the GUI
    /// presenter via `GuestHandle::set_file_dialog_bridge`.
    ///
    /// When set, `GetOpenFileName`/`GetSaveFileName` under
    /// [`FileDialogPolicy::Interactive`] call it with the request and write
    /// its pick back into the `OPENFILENAME` buffer (a pick outside the
    /// bottle cancels). When unset the handlers keep the in-app emulated
    /// dialog, so headless runs and `trace` never see a native panel.
    /// Mirrors the MessageBox bridge seam (`present::message_box_bridge`).
    pub file_dialog_bridge: Option<FileDialogBridge>,
    /// In-flight native file dialog: the guest is parked in
    /// `GetOpenFileName`/`GetSaveFileName` while the host panel is up. See
    /// [`PendingNativeFileDialog`].
    pub pending_native_file_dialog: Option<PendingNativeFileDialog>,
    /// In-flight native MessageBox: the guest is parked in `MessageBoxA/W`
    /// while the host alert is up. See [`PendingNativeMessageBox`].
    pub pending_native_message_box: Option<PendingNativeMessageBox>,
    /// Host-side decision for `ChooseFontW` (Interactive shows the host font
    /// dialog; Cancel returns FALSE without one).
    pub font_dialog_policy: FontDialogPolicy,
    /// In-flight modal font dialog, when [`FontDialogPolicy::Interactive`] is
    /// set and a dialog is open. See [`FontDialogSession`].
    pub font_dialog: Option<FontDialogSession>,
    /// All in-flight host-owned modeless Find/Replace dialogs (FindTextW /
    /// ReplaceTextW). See [`FindDialogSession`]. Multiple dialogs can be open
    /// at once (a guest may show Find and Replace together).
    pub find_dialogs: Vec<FindDialogSession>,
    /// Guest VA of the planted file-dialog modal-loop body (set by session
    /// init alongside `dialog_result_va`). Zero when the dialog machinery is
    /// absent, in which case `Interactive` falls back to `Cancel`.
    pub file_dialog_loop_va: u64,
    /// Guest VA of the planted file-dialog proc stub — the window's
    /// `dialog_proc`, which turns `WM_COMMAND(IDOK/IDCANCEL)` / `WM_CLOSE`
    /// into an `EndDialog` call.
    pub file_dialog_proc_va: u64,
    pub(crate) comm_dlg_extended_error: u32,
    pub(crate) next_menu_handle: crate::handles::Hmenu,
    /// All fake USER32 accelerator tables loaded by `LoadAcceleratorsA/W`;
    /// keyed by handle. The parsed entries live on
    /// `ProcessState::main_module_accelerators`, so each record only carries
    /// the (module, resource id) pair needed to resolve them.
    pub(crate) accel_tables: Vec<crate::user32::accel::AccelRecord>,
    /// All fake USER32 resource menus loaded by `LoadMenuA/W` (and by a
    /// class's `lpszMenuName` at window creation); keyed by handle. The
    /// parsed items live on `ProcessState::main_module_menus`, so each record
    /// only carries the (module, resource id) pair needed to resolve them —
    /// and that pair is the cache key, so repeated loads return the same
    /// `HMENU`.
    pub(crate) resource_menus: Vec<crate::user32::menu::ResourceMenuRecord>,
    pub(crate) next_accel_handle: crate::handles::Haccel,
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
            next_accel_handle: crate::handles::Haccel::from(0x0000_0000_6640_0000),
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
            host_geometry_request: None,
            host_geometry_hwnd: None,
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
            file_dialog: None,
            file_dialog_bridge: None,
            pending_native_file_dialog: None,
            pending_native_message_box: None,
            font_dialog_policy: FontDialogPolicy::default(),
            font_dialog: None,
            find_dialogs: Vec::new(),
            file_dialog_loop_va: 0,
            file_dialog_proc_va: 0,
            comm_dlg_extended_error: 0,
            menus: Vec::new(),
            menu_dirty: false,
            accel_tables: Vec::new(),
            resource_menus: Vec::new(),
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

    /// `lpszMenuName` from the `WNDCLASS(EX)` struct: a `MAKEINTRESOURCE`
    /// menu resource id (value < 0x10000) or a string-name pointer.
    ///
    /// Resolved to a fake `HMENU` at `CreateWindowEx` time when the window is
    /// created without an explicit `hMenu` argument (the class-menu path
    /// Windows applies to top-level windows).
    pub menu_name: u64,

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
    /// The window registered as a drop target via `DragAcceptFiles`.
    ///
    /// Only the registration flag is stored here; the actual drop path
    /// (`WM_DROPFILES` + `DragQueryFileA/W`) is not wired yet.
    pub const DROP_ACCEPTED: Self = Self(1 << 7);

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

    /// The HFONT a `WM_SETFONT` stored for this window, used by the control
    /// paint paths (and any future window-paint path) to draw the window's
    /// text with the guest-selected font. `Hfont::NULL` (0) means never set —
    /// Windows returns 0 from `WM_GETFONT` until a font is stored, and the
    /// paint paths fall back to the system default in that case.
    pub font_handle: crate::handles::Hfont,

    /// Guest dialog procedure (`DialogBoxParam` `lpDialogFunc`); 0 for normal
    /// windows. Dialog windows have no guest WndProc — the runtime bridges
    /// `WM_INITDIALOG` / `WM_COMMAND` to this address instead.
    pub dialog_proc: u64,

    /// Whether the dialog procedure uses the Unicode contract.
    pub dialog_unicode: bool,

    /// The `GWLP_WNDPROC` value that existed when the guest FIRST subclassed
    /// this window (`SetWindowLongPtrW(GWLP_WNDPROC, …)`).
    ///
    /// For built-in controls that original is WIE's default-control-proc
    /// marker (0 — WIE stores nothing for an un-subclassed control's
    /// `GWLP_WNDPROC`): `CallWindowProcW(hwnd, <this value>, …)` runs the
    /// host default control dispatch instead of invoking a guest proc, which
    /// is exactly how notepad's `EDIT_WndProc` forwards what it does not
    /// handle. Re-subclassing never changes it (the class default stays the
    /// host marker), and it is not cleared when a subclass is removed.
    pub subclass_original_wndproc: u64,
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
#[derive(Debug, Clone, thiserror::Error)]
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

    /// `GetOpenFileName`/`GetSaveFileName` wants the host file panel shown.
    ///
    /// The runtime drops the shared state lock for the whole panel session
    /// (the winit event loop needs that lock to service frame events while
    /// the panel is up) and runs the registered [`FileDialogBridge`] on the
    /// guest thread, then the handler's re-entry writes the pick back.
    #[error("host file dialog bridge requested")]
    FileDialogBridgeRequested {
        /// Everything the native panel starts from.
        request: FileDialogRequest,
    },

    /// `MessageBoxA/W` wants the host alert shown.
    ///
    /// The runtime drops the shared state lock for the whole alert session
    /// (the winit event loop needs that lock to service frame events while
    /// the alert is up) and runs the registered
    /// [`crate::present::MessageBoxBridge`] on the guest thread, then the
    /// handler's re-entry returns the chosen id.
    #[error("host message box bridge requested")]
    MessageBoxBridgeRequested {
        /// Everything the host alert starts from.
        request: MessageBoxRequest,
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
