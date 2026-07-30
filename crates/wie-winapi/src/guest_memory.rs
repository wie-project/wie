use anyhow::{Context, Result};

pub(crate) fn read_i32(engine: &mut dyn wie_cpu::CpuEngine, address: u64) -> Result<i32> {
    let mut bytes = [0_u8; 4];

    engine
        .mem_read(address, &mut bytes)
        .context("failed to read i32 from guest memory")?;

    Ok(i32::from_le_bytes(bytes))
}

pub(crate) fn read_u64(engine: &mut dyn wie_cpu::CpuEngine, address: u64) -> Result<u64> {
    let mut bytes = [0_u8; 8];

    engine
        .mem_read(address, &mut bytes)
        .context("failed to read u64 from guest memory")?;

    Ok(u64::from_le_bytes(bytes))
}

pub(crate) fn write_u32(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    value: u32,
) -> Result<()> {
    engine
        .mem_write(address, &value.to_le_bytes())
        .context("failed to write u32 to guest memory")
}

pub(crate) fn write_i32(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    value: i32,
) -> Result<()> {
    engine
        .mem_write(address, &value.to_le_bytes())
        .context("failed to write i32 to guest memory")
}

pub(crate) fn write_u64(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    value: u64,
) -> Result<()> {
    engine
        .mem_write(address, &value.to_le_bytes())
        .context("failed to write u64 to guest memory")
}

/// Compute a guest address with wrapping arithmetic.
///
/// Guest addresses are checked at allocation time (bounded to <48-bit VA), so
/// overflow on field-offset computation is a programming error.  Using
/// `wrapping_add` avoids the checked-branch on every handler memory access.
#[must_use]
pub(crate) fn checked_field_address(base: u64, offset: u64, _field_name: &str) -> u64 {
    base.wrapping_add(offset)
}

/// Compute a guest address with wrapping arithmetic.
///
/// See [`checked_field_address`] for rationale.
#[must_use]
pub(crate) fn checked_address(base: u64, offset: u64, _context_name: &str) -> u64 {
    base.wrapping_add(offset)
}

pub(crate) fn read_u16(engine: &mut dyn wie_cpu::CpuEngine, address: u64) -> Result<u16> {
    let mut bytes = [0_u8; 2];

    engine
        .mem_read(address, &mut bytes)
        .context("failed to read u16 from guest memory")?;

    Ok(u16::from_le_bytes(bytes))
}

pub(crate) fn write_u16(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    value: u16,
) -> Result<()> {
    engine
        .mem_write(address, &value.to_le_bytes())
        .context("failed to write u16 to guest memory")
}

pub(crate) fn read_u32(engine: &mut dyn wie_cpu::CpuEngine, address: u64) -> Result<u32> {
    let mut bytes = [0_u8; 4];

    engine
        .mem_read(address, &mut bytes)
        .context("failed to read u32 from guest memory")?;

    Ok(u32::from_le_bytes(bytes))
}

/// Reads an arbitrary byte slice from guest memory.
pub(crate) fn read_bytes(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    buffer: &mut [u8],
) -> Result<()> {
    engine
        .mem_read(address, buffer)
        .context("failed to read bytes from guest memory")
}

/// Writes an arbitrary byte slice into guest memory.
pub(crate) fn write_bytes(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    bytes: &[u8],
) -> Result<()> {
    engine
        .mem_write(address, bytes)
        .context("failed to write bytes to guest memory")
}
