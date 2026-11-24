# ADR 0009 — Derive the export census from dispatch instead of hand-keeping it

Status: **Accepted** · 2026-11-23 — implemented for 11 of 19 modules (`a4ba020`);
the other 8 are the distinguished subset this ADR anticipated. Proposed
2026-10-01 · maintainability plan T3.6

## Implementation note (2026-11-23)

`dispatch_table/names/mod.rs` went **1,088 → 368 lines**
(`git show a4ba020^:crates/wie-winapi/src/dispatch_table/names/mod.rs | wc -l`)
and now holds only what is genuinely name-table shaped: `resolve_winapi_id`,
`winapi_id_export`, `is_winapi_library`, `is_winapi_implemented`, the test-only
`SOFT_EXPORT_CENSUS` table (21 library rows over 19 module consts — `setupapi`
serves `cfgmgr32` and `dbghelp` serves `imagehlp`), and its tests. Every one of
the 650 export names moved verbatim into the module that dispatches it.

Derivation strength, by measurement:

| Kind | Modules | Names | Property |
| --- | --- | --- | --- |
| single-source | 11: `winmm` 36, `ws2_32` 26, `dbghelp` 14, `setupapi` 12, `shell32` 11, `imm32` 11, `winhttp` 9, `crypt32` 8, `uxtheme` 4, `msimg32` 3, `dinput8` 1 | 135 | `soft_exports!` (`soft_dispatch.rs`) expands one `(name, handler)` row list into both `DISPATCH` and `pub(crate) const EXPORTS`; there is no second list to forget |
| distinguished subset | 8: `kernel32` 192, `ucrt` 154, `user32` 66, `oleaut32` 28, `gdi32` 25, `ole32` 22, `advapi32` 21, `comctl32` 7 | 515 | dispatch accepts names that must stay out of `wie inspect` output, with the reason stated at the list |

**Where the premise did not match the tree:**

1. **Option A's "~19 new consts" is the wrong shape for the majority.** In 11 of
   19 modules the census is not a const that happens to sit next to the match —
   it is a *macro expansion of the dispatch rows*. `dispatch ⊆ census` is
   therefore a compile-time fact there rather than a review discipline, which is
   stronger than the ADR asked for. In the other 8 the ADR's own prediction holds
   exactly: `census ⊆ dispatch` is testable, `dispatch ⊆ census` is unstateable,
   and that residue is review responsibility, not a gap to be closed later.
2. **The `urlmon`/`opengl32` precedent did not extend as far as Context implies.**
   Those two keep their census in-module, outside `names/mod.rs`, and they were
   *not* converted to `soft_exports!` — they are among the four module-own lists
   (`ntdll` 27, `urlmon` 4, `wininet` 17, `opengl32` 134) that this change leaves
   alone. "The target shape already exists here" was true of the file layout, not
   of the macro.
3. **The 650 / 729 / 496 figures in Context describe three scopes, and two of
   them no longer have a definition a reader can check.** The reproducible objects
   after this change are 650 names in 19 module consts (135 macro-derived, 515
   hand-kept) and the four module-own lists this change leaves alone (`ntdll` 27,
   `urlmon` 4, `wininet` 17, and `opengl32`'s macro-generated list). Adding those
   up does not produce 729 under any combination tried, and 496 only ever meant
   "650 minus UCRT's 154". Quote 650-in-19-consts, or a per-module number.

## Context

WIE has two API registration paths, and only one is a single source of truth.

**The dense path is already solved.** 510 APIs are declared through the
`winapi_ids!` macro (`dispatch_table/decl.rs:35`, invoked `:92`), which emits the
`WinApiId` variant, the hot-path `dispatch_winapi_id` match arm, and the
`(library, export, id)` name rows from a single row. A compile-time assertion
(`:78-88`) pins 510 variants / 512 discriminants / 511 name rows. Fake-VA
encoding is derived (`encode_export`, `fake_va.rs:1249`). Adding a dense API is
one row plus the handler.

**The soft path is the opposite**, and it is where nearly all new work happens.
It needs ~1,050 hand-kept names across parallel registries:

| Registry | Location |
| --- | --- |
| soft export census consts | one `EXPORTS` const per dispatch module (650 names in 19 consts, 496 excluding UCRT's 154) — 11 of them generated from the dispatch rows, see Implementation status |
| `is_winapi_library` | `names/mod.rs:49-89` — 27 DLLs + 2 prefixes |
| `is_winapi_implemented` | `names/mod.rs:91-139` — 22 arms, each reading a module's `EXPORTS` |
| `PREPLANTED_SOFT_APIS` | `dynamic_apis.rs:36` — 156 rows |
| `DYNAMIC_FAKE_APIS` | `dynamic_apis.rs:209` — 22 rows |
| per-DLL `*_EXPORTS` | `urlmon.rs:70`, `ntdll.rs:54`, `opengl32.rs:212`, `wininet` — already the target shape |

The two figures above differ by scope, not by error: 650 counts only
`names/mod.rs`'s non-test consts, while the census test (below) exercises the
wider set including the module-level lists — 729 across 21 lists, plus
`UCRT_CALLABLE`'s 154, plus the 156 + 22 dynamic rows.

**The failure modes are asymmetric, which is why nobody noticed.** Forget a
dispatch arm and the guest hard-fails at
`bail!("unsupported WinAPI call: {library}!{name}")` (`dispatch_table/mod.rs:205`).
Forget a *census row* and **nothing breaks at runtime** —
`is_winapi_implemented` is consulted only by `commands/inspect.rs:132` and the
api-sets test, so drift silently degrades `wie inspect` output while `wie run`
behaves perfectly.

## Decision (already implemented as the guard; derivation is the follow-up)

**Land the guard first — done.** `dispatch_table/census_tests.rs` (11 tests,
+17 net) drives real dispatch with zeroed registers for every name in every
registry. `Ok(Some)`, a handler `Err`, and a caught `panic` all count as
reachable; only `Ok(None)` fails, because only that becomes the `bail!`. A
duplicate-name check and an `is_winapi_library ⊆ (census ∪ module-own ∪
dense-only)` check landed with it.

**Then derive each census from the dispatch it claims to describe** — the
follow-up this ADR records. Each dispatch module owns a `pub(crate) const
EXPORTS` beside the `dispatch_*` it describes, and both `is_winapi_implemented`
and the census table read that constant instead of keeping a copy. How strong
the derivation is depends on the module: where every export maps onto one
`fn(&mut HandlerContext<'_>) -> Result<WinApiHandlerResult>`, the census is
generated from the same `(name, handler)` rows the dispatcher scans, so the two
cannot drift; where dispatch also accepts names that must stay out of the
`wie inspect` output (`IDataObject::*` vtable slots, `ordinal N`, `enumfont*`
aliases), the census is a distinguished **subset** of the accepted names and
stays hand-written, with the reason stated at the list. This is the shape
`urlmon.rs:70` and `opengl32.rs:212` already had for the first case.

Three findings from the guard that shape the derivation:

1. **A fresh fixture per probe is mandatory.** A panic caught inside
   `with_font_engine` strands the shared font mutex, deadlocking every later name
   that touches the same state. The probe cannot reuse one context.
2. **`ws2_32!select` blocks forever** on a zeroed argument block: arg 5 is a
   `timeval*`, and NULL means "no deadline", which is the real `select()`
   contract. It is exempt **by name**, and `the_blocking_probe_skip_list_stays_minimal`
   caps the skip list at 4 rows. A blanket per-probe timeout was rejected — it
   would swallow a genuine hang in any future handler.
3. **`comdlg32.dll`, `d3d9.dll`, `version.dll` are dense-only** with no soft
   census, while `wininet`/`urlmon`/`ntdll`/`opengl32` keep theirs in-module.
   Both are now explicit named sets rather than silent gaps.

## Implementation status (2026-11-23)

Option A has landed for the 650 names that lived in `dispatch_table/names/mod.rs`
(the file went from 1088 to 368 lines; the lists moved verbatim, same names in
the same order, so `wie inspect` output is unchanged). Each of the 19 consts now
lives beside the dispatcher it describes, and both `is_winapi_implemented` and
`SOFT_EXPORT_CENSUS` read it.

Two strengths of derivation, by measurement:

| Kind | Modules | Names | Property |
| --- | --- | --- | --- |
| single-source | 11: `shell32` 11, `winmm` 36, `ws2_32` 26, `crypt32` 8, `msimg32` 3, `imm32` 11, `uxtheme` 4, `winhttp` 9, `setupapi` 12, `dbghelp` 14, `dinput8` 1 | 135 | the `(name, handler)` rows in `soft_exports!` (`soft_dispatch.rs`) are the dispatch table *and* the census; a row is dispatched and advertised, and there is no second list |
| distinguished subset | 8: `kernel32` 192, `ucrt` 154, `user32` 66, `oleaut32` 28, `gdi32` 25, `ole32` 22, `advapi32` 21, `comctl32` 7 | 515 | dispatch also accepts names that must not be advertised (7 modules: `IDataObject::*` slots, `ordinal N`, size aliases, `enumfont*`, `createprocess*`, …; `user32` composes three sub-dispatchers), so the census stays hand-written with a stated reason |

Unchanged by this work: the four module-own lists (`ntdll` 27, `urlmon` 4,
`wininet` 17, `opengl32` 134), `is_winapi_library`, and the 156 + 22 dynamic
rows in `dynamic_apis.rs`.

What this does **not** buy: for the 8 subset modules, `census ⊆ dispatch` is
tested but `dispatch ⊆ census` is unstateable, because dispatch legitimately
accepts more names than the census may report. Making it a compile-time fact
there would change `wie inspect` output, so it stays a review responsibility.

## Alternatives considered

- **A. Derive each census from its own `dispatch_*_extra`** (recommended).
  Kills the hand-kept rows. Cost: ~19 new consts; explicitness traded for
  mechanical derivation.
- **B. Extend `winapi_ids!` to also emit the census.** One declaration for both
  paths. But the macro grows, and dense-vs-soft is a real distinction (soft APIs
  have no `WinApiId`), so this may be the wrong unification.
- **C. Leave the lists; keep only the guard.** Zero churn. The guard *detects*
  drift but does not remove the maintenance cost. This is the status quo the ADR
  deliberately improves on, and it is acceptable if A stalls.

**Recommend A with C as the standing floor.** The guard is what makes the lists
safe to leave; A is what makes them cheap.

## Consequences

- Adding a soft API stops being a four-place edit, which is the actual goal.
- ~1,050 hand-kept names become derived facts. The registry rows that matter are
  `PREPLANTED_SOFT_APIS` (156) and `DYNAMIC_FAKE_APIS` (22), where **order is
  ABI** — the guest observes slot positions. Those two are append-only by
  construction and must *not* be derived by sorting or hashing.
- `is_winapi_library` (27 DLLs) stays hand-written: it is a policy list, not a
  description of what exists, and deriving it would make the policy implicit.

## Validation

The census test is the artefact. It must be shown to fail: add a census row with
no matching dispatch arm, confirm the test names it, then revert. A guard never
seen failing is not evidence — the same standard applied to `check_deps()`,
which was drift-tested against a new edge, an internal dev-dep, and a removed
edge.

Second: the skip list must stay at ≤4 rows, enforced by the existing cap. If it
grows, the probe has started swallowing real behaviour.

## Reversibility

The guard is additive and self-contained — it can be deleted with no production
impact. Derivation is stageable per DLL: one `dispatch_*_extra` at a time, each
replacing one const, with the test green at every step.