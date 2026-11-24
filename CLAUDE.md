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

## Lint policy (strict in intent — read the enforcement note, it is narrower)

Intended policy, and what you must write to: no `unwrap`/`expect`/`panic`/`unreachable`/`todo`/`unimplemented`, no `indexing_slicing`, no `as_conversions`, no cast lints, no `clippy::pedantic`, and `unsafe` confined to `wie-cpu`. Use `?`/`ok_or_else`, `.get()`, and `u64::from(x)`/`try_from`. `#[allow(...)]` escapes should be rare (CONTRIBUTING.md).

**What is actually enforced today — do not assume the above is enforced:**

- **No clippy lints are denied.** `Cargo.toml` has no `[workspace.lints.clippy]` section at all: *"No clippy lints are denied here: clippy runs at its defaults and is advisory in CI."* Clippy is advisory because `scripts/check.sh` and `.github/workflows/ci.yml` both invoke it **without** `-D warnings`, even though this file used to document `-D warnings`. The stricter command is worth running before a PR, but the gate does not.
- **`[workspace.lints.rust]` only binds where a crate opts in.** Only `wie-cpu` and `wie-winapi` have `[lints] workspace = true`. `wie-pe`, `wie-runtime` and `wie-cli` do not, so the table's `unsafe_code = "deny"`, `unused_imports`, `rust_2018_idioms` etc. are **inert in those three crates**. In practice `wie-cli` carries ~21 `unsafe` sites (4 files) that nothing currently denies; `wie-winapi`'s 5 are properly annotated with `#[expect(unsafe_code)]`.
- Closing that gap means adding `[lints] workspace = true` to the three crates and then annotating or removing the `unsafe` in `wie-cli`. That is a deliberate policy decision, not a mechanical fix.

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
4. Hitting a stop-bit fake VA returns control to `RuntimeSession`, which resolves the dense `WinApiId` (no string compare on the hot path) and runs the handler via `dispatch_table.rs`. Handlers take a `HandlerContext`, use the Win64 register ABI, and finish with `return_from_win64_api`.

### Key invariants and gotchas

- **Soft-translate only**: guest VA ≠ host VA everywhere. All guest memory access goes through arena translation with software page-permission checks; host `mprotect` (`WIE_MPROTECT`) is a supplement, never the sole oracle (4K guest vs 16K host page clinch).
- **Threading is 1:1**: each guest thread gets its own CPU engine on its own host thread. JIT threads share only the compiled-code cache; WinAPI state sits behind a shared mutex that parked threads (WaitFor*, contended CS) must drop.
- **`dispatch_table.rs` header claims it is auto-generated by `scripts/gen_winapi_dispatch.py`, but that script no longer exists** — the file is maintained by hand. Adding an API means: a `WinApiId` variant, a dispatch arm, the handler in the right DLL module, and registration in the fake-VA table.
- **Guest filesystem is a bottle**: guest `C:\…` maps to `{root}/drive_c/…` (`--root` / `WIE_ROOT`); optional `D:` host bridge via `--drive-d`. Path logic lives in `wie-winapi/src/vfs/` and `bottle.rs`.
- Struct layouts returned to guests must match real Windows exactly — a wrong `WIN32_FIND_DATA` field offset once sent 7-Zip into infinite recursion. Verify offsets against Windows SDK headers, not sibling structs.
- Performance regressions count as failures for PRs (CONTRIBUTING.md): re-check micro-suite timing, e.g. `long_loop` ≈ 0.40–0.55 s release JIT under the default `WIE_JIT_OPT=none` (~0.28–0.36 s with `WIE_JIT_OPT=speed`). The default is `none` because compile cost dominates real guests; see `docs/status.md` for the measured matrix.

### Docs

`docs/RUNBOOK.md` is the symptom → kill-switch playbook + full knob table. The implemented GUI/GPU architecture is documented in `docs/architecture/` (`gui.md`, `d3d9.md`, plus the README); the live progress log is `.slim/deepwork/gui-implementation.md`.
