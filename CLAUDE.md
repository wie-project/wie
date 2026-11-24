# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What WIE is

WIE emulates 64-bit Windows user-mode PE binaries on macOS Apple Silicon. Guest x86-64 code runs through a Cranelift block JIT (default) or an iced-x86 interpreter; Windows API calls are intercepted and handled on the host in Rust. Guest virtual addresses always soft-translate through region tables/arenas — never `mmap(addr = guest_va)`. 32-bit apps and full Windows compatibility are non-goals.

## Commands

```bash
# Build the CLI (release is what the test suite and perf numbers assume)
cargo build -p wie-cli --release

# Pre-PR gate — nine steps, in this order:
#   file-sizes dep-direction winapi-port fmt clippy micro-exes nextest micro-suite perf
# `./scripts/check.sh --list` prints them; `--only <step>` runs one.
./scripts/check.sh

# Individual steps
cargo fmt --all
cargo clippy --workspace --all-targets            # what the gate runs
cargo clippy --workspace --all-targets -- -D warnings   # stricter; worth running before a PR
cargo nextest run --workspace
cargo nextest run -p wie-cpu <test_name>    # single test

# Integration suite (builds micro PEs + runs them under WIE_CPU=jit)
make -C micro-exes && ./scripts/run-micro-suite.sh
./scripts/run-micro-suite.sh cpp_exes      # categories: cpp_exes, seh_exes, dll_tests

# Run a single guest PE
./target/release/wie run micro-exes/out/crt_hello.exe
./target/release/wie inspect <pe> --imports
./target/release/wie trace <pe>        # first N host API stops

# JIT matrix when touching memory lower / chaining (optional, slower)
./scripts/run-micro-suite.sh all --matrix
```

Building `micro-exes/` requires the mingw cross-compiler: `x86_64-w64-mingw32-gcc` (Homebrew `mingw-w64`). Real-tool testing uses Windows `7za.exe` in `real_exes/` (gitignored; fetch with `./scripts/fetch.sh 7za`).

### Debugging / bisecting

Prefer kill-switch env vars over deep debugging first — `docs/RUNBOOK.md` maps symptoms to switches. Key ones: `WIE_CPU=iced` (rule out JIT miscompile), `WIE_JIT_MEM=slow` (helper-only memory), `WIE_JIT_CHAIN=0`, `WIE_STRING_BULK=0`, `WIE_JIT_SIMD=0`, `WIE_MPROTECT=0`. `WIE_RUNTIME_PROFILE=1` prints wall/CPU%, host-stop counts, and JIT counters; while it is armed, Ctrl+C stops the session cleanly, dumps the report to stderr, and exits 130. The full knob table is in docs/RUNBOOK.md.

## Lint policy (read the enforcement note before quoting any rule as a gate)

**What is machine-checked, and what is not.** Two different mechanisms, and the
difference matters:

*Rustc lints.* One table, `[workspace.lints.rust]` in the root `Cargo.toml`, and
it binds only where a crate opts in with `[lints] workspace = true`. **All five
crates opt in**, so `unsafe_code = "deny"`, `unused_imports`,
`unused_must_use`, `unused_extern_crates`, `let_underscore_drop`,
`rust_2018_idioms`, `rust_2021_prelude_collisions` deny workspace-wide, and
`unreachable_pub`, `elided_lifetimes_in_paths`, `explicit_outlives_requirements`
warn. Doc comments on `unsafe` sites are a review rule; these denies are not.

*Clippy.* Still **advisory**. There is no `[workspace.lints.clippy]` section in
any manifest, and both `scripts/check.sh` (step 5 of 9) and
`.github/workflows/ci.yml` run `cargo clippy --workspace --all-targets` without
`-D warnings`. `unwrap_used`, `expect_used`, `panic`, `indexing_slicing`,
`as_conversions` and `pedantic` are therefore not declared and not denied, and
the `#[allow(clippy::…)]` / `#[expect(clippy::…)]` markers under `tests/` are
inert — they document intent and enforce nothing.

**The whole machine-checked set, in one place:** `cargo fmt --check`, the
2000-line file cap, the crate-dependency-direction check, the `winapi-port`
boundary check, the `long_loop` timing budget, the RUNBOOK-knob equality test —
and the rustc lints above. Everything else in this file is convention.

**Clippy is nevertheless clean, and that is new.** `cargo clippy --workspace
--all-targets` reports **zero warnings** at `a4ba020`, on a full rebuild of a pristine
extraction (the commit that did that work records 3 before it). Two consequences: a new warning is now a signal rather than noise, and a
curated `[workspace.lints.clippy]` table would no longer start with a
thousands-of-lines diff. `wie-cpu` also grew the lock convention that made
`unwrap_used` unimplementable before — `crates/wie-cpu/src/lock.rs` has
`lock_unpoisoned` / `read_unpoisoned` / `write_unpoisoned`, used at 75 sites, with
14 bare lock `unwrap`/`expect` sites left and all of them under `jit/tests/**`.
Adding the gate crate-by-crate, starting with `wie-cpu`, is now a cheap
experiment instead of an unbounded one.

**What the project intends (convention, reviewer-enforced):** no
`unwrap`/`expect`/`panic`/`unreachable`/`todo`/`unimplemented`; no
`indexing_slicing`; no `as_conversions`; no cast lints; no `clippy::pedantic`;
`unsafe` confined to `wie-cpu`, `wie-cli`'s AppKit FFI and `wie-winapi`'s two
annotated modules. Write to that standard — use `?`/`ok_or_else`, `.get()`, and
`u64::from(x)`/`try_from`, and keep `#[allow(...)]` escapes rare
(CONTRIBUTING.md) — but do **not** assume a build failure will catch a violation,
and do not describe it in review as if CI enforces it.

**Actual `unsafe` inventory.** Counted as **code sites only** — every line
matching `\bunsafe\b` under `crates/<crate>/src`, minus lines whose first
non-space characters are `//` (doc comments included) or `#[` (attributes). It
counts in-crate test modules too (`jit/tests/**`, `mem/tests.rs`):

| Crate | `unsafe` sites | Files | Machine-enforced? |
| --- | --- | --- | --- |
| `wie-cpu` | **145** | 23 | Yes — `unsafe_code = "deny"`, re-permitted per file (`#![allow(unsafe_code)]` in the 10 JIT/memory files) or per item (`#[expect(unsafe_code)]`, 36) |
| `wie-cli` | 43 | 3 | Yes — all annotated (`#[expect(unsafe_code)]`) |
| `wie-winapi` | 17 | 4 | Yes — all annotated (`#[expect(unsafe_code)]`, one `#[allow]`) |
| `wie-pe` | 0 | 0 | n/a |
| `wie-runtime` | 0 | 0 | n/a |

`wie-cpu` is the largest concentration and the one the project treats as the
legitimate home for `unsafe`; the biggest single files are
`jit/trampolines.rs` 38, `jit/lower/mem.rs` 19, `mem/arena.rs` 16,
`jit/fast_api.rs` 13, `jit/lower/tlb.rs` 10. `wie-cli`'s 43 are `gui/print.rs`
36, `gui/app/native_dialog.rs` 5, `commands/run.rs` 2 — almost entirely macOS
AppKit FFI for the print and page-setup panels. `wie-winapi`'s 17 are
`console/host_term.rs` 9, `kernel32/sync.rs` 6, `ucrt/stdio.rs` 1,
`dispatch_table/mod.rs` 1. (`gui/menu_bar.rs` and `gui/present_wgpu.rs` mention
the word `unsafe` only in doc comments asserting they contain none.)

Recount with, from the repo root:

```bash
for c in wie-cpu wie-cli wie-winapi wie-pe wie-runtime; do
  printf '%s: ' "$c"
  grep -rnE '\bunsafe\b' "crates/$c/src" --include='*.rs' 2>/dev/null |
    awk -F: '{ line=$0; sub(/^[^:]*:[0-9]*:/, "", line);
               if (line ~ /^[[:space:]]*\/\//) next;
               if (line ~ /^[[:space:]]*#\[/) next; print }' | wc -l
done
```

## Architecture

Five workspace crates with a linear dependency flow: `wie-pe` → `wie-cpu` → `wie-winapi` → `wie-runtime` → `wie-cli`.

| Crate | Role |
| --- | --- |
| `wie-pe` | PE64 parse, section map plan, IAT patching with fake API VAs, COFF → `PAGE_*` protects |
| `wie-cpu` | `JitCpu` (Cranelift x86-64→ARM64 block JIT + iced fallback) and `IcedCpu` (pure interpreter, `WIE_CPU=iced`); guest memory: mmap arenas, `RegionTable`, PageMap/VAD/software permission checks, JIT TLB + region pins |
| `wie-winapi` | KERNEL32/UCRT/USER32/GDI32/… handlers, dense `WinApiId` dispatch, guest heap (24 size classes), VFS/bottle path mapping, sync objects, SEH/MSVC C++ EH |
| `wie-runtime` | `RuntimeSession`: PE load, region layout, fake-API hooks, in-guest stubs and accelerators, run loop, TEB last-error, multithread runtime |
| `wie-cli` | Three commands: `inspect` / `run` / `trace` |

### Execution flow

1. `wie-pe` maps the image into one `MEM_IMAGE` arena and rewrites every IAT slot to a fake API VA (`0x7000_0000_0000_xxxx`).
2. A stop bitmap covers the fake range. Hot APIs (GetLastError, critical sections, PID/TID, …) get in-guest stubs so they never stop the host; optional accelerators (`WIE_GUEST_HEAP/IO/MBWC`) rewire IATs to real guest code.
3. The CPU backend executes from the PE entry. `JitCpu` compiles hot lowerable basic blocks to ARM64 (cached in `Arc<JitShared>`); everything else falls back to iced stepping.
4. Hitting a stop-bit fake VA returns control to `RuntimeSession`, which resolves the dense `WinApiId` (no string compare on the hot path) and runs the handler via `dispatch_table::dispatch_winapi_id`. Handlers take a `HandlerContext`, use the Win64 register ABI, and finish with `return_from_win64_api`.

### Key invariants and gotchas

- **Soft-translate only**: guest VA ≠ host VA everywhere. All guest memory access goes through arena translation with software page-permission checks; host `mprotect` (`WIE_MPROTECT`) is a supplement, never the sole oracle (4K guest vs 16K host page clinch).
- **Threading is 1:1**: each guest thread gets its own CPU engine on its own host thread. JIT threads share only the compiled-code cache; WinAPI state sits behind a shared mutex that parked threads (WaitFor*, contended CS) must drop.
- **Adding a *dense* API is one row.** `dispatch_table` is a directory, not a file: `dispatch_table/decl.rs` declares the whole surface through the `winapi_ids!` macro (defined `decl.rs:35`, invoked `decl.rs:92`), which emits the `WinApiId` enum, the hot-path `dispatch_winapi_id` match arm, **and** the `(library, export, id)` name rows from a single row. A compile-time assertion (`decl.rs:82-88`) pins the counts at 510 variants / 512 discriminants (two historical holes) / 511 name rows. Fake-VA encoding is *derived* from the id — `encode_export(id)` — so there is no manual fake-VA registration step. Adding an API = one `winapi_ids!` row + the handler function, in the right DLL module. (There is no `dispatch_table.rs`; point at `dispatch_table/decl.rs`.)
- **Adding a *soft* (string-dispatch) API is single-source for 11 of 19 modules, and hand-kept for the other 8.** No census list lives in `dispatch_table/names/mod.rs` any more: each dispatch module owns a `pub(crate) const EXPORTS` beside the match it describes, and `names/mod.rs` (368 lines) keeps only the lookups (`is_winapi_library`, `is_winapi_implemented`, `resolve_winapi_id`, `winapi_id_export`) plus the test-only `SOFT_EXPORT_CENSUS` table that walks them.
  - **Single-source (11 modules, 135 names):** `winmm` 36, `ws2_32` 26, `dbghelp` 14, `setupapi` 12, `shell32` 11, `imm32` 11, `winhttp` 9, `crypt32` 8, `uxtheme` 4, `msimg32` 3, `dinput8` 1. Each declares one `soft_exports!` row list (`soft_dispatch.rs`), and the macro expands it into **both** the `DISPATCH` table and `EXPORTS`. A row is dispatched and advertised or neither, so a missing row is a compile error, not a stale report. Adding a soft API here = one row + the handler.
  - **Hand-kept subset (8 modules, 515 names):** `kernel32` 192, `ucrt` 154, `user32` 66, `oleaut32` 28, `gdi32` 25, `ole32` 22, `advapi32` 21, `comctl32` 7. Their dispatch accepts names that must stay out of the census — `IDataObject::*` vtable slots, `ordinal N`, `var*` aliases, `createprocess*`, `enumfont*`, UCRT's four non-censused callables, and `user32`'s three sub-dispatchers — so `dispatch ⊆ census` is unstateable there and the reason is stated at each list. Adding a soft API here = the handler, the dispatch arm, **and** the census row, and only the first two are enforced.
  - **The asymmetric failure mode is the reason the census exists at all.** Forget a **dispatch arm** and the guest hard-fails at `bail!("unsupported WinAPI call: {library}!{name}")` (`dispatch_table/mod.rs:208`); forget a **census row** and nothing breaks at runtime — it only silently degrades `wie inspect` output, because `is_winapi_implemented` is not consulted on the production dispatch path. `dispatch_table/census_tests.rs` drives real dispatch for every censused name and is the only thing that ties the two together.
  - Outside either shape: `is_winapi_library` (27 DLLs), `PREPLANTED_SOFT_APIS` (156 rows, `dynamic_apis.rs`) and `DYNAMIC_FAKE_APIS` (22 rows) where **order is ABI** and the lists are append-only by construction, plus the four module-own lists `ntdll` 27, `urlmon` 4, `wininet` 17, `opengl32`.
- **Guest filesystem is a bottle**: guest `C:\…` maps to `{root}/drive_c/…` (`--root` / `WIE_ROOT`); optional `D:` host bridge via `--drive-d`. Path logic lives in `wie-winapi/src/vfs/` and `bottle.rs`.
- Struct layouts returned to guests must match real Windows exactly — a wrong `WIN32_FIND_DATA` field offset once sent 7-Zip into infinite recursion. Verify offsets against Windows SDK headers, not sibling structs.
- Performance regressions count as failures for PRs (CONTRIBUTING.md): re-check micro-suite timing, e.g. `long_loop` ≈ 0.40–0.55 s release JIT under the default `WIE_JIT_OPT=none` (~0.28–0.36 s with `WIE_JIT_OPT=speed`). The default is `none` because compile cost dominates real guests; see `docs/status.md` for the measured matrix.

### Docs

`docs/RUNBOOK.md` is the symptom → kill-switch playbook + full knob table. The implemented GUI/GPU architecture is documented in `docs/architecture/` (`gui.md`, `d3d9.md`, plus the README). `.slim/deepwork/gui-implementation.md` is a gitignored local worklog (see `.gitignore:6`) — it is not in the repo, so do not cite it as a source.
