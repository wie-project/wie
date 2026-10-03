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
| `Sleep(n)` ignored / too fast | Ensure not forced busy: `WIE_IDLE=park` (`WIE_HOST_SLEEP` is deprecated and no longer parsed) |
| DirectInput guest sees no input / hangs | Expected: only a keyboard and a mouse exist. `EnumDevices` returns exactly two devices; `CreateDevice` for a joystick/gamepad/HID GUID fails with `DIERR_INVALIDPARAM` and `CreateEffect` with `DIERR_UNSUPPORTED` — that is the honest "absent", not a bug. **Mouse buttons and the wheel always read 0** (WIE tracks no button/scroll state), so a guest needing clicks must use the `WM_LBUTTONDOWN` path. `SDL_DIRECTINPUT_ENABLED=0` is deliberate and must stay: flipping it puts SDL2 on this shim. |
| Guest gets no `WM_INPUT` | `RegisterRawInputDevices` must match the device class (usage page 1, usage 6 = keyboard / 2 = mouse); an unregistered window gets nothing by design. `RIDEV_EXCLUDE` suppresses the legacy `WM_KEY*`/`WM_MOUSE*` messages for that class. Payloads are synthesized from WIE's keyboard/cursor state, not true hardware raw input. |
| Micros suddenly slow | Avoid `WIE_IDLE=park` on suite; default micro idle is **yield** |
| Memory-heavy guest slow / high helpers | Profile with `WIE_JIT_MEM_TRACE=1`; expect pin hits on VA heaps; bisect `WIE_JIT_MEM=slow` |
| String / SIMD wrong results | `WIE_STRING_BULK=0`, `WIE_STRING_INLINE=0`, `WIE_JIT_SIMD=0` |
| TLB Neon issues on aarch64 | `WIE_TLB_NEON=0` |
| Host mprotect noise / faults | `WIE_MPROTECT=0` (SPC still enforces) |
| Heap freelist suspicion | `WIE_GUEST_HEAP=0` (host freelist only; default) |
| Hang on Wait / CS under MT | See [`docs/architecture/runtime.md`](architecture/runtime.md) (threading model); `ExitProcess` wakes waiters; check peer never signals |
| `CreateThread` fails | Unset `WIE_MT=0`; raise `WIE_MT_MAX_THREADS` (default 64) |
| Interlocked wrong | Expect host atomics via soft-translate; try `WIE_CPU=iced` |
| Guest hangs / livelocks in a spinlock (`pthread_spin_lock`, `CRITICAL_SECTION` spin, `Interlocked*` loop), or a lock word reads `0` ("held") with no holder | **No kill switch exists for this class of bug, and `WIE_CPU=iced` does not help.** Implicitly-locked instruction semantics (`XCHG`/`CMPXCHG` with a memory operand are locked on x86-64 *without* a `LOCK` prefix) are part of the instruction contract, not a disableable subsystem: both backends execute them and there is no knob that turns atomicity off, because non-atomic execution is simply wrong. Bisect by reading the lock word directly (`[word] == 0` while every thread is spinning ⇒ corrupt, not contended). Other likely causes to rule out first: a lost `SignalObject`/event (see the `Wait` row above), or a host `Interlocked*` handler bug in `wie-winapi` — those *are* kill-switchable. |
| Tests fail with `missing guest fixture: …/micro-exes/out/*.exe` | Not an emulator bug — the untracked fixture is absent. `make -C micro-exes` (needs `brew install mingw-w64`) |
| Tests fail with `missing real guest fixture: …/real_exes/notepad.exe` | Not an emulator bug — the untracked RNotepad build is absent. `./scripts/fetch.sh notepad` (`--list` for 7za / 2048 / doomretro) |
| A test "passes" suspiciously fast (<0.1 s) in `micro_gui_window` | Vacuous-pass smell. It means the guest fixture was missing and the test returned early. Both fixture trees are gitignored; see [docs/TESTING.md](TESTING.md#prerequisites--the-guest-fixtures-must-exist) |
| Guest menus / dialogs / strings come out in a language you did not ask for | Expected by design: WIE mirrors the HOST UI language into guest resource selection (`AppleLanguages[0]` on macOS), so a German macOS gets a German notepad. Pin it with `WIE_UI_LANGID=en-US` (or `LC_ALL`/`LANG`) — see the knob table. **Never assert on localized guest text in a test**: menu titles, accelerators and the status-bar Ln/Col string are all localized, so only command *ids* are locale-independent. |

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

### Reading `insn_coverage`

`insn_coverage: total=… jit=… iced=… basis=dynamic_retired` is a **dynamic
retired-instruction** count, not the block-entry ratio it used to be: a compiled
block charges one count per self-loop trip, and guest worker-thread engines are
merged in. `long_loop` legitimately reports ~1.1e9; `cpp_threads`' `iced`
matches the exact `WIE_EXEC_TRACE` interpreter count to the instruction. Use it
to rank ISA families and to compare runs.

The one known undercount is a REP string helper (1 instruction instead of `rcx`
iterations), so treat a `rep movs*`-dominated row as a lower bound.
`insn_per_entry` is the smell detector: **high** = a hot self-loop (one entry
retiring a lot), **low** = call/edge-bound code. When a number looks wrong,
cross-check against the exact per-instruction counter rather than reasoning
about blocks:

```bash
WIE_RUNTIME_PROFILE=1 WIE_EXEC_TRACE=1 ./target/release/wie run micro-exes/out/cpp_threads.exe 2>&1 \
  | grep -E 'insn_coverage:|iced-interp mnemonic counts'
# insn_coverage: … iced=187 …
# --- iced-interp mnemonic counts (total=187) ---   ← must match
```

## Benchmarks & regression gate

Criterion suites live in `crates/wie-cpu/benches/jit_hot_paths.rs`
(`exec/*`, `compile/*`, `mem/*`). Run them with:

```bash
cargo bench -p wie-cpu                      # writes target/criterion/
```

Baselines are per-machine (stored under `target/criterion/`, never committed).
Criterion keeps the previous run's estimates itself, so the comparison is
`cargo bench` now vs `cargo bench` on a known-good tree — there is no committed
reference to diff against, and comparing criterion medians is a manual step.
`.github/workflows/bench.yml` runs the same command on demand
(`workflow_dispatch`, no schedule) and uploads `target/criterion` as an
artifact.

**Whole-program timing is gated** by the `perf` step of `scripts/check.sh`:

```bash
./scripts/check.sh --only perf          # long_loop budget, best-of-5, median decides
```

Budget **0.55 s** median over 5 runs of `./scripts/run-micro-suite.sh exe
long_loop` (release JIT, default `WIE_JIT_OPT=none`). The figure is heavily
load-dependent: measured **0.25/0.26/0.30 s** (min/median/max, n=5) on an idle
host, **0.43/0.47/0.50 s** at load average ~3.4, and 0.7–0.9 s when busy. Treat
0.55 s as a ceiling that catches a real regression, not as a target — one reading
tells you about the host as much as about the JIT. This replaces the deleted
`bench-gate.sh`, which enforced a >10 % criterion regression but nothing gated
whole-program wall time.

Two things make it noisier than it looks, both of which have produced a false
reading during development:

- **Host load dominates.** On a loaded machine the same binary reads ~0.7–0.9 s
  and the budget fails for a reason unrelated to the JIT. The median of five
  damps this but does not remove it; re-measure idle before believing a failure.
- **Measure through the suite, not the raw exe.** `./target/release/wie run
  micro-exes/out/long_loop.exe` also stages the exe into the bottle and reads
  higher than the guest alone.

Not wired into CI on purpose: a timing gate is meaningless on a shared runner.

## Environment knobs (full table)

The README keeps the shortlist; the complete set lives here.

**This table is self-checking.** `crates/wie-runtime/tests/runbook_knobs.rs`
scans every crate for `WIE_*` names that appear as the argument of an
environment read or write, parses the first column of the two tables below, and
asserts the two sets are equal (modulo `RUST_LOG`). Adding a knob therefore
means touching code **and** this table, and the test fails if you forget either
half. Because the extraction only matches read call sites, a doc-comment that
merely *mentions* a dead knob does not keep its row alive.

> **Wave3 defaults (final gate):** padded surface pool + wgpu scratch, latest-wins coalescing/throttling (`drain_pending_publishes` + `retry_delay`), pre-reserve (Q4/C) and size-class TLS are **default on** — steady zero-alloc with no env required. CompactString and the D3D9 in-place pooled target are likewise unconditional now: neither ever shipped an env gate, so `WIE_COMPACT_STRING` / `WIE_D3D9_INPLACE` never existed as knobs and are not listed below. The JIT direct register hand-off (Q8/C) is **still unbuilt** despite the "default on" wording in ADR-0002 (implementation status note added 2026-09-02 — zero call sites; see `docs/implementation-plan.md` Wave 3), so there is no knob for it and none is listed below. 64 px pitch padding is implemented (ADR-0001 status note, 2026-09-02) — `WIE_SURFACE_PAD=0` opts out.
>
> **Removed knobs — do not go looking for these.** Four names appear in older
> notes and older checkouts but no code reads them any more. Each is listed here
> rather than as a table row, because a row would read as a working switch:
>
> - `WIE_JIT_DIRECT_REGS` — removed 2026-09-25; the direct register hand-off was never implemented.
> - `WIE_JIT_TAILCHAIN` — removed 2026-09-25; Cranelift 0.133 rejects tail-call hops under every ABI convention these blocks use, so the shipped chain hop is the nested-call + `MAX_CHAIN_DEPTH` guard shape (ADR-0002 status note, `bcf3880`).
> - `WIE_JIT_VERIFY` — never implemented; use `WIE_JIT_VERIFIER` above.
> - `WIE_HOST_SLEEP` — **deprecated and no longer parsed**; `Sleep(n>0)` parking is controlled entirely by `WIE_IDLE=park` (see `crates/wie-winapi/src/idle.rs`).

| Variable | Effect |
| --- | --- |
| `WIE_CPU=jit` \| `iced` | CPU backend (default **jit**) |
| `WIE_MPROTECT=0` | Disable optional host `mprotect` dual-protection (SPC remains on) |
| `WIE_JIT_MEM=sticky` \| `pin` \| `slow` | JIT mem lower (default **pin** = sticky + stack pin + top-2 data pins; `sticky` = stack pin only; `slow` = helper-only oracle) |
| `WIE_JIT_MEM_TRACE=1` | Dump helper mem-path histogram on finalize |
| `WIE_JIT_OPCODE_HISTO=1` | Sample an opcode histogram over the iced residue (top 60 mnemonics), appended to the `WIE_RUNTIME_PROFILE` report. Set it together with `WIE_RUNTIME_PROFILE` and SIGINT a long-running target to see which instructions keep escaping the JIT. Capture long-running targets only — micro-exes exit in milliseconds and the SIGINT path never arms |
| `WIE_JIT_SUPER=loop` \| `0` \| `all` | Block-wide stack super path: default **loop** (self-loops only) |
| `WIE_JIT_CHAIN=0` | Disable FuncRef chaining / chain table / edge IC |
| `WIE_STRING_BULK=0` | Disable host-span bulk REP MOVS/STOS |
| `WIE_STRING_INLINE=0` | Disable inline 16–64 B REP Neon path |
| `WIE_JIT_SIMD=0` | Scalar XMM lowering (no CLIF SIMD / Neon) |
| `WIE_TLB_NEON=0` | Scalar 4-way TLB tag scan |
| `WIE_JIT_OPT=none\|speed_and_size\|speed` | Cranelift opt_level (default **none**). Cheap compile by default: 7-Zip is ~1.5x faster cold / ~1.7x warm; a pure-compute loop no longer pays for it, because `WIE_JIT_TIER` compiles self-loop blocks at `speed` per block (measured: `long_loop` 0.240/0.249 s with tier-up vs 0.323/0.335 s without). Set `speed` to force every block. Also part of the persistent cache key, so the two never share a ledger. |
| `WIE_JIT_TIER=0` | **Bisect switch for per-block tier-up** (default **on**). `0`/`false`/`off`/`no` builds one Cranelift module at the base `opt_level` and compiles every block there — the pre-2026-09-27 behaviour, verified against a pre-change binary (identical profile counters, wall inside noise). Use it to attribute any regression to tier-up. Note: with it off, the four `jit::tests::tier_tests` cases fail by design. |
| `WIE_JIT_TIER_BUDGET` | Cap on tier-up **decisions** per run (default **8**, clamped to 65,536; `0` = tier nothing). Charged at decision time, before the compile. Sized from measurement: a compute-bound guest needs 1 (`long_loop`) and repays 1.35x, while 7-Zip Extra has 18 self-loops and repays none (measured +19 ms `emu_ms` warm for 8 speculative tier compiles). Lower it to keep speculative `speed` compiles off a tool's start-up; raise it for multi-kernel compute guests. The `jit_tier: tier_compiles=… tier_rejects=… budget_left=…` line in `WIE_RUNTIME_PROFILE` reports the outcome. |
| `WIE_JIT_HOTNESS_THRESHOLD` | **Experimental** fixed hotness threshold override (default **100**; clamped to `[1, 1_000_000]`) |
| `WIE_JIT_LOOP_HOTNESS` | Visits before a **known pure self-loop** compiles (default **8**; `0` in tests). One Cranelift pass beats paying the iced warmup, so loops tier sooner than general blocks; lowering it further thrashes short non-loop blocks on 7-Zip and costs wall clock |
| `WIE_JIT_TARGET_WORK` | Work-weighted promotion target (default **900**, clamped; keeps a 9-insn block at ≈100 visits). The hotness accounting knob to reach for when a guest's block mix defeats the fixed `WIE_JIT_HOTNESS_THRESHOLD` |
| `WIE_JIT_BG=0` | Disable the background compile worker (default **on** for real runs, **off** under `cfg(test)` where hotness is 0 and every block is eager anyway). With it off, compilation happens inline on the guest thread |
| `WIE_JIT_BG_TIMEOUT_US` | Max guest-wait for a background compile before falling back to inline (default **1000**). A single compile is ~50–500 µs, so this is ~20× headroom; the fallback only fires on queue backlog or a dead worker |
| `WIE_JIT_EAGER_BLOCK_INSNS` | Eager-compile cutoff for large one-shot `Pure` blocks (default **0**, disabled). The original size rule lost its arithmetic — a one-shot 96-insn block costs ~9 µs interpreted vs ~1 ms compiled, a ~100× loss when the block never revisits. Set `>0` to re-enable it for diagnosis |
| `WIE_JIT_SSA_FLAGS=0` | Disable the extra Cranelift SSA construction flags. Off-by-default tuning surface; `0`/`false`/`off` disables |
| `WIE_EXEC_TRACE=1` | Iced-residue diagnostics: per-mnemonic interpreter counters, a first-sight log of every mnemonic kept out of the JIT, and the mem-path histogram. Works in **release** too — 7-Zip residual discovery should not require a debug build. Run `WIE_EXEC_TRACE=1 <exe>` to see what stays in the interpreter, and cross-check `insn_coverage` against the counters |
| `WIE_DEGRADE=0` | Restore the hard stop on an unimplemented mnemonic (degrade-not-die is default **on**). `0`/`false`/`off` makes an unknown opcode fail loudly instead of executing partially — use it to bisect "guest misbehaves" down to "guest hit an opcode we do not implement" |
| `WIE_JIT_WORKERS` | Background compile worker count (default ≈ `available_parallelism()/2`, clamped `[1, 4]`). More workers cut boot-time compile latency on multicore hosts; workers compete with guest threads for cores while active. |
| `WIE_JIT_VERIFIER=0\|1` | **Switch for the Cranelift IR verifier** (default: **on in debug builds, off in release** — i.e. `cfg!(debug_assertions)`; an explicit value wins in both directions in both profiles, so `0` is how you stop paying for it while bisecting a JIT bug in a debug build and `1` is how you re-arm it in a release build to confirm a suspected miscompile). The verifier runs *per compiled function* and checks CLIF SSA/dominance, type correctness, use-before-def and operand constraints, so it is the only thing that makes an ill-typed lowering fail loudly with a diagnostic instead of emitting wrong host code. It never checked x86 *semantics* — the JIT-vs-iced differential is the semantic gate, so what the release default gives up is type/SSA safety, not lowering-meaning correctness. Measured on 7-Zip Extra `7za.exe i --max-api 400000`, release JIT, 15 interleaved A/B pairs against one build on a shared saturated warm ledger: compile *work* is unchanged (`eager` 517, `hot` 18, `bg_compiles` 530, `tier_compiles` 8 in both arms) but compile *time* drops — paired ratio 0.595 median (15/15 pairs, sign test p=6e-5) — and `wall_ms` drops with it, 419→259 ms min / 679→464 ms median. On the compute-bound `long_loop` the same saving is invisible in wall clock (paired ratio 1.031, 7/20 pairs, p=0.26) because only 4 blocks ever compile. **Why this is safe for CI: `cargo nextest` runs the dev profile, so the test suite compiles with the verifier ON while release micro-suite runs run with it OFF — existing CI already exercises both arms and needed no change.** That is the compensating control, not a demonstration that the verifier is redundant: nothing in this repo records it ever firing, so "the suite is green with it off" means *no known bug is masked*, **not** *the verifier is unnecessary*. Full workspace suite green in both arms (1827/1827). Replaces the never-implemented `WIE_JIT_VERIFY`. |
| `WIE_JIT_CACHE` | Persistent JIT code-cache **ledger** (`$WIE_CACHE_DIR/jit`, else `$XDG_CACHE_HOME/wie/jit`). Records per-PE block metadata (`guest_va + FNV-1a of the exact guest bytes` → extent, insn count) plus negative entries for blocks that failed to compile. On warm boot known-good blocks skip the Hot visit-threshold warmup (immediate background compile); known-bad blocks skip re-decode entirely. Every consumption is hash-validated against current guest bytes — SMC/protect invalidations tombstone matching records. Records also carry the opt level that produced them (`compiled_at_opt`, `FORMAT_VERSION` 3), because with per-block tiering one process compiles the same guest bytes at two levels and a `Ready`/`Never` verdict is a claim about compiler settings; a record whose level does not match the file it is in is rejected on load. Tier-level records are written to a second ledger file keyed by the tier's own `(pe_hash, opt_level)` and are never read back by the process that wrote them — a `WIE_JIT_OPT=none` run must not consume `speed` verdicts, and a later `WIE_JIT_OPT=speed` run loads that file as its own base. Default **on** (off in unit tests). `0`/`false`/`off` disables; any other value is treated as an explicit directory path. NOTE: this is a metadata ledger, NOT machine-code replay — raw Cranelift aarch64 restore is unsafe here (`is_pic=false` bakes per-run text-region addresses; no relocation export), so the ~3 ms/block tier-1 compile itself still runs on warm boots. |
| `WIE_CACHE_DIR` | Root directory for the `WIE_JIT_CACHE` ledger (default `$XDG_CACHE_HOME/wie`, else the macOS / Linux user-cache convention). Set it to point the JIT cache at a scratch dir when bisecting warm-boot behaviour |
| `WIE_FIXED_CLOCK=1` | Freeze the guest clock table (deterministic runs) |
| `WIE_D3D9_SCALE=2\|4\|8` | D3D9 render resolution divisor (quarter-scale = 4; Present upscales to the window) |
| `WIE_PRESENT_PACING_HZ` | Pace `Present` to a target frame rate (e.g. `60`); unset = no pacing. Sleeps out the remainder of each interval, so it is a **throttle**, not a vsync — useful for making a guest that busy-loops between Presents behave like a 60 fps app. Resolved once per process; non-numeric or `<= 0` is ignored |
| `WIE_RUNTIME_PROFILE=1` | Wall/CPU%, host stops, JIT counters, `mem_backend`. While armed, **Ctrl+C** stops the session cleanly at the next quantum/API-stop boundary, dumps the report to stderr and exits 130; guest Ctrl+C delivery is suppressed while profiling |
| `WIE_PROCESS_HEAP_MB` | Guest process-heap size in MiB (default **512**) |
| `WIE_NO_HOOK_SLICES` | Cap on the no-hook slice lookahead (a positive integer; an invalid value warns and keeps the default). Lower it to stop the quantum from speculating past a block boundary; raise it when a long straight-line region is being cut into too many slices |
| `WIE_GUEST_ENV="NAME=VALUE;NAME2=VALUE2"` | Inject pairs into the guest environment of every new session (upsert, case-insensitive names like Windows). This is how headless CLI runs reach the micro-exes' self-test modes (`WIE_GUEST_ENV=WIE_SELFTEST=2`) without a dedicated flag; the tests' `set_guest_env` path stays authoritative and this is unset there, so CI is unaffected. A pair without `=` is dropped with a warning |
| `WIE_API_JOURNAL=path` | Per-API journal for backend A/B diffs (cached once per process; see `crates/wie-runtime/src/knobs.rs`) |
| `WIE_ROOT` / `--root` | Optional bottle override for guest `C:\` file APIs (default: per-user app-data bottle at `~/Library/Application Support/WIE/bottle`, created on demand) |
| `WIE_DRIVE_D` / `--drive-d` | Host root for guest `D:\` bridge (`auto` = host cwd) |
| `WIE_PRINT_TO=<dir>` | EndDoc headless oracle: write rendered pages as `page-N.bmp` (no bridge needed) |
| `WIE_GUEST_HEAP=1` | Rewire process-heap `HeapAlloc`/`HeapFree` to guest code |
| `WIE_GUEST_IO=0` \| `all` | I/O accelerator: default seeks/size in-guest; `all` also guest Read |
| `WIE_GUEST_MBWC=1` | Guest MultiByte↔WideChar helpers |
| `WIE_IDLE=busy\|yield\|park` | Host idle policy: micros default **yield**; interactive default **park** |
| `WIE_IDLE_PARK_MS` | Message-park quantum (sleep-poll paths only; persistent/GUI idle parks are event-driven) |
| `WIE_IDLE_CAP_MS` | Hard cap on a **single** `Sleep` park in ms (default **60000**, min 1). Bounds how long one guest `Sleep(huge)` can wedge a parked host thread |
| `WIE_API_TRACE=1` | Full API dump (head+tail only by default) |
| `WIE_INPUT_SCRIPT=<path>` | GUI input script path |
| `WIE_VFS_DOWNLOADS=1` | **Test-only.** Opt in to the `micro_vfs_roundtrip` case that exercises the *real* macOS `~/Downloads` directory instead of a temp bottle; unset (the default) skips that case. See `crates/wie-runtime/tests/micro_vfs_roundtrip.rs` |
| `WIE_MT=0` | Disable guest worker spawn |
| `WIE_MT_MAX_THREADS` | Cap on guest worker threads (default **64**) |
| `WIE_MT_DEBUG=1` | Verbose guest worker-thread tracing (spawn / park / worker-exit paths). Cached once per process — set it before the first worker starts |
| `WIE_ALLOW_MISSING_GUESTS=1` | **Test-only.** Downgrade the "missing guest fixture" panic to a skip, for a machine without the mingw-w64 cross toolchain. Never read on a production path; prefer `make -C micro-exes` |
| `WIE_UI_LANGID` | **Pin the guest UI language** (default: mirror the host). A Windows LANGID (`0x409`, `1033`) or a language tag (`en-US`, `de`, `fr_FR.UTF-8`); `C`/`POSIX` mean en-US. Consulted **before** `LC_ALL` / `LANG` / the macOS `AppleLanguages` probe, so it always wins. An unparseable or unknown-language value is **ignored**, never coerced to en-US, and falls through to the next source. Derived once per process, so set it before the first `GetUserDefaultUILanguage` / `LoadMenuW` / `LoadString*`. Use it in CI to make resource selection deterministic instead of depending on the runner's UI language. |
| `RUST_LOG` | tracing filter (CLI defaults to `warn`) |

### Wave3 remaining knobs — sketch (all default **on**, no env required; `=0` opts out for matrix/bisect)

| Variable | Default | Opt-out | Lane |
| --- | --- | --- | --- |
| `WIE_SURFACE_PAD` / padded pool | **64 px** row pitch (`padded_stride`, ADR-0001, implemented 2026-09) + reused wgpu scratch | `WIE_SURFACE_PAD=0` (unpadded, on-demand pack) | Q2/D |
| present channel | **on** (`PresentChannel` latest-wins slot; `take_frame` is lock-free on the presenter side, ADR-0003) | none — always on | Wave 1a |
| `WIE_CAPTURE_STREAM` | **on in GUI sessions** (Wave 2, 2026-09: `IDirect3DDevice9::Present` flushes the `wie-capture-stream` render thread — the **sole** GUI render-thread path, replay + RT/depth handback + publish all run off the big lock; headless/micro-suite keep the legacy in-handler path) | `WIE_CAPTURE_STREAM=0` (legacy in-handler present — also the headless/CI hash oracle) | Wave 2 |
| present throttling / latest-wins | **on** (`drain_pending_publishes` + `retry_delay` coalesce) | `cfg(test)` skips (tests assert publish) | Q5/C |
| pre-reserve / scratch reuse | **on** (capacity-retained `Vec`s, pooled `WgpuPresenter` scratch) | none — always on | Q4/C |
| CompactString | **on** (inline small strings; see `compact_string` crate in `wie-winapi` where string-heavy) | none — unconditional; it never shipped an env gate | Q6/C |
| size-class TLS | **on** (24 classes, TLS-pooled) | none — always on | TLS lane |
| mimalloc | **feature** `mimalloc` (if enabled, host allocator) | default allocator if feature off | Q7/C |
| D3D9 in-place | **on** (pooled `WindowSurface` slice as render target — no temp `Vec<u32>`) | none — unconditional; it never shipped an env gate | Q9/C |
| *(no variable)* direct register hand-off (Q8/C) | **NOT implemented** — no x19–x28 ABI exists, and the env gate was removed 2026-09-25, so there is no opt-out either | — | Q8/C |

> If a knob is not listed above, it is **not** a Wave3 gate — see the full table. All Wave3 paths are verified steady zero-alloc under `WIE_RUNTIME_PROFILE=1` without setting any of these.

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
| Guest fixtures (`micro-exes/out/`, `real_exes/`) and the "missing guest = failure" rule | [`docs/TESTING.md`](TESTING.md#prerequisites--the-guest-fixtures-must-exist) |
| Multithreading | [`docs/architecture/runtime.md`](architecture/runtime.md) (threading model) |
| Live progress log | [`../.slim/deepwork/gui-implementation.md`](../.slim/deepwork/gui-implementation.md) |

## Non-goals of this sheet

- Wine-style identity `mmap(guest_va)` — **never** a remediation path.
- Rolling back to HashMap / hybrid storage — **removed** in the great cleanup.
- Full Windows wait / APC debugging — not modelled.
