# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What WIE is

WIE emulates 64-bit Windows user-mode PE binaries on macOS Apple Silicon. Guest x86-64 code runs through a Cranelift block JIT (default) or an iced-x86 interpreter; Windows API calls are intercepted and handled on the host in Rust. Guest virtual addresses always soft-translate through region tables/arenas — never `mmap(addr = guest_va)`. 32-bit apps and full Windows compatibility are non-goals.

## Commands

```bash
# Build the CLI (release is what the test suite and perf numbers assume)
cargo build -p wie-cli --release

# Pre-PR gate: fmt --check, clippy (advisory), cargo nextest, micro-suite
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

## Lint policy (aspirational in intent, mostly unenforced — read the enforcement note)

**What is actually enforced by the machine (the whole of it):** nothing beyond rustc's defaults. There is **no `[workspace.lints.clippy]` section anywhere** in the repo, so `unwrap_used`, `expect_used`, `panic`, `indexing_slicing`, `as_conversions`, cast lints, and `clippy::pedantic` are **not declared in any `Cargo.toml` and not denied in any crate**. `scripts/check.sh` and `.github/workflows/ci.yml` both run `cargo clippy --workspace --all-targets` **without** `-D warnings`, so clippy runs at its defaults and is purely advisory. Consequences to keep in mind:

- The `#[allow(clippy::…)]` / `#[expect(clippy::…)]` markers sprinkled through test code are currently **inert** — they suppress lints that were never enabled. They document intent; they enforce nothing.
- The only machine-checked lint table is `[workspace.lints.rust]` in the root `Cargo.toml`, and it binds where a crate opts in via `[lints] workspace = true`. **All five crates now opt in**, so `unsafe_code = "deny"`, `unused_imports`, `let_underscore_drop` and `rust_2018_idioms` bind workspace-wide. (Before this, the table bound in only `wie-cpu` and `wie-winapi`, leaving `wie-pe`, `wie-runtime` and `wie-cli` entirely unchecked.)

**What the project intends (convention, reviewer-enforced, not machine-enforced):** no `unwrap`/`expect`/`panic`/`unreachable`/`todo`/`unimplemented`; no `indexing_slicing`; no `as_conversions`; no cast lints; no `clippy::pedantic`; `unsafe` confined to `wie-cpu`. Write to that standard — use `?`/`ok_or_else`, `.get()`, and `u64::from(x)`/`try_from`, and keep `#[allow(...)]` escapes rare (CONTRIBUTING.md) — but do **not** assume a build failure will catch a violation, and do not describe it in review as if CI enforces it.

**Actual `unsafe` inventory** (`\bunsafe\b` in code, comments/lint attributes excluded):

| Crate | `unsafe` sites | Files | Machine-enforced? |
| --- | --- | --- | --- |
| `wie-cpu` | **137** | 21 | Yes — `unsafe_code = "deny"` with per-site allows |
| `wie-cli` | 43 | 3 | Yes — all annotated (`#[expect(unsafe_code)]`) |
| `wie-winapi` | 17 | 4 | Yes — all annotated (`#[expect(unsafe_code)]`, one `#[allow]`) |
| `wie-pe` | 0 | 0 | n/a |
| `wie-runtime` | 0 | 0 | n/a |

`wie-cpu` is the largest concentration and the one the project treats as the legitimate home for `unsafe`. `wie-cli`'s 43 sites are `gui/print.rs` 36, `gui/app/native_dialog.rs` 5, `commands/run.rs` 2 — almost entirely macOS AppKit FFI for the print and page-setup panels. (`gui/menu_bar.rs` and `gui/present_wgpu.rs` mention the word `unsafe` only in doc comments asserting they contain none.)

**Closing the remaining gap is a deliberate policy decision, not a mechanical fix.** The rustc denies now bind everywhere; what is still unenforced is a curated `[workspace.lints.clippy]` table. Adding one would light up thousands of `unwrap`/`expect`/`as` sites, so scope it per crate and decide the lock-poisoning convention first (`crates/wie-cpu` has ~90 lock `unwrap()`s where a poison-tolerant idiom is already the local norm). Decide that on purpose; do not drift into it.

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
- **Adding a *dense* API is one row.** `dispatch_table` is a directory, not a file: `dispatch_table/decl.rs` declares the whole surface through the `winapi_ids!` macro (defined `decl.rs:35`, invoked `decl.rs:92`), which emits the `WinApiId` enum, the hot-path `dispatch_winapi_id` match arm, **and** the `(library, export, id)` name rows from a single row. A compile-time assertion (`decl.rs:78-88`) pins the counts at 510 variants / 512 discriminants (two historical holes) / 511 name rows. Fake-VA encoding is *derived* from the id — `encode_export(id)` — so there is no manual fake-VA registration step. Adding an API = one `winapi_ids!` row + the handler function, in the right DLL module. (There is no `dispatch_table.rs`; point at `dispatch_table/decl.rs`.)
- **Adding a *soft* (string-dispatch) API is NOT single-source.** The soft path is ~4 parallel hand-kept name registries: the soft export census lists in `dispatch_table/names/mod.rs` (650 names across 19 consts), `is_winapi_library` (27 DLLs), `PREPLANTED_SOFT_APIS` in `dynamic_apis.rs:36` (156 rows), `DYNAMIC_FAKE_APIS` in `dynamic_apis.rs:209` (22 rows), plus per-DLL `*_EXPORTS` in `urlmon.rs:70`, `wininet.rs:206`, `ntdll.rs:54`, and `opengl32.rs`. Adding a soft API means the handler, the dispatch arm, **and** the relevant census list.
  - The failure modes are asymmetric, which is why this matters: forget a **dispatch arm** and the guest hard-fails at `bail!("unsupported WinAPI call: {library}!{name}")` (`dispatch_table/mod.rs:205`); forget a **census row** and nothing breaks at runtime — it only silently degrades `wie inspect` output, because `is_winapi_implemented` is not consulted on the production dispatch path.
- **Guest filesystem is a bottle**: guest `C:\…` maps to `{root}/drive_c/…` (`--root` / `WIE_ROOT`); optional `D:` host bridge via `--drive-d`. Path logic lives in `wie-winapi/src/vfs/` and `bottle.rs`.
- Struct layouts returned to guests must match real Windows exactly — a wrong `WIN32_FIND_DATA` field offset once sent 7-Zip into infinite recursion. Verify offsets against Windows SDK headers, not sibling structs.
- Performance regressions count as failures for PRs (CONTRIBUTING.md): re-check micro-suite timing, e.g. `long_loop` ≈ 0.40–0.55 s release JIT under the default `WIE_JIT_OPT=none` (~0.28–0.36 s with `WIE_JIT_OPT=speed`). The default is `none` because compile cost dominates real guests; see `docs/status.md` for the measured matrix.

### Docs

`docs/RUNBOOK.md` is the symptom → kill-switch playbook + full knob table. The implemented GUI/GPU architecture is documented in `docs/architecture/` (`gui.md`, `d3d9.md`, plus the README); the live progress log is `.slim/deepwork/gui-implementation.md`.
