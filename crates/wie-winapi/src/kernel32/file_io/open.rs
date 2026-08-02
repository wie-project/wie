use super::{
    CREATE_ALWAYS, CREATE_NEW, Context, ERROR_FILE_EXISTS, ERROR_FILE_NOT_FOUND,
    ERROR_INVALID_PARAMETER, ERROR_PATH_NOT_FOUND, FILE_ATTRIBUTE_DIRECTORY,
    INVALID_FILE_ATTRIBUTES, OPEN_ALWAYS, OPEN_EXISTING, OpenGuestFile, Path, Result,
    TRUNCATE_EXISTING, WinApiState, file_attributes_for_path, is_main_module_path,
    resolve_full_windows_path,
};

pub(crate) fn guest_basename(path: &str) -> &str {
    crate::vfs::guest_basename(path)
}
pub(crate) fn paths_match_guest(requested: &str, candidate: &str) -> bool {
    crate::vfs::paths_equal_ci(requested, candidate)
}
pub(crate) fn find_open_file(state: &WinApiState, handle: u64) -> Option<&OpenGuestFile> {
    state.file_io.open_files.get(&handle)
}
pub(crate) fn find_open_file_mut(
    state: &mut WinApiState,
    handle: u64,
) -> Option<&mut OpenGuestFile> {
    state.file_io.open_files.get_mut(&handle)
}
pub(crate) fn is_open_file_handle(state: &WinApiState, handle: u64) -> bool {
    state.file_io.open_files.contains_key(&handle)
}
pub fn open_guest_path(state: &mut WinApiState, guest_path: &str) -> Result<u64> {
    match open_or_create_guest_path(state, guest_path, 0, OPEN_EXISTING) {
        Ok(
            OpenFileOutcome::Handle(handle)
            | OpenFileOutcome::HandleExists(handle)
            | OpenFileOutcome::HandleCreated(handle),
        ) => Ok(handle),
        Err(code) => anyhow::bail!("open_guest_path failed: win32 error {code}"),
    }
}
pub(crate) fn open_or_create_guest_path(
    state: &mut WinApiState,
    guest_path: &str,
    _desired_access: u64,
    creation_disposition: u64,
) -> std::result::Result<OpenFileOutcome, u32> {
    if guest_path.is_empty() {
        return Err(ERROR_PATH_NOT_FOUND);
    }

    // Keep volumes.bottle_root in sync with legacy field.
    if state.file_io.volumes.bottle_root != state.file_io.bottle_root {
        state.file_io.volumes.bottle_root = state.file_io.bottle_root.clone();
    }

    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full_path = resolve_full_windows_path(&cwd, guest_path);

    let bottle_host =
        crate::vfs::guest_path_to_host(&state.file_io.volumes, &full_path).map(|m| m.host);
    let existed = guest_path_exists(state, &full_path);

    match creation_disposition {
        CREATE_NEW => {
            if existed {
                return Err(ERROR_FILE_EXISTS);
            }
            let handle = create_new_guest_file(state, &full_path, bottle_host.as_ref())?;
            Ok(OpenFileOutcome::HandleCreated(handle))
        }
        CREATE_ALWAYS => {
            let handle = if existed {
                open_existing_guest_file(state, &full_path, bottle_host.as_ref(), true)?
            } else {
                create_new_guest_file(state, &full_path, bottle_host.as_ref())?
            };
            Ok(if existed {
                OpenFileOutcome::HandleExists(handle)
            } else {
                OpenFileOutcome::HandleCreated(handle)
            })
        }
        OPEN_EXISTING => {
            if !existed {
                return Err(ERROR_FILE_NOT_FOUND);
            }
            let handle = open_existing_guest_file(state, &full_path, bottle_host.as_ref(), false)?;
            Ok(OpenFileOutcome::Handle(handle))
        }
        OPEN_ALWAYS => {
            if existed {
                let handle =
                    open_existing_guest_file(state, &full_path, bottle_host.as_ref(), false)?;
                Ok(OpenFileOutcome::HandleExists(handle))
            } else {
                let handle = create_new_guest_file(state, &full_path, bottle_host.as_ref())?;
                Ok(OpenFileOutcome::HandleCreated(handle))
            }
        }
        TRUNCATE_EXISTING => {
            if !existed {
                return Err(ERROR_FILE_NOT_FOUND);
            }
            let handle = open_existing_guest_file(state, &full_path, bottle_host.as_ref(), true)?;
            Ok(OpenFileOutcome::Handle(handle))
        }
        _ => {
            // Unknown disposition — fail closed.
            Err(ERROR_INVALID_PARAMETER)
        }
    }
}
pub(crate) fn open_existing_guest_file(
    state: &mut WinApiState,
    guest_path: &str,
    bottle_host: Option<&std::path::PathBuf>,
    truncate: bool,
) -> std::result::Result<u64, u32> {
    let host_path = bottle_host.cloned().or_else(|| {
        state
            .file_io
            .host_file_mounts
            .iter()
            .find(|m| paths_match_guest(guest_path, &m.guest_path))
            .map(|m| m.host_path.clone())
    });

    // Large host files: stream without loading into RAM.
    if let Some(ref host) = host_path
        && !is_main_module_path(state, guest_path)
        && let Ok(meta) = std::fs::metadata(host)
        && meta.is_file()
        && meta.len() > crate::vfs::BUFFER_SIZE_THRESHOLD
    {
        if truncate {
            drop(crate::vfs::host_set_len(host, 0));
        }
        return allocate_open_file_ex(state, guest_path, Vec::new(), Some(host.clone()), true)
            .map_err(|_| ERROR_FILE_NOT_FOUND);
    }

    let mut bytes =
        resolve_guest_file_bytes(state, guest_path).map_err(|_| ERROR_FILE_NOT_FOUND)?;
    if truncate {
        bytes.clear();
    }
    allocate_open_file(state, guest_path, bytes, host_path).map_err(|_| ERROR_FILE_NOT_FOUND)
}
pub(crate) fn create_new_guest_file(
    state: &mut WinApiState,
    guest_path: &str,
    bottle_host: Option<&std::path::PathBuf>,
) -> std::result::Result<u64, u32> {
    if let Some(host) = bottle_host {
        if let Some(parent) = host.parent() {
            std::fs::create_dir_all(parent).map_err(|_| ERROR_PATH_NOT_FOUND)?;
        }
        std::fs::write(host, []).map_err(|_| ERROR_PATH_NOT_FOUND)?;
        // Host-backed creates always stream: a new archive starts at size 0, so the
        // size-threshold check would otherwise keep the entire growing file in RAM
        // (7za compression of hundreds of MiB was a classic progressive host-RSS leak).
        return allocate_open_file_ex(state, guest_path, Vec::new(), Some(host.clone()), true)
            .map_err(|_| ERROR_PATH_NOT_FOUND);
    }

    // No bottle: keep an in-memory virtual file (session-only).
    ensure_virtual_file(state, guest_path);
    allocate_open_file(state, guest_path, Vec::new(), None).map_err(|_| ERROR_PATH_NOT_FOUND)
}
pub(crate) fn ensure_virtual_file(state: &mut WinApiState, guest_path: &str) {
    if state
        .file_io
        .virtual_files
        .iter()
        .any(|entry| paths_match_guest(guest_path, &entry.guest_path))
    {
        return;
    }

    state.file_io.virtual_files.push(crate::VirtualGuestFile {
        guest_path: guest_path.to_owned(),
        bytes: Vec::new(),
    });
}
pub(crate) fn resolve_guest_file_bytes(state: &WinApiState, guest_path: &str) -> Result<Vec<u8>> {
    if is_main_module_path(state, guest_path) {
        return Ok(state.file_io.executable_file_bytes.clone());
    }

    if let Some(mount) = state
        .file_io
        .host_file_mounts
        .iter()
        .find(|mount| paths_match_guest(guest_path, &mount.guest_path))
    {
        return std::fs::read(&mount.host_path).with_context(|| {
            format!(
                "failed to read mounted host file {} for guest path {guest_path}",
                mount.host_path.display()
            )
        });
    }

    if let Some(map) = crate::vfs::guest_path_to_host(&state.file_io.volumes, guest_path)
        && map.host.is_file()
    {
        return std::fs::read(&map.host).with_context(|| {
            format!(
                "failed to read volume file {} for guest path {guest_path}",
                map.host.display()
            )
        });
    }

    if let Some(virtual_file) = state
        .file_io
        .virtual_files
        .iter()
        .find(|entry| paths_match_guest(guest_path, &entry.guest_path))
    {
        return Ok(virtual_file.bytes.clone());
    }

    // Allow opening by host absolute path when the guest happens to pass it
    // (useful for ad-hoc testing).
    let as_path = Path::new(guest_path);
    if as_path.is_absolute() && as_path.is_file() {
        return std::fs::read(as_path)
            .with_context(|| format!("failed to read host path {guest_path}"));
    }

    anyhow::bail!("guest file not found: {guest_path}")
}
pub(crate) fn read_create_file_stack_u32(
    engine: &mut dyn wie_cpu::CpuEngine,
    offset: u64,
) -> Result<u32> {
    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for CreateFile stack arg")?;

    let address = rsp
        .checked_add(offset)
        .context("CreateFile stack arg address overflow")?;

    let mut bytes = [0_u8; 4];
    engine
        .mem_read(address, &mut bytes)
        .context("failed to read CreateFile stack arg")?;

    Ok(u32::from_le_bytes(bytes))
}
pub(crate) fn allocate_open_file(
    state: &mut WinApiState,
    path: &str,
    bytes: Vec<u8>,
    host_path: Option<std::path::PathBuf>,
) -> Result<u64> {
    allocate_open_file_ex(state, path, bytes, host_path, false)
}
pub(crate) fn allocate_open_file_ex(
    state: &mut WinApiState,
    path: &str,
    bytes: Vec<u8>,
    host_path: Option<std::path::PathBuf>,
    force_stream: bool,
) -> Result<u64> {
    let handle = state.file_io.next_file_handle.as_u64();

    state.file_io.next_file_handle = crate::FileHandle::from(
        state
            .file_io
            .next_file_handle
            .as_u64()
            .checked_add(1)
            .context("guest file handle allocator overflow")?,
    );

    let size_u64 = if force_stream {
        host_path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map_or(0, |m| m.len())
    } else {
        u64::try_from(bytes.len()).unwrap_or(0)
    };
    // Stream large host-backed files; keep small files fully buffered for guest I/O accel.
    let streaming = force_stream
        || (host_path.is_some()
            && size_u64 > crate::vfs::BUFFER_SIZE_THRESHOLD
            && !is_main_module_path(state, path));
    let bytes = if streaming { Vec::new() } else { bytes };

    state.file_io.open_files.insert(
        handle,
        OpenGuestFile {
            handle,
            path: std::sync::Arc::from(path),
            bytes,
            cursor: 0,
            host_path: host_path.map(|p| std::sync::Arc::from(p.as_path())),
            streaming,
            guest_data_va: None,
            guest_slot_index: None,
        },
    );

    // Keep legacy single-handle fields in sync when opening the main executable.
    if is_main_module_path(state, path) {
        state.file_io.executable_file_cursor = 0;
    }

    Ok(handle)
}
pub(crate) fn persist_open_file_to_host(state: &WinApiState, handle: u64) {
    let Some(open_file) = find_open_file(state, handle) else {
        return;
    };
    if open_file.streaming {
        return;
    }
    let Some(host_path) = open_file.host_path.as_ref() else {
        return;
    };
    if let Some(parent) = host_path.parent() {
        drop(std::fs::create_dir_all(parent));
    }
    if let Err(error) = std::fs::write(host_path, &open_file.bytes) {
        tracing::debug!(
            handle,
            path = %host_path.display(),
            error = %error,
            "failed to persist open file to host"
        );
    }
}
pub(crate) fn maybe_promote_open_file_to_streaming(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    handle: u64,
) {
    let host_path = {
        let Some(open_file) = find_open_file(state, handle) else {
            return;
        };
        if open_file.streaming {
            return;
        }
        let size_u64 = u64::try_from(open_file.bytes.len()).unwrap_or(0);
        if size_u64 <= crate::vfs::BUFFER_SIZE_THRESHOLD {
            return;
        }
        let Some(host) = open_file.host_path.as_ref() else {
            return;
        };
        host.clone()
    };
    if let Some(parent) = host_path.parent() {
        drop(std::fs::create_dir_all(parent));
    }
    {
        let Some(open_file) = find_open_file(state, handle) else {
            return;
        };
        if let Err(error) = std::fs::write(&host_path, &open_file.bytes) {
            tracing::debug!(
                handle,
                path = %host_path.display(),
                error = %error,
                "failed to spill buffered file to host for streaming promote"
            );
            return;
        }
    }
    // Drop any guest I/O mirror (streaming stays on host path only).
    // Best-effort teardown: failure to sync is not fatal.
    let _ = crate::guest_io_host::unregister_open_file(engine, state, handle).ok();
    if let Some(open_file) = find_open_file_mut(state, handle) {
        open_file.bytes.clear();
        open_file.bytes.shrink_to_fit();
        open_file.streaming = true;
    }
}
pub fn mount_host_file(
    state: &mut WinApiState,
    guest_path: &str,
    host_path: impl AsRef<Path>,
) -> Result<()> {
    let host_path = host_path.as_ref();

    if !host_path.is_file() {
        anyhow::bail!(
            "host file does not exist or is not a regular file: {}",
            host_path.display()
        );
    }

    // Replace existing mount for the same guest path.
    state
        .file_io
        .host_file_mounts
        .retain(|mount| !paths_match_guest(guest_path, &mount.guest_path));

    state.file_io.host_file_mounts.push(crate::HostFileMount {
        guest_path: guest_path.to_owned(),
        host_path: host_path.to_path_buf(),
    });

    Ok(())
}
pub(crate) fn guest_path_exists(state: &WinApiState, path: &str) -> bool {
    if is_main_module_path(state, path) {
        return true;
    }
    if state
        .file_io
        .host_file_mounts
        .iter()
        .any(|mount| paths_match_guest(path, &mount.guest_path))
    {
        return true;
    }
    if state
        .file_io
        .virtual_files
        .iter()
        .any(|entry| paths_match_guest(path, &entry.guest_path))
    {
        return true;
    }
    if let Some(map) = crate::vfs::guest_path_to_host(&state.file_io.volumes, path) {
        return map.host.is_file();
    }
    false
}
pub(crate) fn guest_dir_exists(state: &WinApiState, path: &str) -> bool {
    let attrs = file_attributes_for_path(state, path);
    attrs != INVALID_FILE_ATTRIBUTES && (attrs & FILE_ATTRIBUTE_DIRECTORY) != 0
}
/// Result of a successful `CreateFile` open (handle + existence semantics for last-error).
pub(crate) enum OpenFileOutcome {
    /// Opened or created; last-error should be 0.
    Handle(u64),
    /// Opened a file that already existed (OPEN_ALWAYS / CREATE_ALWAYS overwrite).
    HandleExists(u64),
    /// Created a new file (OPEN_ALWAYS / CREATE_NEW / CREATE_ALWAYS on new path).
    HandleCreated(u64),
}
/// Copies the open handle's buffer into `virtual_files` for the same path.
///
/// **Host-backed / streaming files are never mirrored.** Bottle volume paths
/// (WIE_ROOT / drive-D) used to land here on every CloseHandle because they are
/// not `host_file_mounts` entries — so opening+closing every source file during
/// a 7za scan permanently retained full contents in `virtual_files` (session-long
/// RAM growth proportional to scanned data).
pub(crate) fn sync_open_bytes_to_virtual(state: &mut WinApiState, path: &str, handle: u64) {
    let Some(open_file) = find_open_file(state, handle) else {
        return;
    };
    // Host path or streaming ⇒ content lives on disk; never retain a second copy.
    if open_file.host_path.is_some() || open_file.streaming {
        return;
    }
    if is_main_module_path(state, path) {
        return;
    }
    // Volume-mapped paths without an open host_path still must not accumulate.
    if crate::vfs::guest_path_to_host(&state.file_io.volumes, path).is_some() {
        return;
    }
    if state
        .file_io
        .host_file_mounts
        .iter()
        .any(|mount| paths_match_guest(path, &mount.guest_path))
    {
        return;
    }

    let bytes = open_file.bytes.clone();

    if let Some(virtual_file) = state
        .file_io
        .virtual_files
        .iter_mut()
        .find(|entry| paths_match_guest(path, &entry.guest_path))
    {
        virtual_file.bytes = bytes;
        return;
    }

    // Pure in-session virtual files only (no bottle/mount/volume backing).
    state.file_io.virtual_files.push(crate::VirtualGuestFile {
        guest_path: path.to_owned(),
        bytes,
    });
}
