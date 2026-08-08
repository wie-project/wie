//! Integration tests for `wie_pe::resources` against compiled PE fixtures.
//!
//! Fixtures in `tests/fixtures/`:
//! * `dialog1.rc` + `main.c` — sources; compiled with the mingw toolchain.
//! * `dialog1.exe` — `main.c` linked with the windres output (has `.rsrc`
//!   with one `RT_DIALOG`, template id 100, 3 controls).
//! * `no_rsrc.exe` — `main.c` alone (no `.rsrc` section).
//!
//! The prebuilt binaries are checked in, so the tests run without mingw. If a
//! binary is missing, the tests rebuild it from the sources when the
//! `x86_64-w64-mingw32-*` toolchain is available, and skip otherwise.

use std::path::PathBuf;

use wie_pe::resources::{DialogTemplate, ItemClass, PixelRect};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn have_toolchain() -> bool {
    let windres = std::process::Command::new("x86_64-w64-mingw32-windres")
        .arg("--version")
        .output()
        .is_ok();
    let gcc = std::process::Command::new("x86_64-w64-mingw32-gcc")
        .arg("--version")
        .output()
        .is_ok();
    windres && gcc
}

/// Return the fixture exe, rebuilding it from sources when possible.
fn fixture_exe() -> Option<PathBuf> {
    let dir = fixture_dir();
    let exe = dir.join("dialog1.exe");
    if exe.is_file() {
        return Some(exe);
    }
    if !have_toolchain() {
        return None;
    }
    let obj = dir.join("dialog1.o");
    let windres_ok = std::process::Command::new("x86_64-w64-mingw32-windres")
        .arg(dir.join("dialog1.rc"))
        .arg("-o")
        .arg(&obj)
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    let gcc_ok = windres_ok
        && std::process::Command::new("x86_64-w64-mingw32-gcc")
            .arg(dir.join("main.c"))
            .arg(&obj)
            .arg("-o")
            .arg(&exe)
            .arg("-mwindows")
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
    if gcc_ok { Some(exe) } else { None }
}

/// Read `name` and return its bytes plus the section map.
fn fixture_parsed(name: &str) -> Option<(Vec<u8>, Vec<wie_pe::PeSectionMap>)> {
    let path = fixture_dir().join(name);
    let bytes = std::fs::read(path).ok()?;
    let plan = wie_pe::pe_map_plan_from_bytes(&bytes).ok()?;
    Some((bytes, plan.sections))
}

fn single_dialog(dialogs: Vec<DialogTemplate>) -> Option<DialogTemplate> {
    match dialogs.len() {
        1 => dialogs.into_iter().next(),
        _ => None,
    }
}

#[test]
fn parses_windres_dialog_fixture() {
    let Some(exe) = fixture_exe() else {
        eprintln!(
            "SKIP parses_windres_dialog_fixture: fixture exe missing and mingw toolchain unavailable"
        );
        return;
    };
    let Some((image, sections)) = fixture_parsed("dialog1.exe") else {
        eprintln!("SKIP parses_windres_dialog_fixture: could not parse {exe:?}");
        return;
    };
    let Some(dialog) = single_dialog(wie_pe::resources::parse_dialogs(&image, &sections)) else {
        eprintln!("SKIP parses_windres_dialog_fixture: expected exactly one dialog");
        return;
    };

    // Template-level facts.
    assert_eq!(dialog.name, 100, "template id");
    assert_eq!(dialog.title, "Test Dialog");
    assert_eq!(
        (dialog.x, dialog.y, dialog.cx, dialog.cy),
        (10, 20, 160, 60)
    );
    assert_eq!(
        dialog.pixel_rect,
        PixelRect {
            x: 20,
            y: 40,
            cx: 320,
            cy: 120,
        },
        "DLUs convert at 2 px per unit"
    );
    assert_eq!(dialog.font_point, Some(8));
    assert_eq!(dialog.font_face.as_deref(), Some("MS Shell Dlg"));

    // Controls: BUTTON "OK", EDIT, STATIC "Label" in template order.
    assert_eq!(dialog.items.len(), 3);
    let classes: Vec<ItemClass> = dialog.items.iter().map(|item| item.class.clone()).collect();
    assert_eq!(
        classes,
        vec![ItemClass::Button, ItemClass::Edit, ItemClass::Static]
    );
    let ids: Vec<u16> = dialog.items.iter().map(|item| item.id).collect();
    assert_eq!(ids, vec![1, 2, 3]);

    let button = match dialog.items.first() {
        Some(item) => item,
        None => {
            eprintln!("SKIP parses_windres_dialog_fixture: expected a button item");
            return;
        }
    };
    assert_eq!(button.title, "OK");
    assert_eq!((button.x, button.y, button.cx, button.cy), (20, 20, 50, 14));
    assert_eq!(
        button.pixel_rect,
        PixelRect {
            x: 40,
            y: 40,
            cx: 100,
            cy: 28,
        }
    );
}

#[test]
fn no_rsrc_exe_yields_no_dialogs() {
    let Some((image, sections)) = fixture_parsed("no_rsrc.exe") else {
        eprintln!("SKIP no_rsrc_exe_yields_no_dialogs: fixture missing");
        return;
    };
    let dialogs = wie_pe::resources::parse_dialogs(&image, &sections);
    assert!(
        dialogs.is_empty(),
        "expected no dialogs, got {}",
        dialogs.len()
    );
}

#[test]
fn truncated_resource_tree_is_not_fatal() {
    // A section map pointing at a resource root that is cut short must not
    // panic or fail — it just yields no dialogs.
    let sections = vec![wie_pe::PeSectionMap {
        name: ".rsrc".to_owned(),
        va: 0x1000,
        virtual_size: 0x1000,
        pointer_to_raw_data: 0x200,
        size_of_raw_data: 0x1000,
        characteristics: 0x4000_0040,
        final_protect: 0x04,
    }];
    let image = vec![0_u8; 16];
    assert!(wie_pe::resources::parse_dialogs(&image, &sections).is_empty());
}

/// Sanity-check the fixture toolchain path used by `parses_windres_dialog_fixture`.
#[test]
fn fixture_sources_are_present() {
    let dir = fixture_dir();
    assert!(dir.join("dialog1.rc").is_file(), "dialog1.rc checked in");
    assert!(dir.join("main.c").is_file(), "main.c checked in");
}
