# Status — what works today, performance, roadmap

The capability matrix, performance numbers, and the roadmap — the long-form companion to the README.

## What works today

| Capability | Status | Proof |
| --- | --- | --- |
| **Real-world GUI app** | ✅ | **Windows Notepad** (katahiromz/RNotepad): menus, Find/Replace, Go To, Time/Date, Save/Open, Print |
| **Real-world PE** | ✅ | Windows **7-Zip console** — create / list / extract `.7z` |
| **Console apps** | ✅ | `crt_hello`, heap matrix, argv/stdin, file I/O |
| **Terminal games** | ✅ | 2048, Snake (mingw-w64 builds) |
| **C++ / SEH** | ✅ | MSVC-style exceptions, hardware-fault handlers |
| **DLL loading** | ✅ | Import resolution, fake-VA hooking |
| **Multithreading** | ✅ | 1:1 threads, critical sections, events, semaphores, Interlocked, TLS |
| **GUI windows** | ✅ | Real macOS windows, WM_SIZE/PAINT/COMMAND, focus, capture |
| **Controls** | ✅ | BUTTON / STATIC / EDIT (caret, selection, row-level repaints) / LISTBOX / COMBOBOX / status bar |
| **Dialogs** | ✅ | RT_DIALOG parsing; **native macOS dialogs** for Open/Save, confirmations, Font, Page Setup |
| **Menus** | ✅ | Guest menu bars mirrored into the **macOS top bar** |
| **Printing** | ✅ | File→Print opens the **macOS print panel** (printer or Save as PDF) |
| **File access** | ✅ | Global app-data bottle by default; Open/Save dialogs read and write **anywhere on your Mac** |
| **Text** | ✅ | Real macOS system fonts (Unicode, proportional, bold/italic) |
| **Graphics (D3D9)** | 🟡 | Software renderer: Clear, triangles, textures with **full mip chains**, alpha blend, depth, **VS 2.0 + PS 2.0** (flow control, predication, relative addressing), point/line primitives, near-plane clipping. 51 of 119 `IDirect3DDevice9` methods landed (~43%) |
| **Present** | ✅ | wgpu (Metal) with dirty-region uploads |

The WinAPI surface is intentionally incomplete — many handlers are stubs sufficient for the micro-suite and engine bring-up. See [`docs/missing-winapi-handlers.md`](missing-winapi-handlers.md) for the gap list.

## Performance

Headline numbers on Apple Silicon release builds (re-measure with `WIE_RUNTIME_PROFILE=1`):

| Workload | Approx wall | Notes |
| --- | ---: | --- |
| `long_loop` (100M, JIT sticky + stack super) | **~0.25–0.30 s** | ~100% CPU by design; was ~1.4 s sticky-only, ~0.54 s hoist-only. Higher readings mean the host is loaded, not a regression |
| Short micros (`crt_hello`, heap, …) | ~15–25 ms | Init-dominated; emulation often < 1 ms |
| `long_loop` under `WIE_CPU=iced` | fails slice budget | ~11M iced steps/s; needs JIT for pure compute |

What actually burns CPU today: tight guest loops (expected ~100% under JIT), memory helpers (cold / non-pinned loads through TLB helpers), host API stops (every non-stub import), block entry/exit GPR sync, and the cold-compile tax on one-shot code.

## Roadmap — the feature list for running "everything"

The destination: Qt-class desktop apps, games, and anything linking the wider system-DLL
surface. Each pillar lists what it takes, why real apps need it, and the current state
(✅ landed · 🟡 partial · ⬜ missing). The per-DLL gap inventory lives in
[`docs/missing-winapi-handlers.md`](missing-winapi-handlers.md).

### 1. CPU / ABI fidelity — everything else sits on this

| Feature | Why apps need it | State |
| --- | --- | --- |
| SIMD breadth (SSE4.1/4.2, AVX, AVX2, BMI1/2, FMA, POPCNT, CRC32, RDRAND, AES-NI) | Modern Qt builds and game engines compile with `/arch:AVX2` and dispatch on CPUID feature bits | 🟡 SSE/SSE2-era covered; AVX+ missing |
| CPUID fidelity | Binaries branch on reported features; lying breaks the chosen code path | 🟡 |
| RDTSC / RDTSCP semantics; QPC/QPF coherence | Frame pacing, profilers, game timers | ✅ QPC/GetTickCount landed |
| x87 / MMX for older binaries | Legacy 32-bit-era PE32 payloads (32-bit apps are a non-goal, but DLL payloads appear) | 🟡 iced covers; JIT limited |
| JIT unwind metadata (`unwind_info = false` today) | C++ exceptions thrown *through* JIT-compiled frames need .pdata-equivalent info | ⬜ deliberate gap |

### 2. Win32 surface — zero-coverage DLLs ("all kinds of system DLLs")

| DLL | What real apps use it for | State |
| --- | --- | --- |
| `WS2_32` | **QtNetwork**, game multiplayer, updaters, any TCP/UDP | ⬜ 0 handlers |
| `WININET` / `URLMON` | HTTP/update checks, web content | ⬜ |
| `CRYPT32` | Certificates, code signing, hash APIs | ⬜ |
| `MSIMG32` | AlphaBlend/TransparentBlt (GDI-era UI polish) | ⬜ |
| `IMM32` | IME text input (CJK apps, Qt text fields) | ⬜ |
| `SETUPAPI` / `CFGMGR32` | Installers, device queries | ⬜ |
| `UXTHEME` | Visual styles — Qt apps call `SetWindowTheme` | 🟡 `SetWindowTheme` no-op only |
| `MSVCR71` / `MSVCP71` | Legacy CRT binaries | ⬜ |
| `DBGHELP` / `IMAGEHLP` | Crash handlers, stack walking | ⬜ |

### 3. Qt-class apps (Qt5/Qt6, Electron-class)

- **OpenGL (WGL/`opengl32`)** — Qt Quick and the Qt OpenGL backend render through it; the biggest single blocker for real Qt apps. ⬜
- **API-set forwarding** (`api-ms-win-*`) — modern Qt6 links these names; forwarding exists (7 sites). ✅
- **OLE clipboard + drag-drop** (`IDropTarget`, OLE formats) — Qt's clipboard and DnD are OLE-based, not CF_* based. 🟡
- **COM registration lookup** — `CoCreateInstance` returns `REGDB_E_CLASSNOTREG`; Qt ActiveX/QAxWidget and many frameworks need real COM servers. ⬜
- **Registry breadth** — QSettings reads/writes the hive; per-bottle persistence exists; the `RegDelete*`/security family is still missing. 🟡
- **Fonts** — `EnumFontFamiliesEx`, `AddFontResource`, font linking; Qt enumerates system fonts for its font dialogs. 🟡 text works, enumeration missing
- **`ReadDirectoryChangesW`** — `QFileSystemWatcher` (0 handlers today). ⬜
- **`CreateProcess`** — `QProcess` and every app that spawns a child (0 handlers today; today's apps must be single-process). ⬜
- **Locales** — `CompareString`, `LCMapString`, `GetLocaleInfo` for collation and string mapping. ⬜
- **Console APIs** — full `ReadConsoleInput` etc. for interactive CLIs. 🟡

### 4. Games

- **D3D9 completion** — cube/volume textures, materials/lighting, clip planes, queries, swap chains, `Reset`, `TestCooperativeLevel`, VS 3.0/PS 3.0 (51 of 119 device methods today, ≈43%). 🟡
- **D3D11 / DXGI** — modern titles. ⬜
- **GPU offload** — move rasterization to Metal compute, software renderer stays the correctness oracle. ⬜ (roadmap item)
- **Audio** — XAudio2, DirectSound, `waveOut`/winmm (0 audio output today; winmm has only `timeGetTime`). ⬜
- **Input** — XInput (controllers), DirectInput, raw input. ⬜
- **Fullscreen** — exclusive mode, `ChangeDisplaySettings`, monitor enumeration. 🟡
- **Timing/perf** — QPC present; SIMD JIT quality and frame pacing are the perf levers. 🟡

### 5. Process model & inter-process plumbing

- **`CreateProcess` + child processes** — exit codes, stdio pipes, console inheritance. ⬜
- **Named pipes / mailslots** — IPC between a parent and its children. ⬜
- **Job objects** — full `Set/QueryInformationJobObject` (create/assign landed). 🟡
- **Thread pools / fibers** — `QueueUserWorkItem`, `SwitchToFiber`. 🟡

### 6. User-facing completeness

- **Clipboard** — all formats incl. OLE (EDIT cut/copy/paste landed). 🟡
- **IME** (IMM32) — CJK input. ⬜
- **Fonts** — enumeration, `AddFontResource`, `GetGlyphOutline`. 🟡
- **Time zones / DST** — `GetTimeZoneInformation` correctness. 🟡

### 7. Ecosystem & tooling

- **Installers** (Inno Setup / NSIS) — registry, shell links, `ShellExecuteEx`, COM. 🟡
- **Manifests / SxS activation contexts** — modern apps ship manifests; resolution may matter. 🟡
- **`version.dll`** — landed (RNotepad dependency). ✅

The immediate next milestone is **Qt-class apps**: OpenGL, `CreateProcess`, winsock, and the
OLE clipboard/drag-drop stack unlock real Qt5 GUI apps; audio + D3D9 completion + GPU offload
then unlock games. Progress is tracked in the live log at `.slim/deepwork/gui-implementation.md`.

## History

Early work targeted an alternate way to run FuSoYa's Lunar Magic and used Unicorn Engine. After full init sequences proved feasible, Unicorn-specific paths were removed in favour of iced-x86 + Cranelift. The 2026 roadmap then landed the memory backend (mmap-only, soft-translate), the JIT fast paths (multi sticky, region pins, super path, SIMD, bulk strings), the GUI program (windows → controls → dialogs → menus → fonts → wgpu present), the D3D9 software renderer, a type-system-driven architecture cleanup, and the 2026-08 wave: **native macOS dialogs**, **real printing**, the **global-bottle policy** (no setup — apps just work, exes installed to `C:\Program Files\{name}\`, the Windows folder skeleton seeded), a zero-copy struct-read layer, VS 2.0 + PS 2.0 shader execution with flow control, and the pull-based repaint system.

This project uses code generated by artificial intelligence for implementation, tests, and architecture drafts — reviewed and steered by the author.
