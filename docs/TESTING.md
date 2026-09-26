# Testing

How to run WIE's tests, and why the dev/test build profile is shaped the way
it is.

## TL;DR — which command to run

| You are… | Run |
| --- | --- |
| iterating after an edit | `./scripts/test-fast.sh` |
| about to open a PR | `./scripts/check.sh` (authoritative: fmt, clippy, **full** suite, micro-suite) |
| touching memory lowering / JIT chaining / CPU dispatch | `./scripts/check.sh`, then `./scripts/run-micro-suite.sh all --matrix` |
| measuring emulator performance | `cargo build -p wie-cli --release` first — never the `dev` profile |

`scripts/test-fast.sh` is a convenience wrapper, **not** a second gate. It runs
the whole workspace suite minus two slow groups (see below). If it is green,
`scripts/check.sh` is what you actually need to pass.

## Lanes

### Fast lane (inner dev loop)

```sh
./scripts/test-fast.sh                      # whole workspace minus slow groups
./scripts/test-fast.sh -p wie-cpu           # narrow to one crate
./scripts/test-fast.sh -E 'test(/d3d9/)'    # narrow to a subset
```

It runs `cargo nextest run --profile fast`. The `fast` profile is defined in
`.config/nextest.toml`; its `default-filter` excludes every test binary named
`micro_*`, `clock_stub` or `idle_park_wake` — i.e. the two slow groups below.
It sets `fail-fast = true` and `retries = 0` so a red lane stops at the first
real failure instead of grinding through 1700 tests. (nextest defaults to
every workspace member, so no `--workspace` flag is passed — adding one would
defeat a caller-supplied `-p`.)

(`default-filter` accepts exactly one predicate — set difference and
`not group(...)` are rejected with *"predicate not allowed in `default-filter`
expressions"* — so the exclusion is spelled `not binary(/…/)` rather than the
more readable `all() - (…)`. The two select the same tests.)

The script also exports `WIE_JIT_CACHE=0` (belt-and-braces; see
[Persistent JIT cache](#persistent-jit-cache-and-tests)), prints a one-line
summary of what it skipped and why, and exits non-zero on failure.

### Full gate (pre-PR)

```sh
./scripts/check.sh
```

Runs, in order: `check-file-sizes.sh`, `cargo fmt --all --check`, `cargo clippy
--workspace --all-targets` (advisory — clippy lints are not denied), the FULL
`cargo nextest run --workspace` (default profile, slow groups included), then
`make -C micro-exes` and `scripts/run-micro-suite.sh`. Nothing in the fast lane
short-circuits any of it.

### Running the slow groups on their own

```sh
cargo nextest run --profile default \
  -E 'package(wie-runtime) + binary(/^(micro_|clock_stub$|idle_park_wake$)/)'
```

That single filter selects both slow groups (`micro_gui_window` starts with
`micro_`). To take only the GUI suite:

```sh
cargo nextest run --profile default \
  -E 'package(wie-runtime) + binary(micro_gui_window)'
```

Note on naming: nextest has no way to *name* a group in config. An
`[[profile.default.overrides]]` block is selected by its filter, and the
implicit `group(...)` operator only accepts synthesized names that are not
addressable from a shell — both `group(slow)` and `group(<the filter text>)`
are rejected by the filter parser. So the two `overrides` blocks are the
source of truth for the filter expressions and their relaxed `slow-timeout`
values; refer to the groups by their filter, not by name.

## The two slow groups

**`gui` — `crates/wie-runtime/tests/micro_gui_window` (13 tests).** Every test
in this binary takes the process-wide `GUI_SUITE_LOCK`
(`crates/wie-runtime/tests/micro_gui_window/helpers.rs`). The lock is not
optional: under the default parallel schedule the CPU-heavy tests starve the
other test threads' 50 ms sleep quanta, so guests catch up in multi-tick bursts
and race their timer-driven state transitions against each other. So 13 tests
that could use 8 cores run strictly one at a time, and each JIT-compiles a real
guest window (`gui_blit` alone is ~11 s of JIT compile). The suite's wall time
is dominated by `gui_blit` either way, so serializing it costs little. The
frame-hash assertions in this suite are real regression guards for the
present/paint/D3D9/GL paths — do not `#[ignore]` them to make a run faster.

**`guest` — the rest of the `wie-runtime` integration tests.** They spawn real
guest PEs and run the JIT, without the suite-wide lock. Cheaper than `gui`, but
still far heavier than the pure unit tests.

The profile-wide `slow-timeout` is `60s`; each slow group carries a relaxed
one (`120s` / `300s`) so nextest labels a genuinely slow guest run as SLOW
rather than painting it red.

## Why the dev profile is split

Before this profile existed, the workspace `Cargo.toml` defined only
`[profile.release]`. `cargo test` and `cargo nextest run` use the `dev`
profile, so cargo's *defaults* applied to every crate in the graph: Cranelift,
iced-x86, wgpu-core, naga, winit **and** all of our own guest emulation, JIT
lowering and software rasterization compiled at `opt-level=0`. The suites then
spent their wall clock inside the emulator machinery instead of inside the
assertions under test.

The fix splits the profile:

```toml
[profile.dev]
# intentionally empty — keeps the workspace crates at the dev defaults
[profile.dev.package."*"]
opt-level = 2
```

`package."*"` matches **non-workspace members only** — cargo's per-package
profile lookup skips ids for which the workspace considers it a member. So:

- third-party deps (Cranelift, wgpu, naga, winit, …) build at `opt-level=2`;
- `wie-pe`, `wie-cpu`, `wie-winapi`, `wie-runtime`, `wie-cli` keep
  `opt-level=0` with `debug-assertions` and `overflow-checks` **on**.

That asymmetry is the point. The software page-permission checks in `wie-cpu`
and the guest address arithmetic in `wie-runtime` are exactly where a debug
assertion earns its keep, and optimizing them would be optimizing the code you
are trying to debug. This is verified, not assumed — see "Verifying the
profile" below.

`opt-level = 2` rather than `3`: for a dep tree this size the 2-vs-3 delta is
small (LLVM's remaining large wins are mostly inlining-driven, and
`opt-level=2` already inlines across crates) while `3` roughly doubles dep
rebuild time — and dep rebuild time is what every developer pays on each
`cargo build` that invalidates the graph.

`[profile.release]` is untouched: `CONTRIBUTING.md` pins the `long_loop`
regression budget against that fat-LTO build.

### Verifying the profile

Cargo truncates rustc command lines in `-v` output, so the reliable check is to
intercept rustc with `RUSTC_WRAPPER` under a clean target directory and read
the flags each unit actually received:

```sh
cat > /tmp/rustc-spy <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >> /tmp/rustc_argv.log   # $1 is the real rustc
exec "$@"
EOF
chmod +x /tmp/rustc-spy

RUSTC_WRAPPER=/tmp/rustc-spy CARGO_TARGET_DIR=/tmp/wie-spy \
  cargo build -p wie-pe --profile dev
grep -o 'opt-level=[0-9]' /tmp/rustc_argv.log | sort -u   # -> opt-level=2
grep -- '--crate-name wie_pe' /tmp/rustc_argv.log | grep -c 'opt-level'   # -> 0
```

`wie_pe` receiving **no** `opt-level` flag is the proof: rustc's own default is
`opt-level=0` / `debug-assertions=on` / `overflow-checks=on`, so the workspace
crate stayed fully instrumented while its dependencies got optimized.

## Persistent JIT cache and tests

`WIE_JIT_CACHE` controls the on-disk JIT ledger
(`crates/wie-cpu/src/jit/cache_persist.rs`). The default is on, pointing at
`$WIE_CACHE_DIR`/`wie/jit` or the platform user-cache dir.

It is **off by default inside test processes**, and the check has to be runtime,
not just `cfg(test)`:

- `cfg!(test)` is true only for a crate's own `#[cfg(test)] mod tests`. An
  integration test under `crates/*/tests/` links `wie-cpu` as an ordinary
  external dependency, so `cfg!(test)` is **false** there.
- `cargo nextest` runs every test in its own process
  (`NEXTEST_EXECUTION_MODE=process-per-test`) and exports `NEXTEST=1` into each
  one.

Without the second check, all those test processes would concurrently
read/rewrite/rename the same `<pe-hash>.bin`: a last-writer-wins ledger plus
torn-read exposure — a flake and corruption source that has nothing to do with
the emulator being wrong. The condition is therefore
`cfg!(test) || NEXTEST is set`, so `cargo nextest` — the runner this project
gates on — is covered automatically, with nothing for a developer to remember.

An explicit `WIE_JIT_CACHE` always wins, including in tests, so you can point a
test run at a scratch dir when debugging the ledger itself.

Plain `cargo test` exports no distinguishing environment variable at all (it
exports the same `CARGO_*` set as `cargo run`), so an integration-test run
under `cargo test` still needs `WIE_JIT_CACHE=0` by hand. `cargo test` is not
part of this project's gate; `scripts/check.sh` and this document use nextest
throughout.

## Nightly-style axes

The test suite is not the only axis. When touching the JIT, memory lowering or
software rasterizer, vary the runtime knobs too (`docs/RUNBOOK.md` has the full
table):

```sh
WIE_CPU=iced cargo nextest run --workspace -E 'not test(/gui_window/)'
WIE_JIT_MEM=slow ./scripts/run-micro-suite.sh all --matrix
WIE_JIT_CHAIN=0 ./scripts/run-micro-suite.sh cpp_exes
WIE_STRING_BULK=0 ./scripts/run-micro-suite.sh console_tests
WIE_MPROTECT=0 ./scripts/run-micro-suite.sh all --matrix
```

The `not test(/gui_window/)` form above is the escape hatch for when a knob
breaks a GUI test specifically and you want the rest of the suite to tell you
whether anything else moved.
