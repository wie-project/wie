//! GuestMemory mapping + region + JIT pin operations: `map` / `map_image`,
//! the region registry (`register_region` / `find_region`), and the Phase 4.1
//! soft-translate pins (`span_pin` / `region_pin` / `jit_region_pins`).

use super::{
    GuestMemBackend, GuestRegion, JIT_REGION_PIN_SLOTS, MemType, PageState, RegionKind,
    RegionPinInfo, VadNode, backend, protect,
};

impl super::GuestMemory {
    /// Register a named layout range; fill `host_base` when an arena covers it.
    pub(crate) fn register_region(&mut self, mut region: GuestRegion) {
        if region.host_base.is_none()
            && let Some(hb) = self.backend.arena_host_base_for_va(region.base)
        {
            region.host_base = Some(hb);
        }
        self.regions.register(region);
    }

    /// Find the named region containing `va`.
    #[must_use]
    pub(crate) fn find_region(&self, va: u64) -> Option<&GuestRegion> {
        self.regions.find(va)
    }

    /// Build a Phase 4.1 region pin for JIT: soft-translated host base + bounds +
    /// **conservative** software R/W (intersection over every committed page).
    ///
    /// Returns `None` when:
    /// - the region has no `host_base` (not arena-backed),
    /// - any page in the range is free/reserved or missing from the page map,
    /// - no usable R/W rights remain after intersection.
    ///
    /// Pins are **not** an oracle: protect mixed ranges simply disable W (or the
    /// whole pin). Slow path [`Self::read`]/[`Self::write`] remains authoritative.
    #[must_use]
    pub(crate) fn region_pin(&self, region: &GuestRegion) -> Option<RegionPinInfo> {
        self.span_pin(region.base, region.end())
    }

    /// Pin an arbitrary guest VA span that lives in one mmap arena.
    ///
    /// Soft-translate base is the host pointer of `guest_base` (not necessarily
    /// the arena start), so sub-ranges of a VirtualAlloc reservation pin correctly.
    #[must_use]
    pub(crate) fn span_pin(&self, guest_base: u64, guest_end: u64) -> Option<RegionPinInfo> {
        if guest_end <= guest_base {
            return None;
        }
        // One arena must cover first and last byte (contiguous soft translate).
        let arena_base = self.backend.arena_guest_base_for_va(guest_base)?;
        let arena_base_last = self
            .backend
            .arena_guest_base_for_va(guest_end.saturating_sub(1))?;
        if arena_base != arena_base_last {
            return None;
        }
        let host_arena_u = self.backend.arena_host_base_for_va(guest_base)?;
        if host_arena_u == 0 {
            return None;
        }
        let off = guest_base.checked_sub(arena_base)?;
        let host_u = host_arena_u.checked_add(off)?;
        // SAFETY: host is arena soft-translate of guest_base; pin is non-owning
        // and invalidated via generation / invalidate_tlb on unmap.
        #[allow(clippy::as_conversions)] // required: u64 → non-owning *mut u8 (int-to-ptr)
        let host_base = host_u as *mut u8;
        if host_base.is_null() {
            return None;
        }

        let first_page = guest_base >> backend::PAGE_SHIFT;
        let last_page = guest_end.saturating_sub(1) >> backend::PAGE_SHIFT;
        let mut page = first_page;
        let mut allow_r = true;
        let mut allow_w = true;
        let mut saw = false;
        while page <= last_page {
            let run = self.pages.lookup(page)?;
            if run.state != PageState::Committed {
                return None;
            }
            // Gap inside the span (lookup jumped past a free hole).
            if page < run.start_page || page >= run.end_page {
                return None;
            }
            allow_r &= run.protect.allows_read();
            // Phase 4.x: never soft-translate writes onto executable pages so
            // SMC always hits `GuestMemory::write` + code-invalidate drain.
            allow_w &= run.protect.allows_write() && !run.protect.allows_execute();
            saw = true;
            let next = run.end_page;
            if next <= page {
                return None;
            }
            page = next;
        }
        if !saw || (!allow_r && !allow_w) {
            return None;
        }
        Some(RegionPinInfo {
            guest_base,
            guest_end,
            host_base,
            allow_r,
            allow_w,
            generation: self.generation(),
        })
    }

    /// Collect maximal committed homogeneous-protect runs inside `[base, end)`.
    fn committed_runs_in_span(&self, base: u64, end: u64) -> Vec<(u64, u64)> {
        if end <= base {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut page = base >> backend::PAGE_SHIFT;
        let last = end.saturating_sub(1) >> backend::PAGE_SHIFT;
        while page <= last {
            let Some(run) = self.pages.lookup(page) else {
                page = page.saturating_add(1);
                continue;
            };
            if run.state != PageState::Committed {
                page = run.end_page.max(page.saturating_add(1));
                continue;
            }
            // Clip run to the requested span.
            let start_p = run.start_page.max(page).max(base >> backend::PAGE_SHIFT);
            let end_p = run.end_page.min(last.saturating_add(1));
            if end_p > start_p {
                let g0 = start_p.saturating_mul(backend::PAGE_SIZE).max(base);
                let g1 = end_p.saturating_mul(backend::PAGE_SIZE).min(end);
                if g1 > g0 {
                    out.push((g0, g1));
                }
            }
            let next = run.end_page;
            if next <= page {
                page = page.saturating_add(1);
            } else {
                page = next;
            }
        }
        // Merge adjacent (same-protect runs may already be separate PageRuns).
        out.sort_by_key(|&(a, _)| a);
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for (a, b) in out {
            if let Some(last) = merged.last_mut()
                && last.1 == a
            {
                last.1 = b;
            } else {
                merged.push((a, b));
            }
        }
        merged
    }

    /// Whether `[lo, hi)` overlaps any already-chosen pin span.
    fn pin_overlaps(chosen: &[(u64, u64)], lo: u64, hi: u64) -> bool {
        chosen.iter().any(|&(a, b)| lo < b && hi > a)
    }

    /// True when a named layout region fully covers `[lo, hi)` and is **not** a
    /// heap (bootstrap `guest_file_data` / image / TEB would otherwise win size
    /// ranking over hot VirtualAlloc LZMA dicts).
    fn covered_by_bootstrap_named(&self, lo: u64, hi: u64) -> bool {
        self.regions.iter().any(|r| {
            if matches!(r.kind, RegionKind::Heap | RegionKind::Stack) {
                return false;
            }
            r.base <= lo && r.end() >= hi
        })
    }

    /// Pin slots for JIT: stack, then hottest data spans (heaps + VirtualAlloc).
    ///
    /// Empty slots are `None`. Callers copy into `JitCtx` at `run_compiled`.
    /// Helper `pin_resolve` always sees every filled slot. Cranelift IR probes a
    /// capped prefix of data slots (see `IR_DATA_PIN_SLOTS` in lower.rs), so
    /// **largest live RW heaps/VA must fill early slots**.
    #[must_use]
    pub(crate) fn jit_region_pins(&self) -> [Option<RegionPinInfo>; JIT_REGION_PIN_SLOTS] {
        let mut out = [None; JIT_REGION_PIN_SLOTS];
        let mut chosen_spans: Vec<(u64, u64)> = Vec::new();

        // Slot 0: stack (super-path + hot locals).
        if let Some(pin) = self
            .regions
            .find_by_kind(RegionKind::Stack)
            .and_then(|r| self.region_pin(r))
        {
            chosen_spans.push((pin.guest_base, pin.guest_end));
            out[0] = Some(pin);
        }

        // Data candidates: all named heaps + private VAD runs that are not
        // bootstrap layout (file mirror, image, TEB, …). Size-rank RW first so
        // IR's data pin hits LZMA VirtualAlloc rather than 64 MiB file arena.
        let mut candidates: Vec<(u64, RegionPinInfo)> = Vec::new();
        for r in self.regions.iter() {
            if r.kind != RegionKind::Heap {
                continue;
            }
            let Some(pin) = self.region_pin(r) else {
                continue;
            };
            let size = pin.guest_end.saturating_sub(pin.guest_base);
            // RO spans score at 1/4 (bit-shift, not div) so RW VirtualAlloc wins.
            let score = if pin.allow_w { size } else { size >> 2 };
            candidates.push((score, pin));
        }
        for node in self.vad.iter() {
            if node.mem_type != MemType::Private {
                continue;
            }
            let base = node.allocation_base;
            let end = node.end();
            for (g0, g1) in self.committed_runs_in_span(base, end) {
                if Self::pin_overlaps(&chosen_spans, g0, g1) {
                    continue;
                }
                if self.covered_by_bootstrap_named(g0, g1) {
                    continue;
                }
                if candidates
                    .iter()
                    .any(|(_, p)| p.guest_base == g0 && p.guest_end == g1)
                {
                    continue;
                }
                let Some(pin) = self.span_pin(g0, g1) else {
                    continue;
                };
                let size = pin.guest_end.saturating_sub(pin.guest_base);
                if size < backend::PAGE_SIZE {
                    continue;
                }
                let score = if pin.allow_w { size } else { size >> 2 };
                candidates.push((score, pin));
            }
        }
        candidates.sort_by_key(|b| std::cmp::Reverse(b.0));

        let mut slot = 1_usize;
        for (_, pin) in candidates {
            if slot >= JIT_REGION_PIN_SLOTS {
                break;
            }
            if Self::pin_overlaps(&chosen_spans, pin.guest_base, pin.guest_end) {
                continue;
            }
            chosen_spans.push((pin.guest_base, pin.guest_end));
            if let Some(slot_ref) = out.get_mut(slot) {
                *slot_ref = Some(pin);
            }
            slot = slot.saturating_add(1);
        }
        out
    }

    /// Map `[address, address+size)` with Unicorn-style `perms` (r/w/x).
    ///
    /// Creates host storage, marks pages **Committed**, and registers a private
    /// VAD node so free-VA search and VirtualQuery see bootstrap layout.
    pub(crate) fn map(
        &mut self,
        address: u64,
        size: usize,
        perms: crate::RwxPerms,
    ) -> Result<(), crate::CpuError> {
        self.map_with_type(address, size, perms, MemType::Private)
    }

    /// Like [`Self::map`], but register the VAD as a PE image (`MEM_IMAGE`).
    pub(crate) fn map_image(
        &mut self,
        address: u64,
        size: usize,
        perms: crate::RwxPerms,
    ) -> Result<(), crate::CpuError> {
        self.map_with_type(address, size, perms, MemType::Image)
    }

    fn map_with_type(
        &mut self,
        address: u64,
        size: usize,
        perms: crate::RwxPerms,
        mem_type: MemType,
    ) -> Result<(), crate::CpuError> {
        // The arena/mmap layer below still speaks raw rwx bits.
        self.backend.map(address, size, perms.bits())?;
        let protect = protect::PageProtect::from_rwx(perms);
        self.pages
            .set_range(address, size, PageState::Committed, protect)?;
        let size_u64 = u64::try_from(size).map_err(|_| {
            crate::CpuError::Message(format!("mem_map size {size} does not fit u64"))
        })?;
        // Bootstrap maps may overlap an existing VAD only on rematch of the same
        // base (idempotent map). Skip insert if already covered by same base.
        if self.vad.find_base(address).is_none() && !self.vad.overlaps(address, size_u64) {
            self.vad.insert(VadNode {
                allocation_base: address,
                size: size_u64,
                allocation_protect: protect,
                mem_type,
                owns_host: true,
            })?;
        }
        self.bump_generation();
        // Backfill host_base for regions already registered that this map covers.
        if let Some(hb) = self.backend.arena_host_base_for_va(address) {
            self.regions.set_host_base_if_covers(address, hb);
        }
        // Optional host mprotect for uniform host frames (defense-in-depth).
        self.sync_host_protect(address, size);
        Ok(())
    }
}
