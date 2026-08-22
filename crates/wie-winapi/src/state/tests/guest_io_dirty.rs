//! Regression: `guest_io_host::sync_slot_from_host` must re-mirror a buffered
//! file's bytes into the guest I/O arena ONLY when those bytes actually changed
//! (a write), never on a pure read.
//!
//! Before the fix every host-side read re-copied the ENTIRE buffered file into
//! the arena and that guest write tripped the JIT code-write cache flush —
//! O(filesize) host copies + a full `clear_compiled()` per read on large files.
//! The `guest_dirty` flag drives the gating, set on the buffered write paths and
//! cleared once the mirror is refreshed.
//!
//! The stale-arena seam is deliberate: a pure read leaves the host bytes
//! unchanged, so an identical re-mirror is invisible to the arena contents. To
//! observe whether the mirror is gated, we perturb the host bytes WITHOUT
//! setting `guest_dirty` (as happens on a read) and assert the arena is left
//! untouched. This fails before the fix (arena followed the perturbed bytes)
//! and passes after (the mirror is skipped while not dirty).

use super::*;
use std::sync::Arc;

use crate::guest_io_host;
use crate::{GuestIoRuntimeConfig, OpenGuestFile};
use wie_cpu::RwxPerms;

// Guest I/O table + mirrored file arena (matches `register_open_file`).
const TABLE_BASE: u64 = 0x2000_0000;
const TABLE_SIZE: usize = 128 * 40;
const ARENA_BASE: u64 = 0x2000_2000;
const ARENA_SIZE: usize = 0x100_000;
/// Page-aligned span covering the table and the mirrored file arena.
const GUEST_IO_MAP_SIZE: usize = 0x102_000;

/// Insert a small buffered (`!streaming`) file with no host backing and register
/// it into the guest I/O table/arena, mirroring `bytes`.
fn register_buffered_file(
    engine: &mut IcedCpu,
    state: &mut WinApiState,
    handle: u64,
    bytes: Vec<u8>,
) -> anyhow::Result<u64> {
    state.file_io.open_files.insert(
        handle,
        OpenGuestFile {
            handle,
            path: Arc::from(r"C:\test.bin"),
            bytes,
            cursor: 0,
            host_path: None,
            streaming: false,
            guest_data_va: None,
            guest_slot_index: None,
            guest_dirty: false,
        },
    );
    guest_io_host::register_open_file(engine, state, handle)?;
    let file = state
        .file_io
        .open_files
        .get(&handle)
        .ok_or_else(|| anyhow::anyhow!("file missing after register"))?;
    file.guest_data_va
        .ok_or_else(|| anyhow::anyhow!("buffered file must be registered"))
}

fn read_arena(engine: &mut IcedCpu, data_va: u64, len: usize) -> anyhow::Result<Vec<u8>> {
    let mut buf = vec![0_u8; len];
    engine.mem_read(data_va, &mut buf)?;
    Ok(buf)
}

#[test]
fn sync_slot_from_host_re_mirrors_only_when_dirty() -> anyhow::Result<()> {
    let mut engine = test_engine();
    engine.mem_map(TABLE_BASE, GUEST_IO_MAP_SIZE, RwxPerms::ALL)?;
    engine.mem_write(TABLE_BASE, &vec![0_u8; TABLE_SIZE])?;

    let mut state = winapi_state_default();
    state.file_io.guest_io = Some(GuestIoRuntimeConfig {
        table_va: TABLE_BASE,
        file_data_base: ARENA_BASE,
        file_data_size: ARENA_SIZE,
    });
    state.file_io.guest_file_data_next = ARENA_BASE;

    let handle = 7;
    let initial: Vec<u8> = (0..64).map(|i| i as u8).collect();
    let data_va = register_buffered_file(&mut engine, &mut state, handle, initial.clone())?;

    // Arena is mirrored at open.
    assert_eq!(
        read_arena(&mut engine, data_va, initial.len())?,
        initial,
        "arena mirrored on register"
    );

    // Dirty write → sync re-mirrors the changed bytes and clears the flag.
    let updated: Vec<u8> = (0..64).map(|i| (i as u8).wrapping_add(0x10)).collect();
    {
        let file = state.file_io.open_files.get_mut(&handle).unwrap();
        file.bytes = updated.clone();
        file.guest_dirty = true;
    }
    guest_io_host::sync_slot_from_host(&mut engine, &mut state, handle)?;
    assert_eq!(
        read_arena(&mut engine, data_va, updated.len())?,
        updated,
        "dirty write re-mirrors changed bytes"
    );
    assert!(
        !state.file_io.open_files.get(&handle).unwrap().guest_dirty,
        "dirty flag cleared after mirror"
    );

    // Pure read (bytes unchanged by guest I/O): perturb the host bytes WITHOUT
    // setting dirty. The mirror must NOT follow, else it was doing an O(filesize)
    // copy per read. This is the assertion that fails before the fix.
    let perturbed: Vec<u8> = (0..64).map(|i| (i as u8).wrapping_add(0x40)).collect();
    {
        let file = state.file_io.open_files.get_mut(&handle).unwrap();
        file.guest_dirty = false;
        file.bytes = perturbed.clone();
    }
    guest_io_host::sync_slot_from_host(&mut engine, &mut state, handle)?;
    assert_eq!(
        read_arena(&mut engine, data_va, updated.len())?,
        updated,
        "non-dirty sync must leave the arena untouched (no per-read full copy)"
    );

    Ok(())
}

#[test]
fn buffered_write_updates_the_guest_arena() -> anyhow::Result<()> {
    let mut engine = test_engine();
    engine.mem_map(TABLE_BASE, GUEST_IO_MAP_SIZE, RwxPerms::ALL)?;
    engine.mem_write(TABLE_BASE, &vec![0_u8; TABLE_SIZE])?;

    let mut state = winapi_state_default();
    state.file_io.guest_io = Some(GuestIoRuntimeConfig {
        table_va: TABLE_BASE,
        file_data_base: ARENA_BASE,
        file_data_size: ARENA_SIZE,
    });
    state.file_io.guest_file_data_next = ARENA_BASE;

    let handle = 9;
    let initial: Vec<u8> = (0..64).map(|i| i as u8).collect();
    let data_va = register_buffered_file(&mut engine, &mut state, handle, initial.clone())?;

    // A buffered WriteFile must mark the file dirty and push the new bytes into
    // the guest arena before any guest fast-path read.
    const DATA: &[u8] = b"HELLO";
    engine.mem_write(0x4000, DATA)?;
    write_regs(
        &mut engine,
        handle,
        0x4000,
        DATA.len() as u64,
        0x6000,
        STACK_TOP,
    );
    let wrote = kernel32::handle_write_file(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("WriteFile");
    assert_eq!(wrote.return_value, 1, "buffered WriteFile succeeds");

    let file = state.file_io.open_files.get(&handle).unwrap();
    assert!(!file.guest_dirty, "write mirror flushed dirty flag");

    // Host bytes reflect the write at the (starting) cursor position...
    assert_eq!(
        file.bytes.get(..DATA.len()).unwrap(),
        DATA,
        "host buffered bytes updated"
    );
    // ...and the arena mirrors the same bytes back, so a guest read sees them.
    let arena = read_arena(&mut engine, data_va, initial.len())?;
    assert_eq!(
        arena.get(..DATA.len()).unwrap(),
        DATA,
        "guest arena updated after buffered write"
    );

    Ok(())
}
