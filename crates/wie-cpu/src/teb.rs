//! Per-thread TEB ownership, allocation, and initialization (per-thread TEB Task 1).
//!
//! Real Windows gives every thread its own TEB page and puts that page's
//! address in the thread's GS segment base. WIE models the same ownership:
//! the primary thread keeps ONE TEB page at the fixed [`crate::GS_BASE`]; each
//! worker owns a distinct page from a pool and its engine's GS base is bound
//! to it, so GS-relative accesses (e.g. `gs:[0x68]` for last-error) resolve to
//! the engine's OWN page — never one shared mirror.
//!
//! This module is the ownership seam the per-thread phase builds on:
//! - [`PerThreadTeb`] owns one guest TEB page and knows the standard x64 field
//!   offsets ([`crate::guest_layout`]); [`PerThreadTeb::init`] zero-fills the
//!   page and writes the standard fields through the engine — plain
//!   soft-translated guest memory, never host pointers.
//! - [`TebPool`] hands out distinct, non-overlapping TEB pages from a
//!   caller-owned guest range.
//!
//! `CpuEngine` carries a per-engine GS base ([`CpuEngine::set_gs_base`] /
//! [`CpuEngine::gs_base`]); both concrete backends honor it: the iced
//! `exec::effective_address` and the JIT `jit::lower::gpr::effective_addr`
//! resolve GS-relative addresses against the engine's current base, and the
//! JIT last-error trampolines (`jit::trampolines`) read `ctx.gs_base` at run
//! time. The primary TEB keeps its fixed address (GS_BASE); worker TEBs are
//! bound at spawn and re-affirmed at every quantum activation:
//!
//! ```text
//! trait CpuEngine {
//!     /// Per-engine GS segment base (per-thread TEB base). Required so a
//!     /// worker's GS-relative accesses resolve to ITS TEB page.
//!     fn set_gs_base(&mut self, base: u64) -> Result<(), CpuError>;
//!     fn gs_base(&self) -> u64;
//! }
//! ```
//!
//! The WinApiState last-error helpers (`wie_winapi::state`) publish to the
//! ACTIVE engine's GS-relative TEB slot (`gs_base() + TEB_LAST_ERROR_OFFSET`):
//! the primary engine writes the fixed `GS_BASE` page, each worker engine its
//! own `PerThreadTeb` page.

use crate::guest_layout::{
    TEB_LAST_ERROR_OFFSET, TEB_PAGE_SIZE, TEB_PEB_OFFSET, TEB_SELF_OFFSET, TEB_STACK_BASE_OFFSET,
    TEB_STACK_LIMIT_OFFSET,
};
use crate::{CpuEngine, CpuError, GS_BASE};

/// `TEB_PAGE_SIZE` as `u64` (`u64::try_from` is not const-stable).
#[allow(clippy::as_conversions)] // const fn: try_from not const-stable
const fn page_size_u64() -> u64 {
    TEB_PAGE_SIZE as u64
}

/// Values [`PerThreadTeb::init`] writes into a TEB page.
///
/// Stack bounds are per-thread; the PEB pointer is process-wide — every TEB
/// points at the one process PEB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TebInit {
    /// `NT_TIB.StackBase` — exclusive top of this thread's stack.
    pub stack_top: u64,
    /// `NT_TIB.StackLimit` — low end of this thread's committed stack.
    pub stack_limit: u64,
    /// `TEB.ProcessEnvironmentBlock` — the shared process PEB page.
    pub peb_va: u64,
}

/// Ownership of one per-thread TEB page in software-translated guest memory.
///
/// Invariants: `va` is 4 KiB aligned and the page is exactly [`TEB_PAGE_SIZE`]
/// bytes (Windows x64: one page per TEB). Construction is infallible — the
/// only producers ([`TebPool::allocate`] and [`PerThreadTeb::primary`])
/// guarantee the alignment by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerThreadTeb {
    /// Guest VA of the TEB page base — what a per-thread GS base must point at.
    va: u64,
}

impl PerThreadTeb {
    /// Wrap a page-aligned guest VA as a TEB page (precondition: 4 KiB aligned).
    #[must_use]
    pub fn new(va: u64) -> Self {
        Self { va }
    }

    /// The primary thread's TEB page at the fixed [`crate::GS_BASE`].
    ///
    /// `RuntimeMemoryLayout` pins `teb_low.base == GS_BASE` (compile-time gate
    /// in `wie_runtime::memory`), so this is the exact page session init maps
    /// and the primary engine's GS-relative last-error slot keeps publishing
    /// to.
    #[must_use]
    pub const fn primary() -> Self {
        Self { va: GS_BASE }
    }

    /// Guest VA of the TEB page base.
    #[must_use]
    pub const fn va(self) -> u64 {
        self.va
    }

    /// Guest VA of `NT_TIB.StackBase`.
    #[must_use]
    pub const fn stack_base_va(self) -> u64 {
        self.va + TEB_STACK_BASE_OFFSET
    }

    /// Guest VA of `NT_TIB.StackLimit`.
    #[must_use]
    pub const fn stack_limit_va(self) -> u64 {
        self.va + TEB_STACK_LIMIT_OFFSET
    }

    /// Guest VA of `NT_TIB.Self`.
    #[must_use]
    pub const fn self_va(self) -> u64 {
        self.va + TEB_SELF_OFFSET
    }

    /// Guest VA of `TEB.ProcessEnvironmentBlock`.
    #[must_use]
    pub const fn peb_va(self) -> u64 {
        self.va + TEB_PEB_OFFSET
    }

    /// Guest VA of `TEB.LastErrorValue` — the per-thread last-error slot.
    #[must_use]
    pub const fn last_error_va(self) -> u64 {
        self.va + TEB_LAST_ERROR_OFFSET
    }

    /// Initialize the owned TEB page in guest memory.
    ///
    /// Zero-fills the whole page first — "a fresh TEB is all-zero except the
    /// fields written here" is an invariant of this method, not of the
    /// underlying arena (a reused or poisoned page still comes out clean) —
    /// then writes `Self`, `StackBase`, `StackLimit`, the PEB pointer, and a
    /// zero last-error. PEB/PP *contents* stay the caller's job: they are
    /// process-level data shared by every TEB.
    ///
    /// # Errors
    /// Any guest-memory write fails (the page must be mapped first).
    pub fn init(&self, engine: &mut dyn CpuEngine, init: &TebInit) -> Result<(), CpuError> {
        engine.mem_write(self.va, &vec![0_u8; TEB_PAGE_SIZE])?;
        engine.mem_write(self.stack_base_va(), &init.stack_top.to_le_bytes())?;
        engine.mem_write(self.stack_limit_va(), &init.stack_limit.to_le_bytes())?;
        engine.mem_write(self.self_va(), &self.va.to_le_bytes())?;
        engine.mem_write(self.peb_va(), &init.peb_va.to_le_bytes())?;
        engine.mem_write(self.last_error_va(), &0_u32.to_le_bytes())?;
        Ok(())
    }
}

/// Allocator for distinct per-thread TEB pages out of a contiguous guest range.
///
/// Linear bump allocator over `[base, base + size)`: every handed-out page is
/// 4 KiB aligned, exactly [`TEB_PAGE_SIZE`] bytes, and disjoint from every
/// other page the pool has handed out — "distinct ownership" by construction.
/// The range itself is caller-owned: session init must map the pages before
/// [`PerThreadTeb::init`] writes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TebPool {
    /// Inclusive first allocatable VA.
    base: u64,
    /// Exclusive end VA (`base + size`).
    end: u64,
    /// Next free VA (bump cursor).
    next: u64,
}

impl TebPool {
    /// Build a pool over `[base, base + size)`. `None` when the range is not
    /// page-aligned, holds no whole TEB page, or overflows `u64`.
    #[must_use]
    pub fn new(base: u64, size: usize) -> Option<Self> {
        if !base.is_multiple_of(0x1000)
            || size < TEB_PAGE_SIZE
            || !size.is_multiple_of(TEB_PAGE_SIZE)
        {
            return None;
        }
        let size_u64 = u64::try_from(size).ok()?;
        let end = base.checked_add(size_u64)?;
        Some(Self {
            base,
            end,
            next: base,
        })
    }

    /// Maximum number of TEB pages this pool can hand out.
    #[must_use]
    pub fn capacity(self) -> usize {
        let span = self.end - self.base;
        usize::try_from(span / page_size_u64()).unwrap_or(0)
    }

    /// Number of TEB pages handed out so far.
    #[must_use]
    pub fn allocated(self) -> usize {
        let span = self.next - self.base;
        usize::try_from(span / page_size_u64()).unwrap_or(0)
    }

    /// Number of TEB pages still available.
    #[must_use]
    pub fn remaining(self) -> usize {
        self.capacity().saturating_sub(self.allocated())
    }

    /// Hand out the next TEB page, or `None` when the pool is exhausted.
    pub fn allocate(&mut self) -> Option<PerThreadTeb> {
        if self.next >= self.end {
            return None;
        }
        let teb = PerThreadTeb::new(self.next);
        self.next = self.next.saturating_add(page_size_u64());
        Some(teb)
    }
}

#[cfg(test)]
#[allow(clippy::as_conversions, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::guest_layout::TEB_LAST_ERROR_VA;
    use crate::{IcedCpu, RwxPerms};

    /// The primary TEB keeps its fixed guest address (GS_BASE): the layout
    /// pins `teb_low.base == GS_BASE` and the guest-stub last-error mirror
    /// publishes to `TEB_LAST_ERROR_VA`.
    #[test]
    fn primary_teb_sits_at_the_fixed_gs_base() {
        let primary = PerThreadTeb::primary();
        assert_eq!(primary.va(), GS_BASE);
        assert_eq!(primary.last_error_va(), TEB_LAST_ERROR_VA);
        assert_eq!(primary.self_va(), GS_BASE + TEB_SELF_OFFSET);
    }

    /// A pool hands out distinct, page-aligned, non-overlapping TEB pages and
    /// reports exhaustion instead of reusing a page — distinct per-thread
    /// ownership by construction.
    #[test]
    fn teb_pool_allocates_distinct_pages() {
        let pool_base = 0x0000_7000_1000_0000_u64;
        let mut pool = TebPool::new(pool_base, 4 * TEB_PAGE_SIZE).expect("valid pool");
        assert_eq!(pool.capacity(), 4);
        assert_eq!(pool.remaining(), 4);

        let tebs: Vec<PerThreadTeb> = (0..4)
            .map(|_| pool.allocate().expect("page within capacity"))
            .collect();

        // Linear bump: each page is one TEB_PAGE_SIZE past the previous.
        for (i, teb) in tebs.iter().enumerate() {
            assert_eq!(teb.va(), pool_base + i as u64 * TEB_PAGE_SIZE as u64);
        }
        // Pairwise distinct + disjoint: each owns exactly [va, va + page).
        for i in 0..tebs.len() {
            for j in (i + 1)..tebs.len() {
                assert_ne!(tebs[i].va(), tebs[j].va());
                assert!(tebs[i].va() + TEB_PAGE_SIZE as u64 <= tebs[j].va());
            }
        }
        // Per-thread state is distinct even where guest code reads it: each
        // TEB's last-error slot is a different guest VA.
        assert_ne!(tebs[0].last_error_va(), tebs[1].last_error_va());

        assert!(
            pool.allocate().is_none(),
            "pool must be exhausted after capacity pages"
        );
        assert_eq!(pool.remaining(), 0);
    }

    /// A misaligned or partial pool range is rejected at construction.
    #[test]
    fn teb_pool_rejects_invalid_geometry() {
        assert!(TebPool::new(0x0000_7000_1000_0001, TEB_PAGE_SIZE).is_none());
        assert!(TebPool::new(0x0000_7000_1000_0000, TEB_PAGE_SIZE / 2).is_none());
        assert!(TebPool::new(u64::MAX - 0x100, TEB_PAGE_SIZE).is_none());
        assert!(TebPool::new(0x0000_7000_1000_0000, TEB_PAGE_SIZE).is_some());
    }

    /// `PerThreadTeb::init` zero-fills the page and writes exactly the
    /// standard x64 TEB fields — a poisoned (all-0xFF) page comes out clean.
    #[test]
    fn init_zero_fills_page_and_writes_standard_fields() {
        let mut cpu = IcedCpu::open_x86_64();
        let teb = PerThreadTeb::new(0x0000_7000_1000_0000);
        cpu.mem_map(teb.va(), TEB_PAGE_SIZE, RwxPerms::READ_WRITE)
            .expect("map TEB page");
        // Poison the whole page: zero-initialization must come from `init`,
        // not from a fresh (already zero) arena.
        cpu.mem_write(teb.va(), &vec![0xFF_u8; TEB_PAGE_SIZE])
            .expect("poison TEB page");

        let init = TebInit {
            stack_top: 0x0000_0000_2008_0000,
            stack_limit: 0x0000_0000_2000_0000,
            peb_va: teb.va() + 0x800,
        };
        teb.init(&mut cpu, &init).expect("init TEB");

        // Written fields land at the standard x64 TEB offsets.
        let mut read_u64 = |va: u64| -> u64 {
            let mut bytes = [0_u8; 8];
            cpu.mem_read(va, &mut bytes).expect("read TEB field");
            u64::from_le_bytes(bytes)
        };
        assert_eq!(read_u64(teb.stack_base_va()), init.stack_top);
        assert_eq!(read_u64(teb.stack_limit_va()), init.stack_limit);
        assert_eq!(read_u64(teb.self_va()), teb.va());
        assert_eq!(read_u64(teb.peb_va()), init.peb_va);
        assert_eq!(read_u64(teb.last_error_va()), 0);

        // Everything else is zero: the 0xFF poison is fully cleared. The
        // exact 8-byte assertions above already cover each written slot, so
        // the loop skips the whole slot span and checks the rest.
        let mut page = vec![0_u8; TEB_PAGE_SIZE];
        cpu.mem_read(teb.va(), &mut page).expect("read TEB page");
        let written: Vec<u64> = [
            teb.stack_base_va(),
            teb.stack_limit_va(),
            teb.self_va(),
            teb.peb_va(),
            teb.last_error_va(),
        ]
        .iter()
        .flat_map(|&slot| (0..8).map(move |i| slot + u64::try_from(i).unwrap_or(0)))
        .collect();
        for (off, byte) in page.iter().enumerate() {
            let va = teb.va() + u64::try_from(off).unwrap_or(0);
            if !written.contains(&va) {
                assert_eq!(*byte, 0, "TEB+{off:#x} must be zero-initialized");
            }
        }
    }
}
