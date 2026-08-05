//! RT_DIALOG, RT_MENU, RT_STRING, and RT_ACCELERATOR resource parsing for PE images.
//!
//! Walks the `IMAGE_RESOURCE_DIRECTORY` tree in the `.rsrc` section (PE
//! resource format, Microsoft Learn) and parses each `RT_DIALOG` template
//! (type **5** — note: 16 is `RT_VERSION`) into a [`DialogTemplate`], each
//! `RT_MENU` template (type **4**) into a [`MenuTemplate`], and each
//! `RT_STRING` block (type **6**) into a [`StringBlock`].
//! Parsing is best-effort: malformed or out-of-bounds structures skip that
//! entry, and a missing/malformed resource tree yields an empty `Vec` — a
//! broken resource section never fails the module load.
//!
//! # Layout
//!
//! The shared resource-tree walk lives in `common`; each resource type is
//! parsed by its own submodule (`dialog`, `menu`, `string`, `accel`). The
//! public API is re-exported here unchanged.
//!
//! # Template layout notes
//!
//! Two on-disk variants of the standard dialog template exist:
//!
//! * The `winuser.h` `DLGTEMPLATE` layout, emitted by `windres` and MSVC
//!   `rc.exe`: `style, exStyle, cDlgItems, x, y, cx, cy` (18 bytes).
//! * The layout documented on Microsoft Learn, which prefixes
//!   `dlgVer = 1, signature = 0, helpID` (28 bytes).
//!
//! Both are detected and parsed. `DLGTEMPLATEEX` (`dlgVer = 1`,
//! `signature = 0xFFFF`) is not parsed — it is skipped with a comment.
//!
//! Dialog units (DLUs) convert to pixels at the standard `GetDialogBaseUnits`
//! 8×16 font: `px = dlu * base / 4` on the x axis and `dlu * base / 8` on the
//! y axis, which is `dlu * 2` on both axes. Both the raw DLUs and the
//! computed pixel rect are stored.

mod accel;
mod common;
mod dialog;
mod menu;
mod string;
mod version;

pub use accel::{AccelEntry, AccelTemplate, parse_accelerators};
pub use dialog::{
    DialogItemTemplate, DialogTemplate, ItemClass, PixelRect, dlu_to_px, parse_dialogs,
};
pub use menu::{MenuItemTemplate, MenuTemplate, parse_menus};
pub use string::{StringBlock, parse_strings};
pub use version::{
    FIXED_FILE_INFO_SIGNATURE, FIXED_FILE_INFO_SIZE, FixedFileInfo, StringTableBlock,
    StringTableEntry, VersionInfo, VersionQueryMatch, VersionResource, parse_pe_version_resources,
    parse_version_info, query_version_value,
};
