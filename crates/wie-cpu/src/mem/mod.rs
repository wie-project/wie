//! Guest virtual memory for interpreter + JIT (x86-64 only).
//!
//! Sole storage path: contiguous anonymous **mmap arenas** ([`MmapArenaBackend`]).
//! Soft translate only (guest VA ≠ host VA). Layout:
//! - [`GuestMemBackend`] — storage trait (mmap arena implements it)
//! - [`MmapArenaBackend`] — every map → one demand-zero arena
//! - [`RegionTable`] — named layout ranges (`host_base` filled from arenas)
//! - [`PageMap`] / [`protect`] — Windows page state + software permission checks
//! - [`GuestMemory`] — facade used by iced/JIT (SPC on read/write/fetch)

use std::sync::atomic::{AtomicU64, Ordering};

mod alloc;
mod arena;
mod backend;
mod map;
mod mmap_arena;
mod pagemap;
pub mod protect;
mod region;
mod rw;
mod vad;

#[cfg(test)]
mod tests;

pub use backend::{GuestMemBackend, PAGE_SIZE, PAGE_SIZE_USIZE};
pub use mmap_arena::MmapArenaBackend;
pub use pagemap::{PageMap, PageRun, PageState};
pub use region::{GuestRegion, RegionKind, RegionTable};
pub(super) use vad::va_error;
pub use vad::{
    ERROR_INVALID_ADDRESS, ERROR_INVALID_PARAMETER, ERROR_NOT_ENOUGH_MEMORY,
    GUEST_ALLOC_GRANULARITY, MEM_COMMIT, MEM_DECOMMIT, MEM_FREE, MEM_IMAGE, MEM_PRIVATE,
    MEM_RELEASE, MEM_RESERVE, MemType, VadNode, VadTable, align_down, align_up,
    win32_from_cpu_error,
};

/// `MEMORY_BASIC_INFORMATION` (x64 layout, 48 bytes) for `VirtualQuery`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemoryBasicInformation {
    /// Start of the homogeneous run containing the query address.
    pub base_address: u64,
    /// Allocation base (`0` when free).
    pub allocation_base: u64,
    /// Protect at reserve / create time (`0` when free).
    pub allocation_protect: u32,
    /// Bytes from [`Self::base_address`] to the end of the homogeneous run.
    pub region_size: u64,
    /// `MEM_COMMIT` / `MEM_RESERVE` / `MEM_FREE`.
    pub state: u32,
    /// Page protect when committed; otherwise `0`.
    pub protect: u32,
    /// `MEM_PRIVATE` / `MEM_IMAGE` / `0` when free.
    pub type_: u32,
}

impl MemoryBasicInformation {
    /// Pack into the 48-byte guest `MEMORY_BASIC_INFORMATION` layout (x64).
    #[must_use]
    pub fn to_bytes(self) -> [u8; 48] {
        let mut mbi = [0_u8; 48];
        mbi[0..8].copy_from_slice(&self.base_address.to_le_bytes());
        mbi[8..16].copy_from_slice(&self.allocation_base.to_le_bytes());
        mbi[16..20].copy_from_slice(&self.allocation_protect.to_le_bytes());
        // 20..24: padding / PartitionId
        mbi[24..32].copy_from_slice(&self.region_size.to_le_bytes());
        mbi[32..36].copy_from_slice(&self.state.to_le_bytes());
        mbi[36..40].copy_from_slice(&self.protect.to_le_bytes());
        mbi[40..44].copy_from_slice(&self.type_.to_le_bytes());
        // 44..48: padding
        mbi
    }
}

#[cfg(test)]
use backend::page_key;

/// Guest memory: mmap arenas + region registry + software page map (SPC).
///
/// Storage is always [`MmapArenaBackend`]. Permission enforcement lives here
/// (not inside the backend) so SPC is the sole correctness plane.
pub struct GuestMemory {
    backend: MmapArenaBackend,
    regions: RegionTable,
    pages: PageMap,
    vad: VadTable,
    /// Bumped when protect/commit/release change; JIT flushes TLB on change.
    ///
    /// `AtomicU64` so concurrent readers can observe generation with
    /// acquire loads while structural writers bump under the process map lock.
    generation: AtomicU64,
}

impl Default for GuestMemory {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for GuestMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuestMemory")
            .field("backend", &self.backend.name())
            .field("regions", &self.regions.len())
            .field("page_runs", &self.pages.run_count())
            .field("vad", &self.vad.len())
            .field("generation", &self.generation.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl GuestMemory {
    /// Create guest memory with the sole mmap-arena storage backend.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            backend: MmapArenaBackend::new(),
            regions: RegionTable::new(),
            pages: PageMap::new(),
            vad: VadTable::new(),
            generation: AtomicU64::new(0),
        }
    }

    pub(crate) fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// Monotonic generation for TLB / pin invalidation.
    ///
    /// Bumped on map / protect / commit / decommit / release. JIT TLB and
    /// region pins store this value at install time and miss when it diverges.
    /// Acquire load pairs with release bumps so concurrent threads see a
    /// consistent metadata epoch (even while guest execute is still process-locked).
    #[must_use]
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Bump the memory generation (release store for visibility).
    #[inline]
    fn bump_generation(&self) {
        // Saturating add without wrapping forever in pathological cases.
        let _ = self
            .generation
            .try_update(Ordering::AcqRel, Ordering::Acquire, |g| {
                Some(g.saturating_add(1))
            });
    }

    /// Full VAD allocation span when `addr` is an allocation base (`MEM_RELEASE`).
    #[must_use]
    pub(crate) fn allocation_span_at_base(&self, addr: u64) -> Option<(u64, usize)> {
        let node = self.vad.find_base(addr)?;
        let size = usize::try_from(node.size).ok()?;
        Some((node.allocation_base, size))
    }

    /// Software page map (tests / VirtualQuery plumbing).
    #[must_use]
    #[cfg_attr(not(test), expect(dead_code))]
    pub(crate) fn page_map(&self) -> &PageMap {
        &self.pages
    }
}

/// JIT TLB install result: host page base + software permission snapshot.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PageTlbEntry {
    /// Non-owning host pointer to the guest page base.
    pub host: *mut u8,
    /// Software read permission at install time.
    pub allow_r: bool,
    /// Software write permission at install time.
    pub allow_w: bool,
    /// [`GuestMemory::generation`] at install time.
    pub generation: u64,
}

/// Number of soft-translate pin slots filled for JIT (`JitCtx.pins`).
///
/// Layout: `[0]=stack`, `[1]=primary process heap`, `[2..]=largest private
/// VirtualAlloc / committed spans` (VA pin expansion).
pub(crate) const JIT_REGION_PIN_SLOTS: usize = 8;

/// Soft-translated region pin for JIT (stack / heap / VA arenas).
///
/// `allow_*` is the **intersection** of software rights over the whole range —
/// never more permissive than the slow path for any byte in the pin.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RegionPinInfo {
    /// Inclusive guest base VA.
    pub guest_base: u64,
    /// Exclusive guest end VA.
    pub guest_end: u64,
    /// Host base for soft translate (`host + (va - guest_base)`).
    pub host_base: *mut u8,
    /// Every page in range allows data read.
    pub allow_r: bool,
    /// Every page in range allows data write.
    pub allow_w: bool,
    /// [`GuestMemory::generation`] at pin build time.
    pub generation: u64,
}

#[derive(Clone, Copy)]
struct PageProtectMeta {
    allow_r: bool,
    allow_w: bool,
}
