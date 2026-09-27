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

## Prerequisites — the guest fixtures must exist

Both fixture trees are gitignored, so on a fresh clone **both steps are
required** before `cargo nextest run` is meaningful:

```sh
make -C micro-exes            # micro guests → micro-exes/out/*.exe  (needs mingw-w64)
./scripts/fetch.sh notepad    # real guest  → real_exes/notepad.exe  (RNotepad)
```

A missing guest PE is a **test failure, not a skip**
(`crates/wie-runtime/tests/common/mod.rs`), for both trees. That is deliberate:
a test that quietly returns `()` is recorded as a *pass*, and nextest only
prints a failing test's captured stderr — so a skip notice is invisible in
exactly the case it exists to report. 39 notepad-backed GUI tests once passed in
0.02–0.08 s each having executed nothing; see
[docs/testing-model.md](testing-model.md#real-guests-a-skip-that-reported-pass).

```sh
./scripts/fetch.sh --list     # 7za, 2048, notepad, doomretro
```

The one documented escape hatch is `WIE_ALLOW_MISSING_GUESTS=1`, which restores
the old skip **for `micro-exes/out/` only** (for a developer without the
mingw-w64 toolchain). It deliberately does not apply to `real_exes/`, and it must
never be set in CI. CI does both fixture steps itself
(`.github/workflows/ci.yml`: "Build guest fixtures", then "Fetch real guest
fixtures"), so a red `missing guest fixture` message in CI means the toolchain
or the fetch step, not the emulator.

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

The nextest invocation here is byte-for-byte the one in
`.github/workflows/ci.yml`, and both use the `default` profile, so both get
the same overrides, the same `gui` test group and the same three timeouts.
That is the point: the timeout and serialization policy lives in
`.config/nextest.toml`, not in a flag one of the two callers passes. The two
gates differ in exactly one place, on purpose — the final micro-suite step:
locally it is the full data-driven `all` sweep, in CI it is the four cheap
deterministic categories. `docs/testing-model.md` says why. (The `[profile.ci]`
block in `.config/nextest.toml` is unused dead config; do not wire CI to it —
it has no overrides and would silently lose all of the above.)

### Running the slow groups on their own

```sh
cargo nextest run --profile default \
  -E 'package(wie-runtime) & binary(/^(micro_|clock_stub$|idle_park_wake$)/)'
```

That single filter selects both slow groups (`micro_gui_window` starts with
`micro_`). To take only the GUI suite:

```sh
cargo nextest run --profile default \
  -E 'package(wie-runtime) & binary(micro_gui_window)'
```

**Filter syntax: `&` is intersection, `+` is UNION, `-` is difference, and `,`
is rejected outright** (verified with `cargo nextest debug parse-filterset`,
and by counting `cargo nextest list --workspace -E …` output). The two
`overrides` blocks in `.config/nextest.toml` used to be written
`package(wie-runtime) + binary(...)`, which is a union: it matched all 180
tests in the package instead of the 52 in the GUI binary, and because the
**first** matching override wins it also swallowed the `120s` block. Always
check a filter's cardinality before trusting it —
`cargo nextest list --workspace -E '<expr>' | wc -l`.

Note on naming: nextest has no way to *name* a group in config. An
`[[profile.default.overrides]]` block is selected by its filter, and the
implicit `group(...)` operator only accepts synthesized names that are not
addressable from a shell — both `group(slow)` and `group(<the filter text>)`
are rejected by the filter parser. So the two `overrides` blocks are the
source of truth for the filter expressions and their timeout values; refer to
the groups by their filter, not by name. (The GUI group is the one exception:
`test-group = "gui"` gives it a real name, usable as
`cargo nextest show-config test-groups` and
`cargo nextest run -E 'group(gui)'`.)

## Timeouts: what actually kills a test

nextest 0.9.143 has one setting with two jobs, and only one of them is a
timeout (`cargo nextest help repo-config`, "Timeout configuration"):

| key | meaning |
| --- | --- |
| `period` | label the test **SLOW** in the output after this long. Display only. |
| `terminate-after` | **N periods after which the test process is killed** and the test fails. This is the timeout. |
| `grace-period` | how long the process gets to exit after SIGTERM before it is force-killed (default 10 s). |
| `on-timeout` | `"fail"` (default) or `"pass"`. Never set it to `pass` here. |

nextest's shipped default is `60s with no termination on timeout`, so before
this was configured a hung test printed `SLOW [> 60.000s]` every minute
forever and the run only ended when something outside nextest killed it.
Current values in `.config/nextest.toml`:

| scope | setting | label | hard kill |
| --- | --- | --- | --- |
| whole profile | `slow-timeout` | 60 s | 600 s |
| `gui` group (52 tests) | override | 300 s | 600 s |
| `guest` group (45 tests) | override | 120 s | 240 s |

`terminate-after` is set on all three, so no test in this repo can hang a
run. Proof it works, on the same shape scaled down 12x
(`period = "5s", terminate-after = 3`) against a test that never returns:

```
Summary [  15.006s] 2 tests run: 0 passed, 1 failed, 1 timed out, 192 skipped
   TIMEOUT [  15.005s] (2/2) wie-cpu jit::tests::trip_tests::self_loop_reports_dynamic_retired_instructions
```

The headroom is not tight anywhere: the slowest legitimate test measured
anywhere in the repo is 100.6 s (`gui_blit` at 3x CPU oversubscription on an
already-busy host), the guest group's slowest is 3.1 s, and every other test in
the workspace finishes in well under 60 s. A ceiling only ever fires on a run
that is already lost, and it converts a silent hang into a red line naming the
test.

`global-timeout` (a ceiling on the whole run, since 0.9.100) is deliberately
**not** set: the full suite is ~50 s, so a global bound adds nothing the
per-test bounds do not already give, and it is the one setting that could cut
a legitimately slow run short.

**Profiles inherit what they do not define.** `[profile.fast]` sets no
`slow-timeout`, and it picks up `[profile.default]`'s — verified by pointing a
scratch config's default profile at a 15 s ceiling and watching `--profile
fast` time out at exactly 15.007 s. So the 600 s ceiling protects the dev lane
too, which is what you want: a hang in `test-fast.sh` now dies with a red
`TIMEOUT [600s]` line instead of sitting there until you Ctrl-C it. The
`[[profile.default.overrides]]` blocks are *not* inherited (they are
per-profile), which is harmless — the `fast` profile's `default-filter`
excludes exactly the binaries they match.

## The GUI suite runs one test at a time

`crates/wie-runtime/tests/micro_gui_window` is 52 tests, and **all 52
execute** — 39 of them drive RNotepad from `real_exes/notepad.exe`, which
`./scripts/fetch.sh notepad` produces and CI now fetches (before that, those 39
skipped while reporting `PASS`; see
[docs/testing-model.md](testing-model.md#real-guests-a-skip-that-reported-pass)).

All 52 are serialized by nextest, not by the test code:

```toml
[[profile.default.overrides]]
filter = 'package(wie-runtime) & binary(micro_gui_window)'
test-group = "gui"

[test-groups.gui]
max-threads = 1
```

`GUI_SUITE_LOCK` in `helpers.rs` **cannot** do this. nextest's default is
process-per-test, so each of the 52 tests is its own OS process with its own
copy of that mutex; before the group existed, `pgrep -f
'deps/micro_gui_window-'` during a run showed **8 concurrent processes** on an
8-core host, each JIT-compiling a 1280x800 guest. The in-process lock is kept
because it does work under `cargo test`, which shares one process.

Serialization is not only about fairness, it is the single biggest
reliability win available here. Back-to-back runs on the same host, same 32
burner processes (4x CPU oversubscription on 8 cores):

| | worst GUI test | group wall | outcome |
| --- | --- | --- | --- |
| 8-way parallel (before) | 64.9 s (`gui_blit`) | ~70 s | `gui_blit` **failed** |
| `max-threads = 1` (after) | 21.1 s | 122 s | passed |

i.e. the worst individual test got ~3x faster and stopped failing, at the cost
of ~50 s of group wall clock — a trade worth making on a 3-core CI runner,
where 8 concurrent JIT compiles is 2.7x oversubscription. The absolute numbers
move a lot with ambient load: the same serialized group measured later at load
average >100 had a 100.6 s worst test and 172 s wall, still 52/52 green. Plan
against the 600 s per-test ceiling, not against these.

Verify the serialization still holds after any config edit:

```sh
cargo nextest show-config test-groups          # group: gui (max threads = 1)
cargo nextest run -E 'binary(micro_gui_window)' &
for i in $(seq 1 8); do sleep 3; pgrep -cf 'deps/micro_gui_window-'; done
```

## What the gui_blit frame budget measures

`demo::gui_blit_comprehensive_regression` ends with

```rust
assert!(observed.publish_us + observed.present_us < 10_000, …)
```

which looks like a latency SLO and is not one. The two halves:

* `publish_us` = `present_publish_ns_last()` = wall clock around the
  presenter's `publish()` body. Since publish went zero-copy (an `Arc` move
  out of the surface) this measures lock wait and host scheduling, not copy
  volume.
* `present_us` = wall clock of the watcher's `take_frame()`, i.e. a mutex
  acquisition plus an `Arc` clone — not a 4 MB copy, whatever an older
  comment in `helpers.rs` claimed.

Measured over 8 runs on an 8-core M3, dev profile, 1280x800, including 3x and
4x CPU oversubscription: `publish_us` 0-2 us, `present_us` 1-9 us. The 10 ms
ceiling is therefore ~1000x the observed envelope. It is a **canary against a
gross structural stall** (a full-surface copy reintroduced inside a lock, a
present that serializes on the compositor), and it explicitly does *not*
detect a single extra 4 MB copy — that is ~400 us here, 25x under the
ceiling.

Do not "fix" it by tightening (a ~100 us ceiling would catch one extra copy
and fail on a loaded runner) or by moving it off the push path (an assertion
that CI does not run is the exact hole the missing-fixture rule exists to
close). The load-sensitive part of this test was never this assert — it was
the frame *observation* one line above it; see
[docs/testing-model.md](testing-model.md#loaded-runner-behaviour).

## The two slow groups

**`gui` — `crates/wie-runtime/tests/micro_gui_window` (52 tests, 13 run
without RNotepad).** Serialized by the `gui` test group above; each executed
test JIT-compiles a real guest window (`gui_blit` alone is ~11-17 s of that).
The frame-hash assertions in this suite are real regression guards for the
present/paint/D3D9/GL paths — do not `#[ignore]` them to make a run faster.

**`guest` — the rest of the `wie-runtime` integration tests (45 tests).** They
spawn real guest PEs and run the JIT, without a serialization requirement.
Cheaper than `gui` (slowest 3.1 s), still far heavier than the unit tests.

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
