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

Roughly 35 integration tests in `crates/wie-runtime/tests/` do not test host
code directly — they load a real Windows PE64 and assert on what the emulator
did with it. Those binaries are produced by cross-compiling C with mingw-w64:

```sh
make -C micro-exes                      # all targets → micro-exes/out/*.exe
make -C micro-exes gui_exes             # one category
./scripts/run-micro-suite.sh console_tests   # build + run one category
```

`micro-exes/out/` is gitignored. 32 of the 76 executables happen to be tracked
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
  the expected path and the `make -C micro-exes` command.
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

## What CI runs

`.github/workflows/ci.yml`, one job on `macos-latest`, in order: checkout →
Rust → cargo cache → nextest → mingw-w64 → `cargo fmt --all --check` →
`check-file-sizes.sh` → advisory clippy → `make -C micro-exes` →
`cargo nextest run --workspace` → `cargo build -p wie-cli --release` →
`scripts/run-micro-suite.sh` for `console_tests`, `pthread_tests`, `cpp_exes`,
`dll_tests`.

The fixture build must precede the test step: with the rule above, a runner
without the fixtures fails ~35 tests instead of skipping them. The four
micro-suite categories are the cheap deterministic ones (14 executives total);
the `all` sweep, the `--matrix` legs and the fixed-window GUI harnesses stay off
the push path for the wall-clock reasons above.

Clippy in CI is advisory, exactly as in `scripts/check.sh` — no `-D warnings`.
The workspace lint table (`Cargo.toml` `[workspace.lints]`) denies rustc lints
only; clippy runs at its defaults in both places, and the two invocations are
kept identical on purpose.

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
