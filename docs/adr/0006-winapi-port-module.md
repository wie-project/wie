# ADR 0006 — A `port` module as `wie-winapi`'s published surface

Status: Proposed · 2026-10-01 — maintainability plan T3.5

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