# Plan: Type-System-Driven GUI Architecture Rework

Status: proposed — awaiting user decisions on scope (Aug 1, 2026).
Author: ora-3 (architecture review); filed by orchestrator.

## 0. Ground rules

- **Guest-visible ABI is frozen.** Handle *values* (HWND/HMENU/HDC/…), `WinApiId` discriminants (packed into fake VAs, `fake_va.rs:84-86`), COM vtable slot numbers (guests call through real vtable offsets), and the `MSG` struct layout are all guest-visible. All typing below happens on the host side of those boundaries; conversions (`From<u64>` in, `as_u64()` out) live at the handler edges.
- **Hash gates are the tripwire:** `GUI_BLIT_RESTING_FRAME_HASH 0x9FB37202941F08DA` and `D3D9_RESTING_FRAME_HASH 0x3C984C1B79922439` must stay byte-identical at every step; run the micro-suite per step, not just at the end. (Note: D3D9 hash has since been recomputed to `0x28C1AE135D5CD9D0` by P4c — the gate-tripwire principle is unchanged.)
- **Lints shape the design:** no `unsafe`, no `unwrap`, no `as_conversions`. Typed newtypes are the *idiomatic* conversion vehicle here — the `u64::from`/`try_from` ceremony is already pervasive, so `Hwnd::from(x)` fits the house style for free. `#![allow(clippy::type_complexity)]` at `lib.rs:3` is an admit-the-problem flag: the tuple tables it permits are exactly the weak spots below.
- **P4c landed after this plan:** the D3D9 area (P7) can proceed on the committed base.

## 1. Pain-point inventory

| # | Pain | Where | Why it fights the type system |
|---|---|---|---|
| 1 | **u64 handle soup** | `WindowRecord.handle/parent_handle` (`lib.rs:897,921`), `focus/active/foreground/capture` (`lib.rs:308-311`), `control_states: HashMap<u64,_>` (`lib.rs:337`), present `surfaces/published: HashMap<u64,_>` (`present.rs:82-84`), `d3d9_present_hwnd` (`lib.rs:205`), `TimerRecord.window_handle` (`lib.rs:1079`), `QueuedWindowMessage.window_handle` (`lib.rs:1025`), `GuestCallbackRequest.window_handle` (`lib.rs:1003`) | HWND/HMENU/HDC/HFONT/HBRUSH/HPEN/HBITMAP are all `u64`. Namespaces are kept apart only by disjoint base ranges (`state.rs:1150-1162`) — a convention the compiler can't check. A HWND passed where a HBRUSH belongs compiles. |
| 2 | **GDI handle classification** | `SelectObject` probes `find_dib → find_brush → find_pen → find_font` linearly (`state.rs:166-179`); `DcRecord.selected_*: Option<u64>` (`state.rs:1182-1189`) | The "which kind is this handle" question is answered by a 4-probe search instead of being part of the type. |
| 3 | **Menu: flat records + rebuild-per-frame** | `menu_items: Vec<MenuItemRecord>` (`lib.rs:328,1099-1112`) + `menu_item_states`/`menu_item_check_states` as `Vec<(u64,u32,u32)>` (`lib.rs:326-327`); tree rebuilt from scratch on every host Frame: `window_menu_items()` locks the big mutex and calls `build_menu_tree` (`session.rs:2849-2864,2992-3009`); `sync_menu_bar` compares `Vec<MenuNode>` per Frame (`app.rs:93-103`) | The tree exists only at the bridge. Three parallel tables (items / states / check-states) must be kept consistent by hand, then re-derived. |
| 4 | **WM_* raw u32 + untyped params** | Const block `user32/mod.rs:46-164`; `QueuedWindowMessage`/`GuestCallbackRequest` (`lib.rs:997-1044`); repeated bit-twiddling: BN_CLICKED packing (`dialog.rs:618`), id/notify extraction (`controls.rs:146`), `MAKELPARAM` etc. (`app.rs`) | `wParam/lParam` are bitfield soup; every handler re-decodes by hand. No exhaustiveness when a new message is added to the dispatch. |
| 5 | **Duplicate WM_ consts** | `wie-cli/src/gui/input.rs:10-45` redefines WM_KEYDOWN/WM_CHAR/… that already exist in `user32/mod.rs` | Two sources of truth for the same constants; they can drift. |
| 6 | **Dispatch table: three hand-built parallel tables** | `WinApiId` is already a typed dense enum (`dispatch_table.rs:14-410`, `#[repr(u16)]`, transmute + const assert at `:438-445`) — good. But: 395-arm `match` (`dispatch_winapi_id:2113`), string lookups (`resolve_winapi_id:1880`, `winapi_id_export:1893`), `WINAPI_TRAITS: [WinApiTraits; 395]` (`:2705`) are maintained by hand, and the header claims auto-generation by a script that no longer exists (`:1-3`) | Adding an API = editing 3+ places; id↔name↔handler↔traits can silently disagree. |
| 7 | **COM dispatch by raw slot** | `(iface: u8, method: u8)` decoded from fake VAs (`fake_va.rs:52-58,103,154-158`); method lists as `&[&str]` (`d3d9.rs:35-53`); per-iface method-count consts (`d3d9.rs:106-117`); state as `Vec<(u32,u32)>` render states, `Vec<(u32,u32,u32)>` stage/sampler states (`lib.rs:186-188`); textures keyed by raw guest VA (`lib.rs:228-232`) | Slot numbers are ABI (real vtable positions) but the dispatch reads `(u8, u8)`; a wrong slot compiles. |
| 8 | **ControlUiState: struct-of-all-options** | `controls.rs:102-120` — `pressed, focused, default_push, items, caret, sel_start, sel_end, sel_index` for every kind | Reading `caret` on a Button compiles. `ControlClassKind` itself is already a clean enum (`controls.rs:52-64`) — the state behind it isn't. |
| 9 | **Host `WieApp`: 7+ Options** | `app.rs:115-124` — `handle, window, surface, presenter, hwnd, pending_size, last_resize, last_sent_size`; two present backends as two independent `Option` fields (`:118,124`) | "At most one backend" and "window exists iff hwnd known" are unchecked invariants; every event arm re-checks by hand. |
| 10 | **Font-engine borrow dance** | `std::mem::take(&mut state.gdi_state().font_engine)` (`state.rs:276,293`) | Symptom of `GdiState` method structure, not a type gap — minor, fixable with internal methods. |

Good news to bank: `ControlClassKind`, `WinApiId`, `DllId`/`DllStateMap` (`lib.rs:475-591`), `FakeVa` (`fake_va.rs:62-73`), `FontKey`/`ResolvedFont`/`FontEngine` (`font_system.rs:54,272,314`), `D3D9State`'s renderer structs (`d3d9_render.rs`: `FvfLayout:182`, `GuestVertex:250`, `Viewport:384`, `TextureStage:454`, `FragmentState:486`) are already typed correctly. The plan extends that pattern to the places that aren't.

## 2. Proposals (ranked by leverage)

### P1 — Menu: native tree replaces three tables  *(highest leverage per unit of effort)*

```rust
// wie-winapi/src/user32/menu.rs
pub struct MenuRecord {
    pub handle: Hmenu,          // P2 newtype; value unchanged
    pub items: Vec<MenuEntry>,
}
pub enum MenuEntry {
    Item { id: u32, text: String, enabled: bool, checked: bool },
    Popup { text: String, submenu: Hmenu },   // MF_POPUP → structural, not a flag
    Separator,                                // MF_SEPARATOR → structural
}
```

- `WindowState`: `menu_items` + `menu_item_states` + `menu_item_check_states` → `menus: Vec<MenuRecord>` plus `menu_dirty: bool`.
- **Fixes:** #3 entirely. `AppendMenu` = O(1) push; `GetMenuState`/`GetMenuItemInfo` index the tree; `MF_BYPOSITION` maps to `Vec` index (already the semantics); `session.rs::MenuNode` becomes a cheap walk of the same shape or dies; host `sync_menu_bar` reads the tree **only when `menu_dirty`** — kills the per-Frame big-mutex lock + rebuild + `Vec<MenuNode>` compare (`app.rs:93-103`, `session.rs:2849`).
- **Fixes also:** the two `Vec<(u64,u32,u32)>` tuple tables (#2) die with it.
- **ABI:** handle values and `MENUITEMINFO` fills unchanged. Flag-mapping (MF_* ⇄ `MenuEntry`) is a thin conversion at the API boundary.
- **Cost/risk:** ~530-line `menu.rs` rewrite + `session.rs`/`app.rs` touch. Medium diff, low behavior risk. **Do this one first** — it's self-contained and removes the worst per-frame cost.
- **Trap to respect:** `WindowRecord.menu_handle` is *dual-use* — a menu handle **or a child control ID** (`lib.rs:923-924`). The `Hmenu` newtype must NOT be applied to that field.

### P2 — Typed handle newtypes  *(highest safety, widest diff)*

```rust
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Hwnd(u64);
impl Hwnd {
    pub const NULL: Self = Self(0);
    pub const fn as_u64(self) -> u64 { self.0 }      // escape point: return values, registers, MSG
}
impl From<u64> for Hwnd { fn from(v: u64) -> Self { Self(v) } }
// Hmenu, Hdc, Hfont, Hbrush, Hpen, Hbitmap — same shape
```

- **Apply in two waves, in this order:**
  - **P2a — stores and map keys (mechanical, compiler-driven):** `control_states: HashMap<Hwnd, ControlUiState>`, `PresentState.surfaces/published: HashMap<Hwnd,_>`, `WindowState.focus/active/foreground/capture_window_handle`, `WindowRecord.parent_handle`, `TimerRecord.window_handle`, `d3d9_present_hwnd`, `DcKind::Window(Hwnd)`, `GdiState.find_*/remove_*` signatures, `QueuedWindowMessage.window_handle`. Every `windows.iter().find(|w| w.handle == hwnd)` becomes a typed lookup; the compiler finds every site.
  - **P2b — handler boundaries (the big diff):** `let hwnd = Hwnd::from(engine.read_rcx()?)` at entry, `return_from_win64_api(hwnd.as_u64())` at exit. Do it per-DLL (user32 first, gdi32 second) so each commit is reviewable.
- **Fixes:** #1, and #2 via a typed classifier:

```rust
pub enum GdiObject { Dib(Hbitmap), Brush(Hbrush), Pen(Hpen), Font(Hfont) }
impl GdiObject {
    /// Decode from the disjoint base ranges (state.rs:1150-1162); debug-assert on collision.
    pub fn classify(handle: u64, state: &GdiState) -> Option<Self>;
}
```

`SelectObject`'s 4-probe search (`state.rs:166-179`) becomes one match.
- **ABI:** zero — values identical; conversion happens exactly where u64 meets the guest (registers, return values, `MSG`, wParam/lParam).
- **Cost/risk:** the largest diff in the plan, but ~all of it is compiler-directed (which is the point — misuses become compile errors). Low behavior risk; merge-conflict churn is the real cost.
- **`HandleKind` enum alternative rejected:** a separate kind tag adds state without adding safety — the disjoint base ranges already provide the namespace; newtypes give the same protection at zero runtime cost. Keep `HandleKind::from_raw` as a *function* only, for debug asserts.
- **YAGNI guard:** do NOT newtype kernel handles (file/registry/find/resource — each is single-map, no cross-use, hot path). Do NOT newtype D3D9 COM-object keys — those are *guest addresses*, not handles; a `ComObject(u64)` marker is optional clarity only.

### P3 — `WinMsg` newtype + typed payload decoders  *(small, cheap, do early)*

```rust
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WinMsg(u32);
impl WinMsg {
    pub const WM_PAINT: Self = Self(0x000F);
    pub const WM_COMMAND: Self = Self(0x0111);
    // …all handled WM_* — moved verbatim from user32/mod.rs:46-164
    pub const fn as_u32(self) -> u32 { self.0 }
}

pub struct CommandPayload { pub id: u16, pub notify: u16, pub control: Hwnd }
impl CommandPayload { pub fn decode(wparam: u64, lparam: u64) -> Self { … } }
pub struct KeyDownPayload { pub vk: u16, pub repeat: u16, pub extended: bool, pub alt_down: bool }
pub struct SizePayload { pub width: u16, pub height: u16 }
```

- **Storage stays `u32`/`u64`** in `QueuedWindowMessage` and `MSG` — guests post arbitrary `WM_USER+` values, so a closed `enum MessageKind` over the queue would be wrong (a `Raw(u32)` fallback variant would appear in every match). The newtype + consts + payload decoders give exhaustiveness *at the dispatch sites that matter* (controls, dialog, message synthesis) without lying about the boundary.
- **Also fixes #5:** move the consts to a `pub mod wm` in `wie-winapi` and have `wie-cli/src/gui/input.rs` re-export instead of redefining (`input.rs:10-45` dies).
- **ABI:** none. **Cost/risk:** small, zero risk.

### P4 — Per-kind control state  *(medium, contained)*

```rust
pub enum ControlState {
    Button  { pressed: bool, default_push: bool },
    Edit    { caret: usize, sel_start: usize, sel_end: usize },
    ListBox { items: Vec<String>, sel_index: i32 },
    ComboBox { items: Vec<String> },
    Static,
}
impl ControlClassKind {
    fn new_state(self) -> ControlState;
    fn dispatch(self, engine, state, hwnd, msg: WinMsg, wparam, lparam) -> Result<Option<u64>>;
}
```

- `WindowRecord.control_text` **stays** — it serves the generic WM_GETTEXT/WM_SETTEXT protocol shared by every window; moving it into `ControlState` would duplicate state. Only the control-specific bits move.
- `dispatch_control_proc` (`controls.rs:112-211`) becomes `kind.dispatch(…)`; the `(kind, msg)` arms get typed state access — reading `caret` on a Button becomes a compile error.
- **ABI:** none. **Cost/risk:** `controls.rs` is 1407 lines; moderate diff, compiler-guided. Paint output must stay byte-identical → run `gui_control`/`gui_dialog` + the blit hash right after.

### P5 — Host app state: grouped invariants  *(small)*

```rust
enum PresentBackend { Wgpu(WgpuPresenter), Softbuffer(softbuffer::Surface<Arc<Window>, Arc<Window>>) }
struct WindowRuntime {
    hwnd: Hwnd,
    window: Arc<Window>,
    surface: Option<PresentBackend>,     // at-most-one is now a type
    pending_size: Option<(u32, u32)>,
    last_resize: Option<Instant>,
    last_sent_size: Option<(u32, u32)>,
}
struct WieApp { handle: Option<GuestHandle>, runtime: Option<WindowRuntime>, /* menu mirror, modifiers… */ }
```

- Collapses 7 `Option` fields (`app.rs:115-124`) into 2; "window exists iff `runtime.is_some()`" becomes an invariant instead of a per-arm check. The full `enum AppState { AwaitingGuest, Running(WindowRuntime) }` is possible but forces destructuring in every event arm for no additional safety — recommend the grouped-Option middle ground.
- **ABI:** none. **Cost:** `app.rs` restructure. **Risk:** low.

### P6 — Dispatch table: one source of truth  *(honest verdict: park it)*

A `winapi_dispatch!` list generating the 395-arm match, `resolve_winapi_id`, `winapi_id_export`, and `WINAPI_TRAITS` from one table. **Constraints:** `WinApiId` discriminants are guest-visible (packed into fake VAs), so the macro must be append-only — no renumbering, ever.

This is a readability/maintenance win with **zero** correctness value (the enum + const assertion at `dispatch_table.rs:438-445` already guarantees id↔index consistency). It's a ~1-2k-line mechanical diff against a file another lane touches. **Do last or never** — not worth blocking the type work.

### P7 — D3D9 typed COM dispatch  *(deferred until P4c lands; P4c is now committed)*

```rust
// iface byte is ABI (vtable identity): explicit discriminants
enum D3d9Iface { Direct3D9 = 0, Device9 = 1, Texture9 = 2, Surface9 = 3 }
// method slots are ABI (real vtable positions): explicit discriminants
enum Device9Method { QueryInterface = 0, AddRef = 1, Release = 2, …, Present = 17, … }
enum RenderState { AlphaBlendEnable(bool), SrcBlend(D3DBLEND), ZEnable(bool), … }  // D3DRS_* → variant
```

- Decode `(iface, method)` at the fake-VA boundary (`fake_va.rs:154-158`) into typed enums; replace the `&[&str]` method-name arrays (`d3d9.rs:35-53`) and the `Vec<(u32,u32)>`/`Vec<(u32,u32,u32)>` state blobs (`lib.rs:186-188`).
- **Direction only for now** — P4c is committed; sequence after it. The `D3D9State.d3d9_*` field-name prefixing is a symptom of a flat struct and can be grouped into `struct RenderState`/`struct TextureState` sub-structs at the same time.

## 3. Migration strategy (each step keeps both hash gates green)

| Step | Work | Gate | Risk |
|---|---|---|---|
| 0 | P3: `wm` module + re-export into `gui/input.rs` (kills const duplication) | compile | none |
| 1 | P3 payload decoders; migrate dispatch sites incrementally | micro-suite | none |
| 2 | P1 menu tree + `menu_dirty` flag (`menu.rs`, `session.rs`, `app.rs`) | `gui_menu` micro | low (menus never render into the guest surface — hash-neutral by construction) |
| 3 | P2a typed stores/map keys (control_states, present surfaces, WindowState handle fields, `DcKind::Window`, GdiState) | **GUI_BLIT_RESTING_FRAME_HASH** | low (compiler-driven) |
| 4 | P2b handler boundary conversions, per-DLL (user32 → gdi32) | blit hash + micro-suite | medium (diff size) |
| 5 | P4 control-state enum + `ControlClassKind::dispatch` | blit hash + `gui_control`/`gui_dialog` | low-medium (paint-adjacent) |
| 6 | P5 host app state grouping | interactive run + screenshot | low |
| 7 | P7 D3D9 typed dispatch (after P4c); P6 macro (optional, last) | **D3D9_RESTING_FRAME_HASH** | medium |

Ordering logic: pure-host refactors first (P1/P3), guest-visible-adjacent last; handle *values* and id layouts never change, so the gates trip only on accidental logic changes — which is exactly what the per-step micro-suite runs are for.

## 4. What NOT to do (YAGNI)

- **Kernel-handle newtypes** (file/registry/find/resource): single-map, no cross-namespace confusion, hot path. Leave `u64`.
- **Closed `MessageKind` over the queue**: guests post arbitrary u32; `Raw(…)` would appear everywhere. The `WinMsg` newtype + consts + decoders capture the value with less noise.
- **Slotmap/arena for `Vec<WindowRecord>`**: window counts are tiny; linear `find` is fine. Revisit only under profile evidence.
- **Typed storage for wParam/lParam**: bitfield semantics are per-message; typed *decoders* yes, typed *storage* no.
- **Rewriting `DllStateMap`**: it's already the model (`DllId` + typed slots + const assert). Emulate it, don't replace it.
- **Full `AppState` enum** over the grouped-Option middle ground in P5.

## 5. Open questions

1. **Handle-newtype scope:** host-side only (typed at stores/maps; `u64` remains in handler signatures, `GuestCallbackRequest`, and `QueuedWindowMessage`) — recommendation, ~1× diff — or full flow through handler entry/exit, ~2-3× diff for marginal extra safety?
2. **Menu ownership:** full tree replacement (three tables → one tree + dirty flag, P1 as written) or keep flat records and only cache the built `MenuNode` behind a dirty flag (smaller diff, keeps the tuple tables)?
3. **Dispatch macro (P6):** fold into this work, park until the id count stabilizes, or skip? And confirm the invariant: `WinApiId` discriminants are **append-only** (they're guest-visible through the fake-VA encoding) — assumed yes.
