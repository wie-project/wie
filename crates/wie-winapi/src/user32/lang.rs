//! Locale-aware resource selection.
//!
//! Windows resolves a multi-language resource (a template id whose directory
//! level lists several LANGIDs) against the user's UI language with a fixed
//! chain (Microsoft Learn, "MUI Resource Management"): exact LANGID, then the
//! neutral id (primary language, sublanguage 0), then the en-US 0x0409
//! fallback, then the first block in directory order. WIE mirrors that chain
//! in [`resolve_block`], which every `RT_STRING`/`RT_MENU`/`RT_DIALOG`
//! resolver shares, and derives the UI language itself from the host —
//! deliberately, so a guest's menus, dialogs and strings come out in the
//! language of the person running it.
//!
//! # Pinning the language
//!
//! Two environment overrides are consulted **before** the host is asked, so a
//! user or CI can pin the guest language without touching the system locale:
//!
//! 1. `WIE_UI_LANGID` — a Windows LANGID (`0x409`, `409`, `0x0409`) or a
//!    language tag (`en-US`, `de`, `fr_FR.UTF-8`); the explicit, unambiguous
//!    knob, because `LANG` is ambient.
//! 2. `LC_ALL`, then `LANG` — the conventional POSIX locale variables.
//!
//! Only then does macOS get a say (`AppleLanguages[0]`), and only then the
//! en-US default. An override that cannot be parsed is ignored (never
//! silently coerced into en-US), so a typo degrades to the host language
//! rather than a wrong pin.
//!
//! The UI language is process-wide host state, not per-session handler state,
//! so it lives in one process-global `OnceLock` (host config behind a
//! `get_or_init` accessor) shared by every language handler — never on
//! `WinApiState`. Because the derivation runs once, the overrides must be set
//! before the first `GetUserDefaultUILanguage` / `LoadMenuW` / `LoadString*`.
//!
//! [`ui_language_from`]: the pure, unit-tested core; [`host_ui_language`]
//! wraps it with the host probes.

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

/// LANGID derived from the environment overrides, else from the host.
///
/// Thin shell over the pure [`ui_language_from`]: it collects the candidate
/// strings in precedence order and lets the pure function decide. Runs once,
/// as the [`UI_LANGUAGE`] `OnceLock` initializer.
#[must_use]
fn host_ui_language() -> u32 {
    ui_language_from(
        env_override("WIE_UI_LANGID"),
        env_override("LC_ALL"),
        env_override("LANG"),
        apple_languages_ui_language(),
    )
}

/// Read one environment variable as a candidate UI-language string.
///
/// An empty value counts as unset (an empty `LANG=` in a shell profile is
/// "no locale", not "the empty language").
#[must_use]
fn env_override(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Pure core of the UI-language derivation: pick the first candidate that
/// parses, else fall back to the host's `AppleLanguages[0]`, else en-US.
///
/// The whole decision is a pure function of its arguments (no env, no
/// subprocess) so it is unit-testable — `std::env::set_var` is `unsafe` and
/// this crate denies `unsafe`, so the test suite cannot drive the real
/// process environment.
///
/// Precedence, in order:
/// 1. `WIE_UI_LANGID` — an explicit pin (LANGID or language tag).
/// 2. `LC_ALL`, then `LANG` — the ambient POSIX locale. Honoured *before* the
///    macOS probe so the documented override is actually reachable: in any
///    Aqua session `defaults read -g AppleLanguages` always succeeds, so
///    consulting it first made the `LANG` path dead code.
/// 3. `AppleLanguages[0]` (macOS) — the host UI language.
/// 4. en-US `0x0409`.
///
/// Unparseable candidates are skipped, never coerced to en-US: a junk
/// `WIE_UI_LANGID` must not silently mean "English" — it must fall through to
/// the next real source.
#[must_use]
fn ui_language_from(
    explicit: Option<String>,
    lc_all: Option<String>,
    lang: Option<String>,
    apple: Option<u32>,
) -> u32 {
    [explicit, lc_all, lang]
        .into_iter()
        .flatten()
        .find_map(|value| parse_langid(&value))
        .or(apple)
        .unwrap_or(LANGID_EN_US)
}

/// Parse a UI-language override into a LANGID, or `None` when it is not one.
///
/// Two accepted spellings, tried in order:
/// * a Windows LANGID in decimal or hex — `0x409`, `409`, `0x0409` (the
///   `GetUserDefaultUILanguage` return value, so it can be pasted straight
///   from a guest);
/// * anything [`langid_from_locale`] understands — `en-US`, `de`,
///   `fr_FR.UTF-8`.
///
/// A non-numeric string is a locale tag; a numeric one that does not fit a
/// 16-bit LANGID (a bare `1234567890`, say) is junk and yields `None` rather
/// than a truncated id.
#[must_use]
fn parse_langid(value: &str) -> Option<u32> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Some(langid) = numeric_langid(value) {
        return Some(langid);
    }
    // The POSIX "no localization" locales. These are a DELIBERATE request, not
    // junk, and their honest translation is the resource-neutral en-US rather
    // than "whatever the host says" — which is also the pre-override behaviour
    // of `LANG=C` here, so a CI box with `LC_ALL=C` keeps getting en-US.
    if value.eq_ignore_ascii_case("c") || value.eq_ignore_ascii_case("posix") {
        return Some(LANGID_EN_US);
    }
    // A locale tag is only meaningful if its first subtag is a language; a
    // tag with no leading letters ("_UTF-8", "123-abc") is junk, and an
    // unknown language ("not-a-locale") must NOT be coerced to en-US.
    let first = value.split(['_', '-', '.', '@']).next().unwrap_or_default();
    if !first
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
    {
        return None;
    }
    known_langid_from_locale(value)
}

/// Parse a decimal or `0x`-prefixed hex LANGID, or `None` if not that shape.
#[must_use]
fn numeric_langid(value: &str) -> Option<u32> {
    let (digits, radix) = match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(hex) => (hex, 16),
        None => (value, 10),
    };
    if digits.is_empty() {
        return None;
    }
    let all_digits = match radix {
        16 => digits.chars().all(|c| c.is_ascii_hexdigit()),
        _ => digits.chars().all(|c| c.is_ascii_digit()),
    };
    if !all_digits {
        return None;
    }
    let parsed = u32::from_str_radix(digits, radix).ok()?;
    // A LANGID is 16 bits; anything wider is a typo, not a language.
    u16::try_from(parsed).ok().map(u32::from)
}

/// Parse a locale string into a LANGID, but only for a language this table
/// knows — the strict sibling of [`langid_from_locale`].
///
/// The override path uses this so a mistyped pin is REJECTED (and the next
/// real source decides) instead of being silently read as en-US. Consequence:
/// `WIE_UI_LANGID` can only pin a language in the table; an exotic one falls
/// through to the host. Failing to the host language is right, wrong-language
/// menus are not.
#[must_use]
fn known_langid_from_locale(locale: &str) -> Option<u32> {
    let base = locale.split(['.', '@']).next().unwrap_or_default();
    let mut parts = base.split(['_', '-']);
    let lang = parts.next().unwrap_or_default().to_ascii_lowercase();
    let subtag = parts.next().unwrap_or_default().to_ascii_lowercase();
    known_langid_for_lang(&lang, &subtag)
}

/// LANGID derived from the host UI language (macOS `AppleLanguages[0]`).
///
/// The `defaults` command fails (non-macOS host, sandboxed launch) and the
/// dump can carry no quoted entry; both cases fall back to the en-US default
/// via [`host_ui_language`].
///
/// NOTE: the `defaults` subprocess spawn costs ~8 ms on the process's FIRST
/// `GetUserDefaultUILanguage` call (the result is memoized in a `OnceLock`,
/// so later calls are free). It is also *skipped entirely* when an override
/// already decided the language. A faster alternative would be reading
/// `~/Library/Preferences/.GlobalPreferences.plist` directly (no subprocess),
/// but that needs boilerplate plist parsing (XML and binary variants), so the
/// subprocess is kept — revisit only if startup-time language lookup ever
/// shows up on a hot path.
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
/// common Windows LANGID, or `None` for a language this table does not know.
#[must_use]
fn known_langid_for_lang(lang: &str, subtag: &str) -> Option<u32> {
    match (lang, subtag) {
        // Chinese script subtags (AppleLanguages carries zh-Hans/zh-Hant).
        ("zh", "hans") | ("zh", "cn") | ("zh", "sg") => Some(0x0804), // Simplified
        ("zh", "hant") | ("zh", "hk") | ("zh", "tw") | ("zh", "mo") => Some(0x0404), // Traditional
        _ => match lang {
            "en" => Some(0x0409),
            "fr" => Some(0x040C),
            "de" => Some(0x0407),
            "es" => Some(0x040A),
            "it" => Some(0x0410),
            "pt" => Some(0x0416),
            "nl" => Some(0x0413),
            "ja" => Some(0x0411),
            "ko" => Some(0x0412),
            "zh" => Some(0x0804),
            "ru" => Some(0x0419),
            "ar" => Some(0x0401),
            "sv" => Some(0x041D),
            "pl" => Some(0x0415),
            "da" => Some(0x0406),
            "fi" => Some(0x040B),
            "no" | "nb" => Some(0x0414),
            "tr" => Some(0x041F),
            "cs" => Some(0x0405),
            "hu" => Some(0x040E),
            "el" => Some(0x0408),
            "he" => Some(0x040D),
            "th" => Some(0x041E),
            "uk" => Some(0x0422),
            "vi" => Some(0x042A),
            _ => None,
        },
    }
}

/// [`known_langid_for_lang`] with the en-US default applied.
///
/// Only the HOST path uses this: an unrecognised host locale has no better
/// answer than en-US, whereas an unrecognised *override* is a typo and must
/// stay distinguishable.
#[must_use]
fn langid_for_lang(lang: &str, subtag: &str) -> u32 {
    known_langid_for_lang(lang, subtag).unwrap_or(LANGID_EN_US)
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
        // Sample locale strings (the parsing ignores the encoding suffix).
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

    /// `WIE_UI_LANGID` accepts both spellings a user has to hand: the numeric
    /// LANGID the guest itself reports, and a language tag.
    #[test]
    fn explicit_override_accepts_langid_and_tag() {
        // Numeric LANGIDs. Mind the radix: `0x409` is 0x0409 (1033), while
        // bare `409` is the LANGID 0x0199 — the decimal form is the number
        // GetUserDefaultUILanguage returns, i.e. 1033.
        assert_eq!(parse_langid("0x409"), Some(0x0409));
        assert_eq!(parse_langid("0X409"), Some(0x0409));
        assert_eq!(parse_langid("0x0409"), Some(0x0409));
        assert_eq!(parse_langid("1033"), Some(0x0409)); // en-US, as reported
        assert_eq!(parse_langid("1031"), Some(0x0407)); // German, as reported
        assert_eq!(parse_langid("0x407"), Some(0x0407));
        assert_eq!(parse_langid("409"), Some(0x0199)); // decimal 409, as written
        // Language tags, including the POSIX and encoding-suffixed spellings.
        assert_eq!(parse_langid("en-US"), Some(0x0409));
        assert_eq!(parse_langid("de"), Some(0x0407));
        assert_eq!(parse_langid("de-DE"), Some(0x0407));
        assert_eq!(parse_langid("fr_FR.UTF-8"), Some(0x040C));
        // The POSIX "no localization" locales pin en-US deliberately.
        assert_eq!(parse_langid("C"), Some(LANGID_EN_US));
        assert_eq!(parse_langid("POSIX"), Some(LANGID_EN_US));
        // Surrounding whitespace from a sloppy export must not defeat the pin.
        assert_eq!(parse_langid("  en-US\n"), Some(0x0409));
    }

    /// The junk cases: an override that is not a language must return `None`
    /// so the caller falls through to the next real source. It must NEVER
    /// become en-US, which would silently pin the wrong language.
    #[test]
    fn junk_override_is_never_mistaken_for_a_language() {
        for junk in [
            "",
            "   ",
            "not-a-locale-at-all", // unknown language code
            "klingon",             // unknown language code
            "1234567890",          // decimal, but wider than a 16-bit LANGID
            "0x1FFFF",             // hex, likewise
            "0x",                  // hex prefix with no digits
            "_UTF-8",              // encoding suffix, no language
            ".UTF-8",
            "@euro",
            "!!",
        ] {
            assert_eq!(parse_langid(junk), None, "junk {junk:?}");
        }
        // A number that IS a valid LANGID is not junk, even an odd one.
        assert_eq!(parse_langid("0"), Some(0));
        assert_eq!(parse_langid("0xFFFF"), Some(0xFFFF));
    }

    /// The host path and the override path must NOT agree on an unknown
    /// language: the host has nothing better than en-US, an override is a
    /// typo. This is the asymmetry that keeps `WIE_UI_LANGID=nope` from
    /// quietly selecting English resources on a German macOS.
    #[test]
    fn unknown_language_defaults_only_on_the_host_path() {
        assert_eq!(langid_from_locale("klingon"), LANGID_EN_US);
        assert_eq!(known_langid_from_locale("klingon"), None);
        // `C` / `POSIX` map to en-US through langid_from_locale's table miss.
        assert_eq!(langid_from_locale("C"), LANGID_EN_US);
    }

    /// Precedence: an explicit pin beats the ambient locale, which beats the
    /// macOS `AppleLanguages` probe, which beats the en-US default. The host
    /// probe is passed in already-resolved so the macOS branch is testable off
    /// macOS.
    #[test]
    fn overrides_take_precedence_over_the_host_probe() {
        // WIE_UI_LANGID wins even when the host is German.
        assert_eq!(
            ui_language_from(
                Some("en-US".to_string()),
                Some("fr_FR.UTF-8".to_string()),
                Some("ja_JP.UTF-8".to_string()),
                Some(0x0407)
            ),
            0x0409
        );
        // LC_ALL beats LANG.
        assert_eq!(
            ui_language_from(
                None,
                Some("de_DE.UTF-8".to_string()),
                Some("ja_JP.UTF-8".to_string()),
                Some(0x040C)
            ),
            0x0407
        );
        // LANG beats the host — this is the case that used to be unreachable,
        // because `defaults` always succeeds in an Aqua session.
        assert_eq!(
            ui_language_from(None, None, Some("en_US.UTF-8".to_string()), Some(0x0407)),
            0x0409
        );
    }

    /// A junk override must be skipped, not honoured as en-US and not fatal:
    /// the next real source decides.
    #[test]
    fn junk_override_falls_through_to_the_next_source() {
        assert_eq!(
            ui_language_from(
                Some("!!!".to_string()),
                None,
                Some("fr_FR.UTF-8".to_string()),
                Some(0x0407)
            ),
            0x040C
        );
        // ...and with nothing else set, the host wins rather than a fake en-US.
        assert_eq!(
            ui_language_from(Some("1234567890".to_string()), None, None, Some(0x0407)),
            0x0407
        );
    }

    /// No override and no host (non-macOS, no locale env): en-US, the
    /// pre-override behaviour.
    #[test]
    fn no_override_and_no_host_is_en_us() {
        assert_eq!(ui_language_from(None, None, None, None), LANGID_EN_US);
        // A junk LANG (the `C`/`POSIX` locales map to en-US via langid_from_locale).
        assert_eq!(
            ui_language_from(None, Some("C".to_string()), None, Some(0x0407)),
            0x0409
        );
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
