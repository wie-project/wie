//! GDI32 micro-suite: `gdi_regions` round-trips a 32-bpp DIB through
//! GetDIBits/SetDIBits and exercises the region APIs (CreateRectRgn,
//! CombineRgn, SetRectRgn, GetRgnBox).
//!
//! Windowless: the micro uses a memory DC and never touches the present
//! surface or a message loop, so it needs no GUI_SUITE_LOCK serialization
//! (same as the gdi_alpha MSIMG32 micro).

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
fn gdi32_dib_roundtrip_and_region_combine() {
    let Some(pe) = micro_exe("gdi_regions.exe") else {
        eprintln!("skip: gdi_regions.exe not built (run make -C micro-exes gdi_regions)");
        return;
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
