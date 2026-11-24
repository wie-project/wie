# ADR 0005 — Capability traits on the Win32 dispatch port

Status: **Accepted** · 2026-11-23 — implemented (`a4ba020`); stage 2 the same day
(whole-state bindings 476 → 470, ratcheted); stage 3 added `LastErrorAccess`
(470 → 439, ratcheted). Proposed 2026-10-01 · maintainability plan T3.4

## Implementation note (2026-11-23)

The three traits exist in a private `capability` module (the same
`#[expect(unreachable_pub)]` shape as `state`, because `port` is the published
surface): `ConsoleAccess` 7 verbs, `WindowDialogAccess` 22 verbs, `PresentAccess`
18 verbs (stage 2 grew `ConsoleAccess` to 8 own + 2 inherited = 10 and
`WindowDialogAccess` to 23, leaving `PresentAccess` alone; stage 3 added
`LastErrorAccess` with 2 verbs — see the later notes). How a handler obtains
one:

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
   particular cannot fail a reach. (Stage 2 measured those 476 against what each
   body actually touches: six were inside the three traits' reach, they are
   narrowed, and the remaining 470 are now a machine-checked budget. See the next
   section.)

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

## Implementation note — stage 2 (2026-11-23)

Stage 1 left **476** whole-state bindings
(`grep -rn 'let state = &mut \*ctx.state' crates/wie-winapi/src | wc -l`). Stage 2
asked each of them the only question that decides whether it may narrow at all:
*what does this body actually touch?*

**Method (so the number is checkable, not asserted).** For each binding, the
enclosing function's *reach* is every `state.<member>` and `state.<member>()` path
in its body, plus the same for every callee it passes `state` to, transitively.
Each path is then **covered** (a `PresentAccess` verb, a `ConsoleAccess` verb, a
`WindowDialogAccess` verb, or a `window_state().<dialog field>` the trait exposes
as a verb) or **outside** (everything else — `process`, `kernel`, `heap_state`,
`file_io`, `module_state`, a `DllId` slot, `window_state().windows`, …). A binding
is narrowable only when its reach is empty or entirely covered.

**Result: 476 → 470.** Six bindings were inside the three traits' reach. All six
are gone; no handler was rewritten to spell `ctx.state` inline instead of binding
it — that would lower the count without narrowing anything, and the count is only
worth having as a proxy for reach.

| Handler | Capability | Route it now takes |
| --- | --- | --- |
| `comdlg32/color.rs::handle_choose_color_a` | dialog | `ctx.dialogs().set_comm_dlg_extended_error` |
| `comdlg32/font.rs::handle_choose_font_w` | dialog | `ctx.dialogs().font_dialog_policy()` + `state_comm_dlg_none(ctx.dialogs())` |
| `comdlg32/file.rs::handle_get_file_name` | dialog | `ctx.dialogs()` for policy/error/path + the new `has_native_file_dialog_bridge` predicate |
| `user32/window/geom_ops.rs::handle_set_window_pos` | present | new `apply_z_order(&mut dyn PresentAccess, …)` helper |
| `ucrt/stdio.rs::handle_getchar` | console | `ctx.console()` → `ConsoleCall` (`stdin_read_char`) |
| `ucrt/stdio.rs::handle_fgets` | console | `ctx.console()` → `ConsoleCall` (`stdin_available`/`stdin_refill`/`stdin_is_live_host`/`stdin_take`) |

**Four trait edits, all verbs or parameters — no struct handed back.**

- `ConsoleAccess::stdin_read_char()` — `getchar`'s line-at-a-time rule (consume
  the byte, discard the rest up to `\n`) moved out of the `ucrt` handler into
  `console/stdin.rs`, where the buffer lives.
- `ConsoleAccess::stdin_is_live_host()` — `fgets` asks one yes/no question before
  deciding to block on the host; a predicate, not `file_io.stdin_mode`.
- `ConsoleAccess::stdin_refill(max_line)` — **parameterised, not widened.** The
  two callers have two buffering policies (`ReadConsole`/`ReadFile` 64 KiB,
  `ucrt`'s line reader the CRT's 4 KiB); folding one into the verb would have
  changed the other caller's behaviour, so the cap is spelled at each call site.
- `WindowDialogAccess::has_native_file_dialog_bridge()` — `GetOpenFileName` asks
  whether the host panel exists and must not reach the closure behind it.
  `print_dialog_bridge` is still unreachable: `PrintDlgW` is a wide handler.

**Why the other 470 do not narrow.** Every remaining binding reaches a member
that *no* capability exposes, which is this ADR's own decision ("no capability
exposes `process`, `kernel`, `heap_state`, `module_state`, or `dll_states`") and,
for `window_state`, its documented exclusion list. Counted per cluster:

| Cluster | Bindings | The reach that keeps it wide | Verdict |
| --- | --- | --- | --- |
| `kernel32` | 131 | `process.last_error` (most), `heap_state.heap`, `kernel.sync`, `kernel.threads`, `file_io.{open_files,volumes,virtual_files}`, `module_state` | outside every named capability |
| `user32` | 106 | `window_state().windows` / geometry / focus / capture / menus / `touch_window_mirror`; `post_message`; `message_queue` | `WindowDialogAccess` excludes window machinery by decision |
| `d3d9` | 74 | its own `DllId::D3d9` slot (plus present verbs, already narrowed) | a per-DLL slot has no capability |
| crate root | 74 | `ws2_32` 21, `wininet` 9, `comctl32` 9, `winmm` 7, `version` 7, `oleaut32` 6, and ten 1–2s — each its own slot or a host-side map (`wininet`, `winmm`, `ole32`) | same |
| `gdi32` | 43 | `gdi_state().{find_dc,alloc_dc,find_print_job_mut}`, `with_font_engine`, `window_state().{window_width,window_height}` | gdi state has no capability; the present half is already narrowed |
| `advapi32` | 13 | `process.registry_keys`, `registry.persist` | no registry capability |
| `ucrt` | 13 | `heap_state.heap`, `file_io.ucrt_files`, and `handle_stdio_common_vfscanf`'s cursor rewind | heap + the CRT's own file map |
| `comdlg32` | 8 | the interactive panel builders: `create_window_record`, modal frames, `heap_state.heap` for the DEVMODE/DEVNAMES blocks | genuinely multi-capability (dialog **and** windows **and** heap) |
| `dinput` | 7 | its own slot | same |
| `pthread` | 1 | `kernel.threads` | no kernel capability |

Two consequences of that table are worth stating plainly:

1. **The single largest blocker is the last-error alias.** 138 of the 476 bindings
   write `state.process.last_error`, and for 33 of them that write is the
   *entire* reach — they bind an eight-sub-state object to touch one `u32`. A
   last-error capability would be the highest-value next trait by an order of
   magnitude; it is deliberately *not* added here, because this ADR decides not
   to expose `process` and a trait invented to make a count look better is the
   failure mode this whole decision exists to avoid. (`ConsoleAccess` carries
   `set_process_last_error` because a console handler's failure paths are console
   work; the same argument does not stretch over `PresentAccess`.)
2. **A handler needing two capabilities keeps the wide borrow** — two mutable
   borrows of one state are impossible — so `comdlg32`'s panel builders stay wide
   even though both of their capabilities now have names. That is the rule, not a
   gap.

**The ratchet.** `scripts/check.sh --only state-bindings` (a policy step of the gate, right after
the port boundary, and its own named CI step) counts the same grep against
`MAX_WHOLE_STATE_BINDINGS = 470` and fails above it, printing the per-file census
of whatever is there so the next reader sees *where* the draft went. It is a
ceiling with a documented one-line raise for a genuinely multi-capability handler,
and it ramps down: when the count is under the budget it prints the number to pin,
so a narrowing cannot silently regress. Falsified before committing — adding one
binding made it exit 1 at 471, removing it returned 0 at 470.

**What moved in the stage-1 table.** `ConsoleAccess` now reaches 56
`ctx.console()` call sites (54 before) and `ConsoleCall` is the parameter type at
77 sites; `WindowDialogAccess` is reached by `ctx.dialogs()` at 13 sites (1
before) and is the parameter type of 1 helper; `PresentAccess` is the parameter
type of 1 helper and still has no context accessor, which is why present
narrowing happens at helper signatures. `comdlg32` still reaches `window_state()`
171 times (non-comment, measured on this branch; the 146/154 above were
`a4ba020`-era) — the dialog *plumbing* is now capability-typed, the panel
*building* is not, and that split is the honest one.

## Implementation note — stage 3 (2026-11-23): the last-error capability

Stage 2 ended with a measured floor of 470 and one finding it could name
precisely: **the largest single blocker was the last-error alias.** 138 of the
476 bindings wrote `state.process.last_error`, and for 33 of them that write was
their *entire* reach — an eight-sub-state borrow to store one `u32`. That is a
capability worth adding, and it does not require exposing `process`.

**The verb list came from the sites, not from the field.** Reading all 138, a
handler does exactly two things: read the alias (`GetLastError`,
`WSAGetLastError`) and write it (everything else). Nothing in the crate reads or
writes the active thread's slot from a handler — `grep -rn 'threads.active'`
outside `state/` finds only `tls_values`, a different field — because the
thread-slot verbs and the publish step belong to the *dispatch boundary*:
`absorb_guest_last_error` before a dispatch and `publish_last_error_to_guest`
after it, in `wie-runtime`. So the trait is two verbs:

| Verb | Reaches | Sites |
| --- | --- | --- |
| `last_error()` | `process.last_error` (read) | 2 |
| `set_process_last_error(u32)` | `process.last_error` (write) | 136 |

**The fold-at-boundary rule is load-bearing, and it is why two verbs suffice.**
A handler writes the *alias* and returns; after the handler returns,
`publish_last_error_to_guest` copies `process.last_error` into the active
thread's slot and then into the engine's GS-relative TEB word, so the guest sees
the value without the handler ever touching the slot. A trait that also exposed
the slot would invite handlers to write the same value twice and would blur the
one case where the alias is deliberately stale — thread activation, where
`set_active_last_error` writes the slot *alone* because the alias still holds
the previous thread's value. That seam is `wie-runtime`'s and holds
`&mut WinApiState`; it needs no verb here and gets none.

**Reconciliation: the console verb was this verb.** `ConsoleAccess` declared its
own `set_process_last_error` with the fold-at-boundary reasoning written out on
it. Same receiver, same effect, same justification — so `ConsoleAccess` is now a
**subtrait** of `LastErrorAccess` and the duplicate declaration is gone. Console
handlers (`cx.state.set_process_last_error(e)`, ~30 sites) compile unchanged
through supertrait method resolution; there is one spelling, one doc, one
implementation.

**Route.** `ctx.last_error()` → `&mut dyn LastErrorAccess`, the shape
`dialogs()` has and *not* the shape `console()` has: the capability has no
struct to hand back, so there is nothing for the CPU port to sit beside. A
handler that needs the engine and the capability in the *same call* passes two
disjoint field reborrows (`&mut *ctx.engine`, `&mut *ctx.state`) — legal where
`ctx.last_error()` is not, because that borrows the whole context. Four call
sites do this; the constraint still lives in the callee's narrowed parameter.

**Result: 470 → 439.** 31 bindings narrowed; the 33 candidates minus two that
only *looked* last-error-only:

| Narrowed (31) | Where |
| --- | --- |
| `advapi32` (1) | `handle_open_process_token` |
| `kernel32/memory.rs` (5) | `VirtualAlloc`/`VirtualFree`/`VirtualProtect`/`VirtualQuery`/`OpenFileMapping` |
| `kernel32/file_io/vol.rs` (6) | `CreateHardLinkW`, `FindFirst/NextStreamW`, `DeviceIoControl`, `GetTempPathW/A` |
| `kernel32/file_io/time.rs` (2) | `FileTimeToLocalFileTime`, `FileTimeToSystemTime` |
| `kernel32/file_io/path.rs` (1) | the `Get*PathName` mock, plus `write_mock_string_{a,w}` narrowed to the capability |
| `kernel32/misc` (4) | `GetLastError`, `SetLastError`, `GetComputerNameExW`, `GetTimeZoneInformation`, `GetTimeFormatW`/`GetDateFormatW` |
| `kernel32/sync.rs` (3) | `InterlockedFlushSList`, `OpenEventW`, `FlushInstructionCache` |
| `kernel32/module.rs` (2) | `GetModuleFileNameA/W` |
| `kernel32` others (2) | `AssignProcessToJobObject`, `MultiByteToWideChar` |
| `wininet` (2) | `InternetSetOptionW`, `InternetQueryOptionW` (via `finish_false`, narrowed) |
| `ws2_32` (2) | `WSAGetLastError`, `WSASetLastError` |

| Left wide, and why (2) | |
| --- | --- |
| `get_module_handle` | also reads module state: `resolve_loaded_module_handle(…, state: &WinApiState)` |
| `load_library` | also *mutates* it: `resolve_or_load_dll(…, state: &mut WinApiState)` |

Both were flagged by the stage-2 script as last-error-only and are not: the
callee takes `state` in a position the script's call pattern misses. Reading the
body is what settled it, which is the rule for this whole exercise.

**What is left, and why it is a *decision* rather than a backlog.** 439 bindings
remain, each reaching a member no capability exposes:

| Blocker | Bindings it appears in | Why it is still there |
| --- | --- | --- |
| `window_state().windows` + focus/capture/menus/geometry/`touch_window_mirror` | 68 + 36 + 26 + 25 + … | `WindowDialogAccess` excludes the window machinery *by decision*; a capability for it is `user32`'s own seam, sized far larger than a scalar |
| `heap_state.heap` | 59 | every allocating handler needs it; a heap capability is a different design (the heap is `Arc<Mutex<_>>`, not a field) |
| `kernel.sync` / `kernel.threads` | 39 + 33 | no capability; the scheduler handle exists for the port, not for handlers |
| `file_io.{volumes,open_files,virtual_files}` / `process.{main_module_*,registry_keys}` | 32 + 22 + 21 / 26 | no capability; each is a struct, not a scalar or a verb list |
| each `DllId` slot (`d3d9` 24, `ws2`, `wininet`, `winmm`, `ole32`, …) | 24 + … | a handler that *owns* a DLL slot has no capability to take |
| `process.last_error` **plus** one of the above (~100) | 100 | the capability exists now, but the handler needs a second reach too, and two mutable borrows of one state are impossible |

### What this ADR now measures twice

The crate-wide narrowing this ADR implied is **one capability per state
cluster** — last-error, the window machinery, the heap, each `DllId` slot — not
three traits plus migration. Stage 2 measured that, and stage 3 confirmed it
from the other direction: adding the single highest-value capability moved 476
to 439 (-7.8%), not to zero, and the next cluster on the list (window machinery,
68+) is not a scalar but a small object graph. Three more capabilities would
cover the scalars; the rest is a different design question with its own cost,
which is why it is *not* decided here.

The Consequences section below is phrased to describe the seam these traits
actually are.

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
a bare `&mut`. Stage 3 (below) added a fourth on the same terms,
`LastErrorAccess`; the mechanism, the rule ("verbs, never the struct") and the
reversibility are unchanged, which is what makes a per-cluster capability an
incremental decision rather than a redesign.

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
- **These traits are seams for state clusters, not a crate-wide narrowing — and
  that is now measured twice.** The narrowing this ADR implied is *one
  capability per state cluster*: last-error, the window machinery, the heap,
  `kernel.sync`/`threads`, `file_io`, and each `DllId` slot. Four of them landed
  (`ConsoleAccess`, `WindowDialogAccess`, `PresentAccess`, `LastErrorAccess`) and
  together they take the crate from 476 whole-state bindings to 439. The
  remaining 439 are not migration debt: each reaches a member no capability
  exposes, so covering them is three more scalar-ish capabilities (window
  machinery 68+, heap 59, kernel 72) plus a per-DLL-slot decision — a separate
  design question, with its own cost, deliberately not decided by this ADR. The
  capability mechanism, not its result, is what this ADR settles.
- **The remaining reach cannot regrow silently.** `./scripts/check.sh --only
  state-bindings` fails when the binding count rises above a committed budget, so
  a new wide handler is a policy failure rather than a review miss — the same
  shape as the crate-layering and port-boundary checks.
- **The cost of a wide handler is now legible.** Because the budget prints the
  per-file census on failure, "how much unconstrained reach does this module
  carry?" is a number anyone can ask for; after stage 3 `d3d9` (74), `user32`
  (106) and `kernel32` (95) are the three answers worth watching, and
  `kernel32`'s fall from 131 is the last-error capability's visible effect.
- **The fold-at-boundary rule survives as documentation, not as convention.**
  `LastErrorAccess` states, in the trait's own doc, why a handler may write the
  process alias and never the active thread's slot — so the next handler author
  who wonders "should I also set the thread's value?" reads the answer where the
  verb is defined instead of inferring it from the runtime's dispatch code.
- **A cross-cutting scalar can be a capability without becoming a hole.** The
  last-error trait hands out two verbs over one `u32` and no `&mut ProcessState`,
  which is the test for the next one: a capability is a verb list or a scalar,
  never a struct with the old name.

## Validation

- The change is type-level, so `cargo check -p wie-winapi -p wie-runtime
  --all-targets` is the primary evidence: the compiler names every site that
  reaches past its capability.
- `cargo nextest run -p wie-winapi` — 1,320 tests (2,018 workspace-wide), none of
  which needed editing at either stage, since they reach state the same way the
  handlers do.
- `./scripts/check.sh --only file-sizes` and `./scripts/run-micro-suite.sh` unchanged
  baseline, since this touches no behaviour.

## Reversibility

**Additive and stageable** — the traits are introduced alongside the existing
accessors, and callers migrate one cluster at a time (console first, then
comdlg32, then the long tail). Each stage compiles and passes on its own, which
is what makes this reversible in practice: a bad stage is reverted by dropping
one migration, not by unwinding a single whole-crate commit.