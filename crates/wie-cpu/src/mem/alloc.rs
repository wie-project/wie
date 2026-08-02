//! GuestMemory virtual-allocation surface: `VirtualAlloc` / `VirtualFree` /
//! `VirtualProtect` / `VirtualQuery` plus the private `va_*` reserve / commit /
//! decommit / release helpers and the allocation-span VAD query.

use super::{
    ERROR_INVALID_ADDRESS, ERROR_INVALID_PARAMETER, ERROR_NOT_ENOUGH_MEMORY,
    GUEST_ALLOC_GRANULARITY, GuestMemBackend, MEM_COMMIT, MEM_DECOMMIT, MEM_FREE, MEM_RELEASE,
    MEM_RESERVE, MemType, MemoryBasicInformation, PAGE_SIZE, PageState, VadNode, align_down,
    align_up, protect, va_error,
};

/// Whether optional host mprotect dual-protection is enabled (`WIE_MPROTECT`, default on).
fn host_mprotect_enabled() -> bool {
    !matches!(
        std::env::var("WIE_MPROTECT"),
        Ok(v) if v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off")
    )
}

/// Host page size (cached). Guest granule remains 4 KiB.
fn host_page_size() -> usize {
    use std::sync::OnceLock;
    static SIZE: OnceLock<usize> = OnceLock::new();
    *SIZE.get_or_init(|| {
        // SAFETY: sysconf(_SC_PAGESIZE) is thread-safe and returns a positive page size.
        #[expect(unsafe_code)]
        let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if n > 0 {
            usize::try_from(n).unwrap_or(0x1000)
        } else {
            0x1000
        }
    })
}

// POSIX PROT_* (data plane only — guest execute is never host execute).
const HOST_PROT_READ: i32 = libc::PROT_READ;
const HOST_PROT_WRITE: i32 = libc::PROT_WRITE;

impl super::GuestMemory {
    /// `VirtualProtect` — change protect on committed pages; returns previous protect
    /// of the first page (after validating the full range).
    pub(crate) fn virtual_protect(
        &mut self,
        addr: u64,
        size: usize,
        new_protect: u32,
    ) -> Result<u32, crate::CpuError> {
        if size == 0 {
            return Err(va_error(ERROR_INVALID_PARAMETER, "VirtualProtect size 0"));
        }
        // Parsing *is* the validation: an unsupported `PAGE_*` cannot become a
        // `PageProtect`, so no separate `is_supported_protect` check is needed.
        let new_protect = protect::PageProtect::from_win32(new_protect).ok_or_else(|| {
            va_error(
                ERROR_INVALID_PARAMETER,
                "VirtualProtect unsupported protect",
            )
        })?;
        let page_base = align_down(addr, PAGE_SIZE);
        let end = addr
            .checked_add(
                u64::try_from(size).map_err(|_| {
                    va_error(ERROR_INVALID_PARAMETER, "VirtualProtect size overflow")
                })?,
            )
            .ok_or_else(|| va_error(ERROR_INVALID_PARAMETER, "VirtualProtect end overflow"))?;
        let page_end = align_up(end, PAGE_SIZE);
        let size_u64 = page_end.saturating_sub(page_base);
        let size_usize = usize::try_from(size_u64)
            .map_err(|_| va_error(ERROR_NOT_ENOUGH_MEMORY, "VirtualProtect size"))?;

        // Entire range must lie in one allocation and every page must be Committed.
        let node = self
            .vad
            .find(page_base)
            .ok_or_else(|| va_error(ERROR_INVALID_ADDRESS, "VirtualProtect outside allocation"))?;
        if !node.contains_range(page_base, size_u64) {
            return Err(va_error(
                ERROR_INVALID_ADDRESS,
                "VirtualProtect range crosses allocation",
            ));
        }
        let mut page = page_base >> 12;
        let last = page_end >> 12;
        let mut old_protect = protect::PageProtect::NoAccess;
        let mut first = true;
        while page < last {
            match self.pages.lookup(page) {
                Some(run) if run.state == PageState::Committed => {
                    if first {
                        old_protect = run.protect;
                        first = false;
                    }
                    let next = run.end_page.min(last);
                    if next <= page {
                        return Err(va_error(
                            ERROR_INVALID_ADDRESS,
                            "VirtualProtect corrupt pagemap",
                        ));
                    }
                    page = next;
                }
                Some(_) => {
                    return Err(va_error(
                        ERROR_INVALID_ADDRESS,
                        "VirtualProtect on non-committed page",
                    ));
                }
                None => {
                    return Err(va_error(
                        ERROR_INVALID_ADDRESS,
                        "VirtualProtect free page in range",
                    ));
                }
            }
        }

        self.pages
            .set_range(page_base, size_usize, PageState::Committed, new_protect)?;
        self.bump_generation();
        self.sync_host_protect(page_base, size_usize);
        Ok(old_protect.to_win32())
    }

    /// `VirtualQuery` — build a real `MEMORY_BASIC_INFORMATION` for `addr`.
    #[must_use]
    pub(crate) fn virtual_query(&self, addr: u64) -> MemoryBasicInformation {
        let page_va = align_down(addr, PAGE_SIZE);
        let page_key = page_va >> 12;

        // Free: not in PageMap.
        let Some(run) = self.pages.lookup(page_key) else {
            return self.query_free(page_va);
        };

        let Some(node) = self.vad.find(page_va) else {
            // PageMap entry without VAD (should not happen after bootstrap wiring).
            return self.query_free(page_va);
        };

        // Clip homogeneous run to allocation and to continuous same state/protect.
        let alloc_start_page = node.allocation_base >> 12;
        let alloc_end_page = node.end() >> 12;
        let mut run_start = run.start_page.max(alloc_start_page);
        let mut run_end = run.end_page.min(alloc_end_page);
        // Ensure query page is inside clipped run (lookup already guarantees).
        if page_key < run_start {
            run_start = page_key;
        }
        if page_key >= run_end {
            run_end = page_key.saturating_add(1);
        }

        // Extend left within allocation while same state+protect.
        while run_start > alloc_start_page {
            let prev = run_start.saturating_sub(1);
            match self.pages.lookup(prev) {
                Some(r)
                    if r.state == run.state
                        && (run.state != PageState::Committed || r.protect == run.protect) =>
                {
                    run_start = r.start_page.max(alloc_start_page);
                }
                _ => break,
            }
        }
        // Extend right.
        while run_end < alloc_end_page {
            match self.pages.lookup(run_end) {
                Some(r)
                    if r.state == run.state
                        && (run.state != PageState::Committed || r.protect == run.protect) =>
                {
                    run_end = r.end_page.min(alloc_end_page);
                }
                _ => break,
            }
        }

        let base_address = run_start.saturating_mul(PAGE_SIZE);
        let region_size = run_end.saturating_sub(run_start).saturating_mul(PAGE_SIZE);
        let (state, protect) = match run.state {
            PageState::Committed => (MEM_COMMIT, run.protect.to_win32()),
            PageState::Reserved => (MEM_RESERVE, 0),
            PageState::Free => (MEM_FREE, 0),
        };
        MemoryBasicInformation {
            base_address,
            allocation_base: node.allocation_base,
            allocation_protect: node.allocation_protect.to_win32(),
            region_size,
            state,
            protect,
            type_: node.mem_type.win32(),
        }
    }

    fn query_free(&self, page_va: u64) -> MemoryBasicInformation {
        // Free run: from this page to the next VAD or next PageMap entry.
        let page_key = page_va >> 12;
        let mut end_page = page_key.saturating_add(1);
        // Cap free-run report to allocation granularity steps for sanity, but
        // prefer next VAD base when present.
        let next_vad = self
            .vad
            .iter()
            .map(|n| n.allocation_base)
            .filter(|&b| b > page_va)
            .min();
        let next_run = self
            .pages
            .iter_runs()
            .map(|r| r.start_page.saturating_mul(PAGE_SIZE))
            .find(|&b| b > page_va);

        let end_va = match (next_vad, next_run) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => {
                // Unbounded free: report one allocation granularity worth.
                page_va.saturating_add(GUEST_ALLOC_GRANULARITY)
            }
        };
        let end_page_cap = end_va >> 12;
        if end_page_cap > end_page {
            end_page = end_page_cap;
        }
        let region_size = end_page
            .saturating_sub(page_key)
            .saturating_mul(PAGE_SIZE)
            .max(PAGE_SIZE);
        MemoryBasicInformation {
            base_address: page_va,
            allocation_base: 0,
            allocation_protect: 0,
            region_size,
            state: MEM_FREE,
            protect: 0,
            type_: 0,
        }
    }

    /// Optional dual protection: tighten host `mprotect` only for host-aligned
    /// frames where every guest 4 KiB page is committed with the same R/W needs.
    ///
    /// Frames are relative to each arena's guest base so host pointers stay
    /// host-page aligned (soft translate: `host + (va - guest_base)`).
    /// Correctness remains SPC; failures of `mprotect` are ignored.
    /// Disabled with `WIE_MPROTECT=0`.
    pub(super) fn sync_host_protect(&mut self, address: u64, size: usize) {
        if !host_mprotect_enabled() {
            return;
        }
        if size == 0 {
            return;
        }
        let host_ps = host_page_size();
        if host_ps == 0 {
            return;
        }
        let host_ps_u64 = u64::try_from(host_ps).unwrap_or(PAGE_SIZE);
        let end = address.saturating_add(u64::try_from(size).unwrap_or(0));
        let mut va = address;
        while va < end {
            let Some(arena_base) = self.backend.arena_guest_base_for_va(va) else {
                // No arena covering this VA: skip host mprotect.
                va = va.saturating_add(PAGE_SIZE);
                continue;
            };
            let off = va.saturating_sub(arena_base);
            let frame_off = align_down(off, host_ps_u64);
            let frame_guest = arena_base.saturating_add(frame_off);
            let prot = self.host_prot_for_frame(frame_guest, host_ps_u64);
            let _ = self
                .backend
                .mprotect_guest_range(frame_guest, host_ps, prot);
            let next = frame_guest.saturating_add(host_ps_u64);
            if next <= va {
                va = va.saturating_add(PAGE_SIZE);
            } else {
                va = next;
            }
        }
    }

    /// Host PROT flags for one host page frame covering `frame`..`frame+host_ps`.
    fn host_prot_for_frame(&self, frame: u64, host_ps: u64) -> i32 {
        // Default RW — safe under clinch.
        let mut need_r = false;
        let mut need_w = false;
        let mut any_committed = false;
        let mut uniform = true;
        let mut first_protect: Option<protect::PageProtect> = None;
        let mut page = frame;
        let end = frame.saturating_add(host_ps);
        while page < end {
            match self.pages.lookup(page >> 12) {
                Some(run) if run.state == PageState::Committed => {
                    any_committed = true;
                    if run.protect.allows_read() || run.protect.allows_execute() {
                        need_r = true;
                    }
                    if run.protect.allows_write() {
                        need_w = true;
                    }
                    match first_protect {
                        None => first_protect = Some(run.protect),
                        Some(p) if p != run.protect => uniform = false,
                        _ => {}
                    }
                    page = page.saturating_add(PAGE_SIZE);
                }
                Some(_) | None => {
                    // Reserved/free inside frame → keep host RW so SPC alone gates.
                    return HOST_PROT_READ | HOST_PROT_WRITE;
                }
            }
        }
        if !any_committed {
            return HOST_PROT_READ | HOST_PROT_WRITE;
        }
        if !uniform {
            // Mixed guest protects: host union of R/W needs (never RX host tricks).
            let mut p = 0;
            if need_r {
                p |= HOST_PROT_READ;
            }
            if need_w {
                p |= HOST_PROT_WRITE;
            }
            if p == 0 {
                // All NOACCESS-like: still leave host RW so we can re-protect later
                // without faulting the emulator; SPC denies guest.
                return HOST_PROT_READ | HOST_PROT_WRITE;
            }
            return p;
        }
        // Uniform: optional tighten.
        match first_protect {
            Some(p) if p.allows_write() => HOST_PROT_READ | HOST_PROT_WRITE,
            Some(p) if p.allows_read() || p.allows_execute() => HOST_PROT_READ,
            _ => HOST_PROT_READ | HOST_PROT_WRITE,
        }
    }

    /// `VirtualAlloc` — reserve and/or commit private pages.
    ///
    /// Returns the allocation base (reserve) or the committed region base.
    pub(crate) fn virtual_alloc(
        &mut self,
        addr: u64,
        size: usize,
        alloc_type: u32,
        protect: u32,
    ) -> Result<u64, crate::CpuError> {
        let mut do_reserve = (alloc_type & MEM_RESERVE) != 0;
        let do_commit = (alloc_type & MEM_COMMIT) != 0;
        // Windows / Wine: `VirtualAlloc(NULL, size, MEM_COMMIT, …)` without
        // MEM_RESERVE is treated as RESERVE|COMMIT (7za LZMA dictionaries).
        if do_commit && !do_reserve && addr == 0 {
            do_reserve = true;
        }
        if size == 0 || (!do_reserve && !do_commit) {
            return Err(va_error(
                ERROR_INVALID_PARAMETER,
                "VirtualAlloc size/type invalid",
            ));
        }
        // Reject unknown type bits beyond RESERVE|COMMIT.
        let known = MEM_RESERVE | MEM_COMMIT;
        if alloc_type & !known != 0 {
            return Err(va_error(
                ERROR_INVALID_PARAMETER,
                "VirtualAlloc unsupported allocation type flags",
            ));
        }
        // Parsing is the validation (see `virtual_protect`); the typed value
        // then flows to the reserve/commit helpers so they cannot be handed an
        // unvalidated or wrongly-encoded protection.
        let protect = protect::PageProtect::from_win32(protect)
            .ok_or_else(|| va_error(ERROR_INVALID_PARAMETER, "VirtualAlloc unsupported protect"))?;

        if do_reserve && do_commit {
            self.va_reserve_and_commit(addr, size, protect)
        } else if do_reserve {
            self.va_reserve_only(addr, size, protect)
        } else {
            self.va_commit_only(addr, size, protect)
        }
    }

    /// `VirtualFree` — decommit pages or release a whole allocation.
    pub(crate) fn virtual_free(
        &mut self,
        addr: u64,
        size: usize,
        free_type: u32,
    ) -> Result<(), crate::CpuError> {
        let decommit = (free_type & MEM_DECOMMIT) != 0;
        let release = (free_type & MEM_RELEASE) != 0;
        if decommit == release {
            // Exactly one of DECOMMIT or RELEASE.
            return Err(va_error(
                ERROR_INVALID_PARAMETER,
                "VirtualFree type must be DECOMMIT or RELEASE",
            ));
        }
        if release {
            if size != 0 {
                return Err(va_error(
                    ERROR_INVALID_PARAMETER,
                    "VirtualFree MEM_RELEASE requires size 0",
                ));
            }
            return self.va_release(addr);
        }
        self.va_decommit(addr, size)
    }

    fn va_reserve_only(
        &mut self,
        addr: u64,
        size: usize,
        protect: protect::PageProtect,
    ) -> Result<u64, crate::CpuError> {
        let (base, size_u64) = self.align_reserve_request(addr, size)?;
        self.ensure_pages_free(base, size_u64)?;
        // Host storage: RESERVE creates one demand-zero arena for the full span.
        let size_usize = usize::try_from(size_u64)
            .map_err(|_| va_error(ERROR_NOT_ENOUGH_MEMORY, "reserve size does not fit usize"))?;
        // Host RW; SPC uses Reserved so guest cannot touch until commit.
        self.backend
            .map(base, size_usize, crate::RwxPerms::ALL.bits())?;
        self.pages.set_range(
            base,
            size_usize,
            PageState::Reserved,
            protect::PageProtect::NoAccess,
        )?;
        self.vad.insert(VadNode {
            allocation_base: base,
            size: size_u64,
            allocation_protect: protect,
            mem_type: MemType::Private,
            owns_host: true,
        })?;
        self.bump_generation();
        Ok(base)
    }

    fn va_reserve_and_commit(
        &mut self,
        addr: u64,
        size: usize,
        protect: protect::PageProtect,
    ) -> Result<u64, crate::CpuError> {
        let (base, size_u64) = self.align_reserve_request(addr, size)?;
        self.ensure_pages_free(base, size_u64)?;
        let size_usize = usize::try_from(size_u64)
            .map_err(|_| va_error(ERROR_NOT_ENOUGH_MEMORY, "alloc size does not fit usize"))?;
        // Host storage for full span.
        self.backend
            .map(base, size_usize, protect.to_rwx().bits())?;
        self.pages
            .set_range(base, size_usize, PageState::Committed, protect)?;
        self.vad.insert(VadNode {
            allocation_base: base,
            size: size_u64,
            allocation_protect: protect,
            mem_type: MemType::Private,
            owns_host: true,
        })?;
        self.bump_generation();
        Ok(base)
    }

    fn va_commit_only(
        &mut self,
        addr: u64,
        size: usize,
        protect: protect::PageProtect,
    ) -> Result<u64, crate::CpuError> {
        if addr == 0 {
            // COMMIT with NULL address is not supported without RESERVE.
            return Err(va_error(
                ERROR_INVALID_PARAMETER,
                "VirtualAlloc COMMIT requires address or RESERVE",
            ));
        }
        let page_base = align_down(addr, PAGE_SIZE);
        let end = addr
            .checked_add(
                u64::try_from(size)
                    .map_err(|_| va_error(ERROR_INVALID_PARAMETER, "commit size overflow"))?,
            )
            .ok_or_else(|| va_error(ERROR_INVALID_PARAMETER, "commit end overflow"))?;
        let page_end = align_up(end, PAGE_SIZE);
        let size_u64 = page_end.saturating_sub(page_base);
        let size_usize = usize::try_from(size_u64)
            .map_err(|_| va_error(ERROR_NOT_ENOUGH_MEMORY, "commit size"))?;

        let node = self.vad.find(page_base).ok_or_else(|| {
            va_error(
                ERROR_INVALID_ADDRESS,
                "VirtualAlloc COMMIT without prior RESERVE",
            )
        })?;
        if !node.contains_range(page_base, size_u64) {
            return Err(va_error(
                ERROR_INVALID_ADDRESS,
                "VirtualAlloc COMMIT range outside allocation",
            ));
        }
        // All pages must be Reserved or already Committed under this allocation.
        let mut page = page_base >> 12;
        let last = page_end >> 12;
        while page < last {
            match self.pages.lookup(page) {
                Some(run)
                    if run.state == PageState::Reserved || run.state == PageState::Committed =>
                {
                    let next = run.end_page.min(last);
                    if next <= page {
                        return Err(va_error(
                            ERROR_INVALID_ADDRESS,
                            "VirtualAlloc COMMIT corrupt pagemap",
                        ));
                    }
                    page = next;
                }
                _ => {
                    return Err(va_error(
                        ERROR_INVALID_ADDRESS,
                        "VirtualAlloc COMMIT on free page",
                    ));
                }
            }
        }

        // Storage already present from RESERVE; only re-map if somehow missing.
        if self.backend.page_data_ptr_walk(page_base >> 12).is_none() {
            self.backend
                .map(page_base, size_usize, protect.to_rwx().bits())?;
        }
        self.pages
            .set_range(page_base, size_usize, PageState::Committed, protect)?;
        self.bump_generation();
        Ok(page_base)
    }

    fn va_decommit(&mut self, addr: u64, size: usize) -> Result<(), crate::CpuError> {
        if size == 0 {
            return Err(va_error(
                ERROR_INVALID_PARAMETER,
                "VirtualFree DECOMMIT size 0",
            ));
        }
        let page_base = align_down(addr, PAGE_SIZE);
        let end = addr
            .checked_add(
                u64::try_from(size)
                    .map_err(|_| va_error(ERROR_INVALID_PARAMETER, "decommit size"))?,
            )
            .ok_or_else(|| va_error(ERROR_INVALID_PARAMETER, "decommit overflow"))?;
        let page_end = align_up(end, PAGE_SIZE);
        let size_u64 = page_end.saturating_sub(page_base);
        let size_usize = usize::try_from(size_u64)
            .map_err(|_| va_error(ERROR_NOT_ENOUGH_MEMORY, "decommit size"))?;

        let node = self
            .vad
            .find(page_base)
            .ok_or_else(|| va_error(ERROR_INVALID_ADDRESS, "DECOMMIT outside allocation"))?;
        if !node.contains_range(page_base, size_u64) {
            return Err(va_error(
                ERROR_INVALID_ADDRESS,
                "DECOMMIT range crosses allocation",
            ));
        }
        // Transactional: every page must belong to this allocation (already checked)
        // and be Reserved or Committed (free is invalid).
        let mut page = page_base >> 12;
        let last = page_end >> 12;
        while page < last {
            match self.pages.lookup(page) {
                Some(run)
                    if run.state == PageState::Reserved || run.state == PageState::Committed =>
                {
                    let next = run.end_page.min(last);
                    if next <= page {
                        return Err(va_error(ERROR_INVALID_ADDRESS, "DECOMMIT corrupt pagemap"));
                    }
                    page = next;
                }
                _ => {
                    return Err(va_error(
                        ERROR_INVALID_ADDRESS,
                        "DECOMMIT free page in range",
                    ));
                }
            }
        }

        self.backend.discard_range(page_base, size_usize)?;
        self.pages.set_range(
            page_base,
            size_usize,
            PageState::Reserved,
            protect::PageProtect::NoAccess,
        )?;
        self.bump_generation();
        Ok(())
    }

    fn va_release(&mut self, addr: u64) -> Result<(), crate::CpuError> {
        let node =
            self.vad.find_base(addr).cloned().ok_or_else(|| {
                va_error(ERROR_INVALID_ADDRESS, "MEM_RELEASE not allocation base")
            })?;
        let size_usize = usize::try_from(node.size)
            .map_err(|_| va_error(ERROR_NOT_ENOUGH_MEMORY, "release size"))?;
        // Bump generation BEFORE unmap so concurrent readers see the change
        // and abort their generation-guarded access before the arena is freed.
        self.bump_generation();
        self.pages.set_range(
            node.allocation_base,
            size_usize,
            PageState::Free,
            protect::PageProtect::NoAccess,
        )?;
        let _ = self.vad.remove_base(addr);
        self.backend.unmap_range(node.allocation_base, size_usize);
        Ok(())
    }

    fn align_reserve_request(&self, addr: u64, size: usize) -> Result<(u64, u64), crate::CpuError> {
        let size_u64 = u64::try_from(size)
            .map_err(|_| va_error(ERROR_INVALID_PARAMETER, "size does not fit u64"))?;
        if addr == 0 {
            let rounded = align_up(size_u64, GUEST_ALLOC_GRANULARITY);
            if rounded == 0 {
                return Err(va_error(ERROR_INVALID_PARAMETER, "reserve size 0"));
            }
            let base = self
                .vad
                .find_free_region(rounded, &|page| self.pages.lookup(page).is_some())
                .ok_or_else(|| va_error(ERROR_NOT_ENOUGH_MEMORY, "no free guest VA for reserve"))?;
            return Ok((base, rounded));
        }
        let base = align_down(addr, GUEST_ALLOC_GRANULARITY);
        let end = addr
            .checked_add(size_u64)
            .ok_or_else(|| va_error(ERROR_INVALID_PARAMETER, "reserve end overflow"))?;
        let end_aligned = align_up(end, GUEST_ALLOC_GRANULARITY);
        let span = end_aligned.saturating_sub(base);
        if span == 0 {
            return Err(va_error(ERROR_INVALID_PARAMETER, "reserve span 0"));
        }
        Ok((base, span))
    }

    fn ensure_pages_free(&self, base: u64, size: u64) -> Result<(), crate::CpuError> {
        let end = base.saturating_add(size);
        let mut page = base >> 12;
        let last = end >> 12;
        while page < last {
            if let Some(run) = self.pages.lookup(page) {
                // Presence in the map means Reserved or Committed (Free is absent).
                let next = run.end_page.min(last);
                if next <= page {
                    return Err(va_error(
                        ERROR_INVALID_ADDRESS,
                        "reserve over non-free page",
                    ));
                }
                return Err(va_error(
                    ERROR_INVALID_ADDRESS,
                    "reserve over non-free page",
                ));
            }
            page = page.saturating_add(1);
        }
        if self.vad.overlaps(base, size) {
            return Err(va_error(ERROR_INVALID_ADDRESS, "reserve over existing VAD"));
        }
        Ok(())
    }
}
