# ADR 0005 — Capability traits on the Win32 dispatch port

Status: Proposed · 2026-10-01 — maintainability plan T3.4

## Context

The premise to discard first: `state/` is **not** a god module.
`WinApiState` (`state/mod.rs:398-427`) has **8 fields**, and 16 of the 24 state
types live in their own DLL module. `state/` correctly owns only what is
genuinely cross-DLL: `KernelState`, `HeapState`, `FileIoState`, `ModuleState`,
`ProcessState`, `DisplayMetrics`, `ClipboardState`, `WinApiEnvironment`.

The real god object is `WindowState` (`state/window.rs:475`, **31 `pub`
fields**) plus 33 free-floating request/pick/session types in the same file.

`HandlerContext` (`state/mod.rs:879-888`) is a genuine port — 4 fields, one of
them `&mut dyn CpuEngine`. It is the strongest asset in the crate. Its one
weakness is that `state: &mut WinApiState` transitively exposes all 8
sub-states, so the narrowness holds only at the top.

Measured cross-boundary reach, by the directory a state reference originates
from (non-test sources, `state.<accessor>()` / `state.<field>` / the per-DLL
accessors):

| Origin | Sites |
| --- | --- |
| `user32` | **269** |
| `comdlg32` | **154** |
| `kernel32` | **76** |
| `gdi32` | 29 |
| `console` | 15 |
| `d3d9` | 14 |
| `comctl32` | 13 |
| `state` | 8 |
| `opengl32_wgl` | 8 |
| `advapi32` | 8 |
| `shell32` | 7 |
| `ole32`, `dispatch_table` | 3 each |
| `present`, `dinput` | 1 each |

Totals: **1,768** non-test `state.*` references in the crate. Two clusters
dominate, and both are boundary errors rather than necessities:

- **`comdlg32` → `window_state`** (154 sites). `comdlg32/font.rs:85,202-207,467`
  reads **and writes** `WindowState` fields (`font_dialog`,
  `font_dialog_policy`, `file_dialog_loop_va`).
- **`kernel32` → `console`** (65 of kernel32's 76). `ConsoleState` is a
  `DllId` slot owned by `src/console/`, yet its call sites live in kernel32
  (`kernel32/console.rs`, `console_cells.rs`, `console_input.rs`).

The per-pair attribution above the directory totals was measured by a coarser
grep than the pair counts originally estimated, and did not reproduce; the
directory distribution is the defensible figure, and the two clusters were each
confirmed by reading their call sites.

Two further hazards this decision does not resolve but must not make worse:

- Per-DLL state is `[Option<Box<dyn Any + Send>>; 16]` (`state/mod.rs:178`),
  type-erased with lazy `get_or_init::<T>(DllId)`. Passing `GdiState` where
  `PresentState` is expected is a **runtime downcast failure, not a compile
  error**.
- The lock order `WinApiState → MessageQueue → PresentChannel` is prose in
  ~10 doc comments across 4 files. A violation deadlocks the macOS UI thread.

## Decision

Extract **named capability traits** off `WinApiState` — `ConsoleAccess`,
`WindowDialogAccess`, `PresentAccess` — returned by `HandlerContext` methods, so
the cross-module reads become compiler-checked through a named seam rather than
a bare `&mut`.

Additionally, move console handlers into `src/console/` so that `DllId::Console`'s
owner and its callers finally agree.

### Explicitly not decided here

- **Whether to replace `Box<dyn Any>` with a typed enum or 16 named fields.**
  That trades the documented lazy, zero-cost-when-unloaded property
  (`state/mod.rs:164-172`) for compile-time type safety — ~16 words of fat
  pointer. Choose it only if a downcast bug has actually occurred; it is a
  separate decision with a separate cost.
- **`WindowState`'s 31 fields stay `pub` for now.** Privatising them is a
  *within-crate* change — both sides are in-crate, so there is no DLL-boundary
  win — at a cost of 146 in-crate and 37 `wie-runtime` sites. It becomes worth
  doing only *after* this ADR lands, when most pokes go through an accessor
  anyway.

## Alternatives considered

- **A. Capability traits** (recommended). mechanical edits across the clusters; the compiler
  then enforces the boundary and the two dominant clusters acquire names. The
  churn is real but it is churn without semantic change.
- **B. Move the state, add no traits.** Fixes ownership of the two clusters but
  leaves the accessor surface unchanged, so the pokes simply move to the new
  location. Solves the smaller half of the problem.
- **C. Make `WindowState`'s fields private.** No DLL-boundary win (both sides
  in-crate), large churn, and it would fight the capability accessors this ADR
  introduces.
- **D. Typed enum for `DllStateMap`.** Real type safety; loses a documented
  design property. Deferred by the same reasoning as above.

## Consequences

- The two dominant clusters gain *named* seams, which is what makes them
  reviewable: a reviewer can see that `comdlg32` depends on exactly
  `WindowDialogAccess`.
- "Which capabilities does this handler need?" becomes answerable by reading its
  signature — today it is answerable only by counting field pokes.
- Once the capability set is explicit, the lock-order prose (T4.4 in the
  maintainability plan) can become structural: the ordering constraint is a
  property of the traits, not of call sites.
- This is a prerequisite for keeping `wie-winapi` internals movable — see
  ADR-0006, which records why the crate's current public surface makes every
  such refactor cross-crate.

## Validation

- The change is type-level, so `cargo check -p wie-winapi -p wie-runtime
  --all-targets` is the primary evidence: the compiler names every site that
  reaches past its capability.
- `cargo nextest run -p wie-winapi` — 1,261 tests, none of which should need
  editing, since they reach state the same way the handlers do.
- `./scripts/check.sh --only file-sizes` and `./scripts/run-micro-suite.sh` unchanged
  baseline, since this touches no behaviour.

## Reversibility

**Additive and stageable** — the traits are introduced alongside the existing
accessors, and callers migrate one cluster at a time (console first, then
comdlg32, then the long tail). Each stage compiles and passes on its own, which
is what makes this reversible in practice: a bad stage is reverted by dropping
one migration, not by unwinding a single whole-crate commit.