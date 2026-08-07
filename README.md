# WIE — run 64-bit Windows apps on Apple Silicon

[![Project status](https://img.shields.io/badge/status-experimental-orange?style=flat-square)](https://github.com/Vladislav-Kalinkin/wie)
[![License](https://img.shields.io/github/license/Vladislav-Kalinkin/wie?style=flat-square)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.97+-blue?style=flat-square)](https://www.rust-lang.org/)
[![CI](https://img.shields.io/github/actions/workflow/status/Vladislav-Kalinkin/wie/ci.yml?style=flat-square)](https://github.com/Vladislav-Kalinkin/wie/actions)

**WIE (_Wie Is Emulator_) runs Windows PE64 user-mode binaries on macOS Apple Silicon — no Windows, no Wine, no VM.**

Guest x86-64 code executes through a Cranelift block JIT; Windows API calls are intercepted and handled on the host in Rust. GUI apps get **real macOS windows** and **real macOS dialogs** (Open, Save, Print, Font, Page Setup), and a guest's files are real files on your Mac — you choose where they live.

> [!WARNING]
> **Experimental research engine.** Classic GUI apps (windows, controls, menus, dialogs, printing) run today; the D3D9 software renderer covers VS 2.0 + PS 2.0 with flow control, textures and mip chains, blending and depth. 32-bit binaries and full historical Windows compatibility are out of scope — **modern GPU-accelerated apps are the long-run target**.
>
> Pure guest compute (e.g. `long_loop`) pins a core near 100% by design — that is useful work in the JIT, not a hang.

---

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
| **File access** | ✅ | Guest files live in a **bottle**; Open/Save dialogs read and write **anywhere on your Mac** |
| **Text** | ✅ | Real macOS system fonts (Unicode, proportional, bold/italic) |
| **Graphics (D3D9)** | 🟡 | Software renderer: Clear, triangles, textures with **full mip chains**, alpha blend, depth, **VS 2.0 + PS 2.0** (flow control, predication, relative addressing), point/line primitives, near-plane clipping. Device API surface is still small (~5–7% of `IDirect3DDevice9`) |
| **Present** | ✅ | wgpu (Metal) with dirty-region uploads |

The WinAPI surface is intentionally incomplete — many handlers are stubs sufficient for the micro-suite and engine bring-up. See [`docs/missing-winapi-handlers.md`](docs/missing-winapi-handlers.md) for the gap list.

## Roadmap

Modern apps are the destination:

1. **Widen the D3D9 pipeline** — larger device API surface, fixed-function coverage, remaining shader details.
2. **Take rendering to the GPU** — the wgpu/Metal present path already uploads dirty regions; the next step is offloading rasterization to Metal compute, keeping the software renderer as the correctness oracle.
3. **Widen the API surface** — newer graphics APIs, broader Win32 coverage, and the DLL ecosystem real apps link.

Progress is tracked in the live log at `.slim/deepwork/gui-implementation.md`.

---

## Quick start

You need an Apple Silicon Mac and a Rust toolchain. Everything else is in the repo.

```bash
# Build (debug is the daily driver; add --release for perf runs)
cargo build -p wie-cli

# A Windows console app
./target/debug/wie-cli run micro-exes/out/crt_hello.exe
# → hello from crt

# A Windows GUI app — a real window opens on your Mac
./target/debug/wie-cli run --gui micro-exes/out/gui_control.exe

# The flagship interactive demo: text input, combo box, list box,
# buttons, a working modal dialog, a menu and a timer — all live on
# one window.
./target/debug/wie-cli run --gui micro-exes/out/gui_demo.exe

# A terminal game (raw-mode console: no Enter needed)
./target/debug/wie-cli run --console micro-exes/out/snake.exe

# D3D9: a software-rendered textured, blended, depth-tested scene
./target/debug/wie-cli run --gui micro-exes/out/gui_d3d9.exe
```

Interactive GUI sessions stay open until you close the window. Build all test binaries with `make -C micro-exes` (individual targets: `make -C micro-exes snake`).

### Windows Notepad — the flagship demo

Fetch the real Windows PE once, then run it. Its `C:` lives in the per-user app-data bottle by default; the `--root` below gives this run an isolated one:

```bash
./scripts/fetch.sh notepad     # builds katahiromz/RNotepad (needs cmake + mingw-w64)

BOTTLE=$(mktemp -d)
./target/debug/wie-cli run --gui --root "$BOTTLE" real_exes/notepad.exe
```

When you pick Save As… / Open… in the native macOS panel, the file you choose is read and written **in place on your Mac** — the bottle is only the app's private world, not a cage for your files.

### Real 7-Zip

WIE runs the **Windows PE64** standalone console from 7-Zip Extra. The PE is not committed (`real_exes/` is gitignored) — fetch it once:

```bash
./scripts/fetch.sh 7za

BOTTLE=$(mktemp -d)
mkdir -p "$BOTTLE/drive_c/App"
./target/debug/wie-cli run --root "$BOTTLE" real_exes/7za.exe -- a C:\App\out.7z D:\sample.txt
```

See [`docs/7zip.md`](docs/7zip.md) for the full workflow.

---

## Files and the bottle

Guest `C:\` maps to a **bottle** — a host directory the app can treat as its own machine:

- **Apps just work.** File operations never need setup: guest `C:\…` maps to a per-user app-data bottle at `~/Library/Application Support/WIE/bottle/drive_c/…`, created on demand the first time the app touches a file. `--root` / `WIE_ROOT` optionally override that default with a per-session bottle (tests, CI, isolation).
- **The app's own world stays in the bottle.** Its config, its CWD, its data files: `C:\…` → `{root}/drive_c/…`. An exe launched outside a configured bottle runs in place — its own file ops still land in the bottle.
- **Your files are yours.** The native Open/Save panels are the boundary: a file you pick there is mounted into the guest (`Z:\pickN\…`) and read or written **in place** — the host file is what changes.
- **The guest cannot reach your Mac on its own.** Symlink escapes and unmapped paths fail closed; the only way to a host path is a file you explicitly chose in a dialog.

Printing works the same way — a real macOS print job. For headless runs, `WIE_PRINT_TO=<dir>` dumps the rendered pages as `page-N.bmp` instead.

## How it works

Four ideas carry the design:

1. **Soft-translate memory, always.** Guest addresses are never host addresses. Every access goes through region tables / arenas / a TLB with software permission checks. No `mmap(addr = guest_va)`.
2. **A JIT that only compiles what matters.** Cranelift lowers hot x86-64 blocks (including common SSE2) to ARM64; anything cold or complex falls back to the iced-x86 interpreter. Blocks chain, a shadow return stack keeps control in native code, and stack-heavy loops get a block-wide "super path" — one prologue guard, then bare host loads/stores.
3. **Windows API calls are host calls.** The import table is rewritten to dense fake VAs (`0x7000_0000_0000_xxxx`); hitting one returns to the runtime, which decodes the ID (no string compare) and runs the handler. Hot APIs (GetLastError, critical sections, PID/TID, clock) get **in-guest stubs** so they never stop the JIT. Guest structs are read through **typed zero-copy views** (`zerocopy`) with compile-time layout asserts — struct-offset drift is a build error.
4. **GUI = host rendering + native dialogs.** Windows, controls, and dialogs are real host objects painted into a guest framebuffer, presented through wgpu (Metal) with dirty-region uploads. Text uses real macOS system fonts. OS dialogs (Open/Save/Print/Font/Page Setup/confirmations) are real macOS dialogs bridged into the guest's modal flow.

## Architecture

Five crates, linear dependency flow: `wie-pe` → `wie-cpu` → `wie-winapi` → `wie-runtime` → `wie-cli`.

| Crate | Role |
| --- | --- |
| `wie-pe` | PE64 parse, section map plan, IAT patching with fake API VAs, COFF → `PAGE_*` protects |
| `wie-cpu` | `JitCpu` (Cranelift x86-64→ARM64 block JIT + iced fallback) and `IcedCpu` (interpreter). Guest memory: mmap arenas, RegionTable, PageMap/VAD/software permissions, JIT TLB + region pins |
| `wie-winapi` | KERNEL32/UCRT/USER32/GDI32/D3D9 handlers, dense `WinApiId` dispatch, guest heap, VFS/bottle mapping + pick-mounts, sync objects, SEH/MSVC C++ EH, printing, D3D9 software renderer |
| `wie-runtime` | `RuntimeSession`: PE load, region layout, fake-API hooks, in-guest stubs and accelerators, run loop, TEB last-error, multithread runtime |
| `wie-cli` | `inspect` / `run` / `trace` — plus `--gui`, `--screenshot`, `--console` |

Host GUI stack: **winit** (window + event loop) → **wgpu/Metal** (present) → **muda** (macOS menu bar) + **rfd / objc2-app-kit** (native dialogs).

## Performance

Headline numbers on Apple Silicon release builds (re-measure with `WIE_RUNTIME_PROFILE=1`):

| Workload | Approx wall | Notes |
| --- | ---: | --- |
| `long_loop` (100M, JIT sticky + stack super) | **~0.25–0.30 s** | ~100% CPU by design; was ~1.4 s sticky-only, ~0.54 s hoist-only. Higher readings mean the host is loaded, not a regression |
| Short micros (`crt_hello`, heap, …) | ~15–25 ms | Init-dominated; emulation often < 1 ms |
| `long_loop` under `WIE_CPU=iced` | fails slice budget | ~11M iced steps/s; needs JIT for pure compute |

What actually burns CPU today: tight guest loops (expected ~100% under JIT), memory helpers (cold / non-pinned loads through TLB helpers), host API stops (every non-stub import), block entry/exit GPR sync, and the cold-compile tax on one-shot code.

## CLI

```bash
./target/debug/wie-cli --help
./target/debug/wie-cli run --help
```

| Command | Role |
| --- | --- |
| `inspect <pe>` | PE metadata: `--sections`, `--imports` / `--find`, `--image`, `--winapi-map` / `--out` |
| `run <pe>` | Primary gate (`ExitProcess`); `--max-api`, `--expect-code`, `--root`, `--stdin`, guest argv after `--` |
| `run <pe> --gui` | Open a real macOS window for the guest |
| `run <pe> --console` | Raw-mode interactive console (terminal games) |
| `run <pe> --screenshot <png>` | Render one frame headlessly to a PNG |
| `run <pe> --persistent` | Persistent loop until yield/exit |
| `trace <pe>` | First N host API stops (`--max-api`, default 20) |

## Environment knobs

The most useful ones:

| Variable | Effect |
| --- | --- |
| `WIE_CPU=jit` \| `iced` | CPU backend (default **jit**) |
| `WIE_ROOT` / `--root` | Bottle root for guest `C:\` file APIs (required for file-touching apps) |
| `WIE_DRIVE_D` / `--drive-d` | Host root for guest `D:\` bridge (`auto` = host cwd) |
| `WIE_PRINT_TO=<dir>` | EndDoc headless oracle: write rendered pages as `page-N.bmp` |
| `WIE_RUNTIME_PROFILE=1` | Wall/CPU%, host stops, JIT counters |
| `WIE_D3D9_SCALE=2\|4\|8` | D3D9 render resolution divisor (Present upscales to the window) |
| `WIE_FIXED_CLOCK=1` | Freeze the guest clock table (deterministic runs) |
| `WIE_MT=0` | Disable guest worker spawn |
| `RUST_LOG` | tracing filter (CLI defaults to `warn`) |

The full knob table (30+ switches: JIT memory lower, chaining, SIMD, strings, heap, idle policy, …) lives in [`docs/RUNBOOK.md`](docs/RUNBOOK.md).

## Docs

| Doc | Topic |
| --- | --- |
| [`docs/architecture/`](docs/architecture/) | Architecture docs (crate-by-crate) |
| [`docs/RUNBOOK.md`](docs/RUNBOOK.md) | Symptom → kill-switch playbook + full knob table |
| [`docs/missing-winapi-handlers.md`](docs/missing-winapi-handlers.md) | WinAPI surface gaps |
| [`docs/mt-threads.md`](docs/mt-threads.md) | Multithreading internals |
| [`docs/7zip.md`](docs/7zip.md) / [`docs/2048.md`](docs/2048.md) / [`docs/snake.md`](docs/snake.md) | Real-app runbooks |

## History

Early work targeted an alternate way to run FuSoYa's Lunar Magic and used Unicorn Engine. After full init sequences proved feasible, Unicorn-specific paths were removed in favour of iced-x86 + Cranelift. The 2026 roadmap then landed the memory backend (mmap-only, soft-translate), the JIT fast paths (multi sticky, region pins, super path, SIMD, bulk strings), the GUI program (windows → controls → dialogs → menus → fonts → wgpu present), the D3D9 software renderer, a type-system-driven architecture cleanup (typed handles, WinMsg, menu tree, per-kind control state), and the 2026-08 wave: **native macOS dialogs** (Open/Save anywhere via pick-mounts, confirmations, Font, Page Setup), **real printing** (the macOS print panel → GDI print DCs → NSPrintOperation), the bottle policy (global app-data bottle, per-session overrides, fail-closed isolation), a **zero-copy struct-read layer** (`zerocopy` + compile-time layout asserts), a repo-wide structure rule (no file over 1,500 lines, per-seam module splits), **VS 2.0 + PS 2.0 shader execution with flow control**, and **pull-based repaint** (content-revision latch + idle reconcile).

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

- [@DevYatsu](https://github.com/DevYatsu) — Co-developer (core contributions, performance optimizations, and overall development)

## License

**GNU Lesser General Public License v3.0 (LGPL-3.0)** — see [LICENSE.txt](LICENSE.txt).
