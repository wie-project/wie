use crate::WinApiState;
use crate::gdi32::font_system::FontEngine;
use crate::handles::{Hbitmap, Hbrush, Hdc, Hfont, Hpen, Hwnd};
use crate::state::handle_newtype;

use super::{
    BITMAP_HANDLE_BASE, BITMAP_HANDLE_STRIDE, BRUSH_HANDLE_BASE, BRUSH_HANDLE_STRIDE,
    DC_HANDLE_BASE, DC_HANDLE_STRIDE, FONT_HANDLE_BASE, FONT_HANDLE_STRIDE, PEN_HANDLE_BASE,
    PEN_HANDLE_STRIDE, STOCK_BLACK_BRUSH_HANDLE, STOCK_WHITE_BRUSH_HANDLE,
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
    state.heap_state.heap.alloc_coherent(engine, size)
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
    /// Allocate a new DC handle and record.
    pub fn alloc_dc(&mut self, kind: DcKind) -> Hdc {
        let handle = Hdc::from(self.next_dc_handle.as_u64());
        self.next_dc_handle =
            DcHandle::from(self.next_dc_handle.as_u64().wrapping_add(DC_HANDLE_STRIDE));
        self.dcs.push(DcRecord {
            handle,
            kind,
            selected_bitmap: None,
            selected_brush: None,
            selected_pen: None,
            selected_font: None,
            text_color: 0,
            bk_color: 0x00FF_FFFF, // white
            bk_mode: 2,            // OPAQUE (real GDI default)
        });
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
        });
        handle
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
        0x0000_0000_6800_5003 => Some(0x0080_8080), // GRAY_BRUSH
        0x0000_0000_6800_5004 => None,              // NULL_BRUSH — no fill
        _ => state
            .gdi_state()
            .find_brush(brush_handle)
            .map(|brush| brush.color),
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
