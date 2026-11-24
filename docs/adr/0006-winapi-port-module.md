# ADR 0006 — A `port` module as `wie-winapi`'s published surface

Status: **Accepted** · 2026-11-23 — decisions 1 and 2 implemented (`a4ba020`);
decision 3 implemented after `30f68bc` (see its note). Proposed 2026-10-01 ·
maintainability plan T3.5

## Implementation note — decision 3 (2026-11-23, after `30f68bc`)

`wie-cli` no longer depends on `wie-winapi`. The edge is gone from
`crates/wie-cli/Cargo.toml`, from `Cargo.lock`, and from `EXPECTED_EDGES` in
`check_deps()` — where its absence is now *load-bearing*: re-adding the
dependency fails the step as an UNEXPECTED edge (verified by re-adding the line,
watching the step exit 1 and name `wie-cli -> wie-winapi`, then removing it).
`grep -rn wie_winapi crates/wie-cli/src` reads **0** at this commit (57 before).

What replaced it is `crates/wie-runtime/src/host.rs`, the session's host-facing
surface, in the three classes decision 3 asked for:

1. **Types the host names** — `Dimension`, `Hwnd`, `SurfaceFrame`,
   `DisplayMetrics`, `VolumeConfig`, the dialog policies and their
   request/pick payloads: re-exported from `wie_winapi::port` where `port`
   publishes them, and from the crate root where it does not (those payloads
   are host callback data with no guest-visible layout, so `port` was never
   their right home).
2. **Host services** — `host_path_to_guest`, `global_bottle_root`,
   `drive_letter`, `profile_sigint_armed`, `set_raw_mode`, … re-exported. Pure
   functions over host paths / the host tty: no guest state, no big lock.
   `bottle::guest_path_to_host` is aliased `bottle_guest_path_to_host` because
   `wie-winapi` exports a *different* `guest_path_to_host` (the `VolumeConfig`
   one) under the same name.
3. **Work that belongs to the session** — the synthesized raw-input lane. The
   CLI used to call `user32::raw_input::{post_raw_keyboard_event,
   post_raw_mouse_event, is_legacy_*_excluded}` and post the `WM_INPUT`s itself;
   those are now `GuestHandle::{post_raw_keyboard, post_raw_mouse_move,
   post_raw_mouse_button, post_raw_mouse_wheel, legacy_keyboard_input_excluded,
   legacy_mouse_input_excluded}`, which encode the record, apply the
   registration filter, and deliver through `post_message` (queue mutex only,
   never the big one). The `RAWINPUT` vocabulary — `RawInputPost`,
   `RawMouseReport`, `RawMouseButton` — no longer crosses a crate boundary.

**Where the premise still does not hold:** `port` publishes neither `WinMsg`
(the `WM_*` vocabulary the host derives its message-id constants from) nor
`IRect` (the type of `SurfaceFrame::region`), so `host.rs` names those two
through the gated module paths. That is one file and it is recorded as a single
scoped entry in `check_winapi_port`'s allow-list, with the follow-up named
there: publish both in `port` §6, then delete the entry. So the win here is the
*dependency edge*, machine-enforced; the port surface itself still needs those
two names to be complete for host consumers.

`wie-cli`'s own test count did not move (115 unit/integration tests, all in-crate
unit tests plus `tests/run_entry.rs`; the crate has no end-to-end guest runs of
its own — those live in `wie-runtime`'s `micro_gui_window`). `wie-runtime` is
189/189. Behaviour is unchanged by construction: every call site keeps its
arguments, its ordering, and its `WM_INPUT` debug log (same `wiegui` target,
same fields — it moved one frame down the stack, into the session).

## Implementation note (2026-11-23)

`crates/wie-winapi/src/port.rs` publishes **139 names in eight sections**, each
stating why the boundary crosses there. The crate root went from 63 flat `state`
names to **13** (`pub use state::{…}` in `lib.rs`), the other `state` *types* are
nameable only crate-internally, and `mod state` carries
`#[expect(unreachable_pub)]` — scoped, not `allow`, so if an edit makes one of
those names reachable again the crate stops compiling.

What did **not** become opaque: the structs are still `pub` types with `pub`
fields. `WinApiState` has 8 public fields and `WindowState` 31, so a field poke
compiles; the gate is what stops seven of those classes (`heap_state`, `file_io`,
`module_state`, `dll_states`, `message_queue`, `kernel.threads`, `display`) from
appearing in `wie-runtime`. Two are listed in `check_winapi_port` as known
ungated debt rather than pretend-fixed: `state.kernel.sync` (the scheduler drives
`SyncState`'s fields directly; 27 lines in `wie-runtime/src` mention it, and
`check_winapi_port`'s comment records ~45 call sites) and the `WinApiState` struct
literal at `crates/wie-runtime/src/memory.rs:555`.

`wie-runtime` was migrated onto it: `grep -rn wie_winapi crates/wie-runtime/src |
wc -l` reads 408 at `a4ba020` (400 at its parent), and
`./scripts/check.sh --only winapi-port` reports 0 gated-module paths and 0 field
pokes with an **empty** allow-list. That gate covers `crates/wie-runtime/src`
*and* `crates/wie-runtime/tests`, in two rules (module-qualified reach, including
the brace-group import form; and `WinApiState` field pokes). It is wired into
`scripts/check.sh` as step 3 of 9 — **not** into `.github/workflows/ci.yml`,
which runs only the `file-sizes` and `dep-direction` policy steps.

**Where the premise did not match the tree:**

1. **"The legitimate port surface is roughly twelve names" was never
   achievable.** The runtime legitimately constructs ~35 state types, names the
   kernel-object vocabulary, and drives the window/presenter seam. The delivered
   surface is 139 names, and the honest description of the win is narrower: 26
   `WinApiState` accessors replaced the field pokes (impl `WinApiState` grew 41 →
   67 `pub fn`), handler calls the runtime makes itself now go through
   `dispatch_winapi_id` (6 call sites, against 1 at the parent commit), and the
   *rename* cost is what went away — the surface itself did not collapse to
   twelve.
2. **The reference count is not 405.** At `a4ba020^`, `grep -rn wie_winapi
   crates/wie-runtime/src | wc -l` = 400; including `tests/` it is 503. The 405
   in Context reproduces under neither reading.
3. **Decision 3 did not land at `a4ba020`** — recorded here as history. It
   landed after `30f68bc`; see the decision-3 note above.
4. **One item of Context is now stale in the other direction:** the ADR records
   `user32::handle_get_dc` as a direct handler call at `session/mod.rs:930`. It
   is gone — the gate forbids that shape, and the runtime reaches the same
   behaviour through `dispatch_winapi_id`.

Two dead types were found only because the surface stopped publishing them and
the compiler then had nothing to say about them. `HeapAllocation` had two lines
in the whole repo — its definition in `state/process.rs` and its re-export in
`lib.rs` (`git grep -c HeapAllocation a4ba020^`) — and is deleted.
`WindowsHookRecord` stored four fields that nothing read, because nothing
dispatches through a hook; it is now `struct WindowsHookRecord { handle: u64 }`,
which is all any caller ever asked for.

## Context

`crates/wie-winapi/src/lib.rs` is 106 lines and re-exports ~60 names flat from
`state` (`:86-100`) and 6 from `dispatch_table` (`:103-106`).

`mod state` and `mod dispatch_table` are **private modules with wide `pub
use`**. The module tree is hidden; the contents are not. `WinApiState`,
`WindowState` (31 `pub` fields), `HandlerContext` and `DllStateMap` are all
public *structs with public fields*, not opaque types.

`wie-runtime` makes **405 `wie_winapi` references**. The legitimate port surface
is roughly twelve names: `WinApiId`, `traits`, `resolve_winapi_id`, `winapi_id_export`,
`encode_export`, `encode_alias`, `encode_unresolved`, `decode_fake_va`,
`WinApiState`, `HandlerContext`, `WinApiEnvironment`, `is_winapi_library`.

The rest is deep reach. **37 × `state.window_state()`** and 6 × `state.present()`
are state accessors, and **23 direct field pokes** reach past the accessors
entirely — `state.kernel.threads`, `state.file_io.volumes`,
`state.process.main_module_{strings,menus,host_dir,dialogs,accelerators}`,
`state.file_io.{stdin_mode,stdin_cursor,guest_io,bottle_root}`, and others.

Per-module unique imported names, measured:

| Module | Deep names | Examples |
| --- | --- | --- |
| `present` | **11** | `PresentChannel`, `MessageQueue`, `SurfaceFrame`, `spawn_capture_streamer`, `set_frame_timing_enabled`, … |
| `user32` | 5 | `Dimension`, `dragdrop`, `fake_system_metric`, `handle_get_sys_color`, `menu` |
| `kernel32` | 5 | `clock`, `mount_host_file`, `open_guest_path`, `resolve_cs_queue`, `resolve_wait_target` |
| `dll_loader` | 4 | `apply_pe_section_protects`, `resolve_static_guest_import`, `StaticDllMain`, `REAL_MODULE_HANDLE_BASE` |
| `vfs` | 3 | `guest_parent`, `guest_path_to_host`, `VolumeConfig` |
| `handles` | 2 | `Hmenu`, `Hwnd` |
| `seh` | 2 | `dispatch_hardware_fault`, `continue_pending` |
| `gdi32` | 1 | `enumerate` |
| `winmm` | 1 | `DueTimer`, `DueTimerKind`, `WOM_DONE`, `WaveOutCallbackKind` |
| `pthread` | 1 | `take_park` |
| `thread` | 1 | `bind_current_tid` |
| `guest_heap` | 1 | size-class constants |
| `sync_obj`, `idle` | **0** | — |

Two items are worse than a wide surface because they *are* the boundary, not
just the depth:

- **A WinAPI handler is called directly**, bypassing dispatch:
  `user32::handle_get_dc` at `session/mod.rs:930`. `user32::handle_get_sys_color`
  is imported and referenced only inside a doc comment
  (`guest_stubs/data.rs:106`), so it is not a second live call — but it shows the
  import is load-bearing enough to be named in a comment.
- `dll_loader`'s four names are PE-loading internals, i.e. `wie-runtime` is
  reaching past `wie-pe`'s API into loader mechanics.

`sync_obj` and `idle` are **not** referenced from `wie-runtime` at all, despite
being plausible candidates — recorded here so a future reader does not go
looking for them.

**Consequence.** Because everything is re-exported flat, any rename inside
`state`/`dispatch_table` is a breaking change for `wie-runtime`. The module
boundary provides an alias layer, not insulation. That is what turns ADR-0005's
capability-traits refactor into a cross-crate change, and it is the reason a
wide in-crate cleanup would land as one enormous, unreviewable commit.

## Decision

1. **Publish a `wie_winapi::port` module** re-exporting exactly what
   `wie-runtime` should use; mark the rest `#[doc(hidden)]`.
2. **Enforce it with a CI gate** — a check that forbids
   `wie_winapi::{user32,gdi32,present,winmm,kernel32,dll_loader,pthread,seh}::`
   in `wie-runtime/src` outside an explicit allow-list.
3. **Drop `wie-winapi` from `wie-cli`'s direct dependency.** `main.rs` calls
   `console::profile_sigint_armed()` / `ensure_hooks_installed()`; re-export
   both from `wie-runtime`.

Point 3 removes the one *structural* anomaly in the dependency graph: the CLI
reaching around `wie-runtime` to call WinAPI directly, bypassing
`RuntimeSession`'s lock and in-guest-callback invariants — the documented
deadlock class. Nothing currently stops a second such call.

**The gate is the part that matters.** Step 1 is a pure re-export: zero cost, no
behaviour change, but it *cannot prevent* new deep reaches — it only makes the
good path discoverable. Step 2 is what actually holds the line. Adopting 1
without 2 buys documentation, not enforcement.

## Alternatives considered

- **Do nothing.** Every internal rename stays cross-crate; ADR-0005 cannot be
  staged. The cost is invisible until the first big refactor, at which point it
  is maximal.
- **Port module without the CI gate.** Discoverability only. Cheaper, and it is
  the option most likely to be mistaken for enforcement — which is why it is
  rejected as a stopping point.
- **Make `WindowState` private instead.** Breaks 146 in-crate and 37
  `wie-runtime` sites for no DLL-boundary win, because both sides are in-crate.
  It is also orthogonal: privacy limits *who* can write a field, not *which
  module* is allowed to reach across a boundary.

## Consequences

- `wie-runtime`'s 405 references acquire an explicit allow-list, which
  immediately quantifies which are legitimate and turns the ~60 deep ones into
  visible, individually-decidable debt rather than invisible coupling.
- `wie-cli` depends on four crates instead of three, and can no longer call a
  WinAPI handler directly by construction.
- The allow-list must be maintained. That is the intended cost — it makes a new
  deep reach a deliberate, reviewable decision rather than an accident.

## Validation

The CI gate *is* the artefact, so it must be shown to fail: add a forbidden
deep reference to `wie-runtime/src`, confirm the check fails naming the file and
the path, then revert. A gate never seen failing is not evidence.

Second check: the allow-list must be small enough to review by eye. If it
approaches the size of the 405 references, the gate is not constraining anything
and the port surface needs widening.

## Reversibility

Additive in both parts. The re-export is pure; the gate is a script that can be
unwired from CI. Step 3 is a two-function move plus a dependency removal, and
reverts cleanly.