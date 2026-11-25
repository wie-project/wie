//! Application identity (display name and icon) taken from a PE's own
//! resources.
//!
//! The Windows shell describes an executable with the resources the compiler
//! embedded in it, not with the file name: Explorer shows `FileDescription`
//! from the version resource under the icon, and the icon itself comes from the
//! first `RT_GROUP_ICON` group. [`AppIdentity`] is that pair, resolved
//! best-effort:
//!
//! * **name** — `FileDescription`, else `ProductName`, else the executable's
//!   file stem (`NameSource` records which one answered);
//! * **icon** — the payload [`decode_icon_group`] picks for the first group in
//!   directory order (named resources first, then ascending id — the shell's
//!   order), decoded to RGBA8 or handed through as a PNG payload.
//!
//! Nothing here fails: a missing version resource, a missing icon tree or an
//! unreadable file degrades to the stem with no icon. That is the contract the
//! host presenter needs — it must never refuse to open a window because the
//! executable's marketing metadata is thin.

use std::path::Path;

use crate::PeSectionMap;
use crate::resources::{
    IconData, VersionInfo, decode_icon_group, parse_pe_icon_groups, parse_pe_icons,
    parse_pe_version_resources, string_value,
};

/// Where [`AppIdentity::name`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameSource {
    /// `StringFileInfo\<lang>\FileDescription` — the shell's display name.
    FileDescription,
    /// `StringFileInfo\<lang>\ProductName`.
    ProductName,
    /// The executable's file stem: no version resource carried a name.
    ExeStem,
}

/// The identity an executable presents on the desktop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppIdentity {
    /// Human-readable application name (never empty).
    pub name: String,
    /// Which resource — or the stem — supplied [`Self::name`].
    pub name_source: NameSource,
    /// The chosen icon payload, when the image carries a usable one.
    pub icon: Option<IconData>,
}

/// Read the identity of the PE at `path`.
///
/// A file that cannot be read, or whose headers do not parse, still yields an
/// identity: the file stem with [`NameSource::ExeStem`] and no icon.
#[must_use]
pub fn app_identity_from_file(path: &Path) -> AppIdentity {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();
    let Ok(bytes) = std::fs::read(path) else {
        return AppIdentity {
            name: fallback_name(stem),
            name_source: NameSource::ExeStem,
            icon: None,
        };
    };
    let sections = crate::pe_map_plan_from_bytes(&bytes)
        .map(|plan| plan.sections)
        .unwrap_or_default();
    app_identity_from_bytes(stem, &bytes, &sections)
}

/// Read the identity out of already-loaded image bytes.
///
/// `stem` is the executable's file stem, used when no version resource carries
/// a name. `sections` is the section map (see [`crate::PeMapPlan::sections`]).
#[must_use]
pub fn app_identity_from_bytes(stem: &str, image: &[u8], sections: &[PeSectionMap]) -> AppIdentity {
    let versions = parse_pe_version_resources(image, sections);
    let named = versions
        .iter()
        .find_map(|resource| display_name(&resource.info));
    let (name, name_source) = named.unwrap_or_else(|| (fallback_name(stem), NameSource::ExeStem));
    let icon = parse_pe_icon_groups(image, sections)
        .first()
        .and_then(|group| {
            let icons = parse_pe_icons(image, sections);
            decode_icon_group(&icons, group)
        });
    AppIdentity {
        name,
        name_source,
        icon,
    }
}

/// Resolve the display-name chain for one version block.
fn display_name(info: &VersionInfo) -> Option<(String, NameSource)> {
    let description = string_value(info, "FileDescription")
        .map(|value| (value.to_owned(), NameSource::FileDescription));
    let product =
        string_value(info, "ProductName").map(|value| (value.to_owned(), NameSource::ProductName));
    description.or(product)
}

/// The stem as a name, trimmed; never empty.
fn fallback_name(stem: &str) -> String {
    let trimmed = stem.trim();
    if trimmed.is_empty() {
        "app".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Synthetic-image builders and the identity tests.
#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::resources::common::test_util::{
        copy_into, fake_rsrc_section, put_u16, put_u32, put_utf16,
    };
    use std::path::PathBuf;

    /// Pad `buf` to a 4-byte boundary (the `VS_VERSIONINFO` alignment).
    fn pad4(buf: &mut Vec<u8>) {
        while !buf.len().is_multiple_of(4) {
            buf.push(0);
        }
    }

    /// One `VS_VERSIONINFO` node: header, key, optional value, then children.
    fn node(key: &str, value: Option<&str>, children: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        put_u16(&mut buf, 0); // wLength — patched below
        let stored = value.map_or(0, |text| text.encode_utf16().count() + 1);
        put_u16(&mut buf, u16::try_from(stored).expect("value fits u16"));
        put_u16(&mut buf, 1); // wType = text
        put_utf16(&mut buf, key);
        pad4(&mut buf);
        if let Some(text) = value {
            put_utf16(&mut buf, text);
            pad4(&mut buf);
        }
        buf.extend_from_slice(children);
        let total = u16::try_from(buf.len()).expect("node fits u16");
        buf[0..2].copy_from_slice(&total.to_le_bytes());
        buf
    }

    /// A `VS_VERSIONINFO` tree with one string table (`040904b0`) holding the
    /// given `(key, value)` pairs.
    fn version_block(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut table = Vec::new();
        for (key, value) in entries {
            table.extend(node(key, Some(value), &[]));
        }
        let lang = node("040904b0", None, &table);
        let string_info = node("StringFileInfo", None, &lang);
        node("VS_VERSION_INFO", None, &string_info)
    }

    /// Emit an `IMAGE_RESOURCE_DIRECTORY` header followed by one id entry.
    fn dir_with_entry(name: u32, offset: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        put_u32(&mut buf, 0); // characteristics
        put_u32(&mut buf, 0); // timestamp
        put_u16(&mut buf, 0); // major version
        put_u16(&mut buf, 0); // minor version
        put_u16(&mut buf, 0); // named entries
        put_u16(&mut buf, 1); // id entries
        buf.extend_from_slice(&name.to_le_bytes());
        buf.extend_from_slice(&offset.to_le_bytes());
        buf
    }

    /// A synthetic image carrying one `RT_VERSION` resource (id 1, lang
    /// 0x0409) at RVA 0x1200 and the given `VS_VERSIONINFO` block.
    fn image_with_version(block: &[u8]) -> (Vec<u8>, Vec<PeSectionMap>) {
        let sections = vec![fake_rsrc_section()];
        let mut image = vec![0_u8; 0x1400];
        // Root @0x200: RT_VERSION (16) → type dir @0x218.
        copy_into(&mut image, 0x200, &dir_with_entry(16, 0x8000_0018));
        // Type dir @0x218: template id 1 → lang dir @0x240.
        copy_into(&mut image, 0x218, &dir_with_entry(1, 0x8000_0040));
        // Lang dir @0x240: en-US → data entry @0x268.
        copy_into(&mut image, 0x240, &dir_with_entry(0x0409, 0x68));
        // Data entry @0x268 → block at RVA 0x1200 (file 0x400).
        let mut data = Vec::new();
        data.extend_from_slice(&0x1200_u32.to_le_bytes());
        data.extend_from_slice(&u32::try_from(block.len()).expect("fits u32").to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes()); // code page
        data.extend_from_slice(&0u32.to_le_bytes()); // reserved
        copy_into(&mut image, 0x268, &data);
        copy_into(&mut image, 0x400, block);
        (image, sections)
    }

    #[test]
    fn name_chain_prefers_file_description_then_product_name_then_stem() {
        let (image, sections) = image_with_version(&version_block(&[
            ("FileDescription", "Synthetic Editor"),
            ("ProductName", "Synthetic Suite"),
        ]));
        let identity = app_identity_from_bytes("editor", &image, &sections);
        assert_eq!(identity.name, "Synthetic Editor");
        assert_eq!(identity.name_source, NameSource::FileDescription);
        assert!(identity.icon.is_none(), "no icon resource in this image");

        // FileDescription absent → ProductName.
        let (image, sections) = image_with_version(&version_block(&[
            ("ProductName", "Synthetic Suite"),
            ("CompanyName", "Synthetic Corp"),
        ]));
        let identity = app_identity_from_bytes("editor", &image, &sections);
        assert_eq!(identity.name, "Synthetic Suite");
        assert_eq!(identity.name_source, NameSource::ProductName);

        // Neither key → the executable's stem.
        let (image, sections) =
            image_with_version(&version_block(&[("CompanyName", "Synthetic Corp")]));
        let identity = app_identity_from_bytes("editor", &image, &sections);
        assert_eq!(identity.name, "editor");
        assert_eq!(identity.name_source, NameSource::ExeStem);

        // No version tree at all (and no parsable headers) → the stem, and an
        // empty stem never yields an empty name.
        let identity = app_identity_from_bytes("editor", &[0_u8; 64], &[]);
        assert_eq!(identity.name, "editor");
        assert_eq!(identity.name_source, NameSource::ExeStem);
        let blank = app_identity_from_bytes("  ", &[], &[]);
        assert_eq!(blank.name, "app");
    }

    #[test]
    fn truncated_version_resource_falls_back_to_the_stem() {
        let block = version_block(&[("FileDescription", "Synthetic Editor")]);
        let (mut image, sections) = image_with_version(&block);
        // Claim the block is longer than the image holds.
        image[0x268..0x26C].copy_from_slice(&(0x1200_u32).to_le_bytes());
        image[0x26C..0x270].copy_from_slice(&u32::MAX.to_le_bytes());
        let identity = app_identity_from_bytes("editor", &image, &sections);
        assert_eq!(identity.name, "editor");
        assert_eq!(identity.name_source, NameSource::ExeStem);
    }

    /// The local `real_exes/` fixtures are not tracked by git; skip when the
    /// checkout does not carry them.
    fn real_exe(name: &str) -> Option<PathBuf> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../real_exes")
            .join(name);
        path.is_file().then_some(path)
    }

    #[test]
    fn doomretro_identity_comes_from_the_binary() {
        let Some(path) = real_exe("doomretro/doomretro.exe") else {
            return;
        };
        let identity = app_identity_from_file(&path);
        assert_eq!(identity.name, "DOOM Retro");
        assert_eq!(identity.name_source, NameSource::FileDescription);
        match identity.icon.expect("the 256 px PNG icon") {
            IconData::Png(bytes) => assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G'])),
            other => panic!("expected the PNG entry, got {other:?}"),
        }
    }

    #[test]
    fn notepad_identity_comes_from_the_binary() {
        let Some(path) = real_exe("notepad.exe") else {
            return;
        };
        let identity = app_identity_from_file(&path);
        assert_eq!(identity.name, "ReactOS Notepad");
        assert_eq!(identity.name_source, NameSource::FileDescription);
        match identity.icon.expect("a decoded DIB icon") {
            IconData::Rgba(image) => {
                assert_eq!((image.width, image.height), (48, 48));
                assert!(!image.is_uniform());
            }
            other => panic!("expected a DIB, got {other:?}"),
        }
    }

    #[test]
    fn unreadable_file_yields_the_stem() {
        let identity = app_identity_from_file(Path::new("/nonexistent/none/wie-missing.exe"));
        assert_eq!(identity.name, "wie-missing");
        assert_eq!(identity.name_source, NameSource::ExeStem);
        assert!(identity.icon.is_none());
    }
}
