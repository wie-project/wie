//! Regression: `persist_open_file_to_host` must write a handle's buffered bytes
//! back to its host path ONLY when those bytes actually changed, never on a
//! close that has nothing to persist.
//!
//! Before the gate, every close of a bottle-backed file rewrote the WHOLE file
//! (`std::fs::write`) whether or not the guest had written anything: measured
//! at **1 499 µs per call over 6 closes (9.0 ms)** in DOOM Retro startup, all of
//! it duplicating bytes that already matched the file on disk. It is also not
//! what Windows does — closing a handle opened for reading never writes.
//!
//! The observable seam is the same one `guest_io_dirty` uses: perturb the host
//! file behind the handle's back WITHOUT setting `host_dirty` (as a read-only
//! handle would leave it) and assert the write-back is skipped. That fails
//! before the gate (the perturbed file is clobbered) and passes after.

use super::*;
use std::sync::Arc;

use crate::OpenGuestFile;
use crate::kernel32::file_io::open::persist_open_file_to_host;

/// A handle holding `bytes`, backed by `host_path`, with `host_dirty` set as
/// given.
fn handle_for(
    state: &mut WinApiState,
    host_path: &std::path::Path,
    bytes: &[u8],
    dirty: bool,
) -> u64 {
    let handle = 0x40;
    state.file_io.open_files.insert(
        handle,
        OpenGuestFile {
            handle,
            path: Arc::from(r"C:\persist.bin"),
            bytes: bytes.to_vec(),
            cursor: 0,
            host_path: Some(Arc::from(host_path)),
            streaming: false,
            guest_slot_index: None,
            host_dirty: dirty,
        },
    );
    handle
}

fn temp_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("wie-persist-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join(name)
}

/// A read-only handle must not rewrite a file that already exists, even if the
/// host copy has moved on since the open — an unchanged buffered copy is not
/// newer data.
#[test]
fn an_undirtied_handle_does_not_rewrite_the_host_file() {
    let mut state = winapi_state_default();
    let path = temp_path("readonly.bin");
    std::fs::write(&path, b"host bytes").expect("seed host file");
    let handle = handle_for(&mut state, &path, b"buffered bytes from open", false);

    persist_open_file_to_host(&mut state, handle);

    assert_eq!(
        std::fs::read(&path).expect("read back"),
        b"host bytes",
        "a read-only close rewrote the host file"
    );
    drop(std::fs::remove_file(&path));
}

/// A written handle must persist, and the flag must clear so a following close
/// does not repeat the write.
#[test]
fn a_dirty_handle_persists_once_and_clears_the_flag() {
    let mut state = winapi_state_default();
    let path = temp_path("written.bin");
    std::fs::write(&path, b"stale").expect("seed host file");
    let handle = handle_for(&mut state, &path, b"new bytes", true);

    persist_open_file_to_host(&mut state, handle);

    assert_eq!(std::fs::read(&path).expect("read back"), b"new bytes");
    let dirty = state
        .file_io
        .open_files
        .get(&handle)
        .expect("handle kept")
        .host_dirty;
    assert!(
        !dirty,
        "the flag stayed set, so the next call rewrites again"
    );
}

/// A handle that created a file the host has never seen must still produce it:
/// that is what the `exists()` half of the gate is for.
#[test]
fn a_created_file_is_written_even_when_not_dirty() {
    let mut state = winapi_state_default();
    let path = temp_path("created.bin");
    drop(std::fs::remove_file(&path));
    let handle = handle_for(&mut state, &path, b"", false);

    persist_open_file_to_host(&mut state, handle);

    assert!(path.exists(), "creation was skipped by the dirty gate");
    assert_eq!(std::fs::read(&path).expect("read back"), b"");
    drop(std::fs::remove_file(&path));
}
