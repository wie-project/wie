# ADR 0008 — A `Control` trait over the five control concerns

Status: Proposed · 2026-10-01 — maintainability plan T3.3

## Context

WIE implements six Win32 control kinds (`Button`, `Static`, `Edit`, `ListBox`,
`ComboBox`, `StatusBar`) in `crates/wie-winapi/src/user32/controls/`.

**There is no control abstraction.** `grep -rn "trait " src/user32/ src/gdi32/`
returns one hit. A control is a `ControlClassKind` (`controls/mod.rs:223`) plus
a `ControlState` enum (`:356`), and each control's five concerns live in five
separate places:

1. state — a `ControlState` variant (`controls/mod.rs:356`)
2. seeding — `ControlClassKind::new_state` (`:284`)
3. class resolution — `from_identifier` (`:249`)
4. message routing — `ControlClassKind::dispatch` (`:718`, arms `:760-1320`)
5. paint — `paint_control`'s `match kind` (`controls/paint.rs`, after the
   T2.10 split)

**The duplication is literal.** The Button and Static arms of `paint_control`
were the same ten-step sequence — `control_dirty_rect` →
`fill_surface_rect_above_clipped` → `strip_mnemonics` → `TextGeom` →
`paint_label` → `consume_control_invalidation`. The only deltas were face colour
(`COLOR_BTNFACE` vs `COLOR_BTNFACE_PRESSED`) and text x-offset. Every paint bug
fixed for one was silently unfixed for the other.

**Invalidation runs in parallel schemes.** `LabelInvalidation`
(`Clean | Rect | Full`) covered Button/Static/ListBox while a separate
`EditInvalidation` (`Clean | Full | Band`) covered Edit — five parallel
dirty-rect implementations, and `consume_control_invalidation` had to `match`
three variants plus a `_ => {}` fallback *because the shapes were not uniform*.

**The seam was provably wrong.** `controls/paint.rs` exported
`label_invalidate_text_change` as `pub(crate)` for `user32/window/text.rs:85`;
`controls/edit/state.rs:37` was called from `dialog/api.rs`; and `comctl32.rs`
reached back into `user32::controls` for `ControlClassKind`/`ControlState`.
T2.10 closed the last of those by moving the status-bar dispatcher into
`controls/statusbar/`, but the concern-spread remains.

Adding a control means touching ≥5 sites, and cannot be done by adding one type.

### What is already shared — keep it

- All text funnels through `render_control_text` → `render_text_into_surface` →
  `gdi32::text`. No duplicated glyph rasteriser.
- Font resolution uses one take/put idiom.
- Z-order clipping has one implementation (`controls/paint.rs`), reused by
  `dialog/paint.rs` and `gdi32/blit.rs`.
- `PaintCtx` / `PaintFont` / `TextGeom` bundles exist to keep signatures under
  `too_many_arguments` — the right shape for the trait's arguments.

## Decision

Introduce a `Control` trait covering the concerns that are genuinely uniform —
**paint, dispatch, dirty-rect, invalidate** — with each kind a unit struct
registered in `ControlClassKind::impl_for()`. The enum stays as the registry, so
`from_identifier` and the `DllId`-style plumbing do not move.

### Ordering: unify invalidation first

Do the invalidation unification **before** the trait. Collapse `LabelInvalidation`
+ `EditInvalidation` into one `DirtyScope { Clean, Full, Rect(..), Band(..) }`
with a single `dirty_rect()` / `consume()` pair. It is cheap, independently
correct, and it removes the reason `consume_control_invalidation` needed a
fallback arm — which is a precondition for the trait's `invalidate` signature
being uniform.

### The design problem to solve, stated honestly

`dispatch` is **not** uniform. Edit has a second dispatcher
(`dispatch_edit_message`) that runs *before* the generic arms and shadows them.
So `dispatch` must return something like `Option<u64>` meaning "not mine", or
the trait must be split so each kind owns its whole message space. This is a real
design decision, not a mechanical refactor, and it is the main reason this ADR
does not simply say "extract a trait".

## Alternatives considered

- **A. `Control` trait over paint/dispatch/dirty-rect/invalidate.** Open/Closed,
  one place per concern, kills the Button/Static duplication. Cost: 1-3 days, and
  the `Option<u64>` shadowing question above.
- **B. Unify invalidation only.** KISS; halves the parallel-scheme count with no
  signature churn. Does not stop the five-place concern spread.
- **C. Leave as-is.** The T2.10 split already removed the worst navigation trap
  (`button.rs` no longer contains the status bar, and no file exceeds the 1000-
  line target). The cost is future-change friction, not present instability.

**Recommend B, then A, and stop at B if A's `dispatch` signature proves
contorted.** Partial adoption of a trait is worse than none — a half-trait
leaves both paths live — so B must either be a precursor to A or stand alone as
a consistency fix.

## Consequences

- A new control becomes one type plus a registry entry, instead of five
  coordinated edits.
- Partial-repaint correctness is fixed once rather than replicated per control.
- The `Option<u64>` "not mine" return makes Edit's shadowing explicit rather than
  incidental — arguably an improvement in its own right, since today it is
  ordering-dependent behaviour with no type-level statement of the precedence.

## Validation

1. `cargo nextest run -p wie-winapi` — 1,278 tests at time of writing. Adding a
   control kind must not require new test scaffolding beyond the trait impl.
2. **`scripts/notepad-scenario.sh` before and after, with a byte-identical
   frame** (`cmp` + SHA256). This is the strongest available evidence and it was
   used for T2.10 — T2.10 reproduced a byte-identical frame, which is the
   precedent.
3. **`cargo test` with a control-count assertion.** After extraction, assert the
   number of `impl Control` matches the number of `ControlClassKind` variants. A
   new kind that adds a variant without a trait impl becomes a compile failure —
   this is the only way the abstraction actually enforces anything.
4. Known coverage gap, stated so it is not mistaken for verification: one
   640×480 notepad frame does **not** cover pressed-button faces, partial-repaint
   dirty rects, listbox banding, or combo boxes. Those paths need targeted
   tests, not just the scenario script.

## Reversibility

High. Stage it: the trait can be introduced for paint only, with
`ControlClassKind::dispatch` left as the `match`, and the kind-by-kind migration
can be undone one impl at a time. A partially-migrated set is still compilable and
behaviour-identical at every step.