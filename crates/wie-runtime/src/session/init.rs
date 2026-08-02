use super::SessionOptions;
use super::materialize_crt_argv;
use crate::hooks::{
    RuntimeFakeApiEntry, SoftApiTable, collect_stub_entries, resolve_import_fake_va,
};
use crate::memory::{
    DEFAULT_LAYOUT, RuntimeMemoryLayout, build_default_environment_strings_w,
    default_winapi_environment, default_winapi_state, write_process_identity_strings,
};
use crate::mt_runtime::{ProcessConfig, ProcessResources};
use anyhow::{Context, Result};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use super::profile::RuntimeProfile;

/// Apply final PE section / header protects after image copy (Phase 3.3).
///
/// Sequence: whole image → `PAGE_NOACCESS` (gap pages), headers → RO, each
/// section → characteristics-derived protect. IAT must already be patched in
/// the host-side image buffer before this runs.
fn apply_pe_section_protects(
    engine: &mut dyn wie_cpu::CpuEngine,
    plan: &wie_pe::PeMapPlan,
) -> Result<()> {
    use wie_cpu::protect::{PAGE_NOACCESS, PAGE_READONLY};

    let image_size = usize::try_from(plan.size_of_image).context("size_of_image")?;
    if image_size == 0 {
        return Ok(());
    }
    // Gaps / padding: committed NOACCESS so VirtualQuery sees image space.
    engine
        .virtual_protect(plan.image_base, image_size, PAGE_NOACCESS)
        .context("PE gap NOACCESS protect")?;

    let header_len = u64::from(plan.header_size);
    if let Some((start, end)) = wie_pe::page_align_image_range(0, header_len, plan.size_of_image) {
        let len = usize::try_from(end.saturating_sub(start)).context("header range")?;
        if len > 0 {
            engine
                .virtual_protect(plan.image_base.saturating_add(start), len, PAGE_READONLY)
                .context("PE headers protect")?;
        }
    }

    for sec in &plan.sections {
        let rva = u64::from(sec.va);
        let vsize = u64::from(sec.virtual_size);
        if vsize == 0 {
            continue;
        }
        let Some((start, end)) = wie_pe::page_align_image_range(rva, vsize, plan.size_of_image)
        else {
            continue;
        };
        let len = usize::try_from(end.saturating_sub(start)).context("section range")?;
        if len == 0 {
            continue;
        }
        engine
            .virtual_protect(
                plan.image_base.saturating_add(start),
                len,
                sec.final_protect,
            )
            .with_context(|| format!("PE section {} protect", sec.name))?;
    }
    Ok(())
}

/// Register default layout ranges into the CPU region table (Phase 1.2 / 3.3).
fn register_layout_regions(
    engine: &mut dyn wie_cpu::CpuEngine,
    layout: &RuntimeMemoryLayout,
    image_base: u64,
    image_size: usize,
    pe_plan: Option<&wie_pe::PeMapPlan>,
) {
    use wie_cpu::{GuestRegion, RegionKind};

    // Whole image + optional per-section named regions for diagnostics / Phase 4.
    engine.register_region(GuestRegion::new(
        "image",
        RegionKind::Image,
        image_base,
        image_size,
        wie_cpu::RwxPerms::ALL,
    ));
    if let Some(plan) = pe_plan {
        let header_len = usize::try_from(plan.header_size).unwrap_or(0);
        if header_len > 0 {
            engine.register_region(GuestRegion::new(
                "image.headers",
                RegionKind::Image,
                image_base,
                header_len,
                wie_cpu::protect::PageProtect::from_win32(wie_pe::PeMapPlan::header_protect())
                    .map_or(
                        wie_cpu::RwxPerms::READ,
                        wie_cpu::protect::PageProtect::to_rwx,
                    ),
            ));
        }
        for sec in &plan.sections {
            let va = image_base.saturating_add(u64::from(sec.va));
            let size = usize::try_from(sec.virtual_size).unwrap_or(0);
            if size == 0 {
                continue;
            }
            let name = format!("image.{}", sec.name.trim_matches('\0'));
            engine.register_region(GuestRegion::new(
                name,
                RegionKind::Image,
                va,
                size,
                wie_cpu::protect::PageProtect::from_win32(sec.final_protect).map_or(
                    wie_cpu::RwxPerms::READ,
                    wie_cpu::protect::PageProtect::to_rwx,
                ),
            ));
        }
    }

    // Phase 4.x: pure data regions are RW (not RWX). Soft-translate W is
    // denied on executable pages; stack/heap must stay non-X for pin super path.
    let data_rw = wie_cpu::RwxPerms::READ_WRITE;
    let code_rwx = wie_cpu::RwxPerms::ALL;
    let regs: [GuestRegion; 16] = [
        GuestRegion::new(
            "stack",
            RegionKind::Stack,
            layout.stack_base,
            layout.stack_size,
            data_rw,
        ),
        GuestRegion::new(
            "process_heap",
            RegionKind::Heap,
            layout.process_heap_base,
            layout.process_heap_size,
            data_rw,
        ),
        GuestRegion::new(
            "process_heap_shadow",
            RegionKind::Heap,
            layout.process_heap_shadow_base(),
            layout.process_heap_size,
            data_rw,
        ),
        GuestRegion::new(
            "fake_api",
            RegionKind::FakeApi,
            layout.fake_api_base,
            layout.fake_api_size,
            code_rwx,
        ),
        GuestRegion::new(
            "teb",
            RegionKind::Teb,
            layout.teb_low_base,
            layout.teb_low_size,
            data_rw,
        ),
        GuestRegion::new(
            "env",
            RegionKind::Env,
            layout.env_data_base,
            layout.env_data_size,
            data_rw,
        ),
        GuestRegion::new(
            "resource",
            RegionKind::Resource,
            layout.resource_data_base,
            layout.resource_data_size,
            data_rw,
        ),
        GuestRegion::new(
            "guest_io_code",
            RegionKind::GuestCode,
            layout.guest_io_code_base,
            layout.guest_io_code_size,
            code_rwx,
        ),
        GuestRegion::new(
            "guest_io_table",
            RegionKind::GuestIo,
            layout.guest_io_table_base,
            layout.guest_io_table_size,
            data_rw,
        ),
        GuestRegion::new(
            "guest_file_data",
            RegionKind::GuestIo,
            layout.guest_file_data_base,
            layout.guest_file_data_size,
            data_rw,
        ),
        GuestRegion::new(
            "guest_fls",
            RegionKind::Other,
            layout.guest_fls_table_base,
            layout.guest_fls_table_size,
            data_rw,
        ),
        GuestRegion::new(
            "guest_heap_ctrl",
            RegionKind::Heap,
            layout.guest_heap_ctrl_base,
            layout.guest_heap_ctrl_size,
            data_rw,
        ),
        GuestRegion::new(
            "guest_heap_code",
            RegionKind::GuestCode,
            layout.guest_heap_code_base,
            layout.guest_heap_code_size,
            code_rwx,
        ),
        GuestRegion::new(
            "guest_mbwc_code",
            RegionKind::GuestCode,
            layout.guest_mbwc_code_base,
            layout.guest_mbwc_code_size,
            code_rwx,
        ),
        GuestRegion::new(
            "fast_api_stub",
            RegionKind::GuestCode,
            layout.fast_api_stub_base,
            layout.fast_api_stub_size,
            code_rwx,
        ),
        GuestRegion::new(
            "clock_table",
            RegionKind::Other,
            layout.clock_table_va,
            layout.clock_table_size,
            data_rw,
        ),
    ];
    for region in regs {
        engine.register_region(region);
    }
}

/// Bundle of fields produced by session initialization (avoids 8-arg constructors).
struct SessionInit {
    engine: Box<dyn wie_cpu::CpuEngine>,
    environment: wie_winapi::WinApiEnvironment,
    winapi_state: wie_winapi::WinApiState,
    soft_apis: SoftApiTable,
    layout: RuntimeMemoryLayout,
    stop_bitmap: Arc<[u8]>,
    shared_jit: Option<Arc<wie_cpu::JitShared>>,
    guest_mem: Option<Arc<RwLock<wie_cpu::GuestMemory>>>,
    entry_point_va: u64,
    initial_rsp: u64,
}

impl super::RuntimeSession {
    fn from_init(init: SessionInit) -> Self {
        let profile_enabled = std::env::var_os("WIE_RUNTIME_PROFILE").is_some();
        if profile_enabled {
            wie_winapi::present::set_frame_timing_enabled(true);
        }
        let entry_point_va = init.entry_point_va;
        let initial_rsp = init.initial_rsp;
        let process = Self::build_process(init);
        Self {
            process,
            entry_point_va,
            initial_rsp,
            next_api_index: 0,
            no_hook_slices: 0,
            pending_callbacks: Vec::new(),
            profile_enabled,
            profile: RuntimeProfile::default(),
            last_published_last_error: None,
            entry_reached: false,
            was_waiting_for_message: false,
            frame_last_gen: 0,
            frame_last_stops: 0,
            frame_last_iced: 0,
            frame_last_jit: 0,
        }
    }

    fn build_process(init: SessionInit) -> ProcessResources {
        let SessionInit {
            engine,
            environment,
            winapi_state,
            soft_apis,
            layout,
            stop_bitmap,
            shared_jit,
            guest_mem,
            ..
        } = init;
        let config = ProcessConfig {
            soft_apis,
            environment,
            layout,
            stop_bitmap,
            primary_tid: wie_winapi::PRIMARY_THREAD_ID,
        };
        let shared_winapi = Arc::new(Mutex::new(winapi_state));
        // Clone the message-queue Arc so the host can post input without ever
        // locking the big WinApiState mutex.
        let shared_message_queue = {
            let guard = crate::mt_runtime::lock(&shared_winapi);
            guard.message_queue.clone()
        };

        ProcessResources {
            config,
            engine,
            shared_jit,
            guest_mem,
            shared_winapi,
            shared_message_queue,
            worker_joins: Vec::new(),
        }
    }

    /// Creates and initializes a long-lived Lunar Magic runtime session.
    pub fn new(
        path: &std::path::Path,
        idle_policy: wie_winapi::MessageQueueIdlePolicy,
    ) -> Result<Self> {
        // `new_with_options` applies `RuntimeMemoryLayout::with_env_overrides`.
        Self::new_with_options(path, idle_policy, DEFAULT_LAYOUT, SessionOptions::default())
    }

    /// Creates a session with an explicit guest memory layout.
    pub fn new_with_layout(
        path: &std::path::Path,
        idle_policy: wie_winapi::MessageQueueIdlePolicy,
        layout: RuntimeMemoryLayout,
    ) -> Result<Self> {
        Self::new_with_options(path, idle_policy, layout, SessionOptions::default())
    }

    /// Creates a session with guest argv / stdin bootstrap options.
    pub fn new_with_options(
        path: &std::path::Path,
        idle_policy: wie_winapi::MessageQueueIdlePolicy,
        layout: RuntimeMemoryLayout,
        options: SessionOptions,
    ) -> Result<Self> {
        let t_init = Instant::now();
        // Env heap override applies even when callers pass `DEFAULT_LAYOUT` explicitly
        // (CLI / tests). Callers that need a fixed size should set a non-default size
        // after `with_env_overrides`, or clear the env var.
        let layout = layout.with_env_overrides();
        let mut soft_apis = SoftApiTable::default();
        // MT.4: plant Interlocked* (and other soft-only GPA targets) at fixed
        // soft indices 0..N so GetProcAddress encode_unresolved matches.
        for entry in wie_winapi::PREPLANTED_SOFT_APIS {
            soft_apis
                .intern(entry.library, entry.name, 0)
                .with_context(|| {
                    format!("failed to plant soft API {}!{}", entry.library, entry.name)
                })?;
        }
        // Read the PE file once; parse once and reuse the parsed representation.
        let pe_bytes = std::fs::read(path)
            .with_context(|| format!("failed to read PE file: {}", path.display()))?;
        // Parse PE once (avoids double-parse: pe_identity_from_bytes and
        // load_pe_direct_from_bytes both used to call PE::parse independently).
        let pe = wie_pe::PE::parse(&pe_bytes).context("failed to parse PE image")?;
        let identity = wie_pe::pe_identity_from_parsed(&pe, path, &pe_bytes)
            .context("failed to extract PE identity")?;
        let image_size =
            usize::try_from(identity.size_of_image).context("size_of_image does not fit usize")?;

        // WIE CPU backend (JIT default; `WIE_CPU=iced` for interpreter).
        let backend = wie_cpu::open_cpu().context("failed to open WIE CPU backend")?;
        let (mut engine, shared_jit, guest_mem) = match backend {
            wie_cpu::CpuBackend::Jit { engine, shared } => (engine, Some(shared), None),
            wie_cpu::CpuBackend::Iced { engine, guest_mem } => (engine, None, Some(guest_mem)),
        };

        // Phase 3.3: one MEM_IMAGE arena, temporary RWX — headers/sections/IAT
        // are written directly into guest memory (no intermediate Vec<u8> buffer).
        engine
            .mem_map_image(identity.image_base, image_size, wie_cpu::RwxPerms::ALL)
            .context("failed to map PE image memory")?;

        // Load PE directly into guest memory: writes headers + sections + patches IAT
        // in-place through the engine using the already-parsed PE. Returns the section
        // map plan too — no need to re-read the file.
        // Collect RuntimeFakeApiEntry during the resolution pass so we can skip
        // the redundant build_iat_fake_api_entries call later.
        let mut iat_entries: Vec<RuntimeFakeApiEntry> = Vec::new();
        let (image_summary, pe_map_plan, _patched_imports) = {
            let engine_ref = &mut *engine;
            wie_pe::load_pe_direct_from_parsed(
                &pe,
                &pe_bytes,
                identity.image_base,
                image_size,
                |va, bytes| {
                    engine_ref
                        .mem_write(va, bytes)
                        .map_err(|e| anyhow::anyhow!("PE write to guest memory failed: {e}"))
                },
                |import| {
                    let name = if import.name.is_empty() {
                        format!("ORDINAL {}", import.ordinal)
                    } else {
                        import.name.clone()
                    };
                    // Legacy msvcrt data imports: IAT must hold the variable address,
                    // not a callable fake VA (guest loads through the slot).
                    if wie_winapi::ucrt::is_ucrt_library(&import.library)
                        && let Some(data_va) = wie_winapi::ucrt::crt_data_import_va(&name)
                    {
                        let entry = crate::hooks::make_entry(
                            data_va,
                            import.library.clone(),
                            name,
                            import.iat_slot_va,
                        );
                        iat_entries.push(entry);
                        return Ok(data_va);
                    }
                    let (va, entry) = resolve_import_fake_va(
                        &import.library,
                        &name,
                        import.iat_slot_va,
                        &mut soft_apis,
                    )?;
                    iat_entries.push(entry);
                    Ok(va)
                },
            )
            .context("failed to load PE64 image directly into guest memory")?
        };

        let fake_api_entries = collect_stub_entries(&iat_entries, &soft_apis);

        apply_pe_section_protects(engine.as_mut(), &pe_map_plan)
            .context("failed to apply PE section protects")?;

        engine
            .mem_map(
                layout.fake_api_base,
                layout.fake_api_size,
                wie_cpu::RwxPerms::ALL,
            )
            .context("failed to map fake API memory")?;

        let fake_api_size_u64 =
            u64::try_from(layout.fake_api_size).context("fake API size does not fit u64")?;
        let fake_api_end = layout
            .fake_api_base
            .checked_add(fake_api_size_u64)
            .context("fake API end overflow")?
            .checked_sub(1)
            .context("fake API end underflow")?;

        // Guest acceleration regions (outside host-stop hook range for helpers).
        engine
            .mem_map(
                layout.guest_io_code_base,
                layout.guest_io_code_size,
                wie_cpu::RwxPerms::ALL,
            )
            .context("failed to map guest I/O code region")?;
        let data_rw = wie_cpu::RwxPerms::READ_WRITE;
        engine
            .mem_map(
                layout.guest_io_table_base,
                layout.guest_io_table_size,
                data_rw,
            )
            .context("failed to map guest I/O handle table")?;
        engine
            .mem_map(
                layout.guest_file_data_base,
                layout.guest_file_data_size,
                data_rw,
            )
            .context("failed to map guest file-data arena")?;
        engine
            .mem_map(
                layout.guest_fls_table_base,
                layout.guest_fls_table_size,
                data_rw,
            )
            .context("failed to map guest FLS table")?;
        engine
            .mem_write(
                layout.guest_fls_table_base,
                &vec![0_u8; layout.guest_fls_table_size],
            )
            .context("failed to zero guest FLS table")?;

        // Phase 5 stub data: metrics / syscolors / cwd blob (cwd filled after identity).
        engine
            .mem_map(
                layout.guest_stub_data_base,
                layout.guest_stub_data_size,
                data_rw,
            )
            .context("failed to map guest stub data page")?;
        let stub_page = crate::guest_stubs::build_stub_data_page();
        engine
            .mem_write(layout.guest_stub_data_base, &stub_page)
            .context("failed to write guest stub data page")?;

        // B5: host-written guest clock table. Written once here (frozen values
        // under `WIE_FIXED_CLOCK=1`), then refreshed every host stop so the
        // in-guest clock stubs advance without stopping the host.
        engine
            .mem_map(layout.clock_table_va, layout.clock_table_size, data_rw)
            .context("failed to map guest clock table")?;
        crate::guest_stubs::refresh_clock_table(engine.as_mut(), layout.clock_table_va)
            .context("failed to initialize guest clock table")?;

        let stub_cfg = crate::guest_stubs::GuestStubConfig::from_layout(&layout);

        // Plant trivial WinAPI as real x86-64 stubs and build stop-bit mask.
        // OOL helpers live after guest_io ReadFile/SetFP/GetFS (0x000/0x200/0x400).
        // Use the remainder of the guest_io code mapping (~0x1A00 bytes).
        let mut stop_bitmap = crate::guest_stubs::plant_guest_stubs(
            &mut engine,
            &fake_api_entries,
            layout.fake_api_base,
            layout.fake_api_size,
            &stub_cfg,
            layout.guest_io_code_base + 0x600,
            layout.guest_io_code_size.saturating_sub(0x600),
        )?;

        let guest_io_config = crate::guest_io::install_guest_io(
            &mut engine,
            &fake_api_entries,
            &mut stop_bitmap,
            &layout,
        )?;

        engine
            .mem_map(
                layout.guest_heap_ctrl_base,
                layout.guest_heap_ctrl_size,
                data_rw,
            )
            .context("failed to map guest heap control")?;
        engine
            .mem_map(
                layout.guest_heap_code_base,
                layout.guest_heap_code_size,
                wie_cpu::RwxPerms::ALL,
            )
            .context("failed to map guest heap code")?;

        let guest_heap_cfg = crate::guest_heap_accel::install_guest_heap_accel(
            &mut engine,
            &fake_api_entries,
            &mut stop_bitmap,
            &layout,
        )?;

        engine
            .mem_map(
                layout.guest_mbwc_code_base,
                layout.guest_mbwc_code_size,
                wie_cpu::RwxPerms::ALL,
            )
            .context("failed to map guest MultiByteToWideChar code")?;
        let _guest_mbwc = crate::guest_mbwc::install_guest_mbwc(
            &mut engine,
            &fake_api_entries,
            &mut stop_bitmap,
            &layout,
        )?;

        // JIT: direct UCRT imports (malloc/memcpy/strlen/…) + guest heap layout.
        // Dense: small VA→kind table from soft/IAT names (no runtime HashMap probe on stop).
        {
            let mut pairs = Vec::new();
            for entry in &fake_api_entries {
                if let Some(kind) = wie_cpu::FastApiKind::from_export_name(&entry.name) {
                    pairs.push((entry.fake_target_va, kind));
                }
            }
            let heap_end = layout
                .process_heap_base
                .saturating_add(layout.process_heap_size as u64);
            engine.configure_jit_fast_path(wie_cpu::JitFastPathConfig {
                heap: wie_cpu::JitHeapLayout {
                    ctrl_va: guest_heap_cfg.ctrl_va,
                    base: layout.process_heap_base,
                    end: heap_end,
                },
                pairs,
            });
        }

        // Freeze once — every worker + the primary engine share this Arc
        // instead of paying a per-thread `Vec::clone` of the fake-API bitmap.
        let stop_bitmap: Arc<[u8]> = Arc::from(stop_bitmap.into_boxed_slice());
        engine
            .install_runtime_hooks(layout.fake_api_base, fake_api_end, Arc::clone(&stop_bitmap))
            .context("failed to install persistent runtime hooks")?;

        // Selective precompile: in-guest stubs (GetLastError / CS / …) and the
        // PE entry point so the first guest block runs compiled instead of iced.
        // Full .text section precompile is deferred — precompiling every fake-API
        // VA spikes init peak RAM, and precompiling large sections adds startup
        // time disproportionate to the interpreted warmup saved.
        for entry in &fake_api_entries {
            if entry.traits.guest_stub() {
                engine.precompile_at(entry.fake_target_va);
            }
        }
        engine.precompile_at(image_summary.entry_point_va);

        engine
            .mem_map(
                layout.stack_base,
                layout.stack_size,
                wie_cpu::RwxPerms::READ_WRITE,
            )
            .context("failed to map entry stack memory")?;

        let stack_size_u64 =
            u64::try_from(layout.stack_size).context("stack size does not fit u64")?;

        let stack_top = layout
            .stack_base
            .checked_add(stack_size_u64)
            .context("entry stack top overflow")?;

        let initial_rsp = stack_top
            .checked_sub(0x1008)
            .context("entry initial RSP underflow")?;

        engine
            .write_rsp(initial_rsp)
            .context("failed to initialize entry RSP")?;

        engine
            .mem_map(
                layout.teb_low_base,
                layout.teb_low_size,
                wie_cpu::RwxPerms::READ_WRITE,
            )
            .context("failed to map fake low TEB page")?;

        let stack_limit = layout.stack_base;

        engine
            .mem_write(
                layout.teb_low_base.wrapping_add(0x08),
                &stack_top.to_le_bytes(),
            )
            .context("failed to write fake TEB StackBase")?;

        engine
            .mem_write(
                layout.teb_low_base.wrapping_add(0x10),
                &stack_limit.to_le_bytes(),
            )
            .context("failed to write fake TEB StackLimit")?;

        // TEB.Self (x64 offset 0x30) — guest PEB / TLS lookups.
        engine
            .mem_write(
                layout.teb_low_base.wrapping_add(0x30),
                &layout.teb_low_base.to_le_bytes(),
            )
            .context("failed to write fake TEB Self")?;

        // TEB.LastErrorValue (x64 offset 0x68) — guest GetLastError/SetLastError stubs.
        engine
            .mem_write(crate::guest_stubs::TEB_LAST_ERROR_VA, &0_u32.to_le_bytes())
            .context("failed to zero TEB LastErrorValue")?;

        engine
            .mem_map(
                layout.env_data_base,
                layout.env_data_size,
                wie_cpu::RwxPerms::READ_WRITE,
            )
            .context("failed to map entry environment data memory")?;

        // Guest page for UCRT FILE* cookies / CRT pointer slots (ucrt module).
        engine
            .mem_map(0x0000_0000_6800_0000, 0x1000, wie_cpu::RwxPerms::READ_WRITE)
            .context("failed to map guest UCRT data page")?;
        // Pre-init CRT pointer slots (filled fully after process identity is known).
        {
            const CRT: u64 = 0x0000_0000_6800_0000;
            engine.mem_write(CRT + 0x300, &0_u64.to_le_bytes())?; // environ ptr
            engine.mem_write(CRT + 0x308, &0_u64.to_le_bytes())?; // argv ptr (set below)
            engine.mem_write(CRT + 0x310, &1_u32.to_le_bytes())?; // argc (set below)
            engine.mem_write(CRT + 0x318, &0_u32.to_le_bytes())?; // commode
            engine.mem_write(CRT + 0x320, &0_u32.to_le_bytes())?; // fmode
            engine.mem_write(CRT + 0x328, &0_u64.to_le_bytes())?; // acmdln (set below)
        }

        let command_line_a_ptr = layout
            .env_data_base
            .checked_add(0x100)
            .context("entry command line A pointer overflow")?;

        let command_line_w_ptr = layout
            .env_data_base
            .checked_add(0x200)
            .context("entry command line W pointer overflow")?;

        let environment_strings_w_ptr = layout
            .env_data_base
            .checked_add(0x400)
            .context("entry environment strings W pointer overflow")?;

        let module_file_name_a_ptr = layout
            .env_data_base
            .checked_add(0x700)
            .context("entry module file name A pointer overflow")?;

        let module_file_name_w_ptr = layout
            .env_data_base
            .checked_add(0x800)
            .context("entry module file name W pointer overflow")?;

        let process = wie_pe::process_identity_from_host_path_with_args(path, &options.guest_args);
        write_process_identity_strings(
            &mut engine,
            command_line_a_ptr,
            command_line_w_ptr,
            module_file_name_a_ptr,
            module_file_name_w_ptr,
            &process,
        )?;

        // Publish cwd for in-guest GetCurrentDirectoryW (Microsoft Learn path string).
        crate::guest_stubs::publish_cwd_wide(
            engine.as_mut(),
            stub_cfg.cwd_blob_va,
            &process.current_directory,
        )?;

        // UCRT argc/argv/acmdln for CRT-linked and __p__* guest stubs.
        materialize_crt_argv(
            engine.as_mut(),
            &process.module_file_name,
            &options.guest_args,
            command_line_a_ptr,
        )?;

        let environment_strings_w = build_default_environment_strings_w()?;

        engine
            .mem_write(environment_strings_w_ptr, &environment_strings_w)
            .context("failed to write entry UTF-16 environment strings")?;

        engine
            .mem_map(
                layout.process_heap_base,
                layout.process_heap_size,
                wie_cpu::RwxPerms::READ_WRITE,
            )
            .context("failed to map fake process heap memory")?;

        engine
            .mem_map(
                layout.process_heap_shadow_base(),
                layout.process_heap_size,
                wie_cpu::RwxPerms::READ_WRITE,
            )
            .context("failed to map fake process heap shadow memory")?;

        engine
            .mem_map(
                layout.resource_data_base,
                layout.resource_data_size,
                wie_cpu::RwxPerms::READ_WRITE,
            )
            .context("failed to map fake resource memory")?;

        // Phase 1.2 / 3.3: register named layout + PE section ranges.
        register_layout_regions(
            engine.as_mut(),
            &layout,
            image_summary.image_base,
            image_summary.image_size,
            Some(&pe_map_plan),
        );

        let environment = default_winapi_environment(
            &layout,
            image_summary.image_base,
            command_line_a_ptr,
            command_line_w_ptr,
            environment_strings_w_ptr,
            module_file_name_a_ptr,
            module_file_name_w_ptr,
        );

        let executable_file_bytes = pe_bytes.clone();

        let mut winapi_state = default_winapi_state(&layout, executable_file_bytes, &process)?;

        // Register the primary thread kernel object so DuplicateHandle
        // can resolve GetCurrentThread/GetCurrentProcess pseudohandles.
        {
            let ctx = wie_cpu::ThreadContext::default();
            let _ = winapi_state
                .kernel
                .sync
                .register_thread(wie_winapi::PRIMARY_THREAD_ID, ctx);
        }

        // Register .pdata function table for C++ exception handling.
        // RtlLookupFunctionEntry needs this to find unwind info by RIP.
        if let Some(pdata) = pe_map_plan
            .sections
            .iter()
            .find(|s| s.name.starts_with(".pdata"))
        {
            let off = usize::try_from(pdata.pointer_to_raw_data).unwrap_or(0);
            let sz = usize::try_from(pdata.size_of_raw_data).unwrap_or(0);
            if off > 0
                && sz > 0
                && let Some(raw) = pe_bytes.get(off..off.saturating_add(sz))
            {
                let entries = wie_winapi::exception::parse_pdata(raw);
                if !entries.is_empty() {
                    winapi_state
                        .kernel
                        .sync
                        .function_tables
                        .insert(image_summary.image_base, entries);
                }
            }
        }

        winapi_state.window_state().message_queue_idle_policy = idle_policy;
        // Main-module dialogs: the EXE does not go through `load_dll`, so its
        // RT_DIALOG templates are parsed here (mirroring the LoadedModule
        // construction site in dll_loader.rs). DialogBoxParam resolves
        // `hInstance == image base` against this list.
        winapi_state.process.main_module_dialogs =
            wie_pe::resources::parse_dialogs(&pe_bytes, &pe_map_plan.sections);
        // Modal-dialog result slot: EndDialog writes, the in-guest stub reads.
        winapi_state.window_state().dialog_result_va = stub_cfg.dialog_result_va;
        engine
            .mem_write(stub_cfg.dialog_result_va, &0_u32.to_le_bytes())
            .context("failed to zero the guest dialog result slot")?;
        winapi_state.file_io.guest_io = Some(wie_winapi::GuestIoRuntimeConfig {
            table_va: guest_io_config.table_va,
            file_data_base: guest_io_config.file_data_base,
            file_data_size: guest_io_config.file_data_size,
        });
        winapi_state.file_io.guest_file_data_next = layout.guest_file_data_base;
        winapi_state.heap_state.guest_fls_table_va = layout.guest_fls_table_base;
        // Empty inject ⇒ live host stdin on ReadFile(STD_INPUT); non-empty
        // inject is deterministic and never blocks on the TTY.
        winapi_state.file_io.stdin_mode = if options.stdin_bytes.is_empty() {
            wie_winapi::GuestStdinMode::LiveHost
        } else {
            wie_winapi::GuestStdinMode::InjectOnly
        };
        winapi_state.file_io.stdin_bytes = options.stdin_bytes;
        winapi_state.file_io.stdin_cursor = 0;
        winapi_state
            .heap_state
            .heap
            .attach_guest_control(guest_heap_cfg.ctrl_va);

        // Set the import resolver for dynamic DLL loading.
        {
            let mut soft = soft_apis.clone();
            winapi_state.module_state.import_resolver = Some(wie_winapi::ImportResolver::new(
                Box::new(move |lib, name, slot| {
                    let (va, _entry) =
                        crate::hooks::resolve_import_fake_va(lib, name, slot, &mut soft)
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                    Ok(va)
                }),
            ));
        }

        // Store the host directory for DLL search fallback.
        if let Some(parent) = path.parent() {
            winapi_state.process.main_module_host_dir = Some(parent.to_owned());
        }

        let mut session = Self::from_init(SessionInit {
            engine,
            environment,
            winapi_state,
            soft_apis,
            layout,
            stop_bitmap,
            shared_jit,
            guest_mem,
            entry_point_va: image_summary.entry_point_va,
            initial_rsp,
        });
        if session.profile_enabled {
            session.profile.init_ns = t_init.elapsed().as_nanos();
            session.profile.mem_backend = session
                .process
                .with_mut(|e, _| e.mem_backend_name().to_owned());
            // Micro / default sessions: idle from env with Micro default (Yield).
            session.profile.idle_policy =
                wie_winapi::IdlePolicy::from_env_for(wie_winapi::IdleContext::Micro)
                    .as_str()
                    .to_owned();
            session.profile.jit = session.process.with_mut(|e, _| e.cpu_stats());
        }
        tracing::info!(
            target: "wiegui",
            path = %path.display(),
            entry = session.entry_point_va,
            "guest session started"
        );
        Ok(session)
    }
}
