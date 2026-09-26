//! MSIMG32 micro-suite: `gdi_alpha` blends a 50%-alpha red DIB over an opaque
//! black DIB via AlphaBlend and proves the destination pixel changed (red
//! channel > 0 at (0,0)).
//!
//! Windowless: the micro uses two memory DCs and never touches the present
//! surface or a message loop, so it needs no GUI_SUITE_LOCK serialization
//! (same as the print_bmp GDI micro).

mod common;

use common::micro_exe;

#[test]
fn gdi_alpha_blends_onto_dib() {
    let Some(pe) = micro_exe("gdi_alpha.exe") else {
        return; // fixture absent: already reported (see tests/common/mod.rs)
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
