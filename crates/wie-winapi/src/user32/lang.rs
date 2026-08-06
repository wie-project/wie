//! Locale-aware resource selection.
//!
//! Windows resolves a multi-language resource (a template id whose directory
//! level lists several LANGIDs) against the user's UI language with a fixed
//! chain (Microsoft Learn, "MUI Resource Management"): exact LANGID, then the
//! neutral id (primary language, sublanguage 0), then the en-US 0x0409
//! fallback, then the first block in directory order. WIE mirrors that chain
//! in [`resolve_block`], which every `RT_STRING`/`RT_MENU`/`RT_DIALOG`
//! resolver shares, and derives the UI language itself from the host: the
//! first entry of the macOS `AppleLanguages` user default, then `LANG`.
//!
//! The UI language is process-wide host state, not per-session handler state,
//! so it lives in one process-global `OnceLock` (host config behind a
//! `get_or_init` accessor) shared by every language handler — never on
//! `WinApiState`.

use std::sync::OnceLock;

/// LANGID for English (United States) — the final fallback of the resolution
/// chain and the default when the host locale maps to no Windows language id.
pub(crate) const LANGID_EN_US: u32 = 0x0409;

/// Process-wide UI language LANGID, derived once from the host locale.
///
/// Lazy: the first [`ui_language()`] access runs the derivation; every later
/// read (any thread) returns the cached value.
static UI_LANGUAGE: OnceLock<u32> = OnceLock::new();

/// The process UI language every locale-aware handler resolves against.
///
/// `GetUserDefaultUILanguage`, `LoadStringA/W`, `LoadMenuW` and
/// `CreateDialogParam*` all read this one value, so the language they report
/// and the resources they select always agree.
#[must_use]
pub fn ui_language() -> u32 {
    *UI_LANGUAGE.get_or_init(host_ui_language)
}

/// Pick the language-specific block `ui_language` selects out of `blocks`.
///
/// `blocks` yields `(langid, template)` pairs in resource-directory order.
/// Returns the block with the exact LANGID; failing that, the block whose
/// LANGID is the neutral form of `ui_language` (primary language only,
/// sublanguage 0); failing that, the en-US 0x0409 block; failing all three,
/// the first block of the directory — the pre-locale WIE behavior.
pub(crate) fn resolve_block<'a, T: ?Sized>(
    blocks: impl Iterator<Item = (u32, &'a T)>,
    ui_language: u32,
) -> Option<&'a T> {
    let mut first = None;
    let mut neutral = None;
    let mut en_us = None;
    // Primary language: the low byte of the LANGID for every common locale.
    let primary = ui_language & 0xFF;
    for (lang, block) in blocks {
        if first.is_none() {
            first = Some(block);
        }
        if lang == ui_language {
            return Some(block);
        }
        if neutral.is_none() && lang == primary {
            neutral = Some(block);
        }
        if en_us.is_none() && lang == LANGID_EN_US {
            en_us = Some(block);
        }
    }
    neutral.or(en_us).or(first)
}

/// LANGID derived from the host UI language.
///
/// Precedence, mirroring how macOS reports the UI language: the first entry
/// of `defaults read -g AppleLanguages` (the user's UI language), then the
/// `LANG` locale environment variable, then en-US (`0x0409`). Runs once, as
/// the [`UI_LANGUAGE`] `OnceLock` initializer, so the `defaults` process
/// spawn is a one-time cost.
#[must_use]
fn host_ui_language() -> u32 {
    apple_languages_ui_language()
        .or_else(lang_env_ui_language)
        .unwrap_or(LANGID_EN_US)
}

/// UI language LANGID from the macOS user-defaults `AppleLanguages` array.
///
/// The `defaults` command fails (non-macOS host, sandboxed launch) and the
/// dump can carry no quoted entry; both cases fall back to the `LANG` env
/// parse via [`host_ui_language`].
#[must_use]
fn apple_languages_ui_language() -> Option<u32> {
    let output = std::process::Command::new("defaults")
        .args(["read", "-g", "AppleLanguages"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let first = first_apple_language(&stdout)?;
    Some(langid_from_locale(first))
}

/// First quoted string of an `AppleLanguages` plist-array dump.
///
/// `defaults read -g AppleLanguages` prints a plist array with one quoted
/// locale per line (`(\n    "en-US",\n    "fr-FR",\n)`); the user's UI
/// language is the first entry. Returns `None` when the output carries no
/// quoted string at all.
#[must_use]
fn first_apple_language(output: &str) -> Option<&str> {
    output.split('"').nth(1)
}

/// LANGID from the `LANG` locale environment variable (`None` when unset).
#[must_use]
fn lang_env_ui_language() -> Option<u32> {
    std::env::var("LANG")
        .ok()
        .map(|locale| langid_from_locale(&locale))
}

/// Parse one POSIX/BCP-47 locale string (e.g. `en_US.UTF-8`, `zh-Hant`)
/// into a Windows LANGID.
///
/// The language is the first `_`/`-`-separated segment after dropping the
/// encoding (`.UTF-8`) and modifier (`@…`) suffix; the first region/script
/// subtag is passed along for languages whose script matters (`zh`).
#[must_use]
fn langid_from_locale(locale: &str) -> u32 {
    let base = locale.split(['.', '@']).next().unwrap_or_default();
    let mut parts = base.split(['_', '-']);
    let lang = parts.next().unwrap_or_default().to_ascii_lowercase();
    let subtag = parts.next().unwrap_or_default().to_ascii_lowercase();
    langid_for_lang(&lang, &subtag)
}

/// Map an ISO 639 language code plus its first region/script subtag to the
/// common Windows LANGID.
#[must_use]
fn langid_for_lang(lang: &str, subtag: &str) -> u32 {
    match (lang, subtag) {
        // Chinese script subtags (AppleLanguages carries zh-Hans/zh-Hant).
        ("zh", "hans") | ("zh", "cn") | ("zh", "sg") => 0x0804, // Simplified
        ("zh", "hant") | ("zh", "hk") | ("zh", "tw") | ("zh", "mo") => 0x0404, // Traditional
        _ => match lang {
            "en" => 0x0409,
            "fr" => 0x040C,
            "de" => 0x0407,
            "es" => 0x040A,
            "it" => 0x0410,
            "pt" => 0x0416,
            "nl" => 0x0413,
            "ja" => 0x0411,
            "ko" => 0x0412,
            "zh" => 0x0804,
            "ru" => 0x0419,
            "ar" => 0x0401,
            "sv" => 0x041D,
            "pl" => 0x0415,
            "da" => 0x0406,
            "fi" => 0x040B,
            "no" | "nb" => 0x0414,
            "tr" => 0x041F,
            "cs" => 0x0405,
            "hu" => 0x040E,
            "el" => 0x0408,
            "he" => 0x040D,
            "th" => 0x041E,
            "uk" => 0x0422,
            "vi" => 0x042A,
            _ => LANGID_EN_US,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_prefers_exact_then_neutral_then_en_us_then_first() {
        // German 0x0007 first, en-US 0x0409 second (notepad's directory order).
        let blocks = [(0x0007_u32, "german"), (0x0409, "en-us")];

        // Exact match wins even when it is not the first block.
        assert_eq!(resolve_block(blocks.iter().copied(), 0x0409), Some("en-us"));
        assert_eq!(
            resolve_block(blocks.iter().copied(), 0x0007),
            Some("german")
        );
        // Absent locale (French 0x040C): neutral 0x000C misses, 0x0409 wins.
        assert_eq!(resolve_block(blocks.iter().copied(), 0x040C), Some("en-us"));
        // German-Austria 0x0407: exact misses, neutral 0x0007 matches.
        assert_eq!(
            resolve_block(blocks.iter().copied(), 0x0407),
            Some("german")
        );
    }

    #[test]
    fn resolve_neutral_precedes_en_us() {
        // A neutral-only block beats the en-US fallback.
        let blocks = [(0x0409_u32, "en-us"), (0x000C, "french-neutral")];
        assert_eq!(
            resolve_block(blocks.iter().copied(), 0x040C),
            Some("french-neutral")
        );
    }

    #[test]
    fn resolve_falls_back_to_first() {
        // No match and no en-US: the first block wins (legacy behavior).
        let blocks = [(0x0007_u32, "german")];
        assert_eq!(
            resolve_block(blocks.iter().copied(), 0x0409),
            Some("german")
        );
        // An empty block list resolves to nothing.
        let empty: [(u32, &str); 0] = [];
        assert!(resolve_block(empty.iter().copied(), 0x0409).is_none());
    }

    #[test]
    fn host_locale_parses_lang_env() {
        // Sample LANG values (the parsing ignores the encoding suffix).
        let samples = [
            ("en_US.UTF-8", 0x0409),
            ("en-US.UTF-8", 0x0409),
            ("fr_FR.UTF-8", 0x040C),
            ("de_DE.UTF-8", 0x0407),
            ("ja_JP.UTF-8", 0x0411),
            ("C", 0x0409),
            ("", 0x0409),
        ];
        for (locale, expected) in samples {
            assert_eq!(langid_from_locale(locale), expected, "locale {locale:?}");
        }
    }

    /// The `defaults read -g AppleLanguages` dump is a plist array whose first
    /// quoted entry is the user's UI language; the parser must extract that
    /// first entry and only it (later entries never win).
    #[test]
    fn first_apple_language_extracts_first_entry() {
        // Real output shape: one quoted locale per line, first is the UI language.
        let output = "(\n    \"en-US\",\n    \"fr-FR\",\n    \"ar-FR\"\n)";
        let first = first_apple_language(output).expect("first entry");
        assert_eq!(langid_from_locale(first), 0x0409);
        // Single-line plist-array output parses the same way.
        let inline = "(\"en-US\", \"fr-FR\")";
        let first = first_apple_language(inline).expect("first entry");
        assert_eq!(langid_from_locale(first), 0x0409);
        // The FIRST entry wins even when a later one is English.
        let zh_first = "(\n    \"zh-Hant\",\n    \"en-US\"\n)";
        let first = first_apple_language(zh_first).expect("first entry");
        assert_eq!(langid_from_locale(first), 0x0404);
        // No quoted entry at all: nothing to parse.
        assert!(first_apple_language("()").is_none());
        assert!(first_apple_language("").is_none());
    }

    /// AppleLanguages carries Chinese with script subtags; the script (not the
    /// bare language) selects the Windows codepage LANGID.
    #[test]
    fn zh_script_subtags_map_to_script_langid() {
        // Traditional (zh-Hant family) → 0x0404.
        assert_eq!(langid_from_locale("zh-Hant"), 0x0404);
        assert_eq!(langid_from_locale("zh-HK"), 0x0404);
        assert_eq!(langid_from_locale("zh-TW"), 0x0404);
        assert_eq!(langid_from_locale("zh-MO"), 0x0404);
        // Simplified (zh-Hans family) → 0x0804.
        assert_eq!(langid_from_locale("zh-Hans"), 0x0804);
        assert_eq!(langid_from_locale("zh-CN"), 0x0804);
        assert_eq!(langid_from_locale("zh-SG"), 0x0804);
        // Bare zh keeps the Simplified default.
        assert_eq!(langid_from_locale("zh"), 0x0804);
        // The encoding suffix does not disturb the script subtag.
        assert_eq!(langid_from_locale("zh-Hant.UTF-8"), 0x0404);
    }
}
