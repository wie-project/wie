# ADR 0009 — Derive the export census from dispatch instead of hand-keeping it

Status: Proposed · 2026-10-01 — maintainability plan T3.6

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
| soft export census consts | `dispatch_table/names/mod.rs` — 650 names in 19 non-test consts (496 excluding `UCRT_CALLABLE`'s 154) |
| `is_winapi_library` | `names/mod.rs:768-808` — 27 DLLs + 2 prefixes |
| `is_winapi_implemented` | `names/mod.rs:810-855` — 22 arms |
| `PREPLANTED_SOFT_APIS` | `dynamic_apis.rs:36` — 156 rows |
| `DYNAMIC_FAKE_APIS` | `dynamic_apis.rs:209` — 22 rows |
| per-DLL `*_EXPORTS` | `urlmon.rs:70`, `ntdll.rs:54`, `opengl32.rs:212`, `wininet` |

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
follow-up this ADR records. Each `dispatch_*_extra` exposes a
`pub(crate) const EXPORTS` beside its match; the census is that constant. This is
already the pattern `urlmon.rs:70` and `opengl32.rs:212` use.

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