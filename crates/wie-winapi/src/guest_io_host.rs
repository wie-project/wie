//! Host-side maintenance of the guest I/O handle table.
//!
//! Paired with the guest helpers installed by `wie-runtime::guest_io`.
//!
//! The table publishes a handle's **length and cursor** so the guest-side
//! SetFilePointer/GetFileSize acceleration can answer without a host stop. It
//! does *not* publish the file's bytes: that mirror was removed once it was
//! measured to serve nothing (see `register_open_file`).

use crate::WinApiState;
use anyhow::{Context, Result};

/// Maximum simultaneously accelerated open files (must match guest helper).
pub const GUEST_IO_MAX_SLOTS: usize = 128;

/// Bytes per slot (must match guest helper).
pub const GUEST_IO_SLOT_SIZE: usize = 40;

/// Slot flag: valid for guest fast path.
pub const GUEST_IO_FLAG_VALID: u64 = 1;

/// Publishes an open file into the guest handle table and file-data arena.
///
/// Looks up the file content inside `state` to avoid the caller having to clone
/// the (potentially large) byte Vec across a borrow boundary.
pub fn register_open_file(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    handle: u64,
) -> Result<()> {
    let lookup_start = crate::kernel32::file_stages::reg_lookup_start();
    let Some(cfg) = state.file_io.guest_io.clone() else {
        crate::kernel32::file_stages::reg_lookup_done(lookup_start);
        return Ok(());
    };

    let size = match state.file_io.open_files.get(&handle) {
        Some(f) if f.streaming => {
            // Large host streams stay on the host path.
            crate::kernel32::file_stages::reg_lookup_done(lookup_start);
            return Ok(());
        }
        Some(f) => f.bytes.len(),
        None => {
            crate::kernel32::file_stages::reg_lookup_done(lookup_start);
            return Ok(());
        }
    };

    if size == 0 {
        crate::kernel32::file_stages::reg_lookup_done(lookup_start);
        return Ok(());
    }

    let size_u64 = u64::try_from(size).context("file size does not fit u64")?;
    // No bytes are copied into the guest file arena. It used to hold a mirror of
    // the file, which only the short-read branch of the guest ReadFile helper
    // could ever serve (`cmp r8d, 64` -> `jae` host fallback for anything
    // larger), and that branch is no longer reachable: `guest_io_rewire` never
    // rewires ReadFile. Measured before removing it, straight off the profile line
    // `readfile_mirror calls=3872 mirrored=232 (6.0%)`: 3 640 of 3 872 calls
    // (94.0 %) were on handles with **no** mirror, and the 232 that did own one
    // crossed into the host anyway. Under `WIE_GUEST_IO=all` those counters came
    // back byte-identical (`calls=3872 mirrored=232 mirrored_bytes=8083968
    // host_bytes=28876800` in both arms), i.e. zero reads were served from the
    // arena even in the configuration built to use it. The copy cost 5 199.62 us
    // per call over 3 calls (first-touch page faults, not memcpy) for bytes
    // nothing read.
    crate::kernel32::file_stages::reg_lookup_done(lookup_start);

    let slot_index =
        crate::kernel32::file_stages::timed_reg_slot_scan(|| -> Result<Option<usize>> {
            for i in 0..GUEST_IO_MAX_SLOTS {
                let Some(slot_va) = slot_va_at(cfg.table_va, i) else {
                    continue;
                };
                let mut handle_bytes = [0_u8; 8];
                engine.mem_read(slot_va, &mut handle_bytes)?;
                let existing = u64::from_le_bytes(handle_bytes);
                if existing == 0 || existing == handle {
                    return Ok(Some(i));
                }
            }
            Ok(None)
        })?;
    let Some(i) = slot_index else {
        tracing::debug!(handle, "guest I/O handle table full; host path only");
        return Ok(());
    };

    crate::kernel32::file_stages::timed_reg_slot_store(|| -> Result<()> {
        let Some(slot_va) = slot_va_at(cfg.table_va, i) else {
            return Ok(());
        };
        write_u64(engine, slot_va, handle)?;
        // +8 is the data pointer the (now unreachable) short-read branch would
        // load with `mov rsi, [rbp+8]`. Written as 0 rather than left stale, so
        // a slot can never point at arena memory that holds no file any more.
        write_u64(engine, slot_va.wrapping_add(8), 0)?;
        write_u64(engine, slot_va.wrapping_add(16), size_u64)?;
        write_u64(engine, slot_va.wrapping_add(24), 0)?;
        write_u64(engine, slot_va.wrapping_add(32), GUEST_IO_FLAG_VALID)?;

        if let Some(file) = state.file_io.open_files.get_mut(&handle) {
            file.guest_slot_index = Some(u32::try_from(i).unwrap_or(u32::MAX));
        }
        Ok(())
    })
}

/// Clears a handle from the guest table.
pub fn unregister_open_file(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    handle: u64,
) -> Result<()> {
    let Some(cfg) = state.file_io.guest_io.as_ref() else {
        return Ok(());
    };
    let table_va = cfg.table_va;
    for i in 0..GUEST_IO_MAX_SLOTS {
        let Some(slot_va) = slot_va_at(table_va, i) else {
            continue;
        };
        let mut handle_bytes = [0_u8; 8];
        engine.mem_read(slot_va, &mut handle_bytes)?;
        if u64::from_le_bytes(handle_bytes) == handle {
            engine.mem_write(slot_va, &[0_u8; GUEST_IO_SLOT_SIZE])?;
            break;
        }
    }
    if let Some(file) = state.file_io.open_files.get_mut(&handle) {
        file.guest_slot_index = None;
    }
    Ok(())
}

/// Syncs the guest table's cursor/size for a handle from host state.
///
/// These two u64 writes are the whole payload: the guest-side SetFilePointer /
/// GetFileSize acceleration reads them and answers without a host stop. No file
/// bytes are copied — that mirror was removed once it was measured to serve
/// nothing (see `register_open_file`).
pub fn sync_slot_from_host(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    handle: u64,
) -> Result<()> {
    let Some(file) = state.file_io.open_files.get_mut(&handle) else {
        return Ok(());
    };
    let Some(slot_i) = file.guest_slot_index else {
        return Ok(());
    };
    let Some(cfg) = state.file_io.guest_io.as_ref() else {
        return Ok(());
    };
    let Ok(slot_usize) = usize::try_from(slot_i) else {
        return Ok(());
    };
    let Some(slot_va) = slot_va_at(cfg.table_va, slot_usize) else {
        return Ok(());
    };
    let size = u64::try_from(file.bytes.len()).unwrap_or(0);
    write_u64(engine, slot_va.wrapping_add(16), size)?;
    write_u64(engine, slot_va.wrapping_add(24), file.cursor)?;
    Ok(())
}

fn write_u64(engine: &mut dyn wie_cpu::CpuEngine, va: u64, value: u64) -> Result<()> {
    engine
        .mem_write(va, &value.to_le_bytes())
        .with_context(|| format!("failed to write guest I/O u64 at {va:#x}"))
}

fn slot_va_at(table_va: u64, index: usize) -> Option<u64> {
    let offset = index.checked_mul(GUEST_IO_SLOT_SIZE)?;
    let offset_u64 = u64::try_from(offset).ok()?;
    table_va.checked_add(offset_u64)
}

/// Pulls cursor from guest table into host `OpenGuestFile` (before host fallback I/O).
pub fn sync_host_cursor_from_guest(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    handle: u64,
) -> Result<()> {
    let lookup_start = crate::kernel32::file_stages::reg_lookup_start();
    let Some(cfg) = state.file_io.guest_io.clone() else {
        crate::kernel32::file_stages::reg_lookup_done(lookup_start);
        return Ok(());
    };
    let Some(file) = state.file_io.open_files.get_mut(&handle) else {
        return Ok(());
    };
    let Some(slot_i) = file.guest_slot_index else {
        return Ok(());
    };
    let Ok(slot_usize) = usize::try_from(slot_i) else {
        return Ok(());
    };
    let Some(slot_va) = slot_va_at(cfg.table_va, slot_usize) else {
        return Ok(());
    };
    let mut cursor_bytes = [0_u8; 8];
    engine.mem_read(slot_va.wrapping_add(24), &mut cursor_bytes)?;
    file.cursor = u64::from_le_bytes(cursor_bytes);
    Ok(())
}
