//! GuestMemory byte I/O + JIT page-walk surface: `read` / `write` / `fetch_into`,
//! bulk `host_span` / `mem_copy` / `mem_fill`, and the TLB entry helpers
//! (`page_tlb_entry` / `page_data_ptr` / `page_protect_meta`).

use super::{
    GuestMemBackend, PAGE_SIZE, PAGE_SIZE_USIZE, PageProtectMeta, PageState, PageTlbEntry, backend,
    protect,
};
use crate::CpuError;

impl super::GuestMemory {
    /// Write `bytes` at guest `address` after SPC (write permission).
    ///
    /// Uses pointer-based write to mmap arena data plane — no backend mutation,
    /// so this method takes `&self` and is safe for concurrent caller threads.
    pub(crate) fn write(&self, address: u64, bytes: &[u8]) -> Result<(), crate::CpuError> {
        let gen_start = self.generation();
        self.pages
            .check_access(address, bytes.len(), protect::AccessKind::Write)?;
        // Lock-free pointer-based write: each page is resolved separately
        // since cross-page spans may cross arena boundaries.
        let mut offset = 0_usize;
        let mut va = address;
        while offset < bytes.len() {
            let page_off = usize::try_from(va & (PAGE_SIZE - 1))
                .map_err(|_| CpuError::Message("page offset does not fit usize".into()))?;
            let src = bytes
                .get(offset..)
                .ok_or_else(|| CpuError::Message("write slice OOB".into()))?;
            let room_in_page = PAGE_SIZE_USIZE.saturating_sub(page_off);
            let chunk = room_in_page.min(src.len());
            let dst = self
                .backend
                .write_ptr(va)
                .ok_or_else(|| CpuError::Message(format!("mem_write unmapped {va:#x}")))?;
            // SAFETY: write_ptr resolved a host pointer; generation check
            // BEFORE the write guards against concurrent arena removal
            // (VirtualFree/MEM_RELEASE unmaps the arena and bumps generation).
            if self.generation() != gen_start {
                return Err(CpuError::Message(format!(
                    "mem_write generation changed at {va:#x} (concurrent map/free)"
                )));
            }
            #[expect(unsafe_code)]
            unsafe {
                std::ptr::copy_nonoverlapping(src.as_ptr(), dst, chunk);
            }
            offset = offset.saturating_add(chunk);
            va = va.saturating_add(u64::try_from(chunk).unwrap_or(0));
        }
        Ok(())
    }

    /// Read into `bytes` from guest `address` after SPC (read permission).
    pub(crate) fn read(&self, address: u64, bytes: &mut [u8]) -> Result<(), crate::CpuError> {
        let gen_start = self.generation();
        self.pages
            .check_access(address, bytes.len(), protect::AccessKind::Read)?;
        // Generation check BEFORE read guards against concurrent arena removal
        // by another thread (VirtualFree/MEM_RELEASE bumps generation + unmap).
        if self.generation() != gen_start {
            return Err(CpuError::Message(format!(
                "mem_read generation changed at {address:#x} (concurrent map/free)"
            )));
        }
        self.backend.read(address, bytes)
    }

    /// Soft-translate a **contiguous** guest range to a host pointer for bulk copy.
    ///
    /// Phase 4.3: used by REP MOVS/STOS after SPC. Never returns guest VAs.
    ///
    /// Returns `None` when:
    /// - `len == 0` or the range wraps,
    /// - SPC denies the access for the whole span,
    /// - multi-page range is not entirely inside one mmap arena,
    /// - single-page host resolve fails (uncommitted / NOACCESS).
    ///
    /// On success, the returned pointer is valid for `len` bytes for the lifetime
    /// of this `GuestMemory` borrow (and until the covering arena is released).
    #[must_use]
    /// Copy `len` bytes guest→guest without bouncing through a host buffer.
    ///
    /// Uses `memmove` semantics, so overlapping ranges are well defined — which
    /// is what `memmove` needs and what `memcpy` callers get for free. Returns
    /// `false` when either side is not a single mapped span with the required
    /// permission, leaving the caller to fall back to read+write.
    pub(crate) fn mem_copy(&self, dst: u64, src: u64, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        let (Some(s), Some(d)) = (
            self.host_span(src, len, false),
            self.host_span(dst, len, true),
        ) else {
            return false;
        };
        // SAFETY: both spans validated for `len` bytes with the needed
        // permission; `copy` (memmove) is defined for overlapping ranges.
        #[expect(unsafe_code)]
        unsafe {
            std::ptr::copy(s, d, len);
        }
        true
    }

    /// Fill `len` guest bytes with `byte`. Returns `false` if not mappable.
    pub(crate) fn mem_fill(&self, address: u64, byte: u8, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        let Some(d) = self.host_span(address, len, true) else {
            return false;
        };
        // SAFETY: span validated writable for `len` bytes.
        #[expect(unsafe_code)]
        unsafe {
            std::ptr::write_bytes(d, byte, len);
        }
        true
    }

    pub(crate) fn host_span(&self, address: u64, len: usize, write: bool) -> Option<*mut u8> {
        if len == 0 {
            return None;
        }
        let gen_start = self.generation();
        let len_u = u64::try_from(len).ok()?;
        let end = address.checked_add(len_u)?;
        let kind = if write {
            protect::AccessKind::Write
        } else {
            protect::AccessKind::Read
        };
        self.pages.check_access(address, len, kind).ok()?;

        // Phase 4.x: never host-span *write* onto executable pages (SMC must
        // go through `write` + code-invalidate). Reads of RX code are fine.
        if write && self.range_allows_execute(address, len) {
            return None;
        }

        let page_off = usize::try_from(address & (backend::PAGE_SIZE - 1)).ok()?;
        // Single-page: resolve via page walk.
        if page_off.saturating_add(len) <= backend::PAGE_SIZE_USIZE {
            let entry = self.page_tlb_entry_walk(address >> backend::PAGE_SHIFT)?;
            if write && !entry.allow_w {
                return None;
            }
            if !write && !entry.allow_r {
                return None;
            }
            // Generation check: ensure arena is still alive before returning pointer.
            if self.generation() != gen_start {
                return None;
            }
            // SAFETY: host is a mapped page base; in-page offset + len checked.
            #[expect(unsafe_code)]
            return Some(unsafe { entry.host.add(page_off) });
        }

        // Multi-page: require one contiguous mmap arena covering [address, end).
        let guest_base = self.backend.arena_guest_base_for_va(address)?;
        let guest_base_last = self
            .backend
            .arena_guest_base_for_va(end.saturating_sub(1))?;
        if guest_base != guest_base_last {
            return None;
        }
        let host_base_u = self.backend.arena_host_base_for_va(address)?;
        if host_base_u == 0 {
            return None;
        }
        let off = address.checked_sub(guest_base)?;
        let off_usize = usize::try_from(off).ok()?;
        if self.generation() != gen_start {
            return None;
        }
        // SAFETY: SPC passed; start and last byte share one arena; soft translate.
        #[expect(unsafe_code, clippy::as_conversions)] // host base address → data pointer
        Some(unsafe { (host_base_u as *mut u8).add(off_usize) })
    }

    /// Instruction fetch into a small stack buffer after SPC (execute permission).
    pub(crate) fn fetch_into(
        &self,
        address: u64,
        out: &mut [u8],
    ) -> Result<usize, crate::CpuError> {
        let want = out.len().min(15);
        if want == 0 {
            return Ok(0);
        }
        // Fetch may shorten on the trailing edge of a mapping (same as backend
        // default), but never past a permission boundary: try full length first,
        // then shrink until a legal prefix is found.
        let gen_start = self.generation();
        let mut len = want;
        while len > 0 {
            if self
                .pages
                .check_access(address, len, protect::AccessKind::Execute)
                .is_ok()
            {
                let Some(dst) = out.get_mut(..len) else {
                    break;
                };
                if self.generation() != gen_start {
                    return Err(crate::CpuError::Message(format!(
                        "instruction fetch generation changed at {address:#x} (concurrent map/free)"
                    )));
                }
                return self.backend.fetch_into(address, dst);
            }
            len = len.saturating_sub(1);
        }
        Err(crate::CpuError::Message(format!(
            "instruction fetch unmapped {address:#x}"
        )))
    }

    /// Host pointer to a mapped page's data (JIT TLB).
    ///
    /// Returns `None` if the page is not committed (JIT must not install a TLB
    /// entry for free/reserved pages). `PAGE_NOACCESS` also yields `None`.
    ///
    /// Prefer [`Self::page_tlb_entry`] for JIT installs — it also returns R/W
    /// capability bits so the fast path can enforce SPC without a helper call.
    #[must_use]
    #[cfg_attr(not(test), expect(dead_code))]
    pub(crate) fn page_data_ptr(&self, page_key: u64) -> Option<*mut u8> {
        self.page_tlb_entry(page_key).map(|e| e.host)
    }

    /// Fast page-table walk (arena soft-translate formula).
    ///
    /// Committed-only gate (same as [`Self::page_tlb_entry`] host resolution).
    /// Callers that install a TLB must still honour protect via [`PageTlbEntry`].
    #[must_use]
    #[cfg_attr(not(test), expect(dead_code))]
    pub(crate) fn page_data_ptr_walk(&self, page_key: u64) -> Option<*mut u8> {
        self.page_tlb_entry_walk(page_key).map(|e| e.host)
    }

    /// Resolve a committed page for JIT TLB install: host pointer + R/W flags + gen.
    #[must_use]
    pub(crate) fn page_tlb_entry(&self, page_key: u64) -> Option<PageTlbEntry> {
        let meta = self.page_protect_meta(page_key)?;
        let host = self.backend.page_data_ptr(page_key)?;
        Some(PageTlbEntry {
            host,
            allow_r: meta.allow_r,
            allow_w: meta.allow_w,
            generation: self.generation(),
        })
    }

    /// Read-only walk variant of [`Self::page_tlb_entry`].
    #[must_use]
    pub(crate) fn page_tlb_entry_walk(&self, page_key: u64) -> Option<PageTlbEntry> {
        self.page_tlb_entry(page_key)
    }

    fn page_protect_meta(&self, page_key: u64) -> Option<PageProtectMeta> {
        let run = self.pages.lookup(page_key)?;
        if run.state != PageState::Committed {
            return None;
        }
        let allow_r = run.protect.allows_read();
        let allow_x = run.protect.allows_execute();
        // Phase 4.x: W soft-translate is denied on executable pages so stores
        // cannot silently SMC under sticky/pin/TLB without `GuestMemory::write`.
        let allow_w = run.protect.allows_write() && !allow_x;
        // NOACCESS / no usable rights → no TLB entry.
        // Keep RX pages installable for data reads (allow_r).
        if !allow_r && !allow_w && !allow_x {
            return None;
        }
        Some(PageProtectMeta { allow_r, allow_w })
    }

    /// True if any committed page in `[address, address+len)` allows execute.
    #[must_use]
    fn range_allows_execute(&self, address: u64, len: usize) -> bool {
        if len == 0 {
            return false;
        }
        let Some(end) = address.checked_add(u64::try_from(len).unwrap_or(u64::MAX)) else {
            return true;
        };
        let mut page = address >> backend::PAGE_SHIFT;
        let last = end.saturating_sub(1) >> backend::PAGE_SHIFT;
        while page <= last {
            if let Some(run) = self.pages.lookup(page) {
                if run.state == PageState::Committed && run.protect.allows_execute() {
                    return true;
                }
                let next = run.end_page;
                if next <= page {
                    page = page.saturating_add(1);
                } else {
                    page = next;
                }
            } else {
                page = page.saturating_add(1);
            }
        }
        false
    }
}
