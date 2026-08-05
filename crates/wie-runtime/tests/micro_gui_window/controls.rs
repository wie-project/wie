//! Control micro-tests: the mingw `gui_control`/`gui_edit` PEs exercising the
//! host BUTTON/STATIC child model and the multiline EDIT message core.

use crate::helpers::{drive_gui_session, gui_suite_serialize, micro_exe};

#[test]
fn gui_control_child_windows_and_command() {
    let Some(path) = micro_exe("gui_control.exe") else {
        eprintln!(
            "skip: micro-exes/out/gui_control.exe not built (run make -C micro-exes gui_exes)"
        );
        return;
    };
    // Serialized suite: see GUI_SUITE_LOCK.
    let _suite = gui_suite_serialize();

    // The exe creates a BUTTON + STATIC child, self-checks the child model
    // (GetParent/GetDlgCtrlID/IsChild, SetWindowText round-trip), then after
    // TIMER_TICKS simulates a click via SendMessageA(button, WM_LBUTTONDOWN/UP).
    // The host control WndProc fires WM_COMMAND(BN_CLICKED) to the parent,
    // which destroys the window — so exit 0 proves the control dispatch path
    // (incl. control painting + WS_CLIPCHILDREN-clipped parent repaints) ran.
    let exit_code = drive_gui_session(&path, 200);

    assert_eq!(
        exit_code,
        Some(0),
        "gui_control.exe must exit 0 (proves WM_COMMAND from the button fired); got {exit_code:?}"
    );
}

#[test]
fn gui_edit_multiline_edit_messages() {
    let Some(path) = micro_exe("gui_edit.exe") else {
        eprintln!("skip: micro-exes/out/gui_edit.exe not built (run make -C micro-exes gui_exes)");
        return;
    };
    // Serialized suite: see GUI_SUITE_LOCK.
    let _suite = gui_suite_serialize();

    // The exe's selftest drives eight assertion groups against a multiline
    // EDIT (SetWindowText + EM_GETLINECOUNT, EM_LINEFROMCHAR, EM_SETSEL/
    // EM_GETSEL, EM_REPLACESEL, EM_GETMODIFY, WM_COPY →
    // IsClipboardFormatAvailable, EM_UNDO, WM_SETTEXT line-count reset). The
    // first failed group exits with 300..307 (group code + 200) so CI
    // pinpoints the broken message; exit 0 only happens when every group
    // passed, so it proves the EDIT message core end-to-end.
    let exit_code = drive_gui_session(&path, 200);

    assert_eq!(
        exit_code,
        Some(0),
        "gui_edit.exe must exit 0 (proves the multiline EDIT selftest groups passed); \
         got {exit_code:?}"
    );
}
