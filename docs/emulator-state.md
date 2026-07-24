# WIE Emulator — Current State

**WIE** (*Wie Is Emulator*) is a **Windows PE64 userspace emulator** that runs x86-64 Windows binaries on macOS Apple Silicon. It uses soft translation (guest VA ≠ host VA) — not Wine-style identity mapping.

---

## What it is

- Targets **Windows 10-era PE64** (not 32-bit, not vintage/console emulators)
- **Hybrid engine**: Cranelift block JIT (default) + iced-x86 interpreter (`WIE_CPU=iced`)
- **Runtime**: 1:1 host→guest threads, mmap-only guest memory, bottle filesystem
- **~200+ WinAPI handlers** across KERNEL32, UCRT, USER32, GDI32, ADVAPI32, COM (stub)
- **C++ exception handling**: Two-pass SEH dispatch, LSDA, MSVC FuncInfo

---

## Implemented Features

### CPU Engine

- **JIT backend** (default): Cranelift x86-64 → ARM64 block JIT
  - Block compilation (up to 64 instructions)
  - Hotness threshold (default 100 visits)
  - Block chaining + edge IC + shadow return stack
  - 2-way multi-sticky TLB + 16×4 set-associative helper TLB
  - SSE2/XMM lowering via Neon SIMD (I8X16/F32X4)
  - REP MOVS/STOS bulk string operations (inline Neon for 16–64B)
  - Fast UCRT paths: malloc, free, memcpy, strlen, fwrite, fflush
- **Interpreter backend** (`WIE_CPU=iced`): pure iced-x86
- **Memory**: mmap-only arenas, soft translate (guest VA ≠ host VA)
  - Software PageMap + VAD (MEM_FREE/RESERVE/COMMIT)
  - Optional dual mprotect on arena frames
  - Real VirtualAlloc/VirtualFree/VirtualProtect/VirtualQuery
- **Memory JIT**: sticky TLB, region pins, super path for hot code

### WinAPI Surface (~200+ handlers)

| DLL | Coverage |
|-----|----------|
| **KERNEL32** | Process/thread IDs, GetCommandLine, GetVersion, GetTickCount, QueryPerformanceCounter, GetCurrentProcess/Thread, GetProcessHeap, HeapAlloc/Free/ReAlloc/Create, CriticalSections (init/enter/leave/delete), FLS, GetStdHandle, file attributes, FindFirst/NextFile, LoadLibrary/FreeLibrary/GetProcAddress, CreateFile/ReadFile/WriteFile/CloseHandle, GetFileType, CreateThread/ExitThread, TlsAlloc/Get/Set/Free, WaitForSingleObject/MultipleObjects, Events, Semaphores, Interlocked\* operations, VirtualProtect, VirtualQuery, FlushInstructionCache, GetModuleFileName, GetStartupInfo, MultiByteToWideChar/WideCharToMultiByte, LCMapString, GetSystemTimeAsFileTime, GetLocalTime, GetTimeZoneInformation, FileTimeToLocalFileTime, FileTimeToSystemTime, SetFilePointer, GetFileSize, GetFileInformationByHandle, EncodePointer/DecodePointer, GlobalMemoryStatus, GetSystemDefaultLangId, GetUserDefaultLangId, FindResource, LoadResource, SizeofResource, LockResource |
| **USER32** | GetAsyncKeyState, PeekMessage/GetMessage, PostMessage/SendMessage, RegisterClass, MessageBox, window management (GetWindowRect, SetWindowPos, etc.), keyboard state, GetSystemMetrics, DPI functions, LoadIcon/Cursor, GetWindowLong/SetWindowLong |
| **GDI32** | SelectObject, GetDC/ReleaseDC, CreateCompatibleDC, BitBlt, TextOut, GetObject, DeleteObject, CreateFont, GetTextMetrics, etc. |
| **ADVAPI32** | Registry operations (RegCreateKey, RegOpenKey, RegQuery/SetValue, RegDeleteValue, RegCloseKey), InitializeSecurityDescriptor, SetSecurityDescriptorDacl |
| **UCRT** | malloc/free, memcpy, strlen, fwrite, fflush, \_\_acrt_iob_func, \_\_beginthreadex/\_endthreadex |
| **COM** | IUnknown, IDispatch (partial), IDirect3D9 (stub) |
| **MSVC C++ EH** | Two-pass SEH dispatch, LSDA parsing (Mingw/Itanium), MSVC FuncInfo, RtlVirtualUnwind, UnwindMap actions |
| **DLL Loader** | Real PE DLL loading from host filesystem, relocation, import resolution, DllMain call |

### Guest Features

- **Threading**: 1:1 host thread ↔ guest thread mapping, CriticalSections, Events, Semaphores, TLS, Interlocked
- **Memory**: Process heap (24 size classes + bump allocator), VirtualAlloc family
- **File I/O**: Bottle filesystem (host `drive_c/...` ↔ guest `C:\`), optional `D:\` host bridge
- **Win32 Find API**: Proper WIN32_FIND_DATA layout
- **Guest accelerators**: Optional in-guest HeapAlloc/Free, I/O, MultiByte/WideChar helpers

### CLI Commands

- `inspect <pe>` — PE metadata, sections, imports, WinAPI coverage map
- `run <pe>` — Execute with `--max-api N`, `--root bottle`, `--stdin`, `--persistent`
- `trace <pe>` — First N API stops

---

## Test Suite

**Micro-exe test suite** (`micro-exes/out/` — 38 binaries):

| Category | Tests |
|----------|-------|
| N1 (no bottle) | `process_ids`, `tls_basic`, `cs_reenter`, `thread_create_join`, `cs_two_threads`, `event_handshake`, `interlocked_basic`, `mt_stress`, `heap_alloc`, `heap_core`, `winapi_heap`, `modules`, `long_loop` (100M iteration compute), `cpu_string`, `cpu_math`, `cpu_fp`, `crt_hello` |
| CLI | `cli_args` (stdin, argv parsing) |
| N2 (bottle required) | `write_file`, `read_file`, `relative_path`, `vfs_roundtrip` |
| C++ exceptions | `cpp_throw`, `cpp_types`, `cpp_multi_catch`, `cpp_dtor`, `cpp_threads` |
| DLL tests | `dll_basic`, `dll_multi_dll`, `dll_ordinal`, `dll_refcount`, `dll_not_found` |

**Rust unit tests** in each crate (run via `cargo test`):
- `wie-cpu`: oracle tests comparing backends
- `wie-winapi`: exception_tests, exception_helpers
- `wie-runtime`: micro_n1_suite, micro_heap_alloc, micro_cli_args, micro_vfs_roundtrip, micro_n2_files

---

## Phase Roadmap (all completed ✅)

| Phase | Topic |
|-------|-------|
| 0 | Baseline measurement |
| 1 | Memory abstraction (GuestMemBackend trait, RegionTable) |
| 2 | Mmap storage backend |
| 3 | Permissions + dynamic mapping (SPC, VAD, Virtual\*) |
| 4 | JIT foundations (sticky TLB, region pins, super path, chaining) |
| 5 | Guest stubs + UCRT fast path |
| 5.5 | Neon SIMD / Cranelift flags |
| 6 | Host idle park (Sleep / empty GetMessage) |
| 7 | Hardening + mmap-only default |

---

## What's NOT implemented / gaps for running fully working applications

| Gap | Impact |
|-----|--------|
| **Incomplete WinAPI surface** | Many handlers are stubs — enough for the micro-suite but real apps hit unimplemented APIs. Win32k/GDI are sparse. |
| **No GUI** | No window server, no GPU, no DirectX — console apps only. `7zFM` (GUI 7-Zip) explicitly not claimed. |
| **Partial synchronization** | WaitForMultipleObjects, APCs, condition variables — partial coverage. |
| **No network** | Winsock, pipes, named pipes — not implemented. |
| **No COM** | Only IUnknown/IDispatch stubs. Many Win10 APIs rely on COM. |
| **No registry beyond stubs** | RegCreate/Query/Set/DeleteValue work for flat test cases, but real apps expect comprehensive registry. |
| **32-bit apps** | Explicit non-goal — only x86-64. |
| **Wine-style identity mmap** | Explicit non-goal — guest VA always soft-translates. |

---

## Current status with real applications

- Runs **freestanding micro-PEs and CRT console programs** reliably (38 passing test binaries)
- Can run **`7za.exe`** for basic operations (list, extract simple archives)
- Bottlenecks for real-world use:
  1. **Missing WinAPI handlers** that real apps call beyond the micro-suite surface
  2. **I/O path hardening** — bottle filesystem works for micro-tests but real workloads stress it
  3. **Performance tuning** — JIT/memory optimizations are phase-complete but real workloads expose new bottlenecks

**Bottom line**: Skeleton is solid (mmap, JIT, threading, SEH). The WinAPI surface is the long tail — the emulator is not yet a general Windows app runner.
