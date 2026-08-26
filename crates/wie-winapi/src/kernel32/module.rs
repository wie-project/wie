use super::{
    Context, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER,
    ERROR_MOD_NOT_FOUND, ERROR_PROC_NOT_FOUND, FAKE_ADVAPI32_MODULE, FAKE_COMCTL32_MODULE,
    FAKE_COMDLG32_MODULE, FAKE_GDI32_MODULE, FAKE_KERNEL32_MODULE, FAKE_SHELL32_MODULE,
    FAKE_USER32_MODULE, FAKE_WINMM_MODULE, HandlerContext, Result, WinApiHandlerResult,
    WinApiState, copy_path_a_to_guest_buffer, copy_path_w_to_guest_buffer,
    create_fake_resource_record, dll_loader, find_resource_by_handle, guest_basename,
    paths_match_guest, read_ansi_string_from_cpu, read_guest_ansi_lossy, read_guest_utf16_lossy,
    read_wide_string_from_cpu,
};

/// Win32 `MAX_PATH` (260 chars incl. NUL) — the longest module/name buffer
/// WIE reads from the guest.
const MAX_PATH: usize = 260;

/// Shared body of `GetModuleHandleA` / `GetModuleHandleW`.
///
/// `NULL` returns the main module image base; a named module must already be
/// loaded, otherwise the handle is NULL with `ERROR_MOD_NOT_FOUND`.
fn get_module_handle(ctx: &mut HandlerContext<'_>, wide: bool) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let environment = ctx.environment;
    let module_name_va = engine
        .read_rcx()
        .context("failed to read RCX for GetModuleHandle")?;

    // The A path decodes UTF-8-first/cp1252; the W path is lossy UTF-16 —
    // each keeps its historical reader.
    let return_value = if module_name_va == 0 {
        state.process.last_error = 0;
        environment.image_base
    } else {
        let module_name = if wide {
            read_guest_utf16_lossy(engine, module_name_va, MAX_PATH)?
        } else {
            read_ansi_string_from_cpu(engine, module_name_va, MAX_PATH)?
        };
        let handle = resolve_loaded_module_handle(&module_name, environment.image_base, state);
        if handle == 0 {
            state.process.last_error = ERROR_MOD_NOT_FOUND;
        } else {
            state.process.last_error = 0;
        }
        handle
    };

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!GetModuleHandleA`.
pub fn handle_get_module_handle_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    get_module_handle(ctx, false)
}
/// Handles `KERNEL32.dll!GetModuleHandleW`.
pub fn handle_get_module_handle_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    get_module_handle(ctx, true)
}
pub(crate) fn normalize_module_name(name: &str) -> String {
    name.trim()
        .trim_matches('"')
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or(name)
        .to_ascii_lowercase()
}
pub(crate) fn is_main_module_name(state: &WinApiState, name: &str) -> bool {
    let norm = normalize_module_name(name);
    if norm.is_empty() {
        return true;
    }
    let main = normalize_module_name(&state.process.main_module_file_name);
    norm == main || paths_match_guest(name, &state.process.main_module_path)
}
pub(crate) fn is_main_module_path(state: &WinApiState, path: &str) -> bool {
    paths_match_guest(path, &state.process.main_module_path)
        || guest_basename(path).eq_ignore_ascii_case(&state.process.main_module_file_name)
}
pub(crate) fn resolve_loaded_module_handle(
    name: &str,
    main_image_base: u64,
    state: &WinApiState,
) -> u64 {
    if is_main_module_name(state, name) {
        return main_image_base;
    }
    match normalize_module_name(name).as_str() {
        "kernel32.dll" => FAKE_KERNEL32_MODULE,
        "user32.dll" => FAKE_USER32_MODULE,
        "gdi32.dll" => FAKE_GDI32_MODULE,
        "comctl32.dll" => FAKE_COMCTL32_MODULE,
        "advapi32.dll" => FAKE_ADVAPI32_MODULE,
        "shell32.dll" => FAKE_SHELL32_MODULE,
        "comdlg32.dll" => FAKE_COMDLG32_MODULE,
        "winmm.dll" => FAKE_WINMM_MODULE,
        // Not pre-loaded: GetModuleHandle must return NULL (LoadLibrary is separate).
        _ => 0,
    }
}
pub(crate) fn resolve_or_load_dll(
    name: &str,
    engine: &mut dyn wie_cpu::CpuEngine,
    environment: crate::WinApiEnvironment,
    state: &mut WinApiState,
) -> u64 {
    let clean = name.trim().trim_matches('"');
    if clean.is_empty() {
        state.process.last_error = ERROR_MOD_NOT_FOUND;
        return 0;
    }

    // 1. Check if it's the main module.
    if is_main_module_name(state, name) {
        return environment.image_base;
    }

    // 2. Check fake/well-known modules.
    let fake_handle = resolve_loaded_module_handle(name, environment.image_base, state);
    if fake_handle != 0 {
        return fake_handle;
    }

    // 3. Check already-loaded real modules. Bump refcount per LoadLibrary contract.
    let norm = normalize_module_name(name);
    if let Some(module) = state.module_state.loaded_modules.get_mut(&norm) {
        module.ref_count = module.ref_count.saturating_add(1);
        return module.handle;
    }

    // 4. Try to load from disk (requires import_resolver).
    let mut resolver_opt = state.module_state.import_resolver.take();
    let Some(ref mut resolver) = resolver_opt else {
        state.process.last_error = ERROR_MOD_NOT_FOUND;
        return 0;
    };

    // Resolve DLL path via search order.
    let host_path = crate::dll_loader::resolve_dll_path(
        name,
        &state.process.main_module_path,
        &state.file_io.volumes,
        state.process.main_module_host_dir.as_deref(),
    );
    let Some(ref host) = host_path else {
        state.module_state.import_resolver = resolver_opt;
        state.process.last_error = ERROR_MOD_NOT_FOUND;
        return 0;
    };

    // Synthesize a guest-style path for the module descriptor, derived from
    // the process identity (main module dir → guest cwd → drive root).
    let guest_cwd = state.file_io.cwd_utf8();
    let guest_path = resolve_windows_dll_path(name, &state.process.main_module_path, &guest_cwd);

    match crate::dll_loader::load_dll(engine, state, host, &guest_path, &mut |lib, name, slot| {
        resolver.resolve(lib, name, slot)
    }) {
        Ok(result) => {
            state.module_state.import_resolver = resolver_opt;
            // Recursively load dependencies. If any dependency fails to
            // load, the parent load also fails (Windows LoadLibrary contract).
            for dep in &result.dependencies {
                if resolve_or_load_dll(dep, engine, environment, state) == 0 {
                    state.process.last_error = ERROR_MOD_NOT_FOUND;
                    return 0;
                }
            }
            result.module.handle
        }
        Err(e) => {
            state.module_state.import_resolver = resolver_opt;
            tracing::warn!("failed to load DLL {}: {e}", name);
            state.process.last_error = ERROR_MOD_NOT_FOUND;
            0
        }
    }
}
/// Synthesize a guest-style absolute DLL path from a bare module name.
///
/// The directory comes from the process identity, never a literal default:
/// the main module's guest directory first, then the guest current directory,
/// then the `C:\` drive root. A `name` that is already a path (separator or
/// drive letter present) is returned unchanged.
pub(crate) fn resolve_windows_dll_path(
    name: &str,
    main_module_path: &str,
    current_directory: &str,
) -> String {
    if name.contains('\\') || name.contains('/') || name.contains(':') {
        return name.to_owned();
    }
    if let Some(dir) = crate::vfs::guest_dir_of(main_module_path) {
        return format!("{dir}\\{name}");
    }
    let cwd = current_directory
        .trim_end_matches(['\\', '/'])
        .replace('/', "\\");
    if cwd.is_empty() {
        return format!("C:\\{name}");
    }
    format!("{cwd}\\{name}")
}

/// Handles `KERNEL32.dll!GetModuleFileNameA`.
pub fn handle_get_module_file_name_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let module_file_name_a_va = ctx.environment.module_file_name_a_ptr;
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _module_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetModuleFileNameA")?;

    let buffer_va = engine
        .read_rdx()
        .context("failed to read RDX for GetModuleFileNameA")?;

    let buffer_len = engine
        .read_r8()
        .context("failed to read R8 for GetModuleFileNameA")?;

    let (return_value, truncated) =
        copy_path_a_to_guest_buffer(engine, module_file_name_a_va, buffer_va, buffer_len)?;

    state.process.last_error = if truncated {
        ERROR_INSUFFICIENT_BUFFER
    } else {
        0
    };

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!GetModuleFileNameW`.
pub fn handle_get_module_file_name_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let module_file_name_w_va = ctx.environment.module_file_name_w_ptr;
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _module_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetModuleFileNameW")?;

    let buffer_va = engine
        .read_rdx()
        .context("failed to read RDX for GetModuleFileNameW")?;

    let buffer_len = engine
        .read_r8()
        .context("failed to read R8 for GetModuleFileNameW")?;

    let (return_value, truncated) =
        copy_path_w_to_guest_buffer(engine, module_file_name_w_va, buffer_va, buffer_len)?;

    state.process.last_error = if truncated {
        ERROR_INSUFFICIENT_BUFFER
    } else {
        0
    };

    ctx.finish(return_value)
}
/// Shared body of `LoadLibraryA` / `LoadLibraryW` — real DLL loading.
fn load_library(ctx: &mut HandlerContext<'_>, wide: bool) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let environment = ctx.environment;
    let library_name_va = engine
        .read_rcx()
        .context("failed to read RCX for LoadLibrary")?;

    let return_value = if library_name_va == 0 {
        state.process.last_error = ERROR_MOD_NOT_FOUND;
        0
    } else {
        let library_name = if wide {
            read_wide_string_from_cpu(engine, library_name_va, MAX_PATH)?
        } else {
            read_ansi_string_from_cpu(engine, library_name_va, MAX_PATH)?
        };
        let handle = resolve_or_load_dll(&library_name, engine, environment, state);
        tracing::debug!(
            target: "wie_modules",
            library = %library_name,
            handle = format_args!("0x{handle:#x}"),
            ok = handle != 0,
            "LoadLibrary"
        );
        if handle == 0 {
            state.process.last_error = ERROR_MOD_NOT_FOUND;
        } else {
            state.process.last_error = 0;
        }
        handle
    };

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!LoadLibraryA`.
pub fn handle_load_library_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    load_library(ctx, false)
}
/// Handles `KERNEL32.dll!LoadLibraryW` — real DLL loading.
pub fn handle_load_library_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    load_library(ctx, true)
}
/// Handles `KERNEL32.dll!FreeLibrary`.
pub fn handle_free_library(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let module_handle = engine
        .read_rcx()
        .context("failed to read RCX for FreeLibrary")?;

    let return_value = if module_handle == 0 {
        state.process.last_error = ERROR_INVALID_HANDLE;
        0
    } else if module_handle >= dll_loader::REAL_MODULE_HANDLE_BASE {
        // Real loaded module — decrement refcount.
        let Some(name) = state
            .module_state
            .loaded_modules
            .iter()
            .find(|(_, m)| m.handle == module_handle)
            .map(|(n, _)| n.clone())
        else {
            state.process.last_error = ERROR_INVALID_HANDLE;
            return ctx.finish(0);
        };

        if let Some(module) = state.module_state.loaded_modules.get_mut(&name) {
            // Static (IAT) dependencies are pinned for the process lifetime:
            // the exe's IAT slots point into the image, so FreeLibrary must
            // not unmap it. Windows keeps the load count floored for static
            // imports; the call still succeeds (returns TRUE).
            if module.static_dependency {
                state.process.last_error = 0;
                return ctx.finish(1);
            }
            if module.ref_count > 0 {
                module.ref_count = module.ref_count.saturating_sub(1);
            }
            if module.ref_count == 0 {
                // Unmap from guest memory via virtual_protect (PAGE_NOACCESS).
                let _unused = engine.virtual_protect(
                    module.image_base,
                    module.image_size,
                    wie_cpu::protect::PAGE_NOACCESS,
                );
                // Evict GetProcAddress cache entries for this module.
                state
                    .module_state
                    .get_proc_address_cache
                    .retain(|_, entry| entry.module_handle != module_handle);
                state.module_state.loaded_modules.remove(&name);
            }
        }
        state.process.last_error = 0;
        1
    } else {
        // Fake module handle — always succeed (legacy behavior).
        state.process.last_error = 0;
        1
    };

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!GetProcAddress`.
pub fn handle_get_proc_address(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let module_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetProcAddress")?;

    let proc_name_va = engine
        .read_rdx()
        .context("failed to read RDX for GetProcAddress")?;

    // Microsoft Learn: if `lpProcName` is an ordinal, the high bits are zero
    // (MAKEINTRESOURCE). We treat small pointers as ordinals.
    let (proc_name, is_ordinal) = if proc_name_va <= 0xffff {
        (format!("ORDINAL {proc_name_va:#x}"), true)
    } else {
        (
            read_guest_ansi_lossy(engine, proc_name_va, 256)
                .context("failed to read GetProcAddress proc name")?,
            false,
        )
    };

    let name_key = proc_name.to_ascii_lowercase();

    // Check cache first. The hit address is extracted before the tail so the
    // `state` reborrow is dead by the time `ctx.finish` reborrows `ctx`.
    let cached_hit = state
        .module_state
        .get_proc_address_cache
        .get_mut(&name_key)
        .map(|cached| {
            cached.hit_count = cached.hit_count.saturating_add(1);
            cached.address
        });
    if let Some(address) = cached_hit {
        state.process.last_error = 0;
        return ctx.finish(address);
    }

    // For real loaded module handles, search the export table.
    if module_handle >= dll_loader::REAL_MODULE_HANDLE_BASE {
        if let Some(module) = state
            .module_state
            .loaded_modules
            .values()
            .find(|m| m.handle == module_handle)
        {
            let va = if is_ordinal {
                let ordinal = u16::try_from(proc_name_va).unwrap_or(0);
                module.get_export_va_by_ordinal(ordinal)
            } else {
                module.get_export_va(&proc_name)
            };

            if let Some(address) = va {
                // Cache and return.
                state.module_state.get_proc_address_cache.insert(
                    name_key,
                    crate::GetProcAddressCacheEntry {
                        name: proc_name.clone().into(),
                        module_handle,
                        address,
                        hit_count: 1,
                    },
                );
                state.process.last_error = 0;
                return ctx.finish(address);
            }
        }
        state.process.last_error = ERROR_PROC_NOT_FOUND;
        return ctx.finish(0);
    }

    // Fall back to existing fake-API resolution for fake module handles.
    if let Some(address) = crate::dynamic_apis::resolve_get_proc_address(&proc_name) {
        state.module_state.get_proc_address_cache.insert(
            name_key,
            crate::GetProcAddressCacheEntry {
                name: proc_name.clone().into(),
                module_handle,
                address,
                hit_count: 1,
            },
        );
        state.process.last_error = 0;
        return ctx.finish(address);
    }

    state.process.last_error = ERROR_PROC_NOT_FOUND;
    ctx.finish(0)
}
/// Shared body of `LoadLibraryExA` / `LoadLibraryExW` — real DLL loading.
/// The file handle and flags arguments are accepted and ignored.
fn load_library_ex(ctx: &mut HandlerContext<'_>, wide: bool) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let environment = ctx.environment;
    let library_name_va = engine
        .read_rcx()
        .context("failed to read RCX for LoadLibraryEx")?;

    let _file_handle = engine
        .read_rdx()
        .context("failed to read RDX for LoadLibraryEx")?;

    let _flags = engine
        .read_r8()
        .context("failed to read R8 for LoadLibraryEx")?;

    let library_name = if wide {
        read_wide_string_from_cpu(engine, library_name_va, MAX_PATH)?
    } else {
        read_ansi_string_from_cpu(engine, library_name_va, MAX_PATH)?
    };
    let return_value = resolve_or_load_dll(&library_name, engine, environment, state);

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!LoadLibraryExA`.
pub fn handle_load_library_ex_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    load_library_ex(ctx, false)
}
/// Handles `KERNEL32.dll!LoadLibraryExW`.
pub fn handle_load_library_ex_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    load_library_ex(ctx, true)
}
/// Handles `KERNEL32.dll!FindResourceA`.
pub fn handle_find_resource_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _module_handle = engine
        .read_rcx()
        .context("failed to read RCX for FindResourceA")?;

    let _name = engine
        .read_rdx()
        .context("failed to read RDX for FindResourceA")?;

    let _resource_type = engine
        .read_r8()
        .context("failed to read R8 for FindResourceA")?;

    let record = create_fake_resource_record(engine, state)?;

    ctx.finish(record.handle)
}
/// Handles `KERNEL32.dll!LoadResource`.
pub fn handle_load_resource(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _module_handle = engine
        .read_rcx()
        .context("failed to read RCX for LoadResource")?;

    let resource_handle = engine
        .read_rdx()
        .context("failed to read RDX for LoadResource")?;

    let return_value = find_resource_by_handle(state, resource_handle)
        .map_or(0, |resource| resource.loaded_handle);

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!LockResource`.
pub fn handle_lock_resource(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let resource_handle = engine
        .read_rcx()
        .context("failed to read RCX for LockResource")?;

    let return_value =
        find_resource_by_handle(state, resource_handle).map_or(0, |resource| resource.data_ptr);

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!SizeofResource`.
pub fn handle_sizeof_resource(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _module_handle = engine
        .read_rcx()
        .context("failed to read RCX for SizeofResource")?;

    let resource_handle = engine
        .read_rdx()
        .context("failed to read RDX for SizeofResource")?;

    let return_value = find_resource_by_handle(state, resource_handle)
        .map_or(0, |resource| u64::from(resource.size));

    ctx.finish(return_value)
}

/// `GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS` (libloaderapi.h) — resolve the
/// module containing `lpModuleName`.
const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u64 = 0x4;
/// `GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT` — do not bump the refcount.
const GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT: u64 = 0x2;
/// `GET_MODULE_HANDLE_EX_FLAG_PIN` — pin the module in memory.
const GET_MODULE_HANDLE_EX_FLAG_PIN: u64 = 0x1;

/// Module base + name for the module containing a guest address (main image
/// or a loaded DLL), if any.
pub(crate) fn module_containing(
    state: &WinApiState,
    environment: crate::WinApiEnvironment,
    addr: u64,
) -> Option<(u64, String)> {
    // Main module.
    let main_size = u64::try_from(state.file_io.executable_file_bytes.len()).unwrap_or(0);
    let main_end = environment.image_base.saturating_add(main_size);
    if addr >= environment.image_base && addr < main_end {
        return Some((
            environment.image_base,
            state.process.main_module_file_name.clone(),
        ));
    }
    // Loaded guest DLLs.
    for module in state.module_state.loaded_modules.values() {
        let end = module
            .image_base
            .saturating_add(u64::try_from(module.image_size).unwrap_or(0));
        if addr >= module.image_base && addr < end {
            return Some((module.image_base, module.name.clone()));
        }
    }
    None
}
/// Handles `KERNEL32.dll!GetModuleHandleExW`.
///
/// Supports `GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS` (resolve the module
/// containing `lpModuleName`); otherwise behaves like `GetModuleHandleW` and
/// writes the handle to `phModule`.
pub fn handle_get_module_handle_ex_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let environment = ctx.environment;
    let flags = engine
        .read_rcx()
        .context("failed to read RCX for GetModuleHandleExW")?;
    let module_name_va = engine
        .read_rdx()
        .context("failed to read RDX for GetModuleHandleExW")?;
    let out_va = engine
        .read_r8()
        .context("failed to read R8 for GetModuleHandleExW")?;

    let handle = if flags & GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS != 0 {
        if module_name_va == 0 {
            state.process.last_error = ERROR_INVALID_PARAMETER;
            return ctx.finish(0);
        }
        module_containing(state, environment, module_name_va)
            .map(|(base, _)| base)
            .unwrap_or(0)
    } else if module_name_va == 0 {
        environment.image_base
    } else {
        let name = read_wide_string_from_cpu(engine, module_name_va, MAX_PATH)?;
        resolve_loaded_module_handle(&name, environment.image_base, state)
    };

    let _unused = GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT | GET_MODULE_HANDLE_EX_FLAG_PIN;
    if handle == 0 {
        state.process.last_error = ERROR_MOD_NOT_FOUND;
        return ctx.finish(0);
    }
    if out_va != 0 {
        crate::guest_memory::write_u64(engine, out_va, handle)?;
    }
    state.process.last_error = 0;
    ctx.finish(1)
}
/// Handles `KERNEL32.dll!GetProcessId`.
///
/// Returns the pid of the process object; the `(HANDLE)-1` pseudohandle maps
/// to the current (fake) process id.
pub fn handle_get_process_id(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine
        .read_rcx()
        .context("failed to read RCX for GetProcessId")?;
    if handle == u64::MAX {
        return ctx.finish(super::FAKE_CURRENT_PROCESS_ID);
    }
    let pid = match state.kernel.sync.object(handle) {
        Some(crate::KernelObject::Process(p)) => u64::from(p.pid),
        _ => {
            state.process.last_error = ERROR_INVALID_HANDLE;
            return ctx.finish(0);
        }
    };
    state.process.last_error = 0;
    ctx.finish(pid)
}
/// Handles `KERNEL32.dll!RtlPcToFileHeader`.
///
/// Writes the base address of the module containing `pcValue` to
/// `*pBaseOfDll` and returns it (NULL when no module contains the address).
pub fn handle_rtl_pc_to_file_header(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let environment = ctx.environment;
    let pc_value = engine
        .read_rcx()
        .context("failed to read RCX for RtlPcToFileHeader")?;
    let out_va = engine
        .read_rdx()
        .context("failed to read RDX for RtlPcToFileHeader")?;
    let Some((base, _)) = module_containing(state, environment, pc_value) else {
        if out_va != 0 {
            crate::guest_memory::write_u64(engine, out_va, 0)?;
        }
        return ctx.finish(0);
    };
    if out_va != 0 {
        crate::guest_memory::write_u64(engine, out_va, base)?;
    }
    ctx.finish(base)
}
/// Handles `KERNEL32.dll!RtlLookupFunctionEntry`.
///
/// Looks up the `RUNTIME_FUNCTION` covering `controlPc` in the registered
/// `.pdata` tables; writes the module base to `*imageBase` and returns the
/// function entry's begin VA (NULL when not found).
pub fn handle_rtl_lookup_function_entry(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let control_pc = engine
        .read_rcx()
        .context("failed to read RCX for RtlLookupFunctionEntry")?;
    let image_base_out = engine
        .read_rdx()
        .context("failed to read RDX for RtlLookupFunctionEntry")?;
    let _history_table = engine.read_r8()?;

    let Some(found) = crate::exception::lookup_function_entry(&state.kernel.sync, control_pc)
    else {
        if image_base_out != 0 {
            crate::guest_memory::write_u64(engine, image_base_out, 0)?;
        }
        return ctx.finish(0);
    };
    let module_base = found.image_base;
    let entry_va = found.entry.begin_va(found.image_base);
    if image_base_out != 0 {
        crate::guest_memory::write_u64(engine, image_base_out, module_base)?;
    }
    ctx.finish(entry_va)
}
/// Handles `KERNEL32.dll!RtlUnwind` — same forced-unwind machinery as
/// `RtlUnwindEx` (the extra history-table argument is absent here).
pub fn handle_rtl_unwind(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let target_frame = engine
        .read_rcx()
        .context("failed to read RCX for RtlUnwind")?;
    let target_ip = engine
        .read_rdx()
        .context("failed to read RDX for RtlUnwind")?;
    let _exception_record = engine.read_r8()?;
    let return_value = engine
        .read_r9()
        .context("failed to read R9 for RtlUnwind")?;
    crate::seh::forced_unwind_to(
        engine,
        state,
        target_ip,
        (target_frame != 0).then_some(target_frame),
        return_value,
    )
}
/// Handles `KERNEL32.dll!RtlVirtualUnwind`.
///
/// Minimal stub: reports "no unwind info" (`UNW_FLAG_NHANDLER`-style) by
/// zeroing the output frame and returning NULL — the common probe path checks
/// the function-entry return for NULL before unwinding further.
pub fn handle_rtl_virtual_unwind(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // ARGUMENTS: dwHandlerType (rcx), ip (rdx), controlPc (r8), context (r9),
    // frame (stack+0x28), contextSwitch (stack+0x30)
    let _handler_type = engine.read_rcx()?;
    let _base_address = engine.read_rdx()?;
    let _control_pc = engine.read_r8()?;
    let context_va = engine.read_r9()?;
    let rsp = engine.read_rsp()?;
    let frame_va = rsp.wrapping_add(0x28);
    if context_va != 0 {
        // Tidy a KNONVOLATILE_CONTEXT-shaped frame: zero the RSP-slot fields
        // the caller reads after a NULL return (best-effort).
        let _unused = engine.mem_write(context_va.wrapping_add(0x98), &rsp.to_le_bytes());
    }
    if frame_va != 0 {
        let _unused = engine.mem_write(frame_va, &0_u64.to_le_bytes());
    }
    ctx.finish(0)
}

/// Win32 `ERROR_RESOURCE_DATA_NOT_FOUND` — the module has no enumerable
/// resource-name table.
const ERROR_RESOURCE_DATA_NOT_FOUND: u32 = 1812;

/// Handles `KERNEL32.dll!EnumResourceNamesW`.
///
/// WIE does not track the module's `.rsrc` name table, so no resources are
/// enumerable: returns FALSE with `ERROR_RESOURCE_DATA_NOT_FOUND` without
/// invoking the callback.
pub fn handle_enum_resource_names_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _module = ctx.engine.read_rcx()?;
    let _resource_type = ctx.engine.read_rdx()?;
    let _callback = ctx.engine.read_r8()?;
    let _lparam = ctx.engine.read_r9()?;
    ctx.state.process.last_error = ERROR_RESOURCE_DATA_NOT_FOUND;
    ctx.finish(0)
}

#[cfg(test)]
mod tests {
    use super::resolve_windows_dll_path;

    #[test]
    fn dll_path_derives_from_main_module_dir() {
        // A non-C:\App main module: the synthesized path follows its directory.
        assert_eq!(
            resolve_windows_dll_path("x.dll", r"C:\Program Files\MyApp\main.exe", ""),
            r"C:\Program Files\MyApp\x.dll"
        );
    }

    #[test]
    fn dll_path_keeps_app_dir_when_main_module_is_under_it() {
        // Main module under C:\App: the derived (not literal) dir is C:\App.
        assert_eq!(
            resolve_windows_dll_path("x.dll", r"C:\App\myapp.exe", r"C:\App"),
            r"C:\App\x.dll"
        );
    }

    #[test]
    fn dll_path_falls_back_to_guest_cwd() {
        assert_eq!(
            resolve_windows_dll_path("x.dll", "", r"C:\work"),
            r"C:\work\x.dll"
        );
        // A bare file name has no directory component either.
        assert_eq!(
            resolve_windows_dll_path("x.dll", "main.exe", r"C:\work"),
            r"C:\work\x.dll"
        );
    }

    #[test]
    fn dll_path_last_resort_is_drive_root_not_literal_app() {
        assert_eq!(resolve_windows_dll_path("x.dll", "", ""), r"C:\x.dll");
    }

    #[test]
    fn dll_path_passes_qualified_names_through() {
        assert_eq!(
            resolve_windows_dll_path(r"C:\sys\x.dll", "", ""),
            r"C:\sys\x.dll"
        );
        assert_eq!(
            resolve_windows_dll_path("x.dll", r"C:\App\main.exe", ""),
            r"C:\App\x.dll"
        );
    }

    #[test]
    fn guest_dir_of_splits_windows_paths_host_agnostically() {
        // Delegates to the canonical vfs/path.rs module (crate::vfs::guest_dir_of).
        assert_eq!(
            crate::vfs::guest_dir_of(r"C:\App\main.exe"),
            Some(r"C:\App".to_owned())
        );
        assert_eq!(
            crate::vfs::guest_dir_of(r"C:\Program Files\MyApp\main.exe"),
            Some(r"C:\Program Files\MyApp".to_owned())
        );
        // A bare drive letter maps to the drive root.
        assert_eq!(
            crate::vfs::guest_dir_of(r"C:\main.exe"),
            Some(r"C:\".to_owned())
        );
        assert_eq!(crate::vfs::guest_dir_of("main.exe"), None);
        assert_eq!(crate::vfs::guest_dir_of(""), None);
        // Forward slashes are normalized to backslashes.
        assert_eq!(
            crate::vfs::guest_dir_of(r"C:/App/main.exe"),
            Some(r"C:\App".to_owned())
        );
    }
}
