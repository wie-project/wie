//! Gdi32 tests: TextOutA / BitBlt / StretchBlt / PatBlt success paths, stock-object resolution, and the TEXTMETRICA / TEXTMETRICW zerocopy byte layouts.
use super::*;

// --- Gdi32 ---

/// `GetDeviceCaps` on a memory DC reports the session's `DisplayMetrics`
/// (HORZRES/VERTRES and the mm sizes at the 96-dpi logical baseline) so GDI
/// probes agree with the monitor info and system metrics.
#[test]
fn test_get_device_caps_follows_custom_display_metrics() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.display = crate::DisplayMetrics::new(1728, 1117);
    let hdc = state.gdi_state().alloc_dc(crate::gdi32::DcKind::Memory);
    for (index, expected) in [
        (8_u64, 1728_u64), // HORZRES
        (10, 1117),        // VERTRES
        (110, 1728),       // PHYSICALWIDTH
        (111, 1117),       // PHYSICALHEIGHT
        (118, 1728),       // DESKTOPHORZRES
        (117, 1117),       // DESKTOPVERTRES
        (4, 457),          // HORZSIZE mm (1728 px * 25.4 / 96, rounded)
        (6, 296),          // VERTSIZE mm (1117 px * 25.4 / 96, rounded)
    ] {
        write_regs(&mut engine, hdc.as_u64(), index, 0, 0, STACK_TOP);
        assert_return_value!(
            gdi32::handle_get_device_caps(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            expected
        );
    }
}

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

// ── GetObjectA / GetObjectW ────────────────────────────────────────────

/// GetObjectW mirrors GetObjectA: a NULL buffer is a size-only query that
/// returns the 32-byte Win64 BITMAP size; a real buffer gets the 16×16 32-bpp
/// BITMAP (bmWidth @4 == 16, bmPlanes @16 == 1, bmBitsPixel @18 == 32).
#[test]
fn test_get_object_w_size_query_returns_bitmap_size_and_buffer_is_filled() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();

    // Size-only query: object_handle non-zero, NULL buffer → 32 bytes.
    write_regs(&mut engine, 0x6820_0000, 0, 0, 0, STACK_TOP);
    assert_return_value!(
        gdi32::handle_get_object_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        32
    );

    // With a large-enough buffer, the BITMAP is written and the type matches.
    write_regs(&mut engine, 0x6820_0000, 32, 0x4000, 0, STACK_TOP);
    assert_return_value!(
        gdi32::handle_get_object_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        32
    );
    assert_eq!(read_test_i32(&mut engine, 0x4000), 0, "bmType");
    assert_eq!(read_test_i32(&mut engine, 0x4004), 16, "bmWidth");
    assert_eq!(read_test_i32(&mut engine, 0x4008), 16, "bmHeight");
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

// ── Top-level DIB blits ────────────────────────────────────────────────

/// `SetDIBitsToDevice` reports the number of scan lines set (cLines, the 9th
/// stack arg) with a non-null source buffer; 0 when the buffer is null.
#[test]
fn test_set_dib_bits_to_device_reports_scan_line_count() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // rcx=hdc, rdx=xDest, r8=yDest, r9=w; stack: h@0x28, cLines@0x48, lpvBits@0x50.
    write_regs(&mut engine, 0x100, 0, 0, 100, STACK_TOP);
    engine
        .mem_write(STACK_TOP + 0x28, &100_u32.to_le_bytes())
        .ok();
    engine
        .mem_write(STACK_TOP + 0x48, &64_u32.to_le_bytes())
        .ok(); // cLines
    engine
        .mem_write(STACK_TOP + 0x50, &0x5000_u64.to_le_bytes())
        .ok(); // lpvBits
    assert_return_value!(
        gdi32::handle_set_dib_bits_to_device(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        64
    );

    // Null source buffer → 0 scan lines.
    write_regs(&mut engine, 0x100, 0, 0, 100, STACK_TOP);
    engine
        .mem_write(STACK_TOP + 0x28, &100_u32.to_le_bytes())
        .ok();
    engine
        .mem_write(STACK_TOP + 0x48, &64_u32.to_le_bytes())
        .ok(); // cLines
    engine
        .mem_write(STACK_TOP + 0x50, &0_u64.to_le_bytes())
        .ok(); // lpvBits = NULL
    assert_return_value!(
        gdi32::handle_set_dib_bits_to_device(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
}

/// `StretchDIBits` reports the number of scan lines copied (sized by the
/// source height) with a non-null bit buffer; 0 when the buffer is null.
#[test]
fn test_stretch_dib_bits_reports_source_height() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // rcx=hdc, rdx=XDest, r8=YDest, r9=nDestWidth; stack: nSrcHeight@0x48, lpBits@0x50.
    write_regs(&mut engine, 0x100, 0, 0, 200, STACK_TOP);
    engine
        .mem_write(STACK_TOP + 0x48, &40_u32.to_le_bytes())
        .ok(); // nSrcHeight
    engine
        .mem_write(STACK_TOP + 0x50, &0x5000_u64.to_le_bytes())
        .ok(); // lpBits
    assert_return_value!(
        gdi32::handle_stretch_dib_bits(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        40
    );

    write_regs(&mut engine, 0x100, 0, 0, 200, STACK_TOP);
    engine
        .mem_write(STACK_TOP + 0x48, &40_u32.to_le_bytes())
        .ok();
    engine
        .mem_write(STACK_TOP + 0x50, &0_u64.to_le_bytes())
        .ok(); // lpBits = NULL
    assert_return_value!(
        gdi32::handle_stretch_dib_bits(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
}

// ── Palette surface ───────────────────────────────────────────────────

/// CreatePalette returns a non-null fake HPALETTE for a valid LOGPALETTE
/// pointer and NULL for a null one; RealizePalette reports 0 entries;
/// GetSystemPaletteEntries fills the output and returns the count.
#[test]
fn test_palette_surface_create_realize_and_entries() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();

    // CreatePalette with a valid LOGPALETTE → a non-null fake handle.
    write_regs(&mut engine, 0x4000, 0, 0, 0, STACK_TOP);
    let created = gdi32::handle_create_palette(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("CreatePalette must succeed")
    .return_value;
    assert_ne!(created, 0, "a valid LOGPALETTE yields a fake HPALETTE");

    // CreatePalette with a null LOGPALETTE → NULL.
    write_regs(&mut engine, 0, 0, 0, 0, STACK_TOP);
    assert_return_value!(
        gdi32::handle_create_palette(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );

    // RealizePalette on a screen DC → 0 entries realized.
    write_regs(&mut engine, 1, 0, 0, 0, STACK_TOP);
    assert_return_value!(
        gdi32::handle_realize_palette(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );

    // GetSystemPaletteEntries fills the buffer with cEntries zero PALETTEENTRYs
    // and returns the count.
    write_regs(&mut engine, 1, 0, 3, 0x5000, STACK_TOP);
    assert_return_value!(
        gdi32::handle_get_system_palette_entries(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        3
    );
    let filled = read_guest_bytes(&mut engine, 0x5000, 3 * 4);
    assert_eq!(&filled[..], &[0_u8; 12], "zeroed PALETTEENTRYs");
}

// ── Device-DIB present lane ───────────────────────────────────────────

/// `SetDIBitsToDevice` on a window DC blits the guest buffer into the window's
/// present surface and defers a publish — the same `ensure_surface` +
/// `publish_deferred` lane `BitBlt` uses — while still reporting the scan-line
/// count (`cLines`).
#[test]
fn test_set_dib_bits_to_device_publishes_to_window_surface() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let hwnd = 0x6610_0101_u64;
    state.window_state().windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(hwnd),
        title: "Test".to_owned(),
        visible: true,
        width: 320,
        height: 240,
        ..Default::default()
    });
    // GetDC(hwnd) → a Window DC.
    write_regs(&mut engine, hwnd, 0, 0, 0, 0);
    let hdc = crate::user32::handle_get_dc(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("GetDC")
    .return_value;
    assert_ne!(hdc, 0, "GetDC returns a window DC");

    // 4×4 top-down 32-bpp source at 0x5000, BITMAPINFO header at 0x6000.
    let src_px = |row: u32, col: u32| -> u32 {
        0xFF00_0000 | (row << 16) | (col << 8) // BGRA: B=0, G=col, R=row, A=0xFF
    };
    let mut src = Vec::new();
    for row in 0..4_u32 {
        for col in 0..4_u32 {
            src.extend_from_slice(&src_px(row, col).to_le_bytes());
        }
    }
    engine.mem_write(0x5000, &src).expect("write source pixels");
    let mut bmi = [0_u8; 40];
    bmi[0..4].copy_from_slice(&40_u32.to_le_bytes()); // biSize
    bmi[4..8].copy_from_slice(&4_i32.to_le_bytes()); // biWidth
    bmi[8..12].copy_from_slice(&(-4_i32).to_le_bytes()); // biHeight (top-down)
    bmi[12..14].copy_from_slice(&1_u16.to_le_bytes()); // biPlanes
    bmi[14..16].copy_from_slice(&32_u16.to_le_bytes()); // biBitCount
    engine.mem_write(0x6000, &bmi).expect("write BITMAPINFO");

    // SetDIBitsToDevice(hdc, 0, 0, w=4, h=4, 0, 0, StartScan=0, cLines=4,
    //                  lpvBits=0x5000, lpbmi=0x6000, ColorUse=0).
    write_regs(&mut engine, hdc, 0, 0, 4, STACK_TOP);
    engine
        .mem_write(STACK_TOP + 0x28, &4_u32.to_le_bytes())
        .ok(); // h
    engine
        .mem_write(STACK_TOP + 0x30, &0_u32.to_le_bytes())
        .ok(); // xSrc
    engine
        .mem_write(STACK_TOP + 0x38, &0_u32.to_le_bytes())
        .ok(); // ySrc
    engine
        .mem_write(STACK_TOP + 0x40, &0_u32.to_le_bytes())
        .ok(); // StartScan
    engine
        .mem_write(STACK_TOP + 0x48, &4_u32.to_le_bytes())
        .ok(); // cLines
    engine
        .mem_write(STACK_TOP + 0x50, &0x5000_u64.to_le_bytes())
        .ok(); // lpvBits
    engine
        .mem_write(STACK_TOP + 0x58, &0x6000_u64.to_le_bytes())
        .ok(); // lpbmi
    engine
        .mem_write(STACK_TOP + 0x60, &0_u32.to_le_bytes())
        .ok(); // ColorUse
    let r = gdi32::handle_set_dib_bits_to_device(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("SetDIBitsToDevice");
    assert_eq!(r.return_value, 4, "reports cLines scan lines");

    // A surface exists for the top-level window, the blit landed, and a
    // deferred publish was registered.
    let hwnd_h = crate::handles::Hwnd::from(hwnd);
    let surf = state
        .present()
        .surfaces
        .get(&hwnd_h)
        .expect("window present surface exists");
    assert_eq!(surf.width, 320, "surface width");
    assert_eq!(surf.height, 240, "surface height");
    assert_eq!(surf.pixels[0], 0x0000_0000, "row0 col0 (R=0,G=0)");
    assert_eq!(surf.pixels[1], 0x0000_0100, "row0 col1 (G=1)");
    assert_eq!(
        surf.pixels[320], 0x0001_0000,
        "row1 col0 (R=1) (row stride = 320)"
    );
    let _ = surf;
    assert!(
        state.present().pending_publishes.contains(&hwnd_h),
        "SetDIBitsToDevice defers a publish for the window"
    );
    assert!(
        state.present().drain_pending_publishes() >= 1,
        "the deferred publish drains to a frame"
    );
}

/// A device-DIB blit to a memory/screen DC (no window surface) publishes
/// nothing and still reports its scan-line count.
#[test]
fn test_set_dib_bits_to_device_to_memory_dc_does_not_publish() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // rcx=hdc (unknown), stack args present, non-null lpvBits.
    write_regs(&mut engine, 0x100, 0, 0, 100, STACK_TOP);
    engine
        .mem_write(STACK_TOP + 0x28, &100_u32.to_le_bytes())
        .ok(); // h
    engine
        .mem_write(STACK_TOP + 0x30, &0_u32.to_le_bytes())
        .ok(); // xSrc
    engine
        .mem_write(STACK_TOP + 0x38, &0_u32.to_le_bytes())
        .ok(); // ySrc
    engine
        .mem_write(STACK_TOP + 0x40, &0_u32.to_le_bytes())
        .ok(); // StartScan
    engine
        .mem_write(STACK_TOP + 0x48, &64_u32.to_le_bytes())
        .ok(); // cLines
    engine
        .mem_write(STACK_TOP + 0x50, &0x5000_u64.to_le_bytes())
        .ok(); // lpvBits
    let r = gdi32::handle_set_dib_bits_to_device(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("SetDIBitsToDevice");
    assert_eq!(r.return_value, 64, "reports cLines scan lines");
    assert!(
        state.present().pending_publishes.is_empty(),
        "no publish for a non-window DC"
    );
}
