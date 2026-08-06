//! Interactive print-dialog integration (host side).
//!
//! `PrintDlgW` (comdlg32) is implemented in `wie-winapi`: under
//! [`wie_winapi::PrintDialogPolicy::Interactive`] the handler runs the native
//! macOS print panel (NSPrintPanel — the OS-equivalent of Windows' PrintDlg
//! dialog) through the bridge registered here, then writes the pick back into
//! the guest `PRINTDLG` (`hDC` / `nCopies` / `hDevMode` / `hDevNames`). The
//! pick's paper/orientation/copies seed the DEVMODE and the print job, so the
//! emulated print output matches what the user chose.
//!
//! `PageSetupDlgW` (comdlg32) gets the same treatment: under
//! [`wie_winapi::PageSetupDialogPolicy::Interactive`] the handler runs the
//! native macOS page-layout panel (NSPageLayout — the OS-equivalent of
//! Windows' Page Setup dialog) through a second bridge registered here, then
//! writes the pick's paper/orientation back into the guest `PAGESETUPDLG`
//! (`ptPaperSize` / `hDevMode` / `hDevNames`). The settings reach the later
//! `PrintDlgW` panel through the guest DEVMODE (the guest stores the handle,
//! the print handler seeds from it) — no host-side carrier needed.
//!
//! This module is the GUI integration point: it enables the interactive
//! policies on the runtime session so [`super::app::run_gui_windowed`] shows
//! the panels exactly when a real window is on screen, and — on macOS —
//! registers the native bridges. Headless runs and `trace` keep the default
//! [`wie_winapi::PrintDialogPolicy::Cancel`] / [`wie_winapi::PageSetupDialogPolicy::Cancel`]
//! policies and never open a panel.
//!
//! The bridge also maintains the session-scoped NSPrintInfo id-table: the
//! handler assigns each `PrintDlgW` call an id ([`wie_winapi::PrintDialogRequest::print_info_id`]),
//! and the bridge registers the user's resulting `NSPrintInfo` under that key.
//! The EndDoc handoff ([`run_native_print_job`]) consumes the entry to drive a
//! real NSPrintOperation — the guest's pages, the user's destination.

use wie_runtime::RuntimeSession;

// Names the `declare_class!` print-view definition needs at MODULE scope (the
// macro expands where it is invoked, not inside the bridge function): the
// class's ivars and method bodies name the AppKit/foundation types directly.
#[cfg(target_os = "macos")]
use objc2::declare_class;
#[cfg(target_os = "macos")]
use objc2::rc::Retained;
#[cfg(target_os = "macos")]
use objc2::{ClassType, DeclaredClass};
#[cfg(target_os = "macos")]
use objc2_app_kit::NSImage;
#[cfg(target_os = "macos")]
use objc2_foundation::{NSPoint, NSRect, NSSize};

/// Enable interactive print dialogs on a GUI session.
///
/// `PrintDlgW` then runs the native print panel (when the bridge is
/// registered) instead of returning a scripted policy answer. Must run before
/// the guest executes — call it right after the session is created, before
/// `run_windowed`.
#[cfg(target_os = "macos")]
pub fn enable_interactive_print_dialogs(session: &mut RuntimeSession) {
    session.set_print_dialog_policy(wie_winapi::PrintDialogPolicy::Interactive);
}

/// Non-macOS builds have no native print panel: keep the default Cancel
/// policy, so `PrintDlgW` returns FALSE exactly like a user canceling.
#[cfg(not(target_os = "macos"))]
pub fn enable_interactive_print_dialogs(_session: &mut RuntimeSession) {}

/// One retained `NSPrintInfo` in the id-table.
///
/// objc2 marks `InteriorMutable` classes like `NSPrintInfo` as !Send to
/// forbid lock-free cross-thread mutation, but the id-table MUST cross
/// threads: the bridge inserts the user's `NSPrintInfo` on the main thread
/// (inside [`objc2_foundation::run_on_main`]) and P3's EndDoc handoff consumes
/// it later on the guest thread. The owning `Mutex` provides exactly the
/// serialization objc2 requires, and objc2's `Retained` refcount is atomic
/// (the Arc-like storage), so transferring the wrapper only moves the
/// reference. Every dereference of the wrapped object happens while the
/// owning mutex is held.
#[cfg(target_os = "macos")]
// The EndDoc handoff consumes this entry (table.remove by print_info_id); the
// field is read there and written by the print-panel bridge.
pub(crate) struct PrintInfoEntry(pub(crate) objc2::rc::Retained<objc2_app_kit::NSPrintInfo>);
// SAFETY: see the type-level justification — the object is only ever touched
// under the owning `Mutex<PrintInfoTable>`, and the `Retained` refcount is
// atomic, so the transfer itself mutates nothing.
#[cfg(target_os = "macos")]
#[expect(unsafe_code)]
unsafe impl Send for PrintInfoEntry {}

// SAFETY: same as `Send`: every dereference is serialized by the owning
// mutex, which is the synchronization the !Send marker demanded.
#[cfg(target_os = "macos")]
#[expect(unsafe_code)]
unsafe impl Sync for PrintInfoEntry {}

/// Host-side NSPrintInfo id-table: print-dialog id → the user's `NSPrintInfo`.
///
/// The handler assigns the id ([`wie_winapi::PrintDialogRequest::print_info_id`]);
/// the bridge registers the retained `NSPrintInfo` here on accept. P3's EndDoc
/// handoff looks the entry up (and consumes it) to drive a real
/// NSPrintOperation; a leak-per-print is acceptable.
#[cfg(target_os = "macos")]
pub type PrintInfoTable =
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<u64, PrintInfoEntry>>>;

/// Register the native macOS print-panel bridge on a GUI session.
///
/// With a bridge registered, `PrintDlgW` under
/// [`wie_winapi::PrintDialogPolicy::Interactive`] shows a real NSPrintPanel:
/// the guest thread blocks inside the bridge until the user dismisses it
/// (dialog semantics, the same seam as the file-dialog bridge), then the
/// handler writes the pick back into the `PRINTDLG`. Sessions that never
/// register a bridge keep the default Cancel (headless runs, `trace`).
#[cfg(target_os = "macos")]
pub fn register_native_print_dialog_bridge(
    handle: &wie_runtime::GuestHandle,
    table: PrintInfoTable,
) {
    handle.set_print_dialog_bridge(Box::new({
        move |request| show_native_print_dialog(request, &table)
    }));
}

/// Points per millimetre: 1 inch = 72 pt = 25.4 mm (the NSPrintInfo paper
/// size is in points; the DEVMODE carries tenths of a millimetre).
#[cfg(target_os = "macos")]
const POINTS_PER_MM: f64 = 72.0 / 25.4;

/// Win32 `dmOrientation` values (the DEVMODE field the pick round-trips).
#[cfg(target_os = "macos")]
const DMORIENT_PORTRAIT: u16 = 1;
#[cfg(target_os = "macos")]
const DMORIENT_LANDSCAPE: u16 = 2;

/// Show one native print panel from the bridge callback.
///
/// Runs on the guest thread; [`objc2_foundation::run_on_main`] dispatches the
/// panel to the main thread's runloop (where AppKit's modal `runModal` is
/// valid — the same dispatch the rfd panels use) and blocks until the user
/// dismisses it. On accept the user's NSPrintInfo is registered in the
/// id-table under the handler-assigned key (the P3 EndDoc handoff consumes
/// it) and the plain-data pick is returned.
#[cfg(target_os = "macos")]
#[expect(unsafe_code)]
fn show_native_print_dialog(
    request: &wie_winapi::PrintDialogRequest,
    table: &PrintInfoTable,
) -> Option<wie_winapi::PrintDialogPick> {
    use objc2::ClassType;
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{NSPaperOrientation, NSPrintPanelResult};
    use objc2_app_kit::{NSPrintCopies, NSPrintInfo, NSPrintPanel, NSPrintPanelOptions};
    use objc2_foundation::{NSNumber, NSSize, run_on_main};

    run_on_main(|mtm| {
        // A fresh NSPrintInfo per call — never the sharedPrintInfo singleton,
        // so one print session's settings cannot leak into the next.
        // SAFETY: init: initializes the allocated object (the standard
        // alloc/init pair; the object is owned by the returned Retained).
        let print_info = unsafe { NSPrintInfo::init(NSPrintInfo::alloc()) };
        let (width_mm, height_mm) = request.paper_size_mm;
        // SAFETY: the two setters are plain property setters on the owned
        // object (the NSPrintInfo seed — paper size + orientation).
        unsafe {
            print_info.setPaperSize(NSSize::new(
                f64::from(width_mm) * POINTS_PER_MM,
                f64::from(height_mm) * POINTS_PER_MM,
            ));
            print_info.setOrientation(if request.orientation == DMORIENT_LANDSCAPE {
                NSPaperOrientation::Landscape
            } else {
                NSPaperOrientation::Portrait
            });
        }
        // Seed the panel's "Copies" box from the guest DEVMODE. SAFETY:
        // `NSPrintCopies` is the documented key for the copies count and
        // NSNumber is the documented value type; the panel reads it back as-is.
        unsafe {
            print_info.dictionary().setObject_forKey(
                &NSNumber::new_u32(u32::from(request.copies).max(1)),
                ProtocolObject::from_ref(NSPrintCopies),
            );
        }

        // SAFETY: printPanel: creates the panel (the standard AppKit
        // constructor); setOptions: is a plain property setter.
        let panel = unsafe { NSPrintPanel::printPanel(mtm) };
        // Offer the settings the pick carries: copies, paper size, orientation
        // (the page-range and scaling extras are not surfaced by the pick).
        unsafe {
            panel.setOptions(
                NSPrintPanelOptions::NSPrintPanelShowsCopies
                    .union(NSPrintPanelOptions::NSPrintPanelShowsPaperSize)
                    .union(NSPrintPanelOptions::NSPrintPanelShowsOrientation),
            );
        }

        // SAFETY: runModalWithPrintInfo: is the standard AppKit modal print
        // panel call; it blocks on the main runloop until the user dismisses
        // the panel and mutates the passed NSPrintInfo with the choices.
        let result = unsafe { panel.runModalWithPrintInfo(&print_info) };
        if result != NSPrintPanelResult::Printed.0 {
            return None;
        }

        // SAFETY: printInfo: returns the NSPrintInfo the panel was run with
        // (the authoritative post-run settings); paperSize:/orientation: are
        // plain property getters on it.
        let updated = unsafe { panel.printInfo() };
        let size = unsafe { updated.paperSize() };
        let orientation = unsafe { updated.orientation() };
        // SAFETY: dictionary: returns the print-info attribute dictionary;
        // objectForKey: reads the NSPrintCopies value, an NSNumber by
        // contract (the same shape setObject_forKey stored above).
        let copies = unsafe { updated.dictionary().objectForKey(NSPrintCopies) }
            .map(|value| {
                // SAFETY: the NSPrintCopies value is an NSNumber by contract.
                let number: Retained<NSNumber> = unsafe { Retained::cast(value) };
                number.as_u32()
            })
            .unwrap_or(1)
            .max(1);
        let width_mm = (size.width / POINTS_PER_MM).round().max(1.0) as u32;
        let height_mm = (size.height / POINTS_PER_MM).round().max(1.0) as u32;
        // Landscape swaps the reported paper dimensions on some systems;
        // normalize so width ≤ height like the DEVMODE convention.
        let paper_size_mm = if orientation == NSPaperOrientation::Landscape && width_mm < height_mm
        {
            (height_mm, width_mm)
        } else {
            (width_mm, height_mm)
        };

        let pick = wie_winapi::PrintDialogPick {
            paper_size_mm,
            orientation: if orientation == NSPaperOrientation::Landscape {
                DMORIENT_LANDSCAPE
            } else {
                DMORIENT_PORTRAIT
            },
            copies: u16::try_from(copies).unwrap_or(1),
            color: request.color.max(1),
            print_info_id: request.print_info_id,
        };
        // Register the NSPrintInfo for the EndDoc handoff (the handler stored
        // the same id on the print job).
        if let Ok(mut table) = table.lock() {
            table.insert(request.print_info_id, PrintInfoEntry(updated));
        }
        Some(pick)
    })
}

/// Enable interactive page-setup dialogs on a GUI session.
///
/// `PageSetupDlgW` then runs the native page-layout panel (when the bridge is
/// registered) instead of returning a scripted policy answer. Must run before
/// the guest executes — call it right after the session is created, before
/// `run_windowed`.
#[cfg(target_os = "macos")]
pub fn enable_interactive_page_setup_dialogs(session: &mut RuntimeSession) {
    session.set_page_setup_dialog_policy(wie_winapi::PageSetupDialogPolicy::Interactive);
}

/// Non-macOS builds have no native page-layout panel: keep the default Cancel
/// policy, so `PageSetupDlgW` returns FALSE exactly like a user canceling.
#[cfg(not(target_os = "macos"))]
pub fn enable_interactive_page_setup_dialogs(_session: &mut RuntimeSession) {}

/// Register the native macOS page-setup bridge on a GUI session.
///
/// With a bridge registered, `PageSetupDlgW` under
/// [`wie_winapi::PageSetupDialogPolicy::Interactive`] shows a real
/// NSPageLayout panel: the guest thread blocks inside the bridge until the
/// user dismisses it (dialog semantics, the same seam as the print-panel
/// bridge), then the handler writes the pick back into the `PAGESETUPDLG`.
/// The panel shares NO id-table with the print panel — the pick's settings
/// travel to the later `PrintDlgW` panel through the guest DEVMODE, which the
/// handler writes. Sessions that never register a bridge keep the default
/// Cancel (headless runs, `trace`).
#[cfg(target_os = "macos")]
pub fn register_native_page_setup_dialog_bridge(handle: &wie_runtime::GuestHandle) {
    handle.set_page_setup_dialog_bridge(Box::new(show_native_page_setup_dialog));
}

/// Show one native page-layout panel from the page-setup bridge callback.
///
/// Runs on the guest thread; [`objc2_foundation::run_on_main`] dispatches the
/// panel to the main thread's runloop (where AppKit's modal `runModal` is
/// valid) and blocks until the user dismisses it. NSPageLayout edits paper
/// size + orientation only — the margins have no panel control, so the guest
/// `rtMargin` passes through unchanged (the documented deviation). The
/// returned plain-data pick is written back into the guest `PAGESETUPDLG`.
#[cfg(target_os = "macos")]
#[expect(unsafe_code)]
fn show_native_page_setup_dialog(
    request: &wie_winapi::PageSetupDialogRequest,
) -> Option<wie_winapi::PageSetupDialogPick> {
    use objc2::ClassType;
    use objc2_app_kit::{NSPageLayout, NSPageLayoutResult, NSPaperOrientation, NSPrintInfo};
    use objc2_foundation::{NSSize, run_on_main};

    run_on_main(|mtm| {
        // A fresh NSPrintInfo per call — never the sharedPrintInfo singleton,
        // so one session's settings cannot leak into the next.
        // SAFETY: init: initializes the allocated object (the standard
        // alloc/init pair; the object is owned by the returned Retained).
        let print_info = unsafe { NSPrintInfo::init(NSPrintInfo::alloc()) };
        let (width_mm, height_mm) = request.paper_size_mm;
        // SAFETY: the two setters are plain property setters on the owned
        // object (the NSPrintInfo seed — paper size + orientation).
        unsafe {
            print_info.setPaperSize(NSSize::new(
                f64::from(width_mm) * POINTS_PER_MM,
                f64::from(height_mm) * POINTS_PER_MM,
            ));
            print_info.setOrientation(if request.orientation == DMORIENT_LANDSCAPE {
                NSPaperOrientation::Landscape
            } else {
                NSPaperOrientation::Portrait
            });
        }

        // SAFETY: pageLayout: is the standard AppKit constructor (the panel
        // is created on the main thread, which the marker proves).
        let layout = unsafe { NSPageLayout::pageLayout(mtm) };
        // SAFETY: runModalWithPrintInfo: is the standard AppKit modal page
        // layout call; it blocks on the main runloop until the user dismisses
        // the panel and mutates the passed NSPrintInfo with the choices.
        let result = unsafe { layout.runModalWithPrintInfo(&print_info) };
        if result != NSPageLayoutResult::Changed.0 {
            return None;
        }

        // SAFETY: printInfo: returns the NSPrintInfo the panel was run with
        // (the authoritative post-run settings); paperSize:/orientation: are
        // plain property getters on it.
        let updated = unsafe { layout.printInfo() }?;
        let size = unsafe { updated.paperSize() };
        let orientation = unsafe { updated.orientation() };
        let width_mm = (size.width / POINTS_PER_MM).round().max(1.0) as u32;
        let height_mm = (size.height / POINTS_PER_MM).round().max(1.0) as u32;
        // Landscape swaps the reported paper dimensions on some systems;
        // normalize so width ≤ height like the DEVMODE convention.
        let paper_size_mm = if orientation == NSPaperOrientation::Landscape && width_mm < height_mm
        {
            (height_mm, width_mm)
        } else {
            (width_mm, height_mm)
        };

        Some(wie_winapi::PageSetupDialogPick {
            paper_size_mm,
            orientation: if orientation == NSPaperOrientation::Landscape {
                DMORIENT_LANDSCAPE
            } else {
                DMORIENT_PORTRAIT
            },
        })
    })
}

/// Register the native print-OPERATION bridge on a GUI session.
///
/// With a bridge registered, `EndDoc` hands the completed pages to a real
/// macOS NSPrintOperation instead of writing the `WIE_PRINT_TO` BMP oracle:
/// the guest thread blocks inside the bridge until the operation finishes
/// (print semantics, the same seam as the print-dialog bridge), then the
/// `EndDoc` handler returns the operation's success flag. Sessions that never
/// register a bridge keep the BMP oracle (headless runs, `trace`). Shares the
/// [`PrintInfoTable`] with the print-dialog bridge — the operation consumes
/// the user's `NSPrintInfo` (printer/PDF destination + settings) from it.
#[cfg(target_os = "macos")]
pub fn register_native_print_job_bridge(handle: &wie_runtime::GuestHandle, table: PrintInfoTable) {
    handle.set_print_job_bridge(Box::new({
        move |request| run_native_print_job(request, &table)
    }));
}

/// Run one native print operation from the EndDoc bridge callback.
///
/// Runs on the guest thread; [`objc2_foundation::run_on_main`] dispatches the
/// whole operation to the main thread's runloop (AppKit print APIs are
/// main-thread-only) and blocks until it finishes — the same mechanism the
/// print panel uses. The completed pages MOVE into NSImages, a minimal
/// multi-page print view ([`PrintPagesView`]) presents them to the
/// `NSPrintOperation` seeded with the id-table's `NSPrintInfo` (the user's
/// printer/PDF destination + settings from `PrintDlgW`), and the operation
/// runs WITHOUT re-showing the print panel. The id-table entry is consumed
/// here (a leak-per-print would otherwise keep the settings forever).
#[cfg(target_os = "macos")]
#[expect(unsafe_code)]
fn run_native_print_job(request: wie_winapi::PrintJobRequest, table: &PrintInfoTable) -> bool {
    use objc2::ClassType;
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{
        NSBitmapFormat, NSBitmapImageRep, NSDeviceRGBColorSpace, NSImage, NSPrintCopies,
        NSPrintInfo, NSPrintOperation,
    };
    use objc2_foundation::{NSNumber, NSPoint, NSRect, NSSize, NSString, run_on_main};

    run_on_main(|mtm| {
        if request.pages.is_empty() {
            tracing::warn!("EndDoc: no pages to print");
            return false;
        }

        // Consume the id-table entry: the user's NSPrintInfo (printer / PDF
        // destination + settings) from the PrintDlgW panel. A DC created via
        // CreateDCW never went through the panel (print_info_id 0 → no entry):
        // fall back to a fresh NSPrintInfo so CreateDCW print jobs still print.
        let print_info_entry = {
            let mut table = table
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            table.remove(&request.print_info_id).map(|entry| entry.0)
        };
        let print_info = match print_info_entry {
            Some(info) => info,
            None => {
                // SAFETY: init: initializes the allocated object (the
                // standard alloc/init pair; the object is owned by the
                // returned Retained).
                let info = unsafe { NSPrintInfo::init(NSPrintInfo::alloc()) };
                // Seed the copies the guest requested. SAFETY: `NSPrintCopies`
                // is the documented key for the copies count and NSNumber the
                // documented value type (the same shape the panel bridge uses).
                unsafe {
                    info.dictionary().setObject_forKey(
                        &NSNumber::new_u32(request.copies.max(1)),
                        ProtocolObject::from_ref(NSPrintCopies),
                    );
                }
                info
            }
        };
        // The sheet size in points: the NSPrintInfo's paper (the user's pick,
        // or the system default for the fresh fallback). Each page band is one
        // sheet; the raster content fills it (fit-to-page).
        // SAFETY: paperSize: is a plain property getter on the owned object.
        let sheet = unsafe { print_info.paperSize() };
        let page_count = request.pages.len();

        // One NSImage per page: the 0RGB canvas pixels (MOVE — no copy) get
        // an opaque alpha byte in place, then land in an NSBitmapImageRep
        // whose format declares exactly that layout (see below), and the rep
        // is attached to an NSImage sized to the sheet.
        let mut images: Vec<Retained<NSImage>> = Vec::with_capacity(page_count);
        for page in request.pages {
            let width = usize::try_from(page.width).unwrap_or(0);
            let height = usize::try_from(page.height).unwrap_or(0);
            // SAFETY: the page was moved in, so the canvas is ours to mutate.
            // The 0RGB pixel becomes 0xAARRGGBB (opaque alpha).
            let mut pixels = page.pixels;
            for px in &mut pixels {
                *px |= 0xFF00_0000;
            }
            let Some(rep) = (unsafe {
                // SAFETY: init + the bitmap initializer (the standard
                // alloc/init pair). planes = NULL lets AppKit allocate the
                // pixel buffer; `bitmapData()` below returns it.
                NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
                    NSBitmapImageRep::alloc(),
                    std::ptr::null_mut(),
                    width as objc2_foundation::NSInteger,
                    height as objc2_foundation::NSInteger,
                    8,
                    4,
                    true,
                    false,
                    NSDeviceRGBColorSpace,
                    // ThirtyTwoBitLittleEndian + AlphaFirst: each pixel is
                    // a native 32-bit LE word 0xAARRGGBB — byte layout
                    // [B,G,R,A] — exactly the 0xFFRRGGBB words above, so
                    // the copy below is a straight memcpy.
                    NSBitmapFormat::ThirtyTwoBitLittleEndian
                        .union(NSBitmapFormat::AlphaFirst)
                        .union(NSBitmapFormat::AlphaNonpremultiplied),
                    width.saturating_mul(4) as objc2_foundation::NSInteger,
                    32,
                )
            }) else {
                tracing::error!("EndDoc: NSBitmapImageRep allocation failed");
                return false;
            };
            // SAFETY: AppKit allocated the buffer (planes NULL) and
            // bitmapData: returns it; the copy fills exactly the declared
            // width×height×4 bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    pixels.as_ptr().cast::<u8>(),
                    rep.bitmapData(),
                    width.saturating_mul(height).saturating_mul(4),
                );
            }
            // SAFETY: initWithSize: is the standard NSImage initializer; the
            // size (in points) is the sheet, so the rep scales 1:1 onto it.
            let image = unsafe { NSImage::initWithSize(NSImage::alloc(), sheet) };
            // SAFETY: addRepresentation: attaches the rep (auto-deref to
            // NSImageRep) to the owned image.
            unsafe { image.addRepresentation(&rep) };
            images.push(image);
        }

        let view = PrintPagesView::new(mtm, sheet, images);
        // SAFETY: setFrame: is a plain property setter on the owned view; the
        // frame stacks the page bands vertically (AppKit's default non-flipped
        // coordinates are bottom-up, so band i sits at y = i × sheet height).
        unsafe {
            view.setFrame(NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(sheet.width, sheet.height * (page_count as f64)),
            ));
        }

        // SAFETY: printOperationWithView:printInfo: is the standard AppKit
        // print constructor — it retains both the view and the print info for
        // the operation's lifetime.
        let operation =
            unsafe { NSPrintOperation::printOperationWithView_printInfo(&view, &print_info) };
        // SAFETY: setShowsPrintPanel: / setJobTitle: are plain property
        // setters. The panel already ran (PrintDlgW) — never re-show it.
        unsafe {
            operation.setShowsPrintPanel(false);
        }
        if !request.doc_name.is_empty() {
            // SAFETY: setJobTitle: is a plain property setter; the string is
            // retained by the operation.
            unsafe {
                let title = NSString::from_str(&request.doc_name);
                operation.setJobTitle(Some(&title));
            }
        }

        tracing::info!(
            target: "wiegui",
            pages = page_count,
            doc_name = request.doc_name,
            print_info_id = request.print_info_id,
            "EndDoc: native print operation running"
        );
        // SAFETY: runOperation: is the standard blocking print call; it
        // returns whether the print job completed successfully.
        unsafe { operation.runOperation() }
    })
}

/// One page (sheet) of the print document, for the print view's band layout.
///
/// Kept as a plain struct so the class ivars stay `Copy`-free and `Sized`:
/// the pages are stacked vertically in the (non-flipped) view, one sheet per
/// band, and `rectForPage:` returns the band for each page.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug)]
struct PrintSheet {
    /// The sheet size in points.
    size: NSSize,
}

/// The declared ivars of [`PrintPagesView`].
#[cfg(target_os = "macos")]
struct PrintPagesViewIvars {
    /// The sheet size in points (== the NSPrintInfo paper size).
    sheet: PrintSheet,
    /// One NSImage per page, in print order; page `i` is drawn into band `i`.
    images: Vec<Retained<NSImage>>,
}

// A minimal multi-page print view: one sheet-sized band per page.
//
// Implements the three `NSView`/`NSPrinting` hooks the print system needs:
// `knowsPageRange:` (1..N), `rectForPage:` (the page's band — pages are
// stacked vertically in the view, so each band is exactly the sheet rect),
// and `drawRect:` (draw page `i`'s image to fill the band). The frame is
// `N × sheet` tall; AppKit paginates it into one printed page per band.
#[cfg(target_os = "macos")]
declare_class!(
    struct PrintPagesView;

    // SAFETY:
    // - The superclass NSView has no subclassing requirements.
    // - MainThreadOnly matches the superclass's declared mutability.
    // - `PrintPagesView` does not implement `Drop`.
    unsafe impl ClassType for PrintPagesView {
        type Super = objc2_app_kit::NSView;
        type Mutability = objc2::mutability::MainThreadOnly;
        const NAME: &'static str = "WiePrintPagesView";
    }

    impl DeclaredClass for PrintPagesView {
        type Ivars = PrintPagesViewIvars;
    }

    #[expect(unsafe_code)]
    unsafe impl PrintPagesView {
        #[method(knowsPageRange:)]
        fn knows_page_range(&self, range: objc2_foundation::NSRangePointer) -> bool {
            // SAFETY: AppKit hands a valid pointer to the range it wants
            // filled; the length is the page count (1-indexed range).
            unsafe {
                *range = objc2_foundation::NSRange::new(1, self.ivars().images.len());
            }
            true
        }

        #[method(rectForPage:)]
        fn rect_for_page(&self, page: objc2_foundation::NSInteger) -> NSRect {
            let ivars = self.ivars();
            NSRect::new(
                NSPoint::new(0.0, (page - 1) as f64 * ivars.sheet.size.height),
                ivars.sheet.size,
            )
        }

        #[method(drawRect:)]
        fn draw_rect(&self, dirty_rect: NSRect) {
            let ivars = self.ivars();
            // The band whose y-range contains the dirty rect IS the page.
            let page = (dirty_rect.origin.y / ivars.sheet.size.height).floor() as usize;
            if let Some(image) = ivars.images.get(page) {
                // SAFETY: drawInRect: is a plain drawing call; self (and thus
                // the image) outlives the call — AppKit retains the view for
                // the whole print operation.
                unsafe {
                    image.drawInRect(NSRect::new(
                        NSPoint::new(0.0, dirty_rect.origin.y),
                        ivars.sheet.size,
                    ));
                }
            }
        }
    }
);

#[cfg(target_os = "macos")]
impl PrintPagesView {
    /// Allocate + initialize the print view with the given sheet and images.
    ///
    /// Runs on the main thread only (the caller passes its `MainThreadMarker`
    /// — the `MainThreadOnly` mutability forbids allocating on any other).
    #[expect(unsafe_code)]
    fn new(
        mtm: objc2_foundation::MainThreadMarker,
        sheet: NSSize,
        images: Vec<Retained<NSImage>>,
    ) -> Retained<Self> {
        use objc2::msg_send_id;
        let this = mtm.alloc::<Self>().set_ivars(PrintPagesViewIvars {
            sheet: PrintSheet { size: sheet },
            images,
        });
        // SAFETY: init is the standard NSView initializer (the canonical
        // alloc/init pair; the returned Retained owns the object). The frame
        // is set by the caller after construction.
        unsafe { msg_send_id![super(this), init] }
    }
}
