//! Print-DC core (P1a): the `DcKind::Print` job lifecycle plus the geometry
//! and stroke APIs a print program needs — minus text rasterization (P1b).
//!
//! A print DC is a [`DcRecord`] of kind `Print` backed by a [`PrintJob`] in
//! `GdiState::print_jobs` (a 300-DPI letter page canvas is ~34 MB, so the
//! job lives OUT of `DcRecord`). The guest drives the job through the
//! StartDocW → (StartPage → EndPage)* → EndDoc state machine; each EndPage
//! moves the in-progress [`PageCanvas`] into the job's `pages`. EndDoc hands
//! the collected pages off (P3): under a registered print-JOB bridge the
//! pages MOVE into a [`PrintJobRequest`] and a real host NSPrintOperation
//! runs (the native macOS print pipeline); without a bridge the pages are
//! written as `page-N.bmp` under the `WIE_PRINT_TO` directory (or dropped
//! with an info log when unset) — the headless oracle.
//!
//! Text rasterization is deliberately out of scope: `TextOutW` / `DrawTextW`
//! on a print DC are documented no-ops until the P1b text arm lands, so the
//! P1a end-to-end asserts page STRUCTURE (white pages, count, dimensions),
//! not pixels.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::guest_layout::DocInfoW;
use crate::guest_memory::{checked_address, read_i32, with_typed_read};
use crate::guest_string::read_utf16_lossy as read_guest_utf16_lossy;
use crate::handles::{Hbrush, Hdc, Hpen};
use crate::user32::low_i32;
use crate::{HandlerContext, WinApiHandlerResult};

use super::state::{
    DcKind, PageCanvas, PrintJobState, STOCK_BLACK_PEN_HANDLE, STOCK_WHITE_BRUSH_HANDLE,
    brush_color, pen_color,
};

/// Handles `GDI32.dll!CreateDCW` — allocate a print DC.
///
/// Any driver name produces a print DC with the default pick (US Letter,
/// portrait, 1 copy) — generic-app support (RNotepad never calls this; its
/// print DC comes from `PrintDlgW` with `PD_RETURNDC`, wired by the comdlg32
/// lane). `lpszDevice` / `lpszOutput` / `lpInitData` are ignored in P1a.
pub fn handle_create_dc_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let driver_va = engine
        .read_rcx()
        .context("failed to read RCX for CreateDCW")?;

    if driver_va != 0 {
        let driver = read_guest_utf16_lossy(engine, driver_va, 64).unwrap_or_default();
        tracing::debug!(driver, "CreateDCW");
    }

    let handle = state.gdi_state().alloc_print_dc();

    ctx.finish(handle.as_u64())
}

/// Handles `GDI32.dll!StartDocW` — begin a print document.
///
/// Reads `DOCINFOW.cbSize` and `lpszDocName` (capped at 4096 chars) and moves
/// the job from `Idle` to `DocStarted`. Returns 1 on success; 0 for a
/// non-print DC, a null DOCINFO, or a job that already has a document open.
pub fn handle_start_doc_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc = engine
        .read_rcx()
        .context("failed to read RCX for StartDocW")?;
    let docinfo_va = engine
        .read_rdx()
        .context("failed to read RDX for StartDocW")?;

    let mut success = false;
    if docinfo_va != 0 {
        // DOCINFOW (Win64): int cbSize @0, then LPCWSTR lpszDocName @8,
        // lpszOutput @16, lpszDatatype @24, DWORD fwType @32. One typed read
        // replaces the old cbSize + per-field pointer reads; the doc name is
        // read through the view's pointer (capped at 4096 chars).
        let doc_name_va = with_typed_read::<DocInfoW, _, _>(engine, docinfo_va, |docinfo| {
            Ok(docinfo.lpsz_doc_name)
        })
        .context("failed to read DOCINFOW.lpszDocName")?;
        let doc_name = read_guest_utf16_lossy(engine, doc_name_va, 4096)
            .context("failed to read StartDocW document name")?;

        if let Some(job) = state.gdi_state().find_print_job_mut(Hdc::from(hdc))
            && job.state == PrintJobState::Idle
        {
            job.state = PrintJobState::DocStarted;
            job.doc_name = doc_name;
            success = true;
        }
    }

    ctx.finish(u64::from(success))
}

/// Handles `GDI32.dll!StartPage` — begin a new page of the document.
///
/// Pushes a fresh white [`PageCanvas`] (the paper size at [`PRINT_DPI`]) as
/// the job's `current` and moves the job to `PageActive`. Returns 1 on
/// success; 0 unless the job is `DocStarted` (a StartPage without StartDoc is
/// an error).
pub fn handle_start_page(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc = engine
        .read_rcx()
        .context("failed to read RCX for StartPage")?;

    let mut success = false;
    if let Some(job) = state.gdi_state().find_print_job_mut(Hdc::from(hdc))
        && job.state == PrintJobState::DocStarted
        && job.current.is_none()
    {
        // ~34 MB for letter at 300 DPI — allocate explicitly, move (never
        // clone) on EndPage, and drop with the job.
        let (width, height) = job.paper_px;
        job.current = Some(PageCanvas::white(width, height));
        job.state = PrintJobState::PageActive;
        success = true;
    }

    ctx.finish(u64::from(success))
}

/// Handles `GDI32.dll!EndPage` — finish the page being painted.
///
/// Moves the job's `current` canvas into `pages` and returns to
/// `DocStarted`. Returns 1 on success; 0 unless the job is `PageActive`.
pub fn handle_end_page(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc = engine
        .read_rcx()
        .context("failed to read RCX for EndPage")?;

    let mut success = false;
    if let Some(job) = state.gdi_state().find_print_job_mut(Hdc::from(hdc))
        && job.state == PrintJobState::PageActive
        && let Some(canvas) = job.current.take()
    {
        job.pages.push(canvas);
        job.state = PrintJobState::DocStarted;
        success = true;
    }

    ctx.finish(u64::from(success))
}

/// Handles `GDI32.dll!EndDoc` — finish the document and hand the pages off.
///
/// The handoff depends on the host: under a registered print-JOB bridge (GUI
/// sessions — [`WindowState::print_job_bridge`]) the completed pages are MOVED
/// into a [`PrintJobRequest`] and returned as
/// [`WinApiControlSignal::PrintJobBridgeRequested`]; the runtime then runs the
/// bridge (a real macOS NSPrintOperation) without the shared lock and the
/// re-entry returns its success flag (1 / 0). Without a bridge (headless,
/// tests) each collected page is written as `page-N.bmp` under the
/// `WIE_PRINT_TO` directory (info-log + drop when unset) — the P1a oracle.
/// The job returns to `Idle` either way. Returns 1 on success; 0 unless the
/// job is `DocStarted` (an un-ended page is an error, as in real GDI).
pub fn handle_end_doc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    use crate::state::{PendingNativePrintJob, PrintJobRequest, WinApiControlSignal};

    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc = engine.read_rcx().context("failed to read RCX for EndDoc")?;

    // Re-entry: the native print operation ran (the pump arm ran the
    // print-job bridge without the shared lock and recorded its result).
    // Return 1 on success, 0 on failure (or when the bridge never answered).
    if let Some(pending) = state.window_state().pending_native_print_job.take() {
        let success = pending.success.unwrap_or(false);
        return ctx.finish(u64::from(success));
    }

    // Take the job payload OUT of the gdi borrow before the bridge decision:
    // `gdi_state()` and `window_state()` both go through `dll_states`, so the
    // two &mut accesses must not overlap.
    let handoff = if let Some(job) = state.gdi_state().find_print_job_mut(Hdc::from(hdc))
        && job.state == PrintJobState::DocStarted
    {
        let request = PrintJobRequest {
            pages: std::mem::take(&mut job.pages),
            print_info_id: u64::from(job.print_info_id),
            doc_name: std::mem::take(&mut job.doc_name),
            copies: job.copies,
        };
        let dc = job.dc;
        job.state = PrintJobState::Idle;
        Some((dc, request))
    } else {
        None
    };

    let Some((dc, request)) = handoff else {
        return ctx.finish(0);
    };

    // Native-bridge path (GUI sessions): park the guest while the host runs
    // the NSPrintOperation; the engine re-executes the fake API and this
    // handler's re-entry above returns the bridge's success flag.
    if state
        .try_window_state()
        .is_some_and(|window_state| window_state.print_job_bridge.is_some())
    {
        state.window_state().pending_native_print_job =
            Some(PendingNativePrintJob { success: None });
        tracing::info!(
            target: "wiegui",
            pages = request.pages.len(),
            print_info_id = request.print_info_id,
            copies = request.copies,
            doc_name = request.doc_name,
            "EndDoc: native print operation requested"
        );
        return Err(WinApiControlSignal::PrintJobBridgeRequested { request }.into());
    }

    // No bridge (headless / tests): the WIE_PRINT_TO BMP oracle stays.
    write_pages_bmp(dc, &request.doc_name, &request.pages);

    ctx.finish(1)
}

/// Handles `GDI32.dll!AbortDoc` — abandon the current document.
///
/// Drops the in-progress page and any collected pages and returns the job to
/// `Idle`. Returns 1 when a document was active (and was aborted); 0 for an
/// idle job, a non-print DC, or an unknown handle.
pub fn handle_abort_doc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc = engine
        .read_rcx()
        .context("failed to read RCX for AbortDoc")?;

    let mut was_active = false;
    if let Some(job) = state.gdi_state().find_print_job_mut(Hdc::from(hdc))
        && job.state != PrintJobState::Idle
    {
        job.current = None;
        job.pages.clear();
        job.state = PrintJobState::Idle;
        was_active = true;
    }

    ctx.finish(u64::from(was_active))
}

/// Handles `GDI32.dll!SetMapMode` — store/return the previous mapping mode.
///
/// MM_TEXT = 1 (the default) is what RNotepad sets before printing; rendering
/// ignores the mode in P1a. Returns the previous mode (1 for an unknown DC).
pub fn handle_set_map_mode(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc = engine
        .read_rcx()
        .context("failed to read RCX for SetMapMode")?;
    let mode_raw = engine
        .read_rdx()
        .context("failed to read RDX for SetMapMode")?;

    let mode = low_i32(mode_raw, "SetMapMode mode")?;
    let mode = u32::try_from(mode).context("SetMapMode mode does not fit u32")?;

    let previous = state
        .gdi_state()
        .find_dc_mut(Hdc::from(hdc))
        .map_or(1, |dc| std::mem::replace(&mut dc.map_mode, mode));

    ctx.finish(u64::from(previous))
}

/// Handles `GDI32.dll!Rectangle` — 1-px pen border, optional brush fill.
///
/// On a print DC the rectangle lands on the job's CURRENT page canvas
/// (top-down, right/bottom-exclusive like real GDI; clipped to the page);
/// other DCs succeed as a no-op in P1a. The border color is the DC's selected
/// pen — stock pens resolve, unselected defaults to BLACK_PEN (black). The
/// fill is the DC's selected brush — unselected defaults to WHITE_BRUSH;
/// NULL_BRUSH means no fill.
pub fn handle_rectangle(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc = engine
        .read_rcx()
        .context("failed to read RCX for Rectangle")?;
    let left = low_i32(
        engine
            .read_rdx()
            .context("failed to read RDX for Rectangle")?,
        "Rectangle left",
    )?;
    let top = low_i32(
        engine
            .read_r8()
            .context("failed to read R8 for Rectangle")?,
        "Rectangle top",
    )?;
    let right = low_i32(
        engine
            .read_r9()
            .context("failed to read R9 for Rectangle")?,
        "Rectangle right",
    )?;
    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for Rectangle")?;
    let bottom = read_i32(engine, checked_address(rsp, 0x28, "Rectangle bottom"))
        .context("failed to read Rectangle bottom")?;

    let dc_handle = Hdc::from(hdc);
    // Resolve pen/brush BEFORE the job borrow so the two &mut accesses stay
    // disjoint. Defaults mirror real GDI (BLACK_PEN stroke, WHITE_BRUSH fill).
    let (pen, brush) = {
        let gdi = state.gdi_state();
        let dc = gdi.find_dc(dc_handle);
        (
            dc.and_then(|dc| dc.selected_pen)
                .unwrap_or(Hpen::from(STOCK_BLACK_PEN_HANDLE)),
            dc.and_then(|dc| dc.selected_brush)
                .unwrap_or(Hbrush::from(STOCK_WHITE_BRUSH_HANDLE)),
        )
    };
    let stroke = pen_color(state, pen);
    let fill = brush_color(state, brush);

    let is_print = matches!(
        state.gdi_state().find_dc(dc_handle).map(|dc| dc.kind),
        Some(DcKind::Print(_))
    );
    if is_print
        && let Some(job) = state.gdi_state().find_print_job_mut(dc_handle)
        && let Some(canvas) = job.current.as_mut()
    {
        // A Rectangle before StartPage (no current canvas) is a no-op.
        draw_rect_canvas(canvas, left, top, right, bottom, stroke, fill);
    }

    ctx.finish(1)
}

/// Stroke/fill a rectangle on the canvas (1-px pen border).
///
/// `stroke = None` (NULL_PEN) skips the border; `fill = None` (NULL_BRUSH)
/// skips the interior. Coordinates are right/bottom-exclusive and clipped to
/// the page.
fn draw_rect_canvas(
    canvas: &mut PageCanvas,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    stroke: Option<u32>,
    fill: Option<u32>,
) {
    let width = i64::from(canvas.width);
    let height = i64::from(canvas.height);
    let x0 = i64::from(left).clamp(0, width);
    let y0 = i64::from(top).clamp(0, height);
    let x1 = i64::from(right).clamp(0, width);
    let y1 = i64::from(bottom).clamp(0, height);
    if x1 <= x0 || y1 <= y0 {
        return;
    }

    if let Some(color) = fill {
        for y in y0..y1 {
            for x in x0..x1 {
                set_px(canvas, x, y, color);
            }
        }
    }

    if let Some(color) = stroke {
        for x in x0..x1 {
            set_px(canvas, x, y0, color);
            set_px(canvas, x, y1 - 1, color);
        }
        for y in y0..y1 {
            set_px(canvas, x0, y, color);
            set_px(canvas, x1 - 1, y, color);
        }
    }
}

/// Set `(x, y)` to `color` when in bounds (tolerates partially-clipped rects).
fn set_px(canvas: &mut PageCanvas, x: i64, y: i64, color: u32) {
    let Ok(x) = u32::try_from(x) else {
        return;
    };
    let Ok(y) = u32::try_from(y) else {
        return;
    };
    if let Some(px) = canvas.pixel_mut(x, y) {
        *px = color;
    }
}

/// Hand off completed pages: write `page-{n}.bmp` under `WIE_PRINT_TO`.
///
/// 24-bpp bottom-up BMP (BITMAPFILEHEADER + BITMAPINFOHEADER + 4-byte-aligned
/// BGR rows). The pixel content is white in P1a — the text arm lands with
/// P1b. When `WIE_PRINT_TO` is unset the pages are dropped with an info log.
/// Per-page failures are logged, not fatal.
fn write_pages_bmp(dc: Hdc, doc_name: &str, pages: &[PageCanvas]) {
    let dir = match std::env::var("WIE_PRINT_TO") {
        Ok(dir) => PathBuf::from(dir),
        Err(_) => {
            tracing::info!(
                dc = dc.as_u64(),
                doc_name,
                pages = pages.len(),
                "EndDoc: WIE_PRINT_TO unset — dropping print pages"
            );
            return;
        }
    };
    for (i, page) in pages.iter().enumerate() {
        let path = dir.join(format!("page-{}.bmp", i + 1));
        match write_bmp(&path, page) {
            Ok(()) => tracing::info!(path = %path.display(), "EndDoc: wrote print page"),
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "EndDoc: failed to write print page")
            }
        }
    }
}

/// Write one page canvas as a 24-bpp bottom-up BMP file.
fn write_bmp(path: &std::path::Path, page: &PageCanvas) -> Result<()> {
    use std::io::Write;

    let width = u64::from(page.width);
    let height = u64::from(page.height);
    let row_bytes = width.checked_mul(3).context("BMP row size overflow")?;
    // DIB rows are DWORD-aligned (padded to a multiple of 4 bytes).
    let row_padded = row_bytes
        .checked_add(3)
        .context("BMP row padding overflow")?
        / 4
        * 4;
    let pixel_bytes = row_padded
        .checked_mul(height)
        .context("BMP pixel bytes overflow")?;
    let file_size = 14_u64
        .checked_add(40)
        .and_then(|n| n.checked_add(pixel_bytes))
        .context("BMP file size overflow")?;

    let mut data = Vec::with_capacity(usize::try_from(file_size).unwrap_or(0));
    // BITMAPFILEHEADER (14 bytes).
    data.extend_from_slice(&0x4D42_u16.to_le_bytes()); // "BM"
    data.extend_from_slice(&u32::try_from(file_size).unwrap_or(0).to_le_bytes()); // bfSize
    data.extend_from_slice(&0_u16.to_le_bytes()); // bfReserved1
    data.extend_from_slice(&0_u16.to_le_bytes()); // bfReserved2
    data.extend_from_slice(&54_u32.to_le_bytes()); // bfOffBits
    // BITMAPINFOHEADER (40 bytes).
    data.extend_from_slice(&40_u32.to_le_bytes()); // biSize
    data.extend_from_slice(&u32::try_from(width).unwrap_or(0).to_le_bytes()); // biWidth
    data.extend_from_slice(&u32::try_from(height).unwrap_or(0).to_le_bytes()); // biHeight
    data.extend_from_slice(&1_u16.to_le_bytes()); // biPlanes
    data.extend_from_slice(&24_u16.to_le_bytes()); // biBitCount
    data.extend_from_slice(&0_u32.to_le_bytes()); // biCompression (BI_RGB)
    data.extend_from_slice(&u32::try_from(pixel_bytes).unwrap_or(0).to_le_bytes()); // biSizeImage
    data.extend_from_slice(&0_i32.to_le_bytes()); // biXPelsPerMeter
    data.extend_from_slice(&0_i32.to_le_bytes()); // biYPelsPerMeter
    data.extend_from_slice(&0_u32.to_le_bytes()); // biClrUsed
    data.extend_from_slice(&0_u32.to_le_bytes()); // biClrImportant

    // Bottom-up rows: file row 0 is the LAST canvas row (top-down → flip).
    let pad = u32::try_from(row_padded - row_bytes).unwrap_or(0);
    let mut row = Vec::with_capacity(usize::try_from(row_padded).unwrap_or(0));
    let mut y = page.height;
    while y > 0 {
        y -= 1;
        row.clear();
        let base = u64::from(y).saturating_mul(width);
        for x in 0..page.width {
            let idx = usize::try_from(base + u64::from(x)).unwrap_or(0);
            let px = page.pixels.get(idx).copied().unwrap_or(0x00FF_FFFF);
            row.push(u8::try_from((px >> 16) & 0xFF).unwrap_or(0)); // B
            row.push(u8::try_from((px >> 8) & 0xFF).unwrap_or(0)); // G
            row.push(u8::try_from(px & 0xFF).unwrap_or(0)); // R
        }
        row.extend(std::iter::repeat_n(0_u8, usize::try_from(pad).unwrap_or(0)));
        data.extend_from_slice(&row);
    }

    let mut file = std::fs::File::create(path).context("failed to create BMP file")?;
    file.write_all(&data).context("failed to write BMP file")?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::sync::{Arc, Mutex};

    use wie_cpu::{CpuEngine, IcedCpu};

    use crate::gdi32::state::{
        DcKind, PrintJobState, STOCK_BLACK_BRUSH_HANDLE, STOCK_BLACK_PEN_HANDLE,
        STOCK_LTGRAY_BRUSH_HANDLE, STOCK_NULL_BRUSH_HANDLE, STOCK_NULL_PEN_HANDLE,
        STOCK_WHITE_PEN_HANDLE, brush_color, paper_tenths_mm_to_mm, paper_tenths_mm_to_px,
        pen_color,
    };
    use crate::gdi32::{handle_get_device_caps, handle_get_stock_object, handle_rectangle};
    use crate::guest_heap::GuestHeap;
    use crate::guest_memory::{read_i32, read_u8, write_i32, write_u64};
    use crate::handles::{Hbrush, Hdc, Hpen};
    use crate::state::{
        DEFAULT_ENVIRONMENT, DllStateMap, HeapState, KernelState, ModuleState, ProcessState,
        WinApiControlSignal, WinApiEnvironment, WinApiState,
    };
    use crate::sync_obj::SyncState;
    use crate::thread::ThreadState;
    use crate::user32::handle_inflate_rect;
    use crate::{HandlerContext, WinApiHandlerResult, present};

    use super::{
        handle_abort_doc, handle_create_dc_w, handle_end_doc, handle_end_page, handle_set_map_mode,
        handle_start_doc_w, handle_start_page,
    };

    const STACK_VA: u64 = 0x100_0000;
    const STACK_TOP: u64 = 0x100_FF00;

    fn test_engine() -> IcedCpu {
        let mut cpu = IcedCpu::open_x86_64();
        cpu.mem_map(0x1000, 0x10_0000, wie_cpu::RwxPerms::ALL)
            .expect("map test memory");
        cpu.mem_map(STACK_VA, 0x1_0000, wie_cpu::RwxPerms::ALL)
            .expect("map test stack");
        cpu.mem_write(STACK_TOP, &0_u64.to_le_bytes())
            .expect("write return address");
        cpu.write_rsp(STACK_TOP).ok();
        cpu
    }

    fn write_regs(cpu: &mut IcedCpu, rcx: u64, rdx: u64, r8: u64, r9: u64) {
        cpu.write_rcx(rcx).ok();
        cpu.write_rdx(rdx).ok();
        cpu.write_r8(r8).ok();
        cpu.write_r9(r9).ok();
        // `return_from_win64_api` pops the return address, so RSP drifts 8
        // bytes past STACK_TOP after the first call; reset it every call.
        cpu.write_rsp(STACK_TOP).ok();
    }

    /// Write the 5th/6th stack args at their Win64 shadow-space slots.
    fn write_stack_args(cpu: &mut IcedCpu, fifth: u64, sixth: u64) {
        cpu.mem_write(STACK_TOP + 0x28, &fifth.to_le_bytes())
            .expect("write 5th stack arg");
        cpu.mem_write(STACK_TOP + 0x30, &sixth.to_le_bytes())
            .expect("write 6th stack arg");
    }

    fn write_utf16(cpu: &mut IcedCpu, addr: u64, s: &str) {
        let mut bytes = Vec::new();
        for unit in s.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        cpu.mem_write(addr, &bytes).expect("write utf16 string");
    }

    fn test_environment() -> WinApiEnvironment {
        WinApiEnvironment {
            image_base: 0,
            command_line_a_ptr: 0,
            command_line_w_ptr: 0,
            environment_strings_w_ptr: 0,
            module_file_name_a_ptr: 0,
            module_file_name_w_ptr: 0,
            process_heap_handle: 0,
        }
    }

    /// Default state with a bump heap covering [0x2000, 0x10000).
    fn winapi_state() -> WinApiState {
        WinApiState {
            heap_state: HeapState {
                heap: GuestHeap::new(0x2000, 0x10000),
                next_fls_index: 0,
                fls_slots: Vec::new(),
                guest_fls_table_va: 0,
            },
            file_io: crate::FileIoState {
                executable_file_size: 0,
                executable_file_bytes: Arc::new(Vec::new()),
                executable_file_cursor: 0,
                next_find_handle: crate::FindFileHandle::from(0),
                find_handles: Vec::new(),
                host_file_mounts: Vec::new(),
                virtual_files: Vec::new(),
                open_files: ahash::HashMap::default(),
                next_file_handle: crate::FileHandle::from(0),
                next_resource_handle: crate::ResourceHandle::from(0),
                resources: Vec::new(),
                current_directory_wide: Vec::new(),
                bottle_root: None,
                volumes: crate::vfs::VolumeConfig::default(),
                guest_file_data_next: 0,
                guest_io: None,
                stdin_bytes: Vec::new(),
                stdin_cursor: 0,
                stdin_mode: crate::GuestStdinMode::InjectOnly,
                ucrt_files: ahash::HashMap::default(),
                ucrt_next_file_va: 0x0000_0000_6900_0000,
                cached_streams: ahash::HashMap::default(),
            },
            process: ProcessState {
                last_error: 0,
                next_registry_key_handle: crate::RegistryKeyHandle::from(0),
                registry_keys: Vec::new(),
                main_module_file_name: String::new(),
                main_module_path: String::new(),
                main_module_host_dir: None,
                error_mode: 0,
                suspended_threads: ahash::HashMap::default(),
                environment: DEFAULT_ENVIRONMENT
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                    .collect(),
                main_module_dialogs: Vec::new(),
                main_module_menus: Vec::new(),
                main_module_strings: Vec::new(),
                main_module_accelerators: Vec::new(),
            },
            kernel: KernelState {
                threads: ThreadState::primary(),
                sync: SyncState::new(),
                seh_pending: ahash::HashMap::default(),
            },
            dll_states: DllStateMap::new(),
            message_queue: Arc::new(Mutex::new(present::MessageQueue::default())),
            module_state: ModuleState {
                loaded_modules: ahash::HashMap::default(),
                import_resolver: None,
                get_proc_address_cache: ahash::HashMap::default(),
                next_module_handle: crate::ModuleHandle::from(
                    crate::dll_loader::REAL_MODULE_HANDLE_BASE,
                ),
            },
        }
    }

    fn run(
        ctx: &mut HandlerContext<'_>,
        handler: fn(&mut HandlerContext<'_>) -> anyhow::Result<WinApiHandlerResult>,
    ) -> u64 {
        handler(ctx).expect("handler should succeed").return_value
    }

    /// Create a print DC via the CreateDCW handler and return its handle.
    fn create_print_dc(engine: &mut IcedCpu, state: &mut WinApiState) -> u64 {
        write_regs(engine, 0, 0, 0, 0); // driver = NULL (any driver → print job)
        run(
            &mut HandlerContext::new(engine, test_environment(), state),
            handle_create_dc_w,
        )
    }

    /// StartDocW on `hdc` with the given guest doc-name pointer.
    fn start_doc(engine: &mut IcedCpu, state: &mut WinApiState, hdc: u64, docinfo_va: u64) -> u64 {
        write_regs(engine, hdc, docinfo_va, 0, 0);
        run(
            &mut HandlerContext::new(engine, test_environment(), state),
            handle_start_doc_w,
        )
    }

    // ── DC lifecycle ─────────────────────────────────────────────────

    #[test]
    fn create_dc_w_allocates_a_print_job_and_delete_dc_drops_it() {
        let mut engine = test_engine();
        let mut state = winapi_state();

        let hdc = create_print_dc(&mut engine, &mut state);
        assert_ne!(hdc, 0);

        let gdi = state.gdi_state();
        let dc = gdi.find_dc(Hdc::from(hdc)).expect("print DC record exists");
        assert_eq!(dc.kind, DcKind::Print(Hdc::from(hdc)));
        assert_eq!(dc.map_mode, 1, "MM_TEXT default");
        let job = gdi
            .find_print_job(Hdc::from(hdc))
            .expect("print job exists");
        assert_eq!(job.dc, Hdc::from(hdc));
        assert_eq!(job.state, PrintJobState::Idle);
        assert_eq!(job.copies, 1);
        assert_eq!(job.paper_px, (2550, 3300), "US Letter at 300 DPI");
        assert_eq!(job.paper_mm, (216, 279));

        // DeleteDC drops both the record and the (potentially huge) job.
        write_regs(&mut engine, hdc, 0, 0, 0);
        run(
            &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
            crate::gdi32::handle_delete_dc,
        );
        assert!(state.gdi_state().find_dc(Hdc::from(hdc)).is_none());
        assert!(state.gdi_state().find_print_job(Hdc::from(hdc)).is_none());
    }

    #[test]
    fn print_paper_conversion_is_exact_for_letter() {
        // 215.9 mm = 2159 tenths → 2550 px at 300 DPI; 279.4 mm → 3300 px.
        assert_eq!(paper_tenths_mm_to_px(2159, 300), 2550);
        assert_eq!(paper_tenths_mm_to_px(2794, 300), 3300);
        assert_eq!(paper_tenths_mm_to_mm(2159), 216);
        assert_eq!(paper_tenths_mm_to_mm(2794), 279);
    }

    // ── GetDeviceCaps geometry ───────────────────────────────────────

    #[test]
    fn get_device_caps_reports_print_geometry() {
        let mut engine = test_engine();
        let mut state = winapi_state();
        let hdc = create_print_dc(&mut engine, &mut state);

        let mut caps = |index: u64| -> u64 {
            write_regs(&mut engine, hdc, index, 0, 0);
            run(
                &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
                handle_get_device_caps,
            )
        };

        assert_eq!(caps(88), 300, "LOGPIXELSX");
        assert_eq!(caps(90), 300, "LOGPIXELSY");
        assert_eq!(caps(8), 2550, "HORZRES == PHYSICALWIDTH");
        assert_eq!(caps(110), 2550, "PHYSICALWIDTH");
        assert_eq!(caps(10), 3300, "VERTRES == PHYSICALHEIGHT");
        assert_eq!(caps(111), 3300, "PHYSICALHEIGHT");
        assert_eq!(caps(112), 0, "PHYSICALOFFSETX (no hardware margins)");
        assert_eq!(caps(113), 0, "PHYSICALOFFSETY");
        assert_eq!(caps(4), 216, "HORZSIZE mm");
        assert_eq!(caps(6), 279, "VERTSIZE mm");
        assert_eq!(caps(12), 32, "BITSPIXEL");
        assert_eq!(caps(24), u64::MAX, "NUMCOLORS");

        // A memory DC still gets the screen table (96 DPI, 1920×1080).
        let mem_dc = state.gdi_state().alloc_dc(DcKind::Memory);
        write_regs(&mut engine, mem_dc.as_u64(), 88, 0, 0);
        assert_eq!(
            run(
                &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
                handle_get_device_caps,
            ),
            96,
            "screen LOGPIXELSX unchanged for non-print DCs"
        );
    }

    // ── The print state machine ──────────────────────────────────────

    #[test]
    fn start_doc_reads_docinfo_and_runs_the_full_page_cycle() {
        let mut engine = test_engine();
        let mut state = winapi_state();
        let hdc = create_print_dc(&mut engine, &mut state);

        // DOCINFOW at 0x2000: cbSize @0, lpszDocName @8 (Win64 alignment).
        write_i32(&mut engine, 0x2000, 40).expect("cbSize");
        write_utf16(&mut engine, 0x3000, "WIE Print Test");
        write_u64(&mut engine, 0x2008, 0x3000).expect("lpszDocName");

        assert_eq!(start_doc(&mut engine, &mut state, hdc, 0x2000), 1);
        {
            let job = state
                .gdi_state()
                .find_print_job(Hdc::from(hdc))
                .expect("job exists");
            assert_eq!(job.state, PrintJobState::DocStarted);
            assert_eq!(job.doc_name, "WIE Print Test");
            assert_eq!(job.copies, 1);
        }

        // StartPage → a fresh white 2550×3300 canvas.
        write_regs(&mut engine, hdc, 0, 0, 0);
        assert_eq!(
            run(
                &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
                handle_start_page,
            ),
            1
        );
        {
            let job = state.gdi_state().find_print_job(Hdc::from(hdc)).unwrap();
            assert_eq!(job.state, PrintJobState::PageActive);
            let canvas = job.current.as_ref().expect("current page exists");
            assert_eq!((canvas.width, canvas.height), (2550, 3300));
            assert_eq!(canvas.pixels.len(), 2550 * 3300);
            assert!(
                canvas.pixels.iter().all(|&p| p == 0x00FF_FFFF),
                "white page"
            );
        }

        // EndPage → moves the canvas into pages.
        write_regs(&mut engine, hdc, 0, 0, 0);
        assert_eq!(
            run(
                &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
                handle_end_page,
            ),
            1
        );
        {
            let job = state.gdi_state().find_print_job(Hdc::from(hdc)).unwrap();
            assert_eq!(job.state, PrintJobState::DocStarted);
            assert_eq!(job.pages.len(), 1);
            assert!(job.current.is_none());
        }

        // EndDoc → back to Idle, pages handed off (dropped without WIE_PRINT_TO).
        write_regs(&mut engine, hdc, 0, 0, 0);
        assert_eq!(
            run(
                &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
                handle_end_doc,
            ),
            1
        );
        {
            let job = state.gdi_state().find_print_job(Hdc::from(hdc)).unwrap();
            assert_eq!(job.state, PrintJobState::Idle);
            assert!(job.pages.is_empty());
        }
    }

    #[test]
    fn start_page_without_start_doc_is_an_error() {
        let mut engine = test_engine();
        let mut state = winapi_state();
        let hdc = create_print_dc(&mut engine, &mut state);

        write_regs(&mut engine, hdc, 0, 0, 0);
        assert_eq!(
            run(
                &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
                handle_start_page,
            ),
            0,
            "StartPage before StartDoc must fail"
        );
        assert_eq!(
            run(
                &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
                handle_end_page,
            ),
            0,
            "EndPage without StartPage must fail"
        );
        assert_eq!(
            run(
                &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
                handle_end_doc,
            ),
            0,
            "EndDoc without StartDoc must fail"
        );
    }

    // ── The native print-operation handoff (P3) ──────────────────────────

    /// StartDocW ("WIE Print Test") + StartPage + EndPage on `hdc`, so the
    /// job holds one completed page for the EndDoc handoff.
    fn start_one_page_document(engine: &mut IcedCpu, state: &mut WinApiState, hdc: u64) {
        write_i32(engine, 0x2000, 40).expect("cbSize");
        write_utf16(engine, 0x3000, "WIE Print Test");
        write_u64(engine, 0x2008, 0x3000).expect("lpszDocName");
        assert_eq!(start_doc(engine, state, hdc, 0x2000), 1);
        write_regs(engine, hdc, 0, 0, 0);
        assert_eq!(
            run(
                &mut HandlerContext::new(engine, test_environment(), state),
                handle_start_page,
            ),
            1
        );
        write_regs(engine, hdc, 0, 0, 0);
        assert_eq!(
            run(
                &mut HandlerContext::new(engine, test_environment(), state),
                handle_end_page,
            ),
            1
        );
    }

    /// Drive the `EndDoc` → native print-operation handoff with a scripted
    /// print-job bridge (the runtime's two-entry seam, simulated like the
    /// print-dialog bridge helper in `state/tests.rs`): the first entry moves
    /// the pages into [`WinApiControlSignal::PrintJobBridgeRequested`]; the
    /// pump runs the bridge with the owned request; the re-entry returns the
    /// bridge's success flag as the `EndDoc` value. Returns the re-entry's
    /// return value.
    fn end_doc_with_bridge(
        engine: &mut IcedCpu,
        state: &mut WinApiState,
        hdc: u64,
        bridge: crate::PrintJobBridge,
        expect_request: impl FnOnce(&crate::PrintJobRequest),
    ) -> u64 {
        state.window_state().print_job_bridge = Some(bridge);
        write_regs(engine, hdc, 0, 0, 0);
        let err = handle_end_doc(&mut HandlerContext::new(engine, test_environment(), state))
            .expect_err("the first entry parks the guest for the native print operation");
        let signal = err
            .downcast::<WinApiControlSignal>()
            .expect("a control signal");
        let WinApiControlSignal::PrintJobBridgeRequested { request } = signal else {
            panic!("expected a print-job bridge request");
        };
        // Assert the moved payload BEFORE the bridge consumes the request.
        expect_request(&request);
        // What the runtime does between the two entries: take the bridge out,
        // run it with the owned request (no shared lock), restore it, record
        // the result on the pending record.
        let bridge = state
            .window_state()
            .print_job_bridge
            .take()
            .expect("bridge registered");
        let succeeded = bridge(request);
        state.window_state().print_job_bridge = Some(bridge);
        state
            .window_state()
            .pending_native_print_job
            .as_mut()
            .expect("pending print job recorded")
            .success = Some(succeeded);
        // Re-entry: the engine re-executes the fake API; the handler returns
        // the bridge result as the EndDoc value.
        write_regs(engine, hdc, 0, 0, 0);
        run(
            &mut HandlerContext::new(engine, test_environment(), state),
            handle_end_doc,
        )
    }

    #[test]
    fn end_doc_moves_the_pages_into_the_print_job_bridge_and_returns_its_result() {
        let mut engine = test_engine();
        let mut state = winapi_state();
        let hdc = create_print_dc(&mut engine, &mut state);
        start_one_page_document(&mut engine, &mut state, hdc);
        // The job's print-info id + copies come from the PrintDlgW pick; seed
        // them so the round-trip through the request is observable.
        {
            let job = state
                .gdi_state()
                .find_print_job_mut(Hdc::from(hdc))
                .unwrap();
            job.print_info_id = 42;
            job.copies = 3;
        }

        let result = end_doc_with_bridge(
            &mut engine,
            &mut state,
            hdc,
            Box::new(|request| {
                // The bridge receives the pages BY VALUE — the moved canvases.
                assert_eq!(request.pages.len(), 1);
                assert_eq!(request.pages[0].pixels.len(), 2550 * 3300);
                true
            }),
            |request| {
                assert_eq!(request.pages.len(), 1, "the completed page moved");
                assert_eq!(request.print_info_id, 42);
                assert_eq!(request.doc_name, "WIE Print Test");
                assert_eq!(request.copies, 3);
            },
        );

        assert_eq!(result, 1, "a successful operation → EndDoc returns 1");
        // The job returned to Idle; the pages are GONE (moved, never cloned).
        let job = state.gdi_state().find_print_job(Hdc::from(hdc)).unwrap();
        assert_eq!(job.state, PrintJobState::Idle);
        assert!(
            job.pages.is_empty(),
            "pages moved into the request, not cloned"
        );
        assert!(job.doc_name.is_empty(), "doc name moved with the pages");
        assert!(
            state.window_state().pending_native_print_job.is_none(),
            "the pending record is consumed by the re-entry"
        );
    }

    #[test]
    fn end_doc_returns_zero_when_the_print_bridge_reports_failure() {
        let mut engine = test_engine();
        let mut state = winapi_state();
        let hdc = create_print_dc(&mut engine, &mut state);
        start_one_page_document(&mut engine, &mut state, hdc);

        let result = end_doc_with_bridge(
            &mut engine,
            &mut state,
            hdc,
            Box::new(|_request| false),
            |_request| {},
        );

        assert_eq!(result, 0, "a failed operation → EndDoc returns 0");
        let job = state.gdi_state().find_print_job(Hdc::from(hdc)).unwrap();
        assert_eq!(job.state, PrintJobState::Idle);
    }

    #[test]
    fn end_doc_without_a_bridge_keeps_the_bmp_oracle() {
        let mut engine = test_engine();
        let mut state = winapi_state();
        let hdc = create_print_dc(&mut engine, &mut state);
        start_one_page_document(&mut engine, &mut state, hdc);
        // No print-job bridge: the headless WIE_PRINT_TO path (unset → pages
        // dropped with an info log), EndDoc still succeeds.
        assert_eq!(
            run(
                &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
                handle_end_doc,
            ),
            1
        );
        let job = state.gdi_state().find_print_job(Hdc::from(hdc)).unwrap();
        assert_eq!(job.state, PrintJobState::Idle);
        assert!(
            state.window_state().pending_native_print_job.is_none(),
            "the BMP path never records a pending native print job"
        );
    }

    #[test]
    fn abort_doc_drops_an_active_document_but_reports_idle_as_not_active() {
        let mut engine = test_engine();
        let mut state = winapi_state();
        let hdc = create_print_dc(&mut engine, &mut state);

        // AbortDoc on an idle job → 0.
        write_regs(&mut engine, hdc, 0, 0, 0);
        assert_eq!(
            run(
                &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
                handle_abort_doc,
            ),
            0
        );

        // Mid-document abort: DocStarted → StartPage → AbortDoc returns 1 and
        // drops current + any collected pages.
        write_i32(&mut engine, 0x2000, 40).expect("cbSize");
        write_utf16(&mut engine, 0x3000, "Doc");
        write_u64(&mut engine, 0x2008, 0x3000).expect("lpszDocName");
        assert_eq!(start_doc(&mut engine, &mut state, hdc, 0x2000), 1);
        write_regs(&mut engine, hdc, 0, 0, 0);
        run(
            &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
            handle_start_page,
        );
        write_regs(&mut engine, hdc, 0, 0, 0);
        assert_eq!(
            run(
                &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
                handle_abort_doc,
            ),
            1
        );
        let job = state.gdi_state().find_print_job(Hdc::from(hdc)).unwrap();
        assert_eq!(job.state, PrintJobState::Idle);
        assert!(job.current.is_none());
        assert!(job.pages.is_empty());
    }

    #[test]
    fn print_apis_fail_on_non_print_dcs() {
        let mut engine = test_engine();
        let mut state = winapi_state();
        let mem_dc = state.gdi_state().alloc_dc(DcKind::Memory);

        write_i32(&mut engine, 0x2000, 40).expect("cbSize");
        write_utf16(&mut engine, 0x3000, "Doc");
        write_u64(&mut engine, 0x2008, 0x3000).expect("lpszDocName");
        assert_eq!(
            start_doc(&mut engine, &mut state, mem_dc.as_u64(), 0x2000),
            0
        );
        write_regs(&mut engine, mem_dc.as_u64(), 0, 0, 0);
        assert_eq!(
            run(
                &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
                handle_start_page,
            ),
            0
        );
    }

    // ── SetMapMode ───────────────────────────────────────────────────

    #[test]
    fn set_map_mode_returns_the_previous_mode() {
        let mut engine = test_engine();
        let mut state = winapi_state();
        let hdc = create_print_dc(&mut engine, &mut state);

        write_regs(&mut engine, hdc, 1, 0, 0); // MM_TEXT
        assert_eq!(
            run(
                &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
                handle_set_map_mode,
            ),
            1,
            "first SetMapMode returns the MM_TEXT default"
        );
        write_regs(&mut engine, hdc, 8, 0, 0); // MM_ANISOTROPIC
        assert_eq!(
            run(
                &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
                handle_set_map_mode,
            ),
            1,
            "second SetMapMode returns the previously stored mode"
        );
        assert_eq!(
            state.gdi_state().find_dc(Hdc::from(hdc)).unwrap().map_mode,
            8
        );
    }

    // ── Rectangle on the print canvas ────────────────────────────────

    /// Start a document + one page on `hdc`, returning the active canvas.
    fn start_one_page<'a>(
        engine: &mut IcedCpu,
        state: &'a mut WinApiState,
        hdc: u64,
    ) -> &'a mut crate::gdi32::state::PageCanvas {
        write_i32(engine, 0x2000, 40).expect("cbSize");
        write_utf16(engine, 0x3000, "Doc");
        write_u64(engine, 0x2008, 0x3000).expect("lpszDocName");
        assert_eq!(start_doc(engine, state, hdc, 0x2000), 1);
        write_regs(engine, hdc, 0, 0, 0);
        run(
            &mut HandlerContext::new(engine, test_environment(), state),
            handle_start_page,
        );
        state
            .gdi_state()
            .find_print_job_mut(Hdc::from(hdc))
            .expect("job exists")
            .current
            .as_mut()
            .expect("current page")
    }

    fn px(canvas: &crate::gdi32::state::PageCanvas, x: u32, y: u32) -> u32 {
        canvas
            .pixels
            .get(usize::try_from(u64::from(y) * u64::from(canvas.width) + u64::from(x)).unwrap())
            .copied()
            .unwrap()
    }

    #[test]
    fn rectangle_strokes_the_selected_pen_without_filling_when_null_brush() {
        let mut engine = test_engine();
        let mut state = winapi_state();
        let hdc = create_print_dc(&mut engine, &mut state);

        // Select BLACK_PEN + NULL_BRUSH (stock) like RNotepad's header.
        select_stock(&mut engine, &mut state, hdc, 7, 5);

        let _ = start_one_page(&mut engine, &mut state, hdc);
        write_regs(&mut engine, hdc, 100, 100, 400);
        write_stack_args(&mut engine, 300, 0); // bottom = 300
        run(
            &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
            handle_rectangle,
        );

        let canvas = state
            .gdi_state()
            .find_print_job_mut(Hdc::from(hdc))
            .unwrap()
            .current
            .as_ref()
            .unwrap();
        assert_eq!(px(canvas, 100, 100), 0, "top-left border = BLACK_PEN");
        assert_eq!(px(canvas, 399, 299), 0, "bottom-right border");
        assert_eq!(
            px(canvas, 200, 200),
            0x00FF_FFFF,
            "interior stays white (NULL_BRUSH)"
        );
        assert_eq!(px(canvas, 0, 0), 0x00FF_FFFF, "outside the rect untouched");
        assert_eq!(px(canvas, 100, 150), 0, "left edge");
        assert_eq!(px(canvas, 250, 100), 0, "top edge");
    }

    /// GetStockObject(pen_stock) + GetStockObject(brush_stock), then
    /// SelectObject both into `hdc` (exercises the stock-object selection path).
    fn select_stock(
        engine: &mut IcedCpu,
        state: &mut WinApiState,
        hdc: u64,
        pen_stock: u64,
        brush_stock: u64,
    ) {
        let pen = get_stock(engine, state, pen_stock);
        let brush = get_stock(engine, state, brush_stock);
        write_regs(engine, hdc, pen, 0, 0);
        run(
            &mut HandlerContext::new(engine, test_environment(), state),
            crate::gdi32::handle_select_object,
        );
        write_regs(engine, hdc, brush, 0, 0);
        run(
            &mut HandlerContext::new(engine, test_environment(), state),
            crate::gdi32::handle_select_object,
        );
    }

    fn get_stock(engine: &mut IcedCpu, state: &mut WinApiState, index: u64) -> u64 {
        write_regs(engine, index, 0, 0, 0);
        run(
            &mut HandlerContext::new(engine, test_environment(), state),
            handle_get_stock_object,
        )
    }

    #[test]
    fn rectangle_uses_the_dc_pen_and_resolves_stock_pens() {
        let mut engine = test_engine();
        let mut state = winapi_state();
        let hdc = create_print_dc(&mut engine, &mut state);

        // WHITE_PEN + NULL_BRUSH → border white, interior white.
        select_stock(&mut engine, &mut state, hdc, 6, 5);
        start_one_page(&mut engine, &mut state, hdc);
        write_regs(&mut engine, hdc, 10, 10, 60);
        write_stack_args(&mut engine, 50, 0);
        run(
            &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
            handle_rectangle,
        );
        {
            let canvas = state
                .gdi_state()
                .find_print_job(Hdc::from(hdc))
                .unwrap()
                .current
                .as_ref()
                .unwrap();
            assert_eq!(px(canvas, 10, 10), 0x00FF_FFFF, "WHITE_PEN border");
        }

        // NULL_PEN → no stroke at all.
        let null_pen = get_stock(&mut engine, &mut state, 8);
        write_regs(&mut engine, hdc, null_pen, 0, 0);
        run(
            &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
            crate::gdi32::handle_select_object,
        );
        write_regs(&mut engine, hdc, 5, 5, 40);
        write_stack_args(&mut engine, 30, 0);
        run(
            &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
            handle_rectangle,
        );
        {
            let canvas = state
                .gdi_state()
                .find_print_job(Hdc::from(hdc))
                .unwrap()
                .current
                .as_ref()
                .unwrap();
            assert_eq!(
                px(canvas, 5, 5),
                0x00FF_FFFF,
                "NULL_PEN leaves the pixel white"
            );
        }

        // A real CreatePen color strokes as selected.
        let red_pen = state.gdi_state().alloc_pen(0x0000_FF00); // red
        write_regs(&mut engine, hdc, red_pen.as_u64(), 0, 0);
        run(
            &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
            crate::gdi32::handle_select_object,
        );
        write_regs(&mut engine, hdc, 20, 20, 70);
        write_stack_args(&mut engine, 60, 0);
        run(
            &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
            handle_rectangle,
        );
        {
            let canvas = state
                .gdi_state()
                .find_print_job(Hdc::from(hdc))
                .unwrap()
                .current
                .as_ref()
                .unwrap();
            assert_eq!(px(canvas, 20, 20), 0x0000_FF00, "live pen color");
        }
    }

    #[test]
    fn rectangle_clips_to_the_page() {
        let mut engine = test_engine();
        let mut state = winapi_state();
        let hdc = create_print_dc(&mut engine, &mut state);
        select_stock(&mut engine, &mut state, hdc, 7, 5);
        start_one_page(&mut engine, &mut state, hdc);

        // A rect that hangs off the top-left corner must not panic or bleed.
        let min_bits = u64::from_ne_bytes((-2147483648_i64).to_ne_bytes());
        write_regs(&mut engine, hdc, min_bits, min_bits, 100);
        write_stack_args(&mut engine, 100, 0);
        run(
            &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
            handle_rectangle,
        );
        let canvas = state
            .gdi_state()
            .find_print_job(Hdc::from(hdc))
            .unwrap()
            .current
            .as_ref()
            .unwrap();
        assert_eq!(px(canvas, 0, 0), 0, "clipped corner still stroked");
        assert_eq!(
            px(canvas, 99, 99),
            0,
            "clipped bottom-right of the 100×100 rect"
        );
    }

    // ── Stock objects (wingdi.h ids) ─────────────────────────────────

    #[test]
    fn stock_ids_match_wingdi_h_and_pens_resolve() {
        let mut engine = test_engine();
        let mut state = winapi_state();

        // The wingdi.h id table: BLACK_BRUSH is 4 (the old table said 1),
        // WHITE_PEN 6, BLACK_PEN 7, NULL_PEN 8.
        assert_eq!(
            get_stock(&mut engine, &mut state, 4),
            STOCK_BLACK_BRUSH_HANDLE
        );
        assert_eq!(
            get_stock(&mut engine, &mut state, 1),
            STOCK_LTGRAY_BRUSH_HANDLE
        );
        assert_eq!(
            get_stock(&mut engine, &mut state, 6),
            STOCK_WHITE_PEN_HANDLE
        );
        assert_eq!(
            get_stock(&mut engine, &mut state, 7),
            STOCK_BLACK_PEN_HANDLE
        );
        assert_eq!(get_stock(&mut engine, &mut state, 8), STOCK_NULL_PEN_HANDLE);
        assert_eq!(
            get_stock(&mut engine, &mut state, 0xFF),
            0,
            "unknown → NULL"
        );

        // The pen/brush resolvers map the stock handles to colors.
        assert_eq!(
            pen_color(&mut state, Hpen::from(STOCK_BLACK_PEN_HANDLE)),
            Some(0)
        );
        assert_eq!(
            pen_color(&mut state, Hpen::from(STOCK_WHITE_PEN_HANDLE)),
            Some(0x00FF_FFFF)
        );
        assert_eq!(
            pen_color(&mut state, Hpen::from(STOCK_NULL_PEN_HANDLE)),
            None
        );
        assert_eq!(
            brush_color(&mut state, Hbrush::from(STOCK_BLACK_BRUSH_HANDLE)),
            Some(0)
        );
        assert_eq!(
            brush_color(&mut state, Hbrush::from(STOCK_LTGRAY_BRUSH_HANDLE)),
            Some(0x00C0_C0C0)
        );
        assert_eq!(
            brush_color(&mut state, Hbrush::from(STOCK_NULL_BRUSH_HANDLE)),
            None,
            "NULL_BRUSH → no fill"
        );
    }

    // ── GetTextMetricsW (W layout) ───────────────────────────────────

    #[test]
    fn get_text_metrics_w_fills_the_wide_layout() {
        let mut engine = test_engine();
        let mut state = winapi_state();
        let hdc = create_print_dc(&mut engine, &mut state);

        write_regs(&mut engine, hdc, 0x4000, 0, 0);
        assert_eq!(
            run(
                &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
                crate::gdi32::handle_get_text_metrics_w,
            ),
            1
        );
        let tm_height = read_i32(&mut engine, 0x4000).expect("tmHeight");
        let tm_weight = read_i32(&mut engine, 0x401C).expect("tmWeight");
        let tm_italic = read_u8(&mut engine, 0x4034).expect("tmItalic");
        // W layout puts tmItalic at offset 52 and the char fields before it as
        // WCHARs (the A path would put tmItalic at 48).
        assert!(tm_height > 0, "resolved font height is positive");
        assert_eq!(tm_weight, 400, "regular weight default");
        assert_eq!(tm_italic, 0);
        assert_eq!(
            read_u8(&mut engine, 0x4037).expect("tmPitchAndFamily"),
            0x01
        );
    }

    // ── InflateRect (user32) ─────────────────────────────────────────

    #[test]
    fn inflate_rect_grows_and_shrinks_in_place() {
        let mut engine = test_engine();
        let mut state = winapi_state();

        // RECT { left: 10, top: 10, right: 100, bottom: 50 } at 0x2000.
        write_i32(&mut engine, 0x2000, 10).expect("left");
        write_i32(&mut engine, 0x2004, 10).expect("top");
        write_i32(&mut engine, 0x2008, 100).expect("right");
        write_i32(&mut engine, 0x200C, 50).expect("bottom");

        // Inflate by (10, 5): left/top shrink, right/bottom grow.
        write_regs(&mut engine, 0x2000, 10, 5, 0);
        assert_eq!(
            run(
                &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
                handle_inflate_rect,
            ),
            1
        );
        assert_eq!(read_i32(&mut engine, 0x2000).expect("left"), 0);
        assert_eq!(read_i32(&mut engine, 0x2004).expect("top"), 5);
        assert_eq!(read_i32(&mut engine, 0x2008).expect("right"), 110);
        assert_eq!(read_i32(&mut engine, 0x200C).expect("bottom"), 55);

        // Deflate by (-10, -5) → back to the original.
        write_regs(&mut engine, 0x2000, -10_i64 as u64, -5_i64 as u64, 0);
        run(
            &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
            handle_inflate_rect,
        );
        assert_eq!(read_i32(&mut engine, 0x2000).expect("left"), 10);
        assert_eq!(read_i32(&mut engine, 0x2004).expect("top"), 10);
        assert_eq!(read_i32(&mut engine, 0x2008).expect("right"), 100);
        assert_eq!(read_i32(&mut engine, 0x200C).expect("bottom"), 50);

        // NULL rect → FALSE.
        write_regs(&mut engine, 0, 10, 5, 0);
        assert_eq!(
            run(
                &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
                handle_inflate_rect,
            ),
            0
        );
    }
}
