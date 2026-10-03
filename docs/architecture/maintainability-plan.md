# Maintainability Plan

Status: **Waves 0–4 implemented · Wave 5 recorded as ADRs · four gaps open**
Evidence gathered 2026-10-01 against `main`; implementation status 2026-10-03.

Scope: whole workspace — 5 crates, 241,606 lines across 431 `.rs` files.

## Implementation status

Waves 0–4 are implemented and `./scripts/check.sh` is green end to end
(1,929 tests, 213 guest fixtures, `long_loop` budget). Wave 5's six decisions are
recorded as ADRs rather than applied blind — each is a multi-site refactor, and
`doc/adr/0004`–`0009` carry the reasoning.

**Two real bugs were found by implementing this plan, neither visible from
reading the code:**

- The JIT's UCRT `malloc`/`free` kept the guest heap layout in process-global
  `static`s. Two `RuntimeSession`s in one process shared a heap, and one's init
  wiped the other's large free list. Fixed in ADR-0004; `heap_isolation_tests.rs`
  fails before the change and passes after.
- The workspace could not compile for non-macOS: `winit` was ungated and worked
  on macOS only *by accident*, because that backend is unconditional. Fixing it
  also required cfg-gating the GUI module subtree — 136 errors to 0.

**Script layer consolidated 15 → 6.** The real defect was not the count but the
duplication: every check was declared twice, once in `check.sh` and once in
`ci.yml`, so a new one could land in one and be forgotten in the other.
`check-file-sizes.sh` and `check-deps.sh` are now functions inside `check.sh`.

**Nine figures in the original analysis were wrong** and were corrected against
measurement — most consequentially the soft-path census (650 names in
`names/mod.rs`, not 729, which is the wider cross-module set) and the
`encode_export` location (`fake_va.rs`, not `hooks.rs`). Where a claim would not
reproduce at all it was replaced with something measured.

### Open

| Item | Status |
| --- | --- |
| `lock`-shift/rotate test rows | 8 unencodable rows dropped; SDM, `mingw-as` and iced all agree they do not exist (`objdump` disagrees) |
| `exec_shift` locked branch | unreachable by the same argument; kept as a documented guard on the interpreter's fallback path |
| Paint-path visual coverage | pressed-button face, partial repaint, listbox banding, combo — in progress |
| Multi-threaded real guest for the large free list | not covered; proven only in-crate |
| Wave 5 ADRs | recorded, not applied — each needs its own change |

---

This document is a **register of changes**, not a design. Each entry states what was measured, what it costs, what the options are, and what to do. Items are grouped into four kinds, because conflating them is how refactors become unreviewable:

| Kind | What it changes | Review cost | Items |
| --- | --- | --- | --- |
| **A — Documentation lies** | nothing; corrects a false statement | trivial | T0 |
| **B — Architectural** | a module, bounded-context, dependency-direction or public-surface boundary | large, needs an ADR | T3 |
| **C — Policy enforcement** | what CI can reject | medium | T1 |
| **D — Local hygiene** | no boundary moves; duplication/dead code inside one owner | small, mechanical | T2, T4 |

The ordering rule: **T0 before anything**, because three of the current docs actively route contributors away from the mechanisms that make the code maintainable. Then C, because enforcement is what keeps A from rotting back. Then B, one at a time, each with its own ADR. D is fill-in work.

---

## What is already healthy — do not refactor these

Recorded so a future change does not "fix" working code:

- **`guest_layout/`** (5,731 lines, 56 structs, 464 `offset_of!` const-asserts, zero structs without an assert block, 53 round-trip tests through a real `IcedCpu`). The `WIN32_FIND_DATA` disaster in CLAUDE.md is now structurally impossible. A `win_struct!` macro would trade a *proven* mechanism for an unproven one. **Leave it.** The only gap is 8 hand-computed offsets in `comdlg32/font.rs:725-741`.
- **SSE/FP semantics in `wie-cpu`** — `exec/sse_types.rs:27-264` + `exec/sse.rs:1131-1525` define each op once with an explicit ABI encoding, and *both* the interpreter (36 call sites) and the JIT lowering (7 sites) consume it. This is the exact pattern the integer core lacks. It is proof the refactor works here, not a reason to doubt it.
- **Present/publish sink** — `present::PresentState::publish` (`present/mod.rs:933`) is the single surface sink used by GDI `BitBlt`, `d3d9/raster.rs`, and `opengl32.rs`. One decision, honoured everywhere.
- **The `d3d9_render` ↔ `opengl32_render` relationship** — the GL backend is a deliberate *sibling* of the D3D9 core, sharing `IDENTITY`/`Mat4`/`ScreenVertex`/`clip_to_viewport` (`opengl32_render/mod.rs:27-30`), with the GL-vs-D3D depth reconciliation documented. Not a fork.
- **`quantum.rs` ↔ `session`** — the scheduler imports `hooks`, `memory`, `mt_runtime`, never `session`. The coupling surface is `ProcessConfig` (6 fields, `mt_runtime.rs:228-243`). The healthiest seam in the codebase.
- **`glsl.rs` vs `d3d9_shader.rs`** — different source languages (GLSL ES 1.00 front end vs D3D9 bytecode tokenizer). Same shape, different domains. Unifying them would be false DRY.
- **`raw_input.rs` (1,699 lines)** — one concern, and a 50-line module doc that states the payloads are synthesized, enumerates the compromises, and explicitly forbids describing the lane as hardware input. Size costs nothing; splitting it would be motion.

---

## T0 — Documentation lies (do first; hours; zero risk)

Highest value per byte in this document. Each of these was verified against the tree.

### T0.1 — `CLAUDE.md:46` lists six lints that exist in no `Cargo.toml`

`unwrap_used`, `expect_used`, `panic`, `indexing_slicing`, `as_conversions`, `clippy::pedantic`. There is no `[workspace.lints.clippy]` section at all. **0 of 6 are enforced.** The paragraph reads as "this is what you must write to"; it is unbacked.

The per-site `#[allow(clippy::expect_used)]` markers scattered through test code suppress lints that were never enabled.

**Change.** Rewrite the paragraph to describe only what is enforced, and move the aspirational list into CONTRIBUTING.md under an explicit "aspirational, not enforced" heading.
**Trade-off.** none. Documentation only.
**Reversibility.** total.

### T0.2 — `CLAUDE.md:51` understates the `unsafe` surface

Counted as **code sites only** (doc-comment lines and `#[…]` attribute lines excluded):

| Crate | CLAUDE.md says | Measured code sites |
| --- | --- | --- |
| `wie-cli` | ~21 sites, 4 files | **46 sites, 4 files** |
| `wie-winapi` | 5 | **17 sites, 4 files** |

`wie-cli`: `gui/print.rs` 37, `gui/app/native_dialog.rs` 5, `commands/run.rs` 2, `gui/app.rs` 2.
`wie-winapi`: `console/host_term.rs` 9, `kernel32/sync.rs` 6, `ucrt/stdio.rs` 1, `dispatch_table/mod.rs` 1 — all annotated (16 `#[expect]` + 1 `#[allow]`).

This is wrong in the direction that *understates* risk, which is the wrong direction for a doc whose purpose is to constrain.

**Change.** Correct the numbers, and point at T1.1 (which closes the enforcement gap, not just the doc gap).

### T0.3 — `CLAUDE.md:77` misroutes every future API addition

The claim: `dispatch_table.rs` "claims it is auto-generated by `scripts/gen_winapi_dispatch.py`, but that script no longer exists — the file is maintained by hand. Adding an API means: a `WinApiId` variant, a dispatch arm, the handler in the right DLL module, and **registration in the fake-VA table**."

All three parts are false:

1. `grep -rn gen_winapi_dispatch` over the repo hits **only** `CLAUDE.md:77` itself. No file claims it.
2. `dispatch_table/mod.rs:1-6` says the opposite: the enum, the hot-path match, and the name rows "are all generated from the single declaration in `decl`".
3. Fake-VA registration is *derived*, not manual: `encode_export(id)`, defined at `fake_va.rs:1249`. `winapi_ids!` (`decl.rs:35`) emits enum + dispatch match + name rows from one row, with a compile-time count assert at `decl.rs:78-88` (510 variants / 512 discriminants / 511 name rows).

**Consequence.** This is the most expensive line in the repo's documentation: it sends every contributor to hand-edit four files in a mechanism that enforces one.

**Change.** Replace with: *"adding an API = one row in `winapi_ids!` (`dispatch_table/decl.rs`) + the handler; fake-VA encoding is derived."* Note the **soft**-path exception (see T3.6) — the dense path is DRY, the string-dispatch path is not.
**Note.** `docs/architecture/winapi-handling.md:55` carries the same stale claim (507 variants, maintained by hand) — actual is 510, macro-generated. Fix together.

### T0.4 — `CONTRIBUTING.md:15` states a 1500-line cap; the script enforces 2000

`scripts/check-file-sizes.sh:12` → `HARD_CAP=2000` (now `check_file_sizes()` in `scripts/check.sh`). The prose said "under 1500 lines (target: 1000)".

**Change.** Pick one. Recommend keeping 2000 in the script and restating prose as "hard cap 2000, target 1000", because lowering the script cap to 1500 fails the build today (`d3d9_render/tests.rs` 2650, `jit/tests/mod.rs` 2360).

### T0.5 — ~~`scripts/check-file-sizes.sh` exempts a path that does not exist~~ **DONE** (the script is now `check_file_sizes()` in `scripts/check.sh`)

`DATA_TABLES` lists `crates/wie-winapi/src/dispatch_table/names.rs`. The file is `names/mod.rs` (1,054 lines) — and is *under* cap anyway, so the exemption is both dead and unnecessary.

**Change.** Update the path or delete the entry.

### T0.6 — `docs/RUNBOOK.md` knob table has drifted in both directions

39 rows. Measured against `grep` of `WIE_*` reads across `crates/`:

- **5 rows name knobs that no code reads**: `WIE_COMPACT_STRING`, `WIE_D3D9_INPLACE` (never existed / removed silently); `WIE_JIT_DIRECT_REGS`, `WIE_JIT_TAILCHAIN`, `WIE_JIT_VERIFY` (genuinely removed — `RUNBOOK.md:114` *says so in prose*, so the table rows are residue, not drift).
- **10 live knobs have no row**: `WIE_ALLOW_MISSING_GUESTS`, `WIE_DEGRADE`, `WIE_GUEST_ENV`, `WIE_JIT_BG`, `WIE_JIT_EAGER_BLOCK_INSNS`, `WIE_JIT_SSA_FLAGS`, `WIE_JIT_TARGET_WORK`, `WIE_MT_DEBUG`, `WIE_NO_HOOK_SLICES`, `WIE_PRESENT_PACING_HZ`.

**Change.** Delete the two dead rows; fold the three removed-knob rows into the existing prose note; add an appendix block for the 10 undocumented tuning knobs — they are already documented in code doc-comments, so this is a pointer, not new writing.
**Why it matters.** RUNBOOK is the de-facto schema for 16+ knobs and nothing verifies it against code. T1.3 makes it self-checking.

### T0.7 — ~~`docs/notepad-support-plan.md` is a 287 KB worklog that reads as instructions~~ **DONE**

It contains ~10 references to `src/dispatch_table/names.rs` (now `names/mod.rs`), ~11 to `src/state/tests.rs` (now `state/tests/`), and `src/kernel32.rs` (now a directory).

**Change.** Move to `docs/superpowers/` or prefix with a "historical, superseded" banner. Reference material belongs in `docs/adr/`, `docs/architecture/`, `RUNBOOK.md`; plans belong out of the reference tree.

---

## T1 — Policy enforcement (this is what keeps T0 from rotting)

The measured state, verified per-crate:

| Declared in `[workspace.lints.rust]` (`Cargo.toml:42-54`) | wie-cpu | wie-winapi | wie-pe | wie-runtime | wie-cli |
| --- | --- | --- | --- | --- | --- |
| `unsafe_code = "deny"` | binds | binds | **inert** | **inert** | **inert** |
| `unused_imports`, `unused_must_use`, `unused_extern_crates`, `let_underscore_drop`, `rust_2018_idioms`, `rust_2021_prelude_collisions` | binds | binds | inert | inert | inert |
| `unreachable_pub`, `elided_lifetimes_in_paths`, `explicit_outlives_requirements` (warn) | binds | binds | inert | inert | inert |

Opted in: `crates/wie-cpu/Cargo.toml:41`, `crates/wie-winapi/Cargo.toml:28` — that is all. `wie-pe`, `wie-runtime`, `wie-cli` have **no `[lints]` section at all**.

**Machine-enforced rules in this repo, complete list:** `cargo fmt`, a 2000-line file cap, and rustc lints in 2 of 5 crates. `scripts/check.sh:16` and `ci.yml:83` both run clippy bare. Everything CONTRIBUTING rules 1/3/4/7 state is prose-only.

### T1.1 — Add `[lints] workspace = true` to the three crates that lack it

**Options.**

| Option | Effect | Cost |
| --- | --- | --- |
| **A. Add to `wie-cli`, annotate, then `wie-pe`/`wie-runtime`** (recommended) | 46 `wie-cli` unsafe sites become declared-and-annotated. `wie-pe`/`wie-runtime` have **0** unsafe, so they opt in free. | 4 files in `wie-cli` need `#[expect(unsafe_code)]` |
| B. Add to all three, deny globally, allowlist per-module | maximum enforcement | per-module `#![allow]` list grows; more to keep in sync |

**Recommend A**, `wie-cli` first. It is the only one of the three with `unsafe`, and the 46 code sites cluster in 4 files: `gui/print.rs` (37), `gui/app/native_dialog.rs` (5), `commands/run.rs` (2), `gui/app.rs` (2). (`gui/menu_bar.rs` and `gui/present_wgpu.rs` mention `unsafe` only in doc comments asserting they contain none.)

Two sites will need attention beyond a mechanical annotation:

- **`gui/print.rs:85` and `:91`** — hand-rolled `unsafe impl Send`/`Sync for PrintInfoEntry`, which holds an `NSPrintInfo`. These are the one genuine soundness surface in the crate. Either prove `PrintInfoEntry` is main-thread-confined (in which case the impls are wrong) or document the confinement invariant.
- **`gui/app.rs:880,884`** — `env::set_var("WIE_ROOT"/"WIE_DRIVE_D")` with a `// SAFETY:` line but no `#[expect]`. See T2.7: these two lines should be *deleted*, not annotated.

The existing 11 `#[expect(unsafe_code)]` attributes in `wie-cli` become *fulfilled* the moment the crate opts in, so this is not extra annotation work — it is finishing a migration that was started.

**Reversibility.** total (remove the `[lints]` section).

### T1.2 — Decide whether to add `[workspace.lints.clippy]`

This is the one item in this document that is a genuine policy choice, not a mechanical fix. CLAUDE.md and CONTRIBUTING rule 4 currently say clippy is advisory and that natural Rust beats lint-silencing — a defensible position.

**Options.**

| Option | Gain | Cost |
| --- | --- | --- |
| **A. Keep clippy advisory; delete the aspirational lint list from CLAUDE.md** (recommended) | docs match reality; zero churn | no new enforcement |
| B. Add a small curated table — `unwrap_used`, `indexing_slicing`, `as_conversions` — as `[lints.clippy]` table entries | three real hazards become machine-checked | lights up across 3 previously-unlinted crates: 547 `unwrap`/`expect`/`panic` sites in `wie-cpu`, 508 in `wie-runtime`, 2,487 in `wie-winapi`. One-off diff of thousands of lines |
| C. Full table per CLAUDE.md + `-D warnings` | matches the documented intent | not achievable in one change; ~3,700 sites |

**Recommend A now, B as a follow-up scoped to `wie-pe` and `wie-runtime` first** (the two smallest). Option B on `wie-winapi` alone is a multi-week project and would stall every other item here.

Note that `unwrap` counts are *not* automatically violations: `crates/wie-cpu` shows 547 sites of which the large majority are `RwLock::read().unwrap()` where a poison-tolerant idiom (`unwrap_or_else(|e| e.into_inner())`, `shared.rs:928`) is the local convention. B needs a per-crate decision about lock-poisoning first — see T4.6.

### T1.3 — Make the RUNBOOK knob table self-checking

**Precondition:** T2.8 (`knobs.rs`) so the read set is greppable in one file.

Add a test that parses the RUNBOOK table's knob column and asserts equality with the union of knob names the code reads.

**Trade-off.** the naive version greps source and false-positives on comments (6 of 16 knobs appear in a comment before any code). `knobs.rs` makes the set exact. This is why T2.8 precedes it.

### T1.4 — Add a dependency-direction test

**Zero architecture-boundary tests exist.** 1,884 tests, of which 104 (5.5%) live in `tests/`. `wie-cli` has **0** integration tests. Nothing asserts the crate flow, so the one invariant CLAUDE.md calls load-bearing is unverified by construction.

Actual edges (verified from all five `Cargo.toml`s):

```
wie-pe     → (none internal)
wie-cpu    → (none internal)          [dev: criterion]
wie-winapi → wie-cpu, wie-pe
wie-runtime→ wie-pe, wie-cpu, wie-winapi
wie-cli    → wie-pe, wie-runtime, wie-winapi   [+ winit, wgpu, muda, rfd, objc2*]
```

No cycles, no internal dev-deps, no feature-gated internal deps. One deviation: **`wie-cli` reaches around `wie-runtime` into `wie-winapi`** (`main.rs` calls `console::profile_sigint_armed()` / `ensure_hooks_installed()`), so the CLI can call any WinAPI handler directly, bypassing `RuntimeSession`'s lock and in-guest-callback invariants — the documented deadlock class.

**Change.** ~50 lines: run `cargo metadata`, assert the internal edge set equals a hardcoded list. No new deps. Optionally extend to forbid `wie_winapi::{user32,gdi32,present,...}::` in `wie-runtime/src` outside an allow-list (T3.5 makes that list short).

**Value.** Highest per line in the document. It converts a stated invariant into a failing build.

---

## T2 — Local hygiene (no boundary moves; mechanical)

### T2.1 — Extract `prepare_run` in `wie-cli` (lowest risk, highest value in T2)

The 4-line pattern `resolve_volume_config` → `stage_run_source` → `new_with_options` with `bottle_root`/`current_directory` is **copy-pasted 5 times** (`commands/run.rs:312,461,550`, `gui/headless.rs:33`, `gui/app.rs:877`) and **already diverged**:

- `app.rs` also mutates the process environment
- `headless.rs` passes `bottle_root` only
- `run.rs:462` uses `StageMode::ParentFolder` vs `StageMode::from_run_entry` elsewhere

**Change.** `fn prepare_run(path, root, drive_d, app_dir, guest_args) -> Result<PreparedRun>` returning `{ staged, session_options, volumes }`.

**Trade-off.** none — pure DRY, no behaviour change. **Do this one first.**

### T2.2 — `wie-cli` has no `[lib]` target — the highest-leverage structural change in the crate

`crates/wie-cli/Cargo.toml:9-11` declares `[[bin]]` only. 9,421 lines in `src/`, **108 tests all inline**, no `tests/` directory, so **no external test can import anything from `wie-cli`**.

Consequences, measured:
- `gui/present_wgpu.rs` (931), `gui/print.rs` (656, 44 unsafe), `input_script.rs`, `native_dialog.rs` have zero integration coverage.
- `resolve_run_root`, `reject_micro_only_flags`, `run_entry` are exercised only through `main.rs`'s own `mod tests`.
- `resolve_run_root_resolves_named_bottle` **creates and deletes a real directory on the user's filesystem** from a unit test.

**Change.** Add `[lib] name = "wie_cli"` + `src/lib.rs` re-exporting `commands` and `gui`; keep `main.rs` as a thin shim. Mechanical.

**Trade-off.** slightly more build plumbing; one place where the lib and bin must agree on module structure. **Reversibility.** total.

### T2.3 — Delete the two dead permission fields

`GuestRegion::perms` (`mem/region.rs:45`) and `MmapArena::perms` (`mem/arena.rs:29`) are **written, never read for any decision**. Verified: no `.perms` read exists outside `region.rs`/`arena.rs`, and the only read in `arena.rs` is `arena.rs:717`, a test asserting the value is stored.

**Why it matters beyond two fields.** `wie-cpu/src/mem/` carries **8 encodings of page permission** — 6 live (`RwxPerms`, `PageProtect`, `PageState`, `PageProtectMeta{allow_r,allow_w}`, the 2-bit `TLB_PROT_R/W` carried in three separate fields, the host `mprotect` frame table) and these 2 dead ones. `region.rs` and `vad.rs` *look like* permission sources by name and are not. Deleting the dead two removes a quarter of the mental model for a two-field diff.

### T2.4 — Name the W-on-X rule once

The load-bearing subtlety — `allow_w = allows_write() && !allow_x`, so stores cannot silently SMC through the TLB/pins — is documented only at `mem/rw.rs:286-288`, and **re-implemented independently** at `mem/map.rs:90` for pins. Two copies of a non-obvious correctness rule, no shared constant.

**Change.** one `const ALLOW_W: fn(PageProtect) -> bool` in `protect.rs`, called from both. 3-line change, called on TLB fill only so no perf cost. Add a test asserting the two paths agree.

### T2.5 — One `JitCtx::from_raw` for 27 `unsafe` sites

`jit/trampolines.rs` has **20 unsafe blocks with 5 SAFETY comments**. Twelve are the identical 4-line shape — `unsafe extern "C" fn tramp_X(ctx: *mut JitCtx) { let ctx = unsafe { &mut *ctx }; … }` at `:185,193,201,209,217,225,233,241,249,257,266,280,295`. The *invariant* is identical and stated 13 times or not at all. `jit/fast_api.rs` repeats it 7 more times with 2 SAFETY comments. `iced_cpu.rs` has 2 blocks, 0 comments.

**Change.** one `unsafe fn ctx<'a>(*mut JitCtx) -> &'a mut JitCtx` with one canonical SAFETY comment; 27 sites become one-liners.

**Counter-example — audit these two instead.** `jit/lower/mem.rs:103/155/194` (`wie_jit_load`/`wie_jit_store`/`wie_jit_string`) is the *only* place JIT code dereferences guest data, and it is exemplary: 14 unsafe, 14 SAFETY, generation re-check before every write. `jit/lower/tlb.rs` is 8/8. The discipline already exists; it just is not applied to the trampolines.

**Do not** introduce a sealed `unsafe` trait or a `#![deny]`+allowlist regime — `unsafe_code` is already module-scoped via `#![allow(unsafe_code)]` at `jit/mod.rs:13`, `mem/mod.rs:12`, `pipeline.rs:7`, `engine.rs:26`, and a new abstraction over 27 sites is more coupling than it removes.

### T2.6 — One `unpack_rgba`/`pack_rgba`

`0xAARRGGBB` is documented **25 times** across `d3d9_render/*` and `opengl32_render/*`, and each site re-implements the unpack: `d3d9_render/blend.rs:288` (three `(x >> 16) & 0xFF` in one function), `ps.rs:116`, `sample.rs` ×6, `opengl32_render/mod.rs:396/406`, `gdi32/text/mod.rs:179`, `gdi32/dib.rs:258`.

**Change.** one `color.rs` in `d3d9_render`, consumed by all three subsystems. Pure refactor of shifts — provably behaviour-preserving.

**Adjacent, separate decision:** texture upload is genuinely two pipelines (`d3d9/texture.rs:779,812,536` → `Vec<u32>`; `opengl32_render/sample.rs:5` → `Vec<u32>` with row flip). Worth sharing the *bilinear+clamp* subset only — D3D9 carries mip chains, GL deliberately does not. Moderate value, medium risk. **Defer.**

### T2.7 — Delete the two `set_var` calls in `gui/app.rs:877-886`

The GUI path mutates `WIE_ROOT`/`WIE_DRIVE_D` into the process environment, because the session is constructed on a spawned thread. But `SessionOptions` is `Send` (all fields are) and already carries `bottle_root`/`drive_d_root` (`session/mod.rs:41-44`) — which console, persistent and headless all use. The typed route was available.

**Change.** pass the values through `SessionOptions`. This removes 2 of `wie-cli`'s 47 unsafe sites (T1.1) *and* one env side-channel into the core, and makes the runbook-knob set smaller.

**Trade-off.** one env channel remains: `WIE_IDLE` is written at `commands/run.rs:472,559` and read by `wie-cpu`. Leave it; it is genuinely cross-crate.

### T2.8 — `knobs.rs`: one place the `WIE_*` read set is visible

16 distinct knobs at 17 sites across `wie-runtime` + `wie-cli`, read by bare `env::var` at point of use. Two partial centralisations exist (`RuntimeMemoryLayout::with_env_overrides` handles 2; two `OnceLock` getters). No `Config` struct exists.

**Options.**

| Option | Cost | Note |
| --- | --- | --- |
| **A. `knobs.rs` of `OnceLock`-cached getters, parsing stays in place** (recommended first) | ~1 day, zero call-site signature changes | makes the read set greppable in one file — the minimum needed for T1.3 |
| B. full `Config` struct with `from_env()`, injected into `SessionOptions`/`ProcessConfig` | 12 call sites across 8 files, changes `SessionOptions`' shape | proper ports-and-adapters; do after A |

### T2.9 — Deduplicate `winapi_state_default()`

Copied in 5 files: `state/tests/mod.rs:85`, `version.rs:991`, `kernel32/file_io/path.rs:786`, `advapi32/tests.rs`, `ole_clipboard.rs:417`. Move to `state/tests/mod.rs` and import.

### T2.10 — Split `user32/controls/button.rs` (pure `git mv`)

The file is **misnamed**: it holds `paint_control`, the *all-control-kind* entry point (`:24`), plus the status-bar strip/separator/parts painters (`:340-563`), plus BUTTON face/border/mnemonics. Two unrelated controls in one file. A developer looking for status-bar paint will not open `button.rs`.

**Change.** `paint.rs` (shared entry) + `button/` + `statusbar/`. Zero behaviour change, highest value-per-risk in the user32 area.

Also: split the ~25 `pub(crate) const` message/style values out of `controls/mod.rs:200-260` into `controls/consts.rs`; and move `comctl32.rs`'s status bar back into `controls/` — it currently imports `ControlClassKind`/`ControlState` back *out* of `user32::controls`, which is the clearest seam violation in the subsystem.

---

## T3 — Architectural (each needs its own ADR)

### T3.1 — `wie-cpu`: three mnemonic tables where one would do (ADR-[0007](../adr/0007-opclass-table.md))

| Table | file:line | distinct `Mnemonic::` |
| --- | --- | --- |
| interpreter dispatch | `exec/mod.rs:255-757` | 337 |
| JIT admission gate | `jit/block.rs:227-548` | 270 |
| JIT lowering dispatch | `jit/lower/insn.rs:277-838` | 230 |

(Counted uniformly as distinct `Mnemonic::` identifiers per file, so the three
are comparable to each other even though the absolute figures include matches
in doc comments.)

The tables are near-identical in shape and ordering, and operand *forms* are re-decided independently in each: `mov_is_lowerable` (`block.rs:952`) vs `exec/mod.rs:296` vs `lower_mov` (`insn.rs:307`). Same for `xadd`/`cmpxchg`/`bt*` (`:274-311`), shifts (`:769`), cmov (`:791`), setcc (`:803`), div (`:834`). Condition codes are written twice — `exec/ops.rs:45 cond_from` and `jit/lower/mod.rs:1446 cond_from_bits`, the latter enumerating `Jcc | Cmovcc | Setcc` triples in one match, so adding a condition means editing a 3-way triplicated list. Three parallel op enums exist for the same concepts (`exec/ops.rs:10/18/30`, `jit/lower/gpr.rs:55`, `jit/lower/insn.rs:37`) plus a fourth in `exec/x87.rs:131`.

**Measured facts that bound the work.**
- `jit/lower/` names a 248-mnemonic union; **all 248 exist in `exec/`** — the JIT never handles something the interpreter cannot.
- `block.rs \ insn.rs` = 41 mnemonics, all terminators handled by `lower_term`/`try_lower_inline_rep`, plus `Enter`/`Pushf`/`Popf` which have admission arms but no lowering arm and can only be rejected at lowering time.
- `insn.rs \ block.rs` = **exactly one**: `Cqo` at `lower_insn:316`. A dead arm.
- `exec` handles **89** mnemonics the JIT does not — the x87 family being the largest identifiable group, plus `In`/`Out*`, `Cpuid`, `Rdtsc`, `Ldmxcsr`/`Stmxcsr`, `Rcpps`/`Rsqrt*`, `Cmpp*`/`Cmpss`, `Mul`, `Clc`/`Stc`/`Cmc`. **That divergence is deliberate and correct**: 32-bit `div` needs implicit RDX:RAX as i128 (`block.rs:279-292`), implicitly-locked memory RMW needs `block.rs:883-908`, 64-bit `mul` is separate.

**The cost.** A bug fix in integer semantics needs 3 edits plus a matching `analysis::mark_insn_gprs` entry, and **missing the fourth silently emits `iconst 0`** — the crate documents this exact hazard at `block.rs:279-292`, and `jit/tests/implicit_operands.rs` exists purely to catch it afterwards.

**Options.**

| Option | Gain | Cost / trade-off |
| --- | --- | --- |
| **A. One `OpClass` table** (`Gpr{forms} \| Sse{forms} \| String \| Term \| Unsupported`) consumed by `is_lowerable` *and* `lower_insn`'s pre-check; leave `exec` alone | DRY on the two tables that must agree **by construction**; fixes the silent-`iconst 0` class | one lookup on the compile path — irrelevant, compile happens once per block. No semantic change, fully reversible |
| B. Extend the SSE pattern to flags: `exec` owns `flags_*` as pure functions, JIT calls them as Cranelift libcalls | true single source of truth | a host call per flag-setting instruction on the hottest path. `lower_arith_lazy` deferred-flag machinery exists *precisely* to avoid that. **Reject** — this one would regress `long_loop` against the CONTRIBUTING pin |
| C. Leave it; add a generated differential test over one `(encoding, expected)` list | KISS, zero perf risk | detects drift, does not remove the maintenance cost. **Pairs with A, does not replace it** |

**Recommend A, then C.** Do not attempt B.

### T3.2 — `fast_api.rs`: a guest heap allocator in the CPU crate, backed by process-global `static`s (ADR-[0004](../adr/0004-per-engine-guest-heap-layout.md))

`jit/fast_api.rs` is a UCRT reimplementation living in `wie-cpu`: `FastApiKind::{Malloc,Free,Memcpy,Strlen,AcrtIobFunc,Fwrite,Fflush}` (`:19-27`) matched by export name, and `wie_ucrt_malloc` (`:210`) implementing 24 size classes, bump allocation, large-list fitting and block headers against `guest_layout` constants.

Verified: `static HEAP_CTRL / HEAP_BASE / HEAP_END: AtomicU64` at `fast_api.rs:90-92`, set by `install_heap_layout` (`pipeline.rs:159`) — **`static`, not per-`JitCpu`**. `guest_layout.rs:1-13` justifies sharing *constants* across crates; it does not justify sharing *mutable heap state*.

**This is a latent bug, not only a layering smell:** two sessions in one process, or reconfigure-after-first-call, silently share a heap.

**Options.**

| Option | Gain | Cost |
| --- | --- | --- |
| **A. Keep in `wie-cpu`, move the 3 statics onto `JitCtx`** (recommended) | fixes the global-state hazard; keeps the coupling; small blast radius | none — `JitHeapLayout` is already a plain struct threaded through `configure_fast_path` |
| B. Move `fast_api.rs` to `wie-runtime` / a new `wie-ucrt` | proper separation of concerns | needs a `FastApiKind` edge that `IcedCpu` also wants → small enum or trait in `wie-cpu` |

**Recommend A now, B only if `wie-cpu`'s responsibility list is being revisited anyway.** This is the highest-severity finding in the crate and the cheapest to fix.

Adjacent, low value, do only if already in the area: merge `diag.rs` (208) + `profile.rs` (251) — one consumer, overlapping `WIE_*` gates.

### T3.3 — `user32/controls`: no trait, six kinds, five concerns each (ADR-[0008](../adr/0008-control-trait.md))

**Measured:** `grep -rn "trait " src/user32/ src/gdi32/` returns **1 hit**. There is no control abstraction.

`ControlClassKind` (`controls/mod.rs:223`) + a `ControlState` enum (`:356`) cover 6 kinds. One control's concerns live in five places:

1. state — `ControlState` variant, `controls/mod.rs:356`
2. seeding — `ControlClassKind::new_state`, `:284`
3. class resolution — `from_identifier`, `:249`
4. message routing — `ControlClassKind::dispatch`, `:718` (arms span `:760-1320`: Button 13, ListBox 9, generic `_` 11, Static 5, ComboBox 5, Edit 4 + a second dispatcher `dispatch_edit_message` called first at `:748`)
5. paint — `paint_control`'s `match kind`, `controls/button.rs:24`

**Literal duplication:** `controls/button.rs:96-155` (Button) and `:157-190` (Static) are the same 10-step sequence — `control_dirty_rect` → `fill_surface_rect_above_clipped` → `strip_mnemonics` → `TextGeom` → `paint_label` → `consume_control_invalidation`. The only deltas are face colour and text x-offset. Every paint bug fixed for one is silently unfixed for the other.

**Parallel invalidation schemes:** `LabelInvalidation` (`:566`: `Clean | Rect | Full`) for Button/Static/ListBox, and a separate `EditInvalidation` (`Clean/Full/Band`). Five parallel dirty-rect implementations; `consume_control_invalidation` (`button.rs:977`) must `match` three variants plus a `_ => {}` fallback *because the shapes are not uniform*.

**The seam is provably wrong:** `controls/button.rs` exports `label_invalidate_text_change` as `pub(crate)` and it is called from `user32/window/text.rs:85`; `controls/edit/state.rs:37` from `dialog/api.rs`; `comctl32.rs` reaches back into `user32::controls` for its state type. A control is not an encapsulated unit.

**What *is* already shared — keep it:** all text funnels through `render_control_text` (`controls/listbox.rs:99`) → `render_text_into_surface` → `gdi32::text`; font resolution uses one take/put idiom; z-order clipping has one implementation (`controls/paint.rs:68/108`) reused by `dialog/paint.rs` and `gdi32/blit.rs`.

**Options.**

| Option | Gain | Cost |
| --- | --- | --- |
| **A. `Control` trait: `paint` / `dispatch` / `dirty_rect` / `invalidate`**; each kind a unit struct registered in `ControlClassKind::impl_for()`; the enum stays as the registry | Open/Closed, one place per concern, kills the BUTTON/STATIC duplication | the 4 signatures genuinely differ — Edit's dispatcher *shadows* generic arms (`:744-760`), so `dispatch` needs an `Option<u64>` "not mine" return. This is a real design, not a mechanical refactor. 1-3 days |
| **B. Unify only invalidation** — one `DirtyScope { Clean, Full, Rect(..), Band(..) }`, one `dirty_rect()`/`consume()` pair | halves the parallel-scheme count, no signature churn | leaves the 5-place concern spread |
| C. `StatusBar` already demonstrates the failure: it lives in `controls/` for paint (`button.rs:60`, `:340-563`) and in `comctl32.rs` for messages | — | — |

**Recommend B then A**, B first because it is cheap and independently correct. T2.10 (the `button.rs` split) makes A easier — the shared `paint_control` becomes a clean seam.

### T3.4 — `state/window.rs`: the real god object is not `state/` (ADR-[0005](../adr/0005-capability-traits.md))

The premise to discard: `state/` is **not** a god module. `WinApiState` (`state/mod.rs:398-427`) has **8 fields**, and 16 of the 24 state types live in their own DLL module. `state/` correctly owns only what is cross-DLL: `KernelState`, `HeapState`, `FileIoState`, `ModuleState`, `ProcessState`, `DisplayMetrics`, `ClipboardState`, `WinApiEnvironment`.

The god object is **`WindowState`** (`state/window.rs:475`, **31 `pub` fields**) plus 33 free-floating request/pick/session types in the same file (18 dialog/messagebox bridge structs are defined at `:20-475`, *before* `WindowState` itself).

**Measured cross-boundary cost.** Handlers reach directly into other modules' state via `ctx.state.<accessor>()`; 196 `ctx.state.` sites outside tests, 1,100+ `state.<field>` total:

| From → into | sites |
| --- | --- |
| comdlg32 → window_state | **146** |
| kernel32 → console | **65** |
| user32 → present | 33 |
| gdi32 → window_state / present | 23 |
| d3d9 + d3d9_render + d3d9_shader → window_state / present | 14 |
| advapi32 → registry | 8 |
| shell32 → window_state / present | 7 |
| kernel32 → window_state | 5 |
| opengl32_wgl → present | 5 |
| user32 → clipboard/ole32 | 3 |
| comdlg32 → present | 3 |
| comctl32 → window_state | 1 |
| **total** | **~313** |

Two clusters dominate and both are boundary errors:
- **`comdlg32` → `window_state` (146).** `comdlg32/font.rs:85,202-207,467` reads *and writes* `WindowState` fields (`font_dialog`, `font_dialog_policy`, `file_dialog_loop_va`).
- **`kernel32` → `console` (65).** `ConsoleState` is a `DllId` slot owned by `src/console/`, but its 65 call sites live in kernel32 (`kernel32/console.rs:27`, `console_cells.rs:31`, `console_input.rs:7`).

**`HandlerContext` is a real port and the strongest asset in the crate** — 4 fields, one of which is `&mut dyn CpuEngine` (`state/mod.rs:879-888`). Its only weakness is that `&mut WinApiState` transitively exposes all 8 sub-states.

**Two further hazards:**
- Per-DLL state is `[Option<Box<dyn Any + Send>>; 16]` (`state/mod.rs:178`, `DllStateMap`), type-erased with lazy `get_or_init::<T>(DllId)`. **Passing `GdiState` where `PresentState` is expected is a runtime downcast failure, not a compile error.**
- The lock order `WinApiState → MessageQueue` (18 `state.lock_message_queue()` sites) and `WinApiState → PresentChannel` (`present/mod.rs:197-199`) is **prose in doc comments, not a type**. A violation deadlocks the macOS UI thread — the failure mode warned about at `state/window.rs:81,104,137,170,338`, `comdlg32/print.rs:422,751`, `comdlg32/file.rs:804`, `console/host_term.rs:85`. `sync_obj.rs:1012` notes holding `engine`/`winapi` mutexes while waiting deadlocks workers.

**Options.**

| Option | Gain | Cost |
| --- | --- | --- |
| **A. Extract capability traits off `WinApiState`** (`ConsoleAccess`, `WindowDialogAccess`, `PresentAccess`) returned by `HandlerContext` methods | the 313 sites become compiler-checked; the two dominant clusters get *named seams* | 313 mechanical edits |
| B. Move `WindowState` into `src/user32/state.rs` and console handlers into `src/console/` | bounded-context correctness | `DllId::Window` makes it a slot move, but breaks 146 + 65 sites and needs import resolution for console APIs |
| C. Make `WindowState`'s 31 fields private with accessors | stops *within-crate* pokes | breaks 146 in-crate + 37 in `wie-runtime` at once; no DLL-boundary win (both are in-crate) |
| D. Replace `Box<dyn Any>` with a typed enum or 16 named fields | compile-time safety | loses the documented lazy zero-cost-when-unloaded property (`mod.rs:164-172`) for ~16 fat pointers |

**Recommend A, then B for the console cluster only.** D is a genuine trade of a documented design property for type safety — pick it only if a downcast bug has actually occurred. A is the item that makes the boundary real.

### T3.5 — `wie-winapi`'s public surface does not insulate its internals (ADR-[0006](../adr/0006-winapi-port-module.md))

`lib.rs` (106 lines) re-exports ~60 names flat from `state` (`:86-100`) and 6 from `dispatch_table` (`:103-106`). `mod state` and `mod dispatch_table` are **private modules with wide `pub use`** — the module tree is hidden but the contents are not. `WinApiState`, `WindowState` (31 pub fields), `HandlerContext` and `DllStateMap` are public structs with public fields.

`wie-runtime` makes **402 `wie_winapi` references**. The legitimate port surface is ~10 names (`WinApiId`, `resolve_winapi_id`, `encode_export`, `decode_fake_va`, `WinApiState`, `HandlerContext`, …). The rest includes:
- **37 × `state.window_state()`** + 6 × `state.present()`
- 17 direct field pokes: `state.kernel.threads` (4), `state.file_io.volumes` (4), `state.process.main_module_{strings,menus,host_dir,dialogs,accelerators}` (5), `state.file_io.{stdin_mode,stdin_cursor,guest_io,bottle_root}` (6)
- **another module's handler called directly** — `user32::handle_get_sys_color`
- ~10 `present` internals, ~22 `sync_obj::*` names, 9 `handles::*` newtypes

**Consequence.** Any rename inside `state`/`dispatch_table` is a breaking change for `wie-runtime`. The module boundary provides an alias layer, not insulation. Every T3.4 refactor becomes cross-crate.

**Change.**
1. A `wie_winapi::port` module re-exporting exactly what `wie-runtime` should use; the rest `#[doc(hidden)]`.
2. A CI grep gate (T1.4) forbidding the deep reaches above an allow-list.

**Trade-off.** step 1 is pure re-export (zero cost, no behaviour change) but **cannot prevent new deep reaches** — it only makes the good path discoverable. Step 2 is what actually holds. Do 1 and 2 together or 2 alone.

### T3.6 — The API-registration spine: dense path is DRY, soft path is not (ADR-[0009](../adr/0009-census-derivation.md))

This corrects CLAUDE.md's framing in the *opposite* direction — the dense path was refactored and the doc still describes the old world.

**Dense path (510 APIs), genuinely DRY:** one row in `winapi_ids!` (`decl.rs:35`) emits the `WinApiId` variant, the hot-path `dispatch_winapi_id` match arm, and the name rows, with a compile-time count assert (`:78-88`). Fake-VA encoding is derived (`encode_export(id)`, `fake_va.rs:1249`). Adding a dense API = 1 row + the handler. CLAUDE.md's "4th place" does not exist.

**Soft / string-dispatch path, the opposite:** **~1,050 hand-kept names across 4 parallel registries.**

| Registry | file:line | size |
| --- | --- | --- |
| `winapi_ids!` rows | `dispatch_table/decl.rs:92` | 510 |
| soft export census lists (19 consts) | `dispatch_table/names/mod.rs` | **650 names** |
| `is_winapi_library` DLL allow-list | `names/mod.rs:768-808` | 27 DLLs + 2 prefixes |
| `is_winapi_implemented` oracle | `names/mod.rs:810-855` | 22 arms |
| `PREPLANTED_SOFT_APIS` | `dynamic_apis.rs:36` | 156 rows |
| `DYNAMIC_FAKE_APIS` | `dynamic_apis.rs:209` | 22 rows |
| per-DLL `*_EXPORTS` | `urlmon.rs:70`, `ntdll.rs:54`, `opengl32.rs:212`, wininet | 4 more copies |
| `CLASSIFY_TABLE` (stub rewiring) | `wie-runtime/src/guest_stubs/classify.rs:42` | ~99 rows |

**Failure modes, measured:**
- Forget a `*_DLL_EXPORTS` census row → **no runtime failure**. `is_winapi_implemented` is used in production only via `is_winapi_library` at `session/init.rs:508`, which has its own separate list. The oracle itself is used only by `commands/inspect.rs:132` and `tests/api_sets.rs`. Drift = wrong `inspect` output, invisible in `run`. **No test ties any census list to a dispatch match arm.**
- Miss a dispatch arm → `bail!("unsupported WinAPI call: {library}!{name}")` (`dispatch_table/mod.rs:205`) → runtime error string, worker exits 1. Not a panic, but a hard guest failure.
- Miss a `DllId` slot → compile error (array length derives from `DllId::COUNT`). Good.
- New DLL missing from `is_winapi_library` → its imports are treated as loadable guest modules → silent static-load attempt.

**Options.**

| Option | Gain | Cost / trade-off |
| --- | --- | --- |
| **A. Derive each census from its own `dispatch_*_extra`** via a `pub(crate) const EXPORTS` beside the match — the pattern `urlmon.rs:70` and `opengl32.rs:212` already use | kills 650 hand-kept names | 19 new consts; explicitness traded for mechanical derivation |
| B. Extend `winapi_ids!` to emit the census | one declaration, all consumers | the macro grows, and dense-vs-soft is a genuine distinction (soft APIs have no `WinApiId`) — may be the wrong unification |
| C. Leave it; add a test that cross-checks each census against a real dispatch attempt | cheapest, no structural change | detects drift, does not remove the maintenance cost |

**Recommend A, and C as the guard.** Whichever is chosen, the missing *test* (census ↔ dispatch arm) should land regardless — that is the cheapest fix in this whole section, and today it does not exist.

Also: `tests/api_sets.rs:47` duplicates `NTDL_EXPORTS` from `src/ntdll.rs:54` with a comment admitting it "must stay in sync". Import the original (needs `pub(crate)`, so it must stay an in-crate test). And `notepad-support-plan.md:445` tracks one known-stale code comment — the backlog is otherwise empty, which is a good sign.

---

> **ADR status:** T3.1→[0007](../adr/0007-opclass-table.md), T3.2→[0004](../adr/0004-per-engine-guest-heap-layout.md), T3.4→[0005](../adr/0005-capability-traits.md), T3.5→[0006](../adr/0006-winapi-port-module.md).
> T3.3 and T3.6 are now also recorded: [0008](../adr/0008-control-trait.md) and
> [0009](../adr/0009-census-derivation.md). For T3.6 the *guard* has already landed
> (`dispatch_table/census_tests.rs`, 11 tests) — what the ADR records is the
> remaining derivation of each census from its own dispatch arm.

## T4 — Test strategy, observability, CI

### T4.1 — `wie-cpu` test harness (converts a silent failure into a compile error)

277 tests, all in-crate. `jit/tests/` holds 119 across 14 files. The stated policy is "file-size policy, ADR-002" (`tests/mod.rs:1-2`) — but `mod.rs` is **2,360 lines / 52 tests / 60 helpers**, 2.6× the next largest, while `chain_tests.rs` (227 lines) holds **1 test**.

**There is no harness.** Four independent dual-backend harnesses exist: `atomic_tests.rs:66/108/128/133` (the most complete, and the only one with `open_worker`), `cmpxchg_tests.rs:144/158`, `mod.rs:802 simd_dual` + `mod.rs:1824 gs_teb_dual`, `chain_tests.rs:16`. `Backend`/`BACKENDS` are declared in `atomic_tests.rs` but consumed by `btx_tests.rs:479`, `implicit_lock_tests.rs:280`, `implicit_operands.rs:298` — already a de-facto shared fixture reached via `use super::*`.

**The important part.** `atomic_tests.rs:44-50` documents a subtle, *mandatory* constraint as prose: **distinct code VAs per distinct byte sequence**, because the interpreter decode cache is process-wide keyed on `(rip, generation)` and every test CPU starts at generation 0. It is honoured by hand in 5+ files. A contributor who gets it wrong gets a **silent wrong-execution failure, not a compile error**.

**Change.** `jit/tests/harness.rs` with `Backend`/`BACKENDS`/`Engine`/`open`/`open_worker` plus `alloc_code(cpu, bytes) -> u64` that returns a *fresh* VA per call. Removes ~24 copies of a 6-line `virtual_alloc(...).expect("alloc")` prologue and makes the aliasing hazard enforced. Then split `mod.rs` (the harness extraction shrinks it first).

### T4.2 — `wie-cpu`: three mechanisms for "cannot execute this", and they disagree

| Class | Mechanism |
| --- | --- |
| guest fault | interpreter: typed `StepResult::InvalidMemory` (`exec/mod.rs:96`); JIT: four sentinel fields into a `#[repr(C)]` struct read back by the host (`jit/lower/mod.rs:287-293`), harvested at `pipeline.rs:1098-1112`; both converge — **the one thing working well** |
| unsupported insn | interpreter: `degrade_fallback` (`exec/mod.rs:763`), env-gated, **returns `Ok(())` and advances RIP**; JIT: `Err("not lowerable")` (`insn.rs:837`) → `mark_never(rip)` **forever** (`pipeline.rs:180`) |
| malformed operand | interpreter: `CpuError::Message(String)`; JIT: `Err(String)` with **38 distinct ad-hoc literals** across 4 files |
| codegen reject | `shared.rs:866-895`: warn/debug rate-limited, retry at base tier, then `None` |

`CpuError` has a typed `Win32(u32, &'static str)` variant (`lib.rs:191-197`) documented as existing "so callers read a direct field instead of a string scan" — yet `exec/mod.rs` uses `Message(format!(...))` for every malformed operand and `mem/backend.rs` does the same for all map-arg validation. The typed variant is used only from `mem/vad.rs`. Two conventions inside one enum.

**Asymmetry worth closing:** a mnemonic the JIT cannot lower runs at interpreter speed **forever with no signal**; a mnemonic *neither* engine handles runs as a silent architectural no-op with one trace. **Change:** record unlowerable mnemonics into the `OPCODE_HISTO` the interpreter already samples (`pipeline.rs:43`) so one report shows both residues. ~10 lines, closes the observability gap.

Secondary: replace `Result<(), String>` in `jit/lower/` with `Result<(), LowerError>` where `LowerError` is `{ Unsupported, Form(&'static str), Verifier(String) }` — keeps the 3-valued Result (no sentinel), and turns `shared.rs:866`'s warn-vs-debug string split into a `match`.

### T4.3 — Gate or delete the untested dual raster path

`d3d9/capture.rs` (953) records ops on the emu thread; `present/stream.rs` (337) replays them on a render thread into its *own* backbuffer, with textures/shaders **snapshotted by clone** at record time (`capture.rs:20-25`, v1; `Arc` deferred). So there are two rasterisation paths for the same draw, gated by a flag `stream.rs:18-19` states the headless runs and micro-suite never set: *"headless runs and the micro-suite never spawn it"*.

`docs/status.md:46` already concedes the GUI half is untested — the 39 notepad tests "run headless through `RuntimeSession`, where the capture path never runs", and the sole render-thread claim "rests on the 2026-09-25 commit-path removal plus acceptance runs over one micro fixture — not on any test asserting the *absence* of a second render thread."

**Change.** Either make `capture.rs`'s ops the only record path, or put `present/stream.rs` behind the same gate the tests use so the untested path cannot diverge. One `AtomicBool`; fully reversible.

### T4.4 — Lock-order assertion (converts a UI-thread hang into a test failure)

The order `WinApiState → MessageQueue → PresentChannel` is prose in ~10 doc comments across 4 files. A debug-only thread-local lock-set that panics on `channel → state` turns an unreproducible macOS UI deadlock into a failing test.

**Trade-off.** KISS, but adds a thread-local to the dispatch path — scope it to `#[cfg(debug_assertions)]`.

### T4.5 — CI gaps

Running on push and PR: fmt, file-sizes, clippy (advisory), `make micro-exes`, `fetch.sh notepad`, nextest, release build, **4 micro-suite categories** (~14 of ~70 exes).

Local-only (`./scripts/check.sh` runs the full sweep): `--matrix` (`WIE_JIT_MEM=slow/pin`, `WIE_CPU=iced`) and the `long_loop` timing budget. Not in CI: `scripts/notepad-scenario.sh` (the only paint-path check), `bench.yml` (`workflow_dispatch` only). Note the script layer was consolidated 2026-10-01 from 15 scripts to 6 — `check-file-sizes.sh` and `check-deps.sh` are now functions inside `scripts/check.sh` (run one with `--only file-sizes` / `--only dep-direction`), and the 40 s GUI/bench harnesses were removed, so there is now **no automated perf-regression threshold** (recorded in `docs/RUNBOOK.md`).

**Three changes, ranked:**
1. **The `long_loop` perf pin.** CONTRIBUTING rule 2 makes it a pass/fail condition; nothing enforces it. Runner noise → false reds; mitigable with best-of-N on a quiet host (interference only ever adds time).
2. **A scheduled `all` + `--matrix` nightly.** `bench.yml` already proves `workflow_dispatch` triggers work; a `schedule:` is the same mechanism. `WIE_CPU=iced` is one of the two primary bisect switches and is never run in CI.
3. **A non-macOS compile check.** `crates/wie-cli/Cargo.toml:46-93` gates 8 deps behind `cfg(target_os = "macos")` with ~48 lines of commentary asserting non-Apple cleanliness. **Nothing verifies it.** `cargo check --target x86_64-unknown-linux-gnu` catches rot in the target-gating itself.

Also worth noting: **there are zero cargo features.** `grep -rn 'cfg(feature' crates/` → 0. Every variant is a runtime env var. That is defensible (documented at `RUNBOOK.md:118`, and features would multiply a matrix nobody runs — YAGNI), but it means no variant can be *proven* to build differently. **Do not** re-plumb this.

### T4.7 — FOUND BY IMPLEMENTATION (T4.5): `winit` is not platform-gated, and the workspace cannot build for non-macOS

This was not visible from reading the `Cargo.toml` and is the single most
useful thing the T4.5 work produced.

`crates/wie-cli/Cargo.toml:42` declares `winit` in **plain `[dependencies]`**:

```toml
winit = { version = "0.30", default-features = false, features = ["rwh_06"] }
```

`default-features = false` drops the X11/Wayland backends and `rwh_06` is a
raw-window-handle feature, not a backend — so on a non-Apple target winit 0.30
selects no platform and fails its own build:

```
error: The platform you're compiling for is not supported by winit
  --> winit-0.30.13/src/platform_impl/mod.rs:78
```

It works on macOS **by accident**: winit's macOS backend is unconditional, so
`["rwh_06"]` happens to suffice there. The comment above the entry — *"The
macOS backend compiles unconditionally"* — reads like a deliberate exemption. It
is in fact the one dependency the gating pass missed. The other eight
(`muda`, `rfd`, `objc2-app-kit`, `objc2`, `objc2-foundation`, `wgpu`,
`pollster`, `bytemuck`) are correctly gated.

**Measured:** `cargo check --workspace --exclude wie-cli --target x86_64-unknown-linux-gnu`
exits 0. The full `--workspace` fails, with `winit` as the only error. Verified
against a clean `git archive HEAD` tree, so it is not an artefact of the working
tree.

**The fix is larger than moving one line.** Moving `winit` into the existing
`[target.'cfg(target_os = "macos")'.dependencies]` table clears that error and
reveals ~132 `E0433 cannot find module or crate winit` errors, because
`gui/app.rs`, `gui/input_events.rs`, `gui/present_wgpu.rs`, `gui/input.rs` and
`gui/app/native_dialog.rs` use `winit` with no `cfg` gate on the module tree.

So the real change is: **the dependency move plus gating the GUI module
subtree**, which is a `wie-cli` refactor rather than a Cargo.toml edit. Until it
lands, `scripts/check-non-macos.sh` checks the four crates that do compile and
carries a tripwire that fires if that exclusion list goes stale — and the CI job
is deliberately **not** `continue-on-error`, because a soft-fail would hide the
very breakage it was added to detect.

This is the concrete payoff of the ~48 lines of target-gating commentary being
unbacked until T4.5: it was wrong, and nothing would have said so.

### T4.6 — Lock-poisoning convention, before T1.2 option B

~90 lock `unwrap()`s across `wie-cpu` (`shared.rs` 28, `cpu_engine.rs` 25, `pipeline.rs` 21) where the crate already demonstrates a poison-tolerant idiom (`unwrap_or_else(|e| e.into_inner())`, `shared.rs:928`; `degrade_locked_seen`, `exec/mod.rs:139-144`). Pick one convention — one `lock_unpoisoned(&Mutex<T>)` helper is the mechanical option — *then* decide whether `clippy::unwrap_used` can ever be a gate. Doing T1.2-B first means either fighting hundreds of sites or adding a clippy config that excludes lock unwraps, which is worse than having no lint.

---

## Suggested sequencing

| Wave | Items | Why this order |
| --- | --- | --- |
| **0** | T0.1-T0.7 | Correct the docs that misroute contributors. Hours. Land as one commit. |
| **1** | T1.1, T1.4 | `wie-cli` lint opt-in; dep-direction test. Both mechanical, both immediately prevent regressions. |
| **2** | T2.1, T2.2, T2.3, T2.4, T2.5, T2.7, T2.8 | Pure DRY and deletions. No boundary moves. Fixes T2.3 (dead fields) and T2.5 (27 unsafe sites) immediately. |
| **3** | T3.2, T3.6-test, T4.1, T4.2-histogram | Latent-bug fixes and the cheap structural guards. |
| **4** | T3.1 (A then C) | The largest single-DRY win; needs a benchmark before/after because it touches the JIT. |
| **5** | T3.4, T3.5, T3.3, T3.6 (A) | The real architectural work. **One ADR each.** T3.3 lands after T2.10. T3.4 and T3.5 should be sequenced together (T3.5's CI gate is what holds T3.4's refactor). |
| **6** | T1.2-B, T1.3, T4.2-LowerError, T4.3, T4.4, T4.5 | Policy tightening, once the codebase can absorb it. |

Explicitly **not** in this plan: unifying `glsl.rs` with `d3d9_shader.rs`; a `win_struct!` macro for `guest_layout`; collapsing `FastTlb` into `GenTlb` (the interpreter path is measurably slower set-associative — T2.3 deletes the dead fields instead); cargo features for JIT/iced/GUI; splitting `raw_input.rs`; introducing cargo features or a `win_struct!` macro; turning `WinApiControlSignal` into an enum return (a cross-crate protocol change, not a refactor — defer behind T3.4).

## Untraced

- `pump.rs:648-1330` — `SessionPumpHooks::dispatch` is **683 lines** of control-signal arms. Each drop/re-lock boundary is a candidate for a lock-order or double-lock bug that T4.4's assertion would surface, but none was audited individually.
- `guest_rewire.rs` / `guest_heap_accel.rs` / `guest_io.rs` / `guest_mbwc.rs` accelerator tables in `wie-runtime` were counted at file level only.
- The 313 cross-DLL state sites are a **count, not a complexity measure** — the handler bodies that use them were out of scope for the state lane.
- `WIE_COMPACT_STRING`, `WIE_VFS_DOWNLOADS`, `WIE_HOST_SLEEP`, `WIE_UI_LANGID` appear in `RUNBOOK.md` but not in any `src/` read; `WIE_UI_LANGID` *is* read via a helper form (`lang.rs:100 env_override`). The others need one more grep pass before being called dead.
- Whether any `#[allow(clippy::expect_used)]` marker is currently suppressing a lint that some *other* toolchain default enables — they are inert today under this workspace's config.