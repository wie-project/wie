use anyhow::{Context, Result};
use wie_cpu::CpuEngine;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// Read a little-endian integer (`u8`, `u16`, `u32`, `u64`, or `i32`) from
/// guest memory.
///
/// The width is fixed by the return type (`size_of::<T>()`), so the former
/// five per-width readers collapse into this one generic. The error context
/// names the concrete type, keeping the per-width "failed to read u32 from
/// guest memory" messages.
pub(crate) fn read_int<T>(engine: &mut dyn CpuEngine, address: u64) -> Result<T>
where
    T: FromBytes,
{
    // One fixed staging buffer covers every integer width this helper is
    // instantiated with; the active range is sized at runtime, which
    // sidesteps the const-generic array-length restriction on stable.
    let mut bytes = [0_u8; 16];
    let len = std::mem::size_of::<T>();
    let buf = bytes
        .get_mut(..len)
        .ok_or_else(|| anyhow::anyhow!("read_int: width exceeds staging buffer"))?;
    engine.mem_read(address, buf).with_context(|| {
        format!(
            "failed to read {} from guest memory",
            std::any::type_name::<T>()
        )
    })?;
    // `read_from_bytes` is alignment-agnostic and can only fail on a length
    // mismatch, which the `size_of::<T>()` buffer makes impossible.
    T::read_from_bytes(buf).map_err(|_| anyhow::anyhow!("read_int: internal size mismatch"))
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

/// Compute a guest VA with wrapping arithmetic.
///
/// Guest VAs are checked at allocation time (bounded to <48-bit VA), so
/// overflow on field-offset computation is a programming error.  Using
/// `wrapping_add` avoids the checked-branch on every handler memory access.
///
/// The `description` argument is unused by the computation; each call site
/// passes the field or argument the resulting address refers to, which
/// documents the Win64 ABI slot being accessed.
#[must_use]
pub(crate) fn checked_address(base: u64, offset: u64, _description: &str) -> u64 {
    base.wrapping_add(offset)
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

/// Host staging buffer for typed writes when guest memory cannot be borrowed
/// in place (a struct straddling an arena boundary, or an odd guest address).
///
/// `repr(align(16))` covers every Win64 struct alignment (8 for scalar
/// structs, 16 for `__m128`-containing types), so `mut_from_bytes` never
/// reports `AlignmentMismatch` on the fallback path.
#[repr(align(16))]
struct AlignedBuf<T> {
    value: T,
}

impl<T: FromBytes + IntoBytes + Immutable> AlignedBuf<T> {
    /// All-zero buffer (`FromBytes` guarantees the zero bit pattern is valid).
    fn zeroed() -> Self {
        Self {
            value: T::new_zeroed(),
        }
    }

    fn as_bytes(&self) -> &[u8] {
        self.value.as_bytes()
    }

    fn as_mut_bytes(&mut self) -> &mut [u8] {
        self.value.as_mut_bytes()
    }
}

/// Run `f` against a typed read view of a guest struct.
///
/// Fast path: the struct is borrowed in place via [`CpuEngine::host_slice`]
/// (one shared-lock acquisition). Fallback: the struct straddles an arena
/// boundary or sits at a misaligned address — it is copied into a host buffer
/// first. Both paths hand `f` a `&T`; the fallback is invisible to the caller,
/// and Windows tolerates unaligned struct reads, so no guest program can
/// observe the difference.
///
/// # Borrow rule
/// `f` cannot touch the engine: while the view is alive the engine is
/// mutably borrowed by this helper. Do all engine I/O before or after the
/// closure (read-all → compute → write-all → return).
pub(crate) fn with_typed_read<T, F, R>(engine: &mut dyn CpuEngine, address: u64, f: F) -> Result<R>
where
    T: KnownLayout + Immutable + FromBytes,
    F: FnOnce(&T) -> Result<R>,
{
    let len = std::mem::size_of::<T>();
    if let Some(bytes) = engine.host_slice(address, len)
        && let Ok(view) = T::ref_from_bytes(bytes)
    {
        return f(view);
    }
    // Fallback: cross-arena span or alignment mismatch. `read_from_bytes` is
    // alignment-agnostic, so the staging copy preserves guest bytes exactly.
    let mut bytes = vec![0_u8; len];
    engine
        .mem_read(address, &mut bytes)
        .context("failed to stage-read typed struct from guest memory")?;
    // Map the cast error to an owned message: `SizeError` borrows the buffer,
    // which cannot outlive this function.
    let value = T::read_from_bytes(&bytes)
        .map_err(|_| anyhow::anyhow!("staged typed read produced an invalid bit pattern"))?;
    f(&value)
}

/// Run `f` against a typed mutable view of a guest struct, then commit.
///
/// Fast path: the struct is borrowed in place via [`CpuEngine::host_slice_mut`]
/// (one shared-lock acquisition), and `f` writes land directly in guest
/// memory. Fallback (cross-arena span or odd guest address): the struct is
/// staged into an aligned host copy, `f` edits the copy, and the whole struct
/// is written back with `mem_write`. The fallback also covers executable
/// spans, which `host_slice_mut` denies by design (SMC) — `mem_write` runs the
/// code-invalidation path there.
///
/// Zero-fill semantics: the view starts fully zeroed, so padding and any field
/// `f` leaves unset read as zero (GetStartupInfo behavior — real Windows
/// zeroes these structs before filling them).
///
/// # Borrow rule
/// `f` cannot touch the engine: while the view is alive the engine is
/// mutably borrowed by this helper. Do all engine I/O before or after the
/// closure.
pub(crate) fn with_typed_write<T, F, R>(engine: &mut dyn CpuEngine, address: u64, f: F) -> Result<R>
where
    T: KnownLayout + FromBytes + IntoBytes + Immutable,
    F: FnOnce(&mut T) -> Result<R>,
{
    let len = std::mem::size_of::<T>();
    if let Some(bytes) = engine.host_slice_mut(address, len)
        && let Ok(view) = T::mut_from_bytes(bytes)
    {
        // Zero-fill first so padding and unset fields read as zero.
        view.as_mut_bytes().fill(0);
        return f(view);
    }
    // Staged fallback: edit an aligned host copy, then write the whole struct
    // back in one shot.
    let mut buf = AlignedBuf::<T>::zeroed();
    // Map the cast error to an owned message: `CastError` borrows the buffer,
    // which cannot outlive this function.
    let view = T::mut_from_bytes(buf.as_mut_bytes())
        .map_err(|_| anyhow::anyhow!("staged typed write buffer is misaligned"))?;
    let result = f(view)?;
    engine
        .mem_write(address, buf.as_bytes())
        .context("failed to stage-write typed struct to guest memory")?;
    Ok(result)
}

/// Read a whole guest struct into a host copy (the read-modify-write
/// snapshot).
///
/// The in/out structs (`MENUITEMINFO`, `OPENFILENAME`) are read into a `Copy`
/// value, edited on the host copy (which may touch the engine between read
/// and write — the string writes), then committed whole with
/// [`write_typed_copy`], so the guest's untouched fields and pad bytes
/// survive. This is the `with_typed_read` form that copies the whole struct
/// out; see [`with_typed_read`] for the borrow rule and staging fallback.
pub(crate) fn read_typed_copy<T>(engine: &mut dyn CpuEngine, address: u64) -> Result<T>
where
    T: KnownLayout + Immutable + FromBytes + Copy,
{
    with_typed_read::<T, _, _>(engine, address, |view| Ok(*view))
}

/// Write a whole host struct copy back to a guest address (the
/// read-modify-write commit).
///
/// The zero-fill write view is overwritten field-for-field by `value`, so the
/// snapshot's pad bytes (carried by the copy) land in the guest unchanged —
/// the in/out semantics real Windows structs use. See [`with_typed_write`]
/// for the borrow rule and staging fallback.
pub(crate) fn write_typed_copy<T>(engine: &mut dyn CpuEngine, address: u64, value: T) -> Result<()>
where
    T: KnownLayout + FromBytes + IntoBytes + Immutable,
{
    with_typed_write::<T, _, _>(engine, address, |view| {
        *view = value;
        Ok(())
    })
}
