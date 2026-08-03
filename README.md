# WIE — run 64-bit Windows user-mode apps on Apple Silicon

[![Project status](https://img.shields.io/badge/status-experimental-orange?style=flat-square)](https://github.com/Vladislav-Kalinkin/wie)
[![License](https://img.shields.io/github/license/Vladislav-Kalinkin/wie?style=flat-square)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.97+-blue?style=flat-square)](https://www.rust-lang.org/)
[![CI](https://img.shields.io/github/actions/workflow/status/Vladislav-Kalinkin/wie/ci.yml?style=flat-square)](https://github.com/Vladislav-Kalinkin/wie/actions)
[![GitHub stars](https://img.shields.io/github/stars/Vladislav-Kalinkin/wie?style=social)](https://github.com/Vladislav-Kalinkin/wie)

WIE (_Wie Is Emulator_) runs **Windows PE64 user-mode binaries** on **macOS Apple Silicon** — no Windows, no Wine, no VM. Guest x86-64 code runs through a Cranelift block JIT; Windows API calls are intercepted and handled on the host in Rust.

> [!WARNING]
> **Work In Progress:** this is an experimental research engine. GDI-era GUI apps (windows, dialogs, controls, menus, text) run today; D3D9 support covers Clear/Draw/Textures/Blend/Depth + Pixel-Shader 2.0 (Vertex-Shader 2.0 in flight). 32-bit binaries and full historical Windows compatibility are out of scope. **Modern GPU-accelerated apps are the long-run target** — the D3D9 software renderer and the wgpu/Metal present path are the first steps toward them.
>
> Pure guest compute (e.g. `long_loop`) pins a core near 100% by design — that is useful work in the JIT, not a hang. When the guest blocks on live console input, the host waits on I/O and CPU drops to ~1%.

---

## What works today

| Capability | Status | Proof |
| --- | --- | --- |
| **Console apps** | ✅ | `crt_hello`, heap matrix, argv/stdin, file I/O |
| **Real-world PE** | ✅ | Windows **7-Zip console** — create / list / extract `.7z` |
| **Terminal games** | ✅ | 2048, Snake (mingw-w64 builds) |
| **C++ / SEH** | ✅ | MSVC-style exceptions, hardware-fault handlers |
| **DLL loading** | ✅ | Import resolution, fake-VA hooking |
| **Multithreading** | ✅ | 1:1 threads, critical sections, events, semaphores, Interlocked, TLS |
| **GUI windows** | ✅ | Real macOS windows, WM_SIZE/PAINT/COMMAND, focus, capture |
| **Controls** | ✅ | BUTTON / STATIC / EDIT (caret, selection) / LISTBOX / COMBOBOX |
| **Dialogs** | ✅ | RT_DIALOG parsing, modal loops, Tab/Shift+Tab focus |
| **Menus** | ✅ | Guest menu bars mirrored into the **macOS top bar** |
| **Text** | ✅ | Real macOS system fonts (Unicode, proportional, bold/italic, glyph cache) |
| **Graphics (D3D9)** | 🟡 | Software renderer: Clear, DrawPrimitive(UP)/Indexed, **textures, alpha blend, depth, PS 2.0**; VS 2.0 in flight |
| **Present** | ✅ | wgpu (Metal) with dirty-region uploads |

The WinAPI surface is intentionally incomplete — many handlers are stubs sufficient for the micro-suite and engine bring-up. See [`docs/missing-winapi-handlers.md`](docs/missing-winapi-handlers.md) for the gap list.

## Roadmap

Modern apps don't fall off the map — they are the destination:

1. **Complete the D3D9 pipeline** — Vertex-Shader 2.0, remaining shader opcodes and flow control, full fixed-function coverage. (P5, in progress)
2. **Take rendering to the GPU** — the wgpu/Metal present path already uploads dirty regions; the next step is offloading rasterization to Metal compute, keeping the software renderer as the correctness oracle. (P5b–P5d)
3. **Widen the API surface** — newer graphics APIs, broader Win32 coverage, and the DLL ecosystem real apps link. See [`docs/missing-winapi-handlers.md`](docs/missing-winapi-handlers.md) for the gap list.

Progress is tracked in the live log at `.slim/deepwork/gui-implementation.md`.

---

## Quick start

You need an Apple Silicon Mac and a Rust toolchain. Everything else is in the repo.

```bash
# Build once (the GUI is included by default)
cargo build -p wie-cli --release

# A Windows console app
./target/release/wie-cli run micro-exes/out/crt_hello.exe
# → hello from crt

# A Windows GUI app — a real window opens on your Mac
./target/release/wie-cli run --gui micro-exes/out/gui_control.exe

# The flagship interactive demo: text input, combo box, list box,
# buttons, a working modal dialog, a menu and a timer — all live on
# one window. Type in the edit, pick a combo item, click "Dialog…" to
# open a dialog whose text comes back to the main window.
./target/release/wie-cli run --gui micro-exes/out/gui_demo.exe

# A dialog app (Tab / Shift+Tab moves focus, Esc closes)
./target/release/wie-cli run --gui micro-exes/out/gui_dialog.exe

# D3D9: a software-rendered textured, blended, depth-tested scene
./target/release/wie-cli run --gui micro-exes/out/gui_d3d9.exe
```

Interactive GUI sessions stay open until you close the window; the micro-suite drives the same binaries headlessly for CI (see the pre-PR checklist below).

Build all test binaries:

```bash
make -C micro-exes            # all micros
make -C micro-exes snake      # individual targets (snake, crt_hello, …)
```

### Real 7-Zip

WIE runs the **Windows PE64** standalone console from 7-Zip Extra (not macOS `7za`). The PE is not committed (`real_exes/` is gitignored) — download once, then:

```bash
BOTTLE=$(mktemp -d)
mkdir -p "$BOTTLE/drive_c/App"
./target/release/wie-cli run --root "$BOTTLE" real_exes/7za.exe -- a C:\App\out.7z D:\sample.txt
```

See [`docs/7zip.md`](docs/7zip.md) for the full workflow.

---

## How it works

Four ideas carry the design:

1. **Soft-translate memory, always.** Guest addresses are never host addresses. Every access goes through region tables / arenas / a TLB with software permission checks. No `mmap(addr = guest_va)`.
2. **A JIT that only compiles what matters.** Cranelift lowers hot x86-64 blocks (including common SSE2) to ARM64; anything cold or complex falls back to the iced-x86 interpreter. Blocks chain, a shadow return stack keeps control in native code, and stack-heavy loops get a block-wide "super path" — one prologue guard, then bare host loads/stores.
3. **Windows API calls are host calls.** The import table is rewritten to dense fake VAs (`0x7000_0000_0000_xxxx`); hitting one returns to the runtime, which decodes the ID (no string compare) and runs the handler. Hot APIs (GetLastError, critical sections, PID/TID, clock) get **in-guest stubs** so they never stop the JIT.
4. **GUI = host rendering.** Windows, controls, and dialogs are real host objects painted into a guest framebuffer, presented through wgpu (Metal) with dirty-region uploads. Text uses real macOS system fonts.

## Architecture

Five crates, linear dependency flow: `wie-pe` → `wie-cpu` → `wie-winapi` → `wie-runtime` → `wie-cli`.

| Crate | Role |
| --- | --- |
| `wie-pe` | PE64 parse, section map plan, IAT patching with fake API VAs, COFF → `PAGE_*` protects |
| `wie-cpu` | `JitCpu` (Cranelift x86-64→ARM64 block JIT + iced fallback) and `IcedCpu` (interpreter). Guest memory: mmap arenas, RegionTable, PageMap/VAD/software permissions, JIT TLB + region pins |
| `wie-winapi` | KERNEL32/UCRT/USER32/GDI32/D3D9 handlers, dense `WinApiId` dispatch, guest heap (24 size classes), VFS/bottle mapping, sync objects, SEH/MSVC C++ EH, D3D9 software renderer |
| `wie-runtime` | `RuntimeSession`: PE load, region layout, fake-API hooks, in-guest stubs and accelerators, run loop, TEB last-error, multithread runtime |
| `wie-cli` | `inspect` / `run` / `trace` — plus `--gui` and `--screenshot` |

Host GUI stack: **winit** (window + event loop) → **wgpu/Metal** (present, dirty-region uploads) → **muda** (macOS menu bar). The guest-side GUI model (windows/controls/dialogs/messages) lives in `wie-winapi`, painted into the same framebuffer the presenter uploads.

## Multithreading (guest threads)

WIE models **1:1 host thread ↔ guest thread**. Each guest thread runs on its own `CpuEngine`:

- **JIT** (default): each thread gets its own `JitCpu` sharing a common compiled-code cache (`Arc<JitShared>`). Threads execute guest code in parallel, serializing only on the shared WinAPI state mutex.
- **Iced** (`WIE_CPU=iced`): each thread gets its own `IcedCpu` sharing `GuestMemory` behind `Arc<RwLock<…>>`; page-table operations take the write lock, ordinary reads/writes the read lock.

When a thread parks (`WaitFor*`, contended critical section, …) it drops the WinAPI lock so other threads make progress.

| Surface | Status |
| --- | --- |
| `CreateThread` / `ExitThread` / join via `WaitForSingleObject` | OK |
| CRT `_beginthreadex` / `_endthreadex` | OK |
| `CREATE_SUSPENDED` + `ResumeThread` | OK |
| Critical sections (reenter + contended park) | OK |
| Events, semaphores, `WaitForMultipleObjects` (any/all) | OK |
| Interlocked\* (host atomics when aligned + soft-translated) | OK |
| TLS (`TlsAlloc`/`Get`/`Set`/`Free`) | OK |

Guest worker stack: **1 MiB** when `dwStackSize == 0`; host worker threads use **8 MiB**.

---

## Performance

Headline numbers on Apple Silicon release builds (re-measure with `WIE_RUNTIME_PROFILE=1`):

| Workload | Approx wall | Notes |
| --- | ---: | --- |
| `long_loop` (100M, JIT sticky + stack super) | **~0.25–0.30 s** | ~100% CPU by design; was ~1.4 s sticky-only, ~0.54 s hoist-only |
| Short micros (`crt_hello`, heap, …) | ~15–25 ms | Init-dominated; emulation often < 1 ms |
| `long_loop` under `WIE_CPU=iced` | fails slice budget | ~11M iced steps/s; needs JIT for pure compute |

What actually burns CPU today:

1. **Tight guest loops** — expected ~100% core under JIT; iced is orders of magnitude slower.
2. **Memory helpers** — cold / non-pinned loads go through TLB helpers; pure stack loops avoid this via the super path.
3. **Host API stops** — every non-stub import pays a stop; guest stubs / UCRT fast path / heap freelist cut this.
4. **Block entry/exit** — GPR sync is mandatory; XMM sync is skipped for pure GPR blocks.
5. **Cold compile tax** — the hotness threshold avoids compiling one-shot code.

## CLI

```bash
./target/release/wie-cli --help
./target/release/wie-cli run --help
```

| Command | Role |
| --- | --- |
| `inspect <pe>` | PE metadata: `--sections`, `--imports` / `--find`, `--image`, `--winapi-map` / `--out` |
| `run <pe>` | Primary gate (`ExitProcess`); `--max-api`, `--expect-code`, `--root`, `--stdin`, guest argv after `--` |
| `run <pe> --gui` | Open a real macOS window for the guest |
| `run <pe> --screenshot <png>` | Render one frame headlessly to a PNG |
| `run <pe> --persistent` | Persistent loop until yield/exit |
| `trace <pe>` | First N host API stops (`--max-api`, default 20) |

## Environment knobs

| Variable | Effect |
| --- | --- |
| `WIE_CPU=jit` \| `iced` | CPU backend (default **jit**) |
| `WIE_MPROTECT=0` | Disable optional host `mprotect` dual-protection (SPC remains on) |
| `WIE_JIT_MEM=sticky` \| `pin` \| `slow` | JIT mem lower (default **sticky** = 2-way multi sticky + stack pin) |
| `WIE_JIT_MEM_TRACE=1` | Dump helper mem-path histogram on finalize |
| `WIE_JIT_SUPER=loop` \| `0` \| `all` | Block-wide stack super path: default **loop** (self-loops only) |
| `WIE_JIT_CHAIN=0` | Disable FuncRef chaining / chain table / edge IC |
| `WIE_STRING_BULK=0` | Disable host-span bulk REP MOVS/STOS |
| `WIE_STRING_INLINE=0` | Disable inline 16–64 B REP Neon path |
| `WIE_JIT_SIMD=0` | Scalar XMM lowering (no CLIF SIMD / Neon) |
| `WIE_TLB_NEON=0` | Scalar 4-way TLB tag scan |
| `WIE_JIT_OPT=speed\|speed_and_size\|none` | Cranelift opt_level (default **speed**) |
| `WIE_JIT_VERIFY=1` | Enable Cranelift IR verifier outside tests |
| `WIE_FIXED_CLOCK=1` | Freeze the guest clock table (deterministic runs) |
| `WIE_D3D9_SCALE=2\|4\|8` | D3D9 render resolution divisor (quarter-scale = 4; Present upscales to the window) |
| `WIE_RUNTIME_PROFILE=1` | Wall/CPU%, host stops, JIT counters, `mem_backend` |
| `WIE_PROCESS_HEAP_MB` | Guest process-heap size in MiB (default **512**) |
| `WIE_API_JOURNAL=path` | Per-API journal for backend A/B diffs |
| `WIE_ROOT` / `--root` | Bottle root for guest `C:\` file APIs |
| `WIE_DRIVE_D` / `--drive-d` | Host root for guest `D:\` bridge (`auto` = host cwd) |
| `WIE_GUEST_HEAP=1` | Rewire process-heap `HeapAlloc`/`HeapFree` to guest code |
| `WIE_GUEST_IO=0` \| `all` | I/O accelerator: default seeks/size in-guest; `all` also guest Read |
| `WIE_GUEST_MBWC=1` | Guest MultiByte↔WideChar helpers |
| `WIE_IDLE=busy\|yield\|park` | Host idle policy: micros default **yield**; interactive default **park** |
| `WIE_IDLE_PARK_MS` / `WIE_IDLE_MAX_PARKS` | Message-park quantum / cap |
| `WIE_HOST_SLEEP=1` | **Legacy:** `Sleep(n>0)` park only |
| `WIE_MT=0` | Disable guest worker spawn |
| `WIE_MT_MAX_THREADS` | Cap on guest worker threads (default **64**) |
| `RUST_LOG` | tracing filter (CLI defaults to `warn`) |

## Docs

| Doc | Topic |
| --- | --- |
| [`docs/RUNBOOK.md`](docs/RUNBOOK.md) | Symptom → kill-switch playbook |
| [`docs/missing-winapi-handlers.md`](docs/missing-winapi-handlers.md) | WinAPI surface gaps |
| [`docs/mt-threads.md`](docs/mt-threads.md) | Multithreading internals |
| [`docs/7zip.md`](docs/7zip.md) / [`docs/2048.md`](docs/2048.md) / [`docs/snake.md`](docs/snake.md) | Real-app runbooks |

## History

Early work targeted an alternate way to run FuSoYa's Lunar Magic and used Unicorn Engine. After full init sequences proved feasible, Unicorn-specific paths were removed in favour of iced-x86 + Cranelift. The 2026 roadmap then landed the memory backend (mmap-only, soft-translate), the JIT fast paths (multi sticky, region pins, super path, SIMD, bulk strings), the GUI program (windows → controls → dialogs → menus → fonts → wgpu present), the D3D9 software renderer, and a type-system-driven architecture cleanup (typed handles, WinMsg, menu tree, per-kind control state).

## AI-Usage

This project uses code generated by artificial intelligence for implementation, tests, and architecture drafts. The author researches, reviews, runs tests, watches clippy/`unsafe` boundaries, and steers the product direction. Generated code is not accepted without human verification.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

### Pre-PR Checklist

```bash
./scripts/check.sh
```

Optional JIT matrix when touching memory lower / chaining:

```bash
./scripts/check-jit-matrix.sh
```

## Acknowledgments

- [@DevYatsu](https://github.com/DevYatsu) — Co-developer (Core contributions, performance optimizations, and overall development)

## License

**GNU Lesser General Public License v3.0 (LGPL-3.0)** — see [LICENSE.txt](LICENSE.txt).
