# ADR 0005 — Capability traits on the Win32 dispatch port

Status: **Accepted** · 2026-11-23 — implemented (`a4ba020`). Proposed 2026-10-01 ·
maintainability plan T3.4

## Implementation note (2026-11-23)

The three traits exist in a private `capability` module (the same
`#[expect(unreachable_pub)]` shape as `state`, because `port` is the published
surface): `ConsoleAccess` 7 verbs, `WindowDialogAccess` 22 verbs, `PresentAccess`
18 verbs. How a handler obtains one:

| Capability | Route from `HandlerContext` |
| --- | --- |
| console | `ctx.console()` → `ConsoleCall<'_> { engine, state: &mut dyn ConsoleAccess }` |
| window dialogs | `ctx.dialogs()` → `&mut dyn WindowDialogAccess` (1 call site; the rest of `comdlg32` receives the capability as a helper parameter) |
| present | **no accessor** — `PresentAccess` is implemented for `WinApiState`; handlers import the trait to call `publish`/`surface`/… on the state they already hold |

Stage 1 landed with it: `kernel32/console.rs`, `console_cells.rs` and
`console_input.rs` are now `src/console/{api,cells,input}.rs` (that directory also
gained `records.rs` and `stdin.rs` in the same move), and `kernel32/` contains no
console file. `DllId::Console`'s owner and its callers agree for the first time.

**Where the premise did not match the tree.** The Context section's measurements
were re-taken during implementation and three of them do not survive:

1. **The 1,768 total and the directory table do not reproduce.** Re-measured at
   `a4ba020^` (non-test sources, line comments stripped) the three plausible
   readings of the stated method give 1,879 (`state.<ident>`), 1,059
   (`state.<ident>(`) and 197 (`ctx.state.<ident>`). None is 1,768, and the first
   reading ranks `kernel32` (651) above `user32` (307) — the table's ranking, not
   just its totals, is unreproducible.
2. **The two cluster figures are method-dependent, and the ADR does not state
   the method.** Counting `window_state()` call sites in `comdlg32` gives exactly
   the ADR's 154, and `state.console` in `kernel32` gives exactly its 65; a
   different count gives 146 and 70, which is what the tree now cites
   (`capability.rs` on `WindowDialogAccess` and `ConsoleAccess`, and
   maintainability plan T3.4). The 76 that 65 is "of" reproduces under no reading
   tried (89 call-form, 644 all-forms). Treat every number in Context as
   unverified; take 146 and 70 from the code, not from here.
3. **The 313 sites did not become compiler-checked; three seams acquired names.**
   The ADR names "a capability that hands back `&mut WindowState` — the current
   `&mut` with a new name" as the design failure mode, and the delivered shape sits
   between the two:

   | Capability | How it reaches a handler | Narrows? |
   | --- | --- | --- |
   | `ConsoleAccess` | `HandlerContext::console()` → `ConsoleCall { engine, state: &mut dyn ConsoleAccess }`, 54 call sites | yes |
   | `WindowDialogAccess` | `HandlerContext::dialogs()` (1 call site) and helper parameters of type `&mut dyn WindowDialogAccess` (1) | at the helper only — the handler still binds `let state = &mut *ctx.state` and coerces at the call |
   | `PresentAccess` | imported by 15 files for its method names; 0 `dyn` parameters | no |

   And 476 sites in the crate still do `let state = &mut *ctx.state;`
   (`grep -rn 'let state = &mut \*ctx.state;' crates/wie-winapi/src | wc -l`), so
   the wide handle is still everywhere the handler body starts. That is a fair
   description of stage 1: the two dominant clusters have named, checkable seams;
   the crate does not have a capability-only dispatch path, and `PresentAccess` in
   particular cannot fail a reach.

The lock-order hazard this ADR's Context records as "prose in ~10 doc comments
across 4 files" was one statement of the order, in a "Lock ordering:" paragraph on
`PresentChannel` (`present/mod.rs:197-199` at the parent commit); the ~9 other
sites were `deadlock` warnings in the four files named, i.e. the hazard repeated
rather than the rule specified. It is now a type —
`crates/wie-winapi/src/lock_order.rs`, recorded as T4.4 in the
[maintainability plan](../architecture/maintainability-plan.md).

`WindowState`'s 31 fields are still `pub`, which is the deferral this ADR states
explicitly ("Explicitly not decided here"); nothing about that clause has
changed.

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