# Testing model

What the three test tiers are, which lane runs what, and the rule that keeps
the guest-PE integration tests from passing without running.

`docs/TESTING.md` is the how-to-run reference (commands, nextest profiles,
persistent JIT cache). This file is the model: the tiers, the coverage
contract, and where each tier runs.

## The three tiers

| Tier | Command | What it covers | Where it runs |
| --- | --- | --- | --- |
| **Fast lane** | `./scripts/test-fast.sh` | whole workspace suite minus the two slow groups (`gui`, `guest` — the `micro_*`, `clock_stub` and `idle_park_wake` test binaries) | inner dev loop, every save |
| **Full gate** | `./scripts/check.sh` | file sizes, `cargo fmt --all --check`, advisory clippy, the FULL `cargo nextest run --workspace` (slow groups included), `make -C micro-exes`, `scripts/run-micro-suite.sh` | **authoritative pre-PR gate** — locally and as the CI `check` job |
| **Nightly axes** | `./scripts/run-micro-suite.sh all --matrix` (plus the slow groups by hand) | the same guests re-run under `WIE_JIT_MEM=slow`, `WIE_JIT_MEM=pin` and `WIE_CPU=iced`; the full ~70-exe data-driven sweep; the 40 s fixed-window GUI harnesses | not on the push path — run before touching memory lowering, JIT chaining or CPU dispatch, and the target for a future scheduled job |

`scripts/check.sh` is the gate: nothing in the fast lane short-circuits any of
it, and a green fast lane never substitutes for it. The nightly axes are *not*
part of the gate because they multiply the wall clock (the `--matrix` leg
repeats a whole category three times, and the `iced` leg is multi-minute);
they are a targeted tool for the subsystems that have alternate backends, not
a blanket check.

## Guest PEs are build artifacts, not source

About 40 tests in `crates/wie-runtime/tests/` do not test host
code directly — they load a real Windows PE64 and assert on what the emulator
did with it. Those binaries are produced by cross-compiling C with mingw-w64:

```sh
make -C micro-exes                      # all targets → micro-exes/out/*.exe
make -C micro-exes gui_exes             # one category
./scripts/run-micro-suite.sh console_tests   # build + run one category
```

`micro-exes/out/` is gitignored. 32 of the executables happen to be tracked
because they predate that rule; the rest — including every GUI/OpenGL fixture
(`gui_blit`, `gui_d3d9`, `gui_demo`, `gui_menu`, `gui_text`, `gui_control`,
`gui_edit`, `gui_dialog`, `gl_quad`) — exist only after a local build. Toolchain:
Homebrew `mingw-w64` (`x86_64-w64-mingw32-gcc`, `x86_64-w64-mingw32-g++`); CI
installs it with `brew install mingw-w64` before building the fixtures.

A full `make -C micro-exes` is ~30 s of wall clock on four cores, which is why
CI builds the fixtures rather than caching `out/`: a stale cache of guest
binaries is a worse failure mode than a minute of compile time.

## A missing guest PE is a failure

Because the fixtures are untracked, "fixture absent" is the *default* state of
a clean checkout. For a long time the tests resolved their PE with
`path.is_file().then_some(path)` and returned early when it was missing, so a
checkout with no `micro-exes/out/` reported a green suite while the frame-hash
and behaviour guards never executed — and CI, which never built the fixtures,
ran them all as skips. The regression tests were load-bearing and invisible.

The rule now, implemented once in `crates/wie-runtime/tests/common/mod.rs` and
used by every test that consumes a guest binary:

* **`micro-exes/out/<name>` is required.** A missing fixture fails the test with
  the expected path and the `make -C micro-exes` command. 42 call sites across
  32 distinct fixtures, so a fixture-less runner fails up to 42 tests with the
  same message — which is exactly why CI probes for the cross compiler
  (`x86_64-w64-mingw32-gcc --version`, `…-g++ --version`) in a step of its own,
  *before* `make`: one clear failure there instead of 42 confusing ones in the
  test log. The probe is the guard; the panic is only the backstop.
* **`WIE_ALLOW_MISSING_GUESTS=1` is the documented escape hatch** for a
  developer without the mingw-w64 cross toolchain. It restores the old skip,
  after printing a `SKIPPED:` notice — the skip stays visible, it is just no
  longer the default. Never set it in CI.
* **`real_exes/<name>` is optional but never silent.** Those binaries
  (RNotepad, 7-Zip) are gitignored, fetched on demand with
  `./scripts/fetch-rnotepad.sh` / `./scripts/fetch.sh 7za`, and large, so a
  missing one skips — with a `SKIPPED:` line on stderr naming the path, so it
  cannot be mistaken for a pass. The escape hatch does not apply to them: they
  are never expected to be present.

Consequence: any change that adds a guest-PE test must also be runnable after
`make -C micro-exes`, and a red "missing guest fixture" message means the
toolchain or the build step, not the emulator.

**Known coverage hole, stated plainly:** 39 of the 52 tests in
`micro_gui_window` drive RNotepad from `real_exes/`, which CI never fetches.
They are green skips on every push, and under nextest the `SKIPPED:` line is
captured output on a *passing* test, so the CI log shows a plain `PASS`. Only
13 GUI tests — the ones holding the frame-hash guards for present/paint, D3D9
and GL — actually execute in CI. Fetching RNotepad in CI would close this; it
is not done yet because it means pulling a large proprietary binary into the
push path.

## What a loaded runner does

The first CI run of the GUI/OpenGL frame-hash group is the risky one: the
fixtures had never been built in CI, so those tests had always skipped, and a
GitHub `macos-latest` runner has ~3 cores against a development machine's 8.
Measured on an 8-core M3 with 24-32 burner processes (3-4x oversubscription),
back-to-back on the same host:

| | 8-way parallel | `gui` group, `max-threads = 1` |
| --- | --- | --- |
| worst single test (4x burners) | 64.9 s, `gui_blit` **failed** | 21.1 s, passed |
| worst single test (3x burners, ambient load avg >100) | — | 100.6 s, passed |
| group wall clock | ~70 s | 122-172 s |
| `gui_blit` publish+present | 0-2 us (ceiling 10 ms) | unchanged |

Two distinct load-sensitivities came out of that, and only one of them was the
one we expected.

**1. The frame *observation*, not the frame budget, was the flake.** The budget
assert (`publish_us + present_us < 10_000`) has ~1000x headroom over its
measured envelope and never came close on any run. What failed was the line
above it: `demo.rs` read the `FrameWatcher`'s channel with a single
non-blocking `try_recv` immediately after the guest loop, racing the sampler
thread. The presenter publishes on its own thread, so on a loaded host the one
frame that matters can land *after* `ExitProcess` has already returned the
test to that line. Traced under 3x oversubscription: the sampler recorded the
correct frame at t+85.7 s, 86.5 s and 87.3 s in three consecutive runs, and
the test's read had already returned `None` in all three — 3 runs, 3 failures,
hash `0x9fb37202941f08da` correct in hand one moment too late. Fixed by
`FrameWatcher::finish(grace)` (`helpers.rs`), which waits for the sampler
instead of racing it; the same 3x load then passed 4 runs out of 4, and the
whole 52-test group passed 52/52 (worst test 100.6 s). The assertion is
unchanged and still has to
match the same hash on a genuinely published frame — the fix removes a race in
the harness, it does not widen what counts as a pass.

**2. The suite was never actually serialized.** `GUI_SUITE_LOCK` is an
in-process mutex, and nextest's default is process-per-test, so it excluded
nothing: `pgrep` during a run showed 8 concurrent `micro_gui_window-*`
processes, each JIT-compiling a guest window. The real serialization is now
`test-group = "gui"` + `[test-groups.gui] max-threads = 1` in
`.config/nextest.toml`, which nextest applies across processes. See
[docs/TESTING.md](TESTING.md#the-gui-suite-runs-one-test-at-a-time).

**3. Nothing could end a hung test.** `slow-timeout`'s `period` is a *label*;
`terminate-after` is the kill, and it was unset, so a hung test printed
`SLOW [> 60.000s]` every minute until something outside nextest stopped the
run. That is what the earlier "ran 2321 s, ended with SIGTERM" was. Every
scope now has a real ceiling (600 s profile-wide, 600 s for `gui`, 240 s for
`guest`); the slowest legitimate test anywhere in the repo is 100.6 s.
[docs/TESTING.md](TESTING.md#timeouts-what-actually-kills-a-test) has the
mechanism and the table.

## What CI runs

`.github/workflows/ci.yml`, one job on `macos-latest`, in order: checkout →
Rust → cargo cache → nextest → mingw-w64 (+ `--version` probes) →
`cargo fmt --all --check` → `check-file-sizes.sh` → advisory clippy →
`make -C micro-exes` → `cargo nextest run --workspace` →
`cargo build -p wie-cli --release` → `scripts/run-micro-suite.sh` for
`console_tests`, `pthread_tests`, `cpp_exes`, `dll_tests`.

The fixture build must precede the test step: with the rule above, a runner
without the fixtures fails dozens of tests instead of skipping them. The four
micro-suite categories are the cheap deterministic ones (14 executables total);
the `all` sweep, the `--matrix` legs and the fixed-window GUI harnesses stay off
the push path for the wall-clock reasons above.

`cargo nextest run --workspace` is the same invocation `scripts/check.sh` makes,
with no `--profile`, so CI and the local gate share one profile: the same two
overrides, the same `gui` test group, the same three timeouts. There is no
timeout or profile flag that only one of the two passes. The one intentional
difference is the last step: `check.sh` runs the full data-driven micro-suite,
CI runs four categories.

Clippy in CI is advisory, exactly as in `scripts/check.sh` — no `-D warnings`.
The workspace lint table (`Cargo.toml` `[workspace.lints]`) denies rustc lints
only; clippy runs at its defaults in both places, and the two invocations are
kept identical on purpose. A newer clippy therefore cannot turn CI red on its
own: the 14 `chunks_exact_to_as_chunks` warnings the 1.98 toolchain adds are
warnings, and `cargo clippy --workspace --all-targets` exits 0 with them
present. What *is* a real process risk is the pin itself —
`rust-toolchain.toml` says `channel = "stable"`, a floating pin, so a gate's
outcome depends on whatever Rust released last. Pin the toolchain (or pin the
clippy invocation's expectations) if a build's green/red should be a function
of the commit rather than of the release calendar.

## Known-dead fixtures

Not fixed here, listed so nobody mistakes them for a regression:

* `seh_exes` — the phony aliases `seh_access_violation` / `seh_div_zero` have
  no build rule and no C source: `micro-exes/seh/` holds only
  `cpp_exceptions/`, so those two executables can never exist
  (`make: *** No rule to make target 'out/seh_access_violation.exe'`). The
  category is deliberately not mapped in `run-micro-suite.sh`, so it fails in
  `make` exactly as it did before.
* `sib_copy` — listed in the Makefile's `.PHONY` but does not assemble at HEAD
  (pre-existing `%ah`/REX error). It is not in the `all` target's dependency
  list, so it does not break the build.

Reviving either needs new fixtures (or a compiler fix), not a test change.
