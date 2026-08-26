# WIE Runbook (quick mitigations)

One-page playbook for regressions after the foundational work and the **great cleanup** (mmap-only memory, compressed CLI). Prefer kill-switches over deep debug first.

## Identity

| Check | Command / note |
| ----- | -------------- |
| Active CPU | `WIE_CPU` unset → **jit**; `WIE_CPU=iced` for interpreter |
| Active mem | Always **mmap** arenas (`WIE_RUNTIME_PROFILE=1` → `mem_backend=mmap`) |
| Active idle | profile line `idle_policy=…` |
| CLI | `inspect` / `run` / `trace` / `bottle` (`run-micro` and `entry-trace` are aliases) |
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
# expect ~100% CPU on pure loops; mem_backend=mmap; mem_path helpers=… resolve: sticky= multi= pin= walk= …
```

With `WIE_JIT_OPCODE_HISTO=1` the profile block also ends with the sampled
iced-residue opcode histogram (top 60 mnemonics) and, when anything was
recorded, the `[wie] jit_bg_ledger:` promotion-outcome line. Both are part of
the report itself — they print on SIGINT (`kill -INT`) exactly like the rest,
no `RUST_LOG` needed. Capture long-running targets only: micro-exes exit in
milliseconds and the SIGINT path never arms.

```bash
WIE_JIT_OPCODE_HISTO=1 WIE_RUNTIME_PROFILE=1 ./target/release/wie run real_exes/7za.exe -- b &
sleep 15 && kill -INT %1   # histogram lands at the end of the profile dump
```

## Benchmarks & regression gate

Criterion suites live in `crates/wie-cpu/benches/jit_hot_paths.rs`
(`exec/*`, `compile/*`, `mem/*`). Workflow:

```bash
./scripts/bench-gate.sh save main          # on a known-good tree
# …make changes…
./scripts/bench-gate.sh check main         # exit 1 if median regressed >10%
./scripts/bench-gate.sh check main 5 exec/ # tighter gate, one group only
```

Baselines are per-machine (stored under `target/criterion/`, never committed).
The nightly CI job (`.github/workflows/nightly-bench.yml`) runs the same
suites for trend visibility; whole-program timing stays with
`scripts/run-micro-suite.sh` (`long_loop` is the canonical insn-rate probe).

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
| `WIE_JIT_HOTNESS_THRESHOLD` | **Experimental** fixed hotness threshold override (default **100**; clamped to `[1, 1_000_000]`) |
| `WIE_JIT_WORKERS` | Background compile worker count (default ≈ `available_parallelism()/2`, clamped `[1, 4]`). More workers cut boot-time compile latency on multicore hosts; workers compete with guest threads for cores while active. |
| `WIE_JIT_VERIFY=1` | Enable Cranelift IR verifier outside tests |
| `WIE_FIXED_CLOCK=1` | Freeze the guest clock table (deterministic runs) |
| `WIE_D3D9_SCALE=2\|4\|8` | D3D9 render resolution divisor (quarter-scale = 4; Present upscales to the window) |
| `WIE_RUNTIME_PROFILE=1` | Wall/CPU%, host stops, JIT counters, `mem_backend`. While armed, **Ctrl+C** stops the session cleanly at the next quantum/API-stop boundary, dumps the report to stderr and exits 130; guest Ctrl+C delivery is suppressed while profiling |
| `WIE_PROCESS_HEAP_MB` | Guest process-heap size in MiB (default **512**) |
| `WIE_API_JOURNAL=path` | Per-API journal for backend A/B diffs |
| `WIE_ROOT` / `--root` | Optional bottle override for guest `C:\` file APIs (default: per-user app-data bottle at `~/Library/Application Support/WIE/bottle`, created on demand) |
| `WIE_DRIVE_D` / `--drive-d` | Host root for guest `D:\` bridge (`auto` = host cwd) |
| `WIE_PRINT_TO=<dir>` | EndDoc headless oracle: write rendered pages as `page-N.bmp` (no bridge needed) |
| `WIE_GUEST_HEAP=1` | Rewire process-heap `HeapAlloc`/`HeapFree` to guest code |
| `WIE_GUEST_IO=0` \| `all` | I/O accelerator: default seeks/size in-guest; `all` also guest Read |
| `WIE_GUEST_MBWC=1` | Guest MultiByte↔WideChar helpers |
| `WIE_IDLE=busy\|yield\|park` | Host idle policy: micros default **yield**; interactive default **park** |
| `WIE_IDLE_PARK_MS` | Message-park quantum (sleep-poll paths only; persistent/GUI idle parks are event-driven) |
| `WIE_HOST_SLEEP=1` | **Legacy:** `Sleep(n>0)` park only |
| `WIE_API_TRACE=1` | Full API dump (head+tail only by default) |
| `WIE_INPUT_SCRIPT=<path>` | GUI input script path |
| `WIE_VFS_DOWNLOADS=1` | Optional live check against real macOS user dir |
| `WIE_MT=0` | Disable guest worker spawn |
| `WIE_MT_MAX_THREADS` | Cap on guest worker threads (default **64**) |
| `RUST_LOG` | tracing filter (CLI defaults to `warn`) |

## Bottles

Named bottles live under `~/Library/Application Support/WIE/bottles/<name>/`
(each with a `drive_c/` — the guest `C:\` volume; the default single bottle
at `WIE/bottle` is unchanged). Manage them with:

    wie bottle create <name>                 # new empty bottle
    wie bottle list                          # existing bottles
    wie bottle info <name>                   # path, drive_c, size
    wie bottle path <name>                   # host root (for --root scripting)
    wie bottle delete <name> [--yes]         # remove a bottle
    wie bottle add <name> <host-path> [--target C:\Apps\Foo]   # copy file/folder in
    wie bottle run <name> C:\App\app.exe [args...]             # run inside the bottle

`bottle run` is `run --root <bottle>` with the exe resolved inside `drive_c`.

## Launching apps

Launch commands below use the debug build (`target/debug/wie`). `run` modes:
micro (default, until `ExitProcess`) | `--persistent` | `--console` | `--gui`
| `--screenshot`.

### Standalone executable — stages only the exe

```bash
target/debug/wie run /path/to/app.exe
```

Copies just the executable into the bottle — `{root}/drive_c/Program Files/{name}/{name}.exe`
(install-style; the source is untouched) — and starts the guest with that folder as its
current directory. Sibling files from the exe's host folder stay out.

### Full portable app folder

```bash
target/debug/wie run --app-dir /path/to/App app.exe -- relative-resource.dat
```

`--app-dir <HOST_DIR>` stages the whole folder: DLLs, plugins, data files and nested
subdirectories keep their relative paths under `C:\Program Files\{name}\`, and relative
guest args resolve against that staged folder. The run source must live inside the folder
(a miss is rejected up front). Micro / `--gui` / `--screenshot` only — `--console` /
`--persistent` reject it.

### Named bottle: create → stage → run

```bash
wie bottle create doomretro
wie bottle add doomretro /path/to/DoomRetro --target 'C:\Program Files\DoomRetro'
target/debug/wie run --bottle doomretro doomretro.exe -- C:\DOOM2.WAD
```

`bottle add` copies the host file/folder into the bottle's `drive_c` (`--target` defaults
to `C:\<basename>`). With `--bottle <name>` the exe argument may be a bare basename
(unique, case-insensitive match anywhere under `drive_c`), a `drive_c`-relative path, an
explicit guest `C:\…` path (mapped, must exist), or an existing host path (passed
through). No match is an error naming the searched exe and bottle; several basename
matches are an ambiguity error listing the candidates and suggesting a full `C:\…` path.

### GUI runs: named bottle vs explicit root

```bash
target/debug/wie run --gui --bottle doomretro doomretro.exe
target/debug/wie run --gui --root /path/to/bottle app.exe
```

`--bottle <name>` is the only root form `--console` / `--persistent` accept, and works for
every entry; `--root <PATH>` (env `WIE_ROOT`) is the explicit host-root form for the micro
/ `--gui` / `--screenshot` entries. The two are mutually exclusive. With `--gui`, every
absolute `C:` / `D:` guest argument is preflighted against the mapped volumes BEFORE the
guest thread starts — a missing file fails the launch with a clear error (naming the guest
path and its mapped host path) instead of a runtime open failure. No file dialog ever pops
for it (GetOpenFileName stays interactive); a `D:` arg with no `--drive-d` bridge is
unmappable and passes through.

### Flags

| Flag | Effect |
| --- | --- |
| `--` | Everything after it is guest argv, verbatim (`run app.exe -- -n 3 -m hi`). Micro and `--gui` accept it; `--console` / `--persistent` reject it |
| `--app-dir <dir>` | Stage a complete host folder instead of the exe-only default (above) |
| `--root <path>` | Explicit host root for guest `C:\` (env `WIE_ROOT`); micro / `--gui` / `--screenshot` only |
| `--bottle <name>` | Named bottle (guest `C:\` = `WIE/bottles/<name>/drive_c`); the only root form `--console` / `--persistent` accept; exe resolved inside `drive_c` (above) |
| `--persistent` | Message-driven loop that yields on idle instead of gating on `ExitProcess` — for message-loop guests (games, GUI apps); bounded by `--max-api` (default 5,000,000) |
| `--console` | Raw-mode interactive terminal for terminal games: per-key input (no Enter), terminal restored on exit; runs until the guest exits |

## Docs map

| Topic | Doc |
| ----- | --- |
| Multithreading | [`docs/architecture/runtime.md`](architecture/runtime.md) (threading model) |
| Live progress log | [`../.slim/deepwork/gui-implementation.md`](../.slim/deepwork/gui-implementation.md) |

## Non-goals of this sheet

- Wine-style identity `mmap(guest_va)` — **never** a remediation path.
- Rolling back to HashMap / hybrid storage — **removed** in the great cleanup.
- Full Windows wait / APC debugging — not modelled.
