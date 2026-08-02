# Idiom Plan: Rust-idiomatic types, per the Rust Book

Status: Completed (2026-08-02). Stages I and II executed with full gates; Stage III verify-only findings below. Built on four idiom audits (exp-7 winapi, exp-8 cpu, exp-9 runtime+cli, exp-10 pe+cross-cutting) that inventoried raw-int handles, stringly types, semantic bools, error handling, encapsulation, duplication, unsafe discipline, and typestate candidates with file:line evidence.

## What the audits found is ALREADY idiomatic (do not touch)

- `handles.rs` newtypes (`Hwnd`, `Hmenu`, `Hdc`, `Hfont`, `Hbrush`, `Hpen`, `Hbitmap`), `FakeVa` enum, `WinMsg(u32)` + WM_* consts, `WindowClassIdentifier`, `ControlState`, `ControlClassKind`
- CPU enums: `FastApiKind`, `BlockKind`, `BlockTerm`, `JitMemMode`, `PageState`, `RegionKind`, `CacheEntry`, `StepExecError`, `CpuError`; `GuestVa`/`HostAddr` newtypes in jit/lower/
- Unsafe discipline: all blocks carry `// SAFETY:` comments except one (see I-6); `unsafe` confined to FFI/syscall/pointer-ops in the expected crates
- Interior-mutability strategy (split `message_queue` lock, `MessageSignal` cvar pattern, atomics for cross-thread flags) is sound; no `Rc`; hot-path borrowing clean (`run_compiled` drops guards before native execution)
- Workspace lints already mechanically enforce `unwrap`/`expect`/`panic`/indexing/`as`-cast discipline

## Deliberately SKIPPED (KISS / no payoff / risk)

1. **Typestate on the JIT compile pipeline, TLB entries, CacheEntry**: transitions are already guarded by explicit match/if chains; full type-level enforcement "may not pay for itself" (audit exp-8 verdict). Skip.
2. **`JitShared::engine` Mutex → RwLock, `LARGE_FREE` lock**: compile-time-only contention; investigate later if profiling demands. Skip now.
3. **New dependencies** (`bitflags`, `derive_more`, `num_enum`): hand-rolled alternatives are functional; flag constants become associated consts on newtypes instead. `thiserror` is already in workspace deps and WILL be used (I-3).
4. **Naming standardization** (`exec_*` vs `handle_*` vs bare verbs): churn without behavior value; add a convention note to CONTRIBUTING instead. Skip renames.
5. **`JitCtx`/`RegFile` pub fields**: C-ABI/FFI-required. Skip.
6. **Test-file `.expect()` usage** (81 sites in wie-runtime tests): verify whether tests actually inherit the workspace denies (they pass clippy today) — investigate only, no test churn (tests were exempted in the restructure policy).

## Zero-risk mechanical (gate: fmt + clippy + workspace tests after each lane)

| Lane | Change | Evidence |
| --- | --- | --- |
| **I-1 handle newtypes** | Extend the `handles.rs` pattern: `ProcessState` 5× `next_*_handle`, `GdiState` 5× `next_*_handle`, `WindowState` 3× `next_*_handle`, `SyncState::next_handle` + `HashMap<u64, KernelObject>` → `KernelHandle` key, `next_buffer_handle` (console), guest TIDs → `GuestTid(u32)` (sync_obj + runtime `ProcessConfig::primary_tid` 0x5678 → `GuestTid::PRIMARY`), runtime `entry_point_va`/`initial_rsp` → `GuestVa`/`GuestStackPtr` | exp-7 #1-3,18; exp-9 #9 |
| **I-2 encapsulation** | `WinApiState`, `D3D9State`, `WindowState`, `SyncState`, `PresentState`, `KeyboardState(pub [u8;256])`, `RuntimeProfile`, `SessionInit` pub fields → `pub(crate)` + accessors (keep existing `try_*` accessors; `Serialize`-required pub in wie-pe stays) | exp-7 #5,10,11; exp-9 #5 |
| **I-3 error types** | `CpuError::Win32(u32, &'static str)` variant replacing embedded win32 strings; `CpuEngine::return_from_win64_api` `Result<(), ()>` → real error variant; `exception/unwind.rs` `MemRead` `Result<(), ()>` → `Result<u64, ReadError>`; wie-pe `PeMapError` (thiserror, already in workspace deps) replacing `Option`/`anyhow::bail` on parse failures; 6× `let _ = sync_*(...).ok()` teardown swallows → documented policy (`let _ =` with comment, they are best-effort teardown) | exp-8 #2,3; exp-10 #1; exp-7 #6 |
| **I-4 dedupe** | `patch_rel32`/`patch_rel8` triplicated (guest_io/guest_mbwc/guest_heap_accel) + `clear_bit` dup → runtime `asm_utils` module; `guest_string.rs`/`vfs/encoding.rs` ANSI-decode dedupe (share `decode_ansi_utf8_first`); `fake_va` 6 encode fns → `FakeVa::encode(self)` method; wie-pe `read_u16/u32/u64` → const-generic `read_uint_at::<2|4|8>`; repeated rect right/bottom math → small helper | exp-9 #3,7; exp-7 #7; exp-10 #1 |
| **I-5 small enums + atomics** | `AccessType { Read, Write, Fetch }` replacing `i32` consts (exec); `Rflags(u64)` associated-const newtype (regs); `SceneState { Inactive, Active }` for `d3d9_scene_active`; `ProcessLifecycle { Alive, Exiting }` for `process_dying`; `wait_for_input: bool` → `AtomicBool` (real cross-thread fix in gui_loop); 10+ `OnceLock` config accessors → one `JitConfig` struct (cpu) | exp-8 #5,6; exp-7 #8,12; exp-9 #6 |
| **I-6 hygiene** | Add `// SAFETY:` comment to dispatch_table `mem::transmute` (the only bare unsafe); remove dead `peek_fast_ucrt_call`/`peek_self_loop` and `BgWaitState::Ready` dead-variant cleanup; drop noise `#[expect(clippy::large_enum_variant)]` on 2-variant `PresentBackend`; ~22 `for i in 0..N` loops → iterators where index semantics aren't required (dll_loader, unwind, ucrt, console, d3d9 raster) | exp-7 #8,13; exp-10 #8; exp-8 #13,15 |

## Moderate risk, GUI/runtime-centric (gate: full gate + micro-suite + gui_demo sanity after each lane)

| Lane | Change | Evidence |
| --- | --- | --- |
| **II-1 WindowRecord flags** | 7 bools (visible/enabled/invalidated/erase_background/mouse_tracking/pressed/focused) → `WindowFlags` associated-const bitflags newtype (no new dep); used across user32 — careful, gate with micro-suite | exp-7 #4 |
| **II-2 cache/table types** | `MenuTreeCache` `Arc<Mutex<Option<(u64, Vec<MenuNode>)>>>` → `RwLock<Option<(Hmenu, Vec<MenuNode>)>>`; `SoftApiTable` `HashMap<String, u16>` stringy key → typed `(Library, Name)` key or `LibraryName` struct; `classify_guest_stub` 250-line string chain → static lookup table | exp-9 #11,8; exp-7 #2 |
| **II-3 cli state** | `WindowRuntime` `Option<...>` → `enum WindowState { Uncreated, Active(WindowRuntime) }`; `WIE_PRESENT` string compare → parsed `PresentBackendName` enum at startup; `is_interactive_stdin` `to_string_lossy()` → OsStr compare; `executable_file_bytes.clone()` → `Arc<Vec<u8>>` | exp-9 #10,15,14 |

## Verify-only (no code change)

- Confirm whether wie-runtime tests inherit `unwrap_used`/`expect_used` denies (they pass clippy today; if there is an allow mechanism, document it in CONTRIBUTING)
- `is_pe64: bool` redundancy with `Machine` — confirm and remove only if the audit's reading holds
- `GdiState`/`PresentState` remaining pub after I-2 — sweep for stragglers

## Verify-only findings (no code change)

1. **Test `.expect()` usage is legal, not a lint violation**: the audit flagged 81 `.expect()` sites in wie-runtime tests against the workspace `expect_used = "deny"`. Verified: `crates/wie-runtime/Cargo.toml` has no `[lints]` section, so the crate never opts into `lints.workspace = true` and the clippy denies never reach its test targets (proof: `clippy -p wie-runtime --test clock_stub` passes silently; forcing `-W clippy::expect_used` fires the lint). The lib code is still unwrap-free by convention. Recommendation: optionally opt the crate into workspace lints and convert tests to `#![expect]`-annotated modules — deferred, not worth the churn now.
2. **`is_pe64: bool` is verified redundant but kept**: it is set to `true` unconditionally at every construction site (lib.rs:186, 479) and copied through at 524 — never derived from `machine`, can never be false in this PE64-only emulator. Removing it would change the public `Serialize` struct shape (PeIdentity/PeImageSummary) for zero behavior gain; the audit's alternatives (Option<Machine>, caller checks) also churn cli. Kept as-is, documented here.

## Verification per stage

- Stage I lanes: `cargo fmt --all` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace`; **long_loop A/B vs previous commit** after I-5 (cpu-touching); micro-suite after any winapi-touching lane
- Stage II lanes: full gate + micro-suite + `gui_demo` selftest (exit codes 100-103) + interactive sanity
- Stage III: report only, no commits
- Perf invariant: long_loop ≈ 0.28–0.32 s release (A/B under identical load, per the restructure's established method)

## ADR-003: Handle and flag types

- **Status:** Accepted (with this plan)
- **Context:** Win32 emulation mixes many integer kinds (window/DC/brush/pen/font/bitmap/file/find/registry/kernel/menu/buffer handles, guest TIDs, guest VAs). Mixups are silent. `handles.rs` already proves the newtype pattern; state tables lag behind it.
- **Decision:** Extend newtypes to every handle-kind field and table key, one newtype per kind, `Deref`-free explicit `.0`/accessors, associated-const flag patterns instead of `bitflags` crate.
- **Consequences:** Easier: compiler catches handle-kind mixups; harder: transient `.0` noise at API boundaries; perf cost zero (all zero-sized wrapper structs).

## ADR-004: Error typing

- **Status:** Accepted (with this plan)
- **Context:** Errors today are `CpuError::Message(String)` with embedded win32 codes, `Result<(), ()>` placeholders, and `anyhow` strings; `thiserror` is already a workspace dependency used only by CpuError.
- **Decision:** Structured variants (`CpuError::Win32(u32, &'static str)`, `PeMapError` with field context) replace string-embedded errors at the type boundary; `Result<(), ()>` placeholders become real errors; teardown-path swallows stay swallowed but documented.
- **Consequences:** Easier: matchable errors, no per-error string allocation, richer diagnostics; harder: error enum maintenance as new failure modes appear.
