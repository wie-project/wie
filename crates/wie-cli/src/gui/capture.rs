//! Capture the Nth published frame in the `--gui` path.
//!
//! # Why this exists
//!
//! `--screenshot` runs the guest headless and returns when the guest reaches
//! `WaitingForMessage` after its first paint. A game loop never does: DOOM
//! Retro ran to a 120 s cap and wrote no BMP, so the one end-to-end pixel test
//! that matters — "does a host-accelerated blit put the same bytes on screen as
//! the guest's own SDL code?" — had no harness at all. The unit tests can only
//! cover the shapes we thought of; this covers the path the guest actually
//! takes.
//!
//! So: run the real windowed path, and at the Nth *distinct* published frame
//! dump the guest's own buffer with the same writer `--screenshot` uses, then
//! stop the event loop. Two runs of the same build — acceleration on and off —
//! produce two BMPs that must be byte-identical.
//!
//! # Why the capture point is the presenter's redraw, not the publish wake
//!
//! A publish wake does not guarantee a frame is in the slot yet: the D3D9
//! capture streamer wakes the event loop *before* its replay publishes, so a
//! capture hung off the wake sees an empty slot and would either miss the frame
//! or busy-poll for it. The `RedrawRequested` path is where the presenter has
//! actually taken the frame it is about to put on screen — the same frame, the
//! same per-window `take_frame`, one call before the GPU sees it.
//!
//! # Why the count is by buffer identity, not by redraw
//!
//! A window can be asked to redraw more often than it republishes (a resize, a
//! second window's publish waking this one, a skipped present's retry). The
//! presenter already distinguishes a genuinely new frame with `Arc::ptr_eq` on
//! the pixel buffer; this uses the same test, so "frame N" means the same thing
//! in both places.

use std::path::PathBuf;
use std::sync::Arc;

use crate::bmp::write_bmp;

/// Writes the Nth published frame to a BMP and reports when it is done.
pub struct FrameCapture {
    /// Where to write the BMP.
    path: PathBuf,
    /// 1-based published-frame index to capture (clamped to 1).
    target: u64,
    /// Distinct publishes observed so far.
    seen: u64,
    /// Pixel buffer of the last distinct publish, for the identity test.
    last: Option<Arc<Vec<u32>>>,
    /// Set once the file has been attempted, so a later redraw cannot rewrite
    /// it.
    written: bool,
}

impl FrameCapture {
    /// Capture published frame `target` (1-based; `0` is treated as `1`).
    pub fn new(path: PathBuf, target: u64) -> Self {
        Self {
            path,
            target: target.max(1),
            seen: 0,
            last: None,
            written: false,
        }
    }

    /// Observe one frame the presenter is about to draw.
    ///
    /// Returns `true` when this call wrote the file — the caller should stop
    /// the event loop. A failed write also returns `true`: a capture that
    /// cannot be written must not leave the process running to its timeout.
    pub fn observe_frame(
        &mut self,
        width: u32,
        height: u32,
        stride: u32,
        pixels: &Arc<Vec<u32>>,
    ) -> bool {
        if self.written || !self.count(pixels) {
            return false;
        }
        self.written = true;
        match std::fs::File::create(&self.path)
            .and_then(|file| write_bmp(file, width, height, stride, pixels))
        {
            Ok(()) => tracing::info!(
                target: "wiegui",
                "captured published frame {} → {} ({width}x{height})",
                self.seen,
                self.path.display()
            ),
            Err(error) => tracing::error!(
                target: "wiegui",
                "capture of published frame {} failed: {error}",
                self.seen
            ),
        }
        true
    }

    /// Count one published frame by buffer identity; `true` at the target.
    fn count(&mut self, pixels: &Arc<Vec<u32>>) -> bool {
        if self
            .last
            .as_ref()
            .is_some_and(|last| Arc::ptr_eq(last, pixels))
        {
            return false;
        }
        self.seen = self.seen.saturating_add(1);
        self.last = Some(Arc::clone(pixels));
        self.seen == self.target
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The identity test is what makes "frame N" mean one published frame
    /// rather than one redraw — without it a resize or a neighbouring window's
    /// wake double-counts and the capture fires on the wrong picture.
    #[test]
    fn a_publish_is_counted_once_per_buffer() {
        let mut capture = FrameCapture::new(PathBuf::from("/dev/null"), 2);
        let first = Arc::new(vec![1_u32, 2, 3]);
        let second = Arc::new(vec![4_u32, 5, 6]);
        // Three redraws over the same buffer: one publish.
        assert!(
            !capture.count(&first),
            "the first publish is not the target"
        );
        assert!(!capture.count(&first), "a re-wake must not count again");
        assert!(!capture.count(&first));
        assert_eq!(capture.seen, 1, "three redraws, one published frame");
        // The second publish is the target.
        assert!(capture.count(&second), "the second publish is the target");
        assert_eq!(capture.seen, 2);
        // A publish after the target keeps counting but never re-fires.
        assert!(!capture.count(&Arc::new(vec![7_u32])));
        assert_eq!(capture.seen, 3);
    }

    /// `--screenshot-on-frame 0` is nonsense; clamping it to 1 makes the flag
    /// mean "the first frame" instead of never firing.
    #[test]
    fn a_zero_frame_index_means_the_first_frame() {
        let mut capture = FrameCapture::new(PathBuf::from("/dev/null"), 0);
        assert!(capture.count(&Arc::new(vec![0_u32])));
        assert_eq!(capture.target, 1);
    }

    /// The BMP the harness writes is the frame the presenter drew: same
    /// dimensions, rows bottom-up, 0RGB → BGRA. This is the file the two
    /// accelerator settings are compared byte for byte, so its encoding is part
    /// of the test, not an implementation detail.
    #[test]
    fn the_capture_writes_the_frame_the_presenter_drew() {
        let dir = std::env::temp_dir().join(format!("wie-capture-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("frame.bmp");
        // 2x2 frame: top row red, green; bottom row blue, white.
        let pixels: Arc<Vec<u32>> =
            Arc::new(vec![0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0x00ff_ffff]);
        let mut capture = FrameCapture::new(path.clone(), 1);
        assert!(capture.observe_frame(2, 2, 2, &pixels));
        let bytes = std::fs::read(&path).expect("bmp written");
        std::fs::remove_dir_all(&dir).ok();
        // 14-byte file header + 40-byte info header, then bottom-up BGRA.
        assert_eq!(&bytes[0..2], b"BM");
        assert_eq!(
            u32::from_le_bytes([bytes[18], bytes[19], bytes[20], bytes[21]]),
            2
        );
        assert_eq!(
            u32::from_le_bytes([bytes[22], bytes[23], bytes[24], bytes[25]]),
            2
        );
        assert_eq!(bytes.len(), 54 + 16, "header + 2x2 x 4 bytes");
        let body = &bytes[54..];
        // Bottom row first: blue (B=0xff, G=0, R=0), then white.
        assert_eq!(&body[0..4], &[0xff, 0x00, 0x00, 0xff]);
        assert_eq!(&body[4..8], &[0xff, 0xff, 0xff, 0xff]);
        // Then the top row: red, green.
        assert_eq!(&body[8..12], &[0x00, 0x00, 0xff, 0xff]);
        assert_eq!(&body[12..16], &[0x00, 0xff, 0x00, 0xff]);
    }
}
