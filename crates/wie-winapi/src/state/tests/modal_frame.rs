//! ModalFrame protocol tests: the shared "up" (`ModalFrame::activate`) and
//! "down" (`ModalFrame::finish`) halves of a modal session's lifecycle.
//!
//! Covers the frame's contract: depth up/down, activation + focus
//! takeover/restore (including the guest-WndProc focus bridge), the
//! previous-active capture, and the owner invalidation with the erase pattern
//! EndDialog uses.

use super::*;
use crate::handles::Hwnd;
use crate::state::WindowFlags;
use crate::user32::dialog::{ModalFrame, ModalResult, NativePanelCtx, NativePanelKind};
use crate::user32::{
    CreateWindowRequest, WM_SETFOCUS, WindowClassIdentifier, create_window_record, find_window_mut,
};
use crate::{OuterReturn, WinApiControlSignal};

/// A top-level window that can act as the modal owner. `proc != 0` gives it a
/// guest WndProc (so `ModalFrame::finish`'s focus restore bridges).
fn create_top_window(state: &mut WinApiState, class: &str, proc: u64) -> u64 {
    let (hwnd, _, _) = create_window_record(
        state,
        CreateWindowRequest {
            class_identifier: WindowClassIdentifier::Name(class.to_owned()),
            title: class.to_owned(),
            style: 0,
            extended_style: 0,
            parent_handle: 0,
            menu_handle: 0,
            instance_handle: 0,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        },
        true,
    )
    .expect("create window");
    if proc != 0
        && let Some(window) = find_window_mut(state, hwnd)
    {
        window.window_proc = proc;
    }
    hwnd
}

/// A child window (no guest proc) of `parent`.
fn create_child(state: &mut WinApiState, parent: u64, class: &str) -> u64 {
    let (hwnd, _, _) = create_window_record(
        state,
        CreateWindowRequest {
            class_identifier: WindowClassIdentifier::Name(class.to_owned()),
            title: String::new(),
            style: 0,
            extended_style: 0,
            parent_handle: parent,
            menu_handle: 0,
            instance_handle: 0,
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        },
        true,
    )
    .expect("create child");
    hwnd
}

/// The full frame cycle against a plain owner: activate captures the owner and
/// takes depth/activation/focus + first-paint invalidation; finish undoes all
/// of it and invalidates the owner with the erase pattern.
#[test]
fn modal_frame_activate_and_finish_round_trip() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let owner = create_top_window(&mut state, "Owner", 0);
    state.window_state().active_window_handle = Hwnd::from(owner);
    let dialog = create_child(&mut state, owner, "Dialog");
    let control = create_child(&mut state, dialog, "Control");

    let (frame, signal) = ModalFrame::activate(
        &mut state,
        &mut engine,
        dialog,
        Some(control),
        &[dialog, control],
    )
    .expect("activate succeeds");
    assert!(signal.is_none(), "host-side controls do not bridge focus");

    // Up half: depth, activation, focus, first-paint invalidation.
    assert_eq!(state.lock_message_queue().dialog_depth, 1);
    assert_eq!(state.window_state().active_window_handle.as_u64(), dialog);
    assert_eq!(state.window_state().focus_window_handle.as_u64(), control);
    assert!(
        find_window_mut(&mut state, dialog).is_some_and(|window| window.invalidated),
        "the dialog is seeded with a first paint"
    );
    assert!(
        find_window_mut(&mut state, control).is_some_and(|window| window.invalidated),
        "the focused control is seeded with a first paint"
    );
    assert_eq!(
        frame.previous_active.as_u64(),
        owner,
        "activate captures the owner for finish"
    );

    // Down half: everything restored, owner invalidated with the erase flag.
    let signal = frame
        .finish(&mut state, &mut engine, ModalResult::Ok(1))
        .expect("finish succeeds");
    assert!(
        signal.is_none(),
        "an owner without a guest proc bridges nothing"
    );
    assert_eq!(state.lock_message_queue().dialog_depth, 0);
    assert_eq!(
        state.window_state().active_window_handle.as_u64(),
        owner,
        "activation returns to the owner"
    );
    assert_eq!(
        state.window_state().focus_window_handle.as_u64(),
        owner,
        "focus returns to the owner"
    );
    let owner_record = state
        .window_state()
        .windows
        .iter()
        .find(|window| window.handle == Hwnd::from(owner))
        .expect("owner window");
    assert!(
        owner_record.invalidated,
        "the owner repaints over the dialog region"
    );
    assert!(
        owner_record.flags.contains(WindowFlags::ERASE_BACKGROUND),
        "the erase pattern EndDialog uses is applied to the owner"
    );
}

/// A modal that took focus hands it back to a guest-WndProc owner through the
/// `deliver_focus_change` bridge, and the bridge completes the closing API
/// call with the modal result's value.
#[test]
fn modal_frame_finish_bridges_focus_back_to_a_guest_wndproc_owner() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let owner = create_top_window(&mut state, "Owner", 0x7000_0000_1000);
    state.window_state().active_window_handle = Hwnd::from(owner);
    let dialog = create_child(&mut state, owner, "Dialog");
    let control = create_child(&mut state, dialog, "Control");

    let (frame, _signal) = ModalFrame::activate(
        &mut state,
        &mut engine,
        dialog,
        Some(control),
        &[dialog, control],
    )
    .expect("activate succeeds");

    let signal = frame
        .finish(&mut state, &mut engine, ModalResult::Ok(7))
        .expect("finish succeeds")
        .expect("the guest-WndProc owner must receive WM_SETFOCUS through the bridge");
    let WinApiControlSignal::GuestCallbackRequested { request } = signal else {
        panic!("expected a guest-callback request");
    };
    assert_eq!(request.message, WM_SETFOCUS);
    assert_eq!(request.window_handle, owner);
    assert_eq!(
        request.outer_return,
        OuterReturn::Fixed(7),
        "the modal result completes the closing API call"
    );
    assert_eq!(
        state.window_state().focus_window_handle.as_u64(),
        owner,
        "the focus moved back before the bridge runs"
    );
    assert_eq!(state.lock_message_queue().dialog_depth, 0);
}

/// A modal that never took focus (no `WS_TABSTOP` child) leaves the owner's
/// focus untouched on finish.
#[test]
fn modal_frame_finish_leaves_focus_when_the_modal_never_took_it() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let owner = create_top_window(&mut state, "Owner", 0);
    state.window_state().active_window_handle = Hwnd::from(owner);
    state.window_state().focus_window_handle = Hwnd::from(owner);
    let dialog = create_child(&mut state, owner, "Dialog");

    let (frame, _signal) = ModalFrame::activate(&mut state, &mut engine, dialog, None, &[dialog])
        .expect("activate succeeds");
    assert_eq!(
        state.window_state().focus_window_handle.as_u64(),
        owner,
        "no WS_TABSTOP child → the owner keeps focus"
    );

    let signal = frame
        .finish(&mut state, &mut engine, ModalResult::Cancel)
        .expect("finish succeeds");
    assert!(signal.is_none());
    assert_eq!(state.lock_message_queue().dialog_depth, 0);
    assert_eq!(
        state.window_state().focus_window_handle.as_u64(),
        owner,
        "focus is untouched when the modal never took it"
    );
}

/// A modal opened with no window active (headless / pre-window session):
/// activation/focus restore and the owner invalidation are no-ops, and the
/// depth still balances.
#[test]
fn modal_frame_without_owner_skips_restore_and_invalidation() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let dialog = create_child(&mut state, 0, "Dialog");
    let control = create_child(&mut state, dialog, "Control");

    let (frame, _signal) = ModalFrame::activate(
        &mut state,
        &mut engine,
        dialog,
        Some(control),
        &[dialog, control],
    )
    .expect("activate succeeds");
    assert_eq!(frame.previous_active.as_u64(), 0, "nothing was active");
    assert_eq!(state.lock_message_queue().dialog_depth, 1);

    frame
        .finish(&mut state, &mut engine, ModalResult::Cancel)
        .expect("finish must not panic without an owner");
    assert_eq!(state.lock_message_queue().dialog_depth, 0);
    assert_eq!(state.window_state().active_window_handle.as_u64(), 0);
}

/// The native-bridge frame shape (no guest window — the active window IS the
/// modal): depth up at the panel launch, down at the return, the active window
/// restored (a no-op) and guest focus never touched.
#[test]
fn modal_frame_native_bridge_shape_balances_depth_and_owner() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let owner = create_top_window(&mut state, "Owner", 0);
    state.window_state().active_window_handle = Hwnd::from(owner);

    // The bridge first entry: activate keyed by the active window, no focus,
    // no subtree.
    let (frame, _signal) =
        ModalFrame::activate(&mut state, &mut engine, owner, None, &[]).expect("activate succeeds");
    assert_eq!(state.lock_message_queue().dialog_depth, 1);
    assert_eq!(
        state.window_state().active_window_handle.as_u64(),
        owner,
        "the panel has no guest window; the owner stays active"
    );
    assert_eq!(state.window_state().focus_window_handle.as_u64(), 0);

    // The bridge re-entry: finish restores depth and the active window.
    let signal = frame
        .finish(&mut state, &mut engine, ModalResult::Ok(1))
        .expect("finish succeeds");
    assert!(signal.is_none(), "native panels never take guest focus");
    assert_eq!(state.lock_message_queue().dialog_depth, 0);
    assert_eq!(state.window_state().active_window_handle.as_u64(), owner);
}

/// The shared native-panel ctx (`NativePanelCtx::open` / `finish`) wraps the
/// native-bridge shape from the test above and balances depth / activation
/// across the two bridge entries for every kind. A `None` frame (a bridge
/// that opened no frame) finishes nothing — the depth stays untouched.
#[test]
fn native_panel_open_and_finish_balance_depth_and_focus() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let owner = create_top_window(&mut state, "Owner", 0);
    state.window_state().active_window_handle = Hwnd::from(owner);

    // The bridge first entry: the shared up half opens the frame keyed by the
    // active window, no focus, no subtree.
    let frame = {
        let mut native = NativePanelCtx::new(&mut state, &mut engine, NativePanelKind::File);
        native.open().expect("open succeeds")
    };
    assert_eq!(state.lock_message_queue().dialog_depth, 1);
    assert_eq!(
        state.window_state().active_window_handle.as_u64(),
        owner,
        "the panel has no guest window; the owner stays active"
    );
    assert_eq!(
        state.window_state().focus_window_handle.as_u64(),
        0,
        "native panels never take guest focus"
    );

    // The bridge re-entry: the shared down half restores depth + activation.
    let signal = {
        let mut native = NativePanelCtx::new(&mut state, &mut engine, NativePanelKind::File);
        native
            .finish(Some(frame), ModalResult::Ok(1))
            .expect("finish succeeds")
    };
    assert!(signal.is_none(), "native panels never take guest focus");
    assert_eq!(state.lock_message_queue().dialog_depth, 0);
    assert_eq!(state.window_state().active_window_handle.as_u64(), owner);

    // A bridge that opened no frame finishes nothing: the depth is untouched.
    let signal = {
        let mut native = NativePanelCtx::new(&mut state, &mut engine, NativePanelKind::File);
        native
            .finish(None, ModalResult::Cancel)
            .expect("finish with no frame succeeds")
    };
    assert!(signal.is_none());
    assert_eq!(state.lock_message_queue().dialog_depth, 0);
}
