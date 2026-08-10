//! USER32 classic clipboard + the guest-side clipboard data store.
//!
//! The clipboard DATA lives in [`ClipboardStore`], a field of
//! [`crate::ole32::OleState`] (the only shared-state slot this lane is allowed
//! to grow — `state/mod.rs` is frozen). That store is authoritative for both
//! the classic USER32 API family below AND the OLE clipboard handlers in
//! `ole32::ole_clipboard` (OleGetClipboard synthesizes a host `IDataObject`
//! that wraps the same store), so an in-emulator copy/paste round-trip works
//! through either entry point.
//!
//! macOS NSPasteboard bridging is a future lane: the store is purely
//! guest-side, exactly like a single-process QClipboard that Qt widgets share
//! — cross-app paste requires a host bridge that is out of scope here.
//!
//! # Dispatch note
//!
//! [`dispatch_clipboard`] is the string dispatch lane for the USER32 surface.
//! It is NOT wired into `dispatch_winapi` yet: the only USER32 soft-dispatch
//! entry (user32/mod.rs `dispatch_user32_extra`) and the dense `WinApiId`
//! table are both frozen files, so a guest import of e.g.
//! `user32.dll!SetClipboardData` currently stops with "unsupported WinAPI
//! call". The handlers are reachable directly in tests; wiring the lane is a
//! one-line addition to `dispatch_user32_extra` once that file is editable.

use anyhow::{Context, Result};

use ahash::HashMap;
use ahash::HashMapExt;

use crate::HandlerContext;
use crate::WinApiHandlerResult;
use crate::state::WinApiState;

/// `CF_TEXT` — plain ANSI text (winuser.h). The edit-control clipboard
/// contract (`WM_COPY` → `WM_PASTE`) and the OLE round-trip micro both use it.
pub const CF_TEXT: u32 = 1;
/// `CF_UNICODETEXT` — UTF-16LE text (winuser.h).
pub const CF_UNICODETEXT: u32 = 13;
/// First registered-format id (`RegisterClipboardFormat` hands out 0xC000+).
const FIRST_REGISTERED_FORMAT: u16 = 0xC000;

/// `ERROR_ACCESS_DENIED` — OpenClipboard/SetClipboardData & friends fail with
/// this when the clipboard is not open.
const ERROR_ACCESS_DENIED: u32 = 5;
/// `ERROR_OUTOFMEMORY` — GetClipboardData allocation failure.
const ERROR_OUTOFMEMORY: u32 = 8;

/// Cap for reading an HGLOBAL payload that is not a live heap block (a static
/// guest buffer): a text format is NUL-scanned, raw formats fall back to this.
const HGLOBAL_READ_CAP: usize = 64 * 1024;

/// The guest-side clipboard: registered-format registry + CF_* data +
/// OpenClipboard control state.
///
/// Owned by [`crate::ole32::OleState`] (see the module header); the legacy
/// `ClipboardState` text slot stays as the edit-control mirror (WM_COPY writes
/// only that slot), so the two are kept in sync at the handler boundary.
#[derive(Debug, Clone)]
pub struct ClipboardStore {
    /// Registered format names (ASCII-lowercased) → registered id.
    registry: HashMap<String, u16>,
    /// Next registered-format id to hand out (0xC000..=0xFFFF).
    next_registered_id: u16,
    /// Clipboard payloads: format → bytes (CF_TEXT/CF_UNICODETEXT include the
    /// NUL terminator, matching what Windows HGLOBALs carry).
    formats: HashMap<u32, Vec<u8>>,
    /// Whether the clipboard is currently open (OpenClipboard … CloseClipboard).
    open: bool,
    /// The window that opened the clipboard (`OpenClipboard(hwnd)`).
    owner: u64,
}

impl Default for ClipboardStore {
    fn default() -> Self {
        Self {
            registry: HashMap::new(),
            next_registered_id: FIRST_REGISTERED_FORMAT,
            formats: HashMap::new(),
            open: false,
            owner: 0,
        }
    }
}

impl ClipboardStore {
    /// Register (or re-lookup) `name` → id, case-insensitively, starting at
    /// `0xC000` (the standard registered-format range). Returns 0 when the
    /// id space is exhausted.
    #[must_use]
    pub fn register_format(&mut self, name: &str) -> u16 {
        let key = name.to_ascii_lowercase();
        if let Some(&id) = self.registry.get(&key) {
            return id;
        }
        let id = self.next_registered_id;
        if id > u16::MAX.saturating_sub(1) {
            return 0;
        }
        self.next_registered_id = id.saturating_add(1);
        self.registry.insert(key, id);
        id
    }

    /// The registered id for `name`, if it was registered before.
    #[must_use]
    pub fn registered_format_id(&self, name: &str) -> Option<u16> {
        self.registry.get(&name.to_ascii_lowercase()).copied()
    }

    /// Open the clipboard with `hwnd` as owner. Fails (leaves it closed) when
    /// already open — OpenClipboard-while-open → FALSE + ERROR_ACCESS_DENIED.
    #[must_use]
    pub fn open_with(&mut self, hwnd: u64) -> bool {
        if self.open {
            return false;
        }
        self.open = true;
        self.owner = hwnd;
        true
    }

    /// Close the clipboard. Fails when it was not open.
    #[must_use]
    pub fn close(&mut self) -> bool {
        if !self.open {
            return false;
        }
        self.open = false;
        self.owner = 0;
        true
    }

    /// Whether the clipboard is currently open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Store `bytes` under `format` (replacing any previous payload).
    pub fn set_data(&mut self, format: u32, bytes: Vec<u8>) {
        self.formats.insert(format, bytes);
    }

    /// The payload stored under `format`, if any.
    #[must_use]
    pub fn data(&self, format: u32) -> Option<&[u8]> {
        self.formats.get(&format).map(Vec::as_slice)
    }

    /// Whether any payload is stored under `format`.
    #[must_use]
    pub fn has_format(&self, format: u32) -> bool {
        self.formats.contains_key(&format)
    }

    /// Remove one format's payload (registered names survive).
    pub fn remove_format(&mut self, format: u32) {
        self.formats.remove(&format);
    }

    /// `EmptyClipboard`: clear every payload; the format-name registry keeps
    /// its ids.
    pub fn clear_data(&mut self) {
        self.formats.clear();
    }

    /// The stored formats in ascending id order — the `EnumClipboardFormats`
    /// iteration order (0 → first, else next-greater, 0 at the end).
    #[must_use]
    pub fn formats_sorted(&self) -> Vec<u32> {
        let mut formats: Vec<u32> = self.formats.keys().copied().collect();
        formats.sort_unstable();
        formats
    }

    /// The format that follows `format` in enumeration order (0 = start).
    #[must_use]
    pub fn next_format(&self, format: u32) -> u32 {
        self.formats_sorted()
            .into_iter()
            .find(|&candidate| candidate > format)
            .unwrap_or(0)
    }
}

/// Handles `USER32.dll!IsClipboardFormatAvailable`.
///
/// Win64 ABI: `rcx` = the clipboard format. Returns TRUE when the clipboard
/// store holds `fmt`, or when `fmt` is `CF_TEXT` and the legacy edit-control
/// mirror holds text (WM_COPY writes that slot).
pub fn handle_is_clipboard_format_available(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let format = engine
        .read_rcx()
        .context("failed to read RCX for IsClipboardFormatAvailable")?;
    let format = u32::try_from(format & 0xffff_ffff).unwrap_or(0);
    let stored = { ctx.state.ole32().clipboard.has_format(format) };
    let legacy_text = format == CF_TEXT && ctx.state.clipboard().has_text();
    ctx.finish(u64::from(stored || legacy_text))
}

/// Handles `USER32.dll!RegisterClipboardFormatW`.
///
/// Win64 ABI: `rcx` = `LPCWSTR lpszFormat`. Returns the 0xC000+ id (0 on
/// failure — NULL/empty name).
pub fn handle_register_clipboard_format_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let name_va = engine
        .read_rcx()
        .context("failed to read RCX for RegisterClipboardFormatW")?;
    let name = crate::guest_string::read_utf16_lossy(engine, name_va, 256)?;
    let id = register_format_name(ctx, &name);
    ctx.finish(u64::from(id))
}

/// Handles `USER32.dll!RegisterClipboardFormatA`.
///
/// Win64 ABI: `rcx` = `LPCSTR lpszFormat`. The A-path decode is UTF-8-first
/// with a Windows-1252 fallback (mingw literals are UTF-8).
pub fn handle_register_clipboard_format_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let name_va = engine
        .read_rcx()
        .context("failed to read RCX for RegisterClipboardFormatA")?;
    let name = crate::guest_string::read_ansi_lossy(engine, name_va, 256)?;
    let id = register_format_name(ctx, &name);
    ctx.finish(u64::from(id))
}

/// Register `name` unless it is empty (Windows returns 0 for a NULL/empty
/// format name).
fn register_format_name(ctx: &mut HandlerContext<'_>, name: &str) -> u16 {
    if name.is_empty() {
        return 0;
    }
    ctx.state.ole32().clipboard.register_format(name)
}

/// Handles `USER32.dll!OpenClipboard`.
///
/// Win64 ABI: `rcx` = `HWND hwndNewOwner`. Returns TRUE, or FALSE with
/// `ERROR_ACCESS_DENIED` when the clipboard is already open (honest, per the
/// documented contract).
pub fn handle_open_clipboard(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let hwnd = engine
        .read_rcx()
        .context("failed to read RCX for OpenClipboard")?;
    let opened = ctx.state.ole32().clipboard.open_with(hwnd);
    if !opened {
        ctx.state.process.last_error = ERROR_ACCESS_DENIED;
    }
    ctx.finish(u64::from(opened))
}

/// Handles `USER32.dll!CloseClipboard`.
///
/// Returns TRUE; FALSE + `ERROR_ACCESS_DENIED` when the clipboard was not open.
pub fn handle_close_clipboard(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let closed = ctx.state.ole32().clipboard.close();
    if !closed {
        ctx.state.process.last_error = ERROR_ACCESS_DENIED;
    }
    ctx.finish(u64::from(closed))
}

/// Handles `USER32.dll!EmptyClipboard`.
///
/// Clears every stored payload (registered format names keep their ids) and
/// the legacy text mirror. FALSE + `ERROR_ACCESS_DENIED` when not open.
pub fn handle_empty_clipboard(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let open = ctx.state.ole32().clipboard.is_open();
    if !open {
        ctx.state.process.last_error = ERROR_ACCESS_DENIED;
        return ctx.finish(0);
    }
    ctx.state.ole32().clipboard.clear_data();
    ctx.state.clipboard().clear();
    ctx.finish(1)
}

/// Handles `USER32.dll!SetClipboardData`.
///
/// Win64 ABI: `rcx` = `UINT uFormat`, `rdx` = `HANDLE hMem`. Reads the payload
/// from the guest heap block (or, for text formats, a NUL-scanned static
/// buffer), stores it under `uFormat`, and returns `hMem` — NULL with
/// `ERROR_ACCESS_DENIED` when the clipboard is not open.
pub fn handle_set_clipboard_data(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let format = engine
        .read_rcx()
        .context("failed to read RCX for SetClipboardData")?;
    let hmem = engine
        .read_rdx()
        .context("failed to read RDX for SetClipboardData")?;
    let format = u32::try_from(format & 0xffff_ffff).unwrap_or(0);

    let open = ctx.state.ole32().clipboard.is_open();
    if !open {
        ctx.state.process.last_error = ERROR_ACCESS_DENIED;
        return ctx.finish(0);
    }
    if hmem == 0 {
        return ctx.finish(0);
    }
    let bytes = read_hglobal_bytes(engine, ctx.state, hmem, format)?;
    store_clipboard_format_bytes(ctx.state, format, bytes);
    ctx.finish(hmem)
}

/// Handles `USER32.dll!GetClipboardData`.
///
/// Win64 ABI: `rcx` = `UINT uFormat`. Allocates a guest heap block holding the
/// stored bytes and returns its VA (the HGLOBAL), or NULL when the format is
/// absent / the clipboard is not open (`ERROR_ACCESS_DENIED`).
pub fn handle_get_clipboard_data(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let format = engine
        .read_rcx()
        .context("failed to read RCX for GetClipboardData")?;
    let format = u32::try_from(format & 0xffff_ffff).unwrap_or(0);

    let open = ctx.state.ole32().clipboard.is_open();
    if !open {
        ctx.state.process.last_error = ERROR_ACCESS_DENIED;
        return ctx.finish(0);
    }
    let Some(bytes) = clipboard_format_bytes(ctx.state, format) else {
        return ctx.finish(0);
    };
    let size = u64::try_from(bytes.len()).context("clipboard payload length overflow")?;
    let va = ctx
        .state
        .heap_state
        .heap
        .alloc_coherent(engine, size.max(16));
    if va == 0 {
        ctx.state.process.last_error = ERROR_OUTOFMEMORY;
        return ctx.finish(0);
    }
    engine.mem_write(va, &bytes)?;
    ctx.finish(va)
}

/// Handles `USER32.dll!EnumClipboardFormats`.
///
/// Win64 ABI: `rcx` = the previous format (0 starts the enumeration). Returns
/// the next stored format id, or 0 at the end. FALSE (0) + `ERROR_ACCESS_DENIED`
/// when the clipboard is not open.
pub fn handle_enum_clipboard_formats(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let format = engine
        .read_rcx()
        .context("failed to read RCX for EnumClipboardFormats")?;
    let format = u32::try_from(format & 0xffff_ffff).unwrap_or(0);

    let open = ctx.state.ole32().clipboard.is_open();
    if !open {
        ctx.state.process.last_error = ERROR_ACCESS_DENIED;
        return ctx.finish(0);
    }
    let next = ctx.state.ole32().clipboard.next_format(format);
    ctx.finish(u64::from(next))
}

/// String dispatch lane for the USER32 classic-clipboard surface.
///
/// Currently reachable only from tests (see the module header for the
/// routing note); once `dispatch_user32_extra` is editable, this is the
/// one-call wiring point.
pub fn dispatch_clipboard(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "registerclipboardformatw" => Ok(Some(handle_register_clipboard_format_w(ctx)?)),
        "registerclipboardformata" => Ok(Some(handle_register_clipboard_format_a(ctx)?)),
        "openclipboard" => Ok(Some(handle_open_clipboard(ctx)?)),
        "closeclipboard" => Ok(Some(handle_close_clipboard(ctx)?)),
        "emptyclipboard" => Ok(Some(handle_empty_clipboard(ctx)?)),
        "setclipboarddata" => Ok(Some(handle_set_clipboard_data(ctx)?)),
        "getclipboarddata" => Ok(Some(handle_get_clipboard_data(ctx)?)),
        "enumclipboardformats" => Ok(Some(handle_enum_clipboard_formats(ctx)?)),
        _ => Ok(None),
    }
}

/// Read the bytes behind an HGLOBAL / hGlobal pointer.
///
/// Text formats (CF_TEXT / CF_UNICODETEXT) are NUL-scanned so a static guest
/// buffer works; raw formats read the live heap block's allocated size, or
/// fall back to a bounded NUL-scan for non-heap pointers.
pub(crate) fn read_hglobal_bytes(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    hglobal: u64,
    format: u32,
) -> Result<Vec<u8>> {
    if hglobal == 0 {
        return Ok(Vec::new());
    }
    match format {
        CF_TEXT => read_ansi_bytes_until_nul(engine, hglobal, HGLOBAL_READ_CAP),
        CF_UNICODETEXT => {
            // Lossy decode is fine for a text format; re-encode + NUL so the
            // stored payload round-trips as a proper UTF-16 C string.
            let text = crate::guest_string::read_utf16_lossy(engine, hglobal, 32 * 1024)?;
            let mut bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
            bytes.extend_from_slice(&0_u16.to_le_bytes());
            Ok(bytes)
        }
        _ => {
            if let Some(size) = state.heap_state.heap.size_of(hglobal) {
                let size = usize::try_from(size)
                    .unwrap_or(HGLOBAL_READ_CAP)
                    .min(HGLOBAL_READ_CAP);
                let mut bytes = vec![0_u8; size];
                engine.mem_read(hglobal, &mut bytes)?;
                Ok(bytes)
            } else {
                read_ansi_bytes_until_nul(engine, hglobal, HGLOBAL_READ_CAP)
            }
        }
    }
}

/// The bytes the clipboard currently holds for `format` (store first, legacy
/// text mirror second for CF_TEXT / CF_UNICODETEXT).
pub(crate) fn clipboard_format_bytes(state: &mut WinApiState, format: u32) -> Option<Vec<u8>> {
    let stored = state.ole32().clipboard.data(format).map(<[u8]>::to_vec);
    if stored.is_some() {
        return stored;
    }
    match format {
        CF_TEXT => {
            let text = state.clipboard().text()?;
            let mut bytes = crate::guest_string::encode_cp1252(text);
            bytes.push(0);
            Some(bytes)
        }
        CF_UNICODETEXT => {
            let text = state.clipboard().text()?;
            let mut bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
            bytes.extend_from_slice(&0_u16.to_le_bytes());
            Some(bytes)
        }
        _ => None,
    }
}

/// Store `bytes` under `format`, mirroring text formats into the legacy
/// edit-control clipboard slot (so WM_PASTE sees them).
pub(crate) fn store_clipboard_format_bytes(state: &mut WinApiState, format: u32, bytes: Vec<u8>) {
    state.ole32().clipboard.set_data(format, bytes.clone());
    match format {
        CF_TEXT => {
            let text = crate::guest_string::decode_ansi_lossy(&bytes);
            state.clipboard().set_text(text);
        }
        CF_UNICODETEXT => {
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|pair| {
                    u16::from_le_bytes([
                        pair.first().copied().unwrap_or(0),
                        pair.get(1).copied().unwrap_or(0),
                    ])
                })
                .take_while(|&unit| unit != 0)
                .collect();
            state.clipboard().set_text(String::from_utf16_lossy(&units));
        }
        _ => {}
    }
}

/// Read up to `max` NUL-terminated bytes from guest memory (NUL included).
fn read_ansi_bytes_until_nul(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    max: usize,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(max.min(4096));
    let mut scratch = [0_u8; 4096];
    let mut remaining = max;
    let mut cursor = address;
    while remaining > 0 {
        let want = remaining.min(scratch.len());
        let slice = scratch.get_mut(..want).context("clipboard slice bounds")?;
        engine
            .mem_read(cursor, slice)
            .context("failed to read clipboard HGLOBAL")?;
        let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
        let taken = end.saturating_add(usize::from(end < slice.len()));
        let head = slice.get(..taken).context("clipboard head bounds")?;
        bytes.extend_from_slice(head);
        if end < slice.len() {
            break;
        }
        cursor = cursor.wrapping_add(u64::try_from(slice.len()).unwrap_or(0));
        remaining = remaining.saturating_sub(slice.len());
    }
    Ok(bytes)
}
