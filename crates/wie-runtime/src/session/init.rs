//! PE load and region-layout setup for a `RuntimeSession`.

use super::SessionOptions;
use super::materialize_crt_argv;
use super::types::{GuestStackPtr, GuestTid, GuestVa};
use crate::hooks::{
    RuntimeFakeApiEntry, SoftApiTable, collect_stub_entries, resolve_import_fake_va,
};
use crate::memory::{
    DEFAULT_LAYOUT, RuntimeMemoryLayout, build_default_environment_strings_w,
    default_winapi_environment, default_winapi_state, write_process_identity_strings,
};
use crate::mt_runtime::{ProcessConfig, ProcessResources};
use ahash::{HashMap, HashMapExt};
use anyhow::{Context, Result};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use super::profile::RuntimeProfile;

/// Copy `.wad` game data from the host exe dir into the guest-visible exe
/// dir (the bottle). DOOM Retro's bundle ships `doomretro.wad` (resource)
/// and `freedoom1.wad` / `freedoom2.wad` (free IWADs) beside the exe, but the
/// guest can only reach the bottle; without them it errors out at startup
/// ("doomretro.wad can't be found"). The freedoom IWADs are also aliased to
/// the classic names (`DOOM1.WAD` / `DOOM2.WAD`) classic Doom engines search
/// for. When the exe already runs from inside the bottle (or the CLI's
/// app-folder staging copied the WADs in), the copy is a no-op but the
/// freedoom aliases still materialize from whatever is present.
fn stage_wad_payload(
    volumes: &wie_winapi::VolumeConfig,
    exe: &Path,
    module_path: &str,
) -> Result<()> {
    let Some(host_exe_dir) = exe.parent() else {
        return Ok(());
    };
    let guest_dir = wie_winapi::vfs::guest_parent(module_path);
    let Some(map) = wie_winapi::vfs::guest_path_to_host(volumes, &guest_dir) else {
        return Ok(());
    };
    let dest_dir = map.host;
    // On a case-insensitive filesystem (macOS / Windows defaults) two path
    // strings that differ only in letter case can denote the SAME directory
    // (e.g. `Program Files\doomretro` vs `Program Files\DoomRetro`). Compare
    // canonicalized forms so this guard fires whenever both names resolve to
    // the same physical dir. Without it the copy loop below runs and copies
    // each WAD onto itself, which `std::fs::copy` truncates to 0 *before*
    // reading — zeroing every guest WAD during session init. See
    // `stage_wad_payload_does_not_self_copy_wads_across_case_variants`.
    if same_host_dir(host_exe_dir, &dest_dir) {
        // The CLI's app-folder staging already copied the WAD payload beside
        // the guest-visible exe; only the classic freedoom alias names
        // (`DOOM1.WAD` / `DOOM2.WAD`) still need materializing.
        return materialize_freedoom_aliases(&dest_dir);
    }
    std::fs::create_dir_all(&dest_dir)
        .with_context(|| format!("create bottle app dir: {}", dest_dir.display()))?;
    for entry in std::fs::read_dir(host_exe_dir)
        .with_context(|| format!("read exe dir for WAD payload: {}", host_exe_dir.display()))?
    {
        let entry = entry.with_context(|| "read exe-dir entry".to_string())?;
        let src = entry.path();
        if !src.is_file() {
            continue;
        }
        let Some(fname) = src.file_name() else {
            continue;
        };
        let Some(ext) = src.extension() else {
            continue;
        };
        if !ext.eq_ignore_ascii_case("wad") {
            continue;
        }
        let target = dest_dir.join(fname);
        std::fs::copy(&src, &target).with_context(|| {
            format!(
                "copy WAD payload into bottle ({} -> {})",
                src.display(),
                target.display()
            )
        })?;
        materialize_freedoom_alias(&dest_dir, fname, &src)?;
    }
    Ok(())
}

/// Materialize the classic Doom IWAD alias for one freedoom WAD already
/// present in `dir`: `freedoom1.wad` → `DOOM1.WAD`, `freedoom2.wad` →
/// `DOOM2.WAD`. No-op for any other name. The alias is written only when it
/// is missing or a zero-byte placeholder and the source is a non-empty file;
/// a non-empty existing alias (e.g. a user-provided commercial IWAD) is
/// preserved.
fn materialize_freedoom_alias(dir: &Path, wad_name: &std::ffi::OsStr, src: &Path) -> Result<()> {
    let Some(alias_name) = freedoom_alias_name(wad_name) else {
        return Ok(());
    };
    if !is_non_empty_file(src) {
        return Ok(());
    }
    let alias_path = dir.join(alias_name);
    if is_non_empty_file(&alias_path) {
        return Ok(());
    }
    std::fs::copy(src, &alias_path)
        .with_context(|| {
            format!(
                "copy WAD payload alias into bottle ({} -> {})",
                src.display(),
                alias_path.display()
            )
        })
        .map(|_| ())
}

/// True when `path` names an existing regular file with a non-zero length.
fn is_non_empty_file(path: &Path) -> bool {
    path.is_file() && path.metadata().is_ok_and(|m| m.len() > 0)
}

/// The classic alias name for a freedoom IWAD, or `None` for other WADs.
fn freedoom_alias_name(wad_name: &std::ffi::OsStr) -> Option<&'static str> {
    match wad_name.to_string_lossy().to_ascii_lowercase().as_str() {
        "freedoom1.wad" => Some("DOOM1.WAD"),
        "freedoom2.wad" => Some("DOOM2.WAD"),
        _ => None,
    }
}

/// True when `a` and `b` name the same physical directory, regardless of
/// letter-case spelling (case-insensitive filesystems) or symlink indirection.
///
/// Exact path equality short-circuits; otherwise the two paths are compared
/// through [`std::fs::canonicalize`], which resolves symlinks and normalizes
/// to the on-disk case. A failed canonicalization (a path that does not exist
/// yet) falls back to `false` so the caller's normal creation/copy path runs.
fn same_host_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

/// Copy `freedoom1.wad` / `freedoom2.wad` in `dir` to their classic alias
/// names. Idempotent: a non-empty existing alias is left untouched, while a
/// missing or zero-byte alias is replaced from the non-empty freedoom WAD.
/// Covers the whole-folder-staging case where the WAD payload already sits
/// beside the guest-visible exe (the copy loop above never runs).
fn materialize_freedoom_aliases(dir: &Path) -> Result<()> {
    for wad_name in ["freedoom1.wad", "freedoom2.wad"] {
        let src = dir.join(wad_name);
        let Some(wad_os) = src.file_name() else {
            continue;
        };
        materialize_freedoom_alias(dir, wad_os, &src)?;
    }
    Ok(())
}

/// Register default layout ranges into the CPU region table.
fn register_layout_regions(
    engine: &mut dyn wie_cpu::CpuEngine,
    layout: &RuntimeMemoryLayout,
    image_base: u64,
    image_size: usize,
    pe_plan: Option<&wie_pe::PeMapPlan>,
) {
    use wie_cpu::{GuestRegion, RegionKind};

    // Whole image + optional per-section named regions for diagnostics.
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

    // Fixed layout regions — single source of truth is `RuntimeMemoryLayout`
    // (names, kinds, perms, geometry all live in the layout, validated at
    // compile time). Soft-translate W is denied on executable pages; stack
    // and heap stay non-X for the pin super path, so perms come from the
    // layout rather than being re-derived here.
    for region in layout.regions() {
        engine.register_region(GuestRegion::new(
            region.name,
            region.kind,
            region.base,
            region.size,
            region.perms,
        ));
    }
}

/// Bundle of fields produced by session initialization (avoids 8-arg constructors).
pub(crate) struct SessionInit {
    pub(crate) engine: Box<dyn wie_cpu::CpuEngine>,
    pub(crate) environment: wie_winapi::WinApiEnvironment,
    pub(crate) winapi_state: wie_winapi::WinApiState,
    pub(crate) soft_apis: SoftApiTable,
    pub(crate) layout: RuntimeMemoryLayout,
    pub(crate) stop_bitmap: Arc<[u8]>,
    pub(crate) shared_jit: Option<Arc<wie_cpu::JitShared>>,
    pub(crate) guest_mem: Option<Arc<RwLock<wie_cpu::GuestMemory>>>,
    pub(crate) static_dll_mains: Vec<wie_winapi::dll_loader::StaticDllMain>,
    pub(crate) entry_point_va: GuestVa,
    pub(crate) initial_rsp: GuestStackPtr,
}

impl SessionInit {
    /// Assemble an init bundle from the fully-constructed pieces.
    // Wide signature: a session init bundle carries every runtime subsystem.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        engine: Box<dyn wie_cpu::CpuEngine>,
        environment: wie_winapi::WinApiEnvironment,
        winapi_state: wie_winapi::WinApiState,
        soft_apis: SoftApiTable,
        layout: RuntimeMemoryLayout,
        stop_bitmap: Arc<[u8]>,
        shared_jit: Option<Arc<wie_cpu::JitShared>>,
        guest_mem: Option<Arc<RwLock<wie_cpu::GuestMemory>>>,
        static_dll_mains: Vec<wie_winapi::dll_loader::StaticDllMain>,
        entry_point_va: GuestVa,
        initial_rsp: GuestStackPtr,
    ) -> Self {
        Self {
            engine,
            environment,
            winapi_state,
            soft_apis,
            layout,
            stop_bitmap,
            shared_jit,
            guest_mem,
            static_dll_mains,
            entry_point_va,
            initial_rsp,
        }
    }
}

impl super::RuntimeSession {
    fn from_init(init: SessionInit) -> Result<Self> {
        let profile_enabled = std::env::var_os("WIE_RUNTIME_PROFILE").is_some();
        if profile_enabled {
            wie_winapi::present::set_frame_timing_enabled(true);
        }
        let entry_point_va = init.entry_point_va;
        let initial_rsp = init.initial_rsp;
        let process = Self::build_process(init)?;
        // Arm the lock-wait timing gate alongside the frame-timing gate:
        // both live behind the same `profile_enabled` flag.
        if profile_enabled {
            process.lock_wait_stats.set_enabled(true);
        }
        Ok(Self {
            process,
            entry_point_va,
            initial_rsp,
            next_api_index: 0,
            no_hook_slices: 0,
            pending_callbacks: Vec::new(),
            outer_api_names: HashMap::new(),
            profile_enabled,
            profile: RuntimeProfile::default(),
            entry_reached: false,
            was_waiting_for_message: false,
            frame_last_gen: 0,
            frame_last_stops: 0,
            frame_last_iced: 0,
            frame_last_jit: 0,
        })
    }

    fn build_process(init: SessionInit) -> Result<ProcessResources> {
        let SessionInit {
            engine,
            environment,
            winapi_state,
            soft_apis,
            layout,
            stop_bitmap,
            shared_jit,
            guest_mem,
            static_dll_mains,
            ..
        } = init;
        let config = ProcessConfig {
            soft_apis,
            environment,
            layout,
            stop_bitmap,
            primary_tid: GuestTid::PRIMARY,
            static_dll_mains,
        };
        let shared_winapi = Arc::new(Mutex::new(winapi_state));
        // Clone the message-queue Arc so the host can post input without ever
        // locking the big WinApiState mutex.
        let shared_message_queue = {
            let guard = crate::mt_runtime::lock(&shared_winapi);
            guard.message_queue.clone()
        };

        // Worker TEB pages come from a shared pool: distinct per worker, with
        // exited workers returning their page for reuse (spawn/join cycles).
        let worker_teb_pool = Arc::new(Mutex::new(
            crate::mt_runtime::WorkerTebPool::new(&layout)
                .context("failed to build worker TEB pool")?,
        ));

        Ok(ProcessResources {
            config,
            engine,
            shared_jit,
            guest_mem,
            shared_winapi,
            lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
            shared_message_queue,
            worker_joins: Vec::new(),
            worker_teb_pool,
        })
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
        let phase = |name: &str, t0: Instant| {
            tracing::debug!(
                phase = name,
                ms = t0.elapsed().as_secs_f64() * 1e3,
                "init phase"
            );
            Instant::now()
        };
        // Env heap override applies even when callers pass `DEFAULT_LAYOUT` explicitly
        // (CLI / tests). Callers that need a fixed size should set a non-default size
        // after `with_env_overrides`, or clear the env var.
        let layout = layout.with_env_overrides();
        let mut t_phase = Instant::now();
        let mut soft_apis = SoftApiTable::default();
        // Plant Interlocked* (and other soft-only GPA targets) at fixed soft
        // indices 0..N so GetProcAddress encode_unresolved matches.
        for entry in wie_winapi::PREPLANTED_SOFT_APIS {
            soft_apis
                .intern(entry.library, entry.name, 0)
                .with_context(|| {
                    format!("failed to plant soft API {}!{}", entry.library, entry.name)
                })?;
        }
        // Read the PE file once; parse once and reuse the parsed representation.
        // The bytes are shared (Arc) so the WinAPI state's main-module file
        // reader and the session's own .pdata/dialog parsing never copy the
        // whole image.
        let pe_bytes = Arc::new(
            std::fs::read(path)
                .with_context(|| format!("failed to read PE file: {}", path.display()))?,
        );
        // Parse PE once (avoids double-parse: pe_identity_from_bytes and
        // load_pe_direct_from_bytes both used to call PE::parse independently).
        let pe = wie_pe::PE::parse(&pe_bytes).context("failed to parse PE image")?;
        let identity = wie_pe::pe_identity_from_parsed(&pe, path, &pe_bytes)
            .context("failed to extract PE identity")?;
        let image_size =
            usize::try_from(identity.size_of_image).context("size_of_image does not fit usize")?;
        t_phase = phase("pe-read-parse", t_phase);

        // WIE CPU backend (JIT default; `WIE_CPU=iced` for interpreter).
        let backend = wie_cpu::open_cpu().context("failed to open WIE CPU backend")?;
        let (mut engine, shared_jit, guest_mem) = match backend {
            wie_cpu::CpuBackend::Jit { engine, shared } => (engine, Some(shared), None),
            wie_cpu::CpuBackend::Iced { engine, guest_mem } => (engine, None, Some(guest_mem)),
        };
        t_phase = phase("backend-open", t_phase);

        // One MEM_IMAGE arena, temporary RWX — headers/sections/IAT are written
        // directly into guest memory (no intermediate Vec<u8> buffer).
        engine
            .mem_map_image(identity.image_base, image_size, wie_cpu::RwxPerms::ALL)
            .context("failed to map PE image memory")?;

        // Load PE directly into guest memory: writes headers + sections + patches IAT
        // in-place through the engine using the already-parsed PE. Returns the section
        // map plan too — no need to re-read the file.
        // Collect RuntimeFakeApiEntry during the resolution pass so we can skip
        // the redundant build_iat_fake_api_entries call later.
        let mut iat_entries: Vec<RuntimeFakeApiEntry> = Vec::new();
        // Static guest-DLL imports (SDL2.dll, libogg-0.dll, …): pass 1 records
        // `(library, name, iat slot)`; pass 2 loads the real DLL and overwrites
        // the slot once winapi_state + import_resolver + main_module_host_dir
        // exist (all created after the image load below). The IAT slot span is
        // tracked here (pass 2 relaxes it to writable before patching).
        let mut deferred_guest_imports: Vec<(String, String, u64)> = Vec::new();
        let mut deferred_first_slot: Option<u64> = None;
        let mut deferred_last_slot: Option<u64> = None;
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
                    // Static guest-DLL import: record the IAT slot for pass 2,
                    // which loads the real DLL and overwrites the slot. The
                    // soft placeholder below keeps today's "unsupported API"
                    // stop if the DLL cannot be loaded (graceful degradation).
                    if !wie_winapi::is_winapi_library(&import.library) {
                        let slot = import.iat_slot_va;
                        deferred_guest_imports.push((import.library.clone(), name.clone(), slot));
                        let slot_end = slot.saturating_add(8);
                        deferred_first_slot =
                            Some(deferred_first_slot.map_or(slot, |first| first.min(slot)));
                        deferred_last_slot =
                            Some(deferred_last_slot.map_or(slot_end, |last| last.max(slot_end)));
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

        wie_winapi::dll_loader::apply_pe_section_protects(
            engine.as_mut(),
            &pe_map_plan,
            pe_map_plan.image_base,
        )
        .context("failed to apply PE section protects")?;
        t_phase = phase("image-load+protects", t_phase);

        engine
            .mem_map(
                layout.fake_api.base,
                layout.fake_api.size,
                wie_cpu::RwxPerms::ALL,
            )
            .context("failed to map fake API memory")?;

        let fake_api_size_u64 =
            u64::try_from(layout.fake_api.size).context("fake API size does not fit u64")?;
        let fake_api_end = layout
            .fake_api
            .base
            .checked_add(fake_api_size_u64)
            .context("fake API end overflow")?
            .checked_sub(1)
            .context("fake API end underflow")?;

        // Guest acceleration regions (outside host-stop hook range for helpers).
        engine
            .mem_map(
                layout.guest_io_code.base,
                layout.guest_io_code.size,
                wie_cpu::RwxPerms::ALL,
            )
            .context("failed to map guest I/O code region")?;
        let data_rw = wie_cpu::RwxPerms::READ_WRITE;
        engine
            .mem_map(
                layout.guest_io_table.base,
                layout.guest_io_table.size,
                data_rw,
            )
            .context("failed to map guest I/O handle table")?;
        engine
            .mem_map(
                layout.guest_file_data.base,
                layout.guest_file_data.size,
                data_rw,
            )
            .context("failed to map guest file-data arena")?;
        engine
            .mem_map(
                layout.guest_fls_table.base,
                layout.guest_fls_table.size,
                data_rw,
            )
            .context("failed to map guest FLS table")?;
        engine
            .mem_write(
                layout.guest_fls_table.base,
                &vec![0_u8; layout.guest_fls_table.size],
            )
            .context("failed to zero guest FLS table")?;

        // Stub data: metrics / syscolors / cwd blob (cwd filled after identity)
        // plus the planted file-dialog modal-loop + proc stub bodies, so the
        // page must be executable (RWX like the code regions above).
        engine
            .mem_map(
                layout.guest_stub_data.base,
                layout.guest_stub_data.size,
                wie_cpu::RwxPerms::ALL,
            )
            .context("failed to map guest stub data page")?;
        let stub_page = crate::guest_stubs::build_stub_data_page();
        engine
            .mem_write(layout.guest_stub_data.base, &stub_page)
            .context("failed to write guest stub data page")?;

        let stub_cfg = crate::guest_stubs::GuestStubConfig::from_layout(&layout);

        // Plant the file-dialog modal-loop + dialog-proc stub bodies into the
        // (now executable) stub data page. The comdlg32 `GetOpenFileName` /
        // `GetSaveFileName` handler runs the modal loop via a guest callback
        // and bridges WM_COMMAND/WM_CLOSE to the proc stub, which ends the
        // dialog; both are pure guest code with fixed fake-VA callees.
        let mut file_dialog_loop = Vec::new();
        let mut stub_ctx = crate::guest_stubs::StubCtx::new(&mut file_dialog_loop, &stub_cfg);
        stub_ctx.encode_file_dialog_loop();
        engine
            .mem_write(stub_cfg.file_dialog_loop_va, &file_dialog_loop)
            .context("failed to write file-dialog modal-loop body")?;
        let file_dialog_proc = crate::guest_stubs::encode_file_dialog_proc(
            wie_winapi::encode_export(wie_winapi::WinApiId::User32Enddialog),
        );
        engine
            .mem_write(stub_cfg.file_dialog_proc_va, &file_dialog_proc)
            .context("failed to write file-dialog proc stub")?;

        // Host-written guest clock table. Written once here (frozen values
        // under `WIE_FIXED_CLOCK=1`), then refreshed every host stop so the
        // in-guest clock stubs advance without stopping the host.
        engine
            .mem_map(layout.clock_table.base, layout.clock_table.size, data_rw)
            .context("failed to map guest clock table")?;
        crate::guest_stubs::refresh_clock_table(engine.as_mut(), layout.clock_table.base)
            .context("failed to initialize guest clock table")?;

        // Plant trivial WinAPI as real x86-64 stubs and build stop-bit mask.
        // OOL helpers live after guest_io ReadFile/SetFP/GetFS (0x000/0x200/0x400).
        // Use the remainder of the guest_io code mapping (~0x1A00 bytes).
        let mut stop_bitmap = crate::guest_stubs::plant_guest_stubs(
            &mut engine,
            &fake_api_entries,
            layout.fake_api.base,
            layout.fake_api.size,
            &stub_cfg,
            layout.guest_io_code.base + 0x600,
            layout.guest_io_code.size.saturating_sub(0x600),
        )?;

        let guest_io_config = crate::guest_io::install_guest_io(
            &mut engine,
            &fake_api_entries,
            &mut stop_bitmap,
            &layout,
        )?;

        engine
            .mem_map(
                layout.guest_heap_ctrl.base,
                layout.guest_heap_ctrl.size,
                data_rw,
            )
            .context("failed to map guest heap control")?;
        engine
            .mem_map(
                layout.guest_heap_code.base,
                layout.guest_heap_code.size,
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
                layout.guest_mbwc_code.base,
                layout.guest_mbwc_code.size,
                wie_cpu::RwxPerms::ALL,
            )
            .context("failed to map guest MultiByteToWideChar code")?;
        let _guest_mbwc = crate::guest_mbwc::install_guest_mbwc(
            &mut engine,
            &fake_api_entries,
            &mut stop_bitmap,
            &layout,
        )?;
        t_phase = phase("stub-planting+accelerators", t_phase);

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
                .process_heap
                .base
                .saturating_add(layout.process_heap.size as u64);
            engine.configure_jit_fast_path(wie_cpu::JitFastPathConfig {
                heap: wie_cpu::JitHeapLayout {
                    ctrl_va: guest_heap_cfg.ctrl_va,
                    base: layout.process_heap.base,
                    end: heap_end,
                },
                pairs,
            });
        }

        // Freeze once — every worker + the primary engine share this Arc
        // instead of paying a per-thread `Vec::clone` of the fake-API bitmap.
        let stop_bitmap: Arc<[u8]> = Arc::from(stop_bitmap.into_boxed_slice());
        engine
            .install_runtime_hooks(layout.fake_api.base, fake_api_end, Arc::clone(&stop_bitmap))
            .context("failed to install persistent runtime hooks")?;

        // Selective precompile: in-guest stubs (GetLastError / CS / …) and the
        // PE entry point so the first guest block runs compiled instead of iced.
        // Full .text section precompile is deferred — precompiling every fake-API
        // VA spikes init peak RAM, and precompiling large sections adds startup
        // time disproportionate to the interpreted warmup saved.
        //
        // Stubs go to the background compiler (deferred): the guest cannot call
        // any stub until it starts executing, so the Cranelift work overlaps
        // with the entry-point run instead of blocking session init. Only the
        // entry point compiles synchronously — the very first block must be
        // ready before the guest runs.
        for entry in &fake_api_entries {
            if entry.traits.guest_stub() {
                engine.precompile_deferred_at(entry.fake_target_va);
            }
        }
        engine.precompile_at(image_summary.entry_point_va);
        t_phase = phase("precompile", t_phase);

        engine
            .mem_map(
                layout.stack.base,
                layout.stack.size,
                wie_cpu::RwxPerms::READ_WRITE,
            )
            .context("failed to map entry stack memory")?;

        let stack_size_u64 =
            u64::try_from(layout.stack.size).context("stack size does not fit u64")?;

        let stack_top = layout
            .stack
            .base
            .checked_add(stack_size_u64)
            .context("entry stack top overflow")?;

        let initial_rsp = stack_top
            .checked_sub(0x1008)
            .context("entry initial RSP underflow")?;
        t_phase = phase("stack-map", t_phase);

        engine
            .write_rsp(initial_rsp)
            .context("failed to initialize entry RSP")?;

        engine
            .mem_map(
                layout.teb_low.base,
                layout.teb_low.size,
                wie_cpu::RwxPerms::READ_WRITE,
            )
            .context("failed to map fake low TEB page")?;

        // Per-thread TEB pages for guest workers (`CreateThread`). The pages
        // stay mapped for the session; the worker pool re-initializes a page
        // (zero-fill + standard fields) before handing it to a new worker.
        engine
            .mem_map(
                layout.worker_tebs.base,
                layout.worker_tebs.size,
                wie_cpu::RwxPerms::READ_WRITE,
            )
            .context("failed to map worker TEB pool")?;

        let stack_limit = layout.stack.base;

        // Primary TEB page: one 4 KiB page at GS_BASE (a compile-time gate in
        // `crate::memory` pins `teb_low.base == GS_BASE`). The field layout
        // and write order are canonicalized in `wie_cpu::teb` /
        // `guest_layout`; per-thread TEBs for workers are a later phase once
        // CpuEngine grows a per-engine GS-base API. The PEB pointer lives in
        // the spare upper half of the TEB page, so TEB→PEB→ProcessParameters
        // walks (e.g. the UCRT `_get_app_type` check that reads PP->Flags bit
        // 31) return well-defined values instead of dereferencing a NULL PEB.
        let fake_peb_va = layout.teb_low.base.wrapping_add(0x800);
        let fake_pp_va = layout.teb_low.base.wrapping_add(0x900);
        let primary_teb = wie_cpu::PerThreadTeb::primary();
        let teb_init = wie_cpu::TebInit {
            stack_top,
            stack_limit,
            peb_va: fake_peb_va,
        };
        primary_teb
            .init(&mut *engine, &teb_init)
            .context("failed to initialize fake low TEB page")?;
        // PEB.ImageBaseAddress (0x10), ProcessParameters (0x20), ProcessHeap
        // (0x30) — the fields guests probe most. BeingDebugged (0x02) stays 0
        // (the mapped page is zero-filled).
        engine
            .mem_write(
                fake_peb_va.wrapping_add(0x10),
                &identity.image_base.to_le_bytes(),
            )
            .context("failed to write fake PEB ImageBaseAddress")?;
        engine
            .mem_write(fake_peb_va.wrapping_add(0x20), &fake_pp_va.to_le_bytes())
            .context("failed to write fake PEB ProcessParameters")?;
        engine
            .mem_write(
                fake_peb_va.wrapping_add(0x30),
                &layout.process_heap.base.to_le_bytes(),
            )
            .context("failed to write fake PEB ProcessHeap")?;
        // RTL_USER_PROCESS_PARAMETERS.Flags (0x8) = 0: the UCRT app-type
        // check reads bit 31 and takes the normal (desktop) path.
        engine
            .mem_write(fake_pp_va.wrapping_add(0x8), &0_u32.to_le_bytes())
            .context("failed to zero fake PP Flags")?;

        engine
            .mem_map(
                layout.env_data.base,
                layout.env_data.size,
                wie_cpu::RwxPerms::READ_WRITE,
            )
            .context("failed to map entry environment data memory")?;

        // Guest page for UCRT FILE* cookies / CRT pointer slots (ucrt module).
        engine
            .mem_map(super::CRT_GUEST_BASE, 0x1000, wie_cpu::RwxPerms::READ_WRITE)
            .context("failed to map guest UCRT data page")?;
        // Pre-init CRT pointer slots (filled fully after process identity is known).
        {
            engine.mem_write(super::CRT_ENVIRON_PTR_SLOT, &0_u64.to_le_bytes())?; // environ ptr
            engine.mem_write(super::CRT_ARGV_PTR_SLOT, &0_u64.to_le_bytes())?; // argv ptr (set below)
            engine.mem_write(super::CRT_ARGC_SLOT, &1_u32.to_le_bytes())?; // argc (set below)
            engine.mem_write(super::CRT_COMMODE_SLOT, &0_u32.to_le_bytes())?; // commode
            engine.mem_write(super::CRT_FMODE_SLOT, &0_u32.to_le_bytes())?; // fmode
            engine.mem_write(super::CRT_ACMDLN_PTR_SLOT, &0_u64.to_le_bytes())?; // acmdln (set below)
        }

        let command_line_a_ptr = layout
            .env_data
            .base
            .checked_add(0x100)
            .context("entry command line A pointer overflow")?;

        let command_line_w_ptr = layout
            .env_data
            .base
            .checked_add(0x200)
            .context("entry command line W pointer overflow")?;

        let environment_strings_w_ptr = layout
            .env_data
            .base
            .checked_add(0x400)
            .context("entry environment strings W pointer overflow")?;

        let module_file_name_a_ptr = layout
            .env_data
            .base
            .checked_add(0x700)
            .context("entry module file name A pointer overflow")?;

        let module_file_name_w_ptr = layout
            .env_data
            .base
            .checked_add(0x800)
            .context("entry module file name W pointer overflow")?;
        t_phase = phase("teb+crt+env-addrs", t_phase);

        // Effective volume roots: explicit session options win, else `WIE_ROOT`
        // / `WIE_DRIVE_D`, else the global app-data bottle. The identity is
        // derived from the SAME volumes the winapi state will use, so an
        // in-bottle exe's guest module path reflects its real location.
        let bottle_root = options
            .bottle_root
            .clone()
            .or_else(wie_winapi::bottle_root_from_env);
        let drive_d_root = options
            .drive_d_root
            .clone()
            .or_else(wie_winapi::drive_d_from_env);
        let volumes =
            wie_winapi::VolumeConfig::from_parts(bottle_root.clone(), drive_d_root.clone());

        let mut process =
            wie_pe::process_identity_from_host_path_with_args(path, &options.guest_args);
        // The loader defaults the module path to `C:\{name}` (it has no volume
        // knowledge); remap through the volume config when the host path lives
        // under a mapped volume — the bottle's `drive_c` or the D: bridge.
        if let Some(guest_path) = wie_winapi::host_path_to_guest(&volumes, path) {
            process.module_path = guest_path;
        }
        // The CLI stages external app folders beside the exe and sets the
        // process current directory to the staged app dir (normal Windows
        // launch semantics: relative resource paths resolve from the exe's
        // directory). `None` keeps the loader default (`C:\`).
        if let Some(cwd) = options.current_directory.as_deref() {
            process.current_directory = cwd.to_owned();
        }
        // Make sibling `.wad` game data guest-visible (DOOM Retro needs its
        // IWAD + resource WAD; the host exe dir is outside the bottle).
        stage_wad_payload(&volumes, path, &process.module_path)?;
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
        // RTL_USER_PROCESS_PARAMETERS.Environment (x64 offset 0x78): point the
        // guest CRT `getenv` (which walks PEB→ProcessParameters→Environment)
        // at the block above — SDL2 reads its SDL_HINT_* env vars this way.
        engine
            .mem_write(
                fake_pp_va.wrapping_add(0x78),
                &environment_strings_w_ptr.to_le_bytes(),
            )
            .context("failed to write fake PP Environment pointer")?;
        t_phase = phase("identity+env-strings", t_phase);

        engine
            .mem_map(
                layout.process_heap.base,
                layout.process_heap.size,
                wie_cpu::RwxPerms::READ_WRITE,
            )
            .context("failed to map fake process heap memory")?;

        engine
            .mem_map(
                layout.process_heap_shadow_base(),
                layout.process_heap.size,
                wie_cpu::RwxPerms::READ_WRITE,
            )
            .context("failed to map fake process heap shadow memory")?;

        engine
            .mem_map(
                layout.resource_data.base,
                layout.resource_data.size,
                wie_cpu::RwxPerms::READ_WRITE,
            )
            .context("failed to map fake resource memory")?;
        t_phase = phase("heap+resource-maps", t_phase);

        // Register named layout and PE section ranges.
        register_layout_regions(
            engine.as_mut(),
            &layout,
            image_summary.image_base,
            image_summary.image_size,
            Some(&pe_map_plan),
        );
        t_phase = phase("regions-registered", t_phase);

        let environment = default_winapi_environment(
            &layout,
            image_summary.image_base,
            command_line_a_ptr,
            command_line_w_ptr,
            environment_strings_w_ptr,
            module_file_name_a_ptr,
            module_file_name_w_ptr,
        );
        t_phase = phase("regions+env", t_phase);

        let executable_file_bytes = pe_bytes.clone();

        let mut winapi_state = default_winapi_state(&layout, executable_file_bytes, &process)?;
        t_phase = phase("winapi-state", t_phase);
        // `default_winapi_state` built its volume config from the environment;
        // re-apply the effective roots so the state's volumes agree with the
        // identity derived above (an explicit `SessionOptions` root overrides
        // `WIE_ROOT`, mirroring the old post-hoc `set_bottle_root`).
        // Seed the default Windows folder skeleton for the session's C:
        // volume — the override root OR the global app-data bottle (the
        // effective root is the C: the guest will see). Idempotent mkdirs
        // make re-seeding harmless.
        let effective_root = wie_winapi::effective_bottle_root(&volumes);
        winapi_state.file_io.volumes = volumes;
        winapi_state.file_io.bottle_root = bottle_root;
        let _ = wie_winapi::seed_default_skeleton(&effective_root);
        t_phase = phase("seed-skeleton", t_phase);
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
        // Main-module menus: same lifecycle as the dialogs above; LoadMenuW
        // resolves `hInstance == image base` against this list.
        winapi_state.process.main_module_menus =
            wie_pe::resources::parse_menus(&pe_bytes, &pe_map_plan.sections);
        // Main-module string table: same lifecycle as the menus above;
        // LoadStringA/W resolves `hInstance == image base` against this list.
        winapi_state.process.main_module_strings =
            wie_pe::resources::parse_strings(&pe_bytes, &pe_map_plan.sections);
        // Main-module accelerator tables: same lifecycle as the strings above;
        // LoadAcceleratorsA/W resolves `hInstance == image base` against this
        // list by table id.
        winapi_state.process.main_module_accelerators =
            wie_pe::resources::parse_accelerators(&pe_bytes, &pe_map_plan.sections);
        // Modal-dialog result slot: EndDialog writes, the in-guest stub reads.
        winapi_state.window_state().dialog_result_va = stub_cfg.dialog_result_va;
        engine
            .mem_write(stub_cfg.dialog_result_va, &0_u32.to_le_bytes())
            .context("failed to zero the guest dialog result slot")?;
        // Interactive file-dialog machinery: the planted modal-loop + proc stub
        // bodies. `GetOpenFileName`/`GetSaveFileName` under
        // `FileDialogPolicy::Interactive` run the loop via a guest callback and
        // the EndDialog write-back; zero when the runtime did not plant them.
        winapi_state.window_state().file_dialog_loop_va = stub_cfg.file_dialog_loop_va;
        winapi_state.window_state().file_dialog_proc_va = stub_cfg.file_dialog_proc_va;
        winapi_state.file_io.guest_io = Some(wie_winapi::GuestIoRuntimeConfig {
            table_va: guest_io_config.table_va,
            file_data_base: guest_io_config.file_data_base,
            file_data_size: guest_io_config.file_data_size,
        });
        winapi_state.file_io.guest_file_data_next = layout.guest_file_data.base;
        winapi_state.heap_state.guest_fls_table_va = layout.guest_fls_table.base;
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

        // Pass 2: static guest-DLL imports. Pass 1 left soft placeholders in
        // the IAT slots; now that winapi_state, the import resolver, and the
        // main module's host dir exist, load each guest DLL and overwrite the
        // slot with the export's real guest VA. Failures keep the placeholder
        // (the guest sees today's "unsupported API" stop — graceful
        // degradation). No-op for the current micros (no guest-DLL imports).
        // Statically-loaded deps are also pinned against FreeLibrary and
        // recorded (load order) for the DllMain(PROCESS_ATTACH) phase the
        // session pump runs before the exe entry.
        let mut static_dll_mains: Vec<wie_winapi::dll_loader::StaticDllMain> = Vec::new();
        if let (Some(first_slot), Some(last_slot)) = (deferred_first_slot, deferred_last_slot) {
            let mut fake_resolve = |lib: &str, name: &str, slot: u64| -> anyhow::Result<u64> {
                let (va, _entry) =
                    crate::hooks::resolve_import_fake_va(lib, name, slot, &mut soft_apis)
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok(va)
            };
            // The IAT lives in a READONLY section (protects applied at image
            // load); relax the slot span, patch, then re-apply the protects.
            // The restore is unconditional: a failed patch write must not
            // leave the IAT span relaxed.
            let slot_span = last_slot.saturating_sub(first_slot);
            engine
                .virtual_protect(
                    first_slot,
                    usize::try_from(slot_span).context("static import IAT span too large")?,
                    wie_cpu::protect::PAGE_READWRITE,
                )
                .context("failed to relax IAT span for static guest import patching")?;
            let patch_result = (|| -> anyhow::Result<()> {
                for (library, name, slot) in &deferred_guest_imports {
                    match wie_winapi::dll_loader::resolve_static_guest_import(
                        engine.as_mut(),
                        &mut winapi_state,
                        library,
                        name,
                        &mut fake_resolve,
                        &mut static_dll_mains,
                    ) {
                        Ok(Some(export_va)) => {
                            engine
                                .mem_write(*slot, &export_va.to_le_bytes())
                                .with_context(|| {
                                    format!(
                                        "failed to patch static guest import {library}!{name} at {slot:#x}"
                                    )
                                })?;
                        }
                        Ok(None) => {
                            tracing::debug!(
                                target: "wiegui",
                                library, name,
                                "static guest import unavailable; keeping placeholder"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                target: "wiegui",
                                library, name,
                                error = %format!("{e:#}"),
                                "static guest import load failed; keeping placeholder"
                            );
                        }
                    }
                }
                Ok(())
            })();
            let restored = wie_winapi::dll_loader::apply_pe_section_protects(
                engine.as_mut(),
                &pe_map_plan,
                pe_map_plan.image_base,
            )
            .context("failed to re-apply PE section protects after static import patching");
            if let Err(patch_err) = patch_result {
                if let Err(restore_err) = restored {
                    tracing::warn!(
                        target: "wiegui",
                        error = %format!("{restore_err:#}"),
                        "failed to restore PE section protects after static patch error"
                    );
                }
                return Err(patch_err);
            }
            restored?;
        }

        let mut session = Self::from_init(SessionInit::new(
            engine,
            environment,
            winapi_state,
            soft_apis,
            layout,
            stop_bitmap,
            shared_jit,
            guest_mem,
            static_dll_mains,
            GuestVa(image_summary.entry_point_va),
            GuestStackPtr(initial_rsp),
        ))?;
        let _ = phase("winapi-state+session", t_phase);
        if session.profile_enabled {
            session.profile.set_init_ns(t_init.elapsed().as_nanos());
            session.profile.set_mem_backend(
                session
                    .process
                    .with_mut(|e, _| e.mem_backend_name().to_owned()),
            );
            // Micro / default sessions: idle from env with Micro default (Yield).
            session.profile.set_idle_policy(
                wie_winapi::IdlePolicy::from_env_for(wie_winapi::IdleContext::Micro)
                    .as_str()
                    .to_owned(),
            );
            session
                .profile
                .set_jit(session.process.with_mut(|e, _| e.cpu_stats()));
        }
        tracing::info!(
            target: "wiegui",
            path = %path.display(),
            entry = session.entry_point_va.0,
            "guest session started"
        );
        Ok(session)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use std::path::{Path, PathBuf};
    use wie_winapi::MessageQueueIdlePolicy;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!("wie-init-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// `stage_wad_payload` copies sibling `.wad` game data into the
    /// guest-visible exe dir (the bottle's `C:\` root for an unbottled exe)
    /// and aliases the freedoom IWADs to the classic names Doom engines
    /// search for (freedoom1.wad → DOOM1.WAD, freedoom2.wad → DOOM2.WAD).
    #[test]
    fn stage_wad_payload_copies_wads_with_freedoom_aliases() {
        let src = TempDir::new("wad-src");
        let exe = src.path().join("doomretro.exe");
        std::fs::write(&exe, b"MZ").expect("write fake exe");
        for name in ["doomretro.wad", "freedoom1.wad", "freedoom2.wad"] {
            std::fs::write(src.path().join(name), format!("wad-{name}")).expect("write fake wad");
        }
        // A non-WAD sibling must not be copied.
        std::fs::write(src.path().join("readme.txt"), b"hi").expect("write readme");

        let bottle = TempDir::new("wad-bottle");
        let volumes = wie_winapi::VolumeConfig::from_parts(Some(bottle.path().to_path_buf()), None);
        // The unbottled exe's guest module path is `C:\{name}` → exe dir `C:\`.
        super::stage_wad_payload(&volumes, &exe, r"C:\doomretro.exe").expect("stage");

        let drive_c = bottle.path().join("drive_c");
        for (name, content) in [
            ("doomretro.wad", "wad-doomretro.wad"),
            ("freedoom1.wad", "wad-freedoom1.wad"),
            ("freedoom2.wad", "wad-freedoom2.wad"),
            ("DOOM1.WAD", "wad-freedoom1.wad"),
            ("DOOM2.WAD", "wad-freedoom2.wad"),
        ] {
            assert_eq!(
                std::fs::read_to_string(drive_c.join(name)).expect("read staged wad"),
                content,
                "staged {name}"
            );
        }
        assert!(
            !drive_c.join("readme.txt").exists(),
            "non-WAD sibling untouched"
        );
    }

    /// Regression for the WAD-truncation bug: `stage_wad_payload` must not
    /// copy a WAD onto itself when the source and destination directories are
    /// the same physical dir reached under different letter-case spelling
    /// (case-insensitive filesystems such as macOS/Windows defaults). The
    /// CLI stages the app folder as `Program Files\{stem}` (lowercase from the
    /// exe name); the guest module path derives through the volume mapping and
    /// carries the on-disk casing (`DoomRetro`). `std::fs::copy(src, target)`
    /// with `src` aliasing `target` truncates the file to 0 *before* reading —
    /// zeroing every guest WAD at startup (see `same_host_dir`).
    #[test]
    fn stage_wad_payload_does_not_self_copy_wads_across_case_variants() {
        let bottle = TempDir::new("wad-selfcopy-bottle");
        // Guest-visible app dir under the on-disk casing the module path
        // carries (`DoomRetro`).
        let drive_c = bottle.path().join("drive_c");
        let app = drive_c.join("Program Files").join("DoomRetro");
        std::fs::create_dir_all(&app).expect("create DoomRetro dir");
        std::fs::write(app.join("doomretro.wad"), vec![0xAB; 65536]).expect("write real wad");
        std::fs::write(app.join("doomretro.exe"), b"MZ").expect("write exe");

        // Only reproducible where the filesystem is case-insensitive: there
        // `Program Files\doomretro` (the lowercase staged path) IS the same
        // directory as `Program Files\DoomRetro`. On case-sensitive systems
        // the two casings are distinct dirs and the staging copy is legitimate.
        if !app.join("doomretro").is_dir() {
            tracing::warn!("skip: filesystem is case-sensitive; case-alias cannot occur");
            return;
        }

        // The staged exe's host path keeps the lowercase stem spelling, while
        // the module path carries the on-disk casing — the exact mismatch that
        // used to send the copy loop down the self-copy path.
        let exe = app.join("doomretro").join("doomretro.exe");
        let volumes = wie_winapi::VolumeConfig::from_parts(Some(bottle.path().to_path_buf()), None);
        super::stage_wad_payload(&volumes, &exe, r"C:\Program Files\DoomRetro\doomretro.exe")
            .expect("stage should succeed");

        let result = std::fs::read(app.join("doomretro.wad")).expect("read staged wad");
        assert_eq!(
            result.len(),
            65536,
            "WAD must not be truncated to 0 by a case-insensitive self-copy"
        );
        assert_eq!(result[0], 0xAB, "WAD content preserved");
    }

    /// A staged in-bottle copy's guest module path derives through the volume
    /// mapping: `{root}/drive_c/Program Files/{name}/{name}.exe` becomes
    /// `C:\Program Files\{name}\{name}.exe` in the session's process identity
    /// (the string GetModuleFileName serves).
    #[test]
    fn session_identity_maps_bottle_copy_to_program_files() {
        let mut micro = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        micro.pop();
        micro.pop();
        micro.push("micro-exes/out/crt_hello.exe");
        if !micro.is_file() {
            tracing::error!(
                "skip: micro-exes/out/crt_hello.exe not built (run make -C micro-exes)"
            );
            return;
        }
        let bottle =
            std::env::temp_dir().join(format!("wie-identity-bottle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&bottle);
        let copy = bottle
            .join("drive_c")
            .join("Program Files")
            .join("crt_hello")
            .join("crt_hello.exe");
        std::fs::create_dir_all(copy.parent().expect("app dir")).expect("create app dir");
        std::fs::copy(&micro, &copy).expect("stage the in-bottle copy");

        let session = crate::RuntimeSession::new_with_options(
            &copy,
            MessageQueueIdlePolicy::ExitOnIdle,
            crate::DEFAULT_LAYOUT,
            crate::SessionOptions {
                bottle_root: Some(bottle.clone()),
                ..crate::SessionOptions::default()
            },
        )
        .expect("session builds from the staged copy");

        let module_path = session
            .process
            .with_winapi_ref(|s| s.process.main_module_path.clone());
        assert_eq!(module_path, r"C:\Program Files\crt_hello\crt_hello.exe");
        let _ = std::fs::remove_dir_all(&bottle);
    }

    /// `SessionOptions::current_directory` (the CLI's staged-app folder)
    /// overrides the loader's `C:\` default in the guest file-io state — the
    /// value relative path resolution (CreateFileA/W, FindFirstFileA, DLL
    /// search) reads.
    #[test]
    fn session_options_current_directory_reaches_guest_file_io() {
        let mut micro = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        micro.pop();
        micro.pop();
        micro.push("micro-exes/out/crt_hello.exe");
        if !micro.is_file() {
            tracing::error!(
                "skip: micro-exes/out/crt_hello.exe not built (run make -C micro-exes)"
            );
            return;
        }
        let bottle = TempDir::new("cwd-bottle");

        let session = crate::RuntimeSession::new_with_options(
            &micro,
            MessageQueueIdlePolicy::ExitOnIdle,
            crate::DEFAULT_LAYOUT,
            crate::SessionOptions {
                bottle_root: Some(bottle.path().to_path_buf()),
                current_directory: Some(r"C:\Program Files\app".to_owned()),
                ..crate::SessionOptions::default()
            },
        )
        .expect("session builds");

        let cwd = session
            .process
            .with_winapi_ref(|s| String::from_utf16_lossy(&s.file_io.current_directory_wide));
        assert_eq!(cwd, r"C:\Program Files\app");
        // The module path still derives through the volume config — the cwd
        // override must not disturb module identity.
        let module_path = session
            .process
            .with_winapi_ref(|s| s.process.main_module_path.clone());
        assert_eq!(module_path, r"C:\crt_hello.exe");
    }

    /// With whole-folder staging the WAD payload already sits beside the
    /// guest-visible exe, so `stage_wad_payload` early-returns — but the
    /// classic freedoom alias names (`DOOM1.WAD` / `DOOM2.WAD`) must still
    /// materialize from the staged files.
    #[test]
    fn in_bottle_wads_get_freedoom_aliases() {
        let bottle = TempDir::new("inbottle-wads");
        let app = bottle.path().join("drive_c").join("App");
        std::fs::create_dir_all(&app).expect("create app dir");
        std::fs::write(app.join("app.exe"), b"MZ").expect("write fake exe");
        std::fs::write(app.join("freedoom1.wad"), b"wad1").expect("write freedoom1");
        let volumes = wie_winapi::VolumeConfig::from_parts(Some(bottle.path().to_path_buf()), None);

        // The in-bottle exe's module path is C:\App\app.exe → exe dir C:\App,
        // which equals the host app dir — the early-return (staged-folder)
        // path.
        super::stage_wad_payload(&volumes, &app.join("app.exe"), r"C:\App\app.exe").expect("stage");

        assert_eq!(
            std::fs::read(app.join("DOOM1.WAD")).expect("alias created"),
            b"wad1",
            "freedoom1.wad aliases to DOOM1.WAD even when already staged"
        );
        assert!(
            !app.join("DOOM2.WAD").exists(),
            "no freedoom2.wad present → no DOOM2.WAD alias"
        );
    }

    /// A missing classic alias materializes from the non-empty freedoom WAD
    /// beside it (the in-bottle / staged-folder path).
    #[test]
    fn missing_alias_materializes_from_freedoom_wad() {
        let dir = TempDir::new("alias-missing");
        std::fs::write(dir.path().join("freedoom2.wad"), b"wad2").expect("write freedoom2");
        super::materialize_freedoom_aliases(dir.path()).expect("materialize");
        assert_eq!(
            std::fs::read(dir.path().join("DOOM2.WAD")).expect("alias created"),
            b"wad2",
            "missing alias materialized from the freedoom IWAD"
        );
    }

    /// A zero-byte stale alias is replaced from the non-empty freedoom WAD —
    /// the DoomRetro failure mode (a placeholder DOOM2.WAD shadowing the real
    /// IWAD and forcing the file picker).
    #[test]
    fn zero_byte_alias_is_replaced_from_freedoom_wad() {
        let dir = TempDir::new("alias-zero");
        std::fs::write(dir.path().join("freedoom2.wad"), b"wad2").expect("write freedoom2");
        std::fs::write(dir.path().join("DOOM2.WAD"), b"").expect("write stale alias");
        super::materialize_freedoom_aliases(dir.path()).expect("materialize");
        assert_eq!(
            std::fs::read(dir.path().join("DOOM2.WAD")).expect("alias replaced"),
            b"wad2",
            "zero-byte placeholder replaced by the freedoom IWAD"
        );
    }

    /// A non-empty existing alias — a real user-provided commercial IWAD — is
    /// preserved, never overwritten by the freedoom copy.
    #[test]
    fn non_empty_alias_is_preserved() {
        let dir = TempDir::new("alias-keep");
        std::fs::write(dir.path().join("freedoom2.wad"), b"wad2").expect("write freedoom2");
        std::fs::write(dir.path().join("DOOM2.WAD"), b"commercial").expect("write commercial IWAD");
        super::materialize_freedoom_aliases(dir.path()).expect("materialize");
        assert_eq!(
            std::fs::read(dir.path().join("DOOM2.WAD")).expect("alias preserved"),
            b"commercial",
            "non-empty alias untouched"
        );
    }

    /// With no freedoom source present no alias appears, and an existing
    /// alias is left alone.
    #[test]
    fn absent_source_creates_no_alias() {
        let dir = TempDir::new("alias-absent");
        std::fs::write(dir.path().join("DOOM1.WAD"), b"commercial").expect("write commercial IWAD");
        super::materialize_freedoom_aliases(dir.path()).expect("materialize");
        assert_eq!(
            std::fs::read(dir.path().join("DOOM1.WAD")).expect("alias preserved"),
            b"commercial",
            "existing alias untouched when the freedoom source is absent"
        );
        assert!(
            !dir.path().join("DOOM2.WAD").exists(),
            "no freedoom2.wad → no DOOM2.WAD alias"
        );
    }

    /// The unbottled-exe copy path applies the same rule: a zero-byte alias
    /// already in the bottle is replaced, a non-empty commercial IWAD is kept.
    #[test]
    fn copy_path_replaces_zero_byte_alias_but_keeps_commercial() {
        let src = TempDir::new("wad-src2");
        let exe = src.path().join("doomretro.exe");
        std::fs::write(&exe, b"MZ").expect("write fake exe");
        std::fs::write(src.path().join("freedoom2.wad"), b"wad2").expect("write freedoom2");

        let bottle = TempDir::new("wad-bottle2");
        let drive_c = bottle.path().join("drive_c");
        std::fs::create_dir_all(&drive_c).expect("create drive_c");
        // A stale zero-byte placeholder and a real commercial IWAD side by side.
        std::fs::write(drive_c.join("DOOM2.WAD"), b"").expect("write stale alias");
        std::fs::write(drive_c.join("DOOM1.WAD"), b"commercial").expect("write commercial IWAD");

        let volumes = wie_winapi::VolumeConfig::from_parts(Some(bottle.path().to_path_buf()), None);
        super::stage_wad_payload(&volumes, &exe, r"C:\doomretro.exe").expect("stage");

        assert_eq!(
            std::fs::read(drive_c.join("DOOM2.WAD")).expect("alias replaced"),
            b"wad2",
            "zero-byte placeholder replaced during the copy path"
        );
        assert_eq!(
            std::fs::read(drive_c.join("DOOM1.WAD")).expect("alias preserved"),
            b"commercial",
            "commercial IWAD preserved during the copy path"
        );
    }
}
