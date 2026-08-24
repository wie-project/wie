//! Plants guest stub bodies into the fake-API mapping and builds the stop-bit mask.

use anyhow::{Context, Result};

use super::classify::classify_guest_stub;
use super::config::GuestStubConfig;
use super::kind::GuestStubKind;
use crate::guest_rewire::{clear_stop_bits, plant_jmp_abs64};

/// Writes guest stubs into the fake-API mapping and builds a stop-bit mask.
///
/// Bitmap: bit=1 means "host must stop here", bit=0 means passthrough (guest stub).
///
/// Uses the pre-computed `stub_kind` cached on each entry during `make_entry`
/// — the kind variant is correct, but the embedded addresses (table_va, etc.)
/// were set to zero (CLASSIFY_ONLY).  We re-classify only the entries that
/// need real addresses: those whose kind embeds a VA (FlsGetValue, etc.).
pub(crate) fn plant_guest_stubs(
    engine: &mut dyn wie_cpu::CpuEngine,
    entries: &[crate::hooks::RuntimeFakeApiEntry],
    fake_api_base: u64,
    fake_api_size: usize,
    cfg: &GuestStubConfig,
    helper_code_base: u64,
    helper_code_size: usize,
) -> Result<Vec<u8>> {
    let mut stop_bitmap = vec![0xff_u8; fake_api_size.div_ceil(8)];

    let mut planted = 0_usize;
    let mut helper_cursor = helper_code_base;
    let helper_end = helper_code_base.saturating_add(helper_code_size as u64);
    let mut seen_kinds: std::collections::HashSet<GuestStubKind> = std::collections::HashSet::new();

    for entry in entries {
        let kind = match &entry.stub_kind {
            // Most stub kinds embed the VA in the kind itself (set in make_entry
            // with CLASSIFY_ONLY — VA is 0).  Re-classify only those that need
            // a real guest address from the config.
            Some(kind) if kind.needs_real_guest_addresses() => {
                match classify_guest_stub(&entry.library, &entry.name, cfg) {
                    Some(real_kind) => real_kind,
                    None => continue,
                }
            }
            Some(kind) => *kind,
            None => continue,
        };
        if seen_kinds.insert(kind) {
            tracing::debug!(name = %entry.name, kind = ?kind, "planted new guest stub kind");
        }
        let body = kind.encode(cfg);
        let va = entry.fake_target_va;
        if va < fake_api_base {
            continue;
        }
        let offset = usize::try_from(va - fake_api_base)
            .context("guest stub VA offset does not fit usize")?;

        if kind.needs_out_of_line_helper() {
            let body_len = body.len() as u64;
            if helper_cursor
                .checked_add(body_len)
                .is_none_or(|end| end > helper_end)
            {
                tracing::warn!(
                    name = %entry.name,
                    "guest stub helper region full; leaving host path"
                );
                continue;
            }
            engine
                .mem_write(helper_cursor, &body)
                .context("failed to write out-of-line guest stub body")?;
            if offset.checked_add(12).is_none_or(|end| end > fake_api_size) {
                continue;
            }
            plant_jmp_abs64(engine, va, helper_cursor)
                .context("failed to write guest stub entry jmp")?;
            clear_stop_bits(&mut stop_bitmap, fake_api_base, fake_api_size, va, 12);
            helper_cursor = helper_cursor.saturating_add(body_len.saturating_add(15) & !15);
        } else {
            let len = body.len();
            if offset
                .checked_add(len)
                .is_none_or(|end| end > fake_api_size)
            {
                continue;
            }
            engine
                .mem_write(va, &body)
                .context("failed to write guest API stub bytes")?;
            clear_stop_bits(&mut stop_bitmap, fake_api_base, fake_api_size, va, len);
        }
        planted = planted.saturating_add(1);
    }

    tracing::debug!(planted, "planted in-guest WinAPI stubs");
    Ok(stop_bitmap)
}
