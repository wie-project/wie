//! Dialog template resolution: `hInstance`+id → `DialogTemplate`.
//!
//! Only numeric ids are addressable by `DialogBoxParam`; a name pointer
//! (high 16 bits nonzero) references a string-table template the parser does
//! not resolve. A missing id synthesizes a default dialog (per the design:
//! fallback only).

use crate::user32::lang;
use crate::user32::{WS_CLIPCHILDREN, WS_VISIBLE, WinApiState};
use wie_pe::resources::DialogTemplate;

/// Synthesized fallback dialog size (pixels) when a template id is missing.
const DEFAULT_DIALOG_CX: i32 = 300;
const DEFAULT_DIALOG_CY: i32 = 200;

/// Resolve the `hInstance`+id dialog template, synthesizing a default dialog
/// (per the design: fallback only) when the id is missing or not addressable.
pub(crate) fn resolve_template(
    state: &WinApiState,
    image_base: u64,
    instance_handle: u64,
    template_value: u64,
) -> DialogTemplate {
    // Only numeric ids are addressable by DialogBoxParam. A name pointer
    // (high 16 bits nonzero) references a string-table template the parser
    // does not resolve.
    let template_id =
        (template_value >> 16 == 0).then(|| u16::try_from(template_value & 0xFFFF).unwrap_or(0));

    let dialogs: Vec<&DialogTemplate> = if instance_handle == image_base {
        state.process.main_module_dialogs.iter().collect()
    } else {
        state
            .module_state
            .loaded_modules
            .values()
            .filter(|module| module.image_base == instance_handle)
            .flat_map(|module| module.dialogs.iter())
            .collect()
    };

    template_id
        .and_then(|id| find_template(&dialogs, id, lang::ui_language()))
        .cloned()
        .unwrap_or_else(synthesized_default)
}

/// Find a template by numeric id, honoring the UI language when the id exists
/// in several locales (exact LANGID → neutral → en-US → first in directory
/// order).
fn find_template<'a>(
    dialogs: &'a [&DialogTemplate],
    id: u16,
    ui_language: u32,
) -> Option<&'a DialogTemplate> {
    lang::resolve_block(
        dialogs
            .iter()
            .copied()
            .filter(|dialog| dialog.name == id)
            .map(|dialog| (u32::from(dialog.lang), dialog)),
        ui_language,
    )
}

/// Synthesized fallback dialog: `"WIE Dialog"`, ~300×200 px, no items.
fn synthesized_default() -> DialogTemplate {
    DialogTemplate {
        name: 0,
        lang: 0,
        style: WS_VISIBLE | WS_CLIPCHILDREN,
        ex_style: 0,
        x: 0,
        y: 0,
        cx: i16::try_from(DEFAULT_DIALOG_CX / 2).unwrap_or(0),
        cy: i16::try_from(DEFAULT_DIALOG_CY / 2).unwrap_or(0),
        title: "WIE Dialog".to_owned(),
        font_point: None,
        font_face: None,
        pixel_rect: wie_pe::resources::PixelRect {
            x: 0,
            y: 0,
            cx: DEFAULT_DIALOG_CX,
            cy: DEFAULT_DIALOG_CY,
        },
        items: Vec::new(),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use wie_pe::resources::PixelRect;

    /// Template with just the id/lang/title fields resolution reads.
    fn template(name: u16, lang: u16, title: &str) -> DialogTemplate {
        DialogTemplate {
            name,
            lang,
            style: 0,
            ex_style: 0,
            x: 0,
            y: 0,
            cx: 0,
            cy: 0,
            title: title.to_owned(),
            font_point: None,
            font_face: None,
            pixel_rect: PixelRect {
                x: 0,
                y: 0,
                cx: 0,
                cy: 0,
            },
            items: Vec::new(),
        }
    }

    /// `find_template` runs the same locale chain as the string/menu
    /// resolvers: German 0x0007 first in directory order, en-US 0x0409 second.
    #[test]
    fn find_template_picks_ui_language_locale() {
        let dialogs = [
            template(100, 0x0007, "German"),
            template(100, 0x0409, "English"),
        ];
        let refs: Vec<&DialogTemplate> = dialogs.iter().collect();

        // en-US UI: the 0x0409 template wins over the first (German) block.
        assert_eq!(
            find_template(&refs, 100, 0x0409).expect("found").title,
            "English"
        );
        // German UI: the 0x0007 template wins.
        assert_eq!(
            find_template(&refs, 100, 0x0007).expect("found").title,
            "German"
        );
        // Absent locale (French 0x040C): exact and neutral miss, 0x0409 wins.
        assert_eq!(
            find_template(&refs, 100, 0x040C).expect("found").title,
            "English"
        );
        // German-Austria 0x0407: exact misses, neutral 0x0007 matches.
        assert_eq!(
            find_template(&refs, 100, 0x0407).expect("found").title,
            "German"
        );
        // Unknown id resolves to nothing.
        assert!(find_template(&refs, 99, 0x0409).is_none());
    }
}
