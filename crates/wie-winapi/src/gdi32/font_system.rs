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

/// Sorted, deduplicated primary family names of the system font database.
///
/// Feeds the `ChooseFontW` dialog's family LISTBOX. The database is
/// process-wide and read-only, so this is cheap after the first
/// `load_system_fonts`. Names keep their exact-case spelling; the font engine
/// matches faces case-insensitively, so a chosen name resolves regardless of
/// how the host system spells it.
#[must_use]
pub(crate) fn system_family_names() -> Vec<String> {
    let db = system_font_db();
    let mut names: Vec<String> = db
        .faces()
        .filter_map(|face| face.families.first().map(|(name, _)| name.to_string()))
        .collect();
    names.sort();
    names.dedup();
    names
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

/// A font identity for caching: lowercase family + weight + italic + effects.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FontKey {
    /// Lowercased Win32 `lfFaceName` ("" = system default).
    pub family: String,
    /// Resolved weight: 400 or 700 (real bold or fake-bold emulation).
    pub weight: u16,
    /// Italic requested.
    pub italic: bool,
    /// `lfPitchAndFamily` carries the FIXED_PITCH bit (0x01): prefer a
    /// monospace face when the requested face is unavailable.
    pub fixed_pitch: bool,
    /// `lfStrikeOut` requested — the rasterizer paints a strike line through
    /// the run (positioned from the resolved ascent).
    pub strike_out: bool,
    /// `lfUnderline` requested — the rasterizer paints an underline below the
    /// baseline.
    pub underline: bool,
}

impl Default for FontKey {
    fn default() -> Self {
        Self {
            family: String::new(),
            weight: 400,
            italic: false,
            fixed_pitch: false,
            strike_out: false,
            underline: false,
        }
    }
}

/// A font family selection: a fontdb generic or an exact face name.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FamilySelection {
    Generic(Family<'static>),
    Named(String),
}

/// Map a Win32 `lfFaceName` + pitch hint to a fontdb family selection.
///
/// Win32 "system" faces and the classic dialog fonts map to the generic
/// sans-serif family; the console faces (including Lucida Console, the
/// notepad EDIT default) to monospace; Times to serif. Anything else is tried
/// as an exact face name (with a sans-serif fallback at query time). When the
/// LOGFONT's `lfPitchAndFamily` carries FIXED_PITCH (0x01) the fallback
/// becomes monospace instead of the generic sans-serif.
#[must_use]
fn family_selection_for(face_name: &str, fixed_pitch: bool) -> FamilySelection {
    match face_name.to_ascii_lowercase().as_str() {
        "" | "ms shell dlg" | "ms shell dlg 2" | "tahoma" | "segoe ui" | "system" => {
            FamilySelection::Generic(if fixed_pitch {
                Family::Monospace
            } else {
                Family::SansSerif
            })
        }
        "lucida console" | "courier new" | "terminal" | "fixedsys" | "consolas" => {
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

/// Whether a Win32 face name resolves to a MONOSPACED host face.
///
/// The ChooseFont dialog uses this to set `lfPitchAndFamily` for the picked
/// family (real Windows writes the selected font's pitch): a monospaced pick
/// keeps the FIXED_PITCH bit, a proportional pick must clear it — otherwise
/// the guest's `CreateFontIndirectW` sees the OLD fixed-pitch flag (the
/// default face's, e.g. notepad's Lucida Console) and the Windows
/// pitch-substitution rule substitutes monospace for the proportional family
/// the user actually chose.
#[must_use]
pub(crate) fn family_is_monospaced(face_name: &str) -> bool {
    let selection = family_selection_for(face_name, false);
    face_id_for(&selection, 400, false, false)
        .and_then(|(id, _, _)| system_font_db().face(id))
        .is_some_and(|info| info.monospaced)
}

/// Map a raw `lfHeight` to a target pixel height.
///
/// `lfHeight < 0` requests the CHARACTER height (tmHeight = ascent+descent,
/// which the engine makes exactly equal to the px scale); `lfHeight > 0`
/// requests the CELL height (the em square, which is the same px scale in
/// ab_glyph — the two collapse to one number); `lfHeight == 0` uses the
/// 16 px default.
#[must_use]
pub(crate) fn height_px_from_lf(lf_height: i32) -> i32 {
    if lf_height == 0 {
        16
    } else {
        i32::try_from(lf_height.unsigned_abs()).unwrap_or(16).max(1)
    }
}

/// The exact-case spelling of `name` present in the database, if any.
///
/// fontdb matches `Family::Name` with an exact string compare
/// (`face.families.any(|f| f.0 == name)`), and the engine lowercases every
/// face name — so a query like `"helvetica"` can never hit a capitalized
/// macOS face ("Helvetica") without this canonicalization step.
fn canonical_family_name<'a>(db: &'a fontdb::Database, name: &str) -> Option<&'a str> {
    db.faces()
        .flat_map(|face| &face.families)
        .map(|(family, _)| family.as_str())
        .find(|family| family.eq_ignore_ascii_case(name))
}

/// Find the best face id for a selection, tracking which requested attributes
/// had to fall back (so the rasterizer can emulate bold/italic).
///
/// Returns `(face_id, fake_bold, fake_italic)`. Weight falls back 700 → 400;
/// style falls back Italic → Oblique → Normal. A named face that the database
/// resolves keeps its exact resolution; on a miss it falls back to sans-serif
/// — or monospace when the LOGFONT carried FIXED_PITCH. FIXED_PITCH also
/// rejects proportional faces: Windows substitutes a fixed-pitch font rather
/// than honor a proportional face with the requested name, so a hit whose
/// face is not monospaced is skipped for the monospace fallback.
fn face_id_for(
    selection: &FamilySelection,
    weight: u16,
    italic: bool,
    fixed_pitch: bool,
) -> Option<(fontdb::ID, bool, bool)> {
    let db = system_font_db();
    let weights: &[u16] = if weight >= 600 { &[700, 400] } else { &[400] };
    let styles: &[Style] = if italic {
        &[Style::Italic, Style::Oblique, Style::Normal]
    } else {
        &[Style::Normal]
    };
    // Query `families` in order, returning the first hit that satisfies the
    // requested weight/style — and, under FIXED_PITCH, is an actual monospaced
    // face. A proportional hit (e.g. "Helvetica" on macOS answering a
    // FIXED_PITCH "helvetica") is skipped so the request can fall through to
    // the generic monospace, matching Windows' pitch-substitution behavior.
    let first_hit = |families: &[Family<'_>]| -> Option<(fontdb::ID, bool, bool)> {
        for &w in weights {
            for &style in styles {
                let query = Query {
                    families,
                    weight: Weight(w),
                    stretch: Stretch::Normal,
                    style,
                };
                if let Some(id) = db.query(&query) {
                    let monospaced = db.face(id).is_some_and(|info| info.monospaced);
                    if fixed_pitch && !monospaced {
                        continue;
                    }
                    return Some((id, w != weight, italic && style == Style::Normal));
                }
            }
        }
        None
    };
    match selection {
        FamilySelection::Generic(family) => first_hit(std::slice::from_ref(family)),
        FamilySelection::Named(name) => {
            // Exact name first; then the same face under its canonical
            // (database) spelling, so a lowercased query still resolves to a
            // capitalized face; the generic family is the last resort. A
            // FIXED_PITCH request falls back to monospace, not sans-serif.
            let mut families = Vec::with_capacity(3);
            families.push(Family::Name(name));
            if let Some(canonical) = canonical_family_name(db, name)
                && canonical != name
            {
                families.push(Family::Name(canonical));
            }
            if let Some(hit) = first_hit(&families) {
                return Some(hit);
            }
            let fallback = if fixed_pitch {
                Family::Monospace
            } else {
                Family::SansSerif
            };
            first_hit(std::slice::from_ref(&fallback))
        }
    }
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
pub(crate) fn outline_glyph(
    scaled: &PxScaleFont<&FontArc>,
    gid: GlyphId,
) -> Option<RasterizedGlyph> {
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
    /// Px per em — the glyph scale. Because ab_glyph defines the scaled
    /// height (`ascent + |descent|`) as identically equal to this scale,
    /// `scale == height_px` and the character height always matches the
    /// requested height exactly.
    pub scale: f32,
    /// Ascent in px.
    pub ascent: f32,
    /// Descent in px.
    pub descent: f32,
    /// Line gap in px (added into [`ResolvedFont::line_height`] and exposed
    /// as `tmExternalLeading`).
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

    /// Line height in px: ascent + |descent| + external leading.
    ///
    /// This is GDI's line pitch, `tmHeight + tmExternalLeading`: ab_glyph's
    /// descent is negative (below the baseline) so its magnitude is
    /// subtracted, and the line gap is ADDED (not merely exposed separately
    /// — the pre-fix height measured only the ascent/descent span, tighter
    /// than Windows). One value used by every line-based API (EDIT rows,
    /// DT_CALCRECT, control paint).
    pub(crate) fn line_height(&self) -> i32 {
        round_px(self.ascent)
            .saturating_sub(round_px(self.descent))
            .saturating_add(round_px(self.line_gap))
    }

    /// Rasterize `ch` into a coverage buffer at this font's pixel scale.
    ///
    /// Thin wrapper over [`outline_glyph`] so the enumeration / glyph-outline
    /// lanes can rasterize a glyph without reaching into the private helper.
    pub(crate) fn glyph_outline(&self, ch: char) -> Option<RasterizedGlyph> {
        let scaled = self.scaled();
        let gid = scaled.glyph_id(ch);
        outline_glyph(&scaled, gid)
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
        let selection = family_selection_for(&key.family, key.fixed_pitch);
        let (id, fake_bold, fake_italic) =
            face_id_for(&selection, key.weight, key.italic, key.fixed_pitch)?;
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
        let scaled = resolved.scaled();
        self.text_advance_with_scaled(&scaled, resolved, key, text, end)
    }

    /// Like [`Self::text_advance`] but with a pre-computed scaled font.
    ///
    /// Avoids the per-character `FontKey` clone and `scaled()` rebuild that
    /// [`Self::char_advance`] pays — the hot path for
    /// `GetTextExtentPoint32A` on long strings.
    pub(crate) fn text_advance_with_scaled(
        &mut self,
        scaled: &PxScaleFont<&FontArc>,
        resolved: &ResolvedFont,
        key: &FontKey,
        text: &str,
        end: usize,
    ) -> i32 {
        let mut total = 0_i32;
        for ch in text.chars().take(end) {
            let cache_key = (key.clone(), resolved.height_px, ch);
            if let Some(glyph) = self.glyph_cache.get(&cache_key) {
                total = total.saturating_add(glyph.advance);
                continue;
            }
            let gid = scaled.glyph_id(ch);
            if gid.0 != 0 {
                total = total.saturating_add(round_px(scaled.h_advance(gid)));
                continue;
            }
            if let Some(fallback) = self.fallback_font(key, ch) {
                let fallback_scaled = fallback.as_scaled(PxScale::from(resolved.scale));
                let gid = fallback_scaled.glyph_id(ch);
                if gid.0 != 0 {
                    total = total.saturating_add(round_px(fallback_scaled.h_advance(gid)));
                    continue;
                }
            }
            total = total.saturating_add(round_px(scaled.h_advance(scaled.glyph_id(' '))));
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
            if let Some((id, _, _)) = face_id_for(&selection, 400, false, false)
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

/// Build a [`ResolvedFont`]: scale so the character height (`ascent+descent`)
/// equals `height_px`, then compute the px metrics the metric APIs report.
///
/// ab_glyph defines `ScaleFont::height() = ascent() - descent() ≡ scale.y`
/// (both ascent and descent are normalized by `height_unscaled`), so the px
/// scale IS the character height — there is no separate em-to-character
/// ratio to correct for. The pre-fix formula `target * units_per_em / span`
/// assumed `ascent()-descent() = span * scale / units_per_em` and therefore
/// shrank every face whose typo span exceeds the em box (most fonts) to
/// ~86-90% of the requested height — the ~15-20% undersized notepad EDIT.
fn build_resolved(
    font: &FontArc,
    height_px: i32,
    fake_bold: bool,
    fake_italic: bool,
) -> ResolvedFont {
    let target = height_px.max(1) as f32;
    let scale = target;
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
    use super::{
        FontEngine, FontKey, face_id_for, family_selection_for, fontdb_weight_for,
        height_px_from_lf,
    };
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
                family_selection_for(name, false),
                super::FamilySelection::Generic(Family::SansSerif),
                "face {name:?} must map to the generic sans-serif"
            );
        }
        // Console faces → monospace (Lucida Console is the notepad EDIT
        // default — audit finding #6).
        for name in [
            "Lucida Console",
            "Courier New",
            "Terminal",
            "Fixedsys",
            "Consolas",
        ] {
            assert_eq!(
                family_selection_for(name, false),
                super::FamilySelection::Generic(Family::Monospace),
                "face {name:?} must map to the generic monospace"
            );
        }
        // Times → serif.
        for name in ["Times New Roman", "Times"] {
            assert_eq!(
                family_selection_for(name, false),
                super::FamilySelection::Generic(Family::Serif),
                "face {name:?} must map to the generic serif"
            );
        }
        // Anything else is an exact (lowercased) face name.
        assert_eq!(
            family_selection_for("Arial", false),
            super::FamilySelection::Named("arial".to_owned())
        );
        assert_eq!(
            family_selection_for("PingFang SC", false),
            super::FamilySelection::Named("pingfang sc".to_owned())
        );
    }

    #[test]
    fn fixed_pitch_faces_fall_back_to_monospace() {
        // A known console face with FIXED_PITCH → monospace (not the
        // sans-serif fallback the engine used before the fix).
        for name in ["Lucida Console", "Consolas"] {
            assert_eq!(
                family_selection_for(name, true),
                super::FamilySelection::Generic(Family::Monospace),
                "console face {name:?} with FIXED_PITCH must map to monospace"
            );
        }
        // The FIXED_PITCH bit on a system face selects the monospace default.
        for name in ["", "MS Shell Dlg", "Segoe UI"] {
            assert_eq!(
                family_selection_for(name, true),
                super::FamilySelection::Generic(Family::Monospace),
                "system face {name:?} with FIXED_PITCH must fall back to monospace"
            );
        }
    }

    #[test]
    fn ms_shell_dlg_default_remains_sans_serif() {
        // The default dialog font path is untouched: no pitch hint keeps the
        // generic sans-serif mapping.
        assert_eq!(
            family_selection_for("MS Shell Dlg", false),
            super::FamilySelection::Generic(Family::SansSerif)
        );
    }

    /// A real family name from the system font database, for tests that need
    /// a resolvable named face on ANY host. Prefers a mixed-case name so the
    /// lowercase-vs-canonical distinction is meaningful; `None` only when the
    /// database has no faces at all.
    fn any_system_family() -> Option<String> {
        let db = super::system_font_db();
        let mut names: Vec<&str> = db
            .faces()
            .filter_map(|face| face.families.first().map(|(name, _)| name.as_str()))
            .collect();
        names.sort_unstable();
        names.dedup();
        names
            .iter()
            .find(|name| name.chars().any(char::is_uppercase))
            .or_else(|| names.first())
            .map(|name| (*name).to_owned())
    }

    /// Whether the resolved face carries `family` anywhere in its family
    /// list (case-insensitively) — a face can match a query on a non-first
    /// alias, so checking only the first family is not reliable.
    fn face_has_family(db: &fontdb::Database, id: fontdb::ID, family: &str) -> bool {
        db.face(id).is_some_and(|info| {
            info.families
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case(family))
        })
    }

    #[test]
    fn canonical_case_named_face_resolves_to_the_named_family() {
        // The canonical spelling of a real system face must resolve to that
        // face — never collapse to the generic sans-serif fallback. The
        // family is picked from the host database, so any machine works.
        let Some(name) = any_system_family() else {
            return;
        };
        let named = super::FamilySelection::Named(name.clone());
        let Some((id, _, _)) = face_id_for(&named, 400, false, false) else {
            return;
        };
        assert!(
            face_has_family(super::system_font_db(), id, &name),
            "the canonical query must resolve to the {name} family, got face {id:?}"
        );
    }

    #[test]
    fn lowercase_named_face_resolves_to_the_same_family() {
        // The engine lowercases every face name (family_selection_for), so a
        // guest requesting a canonical face yields Named(<lowercased>). That
        // query must resolve to the SAME face as the canonical spelling —
        // not fall through to the generic fallback (the pre-fix behavior:
        // fontdb matches Family::Name case-sensitively).
        let Some(name) = any_system_family() else {
            return;
        };
        let lower = super::FamilySelection::Named(name.to_lowercase());
        let canonical = super::FamilySelection::Named(name.clone());
        let Some((lower_id, _, _)) = face_id_for(&lower, 400, false, false) else {
            return;
        };
        let Some((canonical_id, _, _)) = face_id_for(&canonical, 400, false, false) else {
            return;
        };
        assert_eq!(
            lower_id, canonical_id,
            "a lowercase query must resolve to the same face as the canonical spelling"
        );
        // And that face is the named family, not the generic fallback.
        assert!(
            face_has_family(super::system_font_db(), lower_id, &name),
            "the lowercase query must resolve to the {name} family, got face {lower_id:?}"
        );
    }

    #[test]
    fn fixed_pitch_named_miss_falls_back_to_monospace() {
        // A named face that misses the database falls back to the generic
        // monospace with FIXED_PITCH — and to sans-serif without it.
        let unknown = super::FamilySelection::Named("definitely-not-a-face".to_owned());
        let Some((mono_id, _, _)) = face_id_for(
            &super::FamilySelection::Generic(Family::Monospace),
            400,
            false,
            false,
        ) else {
            return; // no monospace font on this system
        };
        let Some((sans_id, _, _)) = face_id_for(
            &super::FamilySelection::Generic(Family::SansSerif),
            400,
            false,
            false,
        ) else {
            return;
        };
        let Some((with, _, _)) = face_id_for(&unknown, 400, false, true) else {
            return;
        };
        let Some((without, _, _)) = face_id_for(&unknown, 400, false, false) else {
            return;
        };
        assert_eq!(
            with, mono_id,
            "a FIXED_PITCH miss must fall back to the generic monospace"
        );
        assert_eq!(
            without, sans_id,
            "a plain miss keeps the generic sans-serif fallback"
        );
    }

    #[test]
    fn lucida_console_maps_to_monospace() {
        // Audit finding #6: notepad's EDIT uses Lucida Console (the default
        // face in every RNotepad language file) with FIXED_PITCH. Before the
        // fix this fell through to a `Named` lookup → fontdb miss → the
        // generic sans-serif fallback (proportional text in the EDIT).
        assert_eq!(
            family_selection_for("Lucida Console", true),
            super::FamilySelection::Generic(Family::Monospace),
            "Lucida Console must map to the generic monospace"
        );
    }

    #[test]
    fn lucida_console_resolves_to_a_monospace_face() {
        let mut engine = FontEngine::default();
        let key = FontKey {
            family: "lucida console".to_owned(),
            weight: 400,
            italic: false,
            fixed_pitch: true,
            strike_out: false,
            underline: false,
        };
        let Some(resolved) = engine.resolve(&key, 16) else {
            return; // no system fonts — nothing to resolve
        };
        // A monospace face advances every printable ASCII glyph equally: the
        // average of 'x'/'m' equals the widest advance. A proportional
        // sans-serif fallback (the pre-fix behavior) has max > avg.
        assert_eq!(
            resolved.avg_advance, resolved.max_advance,
            "Lucida Console must resolve to a monospace face (avg {} max {})",
            resolved.avg_advance, resolved.max_advance
        );
    }

    #[test]
    fn negative_lfheight_maps_to_character_height() {
        // The notepad EDIT lane: a guest creates its edit font with
        // lfHeight = -13, which requests the CHARACTER height (tmHeight =
        // ascent+descent). Because ab_glyph makes the scaled height exactly
        // equal to the px scale, resolving at 13 px must yield
        // ascent+descent ≈ 13 — host-independently, for ANY face the
        // canonical monospace substitution produces. Before the fix the
        // engine applied a `* units_per_em / span` correction that shrank
        // the rendered height to ~86-90% of the request (the ~15-20%
        // undersized EDIT vs real Windows notepad).
        let mut engine = FontEngine::default();
        let key = FontKey {
            family: "lucida console".to_owned(),
            weight: 400,
            italic: false,
            fixed_pitch: true,
            strike_out: false,
            underline: false,
        };
        let Some(resolved) = engine.resolve(&key, height_px_from_lf(-13)) else {
            return; // no system fonts — nothing to resolve
        };
        let char_h = resolved.ascent - resolved.descent;
        assert!(
            (char_h - 13.0).abs() <= 1.0,
            "ascent+descent must be ≈ |lfHeight| = 13, got {char_h:.2} (scale {:.2})",
            resolved.scale
        );
        // The same key flows out of CreateFontIndirectW("Lucida Console",
        // -13, FIXED_PITCH): family lowercased, pitch bit 0x01 → fixed_pitch.
        assert_eq!(key.family, "lucida console");
        // The positive-lfHeight counterpart (cell height = the em square):
        // the same px scale, so a +13 request also resolves at 13.
        let Some(positive) = engine.resolve(&key, height_px_from_lf(13)) else {
            return;
        };
        assert!(
            (positive.scale - resolved.scale).abs() <= 1.0,
            "both signs must set the px scale to |lfHeight| ({} vs {})",
            positive.scale,
            resolved.scale
        );
    }

    #[test]
    fn line_height_includes_external_leading() {
        // F3: the EDIT line pitch is GDI's tmHeight + tmExternalLeading — the
        // ascent/descent span PLUS the line gap. Before the fix the gap was
        // parsed and exposed separately but never added, so every line-based
        // measure was tighter than Windows'. Any resolvable face with a
        // non-zero (rounded) gap demonstrates the property on any host; hosts
        // whose faces all have zero leading (console faces) skip.
        let mut engine = FontEngine::default();
        let db = super::system_font_db();
        let gap_family = db.faces().find_map(|face| {
            let family = face
                .families
                .first()
                .map(|(name, _)| name.to_ascii_lowercase())
                .unwrap_or_default();
            let key = FontKey {
                family,
                weight: 400,
                italic: false,
                fixed_pitch: false,
                strike_out: false,
                underline: false,
            };
            if super::round_px(engine.resolve(&key, 16)?.line_gap) != 0 {
                Some(key.family)
            } else {
                None
            }
        });
        let Some(family) = gap_family else {
            return; // every face's leading rounds to zero — nothing to prove
        };
        let key = FontKey {
            family,
            weight: 400,
            italic: false,
            fixed_pitch: false,
            strike_out: false,
            underline: false,
        };
        let Some(resolved) = engine.resolve(&key, 16) else {
            return;
        };
        // tmHeight = round(ascent) + round(|descent|); tmExternalLeading =
        // round(line_gap). Their sum is the Windows line pitch and must equal
        // line_height() exactly.
        let span =
            super::round_px(resolved.ascent).saturating_sub(super::round_px(resolved.descent));
        let with_leading = span.saturating_add(super::round_px(resolved.line_gap));
        assert_eq!(
            resolved.line_height(),
            with_leading,
            "line height must equal tmHeight + tmExternalLeading (gap {:.2})",
            resolved.line_gap
        );
        assert_ne!(
            resolved.line_height(),
            span,
            "the pre-fix height omitted the external leading"
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

    #[test]
    fn font_key_includes_effects() {
        // `lfUnderline`/`lfStrikeOut` are part of the font identity: two keys
        // differing only in an effect are distinct (a plain resolve must never
        // serve an underlined run's cached metrics — the rasterizer reads the
        // effect flags off the key to paint the strokes). Pre-fix the key
        // ignored the effects entirely, so the renderer had no signal to draw
        // them.
        let plain = FontKey::default();
        let underlined = FontKey {
            underline: true,
            ..plain.clone()
        };
        let struck = FontKey {
            strike_out: true,
            ..plain.clone()
        };
        let both = FontKey {
            strike_out: true,
            underline: true,
            ..plain.clone()
        };
        assert_ne!(plain, underlined, "underline must distinguish the key");
        assert_ne!(plain, struck, "strikeout must distinguish the key");
        assert_ne!(underlined, struck, "the two effects are independent bits");
        assert_eq!(
            FontKey {
                strike_out: true,
                underline: true,
                ..plain.clone()
            },
            both,
            "the effect bits round-trip through the key"
        );
    }

    #[test]
    fn fixed_pitch_named_proportional_face_falls_back_to_monospace() {
        // F3: Windows never returns a proportional face for a FIXED_PITCH
        // request — GDI substitutes a fixed-pitch font instead. A named
        // proportional family that IS present on the host (e.g. "Helvetica"
        // on macOS) must therefore fall back to the generic monospace, never
        // resolve to itself. The family is picked from the host database, so
        // any machine works; the test skips when no proportional or no
        // monospace face exists.
        let db = super::system_font_db();
        let Some(prop_name) = db
            .faces()
            .find(|face| !face.monospaced)
            .and_then(|face| face.families.first().map(|(name, _)| name.clone()))
        else {
            return; // no proportional face on this system
        };
        let Some((mono_id, _, _)) = face_id_for(
            &super::FamilySelection::Generic(Family::Monospace),
            400,
            false,
            false,
        ) else {
            return; // no monospace face on this system
        };
        let named = super::FamilySelection::Named(prop_name);
        // The family must really be present AND proportional, or the test
        // proves nothing: without FIXED_PITCH the plain request resolves to
        // that same (proportional) face.
        let Some((plain_id, _, _)) = face_id_for(&named, 400, false, false) else {
            return;
        };
        assert_ne!(
            plain_id, mono_id,
            "the chosen family must be proportional for this test to bite"
        );
        // FIXED_PITCH must skip the proportional face and land on the generic
        // monospace (the pre-fix behavior returned the proportional face).
        let Some((with, _, _)) = face_id_for(&named, 400, false, true) else {
            return;
        };
        assert_eq!(
            with, mono_id,
            "a FIXED_PITCH request for a proportional named face must fall back to monospace"
        );
    }

    #[test]
    fn fixed_pitch_keeps_resolvable_monospace_named_faces() {
        // FIXED_PITCH must NOT force the generic monospace fallback when the
        // requested named face actually resolves AND is monospaced — the exact
        // face wins; only proportional faces fall back. The family is picked
        // from the host font database (any machine), so the property is proven
        // without depending on a specific installed face.
        let db = super::system_font_db();
        let Some(name) = db
            .faces()
            .find(|face| face.monospaced)
            .and_then(|face| face.families.first().map(|(name, _)| name.clone()))
        else {
            return; // no monospace face on this system
        };
        let named = super::FamilySelection::Named(name);
        let Some((with, _, _)) = face_id_for(&named, 400, false, true) else {
            return;
        };
        let Some((without, _, _)) = face_id_for(&named, 400, false, false) else {
            return;
        };
        assert_eq!(
            with, without,
            "a resolvable monospace named face must resolve identically with FIXED_PITCH"
        );
        assert!(
            super::system_font_db()
                .face(with)
                .is_some_and(|info| info.monospaced),
            "the resolved face must actually be monospaced"
        );
    }
}
