//! GDI32 micro-suite: `gdi_regions` round-trips a 32-bpp DIB through
//! GetDIBits/SetDIBits and exercises the region APIs (CreateRectRgn,
//! CombineRgn, SetRectRgn, GetRgnBox).
//!
//! Windowless: the micro uses a memory DC and never touches the present
//! surface or a message loop, so it needs no GUI_SUITE_LOCK serialization
//! (same as the gdi_alpha MSIMG32 micro).

mod common;

use common::micro_exe;

#[test]
fn gdi32_dib_roundtrip_and_region_combine() {
    let Some(pe) = micro_exe("gdi_regions.exe") else {
        return; // fixture absent: already reported (see tests/common/mod.rs)
    };
    let summary = wie_runtime::run_micro_exe(&pe, 256).expect("run_micro_exe");
    assert_eq!(
        summary.exit_code,
        Some(0),
        "gdi_regions selftest must exit 0: termination={:?} backend={}",
        summary.run.termination,
        summary.cpu_backend
    );
}
