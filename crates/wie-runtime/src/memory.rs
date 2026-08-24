//! Guest memory layout and WinAPI environment bootstrap helpers.

use ahash::HashMapExt;
use anyhow::{Context, Result, bail};
use wie_cpu::{RegionKind, RwxPerms};

/// One named guest-VA region of the fixed runtime layout.
///
/// The layout is the single source of truth for guest VA geometry: session
/// init registers every region (plus the runtime-known PE image) into the CPU
/// region table, and [`RuntimeMemoryLayout::validate`] checks the whole set
/// for overlap, overflow, and page alignment at compile time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutRegion {
    /// Stable name (`"stack"`, `"process_heap"`, …).
    pub name: &'static str,
    /// Semantic kind (region-table bookkeeping).
    pub kind: RegionKind,
    /// Inclusive start VA (page-aligned).
    pub base: u64,
    /// Size in bytes (page-aligned).
    pub size: usize,
    /// Protection bits.
    pub perms: RwxPerms,
}

impl LayoutRegion {
    /// Exclusive end VA (`base + size`; the layout is validated not to overflow).
    #[must_use]
    pub const fn end(&self) -> u64 {
        self.base + layout_size(self.size)
    }

    /// Whether `va` lies inside this region (`[base, end)`).
    #[must_use]
    pub const fn contains(&self, va: u64) -> bool {
        va >= self.base && va < self.end()
    }

    /// Whether this region shares any byte with `other`.
    #[must_use]
    pub const fn overlaps(&self, other: &Self) -> bool {
        self.base < other.end() && other.base < self.end()
    }
}

/// `usize` → `u64` in const fns (`u64::try_from` is not const-callable yet).
/// Region sizes are far below `u64::MAX`; the compile-time layout check pins them.
#[allow(clippy::as_conversions)] // const fn: try_from not const-stable
const fn layout_size(size: usize) -> u64 {
    size as u64
}

/// Guest virtual-memory layout used by the WIE runtime.
///
/// Centralizes hardcoded address ranges so they can be inspected, documented,
/// and validated in one place. Regions are named, typed, and permission-carrying
/// [`LayoutRegion`]s; the fixed set is checked pairwise (overlap, overflow,
/// page alignment) at compile time by the `const _: ()` gate below and again at
/// session start against the loaded PE image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeMemoryLayout {
    // ── tuning scalars (not regions) ─────────────────────────────────────
    /// Maximum guest instructions to execute between fake API hooks.
    pub instruction_budget: usize,
    /// Maximum number of consecutive no-hook instruction slices before stopping.
    pub no_hook_slice_limit: usize,
    // ── fake process heap ────────────────────────────────────────────────
    /// Fake process heap handle returned by `GetProcessHeap`.
    pub process_heap_handle: u64,
    /// The guest heap arena (base + size).
    pub process_heap: LayoutRegion,
    /// Offset from the heap base to its shadow heap (tolerates high 32-bit
    /// tagged heap-metadata accesses). Shadow base/size derive from this.
    pub process_heap_shadow_delta: u64,
    // ── hook window ──────────────────────────────────────────────────────
    /// Fake-API stop window (dense kind|payload encoding; see wie_winapi::fake_va).
    pub fake_api: LayoutRegion,
    /// Return trampoline for guest window procedures; must lie inside `fake_api`.
    pub callback_return_trampoline_va: u64,
    /// Shared `ret` stub for void synchronization APIs.
    pub fast_api_stub: LayoutRegion,
    // ── guest helper code (RWX, outside the hook window) ─────────────────
    /// Accelerated file I/O helpers.
    pub guest_io_code: LayoutRegion,
    /// HeapAlloc/HeapFree helpers.
    pub guest_heap_code: LayoutRegion,
    /// MultiByteToWideChar helper.
    pub guest_mbwc_code: LayoutRegion,
    // ── guest-visible data tables (RW) ───────────────────────────────────
    /// Open-file handle table.
    pub guest_io_table: LayoutRegion,
    /// File-content mirror arena (CreateFile → guest VA).
    pub guest_file_data: LayoutRegion,
    /// FLS value table (`u64[GUEST_FLS_SLOT_COUNT]`).
    pub guest_fls_table: LayoutRegion,
    /// Heap control block (bump + freelist heads).
    pub guest_heap_ctrl: LayoutRegion,
    /// Data-backed stubs (metrics, colors, cwd) + planted dialog stub bodies.
    pub guest_stub_data: LayoutRegion,
    /// Host-refreshed clock table (6 × u64).
    pub clock_table: LayoutRegion,
    // ── per-process regions ──────────────────────────────────────────────
    /// Guest stack.
    pub stack: LayoutRegion,
    /// Fake low TEB/TIB page for initial GS-relative CRT reads.
    pub teb_low: LayoutRegion,
    /// Pool of distinct per-thread TEB pages for guest workers (`CreateThread`).
    ///
    /// Each page is one `PerThreadTeb` (see `wie_cpu::teb`); a worker's engine
    /// is bound to its page via `CpuEngine::set_gs_base`. The primary thread
    /// keeps `teb_low` (the fixed `GS_BASE` page); this region backs every
    /// worker so their GS-relative state never aliases the primary's.
    pub worker_tebs: LayoutRegion,
    /// Environment string / module path data page.
    pub env_data: LayoutRegion,
    /// Fake resource data blob.
    pub resource_data: LayoutRegion,
}

impl RuntimeMemoryLayout {
    /// Default layout for PE64 sessions (addresses avoid common ImageBase values).
    #[must_use]
    pub const fn default() -> Self {
        // 4 MiB window: dense kind|payload encoding (see wie_winapi::fake_va).
        let fake_api = LayoutRegion {
            name: "fake_api",
            kind: RegionKind::FakeApi,
            base: wie_winapi::FAKE_API_BASE,
            size: wie_winapi::FAKE_API_SIZE,
            perms: RwxPerms::ALL,
        };
        // Must not collide with common PE ImageBase values (0x400000 and
        // modern 0x140000000). Formerly 0x140000000 — broke micro-PEs on Unicorn.
        let process_heap = LayoutRegion {
            name: "process_heap",
            kind: RegionKind::Heap,
            base: 0x0000_0001_6000_0000,
            // 512 MiB: 16 MiB exhausted while 7za scanned ~60k-file trees (malloc→0
            // → CRT `_CxxThrowException` → Int3). mmap is demand-zero; RSS grows on use.
            // Override with `WIE_PROCESS_HEAP_MB`. Room before shadow at base+1GiB.
            size: 0x2000_0000,
            perms: RwxPerms::READ_WRITE,
        };
        // 16-byte stride inside the fake-API window; code pages stay RWX.
        let fast_api_stub = LayoutRegion {
            name: "fast_api_stub",
            kind: RegionKind::GuestCode,
            base: 0x0000_7000_0040_0000,
            size: 0x1000,
            perms: RwxPerms::ALL,
        };
        Self {
            instruction_budget: 20_000_000,
            no_hook_slice_limit: 40,
            process_heap_handle: 0x0000_0000_5000_0000,
            process_heap,
            process_heap_shadow_delta: 0x0000_0001_0000_0000,
            fake_api,
            callback_return_trampoline_va: wie_winapi::callback_return_trampoline_va(),
            fast_api_stub,
            guest_io_code: LayoutRegion {
                name: "guest_io_code",
                kind: RegionKind::GuestCode,
                base: 0x0000_7000_0040_1000,
                size: 0x2000,
                perms: RwxPerms::ALL,
            },
            guest_heap_code: LayoutRegion {
                name: "guest_heap_code",
                kind: RegionKind::GuestCode,
                base: 0x0000_7000_0040_7000,
                size: 0x1000,
                perms: RwxPerms::ALL,
            },
            guest_mbwc_code: LayoutRegion {
                name: "guest_mbwc_code",
                kind: RegionKind::GuestCode,
                base: 0x0000_7000_0040_8000,
                size: 0x1000,
                perms: RwxPerms::ALL,
            },
            guest_io_table: LayoutRegion {
                name: "guest_io_table",
                kind: RegionKind::GuestIo,
                base: 0x0000_7000_0040_3000,
                size: 0x2000,
                perms: RwxPerms::READ_WRITE,
            },
            // Large arena after process heap for file content mirrors.
            guest_file_data: LayoutRegion {
                name: "guest_file_data",
                kind: RegionKind::GuestIo,
                base: 0x0000_0001_5000_0000,
                size: 0x0400_0000,
                perms: RwxPerms::READ_WRITE,
            },
            guest_fls_table: LayoutRegion {
                name: "guest_fls",
                kind: RegionKind::Other,
                base: 0x0000_7000_0040_5000,
                size: 0x1000,
                perms: RwxPerms::READ_WRITE,
            },
            guest_heap_ctrl: LayoutRegion {
                name: "guest_heap_ctrl",
                kind: RegionKind::Heap,
                base: 0x0000_7000_0040_6000,
                size: 0x1000,
                perms: RwxPerms::READ_WRITE,
            },
            // Metrics[256×u32] + colors[32×u32] + cwd wide blob + planted dialog
            // stub bodies (executable, hence RWX like the code regions above).
            guest_stub_data: LayoutRegion {
                name: "guest_stub_data",
                kind: RegionKind::Other,
                base: 0x0000_7000_0040_9000,
                size: 0x2000,
                perms: RwxPerms::ALL,
            },
            // Host-refreshed 6×u64 clock table (GetTickCount/timeGetTime/QPC/…).
            clock_table: LayoutRegion {
                name: "clock_table",
                kind: RegionKind::Other,
                base: 0x0000_7000_0040_B000,
                size: 0x1000,
                perms: RwxPerms::READ_WRITE,
            },
            stack: LayoutRegion {
                name: "stack",
                kind: RegionKind::Stack,
                base: 0x0000_0000_2000_0000,
                size: 0x0080_0000,
                perms: RwxPerms::READ_WRITE,
            },
            teb_low: LayoutRegion {
                name: "teb",
                kind: RegionKind::Teb,
                base: wie_cpu::GS_BASE,
                size: 0x1000,
                perms: RwxPerms::READ_WRITE,
            },
            // 64 pages — one per concurrent worker, matching the 64-thread cap
            // (`DEFAULT_MT_MAX_THREADS`). Exited workers return their page to
            // the pool (free list), so sequential spawn/join cycles reuse pages.
            worker_tebs: LayoutRegion {
                name: "worker_tebs",
                kind: RegionKind::Teb,
                base: 0x0000_7000_0040_C000,
                size: 0x0001_0000,
                perms: RwxPerms::READ_WRITE,
            },
            env_data: LayoutRegion {
                name: "env",
                kind: RegionKind::Env,
                base: 0x0000_0000_3000_0000,
                size: 0x1000,
                perms: RwxPerms::READ_WRITE,
            },
            resource_data: LayoutRegion {
                name: "resource",
                kind: RegionKind::Resource,
                base: 0x0000_0000_6400_0000,
                size: 0x0001_0000,
                perms: RwxPerms::READ_WRITE,
            },
        }
    }

    /// Shadow heap base used to tolerate high 32-bit tagged heap metadata accesses.
    #[must_use]
    pub const fn process_heap_shadow_base(self) -> u64 {
        self.process_heap.base + self.process_heap_shadow_delta
    }

    /// The shadow heap region (same size as the heap, at base + delta).
    #[must_use]
    pub const fn process_heap_shadow(&self) -> LayoutRegion {
        LayoutRegion {
            name: "process_heap_shadow",
            kind: RegionKind::Heap,
            base: self.process_heap_shadow_base(),
            size: self.process_heap.size,
            perms: self.process_heap.perms,
        }
    }

    /// Shared `ret` stub for void synchronization APIs.
    #[must_use]
    pub const fn fast_void_return_stub_va(self) -> u64 {
        self.fast_api_stub.base
    }

    /// The fixed guest regions, including the derived shadow heap.
    ///
    /// This is the set session init registers into the CPU region table and
    /// [`Self::validate`] checks pairwise. The PE image is runtime-known and
    /// added by [`Self::validate_with_image`].
    #[must_use]
    pub const fn regions(&self) -> [LayoutRegion; 18] {
        [
            self.fake_api,
            self.fast_api_stub,
            self.guest_io_code,
            self.guest_heap_code,
            self.guest_mbwc_code,
            self.guest_io_table,
            self.guest_file_data,
            self.guest_fls_table,
            self.guest_heap_ctrl,
            self.guest_stub_data,
            self.clock_table,
            self.stack,
            self.teb_low,
            self.worker_tebs,
            self.env_data,
            self.resource_data,
            self.process_heap,
            self.process_heap_shadow(),
        ]
    }

    /// First problem in the fixed layout, or `None` when clean.
    ///
    /// Checks, in order:
    /// 1. every region base and size is page-aligned;
    /// 2. no region overflows `u64` (`base + size`);
    /// 3. no two regions overlap;
    /// 4. the callback-return trampoline lies inside the fake-API window;
    /// 5. the shadow heap does not overflow and sits at `heap_base + delta`.
    ///
    /// Returns the name of the first offending region. Called at compile time
    /// on [`DEFAULT_LAYOUT`] (the `const _: ()` gate below) and at session
    /// start on the env-overridden layout.
    #[must_use]
    pub const fn validate(&self) -> Option<&'static str> {
        let Some(_) = self
            .process_heap
            .base
            .checked_add(self.process_heap_shadow_delta)
        else {
            return Some("process_heap_shadow");
        };
        if !self.fake_api.contains(self.callback_return_trampoline_va) {
            return Some("callback_return_trampoline_va");
        }
        let regions = self.regions();
        check_overlaps(&regions)
    }

    /// Validate the layout (including env overrides) plus the loaded PE image
    /// range. Called once at session start, before any region is mapped.
    ///
    /// # Errors
    /// When the fixed layout is invalid, or a fixed region overlaps
    /// `[image_base, image_base + image_size)`.
    pub fn validate_with_image(&self, image_base: u64, image_size: usize) -> Result<()> {
        if let Some(name) = self.validate() {
            bail!(
                "guest memory layout: region {name} is misaligned, overflows, or overlaps another region"
            );
        }
        let image_size = u64::try_from(image_size).context("image size does not fit u64")?;
        let Some(image_end) = image_base.checked_add(image_size) else {
            bail!("guest memory layout: PE image range overflows u64");
        };
        for region in self.regions() {
            if region.base < image_end && image_base < region.end() {
                bail!(
                    "guest memory layout: region {} overlaps the PE image range",
                    region.name
                );
            }
        }
        Ok(())
    }

    /// Apply environment overrides (`WIE_PROCESS_HEAP_MB`).
    ///
    /// Heap size is fixed at session start (contiguous mmap arena). Raise it for
    /// large guest workloads (directory scans that pin many CRT allocations).
    /// The caller re-validates the overridden layout via [`Self::validate`].
    #[must_use]
    pub fn with_env_overrides(mut self) -> Self {
        if let Ok(raw) = std::env::var("WIE_PROCESS_HEAP_MB") {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                match trimmed.parse::<u64>() {
                    Ok(mb) if mb > 0 => {
                        // Cap at 16 GiB — enough for heavy tools, avoids absurd maps.
                        let mb = mb.min(16 * 1024);
                        let bytes = usize::try_from(mb.saturating_mul(1024 * 1024))
                            .unwrap_or(self.process_heap.size);
                        // Keep at least 1 MiB so freelist math stays sane.
                        self.process_heap.size = bytes.max(1024 * 1024);
                    }
                    Ok(_) => {
                        tracing::warn!(
                            value = %raw,
                            "WIE_PROCESS_HEAP_MB must be > 0; keeping default process heap size"
                        );
                    }
                    Err(_) => {
                        tracing::warn!(
                            value = %raw,
                            "invalid WIE_PROCESS_HEAP_MB; keeping default process heap size"
                        );
                    }
                }
            }
        }
        if let Ok(v) = std::env::var("WIE_NO_HOOK_SLICES") {
            match v.trim().parse::<usize>() {
                Ok(n) if n > 0 => self.no_hook_slice_limit = n,
                _ => tracing::warn!(
                    value = %v,
                    "invalid WIE_NO_HOOK_SLICES; keeping default no-hook slice limit"
                ),
            }
        }
        self
    }
}

/// Pairwise overlap check over a region list. `None` when every pair is
/// disjoint and every region end fits in `u64`.
const fn check_overlaps(regions: &[LayoutRegion]) -> Option<&'static str> {
    let Some((head, tail)) = regions.split_first() else {
        return None;
    };
    // Page alignment + overflow on the head.
    if head.base & 0xFFF != 0 || layout_size(head.size) & 0xFFF != 0 {
        return Some(head.name);
    }
    let Some(head_end) = head.base.checked_add(layout_size(head.size)) else {
        return Some(head.name);
    };
    // Head vs every tail element.
    let mut rest = tail;
    while let Some((other, remaining)) = rest.split_first() {
        if head.base < other.end() && other.base < head_end {
            return Some(head.name);
        }
        rest = remaining;
    }
    check_overlaps(tail)
}

/// Compile-time gate: the default layout must pass every check. A mistyped
/// base, overlapping region, or non-page-aligned size breaks the build here
/// instead of corrupting guest memory at runtime.
const _: () = assert!(
    RuntimeMemoryLayout::default().validate().is_none(),
    "default guest memory layout has overlapping, overflowing, or misaligned regions"
);

/// Compile-time gate: the primary TEB (`teb_low`) must sit at the fixed
/// `GS_BASE`. `PerThreadTeb::primary()` (wie-cpu) and the layout agree on the
/// primary TEB address; drift here would orphan the primary engine's
/// GS-relative last-error slot (`GS_BASE + TEB_LAST_ERROR_OFFSET`).
const _: () = assert!(
    RuntimeMemoryLayout::default().teb_low.base == wie_cpu::GS_BASE,
    "teb_low must sit at the fixed GS_BASE (primary TEB address)"
);

/// Compile-time gate: the worker TEB pool must not contain the primary TEB
/// page — `PerThreadTeb::primary()` (GS_BASE) is the primary's own page and
/// must never be handed to a worker. Overlap with any other region is already
/// rejected by the layout `validate` gate above.
const _: () = assert!(
    !RuntimeMemoryLayout::default()
        .worker_tebs
        .contains(wie_cpu::GS_BASE),
    "worker TEB pool must not overlap the primary TEB at GS_BASE"
);

/// Default layout constants re-exported for existing call sites.
pub const DEFAULT_LAYOUT: RuntimeMemoryLayout = RuntimeMemoryLayout::default();

/// Fake API address region base.
pub const FAKE_API_BASE: u64 = DEFAULT_LAYOUT.fake_api.base;
/// Fake API address region size.
pub const FAKE_API_SIZE: usize = DEFAULT_LAYOUT.fake_api.size;
/// Fake process heap handle.
pub const PROCESS_HEAP_HANDLE: u64 = DEFAULT_LAYOUT.process_heap_handle;
/// Fake process heap base.
pub const PROCESS_HEAP_BASE: u64 = DEFAULT_LAYOUT.process_heap.base;
/// Fake process heap size.
pub const PROCESS_HEAP_SIZE: usize = DEFAULT_LAYOUT.process_heap.size;
/// Offset used by observed Lunar Magic/CRT heap metadata accesses.
pub const PROCESS_HEAP_SHADOW_DELTA: u64 = DEFAULT_LAYOUT.process_heap_shadow_delta;
/// Shadow heap base.
pub const PROCESS_HEAP_SHADOW_BASE: u64 = DEFAULT_LAYOUT.process_heap_shadow_base();
/// Fake low TEB/TIB page base.
pub const FAKE_TEB_LOW_BASE: u64 = DEFAULT_LAYOUT.teb_low.base;
/// Fake low TEB/TIB page size.
pub const FAKE_TEB_LOW_SIZE: usize = DEFAULT_LAYOUT.teb_low.size;
/// Fake resource data base.
pub const FAKE_RESOURCE_DATA_BASE: u64 = DEFAULT_LAYOUT.resource_data.base;
/// Fake resource data size.
pub const FAKE_RESOURCE_DATA_SIZE: usize = DEFAULT_LAYOUT.resource_data.size;
/// Maximum guest instructions between fake API hooks.
pub const ENTRY_TRACE_INSTRUCTION_BUDGET: usize = DEFAULT_LAYOUT.instruction_budget;
/// Maximum consecutive no-hook slices.
pub const ENTRY_TRACE_NO_HOOK_SLICE_LIMIT: usize = DEFAULT_LAYOUT.no_hook_slice_limit;
/// Guest-code region for trivial WinAPI fast-path stubs.
pub const FAST_API_STUB_BASE: u64 = DEFAULT_LAYOUT.fast_api_stub.base;
/// Size of the guest-code fast-path stub region.
pub const FAST_API_STUB_SIZE: usize = DEFAULT_LAYOUT.fast_api_stub.size;
/// Shared `ret` stub for void synchronization APIs.
pub const FAST_VOID_RETURN_STUB_VA: u64 = DEFAULT_LAYOUT.fast_void_return_stub_va();
/// Return trampoline for guest WndProc invocations.
pub const CALLBACK_RETURN_TRAMPOLINE_VA: u64 = DEFAULT_LAYOUT.callback_return_trampoline_va;

pub(crate) fn default_winapi_state(
    layout: &RuntimeMemoryLayout,
    executable_file_bytes: std::sync::Arc<Vec<u8>>,
    process: &wie_pe::ProcessIdentity,
) -> Result<wie_winapi::WinApiState> {
    let heap_size_u64 =
        u64::try_from(layout.process_heap.size).context("heap size does not fit u64")?;
    let heap_end = layout
        .process_heap
        .base
        .checked_add(heap_size_u64)
        .context("heap end overflow")?;

    let executable_file_size = u64::try_from(executable_file_bytes.len())
        .context("executable file size does not fit u64")?;

    Ok(wie_winapi::WinApiState {
        heap_state: wie_winapi::HeapState {
            heap: wie_winapi::GuestHeap::new(layout.process_heap.base, heap_end),
            next_fls_index: 1,
            fls_slots: Vec::new(),
            guest_fls_table_va: layout.guest_fls_table.base,
        },
        process: wie_winapi::ProcessState {
            last_error: 0,
            next_registry_key_handle: wie_winapi::RegistryKeyHandle::from(0x0000_0000_7000_0000),
            registry_keys: Vec::new(),
            main_module_file_name: process.module_file_name.clone(),
            main_module_path: process.module_path.clone(),
            main_module_host_dir: None,
            error_mode: 0,
            suspended_threads: ahash::HashMap::new(),
            // SDL2 guests: disable the DirectInput joystick driver — WIE has
            // no DirectInput COM implementation, and SDL2's fallback (DInput
            // driver init failing → SDL_InitSubSystem failing → video torn
            // down) would break the display for games that use SDL_Init.
            environment: vec![("SDL_DIRECTINPUT_ENABLED".to_string(), "0".to_string())],
            // The main module's RT_DIALOG/RT_MENU/RT_STRING/RT_ACCELERATOR
            // resources are parsed in session init (the section map is not
            // available here).
            main_module_dialogs: Vec::new(),
            main_module_menus: Vec::new(),
            main_module_strings: Vec::new(),
            main_module_accelerators: Vec::new(),
        },
        kernel: wie_winapi::KernelState {
            threads: wie_winapi::ThreadState::primary(),
            sync: wie_winapi::SyncState::new(),
            seh_pending: ahash::HashMap::new(),
        },
        file_io: wie_winapi::FileIoState {
            executable_file_size,
            executable_file_bytes,
            executable_file_cursor: 0,
            next_find_handle: wie_winapi::FindFileHandle::from(0x0000_0000_6200_0000),
            find_handles: Vec::new(),
            host_file_mounts: Vec::new(),
            virtual_files: Vec::new(),
            open_files: ahash::HashMap::new(),
            next_file_handle: wie_winapi::FileHandle::from(0x0000_0000_6700_0001),
            next_resource_handle: wie_winapi::ResourceHandle::from(0x0000_0000_6300_0000),
            resources: Vec::new(),
            current_directory_wide: process.current_directory.encode_utf16().collect(),
            bottle_root: wie_winapi::bottle_root_from_env(),
            volumes: {
                let bottle = wie_winapi::bottle_root_from_env();
                let drive_d = wie_winapi::drive_d_from_env();
                if let Some(ref root) = bottle {
                    let _ = wie_winapi::seed_default_skeleton(root);
                }
                wie_winapi::VolumeConfig::from_parts(bottle, drive_d)
            },
            guest_file_data_next: layout.guest_file_data.base,
            guest_io: None,
            stdin_bytes: Vec::new(),
            stdin_cursor: 0,
            stdin_mode: wie_winapi::GuestStdinMode::InjectOnly,
            ucrt_files: ahash::HashMap::new(),
            ucrt_next_file_va: 0x0000_0000_6900_0000,
            cached_streams: ahash::HashMap::new(),
        },
        dll_states: wie_winapi::DllStateMap::new(),
        message_queue: std::sync::Arc::new(std::sync::Mutex::new(
            wie_winapi::present::MessageQueue::default(),
        )),
        module_state: wie_winapi::ModuleState {
            loaded_modules: ahash::HashMap::new(),
            import_resolver: None,
            get_proc_address_cache: ahash::HashMap::with_capacity(64),
            next_module_handle: wie_winapi::ModuleHandle::from(
                wie_winapi::dll_loader::REAL_MODULE_HANDLE_BASE,
            ),
        },
    })
}

pub(crate) fn default_winapi_environment(
    layout: &RuntimeMemoryLayout,
    image_base: u64,
    command_line_a_ptr: u64,
    command_line_w_ptr: u64,
    environment_strings_w_ptr: u64,
    module_file_name_a_ptr: u64,
    module_file_name_w_ptr: u64,
) -> wie_winapi::WinApiEnvironment {
    wie_winapi::WinApiEnvironment {
        image_base,
        command_line_a_ptr,
        command_line_w_ptr,
        environment_strings_w_ptr,
        module_file_name_a_ptr,
        module_file_name_w_ptr,
        process_heap_handle: layout.process_heap_handle,
    }
}

/// Writes command line + module path strings for a process into the env data page.
pub(crate) fn write_process_identity_strings(
    engine: &mut dyn wie_cpu::CpuEngine,
    command_line_a_ptr: u64,
    command_line_w_ptr: u64,
    module_file_name_a_ptr: u64,
    module_file_name_w_ptr: u64,
    process: &wie_pe::ProcessIdentity,
) -> Result<()> {
    let mut cmd_a = process.command_line.as_bytes().to_vec();
    cmd_a.push(0);
    engine
        .mem_write(command_line_a_ptr, &cmd_a)
        .context("failed to write entry ANSI command line")?;

    write_utf16_string(engine, command_line_w_ptr, &process.command_line)
        .context("failed to write entry UTF-16 command line")?;

    // Module file name APIs return the full guest path when available.
    let mut mod_a = process.module_path.as_bytes().to_vec();
    mod_a.push(0);
    engine
        .mem_write(module_file_name_a_ptr, &mod_a)
        .context("failed to write entry ANSI module file name")?;

    write_utf16_string(engine, module_file_name_w_ptr, &process.module_path)
        .context("failed to write entry UTF-16 module file name")?;

    Ok(())
}

pub(crate) fn write_utf16_string(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    value: &str,
) -> Result<()> {
    let mut bytes = Vec::new();

    for unit in value.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }

    bytes.extend_from_slice(&0_u16.to_le_bytes());

    engine
        .mem_write(address, &bytes)
        .context("failed to write UTF-16 string")
}

pub(crate) fn build_default_environment_strings_w() -> Result<Vec<u8>> {
    let values = [
        "PATH=C:\\Windows\\System32",
        "TEMP=C:\\Users\\WIE\\AppData\\Local\\Temp",
        "TMP=C:\\Users\\WIE\\AppData\\Local\\Temp",
        // SDL2 guests: disable the DirectInput joystick driver (no WIE
        // DirectInput COM). Without the hint, the dinput driver's
        // CoCreateInstance fails → SDL_InitSubSystem fails → the already-
        // initialized video subsystem gets torn down.
        "SDL_DIRECTINPUT_ENABLED=0",
    ];

    let mut bytes = Vec::new();

    for value in values {
        for unit in value.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }

        bytes.extend_from_slice(&0_u16.to_le_bytes());
    }

    // Additional NUL terminates the entire environment block.
    bytes.extend_from_slice(&0_u16.to_le_bytes());

    Ok(bytes)
}
