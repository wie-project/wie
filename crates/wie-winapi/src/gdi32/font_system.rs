//! Real macOS system-font engine: Win32 font selection → fontdb → ab_glyph.
//!
//! Replaces the embedded 8×16 VGA bitmap font with proportional system fonts.
//! `CreateFont*` records a face name + weight + italic + height; the engine
//! maps that to a fontdb face, rasterizes glyphs with ab_glyph, and exposes px
//! metrics that every metric API shares — same font, same px scale, same
//! per-glyph advances — so `GetTextExtentPoint32` / `GetTextMetricsA` /
//! `DrawText` DT_CALCRECT always agree with the rasterizer.
//!
//! `fontdb::Database` is not `Clone`, so the (read-only) system-font scan
//! lives in one process-wide `OnceLock`; the per-session caches (loaded
//! faces, resolved metrics, per-codepoint fallbacks) stay in the session-owned
//! [`FontEngine`] on `GdiState`, matching the codebase's session-state style.

use ahash::HashMap;
use std::sync::OnceLock;

use ab_glyph::{Font, FontArc, FontVec, Glyph, GlyphId, Point, PxScale, PxScaleFont, ScaleFont};
use fontdb::{Family, Query, Stretch, Style, Weight};

/// Process-wide system font database (read-only after init).
static SYSTEM_FONT_DB: OnceLock<fontdb::Database> = OnceLock::new();

fn system_font_db() -> &'static fontdb::Database {
    SYSTEM_FONT_DB.get_or_init(|| {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        db
    })
}

/// Fallback faces tried (in order) when the primary face lacks a glyph
/// (e.g. CJK in a Latin-only primary). Each is checked for the codepoint.
const FALLBACK_FAMILIES: &[&str] = &[
    "PingFang SC",
    "Hiragino Sans GB",
    "Arial Unicode MS",
    "Menlo",
];

/// Cap on the rasterized-glyph bitmap cache (entries per
/// (font key, pixel height, codepoint)).
///
/// A text-heavy guest (file lists, logs, dialogs) re-draws the same glyphs
/// every frame, so the coverage bitmaps are worth caching — but a hostile or
/// odd guest could cycle an unbounded number of codepoints × heights. On
/// overflow the WHOLE glyph cache is dropped (the face and resolved-metric
/// caches stay warm; the next render re-rasterizes each glyph once). At ~1 KB
/// per 24 px glyph the cap bounds the cache to a few MB.
const GLYPH_CACHE_CAP: usize = 4096;

/// A font identity for caching: lowercase family + weight + italic.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FontKey {
    /// Lowercased Win32 `lfFaceName` ("" = system default).
    pub family: String,
    /// Resolved weight: 400 or 700 (real bold or fake-bold emulation).
    pub weight: u16,
    /// Italic requested.
    pub italic: bool,
}

impl Default for FontKey {
    fn default() -> Self {
        Self {
            family: String::new(),
            weight: 400,
            italic: false,
        }
    }
}

/// A font family selection: a fontdb generic or an exact face name.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FamilySelection {
    Generic(Family<'static>),
    Named(String),
}

/// Map a Win32 `lfFaceName` to a fontdb family selection.
///
/// Win32 "system" faces and the classic dialog fonts map to the generic
/// sans-serif family; the legacy terminal faces to monospace; Times to serif.
/// Anything else is tried as an exact face name (with a sans-serif fallback
/// at query time).
#[must_use]
fn family_selection_for(face_name: &str) -> FamilySelection {
    match face_name.to_ascii_lowercase().as_str() {
        "" | "ms shell dlg" | "ms shell dlg 2" | "tahoma" | "segoe ui" | "system" => {
            FamilySelection::Generic(Family::SansSerif)
        }
        "courier new" | "terminal" | "fixedsys" | "consolas" => {
            FamilySelection::Generic(Family::Monospace)
        }
        "times new roman" | "times" => FamilySelection::Generic(Family::Serif),
        other => FamilySelection::Named(other.to_owned()),
    }
}

/// Map a Win32 `lfWeight` to a fontdb weight.
///
/// `>= 600` requests bold (700); anything else is normal (400). Bold is
/// emulated by a second rasterization pass when no real bold face exists.
#[must_use]
pub(crate) fn fontdb_weight_for(lf_weight: i32) -> u16 {
    if lf_weight >= 600 { 700 } else { 400 }
}

/// Map a raw `lfHeight` to a target pixel height.
///
/// `lfHeight < 0` is a character height (|lfHeight| px); `lfHeight > 0` is a
/// cell height (approximated as |lfHeight| px); `lfHeight == 0` uses the
/// 16 px default.
#[must_use]
pub(crate) fn height_px_from_lf(lf_height: i32) -> i32 {
    if lf_height == 0 {
        16
    } else {
        i32::try_from(lf_height.unsigned_abs()).unwrap_or(16).max(1)
    }
}

/// Find the best face id for a selection, tracking which requested attributes
/// had to fall back (so the rasterizer can emulate bold/italic).
///
/// Returns `(face_id, fake_bold, fake_italic)`. Weight falls back 700 → 400;
/// style falls back Italic → Oblique → Normal.
fn face_id_for(
    selection: &FamilySelection,
    weight: u16,
    italic: bool,
) -> Option<(fontdb::ID, bool, bool)> {
    let db = system_font_db();
    let families: Vec<Family<'_>> = match selection {
        FamilySelection::Generic(family) => vec![*family],
        FamilySelection::Named(name) => {
            // Exact name first; a generic sans-serif is the last resort.
            vec![Family::Name(name), Family::SansSerif]
        }
    };
    let weights: &[u16] = if weight >= 600 { &[700, 400] } else { &[400] };
    let styles: &[Style] = if italic {
        &[Style::Italic, Style::Oblique, Style::Normal]
    } else {
        &[Style::Normal]
    };
    for &w in weights {
        for &style in styles {
            let query = Query {
                families: &families,
                weight: Weight(w),
                stretch: Stretch::Normal,
                style,
            };
            if let Some(id) = db.query(&query) {
                return Some((id, w != weight, italic && style == Style::Normal));
            }
        }
    }
    None
}

/// Load a face into an owned `FontArc` (honoring the face index for `.ttc`).
fn load_face(id: fontdb::ID) -> Option<FontArc> {
    let db = system_font_db();
    db.with_face_data(id, |data, index| {
        FontVec::try_from_vec_and_index(data.to_vec(), index)
            .ok()
            .map(FontArc::from)
    })
    .flatten()
}

/// Round a px advance to an integer (the unit all metric APIs report).
fn round_px(value: f32) -> i32 {
    value.round() as i32
}

/// A rasterized glyph: coverage buffer plus its placement relative to the
/// pen origin and the baseline.
#[derive(Debug, Clone)]
pub struct RasterizedGlyph {
    /// Horizontal pen advance (px).
    pub advance: i32,
    /// Left column of the coverage bbox, relative to the pen x.
    pub left: i32,
    /// Top row of the coverage bbox, relative to the baseline y.
    pub top: i32,
    /// Coverage buffer width (px).
    pub width: i32,
    /// Coverage buffer height (px).
    pub height: i32,
    /// Per-pixel coverage, 0..=255 alpha, row-major (`width * height`).
    pub coverage: Vec<u8>,
}

impl RasterizedGlyph {
    /// An empty glyph (space or missing): advance only, no pixels.
    fn empty(advance: i32) -> Self {
        Self {
            advance,
            left: 0,
            top: 0,
            width: 0,
            height: 0,
            coverage: Vec::new(),
        }
    }
}

/// Rasterize a glyph at the origin into a coverage buffer.
///
/// `#[expect(casts)]`: the glyph bbox is float-px; the coverage buffer is
/// indexed by the integer pixel bounds (clamped to the bbox, so truncation is
/// bounded and sign is preserved by the clamp).
fn outline_glyph(scaled: &PxScaleFont<&FontArc>, gid: GlyphId) -> Option<RasterizedGlyph> {
    let positioned = scaled.outline_glyph(Glyph {
        id: gid,
        scale: scaled.scale(),
        position: Point { x: 0.0, y: 0.0 },
    })?;
    let bounds = positioned.px_bounds();
    // The pixel bounds are already floored/ceiled integers in the glyph's
    // coordinate space (position 0,0 = the pen/baseline origin).
    let left = bounds.min.x.floor() as i32;
    let top = bounds.min.y.floor() as i32;
    let right = bounds.max.x.ceil() as i32;
    let bottom = bounds.max.y.ceil() as i32;
    let width = right.saturating_sub(left).max(0);
    let height = bottom.saturating_sub(top).max(0);
    let width_us = usize::try_from(width).unwrap_or(0);
    let height_us = usize::try_from(height).unwrap_or(0);
    let mut coverage = vec![0_u8; width_us.saturating_mul(height_us)];
    let advance = round_px(scaled.h_advance(gid));
    // `draw` iterates the rasterizer grid: (x, y) are 0-based within the
    // bounds, so the coverage index is `y * width + x` directly.
    positioned.draw(|x, y, cov| {
        let cx = i32::try_from(x).unwrap_or(0);
        let cy = i32::try_from(y).unwrap_or(0);
        if cx >= 0 && cy >= 0 && cx < width && cy < height {
            let index = usize::try_from(cy)
                .unwrap_or(0)
                .saturating_mul(width_us)
                .saturating_add(usize::try_from(cx).unwrap_or(0));
            if let Some(slot) = coverage.get_mut(index) {
                let alpha = (cov.clamp(0.0, 1.0) * 255.0).round() as u8;
                *slot = alpha.max(*slot);
            }
        }
    });
    Some(RasterizedGlyph {
        advance,
        left,
        top,
        width,
        height,
        coverage,
    })
}

/// A resolved font at a fixed pixel height: the loaded face plus px metrics.
///
/// Cheap to clone (`FontArc` is an `Arc`); the scaled view is rebuilt on
/// demand via [`ResolvedFont::scaled`].
#[derive(Debug, Clone)]
pub struct ResolvedFont {
    font: FontArc,
    /// The pixel height this font was resolved at (the glyph-cache key part).
    pub height_px: i32,
    /// Px per em — the scale that makes the line height (`ascent+descent`)
    /// equal the requested height. Also used for `tmInternalLeading`.
    pub scale: f32,
    /// Ascent in px.
    pub ascent: f32,
    /// Descent in px.
    pub descent: f32,
    /// Line gap in px (exposed as `tmExternalLeading`).
    pub line_gap: f32,
    /// Average advance (px) of `x` and `m`.
    pub avg_advance: i32,
    /// Maximum advance (px) over printable ASCII.
    pub max_advance: i32,
    /// Bold requested but only a regular face exists — emulate in the
    /// rasterizer with a +1 px second pass.
    pub fake_bold: bool,
    /// Italic requested but only a normal face exists — synthetic slant is
    /// NOT emulated (real italic faces cover almost everything).
    pub fake_italic: bool,
}

impl ResolvedFont {
    /// The scaled view of the font (cheap to construct).
    pub(crate) fn scaled(&self) -> PxScaleFont<&FontArc> {
        self.font.as_scaled(PxScale::from(self.scale))
    }

    /// Line height in px: ascent + |descent| (ab_glyph's descent is negative
    /// — below the baseline). One value used by every line based API;
    /// `line_gap` is exposed separately as external leading.
    pub(crate) fn line_height(&self) -> i32 {
        round_px(self.ascent).saturating_sub(round_px(self.descent))
    }
}

/// Per-session font engine: caches only (the database is process-global).
#[derive(Debug, Clone, Default)]
pub struct FontEngine {
    /// Loaded face + fallback flags per [`FontKey`].
    font_cache: HashMap<FontKey, (FontArc, bool, bool)>,
    /// Resolved metrics per (key, pixel height).
    resolved_cache: HashMap<(FontKey, i32), ResolvedFont>,
    /// Per-(key, codepoint) fallback face (`None` = no fallback has it).
    fallback_cache: HashMap<(FontKey, char), Option<FontArc>>,
    /// Rasterized glyph bitmaps per (key, pixel height, codepoint), capped at
    /// [`GLYPH_CACHE_CAP`]. Keyed like `resolved_cache` plus the codepoint so
    /// repeated characters in text-heavy apps blit instead of re-rasterizing.
    glyph_cache: HashMap<(FontKey, i32, char), RasterizedGlyph>,
}

impl FontEngine {
    /// Resolve (and cache) a font for `key` at `height_px`.
    ///
    /// Returns a clone so callers can pass it alongside `&mut self`.
    pub fn resolve(&mut self, key: &FontKey, height_px: i32) -> Option<ResolvedFont> {
        let cache_key = (key.clone(), height_px);
        if let Some(resolved) = self.resolved_cache.get(&cache_key) {
            return Some(resolved.clone());
        }
        if let Some((font, fake_bold, fake_italic)) = self.font_cache.get(key) {
            let resolved = build_resolved(font, height_px, *fake_bold, *fake_italic);
            self.resolved_cache.insert(cache_key, resolved.clone());
            return Some(resolved);
        }
        let selection = family_selection_for(&key.family);
        let (id, fake_bold, fake_italic) = face_id_for(&selection, key.weight, key.italic)?;
        let font = load_face(id)?;
        self.font_cache
            .insert(key.clone(), (font.clone(), fake_bold, fake_italic));
        let resolved = build_resolved(&font, height_px, fake_bold, fake_italic);
        self.resolved_cache.insert(cache_key, resolved.clone());
        Some(resolved)
    }

    /// Per-char advance with the same fallback resolution the rasterizer
    /// uses — the metric APIs must agree with the drawn pixels exactly.
    ///
    /// Reads the cached glyph's advance when the bitmap is already cached
    /// (never rasterizes just to measure); otherwise computes the advance
    /// directly from the face's h_advance — byte-identical to what
    /// [`FontEngine::rasterize`] stores.
    pub(crate) fn char_advance(&mut self, resolved: &ResolvedFont, key: &FontKey, ch: char) -> i32 {
        let cache_key = (key.clone(), resolved.height_px, ch);
        if let Some(glyph) = self.glyph_cache.get(&cache_key) {
            return glyph.advance;
        }
        let scaled = resolved.scaled();
        let gid = scaled.glyph_id(ch);
        if gid.0 != 0 {
            return round_px(scaled.h_advance(gid));
        }
        if let Some(fallback) = self.fallback_font(key, ch) {
            let fallback_scaled = fallback.as_scaled(PxScale::from(resolved.scale));
            let gid = fallback_scaled.glyph_id(ch);
            if gid.0 != 0 {
                return round_px(fallback_scaled.h_advance(gid));
            }
        }
        // Missing everywhere: advance as a space.
        round_px(scaled.h_advance(scaled.glyph_id(' ')))
    }

    /// Sum the advances of `text[..end]` (for extent/centering/caret math).
    pub(crate) fn text_advance(
        &mut self,
        resolved: &ResolvedFont,
        key: &FontKey,
        text: &str,
        end: usize,
    ) -> i32 {
        let mut total = 0_i32;
        for ch in text.chars().take(end) {
            total = total.saturating_add(self.char_advance(resolved, key, ch));
        }
        total
    }

    /// Rasterize `ch` (with fallback) into a coverage buffer, served from the
    /// per-(font, height, codepoint) glyph cache on repeat characters.
    pub(crate) fn rasterize(
        &mut self,
        resolved: &ResolvedFont,
        key: &FontKey,
        ch: char,
    ) -> RasterizedGlyph {
        let cache_key = (key.clone(), resolved.height_px, ch);
        if let Some(glyph) = self.glyph_cache.get(&cache_key) {
            return glyph.clone();
        }
        let glyph = self.rasterize_uncached(resolved, key, ch);
        self.insert_glyph(cache_key, glyph.clone());
        glyph
    }

    /// Insert a rasterized glyph into the bounded cache.
    ///
    /// On overflow the WHOLE glyph bitmap cache is dropped (faces and
    /// resolved metrics stay warm; the next render re-rasterizes each glyph
    /// once). See [`GLYPH_CACHE_CAP`].
    fn insert_glyph(&mut self, key: (FontKey, i32, char), glyph: RasterizedGlyph) {
        if self.glyph_cache.len() >= GLYPH_CACHE_CAP {
            self.glyph_cache.clear();
        }
        self.glyph_cache.insert(key, glyph);
    }

    /// Rasterize `ch` unconditionally (the cache-miss path of
    /// [`FontEngine::rasterize`]).
    fn rasterize_uncached(
        &mut self,
        resolved: &ResolvedFont,
        key: &FontKey,
        ch: char,
    ) -> RasterizedGlyph {
        let scaled = resolved.scaled();
        let gid = scaled.glyph_id(ch);
        if gid.0 != 0 {
            return outline_glyph(&scaled, gid)
                .unwrap_or_else(|| RasterizedGlyph::empty(round_px(scaled.h_advance(gid))));
        }
        if let Some(fallback) = self.fallback_font(key, ch) {
            let fallback_scaled = fallback.as_scaled(PxScale::from(resolved.scale));
            let gid = fallback_scaled.glyph_id(ch);
            if gid.0 != 0 {
                let advance = round_px(fallback_scaled.h_advance(gid));
                return outline_glyph(&fallback_scaled, gid)
                    .unwrap_or_else(|| RasterizedGlyph::empty(advance));
            }
        }
        // Missing everywhere: a space (advance only, no pixels).
        RasterizedGlyph::empty(round_px(scaled.h_advance(scaled.glyph_id(' '))))
    }

    /// Resolve (and cache) the fallback face that contains `ch`, if any.
    fn fallback_font(&mut self, key: &FontKey, ch: char) -> Option<FontArc> {
        let cache_key = (key.clone(), ch);
        if let Some(cached) = self.fallback_cache.get(&cache_key) {
            return cached.clone();
        }
        let mut result = None;
        for family in FALLBACK_FAMILIES {
            let selection = FamilySelection::Named((*family).to_owned());
            if let Some((id, _, _)) = face_id_for(&selection, 400, false)
                && let Some(font) = load_face(id)
                && font.as_scaled(PxScale::from(16.0)).glyph_id(ch).0 != 0
            {
                result = Some(font);
                break;
            }
        }
        self.fallback_cache.insert(cache_key, result.clone());
        result
    }
}

/// Build a [`ResolvedFont`]: scale so the line height equals `height_px`,
/// then compute the px metrics the metric APIs report.
fn build_resolved(
    font: &FontArc,
    height_px: i32,
    fake_bold: bool,
    fake_italic: bool,
) -> ResolvedFont {
    let units = font.units_per_em().unwrap_or(1000.0);
    let span = font.ascent_unscaled() - font.descent_unscaled();
    let target = height_px.max(1) as f32;
    let scale = if span > 0.0 {
        target * units / span
    } else {
        target
    };
    let scaled = font.as_scaled(PxScale::from(scale));
    let ascent = scaled.ascent();
    let descent = scaled.descent();
    let line_gap = scaled.line_gap();
    let avg_advance = round_px(
        (scaled.h_advance(scaled.glyph_id('x')) + scaled.h_advance(scaled.glyph_id('m'))) * 0.5,
    );
    let max_advance = (0x20_u32..=0x7E)
        .filter_map(char::from_u32)
        .map(|ch| round_px(scaled.h_advance(scaled.glyph_id(ch))))
        .fold(0_i32, i32::max);
    ResolvedFont {
        font: font.clone(),
        height_px,
        scale,
        ascent,
        descent,
        line_gap,
        avg_advance,
        max_advance,
        fake_bold,
        fake_italic,
    }
}

#[cfg(test)]
mod tests {
    use super::{FontEngine, FontKey, family_selection_for, fontdb_weight_for, height_px_from_lf};
    use fontdb::Family;

    /// Resolve the default 16 px font (skips when system fonts are absent).
    fn default_resolved(engine: &mut FontEngine) -> Option<(FontKey, super::ResolvedFont)> {
        let key = FontKey::default();
        let resolved = engine.resolve(&key, 16)?;
        Some((key, resolved))
    }

    #[test]
    fn glyph_cache_reuses_rasterized_bitmaps() {
        let mut engine = FontEngine::default();
        let Some((key, resolved)) = default_resolved(&mut engine) else {
            return; // no system fonts — nothing to cache
        };
        let first = engine.rasterize(&resolved, &key, 'A');
        let second = engine.rasterize(&resolved, &key, 'A');
        // Byte-identical result (advance + coverage + placement).
        assert_eq!(first.advance, second.advance);
        assert_eq!(first.coverage, second.coverage);
        assert_eq!((first.left, first.top), (second.left, second.top));
        // A second distinct codepoint adds one entry; 'A' stays cached.
        assert_eq!(engine.glyph_cache.len(), 1);
        drop(engine.rasterize(&resolved, &key, 'B'));
        assert_eq!(engine.glyph_cache.len(), 2);
        // char_advance reads the cached advance (no re-rasterization).
        let advance = engine.char_advance(&resolved, &key, 'A');
        assert_eq!(advance, first.advance);
    }

    #[test]
    fn glyph_cache_is_bounded_and_clears_on_overflow() {
        let mut engine = FontEngine::default();
        let key = FontKey::default();
        // Fill past the cap through the shared insert path (cheap empty
        // glyphs — no system fonts or rasterization needed).
        for code in 0..(super::GLYPH_CACHE_CAP + 64) {
            let ch = char::from_u32(u32::try_from(code).unwrap_or(0).saturating_add(0x100))
                .unwrap_or('x');
            engine.insert_glyph((key.clone(), 16, ch), super::RasterizedGlyph::empty(1));
        }
        assert!(
            engine.glyph_cache.len() <= super::GLYPH_CACHE_CAP,
            "glyph cache must stay bounded (len {})",
            engine.glyph_cache.len()
        );
        // After the overflow clear the cache still serves new entries.
        engine.insert_glyph((key.clone(), 16, 'Z'), super::RasterizedGlyph::empty(1));
        assert!(engine.glyph_cache.contains_key(&(key.clone(), 16, 'Z')));
        assert!(engine.glyph_cache.len() <= super::GLYPH_CACHE_CAP);
        // Real rasterization also recovers: a repeat char returns a cached
        // identical bitmap (system fonts optional — skip when absent).
        let Some((key, resolved)) = default_resolved(&mut engine) else {
            return;
        };
        let a = engine.rasterize(&resolved, &key, 'A');
        let b = engine.rasterize(&resolved, &key, 'A');
        assert_eq!(a.advance, b.advance);
        assert_eq!(a.coverage, b.coverage);
    }

    #[test]
    fn rasterize_serves_pre_cached_glyphs() {
        let mut engine = FontEngine::default();
        let key = FontKey::default();
        // Pre-seed the cache with a distinguishable fake glyph. `rasterize`
        // must return it unchanged (a cache hit) — a re-rasterized 'Q' could
        // never produce advance 7 with all-9 coverage.
        let fake = super::RasterizedGlyph {
            advance: 7,
            left: -1,
            top: 2,
            width: 3,
            height: 4,
            coverage: vec![9; 12],
        };
        engine.insert_glyph((key.clone(), 16, 'Q'), fake);
        let Some((key, resolved)) = default_resolved(&mut engine) else {
            return; // no system fonts — the seed alone still proves nothing
        };
        let hit = engine.rasterize(&resolved, &key, 'Q');
        assert_eq!(hit.advance, 7, "cached advance must be served");
        assert_eq!(hit.coverage, vec![9; 12], "cached coverage must be served");
        assert_eq!(engine.glyph_cache.len(), 1);
    }

    #[test]
    fn glyph_cache_key_includes_height() {
        let mut engine = FontEngine::default();
        let key = FontKey::default();
        let Some(resolved_16) = engine.resolve(&key, 16) else {
            return; // no system fonts
        };
        let Some(resolved_24) = engine.resolve(&key, 24) else {
            return;
        };
        // Same codepoint at two heights caches two separate bitmaps (the
        // coverage differs — 16 px vs 24 px).
        drop(engine.rasterize(&resolved_16, &key, 'A'));
        drop(engine.rasterize(&resolved_24, &key, 'A'));
        assert_eq!(engine.glyph_cache.len(), 2);
    }

    #[test]
    fn height_px_mapping() {
        // lfHeight == 0 → the 16 px default.
        assert_eq!(height_px_from_lf(0), 16);
        // Negative = character height in px.
        assert_eq!(height_px_from_lf(-24), 24);
        assert_eq!(height_px_from_lf(-1), 1);
        // Positive = cell height, approximated as the same px count.
        assert_eq!(height_px_from_lf(24), 24);
        assert_eq!(height_px_from_lf(16), 16);
        // Extreme values clamp, never zero.
        assert_eq!(height_px_from_lf(i32::MAX), i32::MAX);
        // |i32::MIN| does not fit i32 — the function falls back to the default.
        assert_eq!(height_px_from_lf(i32::MIN), 16);
    }

    #[test]
    fn family_mapping_strings() {
        // System/dialog faces → sans-serif.
        for name in [
            "",
            "MS Shell Dlg",
            "MS Shell Dlg 2",
            "Tahoma",
            "Segoe UI",
            "System",
        ] {
            assert_eq!(
                family_selection_for(name),
                super::FamilySelection::Generic(Family::SansSerif),
                "face {name:?} must map to the generic sans-serif"
            );
        }
        // Legacy terminal faces → monospace.
        for name in ["Courier New", "Terminal", "Fixedsys", "Consolas"] {
            assert_eq!(
                family_selection_for(name),
                super::FamilySelection::Generic(Family::Monospace),
                "face {name:?} must map to the generic monospace"
            );
        }
        // Times → serif.
        for name in ["Times New Roman", "Times"] {
            assert_eq!(
                family_selection_for(name),
                super::FamilySelection::Generic(Family::Serif),
                "face {name:?} must map to the generic serif"
            );
        }
        // Anything else is an exact (lowercased) face name.
        assert_eq!(
            family_selection_for("Arial"),
            super::FamilySelection::Named("arial".to_owned())
        );
        assert_eq!(
            family_selection_for("PingFang SC"),
            super::FamilySelection::Named("pingfang sc".to_owned())
        );
    }

    #[test]
    fn weight_threshold() {
        assert_eq!(fontdb_weight_for(0), 400);
        assert_eq!(fontdb_weight_for(400), 400);
        assert_eq!(fontdb_weight_for(599), 400);
        assert_eq!(fontdb_weight_for(600), 700);
        assert_eq!(fontdb_weight_for(700), 700);
        assert_eq!(fontdb_weight_for(1000), 700);
        assert_eq!(fontdb_weight_for(i32::MIN), 400);
    }
}
