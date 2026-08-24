//! Directory change notifications: `FindFirstChangeNotificationW/A`,
//! `FindNextChangeNotification`, `FindCloseChangeNotification`, and
//! `ReadDirectoryChangesW` (synchronous form only).
//!
//! # Divergence from Windows
//!
//! A real `ReadDirectoryChangesW` takes a directory **handle** obtained from
//! `CreateFileW(path, FILE_LIST_DIRECTORY, ...)`. `CreateFileW` cannot open a
//! directory as a usable kernel handle here, so the watch is keyed on the
//! guest **path**:
//!
//! - `FindFirstChangeNotificationW/A` resolves the path string to a host path
//!   (the common `QFileSystemWatcher`-style usage) and registers a waitable
//!   `KernelObject::DirectoryWatch`.
//! - `CreateFileW` with `FILE_LIST_DIRECTORY` (0x1, the value `FILE_READ_DATA`
//!   aliases for directory handles) on an existing mapped directory succeeds
//!   and returns an `OpenGuestFile` anchor whose `host_path` is the directory;
//!   `ReadDirectoryChangesW` resolves that handle → host path.
//! - Overlapped I/O is not supported: a non-NULL `lpOverlapped` /
//!   `lpCompletionRoutine` fails fast with `ERROR_INVALID_PARAMETER`.
//!
//! The synchronous blocking form parks via [`crate::HostParkReason::PthreadWait`]
//! — the runtime drops the process locks for ~1 ms and re-dispatches the same
//! fake stop, so the handler re-enters with its original register args and
//! retries the drain. Workers can therefore run (and create the watched file)
//! while the primary waits.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::{
    Context, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, ERROR_PATH_NOT_FOUND, HandlerContext,
    INVALID_HANDLE_VALUE, Result, WinApiHandlerResult, WinApiState, checked_address,
    guest_dir_exists, low_u32, paths_match_guest, read_ansi_string_from_cpu, read_stack_u64,
    read_wide_string_from_cpu, resolve_full_windows_path, write_guest_u32,
};
use crate::guest_layout::FileNotifyInformation;
use crate::guest_memory::with_typed_write;
use crate::sync_obj::{
    DirectoryWatchObject, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED,
    FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME, FILE_NOTIFY_CHANGE_ATTRIBUTES,
    FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_ACCESS,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FileNotifyRecord,
};
use wie_cpu::CpuEngine;

/// `ERROR_NOTIFY_ENUM_DIR` — the `ReadDirectoryChangesW` buffer was too small
/// to hold the next `FILE_NOTIFY_INFORMATION` record (winerror.h).
const ERROR_NOTIFY_ENUM_DIR: u32 = 1022;

/// Handles `KERNEL32.dll!FindFirstChangeNotificationW`.
pub fn handle_find_first_change_notification_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let path_va = engine
        .read_rcx()
        .context("failed to read RCX for FindFirstChangeNotificationW")?;
    let watch_subtree = engine.read_rdx()? != 0;
    let filter = low_u32(
        engine.read_r8()?,
        "FindFirstChangeNotificationW notify filter",
    )?;
    let path = read_wide_string_from_cpu(engine, path_va, 1024)?;
    let return_value = finish_find_first_change(state, &path, watch_subtree, filter);
    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!FindFirstChangeNotificationA`.
pub fn handle_find_first_change_notification_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let path_va = engine
        .read_rcx()
        .context("failed to read RCX for FindFirstChangeNotificationA")?;
    let watch_subtree = engine.read_rdx()? != 0;
    let filter = low_u32(
        engine.read_r8()?,
        "FindFirstChangeNotificationA notify filter",
    )?;
    let path = read_ansi_string_from_cpu(engine, path_va, 1024)?;
    let return_value = finish_find_first_change(state, &path, watch_subtree, filter);
    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!FindNextChangeNotification`.
pub fn handle_find_next_change_notification(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine
        .read_rcx()
        .context("failed to read RCX for FindNextChangeNotification")?;

    let return_value = match state.kernel.sync.object(handle) {
        Some(crate::KernelObject::DirectoryWatch(d)) => {
            d.reset();
            state.process.last_error = 0;
            1
        }
        _ => {
            state.process.last_error = ERROR_INVALID_HANDLE;
            0
        }
    };
    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!FindCloseChangeNotification`.
pub fn handle_find_close_change_notification(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine
        .read_rcx()
        .context("failed to read RCX for FindCloseChangeNotification")?;

    // Deactivate before removal so a concurrently parked waiter wakes, then
    // drop the table ref (dropping the object stops the notify watcher).
    let return_value = match state.kernel.sync.object(handle) {
        Some(crate::KernelObject::DirectoryWatch(d)) => {
            d.deactivate();
            state
                .kernel
                .sync
                .objects
                .remove(&crate::KernelHandle::from(handle));
            state.process.last_error = 0;
            1
        }
        _ => {
            state.process.last_error = ERROR_INVALID_HANDLE;
            0
        }
    };
    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!ReadDirectoryChangesW` (synchronous form only).
pub fn handle_read_directory_changes_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine
        .read_rcx()
        .context("failed to read RCX for ReadDirectoryChangesW")?;
    let buffer_va = engine.read_rdx()?;
    let buffer_len = engine.read_r8()?;
    let watch_subtree = engine.read_r9()? != 0;
    // Win64 stack args: dwNotifyFilter @0x28, lpBytesReturned @0x30,
    // lpOverlapped @0x38, lpCompletionRoutine @0x40.
    let filter = low_u32(
        read_stack_u64(engine, 0x28)?,
        "ReadDirectoryChangesW notify filter",
    )?;
    let bytes_returned_va = read_stack_u64(engine, 0x30)?;
    let overlapped = read_stack_u64(engine, 0x38)?;
    let completion_routine = read_stack_u64(engine, 0x40)?;

    // Documented divergence: no overlapped I/O; the async forms fail fast.
    if overlapped != 0 || completion_routine != 0 {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(0);
    }
    if buffer_va == 0 {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(0);
    }

    // The handle must be a directory opened with FILE_LIST_DIRECTORY access
    // (the anchor registered by CreateFileW); plain file handles are rejected.
    let Some(obj) = state.kernel.sync.watch_handles.get(&handle).cloned() else {
        state.process.last_error = ERROR_INVALID_HANDLE;
        return ctx.finish(0);
    };

    // The filter applies to events delivered from now on (one pending read).
    obj.mask.store(filter, Ordering::Release);
    if let Err(error) = start_watching_if_needed(&obj, watch_subtree) {
        // The watcher could not arm (e.g. the host directory vanished). Fail
        // the call rather than stopping the session.
        tracing::warn!(
            handle,
            error = %error,
            "ReadDirectoryChangesW watch start failed"
        );
        state.process.last_error = ERROR_INVALID_HANDLE;
        return ctx.finish(0);
    }

    // Fast path: a change already arrived (or the watch was closed).
    if obj.try_wait() {
        return match drain_into_guest(engine, &obj, buffer_va, buffer_len)? {
            Some(written) => {
                if bytes_returned_va != 0 {
                    write_guest_u32(engine, bytes_returned_va, written)?;
                }
                state.process.last_error = 0;
                ctx.finish(1)
            }
            None => {
                state.process.last_error = ERROR_NOTIFY_ENUM_DIR;
                ctx.finish(0)
            }
        };
    }

    // Synchronous blocking form: park (drop the process locks for ~1 ms) and
    // re-enter. PthreadWait re-dispatches the same fake stop, so the handler
    // re-runs with the original register args and retries the drain above.
    Err(crate::WinApiControlSignal::HostPark {
        reason: crate::HostParkReason::PthreadWait,
    }
    .into())
}

fn finish_find_first_change(
    state: &mut WinApiState,
    path: &str,
    subtree: bool,
    filter: u32,
) -> u64 {
    if path.trim().is_empty() {
        state.process.last_error = ERROR_PATH_NOT_FOUND;
        return INVALID_HANDLE_VALUE;
    }
    state.file_io.sync_volumes();
    let cwd = state.file_io.cwd_utf8();
    let full_path = resolve_full_windows_path(&cwd, path);
    let Some(host_path) = resolve_watch_host_path(state, &full_path) else {
        state.process.last_error = ERROR_PATH_NOT_FOUND;
        return INVALID_HANDLE_VALUE;
    };

    let (handle, obj) = state
        .kernel
        .sync
        .register_directory_watch(&full_path, host_path, filter);
    if let Err(error) = obj.start_watching(subtree) {
        // The watcher failed to arm (e.g. the host directory vanished between
        // resolution and watch); drop the object and report failure.
        state
            .kernel
            .sync
            .objects
            .remove(&crate::KernelHandle::from(handle));
        tracing::warn!(
            path = %full_path,
            error = %error,
            "FindFirstChangeNotification watch start failed"
        );
        state.process.last_error = ERROR_PATH_NOT_FOUND;
        return INVALID_HANDLE_VALUE;
    }
    state.process.last_error = 0;
    handle
}

/// Resolve an existing guest directory to its host path, or `None` if the
/// path is not a mapped directory (no host backing to watch).
///
/// Shared with the `CreateFileW(FILE_LIST_DIRECTORY)` anchor open
/// (`file_io/mod.rs`) so both watch surfaces agree on what is watchable.
pub(crate) fn resolve_watch_host_path(state: &WinApiState, full_path: &str) -> Option<PathBuf> {
    if !guest_dir_exists(state, full_path) {
        return None;
    }
    crate::vfs::guest_path_to_host(&state.file_io.volumes, full_path)
        .map(|map| map.host)
        .or_else(|| {
            state
                .file_io
                .host_file_mounts
                .iter()
                .find(|mount| paths_match_guest(full_path, &mount.guest_path))
                .map(|mount| mount.host_path.clone())
        })
        .filter(|host| host.is_dir())
}

/// Drain pending records into the guest `FILE_NOTIFY_INFORMATION` buffer,
/// chained via `NextEntryOffset` (0 on the last record).
///
/// Returns `Some(bytes written)` on success, or `None` when the buffer is too
/// small — in which case the records are put back so the next read reports
/// them (real Windows queues until a read succeeds).
fn drain_into_guest(
    engine: &mut dyn CpuEngine,
    obj: &DirectoryWatchObject,
    buffer_va: u64,
    buffer_len: u64,
) -> Result<Option<u32>> {
    let records = std::mem::take(&mut *obj.pending.lock().unwrap_or_else(|p| p.into_inner()));

    let total = records_total(&records)?;
    if total > buffer_len {
        // Restore for the next read; the too-small read consumed nothing.
        if let Ok(mut guard) = obj.pending.lock() {
            let mut restored = records;
            restored.append(&mut *guard);
            *guard = restored;
        }
        return Ok(None);
    }

    let mut cursor = buffer_va;
    for (index, record) in records.iter().enumerate() {
        let name_bytes = utf16_le_bytes(&record.file_name);
        let record_size = u64::try_from(name_bytes.len().saturating_add(12)).unwrap_or(0);
        let next_offset = if index + 1 < records.len() {
            u32::try_from(record_size).context("watch record offset does not fit u32")?
        } else {
            0
        };
        with_typed_write::<FileNotifyInformation, _, _>(engine, cursor, |header| {
            header.next_entry_offset = next_offset;
            header.action = record.action;
            header.file_name_length = u32::try_from(name_bytes.len())
                .context("watch file name length does not fit u32")?;
            Ok(())
        })
        .context("failed to write FILE_NOTIFY_INFORMATION header")?;
        let name_va = checked_address(cursor, 12, "FILE_NOTIFY_INFORMATION.FileName");
        engine
            .mem_write(name_va, &name_bytes)
            .context("failed to write FILE_NOTIFY_INFORMATION file name")?;
        cursor = checked_address(cursor, record_size, "next FILE_NOTIFY_INFORMATION");
    }
    let written = u32::try_from(total).context("watch record bytes do not fit u32")?;
    Ok(Some(written))
}

/// Sum of the packed `FILE_NOTIFY_INFORMATION` record sizes (12-byte header +
/// UTF-16 file name) for the buffer-fit check.
fn records_total(records: &[FileNotifyRecord]) -> Result<u64> {
    let mut total: u64 = 0;
    for record in records {
        let name_len = u64::try_from(record.file_name.encode_utf16().count())
            .context("watch file name units do not fit u64")?
            .saturating_mul(2);
        total = total
            .checked_add(12)
            .and_then(|t| t.checked_add(name_len))
            .context("watch record byte total overflow")?;
    }
    Ok(total)
}

/// UTF-16LE byte encoding of a guest file name (no NUL terminator — the
/// `FileNameLength` field carries the byte count).
fn utf16_le_bytes(name: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(name.len().saturating_mul(2));
    for unit in name.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

// ── notify-crate glue ────────────────────────────────────────────────────

impl DirectoryWatchObject {
    /// Arm the notify watcher for this object (idempotent). `recursive` maps
    /// to `RecursiveMode::Recursive` vs `NonRecursive`.
    ///
    /// The callback holds a **weak** reference so the object's lifetime is
    /// not extended: dropping the last strong ref (close/teardown) drops the
    /// watcher and stops delivery.
    pub fn start_watching(self: &Arc<Self>, recursive: bool) -> notify::Result<()> {
        use notify::Watcher as _;
        if self.watcher.lock().map_or(true, |slot| slot.is_some()) {
            return Ok(());
        }
        let weak = Arc::downgrade(self);
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let Some(obj) = weak.upgrade() else {
                    return;
                };
                obj.handle_notify_event(event);
            })?;
        let mode = if recursive {
            notify::RecursiveMode::Recursive
        } else {
            notify::RecursiveMode::NonRecursive
        };
        watcher.watch(&self.host_path, mode)?;
        if let Ok(mut slot) = self.watcher.lock() {
            *slot = Some(watcher);
        }
        Ok(())
    }

    /// Convert one notify event into a queued [`FileNotifyRecord`] if it
    /// matches the current filter mask.
    fn handle_notify_event(&self, event: notify::Result<notify::Event>) {
        let Ok(event) = event else {
            return;
        };
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        let Some((flag, action)) = Self::map_event_kind(&event.kind) else {
            return;
        };
        if self.mask.load(Ordering::Acquire) & flag == 0 {
            return;
        }
        let Some(name) = self.relative_file_name(&event) else {
            return;
        };
        self.push(FileNotifyRecord {
            action,
            file_name: name,
        });
    }

    /// Map an `EventKind` to `(FILE_NOTIFY_CHANGE_* flag, FILE_ACTION_*)`.
    /// `Create → ADDED`, `Remove → REMOVED`, `Modify → MODIFIED` (renames map
    /// to `RENAMED_OLD/NEW`); the flag side drives the filter mask.
    fn map_event_kind(kind: &notify::EventKind) -> Option<(u32, u32)> {
        use notify::EventKind;
        use notify::event::{AccessKind, CreateKind, ModifyKind, RemoveKind, RenameMode};
        match kind {
            EventKind::Create(kind) => match kind {
                CreateKind::File => Some((FILE_NOTIFY_CHANGE_FILE_NAME, FILE_ACTION_ADDED)),
                CreateKind::Folder => Some((FILE_NOTIFY_CHANGE_DIR_NAME, FILE_ACTION_ADDED)),
                CreateKind::Any | CreateKind::Other => Some((
                    FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME,
                    FILE_ACTION_ADDED,
                )),
            },
            EventKind::Remove(kind) => match kind {
                RemoveKind::File => Some((FILE_NOTIFY_CHANGE_FILE_NAME, FILE_ACTION_REMOVED)),
                RemoveKind::Folder => Some((FILE_NOTIFY_CHANGE_DIR_NAME, FILE_ACTION_REMOVED)),
                RemoveKind::Any | RemoveKind::Other => Some((
                    FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME,
                    FILE_ACTION_REMOVED,
                )),
            },
            EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                Some((FILE_NOTIFY_CHANGE_FILE_NAME, FILE_ACTION_RENAMED_OLD_NAME))
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                Some((FILE_NOTIFY_CHANGE_FILE_NAME, FILE_ACTION_RENAMED_NEW_NAME))
            }
            EventKind::Modify(ModifyKind::Name(_)) => {
                Some((FILE_NOTIFY_CHANGE_FILE_NAME, FILE_ACTION_MODIFIED))
            }
            EventKind::Modify(ModifyKind::Data(_)) => Some((
                FILE_NOTIFY_CHANGE_SIZE | FILE_NOTIFY_CHANGE_LAST_WRITE,
                FILE_ACTION_MODIFIED,
            )),
            EventKind::Modify(ModifyKind::Metadata(_)) => {
                Some((FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_ACTION_MODIFIED))
            }
            EventKind::Modify(ModifyKind::Any | ModifyKind::Other) => {
                Some((FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_ACTION_MODIFIED))
            }
            EventKind::Access(AccessKind::Read) => {
                Some((FILE_NOTIFY_CHANGE_LAST_ACCESS, FILE_ACTION_MODIFIED))
            }
            EventKind::Access(_) => Some((FILE_NOTIFY_CHANGE_LAST_ACCESS, FILE_ACTION_MODIFIED)),
            EventKind::Any | EventKind::Other => {
                Some((FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_ACTION_MODIFIED))
            }
        }
    }

    /// The guest-relative file name for an event (its path minus the watched
    /// host directory), `\`-joined for recursive subdirectory paths.
    fn relative_file_name(&self, event: &notify::Event) -> Option<String> {
        let path = event.paths.first()?;
        let relative = path.strip_prefix(&*self.host_path).ok()?;
        let relative = if relative.as_os_str().is_empty() {
            // The watched directory itself changed (e.g. removed): report the
            // directory's own name.
            std::path::Path::new(path.file_name()?)
        } else {
            relative
        };
        let mut parts = Vec::new();
        for component in relative.components() {
            parts.push(component.as_os_str().to_string_lossy().into_owned());
        }
        if parts.is_empty() {
            return None;
        }
        Some(parts.join("\\"))
    }
}

/// Start the notify watcher for a `ReadDirectoryChangesW` anchor on first use
/// (opening the directory does not spawn threads).
fn start_watching_if_needed(obj: &Arc<DirectoryWatchObject>, recursive: bool) -> Result<()> {
    if obj.watcher.lock().map_or(true, |slot| slot.is_some()) {
        return Ok(());
    }
    obj.start_watching(recursive)
        .context("failed to arm directory watcher for ReadDirectoryChangesW")
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod watch_tests {
    use super::*;
    use wie_cpu::{CpuEngine, IcedCpu, RwxPerms};

    /// Minimal engine with mapped guest memory for drain round-trips.
    fn test_engine() -> IcedCpu {
        let mut cpu = IcedCpu::open_x86_64();
        cpu.mem_map(0x1000, 0x10_0000, RwxPerms::ALL)
            .expect("map test memory");
        cpu
    }

    fn raw_bytes(engine: &mut IcedCpu, va: u64, len: usize) -> Vec<u8> {
        let mut bytes = vec![0_u8; len];
        engine.mem_read(va, &mut bytes).expect("read raw bytes");
        bytes
    }

    fn watch_obj() -> DirectoryWatchObject {
        DirectoryWatchObject::new("C:\\watch", PathBuf::from("/tmp/watch"))
    }

    fn utf16(name: &str) -> Vec<u8> {
        super::utf16_le_bytes(name)
    }

    /// The packed record size is 12 header bytes + 2 bytes per name char.
    #[test]
    fn records_total_is_12_plus_name_bytes() {
        let records = vec![
            FileNotifyRecord {
                action: FILE_ACTION_ADDED,
                file_name: "a.txt".into(),
            },
            FileNotifyRecord {
                action: FILE_ACTION_REMOVED,
                file_name: "b".into(),
            },
        ];
        // "a.txt" = 10 bytes → 22; "b" = 2 bytes → 14; total 36.
        assert_eq!(records_total(&records).expect("total"), 36);
    }

    #[test]
    fn drain_chains_records_with_next_entry_offset() {
        let mut engine = test_engine();
        let obj = watch_obj();
        obj.push(FileNotifyRecord {
            action: FILE_ACTION_ADDED,
            file_name: "a.txt".into(),
        });
        obj.push(FileNotifyRecord {
            action: FILE_ACTION_MODIFIED,
            file_name: "b.txt".into(),
        });

        let written = drain_into_guest(&mut engine, &obj, 0x9000, 64)
            .expect("drain")
            .expect("fits in 64 bytes");
        assert_eq!(written, 22 + 22, "two 22-byte packed records");
        assert!(
            obj.pending.lock().is_ok_and(|g| g.is_empty()),
            "drain consumed records"
        );

        let bytes = raw_bytes(&mut engine, 0x9000, usize::try_from(written).unwrap_or(0));
        // Record 1 @0: NextEntryOffset = 22, Action = ADDED, FileNameLength = 10.
        assert_eq!(&bytes[0..4], &22_u32.to_le_bytes(), "record 1 next offset");
        assert_eq!(
            &bytes[4..8],
            &FILE_ACTION_ADDED.to_le_bytes(),
            "record 1 action"
        );
        assert_eq!(&bytes[8..12], &10_u32.to_le_bytes(), "record 1 name length");
        assert_eq!(&bytes[12..22], &utf16("a.txt")[..], "record 1 file name");
        // Record 2 @22: NextEntryOffset = 0 (last), Action = MODIFIED.
        assert_eq!(
            &bytes[22..26],
            &0_u32.to_le_bytes(),
            "record 2 next offset (last)"
        );
        assert_eq!(
            &bytes[26..30],
            &FILE_ACTION_MODIFIED.to_le_bytes(),
            "record 2 action"
        );
        assert_eq!(
            &bytes[30..34],
            &10_u32.to_le_bytes(),
            "record 2 name length"
        );
        assert_eq!(&bytes[34..44], &utf16("b.txt")[..], "record 2 file name");
    }

    #[test]
    fn drain_restores_records_when_buffer_too_small() {
        let mut engine = test_engine();
        let obj = watch_obj();
        obj.push(FileNotifyRecord {
            action: FILE_ACTION_ADDED,
            file_name: "toolong.txt".into(),
        });

        // Record is 12 + 20 = 32 bytes; a 16-byte buffer must fail and keep
        // the record queued for the next read (real Windows queues until a
        // read succeeds).
        assert_eq!(
            drain_into_guest(&mut engine, &obj, 0x9000, 16).expect("drain"),
            None
        );
        assert!(obj.try_wait(), "record restored for the next read");
    }

    #[test]
    fn drain_empty_returns_zero_written() {
        let mut engine = test_engine();
        let obj = watch_obj();
        let written = drain_into_guest(&mut engine, &obj, 0x9000, 64)
            .expect("drain")
            .expect("fits");
        assert_eq!(written, 0);
    }
}
