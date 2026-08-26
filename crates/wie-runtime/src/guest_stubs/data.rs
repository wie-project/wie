//! Guest data-page builders: metrics/colors tables, cwd blob, clock table.

use anyhow::{Context, Result};

use super::config::{
    CLOCK_TABLE_SIZE, COLOR_COUNT, CWD_BLOB_SIZE, CWD_MAX_CHARS, METRICS_COUNT, OFFSET_COLORS,
};

/// Builds metrics/colors tables matching host `fake_system_metric` / `GetSysColor`.
///
/// Takes the session's [`wie_winapi::DisplayMetrics`] so the in-guest
/// GetSystemMetrics accelerator reports the same dimensions as the host-side
/// handler (the table previously hardcoded 1024×768 and drifted).
pub(crate) fn build_stub_data_page(display: wie_winapi::DisplayMetrics) -> Vec<u8> {
    let mut page = vec![0_u8; 0x500 + CWD_BLOB_SIZE];
    // Metrics
    for i in 0..METRICS_COUNT {
        let v =
            u32::try_from(fake_system_metric(u64::try_from(i).unwrap_or(0), &display)).unwrap_or(0);
        let off = i * 4;
        page[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    // Colors at OFFSET_COLORS
    let color_base = OFFSET_COLORS as usize;
    for i in 0..COLOR_COUNT {
        let v = fake_sys_color(i as u64) as u32;
        let off = color_base + i * 4;
        page[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    // cwd blob starts empty (char_count = 0); session fills after process identity.
    page
}

/// Publish UTF-16 current directory into the guest cwd blob (Microsoft path string).
pub(crate) fn publish_cwd_wide(
    engine: &mut dyn wie_cpu::CpuEngine,
    cwd_blob_va: u64,
    directory: &str,
) -> Result<()> {
    let mut units: Vec<u16> = directory.encode_utf16().collect();
    if units.len() > CWD_MAX_CHARS {
        units.truncate(CWD_MAX_CHARS);
    }
    let char_count = u32::try_from(units.len()).context("cwd char_count overflow")?;
    units.push(0); // NUL
    let mut blob = vec![0_u8; CWD_BLOB_SIZE];
    blob[0..4].copy_from_slice(&char_count.to_le_bytes());
    for (i, u) in units.iter().enumerate() {
        let off = 4 + i * 2;
        if off + 2 <= blob.len() {
            blob[off..off + 2].copy_from_slice(&u.to_le_bytes());
        }
    }
    engine
        .mem_write(cwd_blob_va, &blob)
        .context("failed to publish guest cwd blob")?;
    Ok(())
}

/// Publish the current clock snapshot into the host-written guest clock table.
///
/// Slot layout must match the classify offsets above and
/// `wie_winapi::kernel32::clock::clock_table_values`. The host refreshes this
/// table on every API stop (and once at session init) so the in-guest clock
/// stubs observe advancing values with no host stop.
pub(crate) fn refresh_clock_table(
    engine: &mut dyn wie_cpu::CpuEngine,
    clock_table_va: u64,
) -> Result<()> {
    let mut table = [0_u8; CLOCK_TABLE_SIZE];
    let values = wie_winapi::kernel32::clock::clock_table_values();
    for (slot, value) in values.iter().enumerate() {
        let off = slot.saturating_mul(8);
        if let Some(dst) = table.get_mut(off..off.saturating_add(8)) {
            dst.copy_from_slice(&value.to_le_bytes());
        }
    }
    engine
        .mem_write(clock_table_va, &table)
        .context("failed to refresh guest clock table")
}

/// Must match `wie_winapi::user32::fake_system_metric`.
fn fake_system_metric(metric_index: u64, display: &wie_winapi::DisplayMetrics) -> u64 {
    match metric_index {
        // SM_CXSCREEN / SM_CXFULLSCREEN
        0 | 16 => display.width_metric(),
        // SM_CYSCREEN
        1 => display.height_metric(),
        2 | 3 => 17,
        4 => 23,
        5 | 6 | 19 | 80 => 1,
        7 | 8 | 32 | 33 | 36 | 37 => 4,
        11..=14 => 32,
        15 => 20,
        // SM_CYFULLSCREEN — no emulated taskbar.
        17 => display.height_metric(),
        28 | 34 => 112,
        29 | 35 => 27,
        30 | 31 => 18,
        38 | 39 => 75,
        _ => 0,
    }
}

/// Must match `wie_winapi::user32::handle_get_sys_color` COLORREF values.
fn fake_sys_color(color_index: u64) -> u64 {
    match color_index {
        1 | 6 | 7..=9 | 18 => 0x0000_0000,
        2 | 13 => 0x00d7_7830,
        3 => 0x00bf_bfbf,
        5 | 14 => 0x00ff_ffff,
        10 | 11 => 0x00b4_b4b4,
        12 => 0x00ab_abab,
        16 => 0x00a0_a0a0,
        17 => 0x006d_6d6d,
        0 => 0x00c8_c8c8,
        _ => 0x00f0_f0f0,
    }
}
