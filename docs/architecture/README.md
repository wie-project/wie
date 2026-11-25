# Architecture

**WIE** runs 64-bit Windows user-mode binaries on Apple Silicon macOS. This directory provides a crate-by-crate deep dive into the system design.

## Docs

| Doc | Topic |
| --- | --- |
| [PE Loading](pe-loading.md) | PE64 parsing, resource extraction, IAT fake virtual addresses |
| [CPU & Memory](cpu-and-memory.md) | Cranelift JIT, memory mapping, TLB, software permissions |
| [WinAPI Handling](winapi-handling.md) | API handlers, guest heap, bottle VFS, SEH emulation |
| [GUI](gui.md) | winit/wgpu host, native dialogs, window procedure forwarding |
| [D3D9](d3d9.md) | Software renderer, shader pipeline, state tracking |
| [Runtime](runtime.md) | RuntimeSession, PE loading, API hooks, run loop, threading |
| [C++ Exceptions](cpp-exceptions.md) | Exception handling across the translation boundary |
| [Performance Ceiling](performance-ceiling.md) | Where the remaining time is, per real workload, with a ranked lever table and the dead ends |
| [Startup Latency](startup-latency.md) | What a console hello-world costs end to end (8.79 ms min / 9.13 ms median process wall), the phase table, why the WAD-staging win is not on the `wie run` path, and the measured cold-page-cache case behind the "sometimes 500 ms" |
| [L1: persistent frame](l1-persistent-frame.md) | The landed L1 lever: one `JitCtx` per engine instead of a 3.7 KB struct copy per dispatcher entry — invariant, ABI rules, measurement |
| [JIT code memory on Darwin](code-memory-on-darwin.md) | What the OS allows for code memory (no huge pages, no superpages, `MAP_JIT` vs entitlements), the measured per-block cost of the `mmap`/`mprotect`/`ic ivau` path, the `JITMemoryKind` non-re-export blocker, the measured code working set, and the L1I aliasing defect of page-aligned block starts |
| [Dispatcher entry, attributed](dispatcher-entry-attribution.md) | What a dispatcher entry still costs, piece by piece, after L1 — and which pieces can and cannot be removed |
| [Tier 0: a single-pass emitter](tier0-emitter.md) | The plan for the entry-arm emitter: what the census says it must cover (58 % ceiling without call terminators, memory operands first), the correctness plan, and the open questions — no codegen yet |

## Design Overview

Three translation layers carry the design:

1. **PE → Memory** (`pe-loading.md`): Windows PE64 is parsed into a `MEM_IMAGE` arena; every import is rewritten to a fake VA in the `0x7000…` range.
2. **CPU** (`cpu-and-memory.md`): Hot x86-64 blocks lower to ARM64 via Cranelift (complex/cold code falls through an iced-x86 interpreter).
3. **WinAPI** (`winapi-handling.md`): Hitting a fake VA returns to the runtime, which decodes the dense API ID and dispatches to a Rust handler.

Guest addresses are **never** host addresses — every access soft-translates through region tables and software permission checks.

## Crates

```mermaid
flowchart LR
    A["wie-pe"] --> B["wie-cpu"]
    B --> C["wie-winapi"]
    C --> D["wie-runtime"]
    D --> E["wie-cli"]
```

| Crate | Role |
| --- | --- |
| `wie-pe` | PE64 parse + resource parsing (dialogs, menus, strings) |
| `wie-cpu` | Cranelift block JIT + iced interpreter; guest memory (mmap arenas, TLB, software perms) |
| `wie-winapi` | Windows API handlers (KERNEL32/USER32/GDI32/D3D9/comdlg32), guest heap, bottle VFS, SEH |
| `wie-runtime` | `RuntimeSession`: PE load, fake-API hooks, guest stubs, the run loop, threading |
| `wie-cli` | `inspect` / `run` / `trace`; the winit/wgpu GUI host, native dialogs |