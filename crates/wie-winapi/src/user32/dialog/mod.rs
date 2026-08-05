//! Modal dialog machinery: `CreateDialogParamA/W`, `EndDialog`,
//! `IsDialogMessageA/W`, `GetDlgItem*`, `SetDlgItemText*`, `DefDlgProc*`.
//!
//! The modal loop itself runs in-guest as a `GuestStubKind::DialogBoxParam`
//! stub (see `wie-runtime/src/guest_stubs.rs`); every piece of dialog logic
//! lives here on the host, split by concern:
//!
//! * [`template`] — `hInstance`+id template resolution (with the synthesized
//!   fallback when the id is missing).
//! * [`modal`] — the modal lifecycle: dialog window/child construction and
//!   `EndDialog` teardown.
//! * [`proc`] — the dialog-proc surface: `IsDialogMessage` keyboard routing
//!   and `DefDlgProc`.
//! * [`api`] — the `GetDlgItem*` / `SetDlgItemText*` / `SendDlgItemMessageW`
//!   accessors.
//! * [`paint`] — face + border painting into the owner surface.
//!
//! Deferred (documented in the structural-tier design): mnemonics, arrow-key
//! navigation, `WM_GETDLGCODE`-driven navigation, `DLGTEMPLATEEX`.

mod api;
mod modal;
mod paint;
mod proc;
mod template;

pub(crate) use paint::paint_dialog;

pub use api::{
    handle_get_dlg_item_a, handle_get_dlg_item_int, handle_get_dlg_item_text_a,
    handle_get_dlg_item_text_w, handle_get_dlg_item_w, handle_send_dlg_item_message_w,
    handle_set_dlg_item_int, handle_set_dlg_item_text_a, handle_set_dlg_item_text_w,
};
pub use modal::{handle_create_dialog_param_a, handle_create_dialog_param_w, handle_end_dialog};
pub use proc::{
    handle_def_dlg_proc_a, handle_def_dlg_proc_w, handle_is_dialog_message_a,
    handle_is_dialog_message_w,
};
