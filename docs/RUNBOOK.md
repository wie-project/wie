# WIE Runbook (quick mitigations)

One-page playbook for regressions after the foundational work and the **great cleanup** (mmap-only memory, compressed CLI). Prefer kill-switches over deep debug first.

## Identity

| Check | Command / note |
| ----- | -------------- |
| Active CPU | `WIE_CPU` unset → **jit**; `WIE_CPU=iced` for interpreter |
| Active mem | Always **mmap** arenas (`WIE_RUNTIME_PROFILE=1` → `mem_backend=mmap`) |
| Active idle | profile line `idle_policy=…` |
| CLI | `inspect` / `run` / `trace` (`run-micro` and `entry-trace` are aliases) |
| Interactive console | `wie-cli run --console <exe>` — raw-mode run for terminal games (per-key input, no Enter; terminal restored on exit) |

## Symptoms → actions

| Symptom | Try |
| ------- | --- |
| Crash / wrong reads after `VirtualProtect` / free | `WIE_MPROTECT=0`; bisect with `WIE_JIT_MEM=slow` / `WIE_CPU=iced` |
| Suspected JIT miscompile | `WIE_CPU=iced` or `WIE_JIT_MEM=slow` |
| Stale code after patch / protect | Expect `FlushInstructionCache` / X-loss inv; bisect with `WIE_JIT_CHAIN=0` |
| Idle guest burns 100% CPU (message wait) | `WIE_IDLE=park` (interactive `run --persistent` defaults to park) |
| `Sleep(n)` ignored / too fast | Ensure not forced busy: `WIE_IDLE=park` or legacy `WIE_HOST_SLEEP=1` |
| Micros suddenly slow | Avoid `WIE_IDLE=park` on suite; default micro idle is **yield** |
| Memory-heavy guest slow / high helpers | Profile with `WIE_JIT_MEM_TRACE=1`; expect pin hits on VA heaps; bisect `WIE_JIT_MEM=slow` |
| String / SIMD wrong results | `WIE_STRING_BULK=0`, `WIE_STRING_INLINE=0`, `WIE_JIT_SIMD=0` |
| TLB Neon issues on aarch64 | `WIE_TLB_NEON=0` |
| Host mprotect noise / faults | `WIE_MPROTECT=0` (SPC still enforces) |
| Heap freelist suspicion | `WIE_GUEST_HEAP=0` (host freelist only; default) |
| Hang on Wait / CS under MT | See [`docs/architecture/runtime.md`](architecture/runtime.md) (threading model); `ExitProcess` wakes waiters; check peer never signals |
| `CreateThread` fails | Unset `WIE_MT=0`; raise `WIE_MT_MAX_THREADS` (default 64) |
| Interlocked wrong | Expect host atomics via soft-translate; try `WIE_CPU=iced` |

## Regression matrix

```bash
cargo build -p wie-cli --release
./scripts/run-micro-suite.sh                 # mmap + jit (+ MT micros)
WIE_CPU=iced   ./scripts/run-micro-suite.sh
WIE_JIT_MEM=slow ./scripts/run-micro-suite.sh
WIE_JIT_MEM=pin  ./scripts/run-micro-suite.sh
```

## Profile snapshot

```bash
WIE_RUNTIME_PROFILE=1 ./target/release/wie run micro-exes/out/long_loop.exe
# expect ~100% CPU on pure loops; mem_backend=mmap

WIE_JIT_MEM_TRACE=1 WIE_RUNTIME_PROFILE=1 ./target/release/wie run real_exes/7za.exe -- …
# mem_path helpers=… resolve: sticky= multi= pin= walk= …
```

## Environment knobs (full table)

The README keeps the shortlist; the complete set lives here.

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
| `WIE_ROOT` / `--root` | Optional bottle override for guest `C:\` file APIs (default: per-user app-data bottle at `~/Library/Application Support/WIE/bottle`, created on demand) |
| `WIE_DRIVE_D` / `--drive-d` | Host root for guest `D:\` bridge (`auto` = host cwd) |
| `WIE_PRINT_TO=<dir>` | EndDoc headless oracle: write rendered pages as `page-N.bmp` (no bridge needed) |
| `WIE_GUEST_HEAP=1` | Rewire process-heap `HeapAlloc`/`HeapFree` to guest code |
| `WIE_GUEST_IO=0` \| `all` | I/O accelerator: default seeks/size in-guest; `all` also guest Read |
| `WIE_GUEST_MBWC=1` | Guest MultiByte↔WideChar helpers |
| `WIE_IDLE=busy\|yield\|park` | Host idle policy: micros default **yield**; interactive default **park** |
| `WIE_IDLE_PARK_MS` / `WIE_IDLE_MAX_PARKS` | Message-park quantum / cap |
| `WIE_HOST_SLEEP=1` | **Legacy:** `Sleep(n>0)` park only |
| `WIE_MT=0` | Disable guest worker spawn |
| `WIE_MT_MAX_THREADS` | Cap on guest worker threads (default **64**) |
| `RUST_LOG` | tracing filter (CLI defaults to `warn`) |

## Docs map

| Topic | Doc |
| ----- | --- |
| Multithreading | [`docs/architecture/runtime.md`](architecture/runtime.md) (threading model) |
| Live progress log | [`../.slim/deepwork/gui-implementation.md`](../.slim/deepwork/gui-implementation.md) |

## Non-goals of this sheet

- Wine-style identity `mmap(guest_va)` — **never** a remediation path.
- Rolling back to HashMap / hybrid storage — **removed** in the great cleanup.
- Full Windows wait / APC debugging — not modelled.
