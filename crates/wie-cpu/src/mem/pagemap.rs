//! Sparse run-length guest page map (state + protect) for SPC / VirtualQuery.
//!
//! Absence of a page key means [`PageState::Free`]. Host storage is owned by the
//! backend; this map is the Windows-visible correctness plane only.

use super::backend::{PAGE_SHIFT, PAGE_SIZE};
use super::protect::{AccessKind, PageProtect};
use crate::CpuError;
use std::collections::BTreeMap;
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Guest page lifecycle state (Microsoft Learn page states).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PageState {
    /// Not part of any allocation.
    #[default]
    Free,
    /// Reserved but not committed — all access denied.
    Reserved,
    /// Committed — access gated by protect bits.
    Committed,
}

impl PageState {
    #[inline]
    fn to_bits(self) -> u64 {
        match self {
            Self::Free => 0,
            Self::Reserved => 1,
            Self::Committed => 2,
        }
    }
    #[inline]
    fn from_bits(v: u64) -> Self {
        match v & 0b11 {
            1 => Self::Reserved,
            2 => Self::Committed,
            _ => Self::Free,
        }
    }
}

/// Dense slot for [`PageProtect`] inside the packed cache.
///
/// Independent of the Windows `PAGE_*` numbering so the cache layout does not
/// have to widen if a protection with a large constant is ever added.
#[inline]
fn protect_to_slot(p: PageProtect) -> u8 {
    match p {
        PageProtect::NoAccess => 0,
        PageProtect::ReadOnly => 1,
        PageProtect::ReadWrite => 2,
        PageProtect::Execute => 3,
        PageProtect::ExecuteRead => 4,
        PageProtect::ExecuteReadWrite => 5,
    }
}

#[inline]
fn protect_from_slot(slot: u8) -> Option<PageProtect> {
    match slot {
        0 => Some(PageProtect::NoAccess),
        1 => Some(PageProtect::ReadOnly),
        2 => Some(PageProtect::ReadWrite),
        3 => Some(PageProtect::Execute),
        4 => Some(PageProtect::ExecuteRead),
        5 => Some(PageProtect::ExecuteReadWrite),
        _ => None,
    }
}

/// Packed hot-run cache. Zero = invalid. Bit layout:
/// - bit 0        : valid
/// - bits 1..3    : state (2 bits)
/// - bits 3..11   : protect slot (8 bits, see [`protect_to_slot`])
/// - bits 11..35  : start_page (24 bits — up to 64 GiB VA)
/// - bits 35..59  : pages_len (24 bits — up to 64 GiB span)
///
/// Runs whose start or length exceed the 24-bit window are simply not cached.
#[inline]
fn pack_cache(run: PageRun) -> u64 {
    let len = run.end_page.saturating_sub(run.start_page);
    if run.start_page >> 24 != 0 || len == 0 || len >> 24 != 0 {
        return 0;
    }
    let mut v: u64 = 1; // valid
    v |= (run.state.to_bits() & 0b11) << 1;
    v |= (u64::from(protect_to_slot(run.protect)) & 0xff) << 3;
    v |= (run.start_page & 0xff_ffff) << 11;
    v |= (len & 0xff_ffff) << 35;
    v
}

#[inline]
fn cache_hit(packed: u64, page_key: u64) -> Option<PageRun> {
    if packed & 1 == 0 {
        return None;
    }
    let start = (packed >> 11) & 0xff_ffff;
    let len = (packed >> 35) & 0xff_ffff;
    let end = start.saturating_add(len);
    if page_key < start || page_key >= end {
        return None;
    }
    let state = PageState::from_bits(packed >> 1);
    let protect = protect_from_slot(u8::try_from((packed >> 3) & 0xff).unwrap_or(0))?;
    Some(PageRun {
        start_page: start,
        end_page: end,
        state,
        protect,
    })
}

/// One homogeneous run of guest pages `[start_page, end_page)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRun {
    /// Inclusive start page key (`va >> 12`).
    pub start_page: u64,
    /// Exclusive end page key.
    pub end_page: u64,
    /// Run state (never Free inside the map).
    pub state: PageState,
    /// Windows `PAGE_*` when committed; ignored for reserved (treat as no access).
    pub protect: PageProtect,
}

impl PageRun {
    /// Number of pages in the run.
    #[must_use]
    pub fn page_count(&self) -> u64 {
        self.end_page.saturating_sub(self.start_page)
    }

    /// Inclusive start guest VA.
    #[must_use]
    pub fn start_va(&self) -> u64 {
        self.start_page.saturating_mul(PAGE_SIZE)
    }

    /// Exclusive end guest VA.
    #[must_use]
    pub fn end_va(&self) -> u64 {
        self.end_page.saturating_mul(PAGE_SIZE)
    }
}

/// Sparse run-length page map: `start_page → PageRun` (non-overlapping, sorted).
///
/// The hot-path run cache is a lock-free packed [`AtomicU64`] — one relaxed
/// load per `lookup`, one relaxed store per `fill_cache`. Previously used
/// `Mutex<RunCache>`, which took a mutex per spanned page on every SPC
/// `check_access` call (the dominant SPC cost on streaming workloads).
#[derive(Debug)]
pub struct PageMap {
    /// Keyed by `start_page`; runs do not overlap.
    ///
    /// Behind an [`RwLock`] because the map is walked from paths that do not
    /// hold the outer `GuestMemory` lock — the JIT load/store stubs fall back
    /// to it — while another thread may be re-mapping inside `VirtualAlloc` /
    /// `VirtualFree`. Mutating the `BTreeMap` through a shared reference is
    /// aliasing UB, so a version word (seqlock) would only detect the tear, and
    /// an RCU snapshot would pay a full-tree clone on every mutation. The
    /// guard never escapes this module: every accessor returns an owned value.
    runs: RwLock<BTreeMap<u64, PageRun>>,
    /// Packed hot-run cache (see [`pack_cache`] / [`cache_hit`]).
    cache: AtomicU64,
}

impl Default for PageMap {
    fn default() -> Self {
        Self {
            runs: RwLock::new(BTreeMap::new()),
            cache: AtomicU64::new(0),
        }
    }
}

impl Clone for PageMap {
    fn clone(&self) -> Self {
        Self {
            runs: RwLock::new(self.read_runs().clone()),
            cache: AtomicU64::new(self.cache.load(Ordering::Relaxed)),
        }
    }
}

impl PageMap {
    /// Empty map (all VA free).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Invalidate the hot-path run cache (after any mutation).
    fn bump_cache(&mut self) {
        self.cache.store(0, Ordering::Relaxed);
    }

    /// Shared guard on the run tree. Poison cannot lose a walk: the tree is
    /// plain data and every writer either finished or did not start.
    fn read_runs(&self) -> RwLockReadGuard<'_, BTreeMap<u64, PageRun>> {
        self.runs.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Exclusive guard on the run tree.
    fn write_runs(&self) -> RwLockWriteGuard<'_, BTreeMap<u64, PageRun>> {
        self.runs.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Look up the run covering `page_key`, if any.
    #[must_use]
    pub fn lookup(&self, page_key: u64) -> Option<PageRun> {
        let packed = self.cache.load(Ordering::Relaxed);
        if let Some(hit) = cache_hit(packed, page_key) {
            return Some(hit);
        }
        // Greatest start_page ≤ page_key.
        lookup_in(&self.read_runs(), page_key)
    }

    /// First stored run start strictly above `page`, as a page key.
    ///
    /// Exists so callers that only need one boundary (`VirtualQuery`'s free-run
    /// end) do not have to materialise the tree; the guard is dropped here.
    #[must_use]
    pub fn first_start_page_after(&self, page: u64) -> Option<u64> {
        self.read_runs()
            .range((Excluded(page), Unbounded))
            .next()
            .map(|(&k, _)| k)
    }

    /// Cache a successful lookup for subsequent adjacent accesses.
    fn fill_cache(&self, run: PageRun) {
        let packed = pack_cache(run);
        if packed != 0 {
            self.cache.store(packed, Ordering::Relaxed);
        }
    }

    /// Software permission check for `[va, va+len)`.
    ///
    /// All-or-nothing: if any spanned page is free, reserved, or wrong protect,
    /// the whole operation fails and no host bytes are touched by the caller.
    pub fn check_access(&self, va: u64, len: usize, kind: AccessKind) -> Result<(), CpuError> {
        if len == 0 {
            return Ok(());
        }
        let len_u64 = u64::try_from(len)
            .map_err(|_| CpuError::Message(format!("access length {len} does not fit u64")))?;
        let end = va
            .checked_add(len_u64)
            .ok_or_else(|| CpuError::Message(format!("access overflow at {va:#x}+{len:#x}")))?;

        let mut page = va >> PAGE_SHIFT;
        let last_page = end.saturating_sub(1) >> PAGE_SHIFT;
        while page <= last_page {
            let run = self.lookup(page);
            let Some(run) = run else {
                return Err(access_denied(va, kind, "unmapped"));
            };
            if run.state != PageState::Committed {
                return Err(access_denied(va, kind, "not committed"));
            }
            if !run.protect.allows(kind) {
                return Err(access_denied(va, kind, "permission denied"));
            }
            self.fill_cache(run);
            // Advance past this homogeneous run (or at least this page).
            let next = run.end_page;
            if next <= page {
                // Defensive: corrupt run would infinite-loop.
                return Err(CpuError::Message("pagemap corrupt run".into()));
            }
            page = next;
        }
        Ok(())
    }

    /// Mark `[address, address+size)` as `state`/`protect`, splitting and merging runs.
    ///
    /// `size` and `address` must be page-aligned; `size == 0` is a no-op.
    /// Setting [`PageState::Free`] removes entries from the map.
    pub fn set_range(
        &mut self,
        address: u64,
        size: usize,
        state: PageState,
        protect: PageProtect,
    ) -> Result<(), CpuError> {
        if size == 0 {
            return Ok(());
        }
        if !address.is_multiple_of(PAGE_SIZE) {
            return Err(CpuError::Message(format!(
                "pagemap set_range address {address:#x} not page-aligned"
            )));
        }
        let size_u64 = u64::try_from(size)
            .map_err(|_| CpuError::Message(format!("pagemap size {size} does not fit u64")))?;
        if !size_u64.is_multiple_of(PAGE_SIZE) {
            return Err(CpuError::Message(format!(
                "pagemap set_range size {size:#x} not page-aligned"
            )));
        }
        let end = address.checked_add(size_u64).ok_or_else(|| {
            CpuError::Message(format!("pagemap overflow at {address:#x}+{size:#x}"))
        })?;

        let start_page = address >> PAGE_SHIFT;
        let end_page = end >> PAGE_SHIFT;
        self.set_page_range(start_page, end_page, state, protect);
        Ok(())
    }

    /// Core mutator on page-key half-open range `[start_page, end_page)`.
    ///
    /// Holds the write guard across the whole split / remove / merge sequence,
    /// so a concurrent walk sees the map either fully before or fully after the
    /// remap and never a half-replaced tree.
    fn set_page_range(
        &mut self,
        start_page: u64,
        end_page: u64,
        state: PageState,
        protect: PageProtect,
    ) {
        if start_page >= end_page {
            return;
        }
        self.bump_cache();
        let mut runs = self.write_runs();

        // Collect runs that overlap [start_page, end_page) and any adjacent merges.
        // First split any run that straddles the boundaries so we can replace the middle.
        split_at_in(&mut runs, start_page);
        split_at_in(&mut runs, end_page);

        // Remove fully covered runs inside [start_page, end_page).
        let doomed: Vec<u64> = runs.range(start_page..end_page).map(|(&k, _)| k).collect();
        for k in doomed {
            runs.remove(&k);
        }

        if state != PageState::Free {
            let mut new_run = PageRun {
                start_page,
                end_page,
                state,
                protect,
            };
            // Merge with left neighbour if homogeneous.
            if start_page > 0
                && let Some(left) = lookup_exact_end_in(&runs, start_page)
                && left.state == state
                && left.protect == protect
            {
                new_run.start_page = left.start_page;
                runs.remove(&left.start_page);
            }
            // Merge with right neighbour.
            if let Some(right) = runs.get(&end_page).copied()
                && right.state == state
                && right.protect == protect
            {
                new_run.end_page = right.end_page;
                runs.remove(&end_page);
            }
            runs.insert(new_run.start_page, new_run);
        }
    }

    /// Homogeneous run starting at or covering `va` for VirtualQuery.
    #[must_use]
    pub fn query_run(&self, va: u64) -> Option<PageRun> {
        let page = va >> PAGE_SHIFT;
        self.lookup(page)
    }

    /// Number of stored runs (diagnostics / tests).
    #[must_use]
    pub fn run_count(&self) -> usize {
        self.read_runs().len()
    }
}

/// Greatest `start_page ≤ page_key` run that covers `page_key`.
fn lookup_in(runs: &BTreeMap<u64, PageRun>, page_key: u64) -> Option<PageRun> {
    let (&start, run) = runs.range(..=page_key).next_back()?;
    if page_key < run.end_page {
        debug_assert_eq!(start, run.start_page);
        Some(*run)
    } else {
        None
    }
}

/// Run whose exclusive end equals `page`, if stored as a single entry.
fn lookup_exact_end_in(runs: &BTreeMap<u64, PageRun>, page: u64) -> Option<PageRun> {
    let (&start, run) = runs.range(..page).next_back()?;
    if run.end_page == page && run.start_page == start {
        Some(*run)
    } else {
        None
    }
}

/// Split a run at `page` so that `page` is either a run start or outside all runs.
fn split_at_in(runs: &mut BTreeMap<u64, PageRun>, page: u64) {
    let Some(run) = lookup_in(runs, page) else {
        return;
    };
    if run.start_page == page {
        return;
    }
    // page is strictly inside [start, end).
    runs.remove(&run.start_page);
    let left = PageRun {
        start_page: run.start_page,
        end_page: page,
        state: run.state,
        protect: run.protect,
    };
    let right = PageRun {
        start_page: page,
        end_page: run.end_page,
        state: run.state,
        protect: run.protect,
    };
    runs.insert(left.start_page, left);
    runs.insert(right.start_page, right);
}

fn access_denied(va: u64, kind: AccessKind, why: &str) -> CpuError {
    let op = match kind {
        AccessKind::Read => "mem_read",
        AccessKind::Write => "mem_write",
        AccessKind::Execute => "instruction fetch",
    };
    CpuError::Message(format!("{op} {why} {va:#x}"))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::mem::protect::{AccessKind, PageProtect};

    #[test]
    fn set_and_lookup_committed() {
        let mut m = PageMap::new();
        m.set_range(0x1000, 0x2000, PageState::Committed, PageProtect::ReadWrite)
            .expect("set");
        let r = m.lookup(0x1000 >> 12).expect("run");
        assert_eq!(r.state, PageState::Committed);
        assert_eq!(r.protect, PageProtect::ReadWrite);
        assert_eq!(r.page_count(), 2);
        assert!(m.lookup(0x3000 >> 12).is_none());
    }

    #[test]
    fn merge_adjacent_same_protect() {
        let mut m = PageMap::new();
        m.set_range(0x1000, 0x1000, PageState::Committed, PageProtect::ReadOnly)
            .expect("a");
        m.set_range(0x2000, 0x1000, PageState::Committed, PageProtect::ReadOnly)
            .expect("b");
        assert_eq!(m.run_count(), 1);
        let r = m.lookup(0x1000 >> 12).expect("run");
        assert_eq!(r.end_page - r.start_page, 2);
    }

    #[test]
    fn split_on_protect_change() {
        let mut m = PageMap::new();
        m.set_range(0x1000, 0x3000, PageState::Committed, PageProtect::ReadWrite)
            .expect("set");
        m.set_range(0x2000, 0x1000, PageState::Committed, PageProtect::ReadOnly)
            .expect("mid");
        assert_eq!(m.run_count(), 3);
        assert_eq!(
            m.lookup(0x1000 >> 12).expect("l").protect,
            PageProtect::ReadWrite
        );
        assert_eq!(
            m.lookup(0x2000 >> 12).expect("m").protect,
            PageProtect::ReadOnly
        );
        assert_eq!(
            m.lookup(0x3000 >> 12).expect("r").protect,
            PageProtect::ReadWrite
        );
    }

    #[test]
    fn free_removes_range() {
        let mut m = PageMap::new();
        m.set_range(0x1000, 0x3000, PageState::Committed, PageProtect::ReadWrite)
            .expect("set");
        m.set_range(0x2000, 0x1000, PageState::Free, PageProtect::NoAccess)
            .expect("free");
        assert!(m.lookup(0x2000 >> 12).is_none());
        assert!(m.lookup(0x1000 >> 12).is_some());
        assert!(m.lookup(0x3000 >> 12).is_some());
    }

    #[test]
    fn check_access_ro_write_fails() {
        let mut m = PageMap::new();
        m.set_range(0x1000, 0x1000, PageState::Committed, PageProtect::ReadOnly)
            .expect("set");
        assert!(m.check_access(0x1000, 8, AccessKind::Read).is_ok());
        assert!(m.check_access(0x1000, 8, AccessKind::Write).is_err());
        assert!(m.check_access(0x1000, 8, AccessKind::Execute).is_err());
    }

    #[test]
    fn check_access_rx_fetch_ok() {
        let mut m = PageMap::new();
        m.set_range(
            0x1000,
            0x1000,
            PageState::Committed,
            PageProtect::ExecuteRead,
        )
        .expect("set");
        assert!(m.check_access(0x1000, 15, AccessKind::Execute).is_ok());
        assert!(m.check_access(0x1000, 8, AccessKind::Write).is_err());
    }

    #[test]
    fn check_access_reserved_denied() {
        let mut m = PageMap::new();
        m.set_range(0x1000, 0x1000, PageState::Reserved, PageProtect::NoAccess)
            .expect("set");
        assert!(m.check_access(0x1000, 1, AccessKind::Read).is_err());
    }

    #[test]
    fn check_access_cross_page_all_or_nothing() {
        let mut m = PageMap::new();
        m.set_range(
            0x1000,
            0x1000,
            PageState::Committed,
            PageProtect::ExecuteReadWrite,
        )
        .expect("a");
        // Second page unmapped — spanning write must fail entirely.
        let err = m
            .check_access(0x1ffc, 8, AccessKind::Write)
            .expect_err("should fail");
        let msg = err.to_string();
        assert!(msg.contains("unmapped") || msg.contains("not committed"));
    }

    #[test]
    fn check_access_cross_page_ok_when_both_committed() {
        let mut m = PageMap::new();
        m.set_range(0x1000, 0x2000, PageState::Committed, PageProtect::ReadWrite)
            .expect("set");
        m.check_access(0x1ffc, 8, AccessKind::Write).expect("ok");
    }
}
