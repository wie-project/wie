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
| `long_loop` (100M, JIT sticky + stack super) | **~0.40–0.55 s** | ~100% CPU by design; was ~1.4 s sticky-only, ~0.54 s hoist-only. Higher readings mean the host is loaded, not a regression. **Raised from ~0.25–0.30 s on 2026-09-27** by the cheap-compile default (below): the shipped `opt_level=none` is ~38% slower here. `WIE_JIT_OPT=speed` restores ~0.28–0.36 s. |
| Short micros (`crt_hello`, heap, …) | ~15–25 ms | Init-dominated; emulation often < 1 ms |
| `long_loop` under `WIE_CPU=iced` | fails slice budget | ~11M iced steps/s; needs JIT for pure compute |

> **JIT compile-cost policy (measured 2026-09-27, release, Apple Silicon 8-core, host shared — load avg 7–22 throughout, so read the ratios, not the seconds).** The Cranelift `opt_level` default flipped `speed` → `none` (`WIE_JIT_OPT=speed` is the documented opt-out for compute-bound guests). 7-Zip Extra `7za.exe i --max-api 400000`, interleaved A/B, median of 4 **cold**-ledger pairs (whole cache wiped per run): `emu_ms` **3783 → 2590 (1.46x faster)**, `compile_us` **7,680,801 → 5,115,307 (1.50x less)**; median of 7 **warm** pairs: `emu_ms` **1723 → 989 (1.74x)**, `compile_us` **1,890,008 → 987,747 (1.91x)**. `eager` foreground compiles are **987 in both arms** — the count is unchanged, only the per-compile cost halves, so the win is per-compile cost, not fewer compiles. `long_loop` pays for it: **0.415 s vs 0.300 s median (~38% slower)**. Tier-up of hot blocks to `speed` is the planned recovery and is **not yet implemented**; until it lands, `long_loop` is the known cost of the default. Chain `resyncs` are **not** fixed by this change (~755 → ~707 cold; ~978 → ~980 warm): they track ledger warmth, not opt level.

> **Wave3 defaults / harness (no fabricated numbers):** `long_loop` is now pooled (`wie-cpu` JIT workers `min(3, available_parallelism()-1)` UTILITY, pooled Cranelift contexts) — compile latency overlaps guest execution. `wake→present` P95 target is **<16.6 ms** under a **40 s `WIE_RUNTIME_PROFILE=1` capture** (see ADR 0001 validation: `present_ns_last` / hand-back counters / `Arc::try_unwrap` probe; see `docs/perf-plan.md` §7). Re-measure on your machine with that harness; do not compare debug builds. Steady-state GDI/D3D9 frames are zero-alloc/zero-copy (per-HWND pooled surface via the ADR-0003 present channel's spare pool, latest-wins coalescing via `drain_pending_publishes` + region union). 64-px pitch padding is **implemented** (ADR-0001 status note, 2026-09-02; `WIE_SURFACE_PAD=0` opts out). **Wave 2 implementation is complete** (commits `3cb2370`, `22b56c7`, `7eb4c1d`, `25dba15`, `7c08ef3`): GUI D3D9 capture is default-on and render work is off the emu thread; the capture stream render thread is the **sole** GUI render-thread path (the superseded commit thread was removed 2026-09-25, and `WIE_CAPTURE_STREAM=0` remains the legacy escape hatch). The duplicate `ROOT` and PE-build harness defects were repaired earlier and are now historical fixes, not current blockers. **Wave 2 acceptance passed 2026-09-25, re-verified after the commit-path removal**: `WAVE2_BASELINE=1 ./scripts/acceptance-wave2.sh 40` exited 0 on a release build through the native logged-in GUI path with `present_enqueued=693`, `capture_frames=693`, `frames_published=1` (GDI/GL/DIB counter, not the capture denominator), `handler_ms/present=0.096` (PASS, ≤1 ms) and `present_ms=241.008` (PASS, >0); that report has no `commit_frames` key — the commit path and its profile key are deleted, so the live schema is capture-only. The old `frames_published`/CPU check was replaced by those production capture-path invariants; process `cpu%=0.0` is informational only, not a guest-thread acceptance gate. The baseline was appended at `docs/baselines/wave2-acceptance.txt` (latest row timestamp `2026-09-25T19:10:12Z`; the older `2026-09-25T17:57:50Z` row with `commit_frames=0` is frozen pre-removal evidence). JIT SSA-rflags landed (commits `3e2f785`, `53ec954`); direct-register ABI remains open because the config gate has no lowering call sites.

What actually burns CPU today: tight guest loops (expected ~100% under JIT), memory helpers (cold / non-pinned loads through TLB helpers), host API stops (every non-stub import), block entry/exit GPR sync, and the cold-compile tax on one-shot code. Wave3 makes padded surface pool, latest-wins throttling, pre-reserve, size-class TLS, and CompactString the default (no env required for steady zero-alloc); JIT direct regs remains **unbuilt** (ADR-0002 status note) and its inert `WIE_JIT_DIRECT_REGS` env gate was removed 2026-09-25, as was the unusable `WIE_JIT_TAILCHAIN` tail-call knob — see `docs/implementation-plan.md` for the sequenced plan and current status.

## Current wave close-out

- **Wave 2 implementation:** ✅ complete in September (`3cb2370`, `22b56c7`, `7eb4c1d`, `25dba15`, `7c08ef3`). The D3D9 capture stream render thread is the sole GUI render-thread path; headless/CI retain the legacy in-handler raster path, which `WIE_CAPTURE_STREAM=0` also selects explicitly.
- **Wave 2 acceptance:** ✅ passed 2026-09-25, re-verified after the commit-path removal. `WAVE2_BASELINE=1 ./scripts/acceptance-wave2.sh 40` exited 0 on a release build through the native logged-in GUI path: `present_enqueued=693`, `capture_frames=693`, `frames_published=1` (GDI/GL/DIB counter, not the capture denominator), `handler_ms/present=0.096` (PASS, ≤1 ms), `present_ms=241.008` (PASS, >0). That report has no `commit_frames` key — the commit path and its profile key are deleted, and the live acceptance schema is capture-only. Process `cpu%=0.0` is informational only, not a guest-thread acceptance gate. Baseline appended at `docs/baselines/wave2-acceptance.txt` (latest row timestamp `2026-09-25T19:10:12Z`); the older `2026-09-25T17:57:50Z` row with `commit_frames=0` is kept as frozen pre-removal evidence.
- **Wave 3:** ✅ SSA-rflags landed (`3e2f785`, `53ec954`); 🔴 direct-register ABI and direct chaining remain open. The inert `WIE_JIT_DIRECT_REGS` and `WIE_JIT_TAILCHAIN` gates were removed 2026-09-25 (neither had a working implementation behind them), so no live knob controls these open items.
- **Wave 4:** ✅ degrade-not-die, scalar x87, and broad packed integer/SSE lowering landed; the next ISA family is selected from opcode/stop histograms, not assumed. The `insn_coverage` denominator is now **exact** (`basis=dynamic_retired`): a per-block trip counter emitted by the lowering step plus a cross-engine `JitStats` merge replaced the old block-entry ratio, and the `UNRELIABLE` label is gone. `long_loop` reports 1,100,000,014 retired instructions (was 25) and `cpp_threads`' `iced` matches the exact `WIE_EXEC_TRACE` count to the instruction. Only a REP string helper still undercounts (1 instruction, not `rcx` iterations).
- **Wave 5:** ✅ waveOut playback/timed completion and `CALLBACK_WINDOW`/`CALLBACK_EVENT` routing landed; ✅ per-thread multi-queue message pump landed 2026-09-26 (`present/queue.rs` `MessageQueues`, `WindowRecord.owner_tid`, real `PostThreadMessageW`); ✅ RawInput landed 2026-09-26 (synthesized payloads, two views of one buffered input); ✅ DirectInput8 keyboard + mouse shim landed 2026-09-26; ⬜ inter-thread `SendMessage` marshaling, `HWND_BROADCAST`, per-thread wake targeting, and joysticks/force feedback remain open.
- **Wave 6:** ⬜ GPU-offload decision remains open and gated on measured frame-budget headroom.

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
- **GPU offload** — move rasterization to Metal compute, software renderer stays the correctness oracle. ⬜ decision open; gated on measured frame-budget headroom.
- **Audio** — XAudio2 and DirectSound remain absent; `waveOut`/winmm now has a bounded playback sink, timed `WOM_DONE` delivery, and window/event callback routing, but still has no host audio output. 🟡
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

- **DirectInput8** (`dinput8.dll`, landed 2026-09-26): keyboard + mouse compatibility shim over the existing input state. Two devices, real guest COM vtables (11-slot `IDirectInput8`, 32-slot `IDirectInputDevice8`), `GetDeviceState` honouring the offsets the guest declared in its own `DIDATAFORMAT`. Joysticks, force feedback, HID, action maps and buffered event calls fail with a specific `HRESULT`. **Mouse buttons and the wheel are real, from one source of truth.** `GetDeviceState` reports `rgbButtons[0..4]` from the host's live `MK_*` mask and `lZ` from accumulated wheel notches (120 per notch, consumed on read like `lX`/`lY`), carried by the presenter-side window mirror — the same lock-free seam as `set_key_state`/`set_cursor_pos`, so no input event takes the big guest lock. `GetKeyState`, `GetAsyncKeyState` and `GetKeyboardState` project the five mouse rows from that same mask at read time rather than reading `KeyboardState`, which the host never wrote for mouse VKs and could therefore only ever be a permanently-wrong second source. The VK↔MK mapping lives in exactly one place (`state::MOUSE_BUTTON_SLOTS`: VK code, `MK_*` bit, `rgbButtons` index) because Win32 has two spellings of "is the left button down" and they must not drift. `DIDEVCAPS::dwAxes` reports 3 for the mouse, since the wheel is a real third relative axis. Buttons are level state; the wheel is relative; button state is reported regardless of `SetCapture`, matching Windows (capture routes messages, not device state). `SDL_DIRECTINPUT_ENABLED=0` stays.
- **RawInput** (landed 2026-09-26): real API surface plus `WM_INPUT` delivery, replacing four fake stubs. Two views of one buffered input — a fake-`HRAWINPUT` map that is never consumed, and a FIFO that `GetRawInputBuffer` drains — so a drain cannot invalidate an `lParam` a guest still holds. Payloads are synthesized (macOS exposes no raw HID stream), and `GetRawInputBuffer` is process-wide rather than per-thread. Both deviations are documented at the module.

The immediate next milestone is **Qt-class apps**: OpenGL, `CreateProcess`, winsock, and the
OLE clipboard/drag-drop stack unlock real Qt5 GUI apps; audio + D3D9 completion + GPU offload
then unlock games. Progress is tracked in the live log at `.slim/deepwork/gui-implementation.md`.

## History

Early work targeted an alternate way to run FuSoYa's Lunar Magic and used Unicorn Engine. After full init sequences proved feasible, Unicorn-specific paths were removed in favour of iced-x86 + Cranelift. The 2026 roadmap then landed the memory backend (mmap-only, soft-translate), the JIT fast paths (multi sticky, region pins, super path, SIMD, bulk strings), the GUI program (windows → controls → dialogs → menus → fonts → wgpu present), the D3D9 software renderer, a type-system-driven architecture cleanup, and the 2026-08 wave: **native macOS dialogs**, **real printing**, the **global-bottle policy** (no setup — apps just work, exes installed to `C:\Program Files\{name}\`, the Windows folder skeleton seeded), a zero-copy struct-read layer, VS 2.0 + PS 2.0 shader execution with flow control, and the pull-based repaint system.

This project uses code generated by artificial intelligence for implementation, tests, and architecture drafts — reviewed and steered by the author.
