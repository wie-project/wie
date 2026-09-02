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

> **Wave3 defaults / harness (no fabricated numbers):** `long_loop` is now pooled (`wie-cpu` JIT workers `min(3, available_parallelism()-1)` UTILITY, pooled Cranelift contexts) — compile latency overlaps guest execution. `wake→present` P95 target is **<16.6 ms** under a **40 s `WIE_RUNTIME_PROFILE=1` capture** (see ADR 0001 validation: `present_ns_last` / hand-back counters / `Arc::try_unwrap` probe; see `docs/perf-plan.md` §7). Re-measure on your machine with that harness; do not compare debug builds. Steady-state GDI/D3D9 frames are zero-alloc/zero-copy (per-HWND pooled surface via the ADR-0003 present channel's spare pool, latest-wins coalescing via `drain_pending_publishes` + region union). 64-px pitch padding is **implemented** (ADR-0001 status note, 2026-09-02; `WIE_SURFACE_PAD=0` opts out). JIT direct regs (`WIE_JIT_DIRECT_REGS`) is **still NOT implemented** — the earlier "default on" claim was drift (ADR-0002 status note, 2026-09-02; zero call sites; tracked as Wave 3 in `docs/implementation-plan.md`).

What actually burns CPU today: tight guest loops (expected ~100% under JIT), memory helpers (cold / non-pinned loads through TLB helpers), host API stops (every non-stub import), block entry/exit GPR sync, and the cold-compile tax on one-shot code. Wave3 makes padded surface pool, latest-wins throttling, pre-reserve, size-class TLS, and CompactString the default (no env required for steady zero-alloc); JIT direct regs (`WIE_JIT_DIRECT_REGS`) remains **unbuilt** (ADR-0002 status note) — see `docs/implementation-plan.md` for the sequenced plan and current status.

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
| `WS2_32` | **QtNetwork**, game multiplayer, updaters, any TCP/UDP | ✅ real loopback TCP (Tier-1 wave) |
| `WININET` / `URLMON` | HTTP/update checks, web content | ✅ host HTTP + URLDownloadToFile (Tier-3 wave) |
| `CRYPT32` | Certificates, code signing, hash APIs | ✅ real SHA-1/SHA-256 + urandom (Tier-1 wave) |
| `MSIMG32` | AlphaBlend/TransparentBlt (GDI-era UI polish) | ✅ real (Tier-1 wave) |
| `IMM32` | IME text input (CJK apps, Qt text fields) | ✅ benign no-ops (IME is a non-goal) |
| `SETUPAPI` / `CFGMGR32` | Installers, device queries | ✅ empty device enumeration (Tier-1 wave) |
| `UXTHEME` | Visual styles — Qt apps call `SetWindowTheme` | ✅ no-ops (Tier-1 wave) |
| `MSVCR71` / `MSVCP71` + `MSVCR100/110/120/140` | Legacy CRT binaries | ✅ forwarded into ucrt + `_s` family (Tier-1/3 waves) |
| `DBGHELP` / `IMAGEHLP` | Crash handlers, stack walking | ✅ minimal init/fail-graceful (Tier-1 wave) |
| `ntdll` | Nt*/Rtl* surface modern toolchains link | ✅ Nt* + Rtl* dispatch + api-set check (Tier-3 wave) |
| `opengl32` | Qt GL contexts (WGL), legacy gl* | ✅ real GL 1.1 fixed-function software renderer + GL 1.5 buffer objects + display lists + Gouraud lighting — matrices, immediate mode, client arrays, VBOs, display lists, depth + blend, GL_RGBA textures, swap publishes the rendered frame, wglGetProcAddress resolves dispatch exports (GLSL/stencil/FBO stubs remain) |

### 3. Qt-class apps (Qt5/Qt6, Electron-class)

- **OpenGL (WGL/`opengl32`)** — Qt Quick and the Qt OpenGL backend render through it; the biggest single blocker for real Qt apps. ✅ real GL 1.1 fixed-function software renderer + GL 1.5 buffer objects + display lists + Gouraud lighting + **GLSL ES 1.00 shaders** (hand-written lexer/parser/interpreter: attribute/varying/uniform, constructors, swizzles, ternary, constant-bounded for, user functions, built-ins gl_Position/gl_FragColor/gl_FragCoord/texture2D/gl_ModelViewProjectionMatrix; per-pixel varying interpolation; glGetUniformLocation/glUniform*/glUniformMatrix4fv). `gl_quad` micro renders a red quad, checkerboard texture, client-array + VBO triangles, a lit quad, a display list, a v_uv-gradient shader quad, and a texture2D shader quad — all self-verified via glReadPixels. Still stubbed: stencil, FBOs/multisample, mipmaps, dynamic loop bounds (link error), custom attribute names (link error)
- **API-set forwarding** (`api-ms-win-*`) — modern Qt6 links these names; forwarding exists (7 sites); `tests/api_sets.rs` asserts the crt-* families classify. ✅
- **OLE clipboard + drag-drop** (`IDropTarget`, OLE formats) — Qt's clipboard and DnD are OLE-based, not CF_* based. 🟡 guest-side landed (OleSet/OleGetClipboard + IDataObject + classic clipboard family); NSPasteboard host bridge + real DnD are future lanes
- **COM registration lookup** — `CoCreateInstance` returns `REGDB_E_CLASSNOTREG`; Qt ActiveX/QAxWidget and many frameworks need real COM servers. ⬜
- **Registry breadth** — QSettings reads/writes the hive; per-bottle persistence exists; the `RegDelete*`/security family is still missing. 🟡
- **Fonts** — `EnumFontFamiliesEx`, `AddFontResource`, font linking; Qt enumerates system fonts for its font dialogs. 🟡 EnumFontFamiliesExW/A + EnumFonts + AddFontResource/RemoveFontResource landed (real fontdb enumeration, full-iteration callback bridge); GetGlyphOutline GGO_BITMAP; font linking ⬜
- **`ReadDirectoryChangesW`** — `QFileSystemWatcher`. ✅ landed (notify/kqueue-backed, sync form; FindFirstChangeNotification family too)
- **`CreateProcess`** — `QProcess` and every app that spawns a child. ✅ landed (in-process child session: CreateProcessW/A, GetExitCodeProcess, OpenProcess, WaitForSingleObject on process handles; `spawn_child` micro verifies exit code 42)
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
