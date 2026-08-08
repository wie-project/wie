//! Gdi32 tests: TextOutA / BitBlt / StretchBlt / PatBlt success paths, stock-object resolution, and the TEXTMETRICA / TEXTMETRICW zerocopy byte layouts.
use super::*;

// --- Gdi32 ---

#[test]
fn test_text_out_a_returns_cch() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x100, 10, 20, 0x2000, 0x3000);
    // cchString at RSP+0x28 = 5.
    engine.mem_write(0x3028, &5_u32.to_le_bytes()).ok();
    assert_return_value!(
        gdi32::handle_text_out_a(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        5
    );
}

#[test]
fn test_bit_blt_success() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x100, 0, 0, 100, 0);
    assert_return_value!(
        gdi32::handle_bit_blt(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        1
    );
}

#[test]
fn test_stretch_blt_success() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x100, 0, 0, 100, 0);
    assert_return_value!(
        gdi32::handle_stretch_blt(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        1
    );
}

#[test]
fn test_pat_blt_success() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x100, 0, 0, 100, 0);
    assert_return_value!(
        gdi32::handle_pat_blt(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        1
    );
}

// ── GDI32 ─────────────────────────────────────────────────────────

#[test]
fn test_get_stock_object_white_brush() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0, 0, 0, 0, STACK_TOP);
    let r = gdi32::handle_get_stock_object(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("GetStockObject(WHITE_BRUSH)");
    assert!(r.return_value != 0);
}

#[test]
fn test_get_stock_object_unknown_returns_zero() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0xFF, 0, 0, 0, STACK_TOP);
    assert_return_value!(
        gdi32::handle_get_stock_object(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
}

// ── gdi32 lane: TEXTMETRIC byte layouts (zerocopy writes) ─────────────

/// Create a memory DC and return its HDC through the real handler.
fn create_memory_dc(engine: &mut IcedCpu, state: &mut WinApiState) -> u64 {
    let r = crate::gdi32::handle_create_compatible_dc(&mut HandlerContext::new(
        engine,
        test_environment(),
        state,
    ))
    .expect("CreateCompatibleDC must succeed");
    r.return_value
}

/// Read `len` guest bytes at `addr` into a Vec (avoids slice-typed buffers).
fn read_guest_bytes(engine: &mut IcedCpu, addr: u64, len: usize) -> Vec<u8> {
    let mut bytes = vec![0_u8; len];
    engine.mem_read(addr, &mut bytes).expect("read guest bytes");
    bytes
}

/// GetTextMetricsA writes the TEXTMETRICA layout through the typed view:
/// LONGs @0..43, BYTE char fields @44..52, 3 trailing pad bytes (56 total).
#[test]
fn test_get_text_metrics_a_writes_textmetrica_byte_layout() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let hdc = create_memory_dc(&mut engine, &mut state);
    let metrics_va = 0x4000_u64;
    write_regs(&mut engine, hdc, metrics_va, 0, 0, 0);
    let r = crate::gdi32::handle_get_text_metrics_a(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("GetTextMetricsA must dispatch");
    assert_eq!(r.return_value, 1);

    let bytes = read_guest_bytes(&mut engine, metrics_va, 56);
    let height = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let weight = i32::from_le_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]);
    assert!(height > 0, "resolved font height is positive");
    assert_eq!(weight, 400, "regular weight default");
    // A char fields are single BYTEs at 44..47; flags follow at 48..52.
    assert_eq!(&bytes[44..48], &[0, 0, 0, 0], "tmFirstChar..tmBreakChar");
    assert_eq!(bytes[48], 0, "tmItalic @48");
    assert_eq!(bytes[51], 0x01, "tmPitchAndFamily @51");
    assert_eq!(bytes[52], 0, "tmCharSet @52");
    assert_eq!(&bytes[53..56], &[0, 0, 0], "trailing pad @53..55 zeroed");
}

/// GetTextMetricsW writes the TEXTMETRICW layout: WCHAR char fields @44..51,
/// flags @52..56, 3 trailing pad bytes (60 total) — the A/W offset shift.
#[test]
fn test_get_text_metrics_w_writes_textmetricw_byte_layout() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let hdc = create_memory_dc(&mut engine, &mut state);
    let metrics_va = 0x4000_u64;
    write_regs(&mut engine, hdc, metrics_va, 0, 0, 0);
    let r = crate::gdi32::handle_get_text_metrics_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("GetTextMetricsW must dispatch");
    assert_eq!(r.return_value, 1);

    let bytes = read_guest_bytes(&mut engine, metrics_va, 60);
    let height = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let weight = i32::from_le_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]);
    assert!(height > 0, "resolved font height is positive");
    assert_eq!(weight, 400, "regular weight default");
    // W char fields are 2-byte WCHARs at 44..51 (vs BYTEs at 44..47 in A).
    assert_eq!(&bytes[44..52], &[0; 8], "tmFirstChar..tmBreakChar WCHARs");
    assert_eq!(bytes[52], 0, "tmItalic @52");
    assert_eq!(bytes[55], 0x01, "tmPitchAndFamily @55");
    assert_eq!(bytes[56], 0, "tmCharSet @56");
    assert_eq!(&bytes[57..60], &[0, 0, 0], "trailing pad @57..59 zeroed");
}
