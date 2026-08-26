use crate::WinApiState;
use crate::gdi32::font_system::FontEngine;
use crate::handles::{Hbitmap, Hbrush, Hdc, Hfont, Hpen, Hwnd};
use crate::state::handle_newtype;

use super::{
    BITMAP_HANDLE_BASE, BITMAP_HANDLE_STRIDE, BRUSH_HANDLE_BASE, BRUSH_HANDLE_STRIDE,
    DC_HANDLE_BASE, DC_HANDLE_STRIDE, FONT_HANDLE_BASE, FONT_HANDLE_STRIDE, PEN_HANDLE_BASE,
    PEN_HANDLE_STRIDE, STOCK_BLACK_BRUSH_HANDLE, STOCK_BLACK_PEN_HANDLE, STOCK_DKGRAY_BRUSH_HANDLE,
    STOCK_GRAY_BRUSH_HANDLE, STOCK_LTGRAY_BRUSH_HANDLE, STOCK_NULL_BRUSH_HANDLE,
    STOCK_NULL_PEN_HANDLE, STOCK_WHITE_BRUSH_HANDLE, STOCK_WHITE_PEN_HANDLE,
};

// ── Allocator-counter newtypes (ADR-003) ───────────────────────────────
//
// The `next_*_handle` counters are typed separately from the guest-visible
// handle newtypes (`handles.rs`) so one allocator cannot be fed another's
// counter. They are internal to the GDI state; the alloc functions convert to
// the typed handles (`Hdc::from`, …) at the store boundary.

handle_newtype! {
    /// Allocator counter for device-context handles.
    DcHandle
}

handle_newtype! {
    /// Allocator counter for bitmap/DIB handles.
    BitmapHandle
}

handle_newtype! {
    /// Allocator counter for brush handles.
    BrushHandle
}

handle_newtype! {
    /// Allocator counter for pen handles.
    PenHandle
}

handle_newtype! {
    /// Allocator counter for font handles.
    FontHandle
}

/// Allocates `size` bytes from the guest process heap for GDI object backing
/// buffers. `pub(super)` keeps it internal to the `state` module tree while
/// letting `objects` reuse it for `CreateDIBSection`.
pub(super) fn allocate_gdi_heap_block(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    size: u64,
) -> u64 {
    state
        .heap_state
        .heap
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .alloc_coherent(engine, size)
}

/// What kind of device context a [`DcRecord`] represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DcKind {
    /// DC obtained via GetDC(hwnd) or BeginPaint.
    Window(Hwnd),
    /// DC created via CreateCompatibleDC.
    Memory,
    /// DC obtained via GetDC(NULL) — the screen.
    Screen,
    /// A print DC from `CreateDCW` (or from `PrintDlgW` with `PD_RETURNDC`
    /// once the comdlg32 lane wires it). The page canvas lives in a
    /// [`PrintJob`] in `GdiState::print_jobs`, keyed by this DC's handle —
    /// the `Hdc` payload mirrors the record's own handle so kind-only checks
    /// (`GetDeviceCaps`, `DeleteDC`) never need a second lookup.
    Print(Hdc),
}

/// A live device context (DC).
#[derive(Debug, Clone)]
pub struct DcRecord {
    /// Fake handle for this DC.
    pub handle: Hdc,
    /// What this DC represents.
    pub kind: DcKind,
    /// Handle of the bitmap currently selected into this DC (if any).
    pub selected_bitmap: Option<Hbitmap>,
    /// Handle of the brush currently selected into this DC (if any).
    pub selected_brush: Option<Hbrush>,
    /// Handle of the pen currently selected into this DC (if any).
    pub selected_pen: Option<Hpen>,
    /// Handle of the font currently selected into this DC (if any).
    pub selected_font: Option<Hfont>,
    pub text_color: u32,
    pub bk_color: u32,
    pub bk_mode: u32,
    /// `SetMapMode` mode (MM_TEXT = 1 default; rendering ignores it in P1a).
    pub map_mode: u32,
}

/// A DIBSECTION allocated by CreateDIBSection (or a compatible bitmap).
#[derive(Debug, Clone)]
pub struct DibSection {
    /// Fake handle.
    pub handle: Hbitmap,
    /// Pixel width (always positive; stored from the absolute value).
    pub width: i32,
    /// **Signed** height: negative = top-down DIB.
    pub height: i32,
    /// Bits per pixel (1, 4, 8, 16, 24, 32).
    pub bit_count: u16,
    /// Row stride in bytes (aligned to 4).
    pub stride: i32,
    /// Guest virtual address of the pixel buffer.
    pub bits_va: u64,
    /// Size of the pixel buffer in bytes.
    pub byte_len: u64,
}

/// A brush allocated by `CreateSolidBrush` (or a future `CreateBrushIndirect`).
#[derive(Debug, Clone)]
pub struct BrushRecord {
    /// Fake HBRUSH handle.
    pub handle: Hbrush,
    /// 0RGB color (COLORREF-compatible).
    pub color: u32,
}

/// A pen allocated by `CreatePen` (rendering not yet implemented).
#[derive(Debug, Clone)]
pub struct PenRecord {
    /// Fake HPEN handle.
    pub handle: Hpen,
    /// 0RGB color (COLORREF-compatible).
    pub color: u32,
}

/// Resolution of WIE's emulated print device, in dots per inch.
pub const PRINT_DPI: u32 = 300;

/// Default paper: US Letter, in tenths of a millimetre (215.9 × 279.4 mm).
pub const DEFAULT_PAPER_TENTHS_MM: (u32, u32) = (2159, 2794);

/// Convert a paper size in tenths of a millimetre to device px at `dpi`.
///
/// 254 tenths-of-mm = 1 inch; plain integer division (no half-up) keeps the
/// Letter default exact (2159 × 300 / 254 = 2550, 2794 × 300 / 254 = 3300).
#[must_use]
pub fn paper_tenths_mm_to_px(tenths_mm: u32, dpi: u32) -> u32 {
    tenths_mm.saturating_mul(dpi) / 254
}

/// Convert a paper size in tenths of a millimetre to whole mm (rounded).
#[must_use]
pub fn paper_tenths_mm_to_mm(tenths_mm: u32) -> u32 {
    tenths_mm.saturating_add(5) / 10
}

/// Page-printing state machine, per [`PrintJob`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrintJobState {
    /// DC allocated; no document started.
    Idle,
    /// `StartDocW` succeeded — between StartDocW and EndDoc/AbortDoc.
    DocStarted,
    /// `StartPage` succeeded — `current` holds the page being painted.
    PageActive,
}

/// A raster page of a print job: white-filled 0RGB pixels, top-down.
///
/// Memory note: a 300-DPI letter page is 2550 × 3300 px ≈ 33.7 MB. Canvases
/// are allocated in `StartPage`, moved (never cloned) between the job's
/// `current` and `pages`, and dropped with the job (`DeleteDC` / `AbortDoc` /
/// `EndDoc`).
#[derive(Debug, Clone)]
pub struct PageCanvas {
    /// Pixel width (== the job's paper width in device px).
    pub width: u32,
    /// Pixel height (== the job's paper height in device px).
    pub height: u32,
    /// `width * height` 0RGB pixels, white-filled (`0x00FF_FFFF`), top-down
    /// (row 0 is the top of the page).
    pub pixels: Vec<u32>,
}

impl PageCanvas {
    /// A fresh white page canvas (`width` × `height` pixels, 0RGB white).
    ///
    /// The ~34 MB allocation is explicit here — callers move (never clone)
    /// the canvas between job states.
    #[must_use]
    pub fn white(width: u32, height: u32) -> Self {
        let count =
            usize::try_from(u64::from(width).saturating_mul(u64::from(height))).unwrap_or(0);
        Self {
            width,
            height,
            pixels: vec![0x00FF_FFFF; count],
        }
    }

    /// Mutable pixel at `(x, y)` — `None` when out of bounds, so callers can
    /// draw partially-clipped rects without index-panic risk.
    pub fn pixel_mut(&mut self, x: u32, y: u32) -> Option<&mut u32> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let idx = usize::try_from(
            u64::from(y)
                .saturating_mul(u64::from(self.width))
                .saturating_add(u64::from(x)),
        )
        .ok()?;
        self.pixels.get_mut(idx)
    }
}

/// A print job backing a `DcKind::Print` DC.
///
/// Lives in `GdiState::print_jobs`, NOT inside [`DcRecord`]: a 300-DPI page
/// canvas is ~34 MB and `DcRecord` is cloned on some paths.
#[derive(Debug, Clone)]
pub struct PrintJob {
    /// The print DC's handle (== the `DcKind::Print` payload).
    pub dc: Hdc,
    /// P1a: always 0. Reserved for the `PRINTDLG.hDC` identity that produced
    /// this job once the comdlg32 lane wires `PrintDlgW`.
    pub print_info_id: u32,
    /// P1a: always `None`. Reserved for the page-setup pick once
    /// `PageSetupDlgW` lands.
    pub pick: Option<u32>,
    /// Paper size in device px (`PHYSICALWIDTH`, `PHYSICALHEIGHT` — the
    /// canvas matches these exactly).
    pub paper_px: (u32, u32),
    /// Paper size in whole mm (`HORZSIZE`, `VERTSIZE`).
    pub paper_mm: (u32, u32),
    /// Resolution in dots per inch ([`PRINT_DPI`]).
    pub dpi: u32,
    /// Doc/page state machine.
    pub state: PrintJobState,
    /// Completed pages (EndPage'd), in print order.
    pub pages: Vec<PageCanvas>,
    /// The page being painted (between StartPage and EndPage/AbortDoc).
    pub current: Option<PageCanvas>,
    /// `DOCINFO.lpszDocName` from `StartDocW`.
    pub doc_name: String,
    /// Copies requested (P1a: 1 — the default pick).
    pub copies: u32,
}

impl PrintJob {
    /// A fresh print job for `dc` with the default pick: US Letter, portrait,
    /// 1 copy, 300 DPI, idle.
    #[must_use]
    pub fn new(dc: Hdc) -> Self {
        let width = paper_tenths_mm_to_px(DEFAULT_PAPER_TENTHS_MM.0, PRINT_DPI);
        let height = paper_tenths_mm_to_px(DEFAULT_PAPER_TENTHS_MM.1, PRINT_DPI);
        Self {
            dc,
            print_info_id: 0,
            pick: None,
            paper_px: (width, height),
            paper_mm: (
                paper_tenths_mm_to_mm(DEFAULT_PAPER_TENTHS_MM.0),
                paper_tenths_mm_to_mm(DEFAULT_PAPER_TENTHS_MM.1),
            ),
            dpi: PRINT_DPI,
            state: PrintJobState::Idle,
            pages: Vec::new(),
            current: None,
            doc_name: String::new(),
            copies: 1,
        }
    }
}

/// A font allocated by `CreateFontA/W` or `CreateFontIndirectA`.
///
/// Records the Win32 attributes the font engine resolves: face name, height,
/// weight, italic and charset. Resolution (family → system face, height →
/// px scale) happens lazily in [`FontEngine`]; the metric APIs and the
/// rasterizer share the same resolved font.
#[derive(Debug, Clone)]
pub struct FontRecord {
    /// HFONT handle.
    pub handle: Hfont,
    /// Raw `lfFaceName` ("" = system default / sans-serif).
    pub family: String,
    /// Raw `lfHeight` (px semantics applied at resolve time).
    pub height: i32,
    /// Resolved weight (400 or 700).
    pub weight: u16,
    /// Italic requested.
    pub italic: bool,
    /// `lfCharSet` (reported by `GetTextMetricsA.tmCharSet`).
    pub charset: u8,
    /// Raw `lfPitchAndFamily` byte (low bits: pitch, high nibble: family).
    /// The FIXED_PITCH bit (0x01) steers the monospace fallback at resolve
    /// time; recorded verbatim because `alloc_font` predates it.
    pub pitch: u8,
    /// `lfStrikeOut` requested — the rasterizer paints a strike line through
    /// the run. Recorded via [`GdiState::set_font_effects`] because
    /// `alloc_font` predates it (same pattern as `pitch`).
    pub strike_out: bool,
    /// `lfUnderline` requested — the rasterizer paints an underline below the
    /// baseline. See [`GdiState::set_font_effects`].
    pub underline: bool,
}

/// Per-slot GDI state stored in `DllId::Gdi`.
#[derive(Debug, Clone)]
pub struct GdiState {
    /// All allocated DCs.
    pub dcs: Vec<DcRecord>,
    /// All allocated DIB sections.
    pub dibs: Vec<DibSection>,
    /// All allocated brushes.
    pub brushes: Vec<BrushRecord>,
    /// All allocated pens.
    pub pens: Vec<PenRecord>,
    /// All allocated fonts.
    pub fonts: Vec<FontRecord>,
    /// Live print jobs — one per `DcKind::Print` DC. Kept OUT of `DcRecord`
    /// because a 300-DPI page canvas is ~34 MB and `DcRecord` is cloned on
    /// some paths.
    pub print_jobs: Vec<PrintJob>,
    /// The system-font engine (face + metrics caches for text rendering).
    pub font_engine: FontEngine,
    /// Next handle for DC allocation.
    pub next_dc_handle: DcHandle,
    /// Next handle for bitmap allocation.
    pub next_bitmap_handle: BitmapHandle,
    /// Next handle for brush allocation.
    pub next_brush_handle: BrushHandle,
    /// Next handle for pen allocation.
    pub next_pen_handle: PenHandle,
    /// Next handle for font allocation.
    pub next_font_handle: FontHandle,
}

impl Default for GdiState {
    fn default() -> Self {
        Self {
            dcs: Vec::new(),
            dibs: Vec::new(),
            brushes: Vec::new(),
            pens: Vec::new(),
            fonts: Vec::new(),
            print_jobs: Vec::new(),
            font_engine: FontEngine::default(),
            next_dc_handle: DcHandle::from(DC_HANDLE_BASE),
            next_bitmap_handle: BitmapHandle::from(BITMAP_HANDLE_BASE),
            next_brush_handle: BrushHandle::from(BRUSH_HANDLE_BASE),
            next_pen_handle: PenHandle::from(PEN_HANDLE_BASE),
            next_font_handle: FontHandle::from(FONT_HANDLE_BASE),
        }
    }
}

impl GdiState {
    /// A fresh `DcRecord` with the given kind (shared constructor so
    /// [`Self::alloc_dc`] and [`Self::alloc_print_dc`] cannot drift).
    fn new_dc_record(handle: Hdc, kind: DcKind) -> DcRecord {
        DcRecord {
            handle,
            kind,
            selected_bitmap: None,
            selected_brush: None,
            selected_pen: None,
            selected_font: None,
            text_color: 0,
            bk_color: 0x00FF_FFFF, // white
            bk_mode: 2,            // OPAQUE (real GDI default)
            map_mode: 1,           // MM_TEXT
        }
    }

    /// Compute and bump the next DC handle (shared by [`Self::alloc_dc`] and
    /// [`Self::alloc_print_dc`]).
    fn next_dc_handle_value(&mut self) -> Hdc {
        let handle = Hdc::from(self.next_dc_handle.as_u64());
        self.next_dc_handle =
            DcHandle::from(self.next_dc_handle.as_u64().wrapping_add(DC_HANDLE_STRIDE));
        handle
    }

    /// Allocate a new DC handle and record.
    pub fn alloc_dc(&mut self, kind: DcKind) -> Hdc {
        let handle = self.next_dc_handle_value();
        self.dcs.push(Self::new_dc_record(handle, kind));
        handle
    }

    /// Allocate a print DC: a `DcKind::Print` record plus its default-letter
    /// [`PrintJob`]. The job's `dc` and the record's handle are the same
    /// value, so every print API keys off one handle.
    pub fn alloc_print_dc(&mut self) -> Hdc {
        let handle = self.next_dc_handle_value();
        self.dcs
            .push(Self::new_dc_record(handle, DcKind::Print(handle)));
        self.print_jobs.push(PrintJob::new(handle));
        handle
    }

    /// Allocate a new bitmap/DIB handle.
    pub fn alloc_bitmap_handle(&mut self) -> Hbitmap {
        let handle = Hbitmap::from(self.next_bitmap_handle.as_u64());
        self.next_bitmap_handle = BitmapHandle::from(
            self.next_bitmap_handle
                .as_u64()
                .wrapping_add(BITMAP_HANDLE_STRIDE),
        );
        handle
    }

    /// Allocate a new brush handle and record.
    pub fn alloc_brush(&mut self, color: u32) -> Hbrush {
        let handle = Hbrush::from(self.next_brush_handle.as_u64());
        self.next_brush_handle = BrushHandle::from(
            self.next_brush_handle
                .as_u64()
                .wrapping_add(BRUSH_HANDLE_STRIDE),
        );
        self.brushes.push(BrushRecord { handle, color });
        handle
    }

    /// Allocate a new pen handle and record.
    pub fn alloc_pen(&mut self, color: u32) -> Hpen {
        let handle = Hpen::from(self.next_pen_handle.as_u64());
        self.next_pen_handle = PenHandle::from(
            self.next_pen_handle
                .as_u64()
                .wrapping_add(PEN_HANDLE_STRIDE),
        );
        self.pens.push(PenRecord { handle, color });
        handle
    }

    /// Allocate a new font handle and record.
    pub fn alloc_font(
        &mut self,
        family: String,
        height: i32,
        weight: u16,
        italic: bool,
        charset: u8,
    ) -> Hfont {
        let handle = Hfont::from(self.next_font_handle.as_u64());
        self.next_font_handle = FontHandle::from(
            self.next_font_handle
                .as_u64()
                .wrapping_add(FONT_HANDLE_STRIDE),
        );
        self.fonts.push(FontRecord {
            handle,
            family,
            height,
            weight,
            italic,
            charset,
            pitch: 0,
            strike_out: false,
            underline: false,
        });
        handle
    }

    /// Record the `lfPitchAndFamily` byte on a font (the pitch hint drives
    /// the monospace fallback at resolution time). A no-op for an unknown
    /// handle — the caller allocates the font first, so a miss is a bug.
    pub fn set_font_pitch(&mut self, handle: Hfont, pitch: u8) {
        if let Some(font) = self.fonts.iter_mut().find(|font| font.handle == handle) {
            font.pitch = pitch;
        }
    }

    /// Record the LOGFONT's `lfStrikeOut`/`lfUnderline` flags on a font (the
    /// rasterizer draws the effect strokes from them). A no-op for an unknown
    /// handle — the caller allocates the font first, so a miss is a bug.
    pub fn set_font_effects(&mut self, handle: Hfont, strike_out: bool, underline: bool) {
        if let Some(font) = self.fonts.iter_mut().find(|font| font.handle == handle) {
            font.strike_out = strike_out;
            font.underline = underline;
        }
    }

    /// Find a DC by handle.
    pub fn find_dc(&self, handle: Hdc) -> Option<&DcRecord> {
        self.dcs.iter().find(|dc| dc.handle == handle)
    }

    /// Find a mutable DC by handle.
    pub fn find_dc_mut(&mut self, handle: Hdc) -> Option<&mut DcRecord> {
        self.dcs.iter_mut().find(|dc| dc.handle == handle)
    }

    /// Find a DIB section by handle.
    pub fn find_dib(&self, handle: Hbitmap) -> Option<&DibSection> {
        self.dibs.iter().find(|dib| dib.handle == handle)
    }

    /// Find a brush record by handle.
    pub fn find_brush(&self, handle: Hbrush) -> Option<&BrushRecord> {
        self.brushes.iter().find(|brush| brush.handle == handle)
    }

    /// Find a pen record by handle.
    pub fn find_pen(&self, handle: Hpen) -> Option<&PenRecord> {
        self.pens.iter().find(|pen| pen.handle == handle)
    }

    /// Find a font record by handle.
    pub fn find_font(&self, handle: Hfont) -> Option<&FontRecord> {
        self.fonts.iter().find(|font| font.handle == handle)
    }

    /// Find a print job by DC handle.
    pub fn find_print_job(&self, dc: Hdc) -> Option<&PrintJob> {
        self.print_jobs.iter().find(|job| job.dc == dc)
    }

    /// Find a mutable print job by DC handle.
    pub fn find_print_job_mut(&mut self, dc: Hdc) -> Option<&mut PrintJob> {
        self.print_jobs.iter_mut().find(|job| job.dc == dc)
    }

    /// Remove a DC by handle.
    pub fn remove_dc(&mut self, handle: Hdc) {
        self.dcs.retain(|dc| dc.handle != handle);
    }

    /// Remove a DIB section by handle.
    pub fn remove_dib(&mut self, handle: Hbitmap) {
        self.dibs.retain(|dib| dib.handle != handle);
    }

    /// Remove a brush by handle.
    pub fn remove_brush(&mut self, handle: Hbrush) {
        self.brushes.retain(|brush| brush.handle != handle);
    }

    /// Remove a pen by handle.
    pub fn remove_pen(&mut self, handle: Hpen) {
        self.pens.retain(|pen| pen.handle != handle);
    }

    /// Remove a font by handle.
    pub fn remove_font(&mut self, handle: Hfont) {
        self.fonts.retain(|font| font.handle != handle);
    }

    /// Drop a print job (its page canvases go with it) by DC handle.
    pub fn remove_print_job(&mut self, dc: Hdc) {
        self.print_jobs.retain(|job| job.dc != dc);
    }
}

/// A GDI object classified from its handle's disjoint base range.
///
/// The four object ranges (`0x6820` bitmaps, `0x6830` brushes, `0x6840` pens,
/// `0x6850` fonts) are the namespace the handle value already lives in — this
/// decodes it into a typed [`GdiObject`] so `SelectObject` / `DeleteObject`
/// need a single match instead of probing every record table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GdiObject {
    /// `HBITMAP` — a bitmap/DIB handle.
    Dib(Hbitmap),
    /// `HBRUSH` — a brush handle.
    Brush(Hbrush),
    /// `HPEN` — a pen handle.
    Pen(Hpen),
    /// `HFONT` — a font handle.
    Font(Hfont),
}

impl GdiObject {
    /// Decode `handle` from its disjoint base range (the `*_HANDLE_BASE`
    /// constants above). DC handles and FAKE-range values classify to `None`.
    ///
    /// Debug-asserts the cross-kind collision invariant: a handle must never
    /// live in two GDI record tables at once — the ranges are disjoint, so a
    /// collision would mean an allocator reused a value across kinds.
    #[must_use]
    pub fn classify(handle: u64, state: &GdiState) -> Option<Self> {
        let classified = match handle & 0xFFFF_0000 {
            BITMAP_HANDLE_BASE => Some(Self::Dib(Hbitmap::from(handle))),
            BRUSH_HANDLE_BASE => Some(Self::Brush(Hbrush::from(handle))),
            PEN_HANDLE_BASE => Some(Self::Pen(Hpen::from(handle))),
            FONT_HANDLE_BASE => Some(Self::Font(Hfont::from(handle))),
            _ => None,
        };
        debug_assert!(
            {
                let dib = u8::from(state.dibs.iter().any(|d| d.handle == Hbitmap::from(handle)));
                let brush = u8::from(
                    state
                        .brushes
                        .iter()
                        .any(|b| b.handle == Hbrush::from(handle)),
                );
                let pen = u8::from(state.pens.iter().any(|p| p.handle == Hpen::from(handle)));
                let font = u8::from(state.fonts.iter().any(|f| f.handle == Hfont::from(handle)));
                dib.saturating_add(brush)
                    .saturating_add(pen)
                    .saturating_add(font)
                    <= 1
            },
            "GDI handle {handle:#x} collides across record tables"
        );
        classified
    }
}

/// Resolve a brush handle to its 0RGB color.
///
/// Recognizes the stock brushes returned by `GetStockObject`; anything else
/// must be a live [`BrushRecord`]. Returns `None` for the NULL_BRUSH (no
/// pixels change) and for unknown handles.
#[must_use]
pub fn brush_color(state: &mut WinApiState, brush_handle: Hbrush) -> Option<u32> {
    match brush_handle.as_u64() {
        STOCK_WHITE_BRUSH_HANDLE => Some(0x00FF_FFFF),
        STOCK_BLACK_BRUSH_HANDLE => Some(0),
        STOCK_GRAY_BRUSH_HANDLE => Some(0x0080_8080),
        STOCK_LTGRAY_BRUSH_HANDLE => Some(0x00C0_C0C0),
        STOCK_DKGRAY_BRUSH_HANDLE => Some(0x0040_4040),
        STOCK_NULL_BRUSH_HANDLE => None, // NULL_BRUSH — no fill
        _ => state
            .gdi_state()
            .find_brush(brush_handle)
            .map(|brush| brush.color),
    }
}

/// Resolve a pen handle to its 0RGB color.
///
/// Recognizes the stock pens returned by `GetStockObject` (`WHITE_PEN`,
/// `BLACK_PEN`, `NULL_PEN`); anything else must be a live [`PenRecord`].
/// Returns `None` for the NULL_PEN (no stroke) and for unknown handles.
#[must_use]
pub fn pen_color(state: &mut WinApiState, pen_handle: Hpen) -> Option<u32> {
    match pen_handle.as_u64() {
        STOCK_WHITE_PEN_HANDLE => Some(0x00FF_FFFF),
        STOCK_BLACK_PEN_HANDLE => Some(0),
        STOCK_NULL_PEN_HANDLE => None, // NULL_PEN — no stroke
        _ => state.gdi_state().find_pen(pen_handle).map(|pen| pen.color),
    }
}

/// A stock-object handle that `SelectObject` records on a DC.
///
/// Stock handles live in the 0x6800_500x FAKE range, so
/// [`GdiObject::classify`] decodes them to `None` — but selecting a stock
/// brush/pen must still take effect for the fill/stroke paths. Stock fonts
/// and palettes stay unrecorded (the font engine treats "no stored font" as
/// the system default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StockSelectKind {
    /// `WHITE_BRUSH` / `LTGRAY_BRUSH` / `GRAY_BRUSH` / `DKGRAY_BRUSH` /
    /// `BLACK_BRUSH` (incl. `NULL_BRUSH`).
    Brush,
    /// `WHITE_PEN` / `BLACK_PEN` / `NULL_PEN`.
    Pen,
}

/// Classify a stock-object handle into a selectable kind, if it is one.
#[must_use]
pub fn stock_select_kind(handle: u64) -> Option<StockSelectKind> {
    match handle {
        STOCK_WHITE_BRUSH_HANDLE
        | STOCK_LTGRAY_BRUSH_HANDLE
        | STOCK_GRAY_BRUSH_HANDLE
        | STOCK_DKGRAY_BRUSH_HANDLE
        | STOCK_BLACK_BRUSH_HANDLE
        | STOCK_NULL_BRUSH_HANDLE => Some(StockSelectKind::Brush),
        STOCK_WHITE_PEN_HANDLE | STOCK_BLACK_PEN_HANDLE | STOCK_NULL_PEN_HANDLE => {
            Some(StockSelectKind::Pen)
        }
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use crate::gdi32::height_px_from_lf;

    #[test]
    fn height_px_mapping_does_not_need_system_fonts() {
        // lfHeight == 0 → the 16 px default.
        assert_eq!(height_px_from_lf(0), 16);
        // Negative = character height in px.
        assert_eq!(height_px_from_lf(-24), 24);
        assert_eq!(height_px_from_lf(-1), 1);
        // Positive = cell height, approximated as the same px count.
        assert_eq!(height_px_from_lf(24), 24);
        assert_eq!(height_px_from_lf(16), 16);
        // Extreme values never collapse to zero.
        assert_eq!(height_px_from_lf(i32::MAX), i32::MAX);
        // |i32::MIN| does not fit i32 — the function falls back to the default.
        assert_eq!(height_px_from_lf(i32::MIN), 16);
    }

    #[test]
    fn font_handle_allocator_is_disjoint_from_other_gdi_bases() {
        let mut gdi = crate::gdi32::GdiState::default();
        let a = gdi.alloc_font(String::new(), 16, 400, false, 0);
        let b = gdi.alloc_font("arial".to_owned(), 24, 700, true, 1);
        assert_ne!(a, b);
        // Font handles live in 0x6850_0000; DC/bitmap/brush/pen bases are
        // 0x6810/0x6820/0x6830/0x6840 — disjoint by construction.
        assert_eq!(a.as_u64() & 0xFFFF_0000, 0x6850_0000);
        assert_eq!(b.as_u64() & 0xFFFF_0000, 0x6850_0000);
        assert_eq!(gdi.fonts.len(), 2);
        assert!(gdi.find_font(a).is_some());
        assert!(gdi.find_font(b).is_some());
        let record = gdi.find_font(b).expect("font b exists");
        assert_eq!(record.weight, 700);
        assert!(record.italic);
        assert_eq!(record.charset, 1);
        gdi.remove_font(a);
        assert!(gdi.find_font(a).is_none());
        assert!(gdi.find_font(b).is_some());
    }

    #[test]
    fn classify_decodes_each_gdi_kind_from_its_base_range() {
        use crate::gdi32::{DcKind, GdiObject};

        let mut gdi = crate::gdi32::GdiState::default();
        // Allocate one object of every kind so the collision assert in
        // `classify` has real tables to check against.
        let bitmap = gdi.alloc_bitmap_handle();
        let brush = gdi.alloc_brush(0x00ff_0000);
        let pen = gdi.alloc_pen(0x0000_ff00);
        let font = gdi.alloc_font("arial".to_owned(), 16, 400, false, 0);

        assert_eq!(
            GdiObject::classify(bitmap.as_u64(), &gdi),
            Some(GdiObject::Dib(bitmap)),
        );
        assert_eq!(
            GdiObject::classify(brush.as_u64(), &gdi),
            Some(GdiObject::Brush(brush)),
        );
        assert_eq!(
            GdiObject::classify(pen.as_u64(), &gdi),
            Some(GdiObject::Pen(pen)),
        );
        assert_eq!(
            GdiObject::classify(font.as_u64(), &gdi),
            Some(GdiObject::Font(font)),
        );

        // A DC handle, the NULL handle, and a FAKE-range stock handle
        // classify to None.
        let dc = gdi.alloc_dc(DcKind::Memory);
        assert_eq!(GdiObject::classify(dc.as_u64(), &gdi), None);
        assert_eq!(GdiObject::classify(0, &gdi), None);
        assert_eq!(GdiObject::classify(0x0000_0000_6800_5001, &gdi), None);
    }
}
