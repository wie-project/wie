//! File-dialog handler tests (moved out of the old single-file `comdlg32`).

use super::file::{
    FNERR_INVALIDFILENAME, SelectedPathWrite, apply_default_extension, basename_of,
    complete_file_dialog, directory_of, finalize_guest_path, handle_get_open_file_name_w,
    handle_get_save_file_name_w, is_absolute_windows_path, is_simple_filter_glob, list_directory,
    parse_ofn_filter, resolve_initial_dir, split_path_components, write_selected_path,
};
use super::test_support::{
    STACK_TOP, read_guest_u32_at, read_guest_utf16, test_engine, test_environment, test_state,
    utf16_bytes, write_ofn, write_regs,
};
use crate::handles::Hwnd;
use crate::kernel32::{
    handle_close_handle, handle_create_file_w, handle_read_file, handle_write_file,
};
use crate::state::FileDialogSession;
use crate::user32::dialog::handle_end_dialog;
use crate::user32::{IDOK, find_window, find_window_mut};
use crate::vfs::VolumeConfig;
use crate::{
    FileDialogBridge, FileDialogPick, FileDialogPolicy, HandlerContext, WinApiControlSignal,
    WinApiHandlerResult, WinApiState,
};
use std::path::PathBuf;
use wie_cpu::{CpuEngine, IcedCpu};

// ── Pure helpers ──────────────────────────────────────────────────────

#[test]
fn split_windows_path() {
    let (name, file_off, ext_off) = split_path_components(r"C:\Games\level.smc");
    assert_eq!(name, "level.smc");
    assert_eq!(file_off, 9);
    assert_eq!(ext_off, 15);
}

#[test]
fn split_path_without_extension() {
    let (name, file_off, ext_off) = split_path_components(r"C:\Games\level");
    assert_eq!(name, "level");
    assert_eq!(file_off, 9);
    assert_eq!(ext_off, 14);
}

#[test]
fn apply_default_extension_appends_when_name_has_no_dot() {
    assert_eq!(
        apply_default_extension(r"C:\work\notes", Some("txt")),
        r"C:\work\notes.txt"
    );
    // The def-ext often arrives with its dot; the helper trims it.
    assert_eq!(
        apply_default_extension(r"C:\work\notes", Some(".txt")),
        r"C:\work\notes.txt"
    );
}

#[test]
fn apply_default_extension_skips_when_dot_present_or_directory() {
    assert_eq!(
        apply_default_extension(r"C:\work\notes.txt", Some("txt")),
        r"C:\work\notes.txt"
    );
    assert_eq!(
        apply_default_extension(r"C:\work\dir\", Some("txt")),
        r"C:\work\dir\",
        "a directory selection never gains an extension"
    );
    assert_eq!(
        apply_default_extension(r"C:\work\notes", None),
        r"C:\work\notes"
    );
    assert_eq!(
        apply_default_extension(r"C:\work\notes", Some("")),
        r"C:\work\notes"
    );
}

#[test]
fn is_absolute_windows_path_detects_drive_and_unc() {
    assert!(is_absolute_windows_path(r"C:\foo"));
    assert!(is_absolute_windows_path(r"c:/foo"));
    assert!(is_absolute_windows_path(r"\\server\share"));
    assert!(!is_absolute_windows_path("notes.txt"));
    assert!(!is_absolute_windows_path(r"sub\dir\notes.txt"));
}

#[test]
fn directory_and_basename_split() {
    assert_eq!(directory_of(r"C:\foo\bar.txt"), r"C:\foo");
    assert_eq!(basename_of(r"C:\foo\bar.txt"), "bar.txt");
    assert_eq!(directory_of("plain.txt"), "");
    assert_eq!(basename_of("plain.txt"), "plain.txt");
}

fn session_with(initial_dir: &str, def_ext: Option<&str>) -> FileDialogSession {
    FileDialogSession {
        dialog_hwnd: 0,
        edit_hwnd: 0,
        ofn_ptr: 0,
        file_buffer_ptr: 0,
        max_file: 0,
        file_title_ptr: 0,
        max_file_title: 0,
        unicode: true,
        initial_dir: initial_dir.to_owned(),
        default_extension: def_ext.map(str::to_owned),
    }
}

#[test]
fn finalize_guest_path_joins_bare_name_with_directory() {
    let session = session_with(r"C:\work", Some("txt"));
    assert_eq!(finalize_guest_path(&session, "notes"), r"C:\work\notes.txt");
    assert_eq!(
        finalize_guest_path(&session, "notes.txt"),
        r"C:\work\notes.txt"
    );
    assert_eq!(
        finalize_guest_path(&session, "  "),
        "",
        "blank edit cancels"
    );
}

#[test]
fn finalize_guest_path_keeps_absolute_typed_path() {
    let session = session_with(r"C:\work", Some("txt"));
    assert_eq!(
        finalize_guest_path(&session, r"D:\elsewhere\report.md"),
        r"D:\elsewhere\report.md"
    );
    // Trailing-separator directory selection keeps the extension rule off.
    assert_eq!(finalize_guest_path(&session, r"C:\work\"), r"C:\work\");
}

// ── Policy seam (dispatch-level) ──────────────────────────────────────

/// Drive the W handler with a scripted policy; returns the result value.
fn dispatch_open(
    engine: &mut IcedCpu,
    state: &mut WinApiState,
    policy: FileDialogPolicy,
) -> anyhow::Result<WinApiHandlerResult> {
    state.window_state().file_dialog_policy = policy;
    write_regs(engine, 0x5000, 0, 0, 0);
    handle_get_open_file_name_w(&mut HandlerContext::new(engine, test_environment(), state))
}

#[test]
fn accept_policy_writes_utf16_path_into_buffer() {
    let mut engine = test_engine();
    let mut state = test_state();
    let file_buf = 0x6000;
    engine.mem_write(file_buf, &utf16_bytes("*.txt")).ok();
    write_ofn(&mut engine, 0x5000, file_buf, 260, 0);

    let result = dispatch_open(
        &mut engine,
        &mut state,
        FileDialogPolicy::Accept {
            path: r"C:\work\notes.txt".to_owned(),
        },
    )
    .expect("accept must succeed");
    assert_eq!(result.return_value, 1);
    assert_eq!(
        read_guest_utf16(&mut engine, file_buf, 64),
        r"C:\work\notes.txt"
    );
    // nFileOffset points at the basename, nFileExtension after the dot.
    let mut off = [0_u8; 2];
    engine.mem_read(0x5000 + 100, &mut off).ok();
    assert_eq!(u16::from_le_bytes(off), 8);
    engine.mem_read(0x5000 + 102, &mut off).ok();
    assert_eq!(u16::from_le_bytes(off), 14);
    assert_eq!(
        state.window_state().last_file_dialog_path.as_deref(),
        Some(r"C:\work\notes.txt")
    );
}

/// `write_selected_path` ANSI variant: the A API must write the path into
/// `lpstrFile`, the basename into `lpstrFileTitle`, and the
/// `nFileOffset`/`nFileExtension` offsets per the OPENFILENAME contract.
#[test]
fn write_selected_path_ansi_fills_offsets_and_title() {
    let mut engine = test_engine();
    let mut state = test_state();
    let file_buf = 0x6000;
    let title_buf = 0x7000;

    write_selected_path(
        &mut engine,
        &SelectedPathWrite {
            ofn_ptr: 0x5000,
            file_buffer_ptr: file_buf,
            max_file: 260,
            file_title_ptr: title_buf,
            max_file_title: 64,
            path: r"C:\work\notes.txt",
            unicode: false,
        },
    )
    .expect("the ANSI write-back must succeed");

    // ANSI bytes, NUL-terminated.
    let mut raw = [0_u8; 64];
    engine.mem_read(file_buf, &mut raw).ok();
    let path_len = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
    assert_eq!(&raw[..path_len], br"C:\work\notes.txt");
    engine.mem_read(title_buf, &mut raw).ok();
    let title_len = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
    assert_eq!(&raw[..title_len], b"notes.txt");
    // nFileOffset = basename start (8 for `C:\work\`), nFileExtension =
    // the char after the dot (14).
    let mut off = [0_u8; 2];
    engine.mem_read(0x5000 + 100, &mut off).ok();
    assert_eq!(u16::from_le_bytes(off), 8);
    engine.mem_read(0x5000 + 102, &mut off).ok();
    assert_eq!(u16::from_le_bytes(off), 14);
    // No path state is recorded by the raw write-back (that is the
    // caller's job).
    assert!(state.window_state().last_file_dialog_path.is_none());
}

/// The extension offset for a dotless name points at the NUL terminator
/// (the position after the whole basename), per the OPENFILENAME docs.
#[test]
fn write_selected_path_dotless_name_extension_offset_is_end() {
    let mut engine = test_engine();
    let file_buf = 0x6000;

    write_selected_path(
        &mut engine,
        &SelectedPathWrite {
            ofn_ptr: 0x5000,
            file_buffer_ptr: file_buf,
            max_file: 260,
            file_title_ptr: 0,
            max_file_title: 0,
            path: r"C:\dir\name",
            unicode: true,
        },
    )
    .expect("the dotless write-back must succeed");

    assert_eq!(read_guest_utf16(&mut engine, file_buf, 64), r"C:\dir\name");
    // `C:\dir\` is 7 chars (offset 7); the basename `name` has no dot, so
    // nFileExtension = offset + basename length = 11 (the NUL position).
    let mut off = [0_u8; 2];
    engine.mem_read(0x5000 + 100, &mut off).ok();
    assert_eq!(u16::from_le_bytes(off), 7);
    engine.mem_read(0x5000 + 102, &mut off).ok();
    assert_eq!(u16::from_le_bytes(off), 11);
}

#[test]
fn interactive_policy_without_loop_machinery_falls_back_to_cancel() {
    let mut engine = test_engine();
    let mut state = test_state();
    let file_buf = 0x6000;
    engine.mem_write(file_buf, &utf16_bytes("notes.txt")).ok();
    write_ofn(&mut engine, 0x5000, file_buf, 260, 0);
    // file_dialog_loop_va / proc_va stay 0 (headless session default).

    let result = dispatch_open(&mut engine, &mut state, FileDialogPolicy::Interactive)
        .expect("fallback must succeed");
    assert_eq!(result.return_value, 0, "no host dialog → cancel");
    assert_eq!(state.window_state().comm_dlg_extended_error, 0);
    assert!(state.window_state().file_dialog.is_none());
}

#[test]
fn interactive_policy_builds_dialog_and_requests_modal_loop() {
    let mut engine = test_engine();
    let mut state = test_state();
    let loop_va = 0x7000_0040_B000;
    let proc_va = 0x7000_0040_B100;
    state.window_state().file_dialog_loop_va = loop_va;
    state.window_state().file_dialog_proc_va = proc_va;
    let file_buf = 0x6000;
    engine.mem_write(file_buf, &utf16_bytes("notes.txt")).ok();
    write_ofn(&mut engine, 0x5000, file_buf, 260, 0);

    let error = dispatch_open(&mut engine, &mut state, FileDialogPolicy::Interactive)
        .expect_err("interactive must request the modal loop");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("must be a control signal");
    let WinApiControlSignal::GuestCallbackRequested { request } = signal else {
        panic!("expected GuestCallbackRequested, got {signal:?}");
    };
    assert_eq!(request.callback_address, loop_va, "loop body VA");

    let dialog_hwnd = state
        .window_state()
        .file_dialog
        .as_ref()
        .expect("session recorded")
        .dialog_hwnd;
    assert_eq!(request.window_handle, dialog_hwnd);

    // The dialog + 4 controls exist; the dialog carries the proc stub.
    let windows = &state.window_state().windows;
    assert_eq!(windows.len(), 5, "dialog + EDIT + LISTBOX + OK + Cancel");
    let dialog = find_window(&mut state, dialog_hwnd).expect("dialog window");
    assert_eq!(
        dialog.dialog_proc, proc_va,
        "dialog proc = file-dialog stub"
    );
    assert_ne!(dialog.width, 0);
    // Modal: depth up, dialog active, edit focused.
    assert_eq!(state.lock_message_queue().dialog_depth, 1);
    assert_eq!(
        state.window_state().active_window_handle.as_u64(),
        dialog_hwnd
    );
    // The path EDIT is seeded with the lpstrFile basename.
    let edit_hwnd = state.window_state().file_dialog.as_ref().unwrap().edit_hwnd;
    assert_eq!(state.window_state().focus_window_handle.as_u64(), edit_hwnd);
    let edit = find_window(&mut state, edit_hwnd).expect("edit window");
    assert_eq!(edit.control_text, "notes.txt");
}

#[test]
fn end_dialog_writes_chosen_path_for_file_dialog() {
    let mut engine = test_engine();
    let mut state = test_state();
    // A bottle must be configured or the confinement would refuse the
    // accept: `C:\new-note.txt` has to land inside the guest C: volume.
    state.file_io.volumes = VolumeConfig {
        bottle_root: Some(PathBuf::from("/tmp/bottle")),
        drive_d_root: None,
    };
    let loop_va = 0x7000_0040_B000;
    let proc_va = 0x7000_0040_B100;
    state.window_state().file_dialog_loop_va = loop_va;
    state.window_state().file_dialog_proc_va = proc_va;
    state.window_state().dialog_result_va = 0x4000;
    engine.mem_write(0x4000, &0_u32.to_le_bytes()).ok();
    let file_buf = 0x6000;
    engine.mem_write(file_buf, &utf16_bytes("notes.txt")).ok();
    let def_ext = 0x6100;
    engine.mem_write(def_ext, &utf16_bytes("txt")).ok();
    write_ofn(&mut engine, 0x5000, file_buf, 260, def_ext);

    // Build the dialog exactly like the interactive handler does.
    dispatch_open(&mut engine, &mut state, FileDialogPolicy::Interactive)
        .expect_err("interactive must request the modal loop");
    let dialog_hwnd = state
        .window_state()
        .file_dialog
        .as_ref()
        .expect("session recorded")
        .dialog_hwnd;

    // The user typed a new file name (def-ext "txt" is appended).
    let edit_hwnd = state.window_state().file_dialog.as_ref().unwrap().edit_hwnd;
    if let Some(window) = find_window_mut(&mut state, edit_hwnd) {
        window.control_text = "new-note".to_owned();
    }

    // OK: EndDialog(1) → the path lands in the OPENFILENAME buffer.
    write_regs(&mut engine, dialog_hwnd, IDOK, 0, 0);
    let result = handle_end_dialog(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("EndDialog succeeds");
    assert_eq!(result.return_value, 1);
    assert_eq!(
        read_guest_utf16(&mut engine, file_buf, 64),
        r"C:\new-note.txt",
        "bare name joins the dialog directory (guest cwd = C:\\) and gains .txt"
    );
    assert_eq!(
        state.window_state().last_file_dialog_path.as_deref(),
        Some(r"C:\new-note.txt")
    );
    // The modal loop's result slot holds TRUE.
    let mut slot = [0_u8; 4];
    engine.mem_read(0x4000, &mut slot).ok();
    assert_eq!(u32::from_le_bytes(slot), 1);
    // The session is cleared and the dialog subtree torn down.
    assert!(state.window_state().file_dialog.is_none());
    assert!(
        !state
            .window_state()
            .windows
            .iter()
            .any(|w| w.handle == Hwnd::from(dialog_hwnd))
    );
}

#[test]
fn end_dialog_cancel_clears_session_without_writing() {
    let mut engine = test_engine();
    let mut state = test_state();
    state.window_state().file_dialog_loop_va = 0x7000_0040_B000;
    state.window_state().file_dialog_proc_va = 0x7000_0040_B100;
    state.window_state().dialog_result_va = 0x4000;
    engine.mem_write(0x4000, &0_u32.to_le_bytes()).ok();
    let file_buf = 0x6000;
    engine.mem_write(file_buf, &utf16_bytes("notes.txt")).ok();
    write_ofn(&mut engine, 0x5000, file_buf, 260, 0);

    dispatch_open(&mut engine, &mut state, FileDialogPolicy::Interactive)
        .expect_err("interactive must request the modal loop");
    let dialog_hwnd = state
        .window_state()
        .file_dialog
        .as_ref()
        .expect("session recorded")
        .dialog_hwnd;

    write_regs(&mut engine, dialog_hwnd, 0, 0, 0); // EndDialog(0) = cancel
    let result = handle_end_dialog(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("EndDialog succeeds");
    assert_eq!(result.return_value, 1);
    // The lpstrFile buffer keeps its original content.
    assert_eq!(read_guest_utf16(&mut engine, file_buf, 64), "notes.txt");
    let mut slot = [0_u8; 4];
    engine.mem_read(0x4000, &mut slot).ok();
    assert_eq!(u32::from_le_bytes(slot), 0, "cancel → FALSE");
    assert!(state.window_state().file_dialog.is_none());
}

/// `complete_file_dialog` is a no-op for dialogs it does not own.
#[test]
fn complete_file_dialog_ignores_unknown_dialog() {
    let mut engine = test_engine();
    let mut state = test_state();
    state.window_state().file_dialog = Some(session_with(r"C:\work", None));
    let result = complete_file_dialog(&mut engine, &mut state, 0x1234, 1).expect("no-op");
    assert_eq!(result, 1);
    assert!(
        state.window_state().file_dialog.is_some(),
        "session untouched"
    );
}

// ── Bottle confinement (directory + selection) ────────────────────────

/// A temporary bottle with a small drive_c layout for listing tests.
fn temp_bottle(tag: &str) -> (PathBuf, VolumeConfig) {
    let root = std::env::temp_dir().join(format!("wie-ofn-{tag}-{}", std::process::id()));
    let drive_c = root.join("drive_c");
    std::fs::create_dir_all(drive_c.join("App")).expect("create drive_c/App");
    std::fs::create_dir_all(drive_c.join("Windows")).expect("create drive_c/Windows");
    std::fs::write(drive_c.join("root.txt"), b"x").expect("write root file");
    std::fs::write(drive_c.join("App").join("app.txt"), b"x").expect("write app file");
    let volumes = VolumeConfig {
        bottle_root: Some(root.clone()),
        drive_d_root: None,
    };
    (root, volumes)
}

#[test]
fn resolve_initial_dir_prefers_confined_caller_dir() {
    let volumes = VolumeConfig {
        bottle_root: Some(PathBuf::from("/tmp/bottle")),
        drive_d_root: None,
    };
    // lpstrInitialDir confined to the bottle wins over everything.
    assert_eq!(
        resolve_initial_dir(&volumes, r"C:\App", r"C:\work", r"C:\other"),
        r"C:\work"
    );
    // Out-of-bottle lpstrInitialDir (unmapped D:, host path) falls
    // through to the lpstrFile directory.
    assert_eq!(
        resolve_initial_dir(&volumes, r"C:\App", r"D:\x", r"C:\work"),
        r"C:\work"
    );
    assert_eq!(
        resolve_initial_dir(&volumes, r"C:\App", "/Users/me/x", r"C:\work"),
        r"C:\work"
    );
    // ...then to the guest cwd when it is bottle-mapped.
    assert_eq!(resolve_initial_dir(&volumes, r"C:\App", "", ""), r"C:\App");
    // Nothing guest-visible → the bottle root.
    assert_eq!(
        resolve_initial_dir(&volumes, r"D:\cwd", r"D:\caller", r"D:\file"),
        r"C:\"
    );
    // No bottle configured → the fallback root (listing will be empty).
    assert_eq!(
        resolve_initial_dir(&VolumeConfig::default(), r"C:\App", "", ""),
        r"C:\"
    );
}

#[test]
fn list_directory_confines_to_bottle_and_blocks_ascent() {
    let (root, volumes) = temp_bottle("confine-list");
    let mut state = test_state();
    state.file_io.volumes = volumes;

    // An in-bottle directory lists its own entries, no parent link.
    let app = list_directory(&state, r"C:\App");
    assert!(app.contains(&"app.txt".to_owned()));
    assert!(!app.contains(&"..".to_owned()));

    // `..` ascends within the volume: C:\App\.. → the bottle root.
    let root_listing = list_directory(&state, r"C:\App\..");
    assert!(root_listing.contains(&"App".to_owned()));
    assert!(root_listing.contains(&"root.txt".to_owned()));

    // Ascent above the bottle root is blocked (empty listing).
    assert!(list_directory(&state, r"C:\App\..\..").is_empty());
    assert!(list_directory(&state, r"C:\..").is_empty());

    // Unmapped drives and host paths are not listable at all.
    assert!(list_directory(&state, r"E:\anything").is_empty());
    assert!(list_directory(&state, r"D:\x").is_empty());
    assert!(list_directory(&state, "/Users/me/x").is_empty());

    let _unused = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn list_directory_hides_symlink_escape_entries() {
    let (root, volumes) = temp_bottle("confine-symlink");
    let outside = root.join("outside-secret.txt");
    std::fs::write(&outside, b"secret").expect("write outside file");
    std::os::unix::fs::symlink(&outside, root.join("drive_c").join("leak.txt"))
        .expect("create escape symlink");
    let mut state = test_state();
    state.file_io.volumes = volumes;

    // The symlink's host path resolves outside the bottle, so it maps to
    // no guest path and must not appear in the listing.
    let listing = list_directory(&state, r"C:\");
    assert!(!listing.iter().any(|name| name == "leak.txt"));

    let _unused = std::fs::remove_dir_all(&root);
}

#[test]
fn end_dialog_rejects_out_of_bottle_path() {
    let mut engine = test_engine();
    let mut state = test_state();
    state.file_io.volumes = VolumeConfig {
        bottle_root: Some(PathBuf::from("/tmp/bottle")),
        drive_d_root: None,
    };
    state.window_state().file_dialog_loop_va = 0x7000_0040_B000;
    state.window_state().file_dialog_proc_va = 0x7000_0040_B100;
    state.window_state().dialog_result_va = 0x4000;
    engine.mem_write(0x4000, &0_u32.to_le_bytes()).ok();
    let file_buf = 0x6000;
    engine.mem_write(file_buf, &utf16_bytes("notes.txt")).ok();
    write_ofn(&mut engine, 0x5000, file_buf, 260, 0);

    // Each escape gets a fresh dialog (EndDialog tears the subtree down).
    // `/Users/me/x.txt` is deliberately absent: Windows path rules read a
    // leading `/` as rooted-on-current-drive, so it resolves to the
    // confined `C:\Users\me\x.txt` — not an escape.
    for escape in [
        r"C:\..\..\etc\passwd",
        r"E:\elsewhere.txt",
        r"..\..\..\etc\passwd",
        r"\\server\share\x",
    ] {
        dispatch_open(&mut engine, &mut state, FileDialogPolicy::Interactive)
            .expect_err("interactive must request the modal loop");
        let dialog_hwnd = state
            .window_state()
            .file_dialog
            .as_ref()
            .expect("session recorded")
            .dialog_hwnd;
        let edit_hwnd = state.window_state().file_dialog.as_ref().unwrap().edit_hwnd;
        if let Some(window) = find_window_mut(&mut state, edit_hwnd) {
            window.control_text = escape.to_owned();
        }
        write_regs(&mut engine, dialog_hwnd, IDOK, 0, 0);
        let result = handle_end_dialog(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("EndDialog succeeds");
        // Refused like a cancel: the modal-loop result slot holds FALSE,
        // the buffer is untouched, and the session is cleared so a later
        // dialog can open.
        assert_eq!(result.return_value, 1, "EndDialog itself succeeds");
        assert_eq!(
            read_guest_u32_at(&mut engine, 0x4000),
            0,
            "escape {escape} must be refused (FALSE result)"
        );
        assert_eq!(read_guest_utf16(&mut engine, file_buf, 64), "notes.txt");
        assert!(state.window_state().file_dialog.is_none());
        assert!(state.window_state().last_file_dialog_path.is_none());
    }
}

#[test]
fn end_dialog_collapses_dotdot_within_bottle() {
    let mut engine = test_engine();
    let mut state = test_state();
    state.file_io.volumes = VolumeConfig {
        bottle_root: Some(PathBuf::from("/tmp/bottle")),
        drive_d_root: None,
    };
    state.window_state().file_dialog_loop_va = 0x7000_0040_B000;
    state.window_state().file_dialog_proc_va = 0x7000_0040_B100;
    state.window_state().dialog_result_va = 0x4000;
    engine.mem_write(0x4000, &0_u32.to_le_bytes()).ok();
    let file_buf = 0x6000;
    engine.mem_write(file_buf, &utf16_bytes("notes.txt")).ok();
    write_ofn(&mut engine, 0x5000, file_buf, 260, 0);

    dispatch_open(&mut engine, &mut state, FileDialogPolicy::Interactive)
        .expect_err("interactive must request the modal loop");
    let dialog_hwnd = state
        .window_state()
        .file_dialog
        .as_ref()
        .expect("session recorded")
        .dialog_hwnd;
    let edit_hwnd = state.window_state().file_dialog.as_ref().unwrap().edit_hwnd;

    // `..` inside the volume is collapsed to the canonical guest path.
    if let Some(window) = find_window_mut(&mut state, edit_hwnd) {
        window.control_text = r"C:\App\..\readme.txt".to_owned();
    }
    write_regs(&mut engine, dialog_hwnd, IDOK, 0, 0);
    let result = handle_end_dialog(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("EndDialog succeeds");
    assert_eq!(result.return_value, 1);
    assert_eq!(
        read_guest_utf16(&mut engine, file_buf, 64),
        r"C:\readme.txt",
        "within-bottle `..` collapses and the canonical path is written back"
    );
    assert!(state.window_state().file_dialog.is_none());
}

// ── Native file-dialog bridge (macOS panels via rfd) ──────────────────

/// Drive the W handler with a scripted native bridge (Interactive policy).
///
/// The real flow is two entries around the bridge: the handler's first
/// entry builds the request and returns `FileDialogBridgeRequested`; the
/// runtime runs the bridge WITHOUT the shared lock and records the pick;
/// the engine's re-execution of the fake API re-enters the handler, which
/// writes the pick back. This helper simulates exactly that (the runtime
/// is not involved in unit tests).
fn dispatch_open_with_bridge(
    engine: &mut IcedCpu,
    state: &mut WinApiState,
    bridge: FileDialogBridge,
) -> anyhow::Result<WinApiHandlerResult> {
    state.window_state().file_dialog_policy = FileDialogPolicy::Interactive;
    state.window_state().file_dialog_bridge = Some(bridge);
    write_regs(engine, 0x5000, 0, 0, 0);
    let first =
        handle_get_open_file_name_w(&mut HandlerContext::new(engine, test_environment(), state))
            .expect_err("the first entry parks the guest for the native panel");
    let signal = first
        .downcast_ref::<WinApiControlSignal>()
        .expect("a control signal");
    let WinApiControlSignal::FileDialogBridgeRequested { request } = signal else {
        panic!("expected a file-dialog bridge request");
    };
    // What the runtime does between the two entries: take the bridge out,
    // run it (no shared lock), restore it, record the pick.
    let bridge = state
        .window_state()
        .file_dialog_bridge
        .take()
        .expect("bridge registered");
    let picked = bridge(request);
    state.window_state().file_dialog_bridge = Some(bridge);
    state
        .window_state()
        .pending_native_file_dialog
        .as_mut()
        .expect("pending session recorded")
        .pick = picked;
    // Re-entry: the handler writes the pick back.
    handle_get_open_file_name_w(&mut HandlerContext::new(engine, test_environment(), state))
}

/// A scripted bridge standing in for the native panel: the pick is a host
/// path inside the bottle, so the write-back must succeed and return TRUE.
#[test]
fn bridge_pick_inside_bottle_writes_guest_path_and_returns_true() {
    let mut engine = test_engine();
    let mut state = test_state();
    state.file_io.volumes = VolumeConfig {
        bottle_root: Some(PathBuf::from("/tmp/bottle")),
        drive_d_root: None,
    };
    let file_buf = 0x6000;
    engine.mem_write(file_buf, &utf16_bytes("notes.txt")).ok();
    write_ofn(&mut engine, 0x5000, file_buf, 260, 0);

    let bridge: FileDialogBridge = Box::new(|request| {
        // The native panel starts at the BOTTLE ROOT mapped into the
        // bottle (`{root}/drive_c`), not the guest cwd.
        assert_eq!(
            request.initial_host_dir.as_deref(),
            Some(std::path::Path::new("/tmp/bottle/drive_c")),
            "initial dir = the bottle root mapped into the bottle"
        );
        assert_eq!(request.default_file_name.as_deref(), Some("notes.txt"));
        assert!(!request.is_save, "GetOpenFileName is an open panel");
        Some(FileDialogPick {
            host_path: PathBuf::from("/tmp/bottle/drive_c/App/notes.txt"),
        })
    });

    let result = dispatch_open_with_bridge(&mut engine, &mut state, bridge)
        .expect("bridge accept must succeed");
    assert_eq!(result.return_value, 1, "an in-bottle pick → TRUE");
    assert_eq!(
        read_guest_utf16(&mut engine, file_buf, 64),
        r"C:\App\notes.txt",
        "the host pick maps back to the guest path in lpstrFile"
    );
    assert_eq!(
        state.window_state().last_file_dialog_path.as_deref(),
        Some(r"C:\App\notes.txt")
    );
    assert!(
        state.window_state().file_dialog.is_none(),
        "the bridge path builds no in-app dialog session"
    );
}

/// The picked host path lands OUTSIDE both guest volumes (the user
/// browsed away via the panel's sidebar): the native dialog IS the
/// user's explicit grant, so the accept registers a pick-mount and
/// returns TRUE with the mounted guest path (`Z:\pick{N}\{name}`) in
/// `lpstrFile` — the guest can now open/save the REAL host file in place.
#[test]
fn bridge_pick_outside_bottle_registers_mount_and_returns_true() {
    let _serial = crate::vfs::pick_mount::TEST_SERIAL
        .lock()
        .expect("pick-mount test lock poisoned");
    crate::vfs::pick_mount::clear_pick_mounts();
    let mut engine = test_engine();
    let mut state = test_state();
    state.file_io.volumes = VolumeConfig {
        bottle_root: Some(PathBuf::from("/tmp/bottle")),
        drive_d_root: None,
    };
    let file_buf = 0x6000;
    engine.mem_write(file_buf, &utf16_bytes("notes.txt")).ok();
    write_ofn(&mut engine, 0x5000, file_buf, 260, 0);

    let bridge: FileDialogBridge = Box::new(|_| {
        Some(FileDialogPick {
            host_path: PathBuf::from("/etc/passwd"),
        })
    });
    let result = dispatch_open_with_bridge(&mut engine, &mut state, bridge)
        .expect("bridge accept must succeed");
    assert_eq!(result.return_value, 1, "an out-of-bottle pick → TRUE");
    let guest_path = read_guest_utf16(&mut engine, file_buf, 64);
    assert!(
        guest_path.starts_with(r"Z:\pick"),
        "the pick registers a mounted guest path, got: {guest_path}"
    );
    assert!(guest_path.ends_with("\\passwd"));
    // The mount binds the guest path to the EXACT picked host file.
    assert_eq!(
        crate::vfs::guest_path_to_host(&state.file_io.volumes, &guest_path).map(|map| map.host),
        Some(PathBuf::from("/etc/passwd")),
        "the guest path maps to the real picked host file"
    );
    assert_eq!(
        state.window_state().last_file_dialog_path.as_deref(),
        Some(guest_path.as_str())
    );
    assert!(
        state.window_state().file_dialog.is_none(),
        "the bridge path builds no in-app dialog session"
    );
}

/// An Open pick whose host file does not exist is genuinely invalid (the
/// open panel never offers nonexistent files — a missing target is a
/// raced/deleted pick): it must stay a refusal — FALSE, `lpstrFile`
/// untouched — with `FNERR_INVALIDFILENAME` surfaced via
/// `CommDlgExtendedError` so the guest does not mistake it for a plain
/// cancel.
#[test]
fn bridge_open_pick_of_missing_file_stays_fnerr() {
    let _serial = crate::vfs::pick_mount::TEST_SERIAL
        .lock()
        .expect("pick-mount test lock poisoned");
    crate::vfs::pick_mount::clear_pick_mounts();
    let mut engine = test_engine();
    let mut state = test_state();
    state.file_io.volumes = VolumeConfig {
        bottle_root: Some(PathBuf::from("/tmp/bottle")),
        drive_d_root: None,
    };
    let file_buf = 0x6000;
    engine.mem_write(file_buf, &utf16_bytes("notes.txt")).ok();
    write_ofn(&mut engine, 0x5000, file_buf, 260, 0);

    let missing =
        std::env::temp_dir().join(format!("wie-fnerr-missing-{}.txt", std::process::id()));
    let _unused = std::fs::remove_file(&missing);
    let bridge: FileDialogBridge = Box::new(move |_| {
        Some(FileDialogPick {
            host_path: missing.clone(),
        })
    });
    let result = dispatch_open_with_bridge(&mut engine, &mut state, bridge)
        .expect("bridge accept must succeed");
    assert_eq!(result.return_value, 0, "a missing Open target → FALSE");
    assert_eq!(
        read_guest_utf16(&mut engine, file_buf, 64),
        "notes.txt",
        "lpstrFile stays untouched"
    );
    assert!(state.window_state().last_file_dialog_path.is_none());
    assert_eq!(
        state.window_state().comm_dlg_extended_error,
        FNERR_INVALIDFILENAME,
        "an invalid pick must surface FNERR_INVALIDFILENAME, not a \
             silent cancel"
    );
    // The invalid pick must NOT leave a mount behind.
    assert_eq!(
        crate::vfs::pick_mount::resolve_pick_mount(r"Z:\pick1\notes.txt"),
        None,
        "no mount may exist for the refused pick"
    );
}

/// A SAVE pick of a host file that does not exist yet (the user typed a
/// new name in the save panel) is the create-case, not an error: the
/// accept registers a pick-mount and returns TRUE so the guest can create
/// the file where the user picked.
#[test]
fn bridge_save_pick_of_new_file_registers_mount_and_returns_true() {
    let _serial = crate::vfs::pick_mount::TEST_SERIAL
        .lock()
        .expect("pick-mount test lock poisoned");
    crate::vfs::pick_mount::clear_pick_mounts();
    let mut engine = test_engine();
    let mut state = test_state();
    state.file_io.volumes = VolumeConfig {
        bottle_root: Some(PathBuf::from("/tmp/bottle")),
        drive_d_root: None,
    };
    let file_buf = 0x6000;
    engine.mem_write(file_buf, &utf16_bytes("fresh.txt")).ok();
    write_ofn(&mut engine, 0x5000, file_buf, 260, 0);

    let picked_host =
        std::env::temp_dir().join(format!("wie-save-create-{}.txt", std::process::id()));
    let _unused = std::fs::remove_file(&picked_host);
    let bridge_host = picked_host.clone();
    let bridge: FileDialogBridge = Box::new(move |request| {
        assert!(request.is_save, "GetSaveFileName is a save panel");
        Some(FileDialogPick {
            host_path: bridge_host.clone(),
        })
    });
    state.window_state().file_dialog_policy = FileDialogPolicy::Interactive;
    state.window_state().file_dialog_bridge = Some(bridge);
    write_regs(&mut engine, 0x5000, 0, 0, 0);
    let first = handle_get_save_file_name_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect_err("the first entry parks the guest for the native panel");
    let signal = first
        .downcast_ref::<WinApiControlSignal>()
        .expect("a control signal");
    let WinApiControlSignal::FileDialogBridgeRequested { request } = signal else {
        panic!("expected a file-dialog bridge request");
    };
    let bridge = state
        .window_state()
        .file_dialog_bridge
        .take()
        .expect("bridge registered");
    let picked = bridge(request);
    state.window_state().file_dialog_bridge = Some(bridge);
    state
        .window_state()
        .pending_native_file_dialog
        .as_mut()
        .expect("pending session recorded")
        .pick = picked;
    let result = handle_get_save_file_name_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("bridge save must succeed");
    assert_eq!(result.return_value, 1, "a save pick of a new file → TRUE");
    let guest_path = read_guest_utf16(&mut engine, file_buf, 64);
    assert!(
        guest_path.starts_with(r"Z:\pick"),
        "the save pick registers a mounted guest path, got: {guest_path}"
    );
    assert_eq!(
        crate::vfs::guest_path_to_host(&state.file_io.volumes, &guest_path).map(|map| map.host),
        Some(picked_host.clone()),
        "the guest path maps to the picked (not-yet-existing) host file"
    );
}

// ── E2E through the file handlers ───────────────────────────────────────
//
// The dialog accept produces the guest path; the GUEST then reopens it
// with CreateFileW/WriteFile/ReadFile/CloseHandle. These drive the real
// handlers (the same entry points notepad hits) against a temp bottle.

/// Save-create end to end: the guest's `CreateFileW(CREATE_ALWAYS)` on a
/// mounted guest path must CREATE the real host file at the picked
/// location, and the bytes written through `WriteFile` must land on it.
#[test]
fn mounted_save_create_writes_the_real_host_file() {
    let _serial = crate::vfs::pick_mount::TEST_SERIAL
        .lock()
        .expect("pick-mount test lock poisoned");
    crate::vfs::pick_mount::clear_pick_mounts();
    let mut engine = test_engine();
    let mut state = test_state();
    // The app runs in a bottle (the standing "filesystem ⇒ bottle"
    // policy); the picked file itself lives OUTSIDE it, via the mount.
    let bottle = std::env::temp_dir().join(format!("wie-pickmount-bottle-{}", std::process::id()));
    state.file_io.bottle_root = Some(bottle.clone());
    state.file_io.volumes.bottle_root = Some(bottle);
    let host = std::env::temp_dir().join(format!("wie-pickmount-saved-{}.txt", std::process::id()));
    let _unused = std::fs::remove_file(&host);

    // The dialog-accept step: the pick registers the mount.
    let guest_path = crate::vfs::register_pick_mount(&host).expect("the pick mounts the new file");
    let file_name_ptr = 0x6000;
    engine
        .mem_write(file_name_ptr, &utf16_bytes(&guest_path))
        .ok();
    // CreateFileW(file, GENERIC_WRITE, 0, 0, CREATE_ALWAYS, ...).
    write_regs(&mut engine, file_name_ptr, 0x4000_0000, 0, 0);
    engine
        .mem_write(STACK_TOP + 0x28, &2_u32.to_le_bytes())
        .ok(); // CREATE_ALWAYS
    let created = handle_create_file_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("CreateFileW on the mount must succeed");
    let handle = created.return_value;
    assert_ne!(
        handle,
        u64::MAX,
        "a valid handle (not INVALID_HANDLE_VALUE)"
    );

    // WriteFile(handle, "saved through the mount", ...).
    let payload = b"saved through the pick mount";
    let data_ptr = 0x7000;
    engine.mem_write(data_ptr, payload).ok();
    write_regs(
        &mut engine,
        handle,
        data_ptr,
        u64::try_from(payload.len()).unwrap_or(0),
        0x8000,
    );
    handle_write_file(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("WriteFile on the mount must succeed");

    // CloseHandle — the guest's Save flow ends here.
    write_regs(&mut engine, handle, 0, 0, 0);
    handle_close_handle(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("CloseHandle must succeed");

    let on_disk = std::fs::read(&host).expect("the real host file must exist");
    assert_eq!(
        on_disk, payload,
        "the bytes land on the REAL host file at the picked location"
    );
    let _unused = std::fs::remove_file(&host);
}

/// Open end to end: a host file read through the mount — the guest's
/// `CreateFileW(OPEN_EXISTING)` + `ReadFile` on the mounted guest path
/// must return the real file's bytes.
#[test]
fn mounted_open_reads_the_real_host_file() {
    let _serial = crate::vfs::pick_mount::TEST_SERIAL
        .lock()
        .expect("pick-mount test lock poisoned");
    crate::vfs::pick_mount::clear_pick_mounts();
    let mut engine = test_engine();
    let mut state = test_state();
    let bottle = std::env::temp_dir().join(format!("wie-pickmount-bottle-{}", std::process::id()));
    state.file_io.bottle_root = Some(bottle.clone());
    state.file_io.volumes.bottle_root = Some(bottle);
    let host = std::env::temp_dir().join(format!("wie-pickmount-open-{}.txt", std::process::id()));
    let original = b"hello from the host file";
    std::fs::write(&host, original).expect("seed the host file");

    let guest_path = crate::vfs::register_pick_mount(&host).expect("the pick mounts the open file");
    let file_name_ptr = 0x6000;
    engine
        .mem_write(file_name_ptr, &utf16_bytes(&guest_path))
        .ok();
    // CreateFileW(file, GENERIC_READ, 0, 0, OPEN_EXISTING, ...).
    write_regs(&mut engine, file_name_ptr, 0x8000_0000, 0, 0);
    engine
        .mem_write(STACK_TOP + 0x28, &3_u32.to_le_bytes())
        .ok(); // OPEN_EXISTING
    let opened = handle_create_file_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("CreateFileW on the mount must succeed");
    let handle = opened.return_value;
    assert_ne!(
        handle,
        u64::MAX,
        "a valid handle (not INVALID_HANDLE_VALUE)"
    );

    // ReadFile(handle, buf, 64, &bytesRead).
    let read_ptr = 0x7000;
    write_regs(&mut engine, handle, read_ptr, 64, 0x8000);
    handle_read_file(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("ReadFile on the mount must succeed");
    let mut read_back = vec![0_u8; original.len()];
    engine
        .mem_read(read_ptr, &mut read_back)
        .expect("read the guest buffer back");
    assert_eq!(
        read_back, original,
        "the guest reads the REAL host file through the mount"
    );

    write_regs(&mut engine, handle, 0, 0, 0);
    handle_close_handle(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("CloseHandle must succeed");
    let _unused = std::fs::remove_file(&host);
}

/// THE full post-accept chain in one test: the scripted bridge picks an
/// out-of-bottle host file → the dialog re-entry writes the mounted guest
/// path (`Z:\pick{N}\{name}`) into `lpstrFile` → the guest re-opens THAT
/// BUFFER PATH with `CreateFileW(OPEN_EXISTING)` → `ReadFile` returns the
/// real host file's bytes. This is the hand-off the existing tests each
/// cover in isolation: the bridge tests stop at the buffer + mount, and
/// `mounted_open_reads_the_real_host_file` mounts directly, skipping the
/// dialog and the buffer round-trip.
#[test]
fn dialog_pick_to_open_reads_the_real_host_file() {
    let _serial = crate::vfs::pick_mount::TEST_SERIAL
        .lock()
        .expect("pick-mount test lock poisoned");
    crate::vfs::pick_mount::clear_pick_mounts();
    let mut engine = test_engine();
    let mut state = test_state();
    let bottle =
        std::env::temp_dir().join(format!("wie-dialog-chain-bottle-{}", std::process::id()));
    state.file_io.bottle_root = Some(bottle.clone());
    state.file_io.volumes = VolumeConfig {
        bottle_root: Some(bottle),
        drive_d_root: None,
    };
    // The user picked a host file OUTSIDE the bottle via the native panel.
    let host = std::env::temp_dir().join(format!("wie-dialog-chain-{}.txt", std::process::id()));
    let original = b"dialog pick -> real host file bytes";
    std::fs::write(&host, original).expect("seed the picked host file");

    // Bridge entry → re-entry: the pick registers the mount and the guest
    // buffer (`lpstrFile`) receives the mounted guest path.
    let file_buf = 0x6000;
    engine.mem_write(file_buf, &utf16_bytes("notes.txt")).ok();
    write_ofn(&mut engine, 0x5000, file_buf, 260, 0);
    let bridge_host = host.clone();
    let bridge: FileDialogBridge = Box::new(move |_| {
        Some(FileDialogPick {
            host_path: bridge_host.clone(),
        })
    });
    let result = dispatch_open_with_bridge(&mut engine, &mut state, bridge)
        .expect("the dialog accept must succeed");
    assert_eq!(result.return_value, 1, "an out-of-bottle pick → TRUE");
    let guest_path = read_guest_utf16(&mut engine, file_buf, 64);
    assert!(
        guest_path.starts_with(r"Z:\pick"),
        "lpstrFile must hold the mounted guest path, got: {guest_path}"
    );
    assert_eq!(
        crate::vfs::guest_path_to_host(&state.file_io.volumes, &guest_path).map(|map| map.host),
        Some(host.clone()),
        "the mounted path maps to the exact picked host file"
    );

    // The guest re-opens THE BUFFER PATH with CreateFileW(OPEN_EXISTING)
    // and ReadFile — the same handlers notepad hits after
    // GetOpenFileNameW. The buffer is re-seeded from itself to make the
    // round-trip explicit: the file op consumes the dialog's output.
    let file_name_ptr = file_buf;
    engine
        .mem_write(file_name_ptr, &utf16_bytes(&guest_path))
        .ok();
    write_regs(&mut engine, file_name_ptr, 0x8000_0000, 0, 0); // GENERIC_READ
    engine
        .mem_write(STACK_TOP + 0x28, &3_u32.to_le_bytes())
        .ok(); // OPEN_EXISTING
    let opened = handle_create_file_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("CreateFileW on the returned buffer path must succeed");
    let handle = opened.return_value;
    assert_ne!(
        handle,
        u64::MAX,
        "a valid handle (not INVALID_HANDLE_VALUE)"
    );

    let read_ptr = 0x7000;
    write_regs(&mut engine, handle, read_ptr, 64, 0x8000);
    handle_read_file(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("ReadFile on the returned buffer path must succeed");
    let mut read_back = vec![0_u8; original.len()];
    engine
        .mem_read(read_ptr, &mut read_back)
        .expect("read the guest buffer back");
    assert_eq!(
        read_back, original,
        "the guest reads the REAL picked host file through the dialog-returned path"
    );

    write_regs(&mut engine, handle, 0, 0, 0);
    handle_close_handle(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("CloseHandle must succeed");
    let _unused = std::fs::remove_file(&host);
}

/// The bridge returning `None` is the user pressing Cancel in the native
/// panel: FALSE, no write-back.
#[test]
fn bridge_cancel_returns_false_without_touching_buffer() {
    let mut engine = test_engine();
    let mut state = test_state();
    let file_buf = 0x6000;
    engine.mem_write(file_buf, &utf16_bytes("notes.txt")).ok();
    write_ofn(&mut engine, 0x5000, file_buf, 260, 0);

    let bridge: FileDialogBridge = Box::new(|_| None);
    let result = dispatch_open_with_bridge(&mut engine, &mut state, bridge)
        .expect("bridge cancel must succeed");
    assert_eq!(result.return_value, 0, "cancel → FALSE");
    assert_eq!(read_guest_utf16(&mut engine, file_buf, 64), "notes.txt");
    assert!(state.window_state().last_file_dialog_path.is_none());
}

/// The request must carry the save flag and the best-effort filter parse:
/// the simple "*.txt" group survives, the "*.*" catch-all is dropped.
#[test]
fn bridge_save_receives_save_flag_and_parsed_filter() {
    let mut engine = test_engine();
    let mut state = test_state();
    state.file_io.volumes = VolumeConfig {
        bottle_root: Some(PathBuf::from("/tmp/bottle")),
        drive_d_root: None,
    };
    let file_buf = 0x6000;
    engine.mem_write(file_buf, &utf16_bytes("report.txt")).ok();
    write_ofn(&mut engine, 0x5000, file_buf, 260, 0);
    // lpstrFilter: "Text Documents\0*.txt\0All Files\0*.*\0\0".
    let filter_buf = 0x6200;
    engine
        .mem_write(
            filter_buf,
            &utf16_bytes("Text Documents\0*.txt\0All Files\0*.*\0\0"),
        )
        .ok();
    engine
        .mem_write(0x5000 + 24, &filter_buf.to_le_bytes())
        .ok();

    let bridge: FileDialogBridge = Box::new(|request| {
        assert!(request.is_save, "GetSaveFileName is a save panel");
        assert_eq!(request.default_file_name.as_deref(), Some("report.txt"));
        assert_eq!(request.filters.len(), 1, "the *.* catch-all is dropped");
        assert_eq!(request.filters[0].name, "Text Documents");
        assert_eq!(request.filters[0].patterns, vec!["*.txt".to_owned()]);
        Some(FileDialogPick {
            host_path: PathBuf::from("/tmp/bottle/drive_c/report.txt"),
        })
    });

    state.window_state().file_dialog_policy = FileDialogPolicy::Interactive;
    state.window_state().file_dialog_bridge = Some(bridge);
    write_regs(&mut engine, 0x5000, 0, 0, 0);
    // Entry 1: build the request; entry 2 (after the bridge ran) writes
    // the pick back — the same two-entry flow `dispatch_open_with_bridge`
    // drives, here for the Save handler.
    let first = handle_get_save_file_name_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect_err("the first entry parks the guest for the native panel");
    let signal = first
        .downcast_ref::<WinApiControlSignal>()
        .expect("a control signal");
    let WinApiControlSignal::FileDialogBridgeRequested { request } = signal else {
        panic!("expected a file-dialog bridge request");
    };
    let bridge = state
        .window_state()
        .file_dialog_bridge
        .take()
        .expect("bridge registered");
    let picked = bridge(request);
    state.window_state().file_dialog_bridge = Some(bridge);
    state
        .window_state()
        .pending_native_file_dialog
        .as_mut()
        .expect("pending session recorded")
        .pick = picked;
    let result = handle_get_save_file_name_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("bridge save must succeed");
    assert_eq!(result.return_value, 1);
    assert_eq!(
        read_guest_utf16(&mut engine, file_buf, 64),
        r"C:\report.txt",
        "the save pick maps back into the bottle"
    );
}

/// A drive-D bridge pick maps to a `D:\…` guest path (the D: volume is
/// guest-visible when the bridge root is configured).
#[test]
fn bridge_pick_in_drive_d_maps_to_guest_d_path() {
    let mut engine = test_engine();
    let mut state = test_state();
    state.file_io.volumes = VolumeConfig {
        bottle_root: Some(PathBuf::from("/tmp/bottle")),
        drive_d_root: Some(PathBuf::from("/Users/me/data")),
    };
    let file_buf = 0x6000;
    engine.mem_write(file_buf, &utf16_bytes("a.7z")).ok();
    write_ofn(&mut engine, 0x5000, file_buf, 260, 0);

    let bridge: FileDialogBridge = Box::new(|_| {
        Some(FileDialogPick {
            host_path: PathBuf::from("/Users/me/data/archive/a.7z"),
        })
    });
    let result = dispatch_open_with_bridge(&mut engine, &mut state, bridge)
        .expect("bridge accept must succeed");
    assert_eq!(result.return_value, 1);
    assert_eq!(
        read_guest_utf16(&mut engine, file_buf, 64),
        r"D:\archive\a.7z"
    );
}

/// The native panel opens at the BOTTLE ROOT even when the guest cwd is
/// `C:\App` (the process identity hardcodes it) — the user asked for the
/// bottle root, not the guest's working directory.
#[test]
fn bridge_initial_dir_is_the_bottle_root_not_the_guest_cwd() {
    let mut engine = test_engine();
    let mut state = test_state();
    state.file_io.volumes = VolumeConfig {
        bottle_root: Some(PathBuf::from("/tmp/bottle")),
        drive_d_root: None,
    };
    // The real session seeds the guest cwd to C:\App; the in-app dialog
    // would resolve to {root}/drive_c/App, but the bridge must NOT.
    state.file_io.current_directory_wide = "C:\\App\0".encode_utf16().collect();
    let file_buf = 0x6000;
    engine.mem_write(file_buf, &utf16_bytes("notes.txt")).ok();
    write_ofn(&mut engine, 0x5000, file_buf, 260, 0);

    let bridge: FileDialogBridge = Box::new(|request| {
        assert_eq!(
            request.initial_host_dir.as_deref(),
            Some(std::path::Path::new("/tmp/bottle/drive_c")),
            "initial dir = the bottle root, not C:\\App's directory"
        );
        assert_ne!(
            request.initial_host_dir.as_deref(),
            Some(std::path::Path::new("/tmp/bottle/drive_c/App")),
            "the guest cwd must not leak into the native panel"
        );
        Some(FileDialogPick {
            host_path: PathBuf::from("/tmp/bottle/drive_c/notes.txt"),
        })
    });

    let result = dispatch_open_with_bridge(&mut engine, &mut state, bridge)
        .expect("bridge accept must succeed");
    assert_eq!(result.return_value, 1);
    assert_eq!(
        read_guest_utf16(&mut engine, file_buf, 64),
        r"C:\notes.txt",
        "a pick at the bottle root maps back to C:\\"
    );
}

/// `parse_ofn_filter` keeps only simple `*.ext` glob groups; complex or
/// catch-all patterns drop the group (a wrong native filter grays out
/// every file on macOS, which is worse than showing all files).
#[test]
fn parse_ofn_filter_keeps_simple_globs_only() {
    // A `*.*` catch-all pair is dropped; the simple pair survives.
    let filters = parse_ofn_filter(&[
        "Text Documents".to_owned(),
        "*.txt".to_owned(),
        "All Files".to_owned(),
        "*.*".to_owned(),
    ]);
    assert_eq!(filters.len(), 1, "All Files (*.*) is dropped");
    assert_eq!(filters[0].name, "Text Documents");
    assert_eq!(filters[0].patterns, vec!["*.txt".to_owned()]);

    // Semicolon-separated simple globs survive as one filter group.
    let filters = parse_ofn_filter(&["Code".to_owned(), "*.rs;*.toml".to_owned()]);
    assert_eq!(filters.len(), 1);
    assert_eq!(
        filters[0].patterns,
        vec!["*.rs".to_owned(), "*.toml".to_owned()]
    );

    // Complex patterns drop the whole group (nothing to show → no filter).
    assert!(parse_ofn_filter(&["Any".to_owned(), "*".to_owned()]).is_empty());
    assert!(parse_ofn_filter(&["All".to_owned(), "*.*".to_owned()]).is_empty());
    assert!(parse_ofn_filter(&["Multi".to_owned(), "*.tar.gz".to_owned()]).is_empty());
    assert!(parse_ofn_filter(&["Bare".to_owned(), "readme.txt".to_owned()]).is_empty());
    assert!(parse_ofn_filter(&["Empty".to_owned(), String::new()]).is_empty());
    // No filter at all → empty.
    assert!(parse_ofn_filter(&[]).is_empty());
}

#[test]
fn simple_filter_glob_rejects_catchalls_and_complex_patterns() {
    assert!(is_simple_filter_glob("*.txt"));
    assert!(is_simple_filter_glob("*.TXT"));
    assert!(!is_simple_filter_glob("*.*"));
    assert!(!is_simple_filter_glob("*"));
    assert!(!is_simple_filter_glob("*.tar.gz"));
    assert!(!is_simple_filter_glob("*.doc;*.txt"));
    assert!(!is_simple_filter_glob("readme.txt"));
    assert!(!is_simple_filter_glob(""));
}
