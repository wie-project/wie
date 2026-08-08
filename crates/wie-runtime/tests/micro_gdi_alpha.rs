//! MSIMG32 micro-suite: `gdi_alpha` blends a 50%-alpha red DIB over an opaque
//! black DIB via AlphaBlend and proves the destination pixel changed (red
//! channel > 0 at (0,0)).
//!
//! Windowless: the micro uses two memory DCs and never touches the present
//! surface or a message loop, so it needs no GUI_SUITE_LOCK serialization
//! (same as the print_bmp GDI micro).

use std::path::PathBuf;

fn micro_exe(name: &str) -> Option<PathBuf> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("micro-exes/out");
    path.push(name);
    path.is_file().then_some(path)
}

#[test]
fn gdi_alpha_blends_onto_dib() {
    let Some(pe) = micro_exe("gdi_alpha.exe") else {
        eprintln!("skip: gdi_alpha.exe not built (run make -C micro-exes gdi_alpha)");
        return;
    };
    let summary = wie_runtime::run_micro_exe(&pe, 256).expect("run_micro_exe");
    assert_eq!(
        summary.exit_code,
        Some(0),
        "gdi_alpha selftest must exit 0: termination={:?} backend={}",
        summary.run.termination,
        summary.cpu_backend
    );
}
