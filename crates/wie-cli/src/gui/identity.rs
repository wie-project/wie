//! The executable's own identity — its name and icon — in the macOS chrome.
//!
//! Windows executables describe themselves with resources: the shell shows
//! `FileDescription` under the icon taken from the first `RT_GROUP_ICON` group.
//! `wie run` presents a guest app in macOS chrome instead of a Windows shell,
//! so this module carries that identity across the same way:
//!
//! * **window title** — [`GuiIdentity::title_for`] prefixes the guest's
//!   own caption with the extracted name (`DOOM Retro — <caption>`), because
//!   the caption is the guest's document/window text and dropping it would
//!   lose information the guest deliberately published;
//! * **Dock tile** — [`GuiIdentity::apply_dock_icon`] hands the decoded icon to
//!   `-[NSApplication setApplicationIconImage:]`, the AppKit API for that
//!   chrome. It works for an unbundled CLI: the process has a Dock tile (winit
//!   runs a Regular-policy application), and the call replaces that tile's
//!   image for the lifetime of the process. Nothing is persisted — there is no
//!   bundle to write to — and the **menu-bar application name is not
//!   controllable**: AppKit takes it from `CFBundleName`/the process name, and
//!   `NSProcessInfo` has no setter. Renaming the first main-menu item is the
//!   only lever and it belongs to `muda`, not here.
//!
//! `wie-pe` does the resource reading and DIB decoding. A PNG-compressed icon
//! payload arrives still compressed and [`decode_png`] expands it here, in the
//! one crate that already depends on a PNG decoder (the `png` crate, via
//! `muda`) — so the shared PE crate stays free of an inflate implementation.

use std::path::Path;

use objc2::ClassType;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSApplication, NSBitmapFormat, NSBitmapImageRep, NSDeviceRGBColorSpace, NSImage,
};
use objc2_foundation::{MainThreadMarker, NSInteger, NSSize};
use wie_pe::identity::NameSource;
use wie_pe::resources::{IconData, RgbaImage};

/// The guest executable's identity, decoded for presentation.
#[derive(Debug, Clone)]
pub struct GuiIdentity {
    /// Human-readable application name (never empty).
    pub name: String,
    /// Which resource supplied [`Self::name`] (logged; `ExeStem` means the
    /// binary carried no usable version resource).
    pub name_source: NameSource,
    /// Decoded RGBA8 icon, when the executable carries a usable one.
    pub icon: Option<RgbaImage>,
}

impl GuiIdentity {
    /// Read the identity out of the PE at `path` (best-effort; see
    /// [`wie_pe::identity::app_identity_from_file`]).
    pub fn load(path: &Path) -> Self {
        let identity = wie_pe::identity::app_identity_from_file(path);
        let icon = match identity.icon {
            Some(IconData::Rgba(image)) => Some(image),
            Some(IconData::Png(bytes)) => {
                let decoded = decode_png(&bytes);
                if decoded.is_none() {
                    tracing::warn!(
                        target: "wiegui",
                        byte_len = bytes.len(),
                        "app icon: the PNG icon payload did not decode"
                    );
                }
                decoded
            }
            None => None,
        };
        tracing::info!(
            target: "wiegui",
            name = %identity.name,
            source = ?identity.name_source,
            icon = ?icon.as_ref().map(|image| (image.width, image.height)),
            "app identity from the executable's resources"
        );
        Self {
            name: identity.name,
            name_source: identity.name_source,
            icon,
        }
    }

    /// The native title for a guest top-level window.
    ///
    /// The extracted name prefixes the guest caption — `DOOM Retro — <caption>`
    /// — so the binary's identity is visible without hiding what the guest
    /// wrote there. A caption that already contains the name (case-insensitive)
    /// is left alone, which avoids `DOOM Retro — DOOM Retro` for apps that set
    /// their own name as the title; an empty caption yields the name alone.
    pub fn title_for(&self, guest_caption: &str) -> String {
        window_title(&self.name, guest_caption)
    }

    /// Set the Dock tile image from the executable's icon.
    ///
    /// Must run on the main thread with a live `NSApplication` (after the
    /// winit event loop has been built). A no-op when the executable carries
    /// no icon or the icon did not decode.
    #[expect(unsafe_code)]
    pub fn apply_dock_icon(&self) {
        let Some(image) = self.icon.as_ref() else {
            return;
        };
        let Some(mtm) = MainThreadMarker::new() else {
            tracing::warn!(
                target: "wiegui",
                "app icon: not on the main thread, keeping the default Dock tile"
            );
            return;
        };
        let Some(dock) = dock_icon(image) else {
            tracing::warn!(
                target: "wiegui",
                "app icon: building the NSImage failed, keeping the default Dock tile"
            );
            return;
        };
        // `sharedApplication` is only valid on the main thread, which the
        // `MainThreadMarker` above proves we are on.
        let application = NSApplication::sharedApplication(mtm);
        // SAFETY: `setApplicationIconImage:` is AppKit's documented setter for
        // the Dock tile image, and the image outlives the call.
        unsafe {
            application.setApplicationIconImage(Some(&dock.image));
        }
        // Read back what AppKit now reports as the application icon. AppKit
        // COPIES the image into the app (pointer identity is not preserved), so
        // the check is geometric: an echoed icon carries our size and our rep
        // dimensions. The pixel content of the image handed over is asserted by
        // `dock_icon_carries_the_decoded_pixels`.
        let read_back = unsafe { application.applicationIconImage() };
        let probe = read_back
            .as_ref()
            .and_then(|current| icon_geometry(current));
        // The rep geometry of the image we handed over — the pixels the Dock
        // samples if the setter took it.
        // SAFETY: plain property getters on the owned rep.
        let (applied_width, applied_height) =
            unsafe { (dock.rep.pixelsWide(), dock.rep.pixelsHigh()) };
        let expected = (f64::from(image.width), f64::from(image.height));
        match probe {
            Some((width, height, rep_width, rep_height))
                if width == expected.0 && height == expected.1 =>
            {
                tracing::info!(
                    target: "wiegui",
                    width,
                    height,
                    applied_width,
                    applied_height,
                    read_back_width = width,
                    read_back_height = height,
                    read_back_rep_width = rep_width,
                    read_back_rep_height = rep_height,
                    "Dock tile set from the executable's icon and read back at the icon's size"
                );
            }
            other => {
                tracing::warn!(
                    target: "wiegui",
                    expected_width = expected.0,
                    expected_height = expected.1,
                    applied_width,
                    applied_height,
                    read_back = ?other,
                    "app icon: the application icon did not read back at the icon's size"
                );
            }
        }
    }
}

/// The title rule for a guest window: see [`GuiIdentity::title_for`].
#[must_use]
pub fn window_title(app_name: &str, guest_caption: &str) -> String {
    let name = app_name.trim();
    let caption = guest_caption.trim();
    if name.is_empty() {
        return caption.to_owned();
    }
    if caption.is_empty() {
        return name.to_owned();
    }
    if caption.to_lowercase().contains(&name.to_lowercase()) {
        return caption.to_owned();
    }
    format!("{name} — {caption}")
}

/// An `NSImage` built from the decoded icon, with its backing rep kept so the
/// pixels can be probed (the tests and the read-back in `apply_dock_icon`).
struct DockIcon {
    image: Retained<NSImage>,
    rep: Retained<NSBitmapImageRep>,
}

/// Build an `NSImage` from non-premultiplied RGBA8 pixels.
///
/// The rep's format declares exactly the buffer layout: 8-bit samples, four
/// samples per pixel, alpha last (no `AlphaFirst`), straight (not
/// premultiplied) — so the byte copy is a straight memcpy and the Dock can
/// scale the tile as it likes.
#[expect(unsafe_code)]
fn dock_icon(image: &RgbaImage) -> Option<DockIcon> {
    let width = NSInteger::try_from(image.width).ok()?;
    let height = NSInteger::try_from(image.height).ok()?;
    let bytes_per_row = width.checked_mul(4)?;
    let pixel_bytes = image.pixels.len();
    // SAFETY: the standard alloc/init pair for NSBitmapImageRep; `planes` NULL
    // lets AppKit allocate the pixel buffer, and the declared geometry matches
    // the RGBA8 buffer copied into it below.
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            width,
            height,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            NSBitmapFormat::AlphaNonpremultiplied,
            bytes_per_row,
            32,
        )
    }?;
    // SAFETY: AppKit allocated the buffer (planes NULL) and `bitmapData`
    // returns it; the copy fills exactly width×height×4 bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(
            image.pixels.as_ptr().cast::<u8>(),
            rep.bitmapData(),
            pixel_bytes,
        );
    }
    let size = NSSize::new(f64::from(image.width), f64::from(image.height));
    // SAFETY: `initWithSize:` is the standard NSImage initializer; the rep is
    // attached to the owned image before it is returned.
    let ns_image = unsafe { NSImage::initWithSize(NSImage::alloc(), size) };
    // SAFETY: `addRepresentation:` attaches the owned rep (auto-deref to
    // NSImageRep) to the owned image.
    unsafe { ns_image.addRepresentation(&rep) };
    Some(DockIcon {
        image: ns_image,
        rep,
    })
}

/// `(points_width, points_height, rep_pixels_wide, rep_pixels_high)` of an
/// `NSImage`, read from the image and its first representation.
///
/// This is how `apply_dock_icon` checks that `-[NSApplication
/// setApplicationIconImage:]` took the image: AppKit hands back a copy, so the
/// comparison is by geometry, not by pointer.
#[must_use]
#[expect(unsafe_code)]
fn icon_geometry(image: &NSImage) -> Option<(f64, f64, isize, isize)> {
    // SAFETY: plain property getters on an owned image and its own
    // representation array.
    let size = unsafe { image.size() };
    let reps = unsafe { image.representations() };
    let rep = reps.first()?;
    let wide = unsafe { rep.pixelsWide() };
    let high = unsafe { rep.pixelsHigh() };
    Some((size.width, size.height, wide, high))
}

/// Decode a PNG stream to non-premultiplied RGBA8.
///
/// `EXPAND` turns palette and sub-byte grayscale images into plain RGB(A)
/// samples, `STRIP_16` narrows 16-bit samples to 8, and the remaining colour
/// types are widened here — so the caller always gets the one layout AppKit's
/// `NSBitmapImageRep` is told to expect.
#[must_use]
fn decode_png(bytes: &[u8]) -> Option<RgbaImage> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut buffer = vec![0_u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buffer).ok()?;
    if info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    let data = buffer.get(..info.buffer_size())?;
    let pixels = match info.color_type {
        png::ColorType::Rgba => data.to_vec(),
        png::ColorType::Rgb => rgb_to_rgba(data)?,
        png::ColorType::Grayscale => gray_to_rgba(data, 1)?,
        png::ColorType::GrayscaleAlpha => gray_to_rgba(data, 2)?,
        // `EXPAND` removes the palette; an indexed output here means the
        // transform did not run, so refuse rather than emit garbage.
        png::ColorType::Indexed => return None,
    };
    Some(RgbaImage {
        width: info.width,
        height: info.height,
        pixels,
    })
}

/// Widen RGB8 to RGBA8 with an opaque alpha.
fn rgb_to_rgba(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() / 3 * 4);
    let (pixels, _) = data.as_chunks::<3>();
    for pixel in pixels {
        let [red, green, blue] = *pixel;
        out.extend_from_slice(&[red, green, blue, 0xFF]);
    }
    Some(out)
}

/// Widen grayscale (one or two channels) to RGBA8.
fn gray_to_rgba(data: &[u8], channels: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() / channels * 4);
    for pixel in data.chunks_exact(channels) {
        let level = *pixel.first()?;
        let alpha = if channels == 2 { *pixel.get(1)? } else { 0xFF };
        out.extend_from_slice(&[level, level, level, alpha]);
    }
    Some(out)
}

/// The identity tests: the title rule, the PNG decode, and the real binaries.
#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// The local `real_exes/` fixtures are not tracked by git; skip when the
    /// checkout does not carry them.
    fn real_exe(name: &str) -> Option<std::path::PathBuf> {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../real_exes")
            .join(name);
        path.is_file().then_some(path)
    }

    /// Encode RGBA8 pixels as a PNG (the format a 256 px RT_ICON entry uses).
    fn encode_png(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("png header");
            writer.write_image_data(pixels).expect("png pixels");
            writer.finish().expect("png finish");
        }
        out
    }

    #[test]
    fn window_title_prefixes_the_guest_caption() {
        assert_eq!(
            window_title("DOOM Retro", "Doom Retro - E1M1"),
            "Doom Retro - E1M1",
            "a caption that already names the app is left alone"
        );
        assert_eq!(
            window_title("ReactOS Notepad", "Untitled - Notepad"),
            "ReactOS Notepad — Untitled - Notepad"
        );
        assert_eq!(window_title("DOOM Retro", ""), "DOOM Retro");
        assert_eq!(window_title("DOOM Retro", "   "), "DOOM Retro");
        assert_eq!(window_title("", "Untitled"), "Untitled");
        assert_eq!(window_title("  ", ""), "");
        assert_eq!(
            window_title("Editor", "editor — README.txt"),
            "editor — README.txt",
            "the name match is case-insensitive"
        );
    }

    #[test]
    #[expect(unsafe_code)]
    fn dock_icon_carries_the_decoded_pixels() {
        // The exact buffer handed to `setApplicationIconImage:`: two pixels
        // apart so a wrong channel order or stride would fail the compare.
        let image = RgbaImage {
            width: 2,
            height: 2,
            pixels: vec![
                255, 0, 0, 255, // red, opaque
                0, 255, 0, 128, // green, half transparent
                0, 0, 255, 0, // blue, fully transparent
                10, 20, 30, 255, // arbitrary
            ],
        };
        let dock = dock_icon(&image).expect("the NSImage builds");
        // The image's size in points is the icon's pixel size (the Dock scales
        // the tile itself).
        let size = unsafe { dock.image.size() };
        assert_eq!((size.width, size.height), (2.0, 2.0));
        // The backing rep has the same geometry and the same bytes we decoded
        // — including the alpha channel and the channel order.
        let wide = unsafe { dock.rep.pixelsWide() };
        let high = unsafe { dock.rep.pixelsHigh() };
        assert_eq!((wide, high), (2, 2));
        let data = unsafe { dock.rep.bitmapData() };
        assert!(!data.is_null(), "AppKit allocated the pixel buffer");
        // SAFETY: the rep declares 2x2x4 bytes, which the assertion below
        // reads in full.
        let pixels = unsafe { std::slice::from_raw_parts(data, image.pixels.len()) };
        assert_eq!(pixels, image.pixels.as_slice());
    }

    #[test]
    fn decode_png_expands_to_rgba8() {
        let pixels = vec![
            255, 0, 0, 255, // red
            0, 255, 0, 128, // half-transparent green
            0, 0, 255, 0, // fully transparent blue
            10, 20, 30, 255, // arbitrary
        ];
        let png = encode_png(2, 2, &pixels);
        let decoded = decode_png(&png).expect("decodes");
        assert_eq!((decoded.width, decoded.height), (2, 2));
        assert_eq!(decoded.pixels, pixels);
        assert!(!decoded.is_uniform());
        // A truncated stream decodes to None, never a panic.
        for cut in 0..png.len() {
            let _decoded = decode_png(&png[..cut]);
        }
        assert!(decode_png(&[]).is_none());
    }

    #[test]
    fn doomretro_identity_is_read_from_the_binary() {
        let Some(path) = real_exe("doomretro/doomretro.exe") else {
            return;
        };
        let identity = GuiIdentity::load(&path);
        assert_eq!(identity.name, "DOOM Retro");
        assert_eq!(identity.name_source, NameSource::FileDescription);
        let icon = identity.icon.as_ref().expect("the 256 px PNG icon decodes");
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.pixels.len(), 256 * 256 * 4);
        assert!(!icon.is_uniform(), "real icon pixels, not a blank block");
        assert_eq!(identity.title_for("Doom Retro - E1M1"), "Doom Retro - E1M1");
    }

    #[test]
    fn notepad_identity_is_read_from_the_binary() {
        let Some(path) = real_exe("notepad.exe") else {
            return;
        };
        let identity = GuiIdentity::load(&path);
        assert_eq!(identity.name, "ReactOS Notepad");
        assert_eq!(
            identity.title_for("Untitled - Notepad"),
            "ReactOS Notepad — Untitled - Notepad"
        );
        let icon = identity.icon.as_ref().expect("a DIB icon");
        assert_eq!((icon.width, icon.height), (48, 48));
        assert!(!icon.is_uniform());
    }

    #[test]
    fn a_missing_or_truncated_binary_falls_back_to_the_stem() {
        let identity = GuiIdentity::load(Path::new("/nonexistent/wie-missing.exe"));
        assert_eq!(identity.name, "wie-missing");
        assert_eq!(identity.name_source, NameSource::ExeStem);
        assert!(identity.icon.is_none());

        // A file that exists but is not a PE: headers do not parse, so no
        // resources — the name is still usable.
        let mut path = std::env::temp_dir();
        path.push(format!("wie-identity-{}.exe", std::process::id()));
        std::fs::write(&path, b"MZ not a PE image, only DOS junk").expect("write temp exe");
        let identity = GuiIdentity::load(&path);
        let _removed = std::fs::remove_file(&path);
        assert_eq!(identity.name_source, NameSource::ExeStem);
        assert!(
            identity.name.starts_with("wie-identity-"),
            "the temp file's stem: {}",
            identity.name
        );
        assert!(identity.icon.is_none());
    }
}
