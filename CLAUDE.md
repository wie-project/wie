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
# `deny`-level entries in [workspace.lints.clippy] and the crate-root lints fail
# both forms regardless of -D warnings; see "Lint policy" below.
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

*Clippy.* Now **partly enforced, and the split is by crate** — so measure
before quoting any of it as a gate. Two mechanisms, because cargo forbids mixing
them: a manifest may not both inherit `[lints] workspace = true` and declare its
own `[lints.clippy]` (`error: cannot override workspace.lints in lints`), so
`[workspace.lints.clippy]` is all-or-nothing across all five crates.

*Workspace table — all five crates, every target.* `todo`, `unimplemented`,
`dbg_macro` and `print_stdout`, all `deny`. The first three measure 0 flagged
sites in every crate, test code included; `print_stdout` measures 0 in the four
library crates, so the whole table costs the diff of one manifest block. Its one
exception is `wie-cli`, stated as
`#![expect(clippy::print_stdout, reason = …)]` at its crate root because writing
the report to stdout is that crate's product.

*Per-crate, in source.* `#![cfg_attr(not(test), deny(…))]` at the top of each
`lib.rs`, which is where the panic family lives — `wie-cpu` still measures 63
sites (39 under `src/jit/**`, 24 in `benches/jit_hot_paths.rs`) and so cannot be
carried by a workspace entry:

| lint | `wie-pe` | `wie-cpu` | `wie-winapi` | `wie-runtime` | `wie-cli` |
| --- | --- | --- | --- | --- | --- |
| `unwrap_used` / `expect_used` / `panic` | deny | — | deny | deny | deny |
| `indexing_slicing` | deny | — | — | — | deny |
| `as_conversions` | deny | — | — | — | — |

(The four workspace-table lints deny in all five crates, which is why they are
not repeated in the columns.)

`not(test)` because an assertion in a test is *supposed* to panic. That is also
why the 69 `#[allow(…)]` mentions of the panic family that sit on `mod tests`
(59 `expect_used`, 8 `unwrap_used`, 2 `panic`) are now redundant rather than
load-bearing: they neither need to be added nor removed. A legitimate production exception is
`#[expect(clippy::…, reason = "…")]`, never `#[allow]` — `expect` fails the build
when the site stops firing, so the annotation cannot rot. There are six, in
three places: two `panic!`s in `lock_order.rs` (the debug-only violation
detector), three `.expect("surface ensured")` in `opengl32_wgl.rs`, one
`.expect("device presence checked above")` in `dinput/device.rs`.

**What is still convention, and the number that decides it.** Flagged sites,
counted by running the lint (`cargo clippy --workspace --all-targets -- -W
clippy::<lint>`) — `--all-targets` total outside parentheses, production-only
inside. A grep count and a lint count are different numbers here, often by 4×:

| lint | `wie-pe` | `wie-cpu` | `wie-winapi` | `wie-runtime` | `wie-cli` | status |
| --- | --- | --- | --- | --- | --- | --- |
| `unwrap_used` | 0 (0) | 16 (1) | 63 (0) | 17 (0) | 0 (0) | gated except `wie-cpu` |
| `expect_used` | 0 (0) | 27 (0) | 279 (4) | 323 (0) | 22 (2) | gated except `wie-cpu` |
| `panic` | 0 (0) | 20 (0) | 65 (2) | 106 (0) | 1 (0) | gated except `wie-cpu` |
| `indexing_slicing` | 69 (1) | 463 (198) | 1021 (242) | 42 (14) | 10 (0) | `wie-cpu`, `wie-winapi`, `wie-runtime` convention |
| `as_conversions` | 1 (0) | 935 (441) | 689 (323) | 220 (72) | 92 (46) | all but `wie-pe` convention |
| `pedantic` (group) | 61 | 1,276 | 5,510 | 1,047 | 469 | **never** — 8,363 workspace-wide; `doc_markdown` 2,934 and `missing_errors_doc` 1,844 are the bulk, and they are style |

The remainders are stated, not silent: `wie-cpu`'s 16/27/20 panic-family sites are
39 under `src/jit/**` (the JIT rewrite in flight — re-check when it lands) and 24
in its `benches/jit_hot_paths.rs`, which no workspace table could exempt;
`indexing_slicing` is 1,021 + 198 production sites in `wie-winapi`/`wie-cpu`,
overwhelmingly guest-buffer reads after an explicit bounds check, and gating them
would mean more `#[expect]`s than lines of code. `.expect()` in tests remains
legal everywhere.

**The whole machine-checked set, in one place:** `cargo fmt --check`, the
2000-line file cap, the crate-dependency-direction check, the `winapi-port`
boundary check, the whole-state binding budget, the `long_loop` timing budget,
the RUNBOOK-knob equality test, the rustc lints above, and the clippy entries in
this section. Everything else in this file is convention.

**Clippy is also clean, which is what makes the above affordable.** `cargo
clippy --workspace --all-targets` reports **zero warnings** at `a4ba020`, on a
full rebuild of a pristine extraction (the commit that did that work records 3
before it). A new warning is therefore a signal rather than noise, and the table
above is red-line enforcement: `deny` fails the build even though `check.sh` and
CI invoke clippy without `-D warnings`.

**Still convention (reviewer-enforced only):** no `unreachable!()`; no cast
lints beyond the two crates above; no `clippy::pedantic`; `unsafe` confined to
`wie-cpu`, `wie-cli`'s AppKit FFI and `wie-winapi`'s two annotated modules. Write
to that standard — `?`/`ok_or_else`, `.get()`, `u64::from(x)`/`try_from`, and
`#[allow(...)]` escapes kept rare (CONTRIBUTING.md) — and for the rows marked
convention above, do **not** claim a build failure will catch a violation.

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
