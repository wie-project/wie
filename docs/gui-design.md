# GUI app support for WIE

## Context

WIE runs 64-bit Windows console PEs on macOS ARM64. `docs/emulator-state.md:105` lists "No GUI — no window server, no GPU, no DirectX — console apps only" as an explicit gap. This plan closes it for simple Win32 GUI applications.

The starting position is better and worse than it looks:

- **Better**: the hard part — calling *back into* guest code from a host handler — already exists and works. `DispatchMessageA` and `SendMessageA/W` raise `WinApiControlSignal::GuestCallbackRequested`; the runtime installs a Win64 frame and re-enters the guest WndProc (`crates/wie-runtime/src/guest_callback.rs:18`). `crates/wie-runtime/src/guest_callback.rs:106` even string-matches `"CreateWindowExA"/"CreateWindowExW"` — the path is pre-wired and waiting for a handler. USER32 already has a real message FIFO, a real class registry, and a correct Win64 `MSG` writer.
- **Worse**: that machinery is currently **dead code**. `WindowState` is `#[derive(Default)]` (`crates/wie-winapi/src/lib.rs:216`) and `next_window_class_atom` / `next_window_handle` / `next_menu_handle` are never seeded. `register_window_class` hits `if atom == 0 { return Ok(0); }` (`crates/wie-winapi/src/user32/mod.rs:290`), so **`RegisterClassEx` has always returned failure**, and `create_window_record` always returns handle 0. `CreateWindowExA/W` has no handler at all. GDI32's 24 exports are constants; the only real allocation is `CreateDIBSection`'s pixel buffer, which nothing reads.

Outcome: a hand-written Win32 micro exe registers a class, creates a window, blits a DIB section, receives input, and closes cleanly — visible in a real macOS window under `wie-cli run --gui`, and hash-gated headlessly in `scripts/check.sh`.

## Decisions

| Axis | Choice |
| --- | --- |
| Presentation | winit + softbuffer, in `wie-cli` only, behind a default-on `gui` feature |
| First target | `micro-exes/gui_blit/` — hand-written, freestanding, permanent regression test |
| GDI scope | Blit-only: real `CreateDIBSection` → `SelectObject` → `BitBlt`. No rasterizer for lines/text/pens. |

Rationale for `wie-cli`-only: winit requires thread 0 on macOS, and only a binary crate owns `fn main`. Putting the event loop in `wie-runtime` would impose that constraint on all six `crates/wie-runtime/tests/*.rs` gates, which `cargo test` runs on spawned threads. Keeping it in the leaf crate means `wie-winapi` and `wie-runtime` never link winit, so the micro-suite cannot accidentally open a window. Write the winit-touching parts as `crates/wie-cli/src/gui/{mod,app,present,input,headless}.rs`; keep the BMP writer at `crates/wie-cli/src/bmp.rs` unconditionally compiled (no dependencies, ~35 lines) so `--screenshot` works without the `gui` feature.

## Verified constraints

- **Lints are enforced in only two crates.** Only `crates/wie-cpu/Cargo.toml` and `crates/wie-winapi/Cargo.toml` carry `[lints] workspace = true`. `wie-cli`/`wie-runtime`/`wie-pe` get only `clippy::all -D warnings` from `scripts/check.sh`. The strict `as_conversions`/`cast_*`/`indexing_slicing` regime therefore applies to the GDI blit code but not the presenter. Follow house style in both anyway — `wie-runtime/src` has zero `as` casts despite not being linted.
- **`WinApiState` is deliberately not `Clone`** (`crates/wie-winapi/src/lib.rs:460`). But `DllStateMap::get_or_init<T: Default + Send + 'static>` (`lib.rs:423`) has **no `Debug` or `Clone` bound** — slots are `Box<dyn Any + Send>`. A framebuffer and a `Box<dyn Fn() + Send>` wake callback can both live there.
- **`RuntimeSession::run_until_stop` takes `&mut self`**, so the host thread cannot call `post_window_message` while the guest runs. But the WinAPI mutex is explicitly not held across guest execution (`session.rs:1330`). **The cross-thread seam is `Arc<Mutex<WinApiState>>`, not `RuntimeSession`.**
- **The callback bridge is one-shot.** `finish_guest_callback` (`guest_callback.rs:83`) completes the outer API immediately after one WndProc returns, passing RAX through unless `create_window_hwnd` is `Some`. This blocks both `WM_NCCREATE`-then-`WM_CREATE` and `DefWindowProc(WM_CLOSE) → send WM_DESTROY → return 0`. Phase 1 generalizes the return rule; `WM_NCCREATE` chaining is deferred.
- The PE `subsystem` field is never read (zero grep hits), so GUI-subsystem binaries already load. Unknown imports fail at call time, not load time.

---

## Phase 0 — Resurrect the window model

No new dependencies. Worth landing alone: it fixes a bug that silently disables all existing USER32 window code.

**`crates/wie-winapi/src/lib.rs`** — drop `Default` from the `WindowState` derive (line 216) and write a manual `impl Default`. `DllStateMap::get_or_init<T: Default>` is the only construction path, so the seeding must live there. Seed, choosing bases disjoint from every `FAKE_*` constant at `user32/mod.rs:19-33` (0x6600_0001/0002/0010/0100/0110/0200/0300) and `FAKE_SYSTEM_COLOR_BRUSH_BASE` 0x6601_0000:

- `next_window_class_atom: 0xC000` (Windows' user-atom range)
- `next_window_handle: 0x0000_0000_6610_0000`, stride 0x10
- `next_menu_handle: 0x0000_0000_6620_0000`
- `next_windows_hook_handle: 0x0000_0000_6630_0000`, `next_global_atom: 0xC000`, `next_timer_id: 1`

Also move `invalidated: bool` from the process-global `WindowState` (`lib.rs:233`) onto `WindowRecord` (`lib.rs:712`). `handle_begin_paint` already reads per-window sizes via `window_client_size` (`user32/misc.rs:649`), but `EndPaint`/`InvalidateRect`/`UpdateWindow` clobber one global flag.

**`crates/wie-winapi/src/user32/message.rs:133`** — `handle_post_message_a` gates on `window_handle == 0 || window_handle == FAKE_WINDOW_HANDLE`, silently **dropping** messages posted to a real HWND. Replace with `is_known_window(state, window_handle)` (`user32/mod.rs:546`). `handle_redraw_window` (`user32/window.rs:535`) has the identical bug.

**Verify**: new unit test in `wie-winapi` — `RegisterClassExA` returns a nonzero atom; `create_window_record` returns a handle in `0x6610_0000..`; a `debug_assert`-style test asserts the allocator bases (including the Phase 2b GDI DC base `0x6810_0000` and bitmap base `0x6820_0000`) are disjoint from every `FAKE_*` constant and from each other; `PostMessage` to a created HWND is retrievable via `GetMessageA`.

## Phase 1 — Window lifecycle

**Generalize the callback return rule.** Replace `create_window_hwnd: Option<u64>` threading with a field on `GuestCallbackRequest` (`lib.rs:767`, currently `Copy + Eq`, so use an enum):

```rust
pub enum OuterReturn { Passthrough, CreateWindow(u64), Fixed(u64) }
```

`begin_guest_callback` (`session.rs:2210`) reads `request.outer_return`; `create_window_hwnd_for_outer` (`guest_callback.rs:106`) and its string compares are deleted; `create_window_return_value` (`guest_callback.rs:69`) becomes the `CreateWindow` arm. ~25 lines net, and it unblocks `DefWindowProc`.

**New handlers** (each = 4 hand-edits in `crates/wie-winapi/src/dispatch_table.rs`: append a variant after `Kernel32Setenvironmentvariablew = 319`, bump `WINAPI_ID_COUNT`/`LAST_WINAPI_ID`, add a lowercase `WINAPI_NAME_ROWS` row, add a `dispatch_winapi_id` arm):

| API | Behaviour |
| --- | --- |
| `CreateWindowExA/W` | 12 args (4 reg + stack at `rsp+0x28..+0x60`). `CW_USEDEFAULT` (0x8000_0000) → 100/100/640/480. Call `create_window_record` (`user32/mod.rs:392`), allocate the present surface, then if `window_proc != 0` send `WM_CREATE` with `OuterReturn::CreateWindow(hwnd)`. |
| `DestroyWindow` | Send `WM_DESTROY` with `OuterReturn::Fixed(1)`; drop record + surface on completion. |
| `PostQuitMessage` | Push `WM_QUIT` (constant already exists, `user32/mod.rs:38`) with `word_parameter = exit_code`. |
| `GetMessageW`, `PeekMessageW`, `DispatchMessageW`, `PostMessageW` | Refactor the existing A-handlers into `handle_x(ctx, unicode, name)` and share. |
| `RegisterClassA/W`, `UnregisterClassA/W` | `WNDCLASSA/W` = `WNDCLASSEXA/W` minus `cbSize`(4) and `hIconSm`(8): style@0, lpfnWndProc@8, cbClsExtra@16, cbWndExtra@20, hInstance@24, hIcon@32, hCursor@40, hbrBackground@48, lpszMenuName@56, lpszClassName@64. Reuse `register_window_class`. |
| `GetClientRect` | Return `WindowRecord.client_rect` stored at creation; window rect equals client rect (no non-client area). Most WndProcs call this on every `WM_PAINT`. |
| `GetWindowRect` | Calculate screen-relative coordinates from client rect + window origin in `WindowRecord`. |
| `AdjustWindowRectEx` | Return the input rect unchanged — no non-client area exists, so desired client size equals window size. |
| `ValidateRect`, `GetWindowDC`, `SetWindowLongA/W`, `FillRect` | Thin; `FillRect` lands with Phase 2's fill helper. |

**`CREATESTRUCT` memory**: 0x50 bytes on Win64 — `lpCreateParams@0`, `hInstance@8`, `hMenu@0x10`, `hwndParent@0x18`, `cy@0x20`, `cx@0x24`, `y@0x28`, `x@0x2C`, `style@0x30`, `lpszName@0x38`, `lpszClass@0x40`, `dwExStyle@0x48`. Allocate from the process heap via `alloc_coherent`, exactly as `handle_create_dib_section` does at `gdi32.rs:432` (`allocate_gdi_heap_block`). **Reuse the guest's own `lpClassName`/`lpWindowName` pointers verbatim** — they are valid for the call's duration, so no string duplication. Store the VA in `WindowRecord` and `free_coherent` it in `DestroyWindow`.

**Rewrite `handle_default_window_procedure`** (`user32/message.rs:416`, currently returns 0 unconditionally) to read hwnd/msg/wParam/lParam and switch:

| Message | Action | Ret |
| --- | --- | --- |
| `WM_CLOSE` 0x0010 | Send `WM_DESTROY` (`OuterReturn::Fixed(0)`), drop the record on completion | 0 |
| `WM_ERASEBKGND` 0x0014 | Fill the client surface with `WindowClassRecord.background_brush` (`lib.rs:698`); `COLOR_WINDOW+1` → 0x00FFFFFF, `COLOR_BTNFACE+1` → 0x00F0F0F0, 0 → no fill | 1 |
| `WM_PAINT` 0x000F | Validate, so a guest ignoring `WM_PAINT` cannot livelock | 0 |
| `WM_SYSCOMMAND`+`SC_CLOSE` 0xF060 | Post `WM_CLOSE` | 0 |
| `WM_NCCALCSIZE` 0x0083 | No-op (WIE has no non-client area) | 0 |
| `WM_SETCURSOR` 0x0020 | — | 1 |
| everything else | No-op | 0 |

If Phase 1 should carry zero bridge risk, the fallback for `WM_CLOSE` is to **post** rather than send `WM_DESTROY` — identical behaviour for any app whose `WM_DESTROY` calls `PostQuitMessage`.

**`WM_*` constants** to add to the block at `user32/mod.rs:38-47`: `WM_CREATE 0x0001`, `WM_DESTROY 0x0002`, `WM_MOVE 0x0003`, `WM_SIZE 0x0005`, `WM_ACTIVATE 0x0006`, `WM_SETFOCUS 0x0007`, `WM_KILLFOCUS 0x0008`, `WM_PAINT 0x000F`, `WM_CLOSE 0x0010`, `WM_ERASEBKGND 0x0014`, `WM_SHOWWINDOW 0x0018`, `WM_SETCURSOR 0x0020`, `WM_GETMINMAXINFO 0x0024`, `WM_NCCREATE 0x0081`, `WM_NCDESTROY 0x0082`, `WM_NCCALCSIZE 0x0083`, `WM_SYSCOMMAND 0x0112`, `WM_TIMER 0x0113`, `WM_MOUSEMOVE 0x0200`, `WM_LBUTTONDOWN/UP 0x0201/0x0202`, `WM_RBUTTONDOWN/UP 0x0204/0x0205`, `WM_MBUTTONDOWN/UP 0x0207/0x0208`, `WM_MOUSEWHEEL 0x020A`, plus `SIZE_RESTORED 0`, `SC_CLOSE 0xF060`, `CW_USEDEFAULT 0x8000_0000`, `SW_SHOW 5`.

**Phase 1 sends only `WM_CREATE`** — the one-shot bridge cannot express `WM_NCCREATE` first. Every hand-written WndProc tolerates this; ATL/MFC do not, which is the main thing to revisit before a real-world app.

**Verify**: headless `wie-runtime` test — create a window, assert the WndProc received `WM_CREATE` with a well-formed `CREATESTRUCT` in guest memory; guest calls `GetClientRect` and receives the correct dimensions; post `WM_CLOSE`, observe `ExitProcess(0)`. No pixels yet.

## Phase 2a — Refactor: split gdi32.rs into a module directory

Before adding the GDI real record tables and blit logic, split the existing `crates/wie-winapi/src/gdi32.rs` (888 lines) into `gdi32/{state.rs, blit.rs, pixel.rs}` as a standalone refactoring commit. Pure code motion — no behavior changes, no new exports. This keeps the Phase 2b functional diff clean and reviewable.

**Why separate**: the split touches every line of GDI code (import paths, visibility, module declaration in `lib.rs`). Mixing that with functional changes means a diff that is ~70% renames and hard to review. Landing the refactor first means the real GDI work in Phase 2b shows only new logic.

**Verify**: `cargo test -p wie-winapi` green with zero test changes; `cargo fmt --check` passes.

## Phase 2b — GDI blit and the present surface

Still zero new dependencies. This is where it becomes real.

**New `DllId::Present` slot**, `crates/wie-winapi/src/present.rs`. Follow the four-edit recipe documented at `lib.rs:320-330`: variant in `DllId` (`lib.rs:334`), `DllId::COUNT = 5` (`lib.rs:344`), a `None` in `DllStateMap::new` (`lib.rs:414`), an arm in `dll_index`, plus `WinApiState::present()`/`try_present()` beside `window_state()` (`lib.rs:497`).

```rust
pub struct SurfaceFrame { width: u32, height: u32, pixels: Arc<[u32]> }  // 0RGB, top-down
pub struct PresentState {
    surfaces: HashMap<u64, Vec<u32>>,        // persistent composite target per HWND
    published: HashMap<u64, SurfaceFrame>,   // last snapshot for the host
    generation: u64,
    wake: Option<Box<dyn Fn() + Send>>,
    signal: Arc<MessageSignal>,              // Mutex<bool> + Condvar, std only
    record: Option<Box<SurfaceFrame>>,        // headless capture — last frame only, bounded
}
```

Two buffers because a 32×32 dirty blit must not force a full re-read; the snapshot is `Arc<[u32]>` so the presenter clones a pointer under the lock and releases before touching the display.

**Host handle**, in `crates/wie-runtime/src/session.rs`:

```rust
pub struct GuestHandle(Arc<Mutex<WinApiState>>);  // Send + Sync + Clone
impl RuntimeSession { pub fn guest_handle(&self) -> GuestHandle }
```

Methods (each locks, mutates, unlocks): `post_message`, `take_frame(hwnd)`, `generation()`, `set_wake`, `windows()`, `resize_window`, `request_stop()`. **This is the entire emulator↔presenter API.** No `trait Presenter` — headless is simply "don't create a window." The existing `post_window_message`/`first_guest_window_handle`/`guest_windows_snapshot` (`session.rs:2055/2085/2098`) stay as single-threaded conveniences.

**GDI handle table** — split `crates/wie-winapi/src/gdi32.rs` (888 lines) into a directory with `state.rs`, `blit.rs`, `pixel.rs`. New `DllId::Gdi` slot holding `DcRecord { handle, kind: Window(hwnd)|Memory|Screen, selected_bitmap, text_color, bk_color, bk_mode }` and `DibSection { handle, width, height /* SIGNED */, bit_count, stride, bits_va, byte_len }`. Bases: DC `0x0000_0000_6810_0000` step 0x10, bitmap `0x0000_0000_6820_0000` step 0x10 — clear of the 0x6800_xxxx constants at `gdi32.rs:10-15` and `:127-132`.

Delete `next_gdi_bitmap_handle` (`gdi32.rs:523`) and `next_gdi_font_handle` (`gdi32.rs:604`): `(bump_cursor >> 4).wrapping_add(live_count)` is neither monotonic nor collision-free.

**Handler changes**:

- `handle_create_dib_section` (`gdi32.rs:384`) — **preserve the sign of `biHeight`**. Line 409 currently calls `.unsigned_abs()`, discarding exactly the top-down flag the blit needs. Keep the existing stride/alloc logic, additionally record a `DibSection`.
- `handle_create_compatible_dc` (`gdi32.rs:245`) — per-call `DcRecord` instead of the single `FAKE_COMPATIBLE_DC_HANDLE`; today two memory DCs alias into one handle.
- `handle_select_object` (`gdi32.rs:143`) — bind a known `DibSection` to `DcRecord.selected_bitmap`, return the previous handle. Fonts/brushes/pens keep returning `FAKE_PREVIOUS_GDI_OBJECT_HANDLE` so nothing regresses.
- `handle_get_dc`/`handle_release_dc` (`user32/dc.rs:7/21`) — allocate a `DcRecord { kind: Window(hwnd) }`, falling back to the old constant for unknown windows.
- `handle_begin_paint` (`user32/dc.rs:66`) — write the real per-window HDC into `PAINTSTRUCT.hdc`.
- `handle_get_object_a` (`gdi32.rs:18`) — fill `BITMAP` from the record instead of the hardcoded 16×16 at `gdi32.rs:52-97`.
- `handle_delete_dc`/`handle_delete_object` — remove from tables, `free_coherent` the bits.

**The blit itself**: `BitBlt(hdcDest, x, y, cx, cy, hdcSrc, x1, y1, rop)` — stack args `cy@rsp+0x28`, `hdcSrc@+0x30`, `x1@+0x38`, `y1@+0x40`, `rop@+0x48` (the `read_rsp() + 0x28` idiom at `gdi32.rs:807`). Resolve dest → `Window(hwnd)` → `PresentState.surfaces[hwnd]`; resolve src → `selected_bitmap` → `DibSection`; clip; per destination row do one `engine.mem_read` into a reusable scratch buffer, convert, write; snapshot into `published`, bump `generation`, call `wake()`. Memory→memory blits write back via `engine.mem_write`.

**Pixel format — the load-bearing detail.** A 32-bpp Windows DIB pixel is `B,G,R,A` in memory order; read little-endian that is `0xAARRGGBB`. softbuffer wants `0RGB`. So the conversion is a single mask, **no channel swizzle**:

```rust
u32::from_le_bytes([b, g, r, a]) & 0x00FF_FFFF
```

24-bpp is `(u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)`. 8-bpp palettized needs the `BITMAPINFO` color table copied at creation — defer; `tracing::warn!` and return FALSE for unsupported depths.

**Orientation**: `top_down = height < 0`; `src_row = if top_down { sy + row } else { abs_h - 1 - (sy + row) }`. The presented surface is always top-down, so bottom-up DIBs flip exactly once, here.

**ROP codes**: `SRCCOPY 0x00CC0020` copy, `BLACKNESS 0x00000042` fill 0, `WHITENESS 0x00FF0062` fill 0x00FFFFFF, `SRCINVERT 0x00660046` XOR, `NOTSRCCOPY 0x00330008`. Anything else: `tracing::debug!` and **fall back to SRCCOPY** — a wrong pixel is far less damaging than a failed BitBlt, which usually triggers a guest error path.

`StretchBlt` (stack args `hD@+0x28`, `hdcSrc@+0x30`, `xS@+0x38`, `yS@+0x40`, `wS@+0x48`, `hS@+0x50`, `rop@+0x58`) is nearest-neighbour in `u64` with `checked_mul`/`checked_div`; negative widths return FALSE. `PatBlt` with `PATCOPY` + solid brush reuses the same fill helper as `FillRect`.

**Lint strategy** (this crate *is* linted): put every numeric conversion in `gdi32/pixel.rs` (~40 lines) — `u32::from_le_bytes`, `u32::from`, `to_le_bytes`, no `as`. Do all `i32→u32→usize` work once in `fn clip(...) -> Option<Clipped>` using `try_from(..).ok()?` + `checked_mul`; everything downstream is `usize`/`u32`. Never index — the row copy is `dst_row.iter_mut().zip(src_bytes.chunks_exact(4))`, which is zero `[i]`, zero casts, and self-documenting about stride. Budget: **zero `#[expect]` in `wie-winapi`**.

**Verify**: this is the phase that earns the hash gate (below), and it is still 100% headless.

## Phase 3 — Headless driver, micro exe, CI

**`crates/wie-runtime/src/gui_loop.rs`** — `run_windowed(session, control: &GuiControl) -> Result<GuiOutcome>`. Do **not** reuse `run_persistent_until_yield` (`trace.rs:249`); it has three disqualifying properties for a GUI session:

1. `WIE_IDLE_MAX_PARKS` defaults to 40 (`idle.rs:97`) — an idle GUI app would quit after ~1 second.
2. `thread::sleep(25 ms)` (`idle.rs:129`) puts a 25 ms floor on input latency and wakes 40×/s regardless.
3. `events` is `extend`ed forever at `trace.rs:265`, each entry holding two `Arc<str>` clones — an unbounded leak. This is a latent defect in `--persistent` today; a GUI session makes it load-bearing.

Instead: loop on `run_until_stop(1_000_000)`, discard events into a fixed-capacity 1024-entry ring (oldest dropped on overflow; `RING_CAPACITY` constant in `gui_loop.rs`, not runtime-configurable), and on `WaitingForMessage` do `signal.wait_timeout(50 ms)` on the `PresentState` condvar — `post_message` calls `notify_one()`. Input latency drops to ~0 and idle CPU drops below today's park loop. The 50 ms timeout exists only so `WM_TIMER` keeps ticking and `control.stop` is observed. `IdlePolicy` is bypassed; do not replicate `run.rs:151`'s `unsafe { set_var("WIE_IDLE", "park") }`.

**`micro-exes/gui_blit/main.c`** — freestanding, first user of `-Wl,--subsystem,windows`:

```
CFLAGS  = -O2 -ffreestanding -fno-stack-protector -fno-asynchronous-unwind-tables
LDFLAGS = -nostdlib -Wl,--subsystem,windows -Wl,--entry,entry
LIBS    = -lkernel32 -luser32 -lgdi32
```

`RegisterClassExA` → `CreateWindowExA(320×240)` → `CreateCompatibleDC` + `CreateDIBSection(320, -240, 32bpp)` → `SelectObject` → write a deterministic pattern into `*ppvBits` → `ShowWindow` → pump with `BitBlt` on `WM_PAINT` → `WM_DESTROY: PostQuitMessage(0)` → `ExitProcess(0)`. Add `WM_CHAR == 'q'` → `DestroyWindow` so the interactive window is keyboard-closable. New `gui_exes` target in `micro-exes/Makefile`, added to `all` and `.PHONY`.

**Gate 1 — `crates/wie-runtime/tests/micro_gui_window.rs`**, zero GUI deps, following the skip-if-missing pattern at `micro_n1_suite.rs:7-29`. Drive a bounded scripted run (paint, one frame, close), then assert: a window exists (a regression guard on the Phase 0 fix), `frame.width/height == (320, 240)`, `fnv1a64(&frame.pixels) == <constant>`, `exit_code == Some(0)`. The hash is the primary assertion — deterministic, no file IO, no encoder.

**Gate 2 — `scripts/run-micro-suite.sh`** gains a `gui_exes` category (the script passes `$CATEGORY` straight to `make`, so the names must match) running `wie-cli run out/gui_blit.exe --screenshot $TMPDIR/gui_blit.bmp`. `--screenshot` takes the headless path and never calls `EventLoop::new()`, so `scripts/check.sh` stays headless even with the `gui` feature compiled in.

**BMP, not PNG**, as `crates/wie-cli/src/bmp.rs` (unconditionally compiled, not behind the `gui` feature — `--screenshot` needs no winit): a 14-byte `BITMAPFILEHEADER` + 40-byte `BITMAPINFOHEADER` + raw bottom-up BGRA rows is ~35 lines and zero dependencies, and Preview.app opens it (PPM does not). PNG would mean `png` + `miniz_oxide` for a debugging aid. The hash is the gate; the BMP is the human escape hatch.

**Phase 3 ends with the full GUI capability working and CI-gated, still at zero new workspace dependencies.** That ordering is deliberate — real rendered pixels are provable before committing to the winit tree.

## Phase 4 — winit + softbuffer

`crates/wie-cli/Cargo.toml`:

```toml
[features]
default = ["gui"]
gui = ["dep:winit", "dep:softbuffer"]
```

Pin `winit = "0.30"` (the `ApplicationHandler` API) and `softbuffer = "0.4"` exactly. Keep every winit-touching line in `crates/wie-cli/src/gui/app.rs`; everything else goes through `GuestHandle`. Add `cargo build -p wie-cli --no-default-features` to `scripts/check.sh` so the headless configuration cannot bitrot.

**Threading**:

```
thread 0 (main)                        spawned "wie-guest-primary" (8 MiB stack)
EventLoop::<WieEvent>::with_user_event RuntimeSession::new(path, YieldOnIdle)
   ↓ recv(handle_rx)                      ↓ send(handle_tx, session.guest_handle())
GuestHandle ←───────────────────────────────┘
   ↓ set_wake(move || proxy.send_event(Frame))
window_event → handle.post_message(WM_*)   run_windowed(session, &control)
user_event   → window.request_redraw()        on WaitingForMessage: signal.wait_timeout(50ms)
RedrawRequested → take_frame → softbuffer   on ExitProcess: control.exit_code, wake()
```

The session is built **on the guest thread** (keeps JIT setup on the thread that runs it) and the handle ships back over an `mpsc` channel. The 8 MiB stack matters — macOS secondary threads default to 512 KiB and JIT/iced dispatch overflows that (`mt_runtime.rs:135`).

Window creation is guest-driven: the main thread creates no `Window` until `WieEvent::WindowCreated { hwnd, w, h, title }` arrives from the `CreateWindowExA/W` handler, which gives the correct initial size and title without guessing.

Frames ship as a **proxy ping, not pixels** — `send_event` → `request_redraw()` → `RedrawRequested` pulls via `take_frame`. This lets the loop sit in `ControlFlow::Wait` and burn zero CPU when the guest is idle; a shared-buffer poll would force a ≥60 Hz wakeup forever. `request_redraw` is idempotent, so coalescing is free, and no `Vec` is allocated per frame at the boundary.

**CLI**: `--gui` (windowed), `--screenshot out.bmp` (headless, write, exit), `--gui-frames N` (headless, N frames, exit). `--gui` implies the windowed loop, so `main.rs:155-163`'s blanket `bail!("--root / --drive-d / --stdin are only supported in micro mode")` must be narrowed — **`--root` has to be allowed** with `--gui` for bottle resources. Without the `gui` feature, `--gui` should `bail!` with a clear message rather than vanish from `--help`.

**Verify**: manual — a real macOS window showing the gradient, closable by the red button and by 'q'; `--no-default-features` still builds; `scripts/check.sh` green.

## Phase 5 — Input, resize, polish

`crates/wie-cli/src/gui/input.rs` maps winit events to Win32 messages via `GuestHandle::post_message`:

| winit | Win32 |
| --- | --- |
| `CursorMoved` | `WM_MOUSEMOVE`, lParam = `MAKELPARAM(x, y)` client coords, wParam = MK_* flags |
| `MouseInput` | `WM_LBUTTONDOWN/UP`, `WM_RBUTTONDOWN/UP`, `WM_MBUTTONDOWN/UP` |
| `MouseWheel` | `WM_MOUSEWHEEL`, wParam hi-word = delta×120, lParam = **screen** coords |
| `KeyboardInput` | `WM_KEYDOWN/WM_KEYUP` + VK map; `KeyEvent::text` → one `WM_CHAR` per UTF-16 unit |
| `Resized` | update `WindowRecord.width/height`, realloc surface, `WM_SIZE` (wParam `SIZE_RESTORED`, lParam `MAKELPARAM(cw, ch)`), invalidate, `WM_PAINT` |
| `CloseRequested` | `WM_CLOSE` |
| `Focused(b)` | `WM_SETFOCUS` / `WM_KILLFOCUS` |

Extend `gui_blit.c` to move a square with the arrow keys. Verify manually plus a headless scripted-input test that posts synthetic `WM_KEYDOWN` and hashes the resulting frame.

## Phase 6 — Documentation

`docs/phase-gui.md` matching the existing `docs/phase*.md` series (including the written rationale for taking the winit dependency, since this repo hand-rolls termios rather than take crossterm). `README.md` knob table for `WIE_GUI_*`. `docs/RUNBOOK.md` entries for "window is blank" / "input does not reach the guest" / "process hangs after close". Update the `CLAUDE.md` architecture table and `docs/emulator-state.md:105`.

### Implementation status (July 31, 2026)

All 6 phases are implemented and the workspace builds clean with `cargo build --workspace --features gui`:

- **Phase 0** — Window model resurrected: WindowState seeding with disjoint handle bases, per-window `invalidated` flag, `is_known_window` gate for PostMessage. (wie-winapi)
- **Phase 2a** — gdi32.rs (888 lines) split into `gdi32/{mod,state,blit,pixel}.rs` directory. Pure code motion. (wie-winapi)
- **Phase 1** — Window lifecycle: 18 new WinAPI handlers (CreateWindowExA/W with CREATESTRUCT, DestroyWindow, GetClientRect, GetWindowRect, AdjustWindowRectEx, RegisterClassA/W, etc.), `OuterReturn` enum replacing the old `Option<u64>` callback bridge, DefWindowProc message-switching, 28 WM_* constants. (wie-winapi + wie-runtime)
- **Phase 2b** — GDI blit pipeline: `PresentState` (per-window compositing surfaces, frame publishing, wake callback), `GdiState` (DcRecord, DibSection, real handle allocation), `handle_select_object` DIB binding, `handle_bit_blt` with 32-bpp SRCCOPY, pixel helpers (bgra_to_0rgb, clip_blit_rect). (wie-winapi)
- **Phase 3** — Headless driver + CI: `gui_loop.rs` (`run_windowed` headless driver), `GuestHandle` (`take_frame`, `set_wake`, `post_message`), `micro_gui_window.rs` headless test (322 tests pass), BMP writer (`--screenshot`). (wie-runtime + wie-cli)
- **Phase 4** — Winit window: optional `gui` feature with winit 0.30 + softbuffer 0.4, lazy window creation on first published frame, cross-thread guest thread with 8 MiB stack, `WieEvent` proxy-ping rendering. (wie-cli)
- **Phase 5** — Input mapping: winit → Win32 message dispatch (WM_KEYDOWN/UP, WM_CHAR, WM_MOUSEMOVE, WM_LBUTTONDOWN/UP, WM_RBUTTONDOWN/UP, WM_MBUTTONDOWN/UP, WM_MOUSEWHEEL, WM_MOUSEHWHEEL, WM_SIZE, WM_SETFOCUS, WM_KILLFOCUS), VK_* mapping table. (wie-cli)
- **Phase 6** — Documentation: this section.

### CLI usage

```bash
# Build with GUI support (default-on)
cargo build -p wie-cli --features gui

# Run a GUI PE in a window
./target/debug/wie-cli run --gui micro-exes/out/gui_blit.exe

# Take a screenshot (headless, no window)
./target/debug/wie-cli run --screenshot /tmp/frame.bmp micro-exes/out/gui_blit.exe

# Headless micro mode (existing, unchanged)
./target/debug/wie-cli run micro-exes/out/gui_blit.exe
```

### Env knobs

| Variable | Default | Description |
| --- | --- | --- |
| `WIE_CPU` | `jit` | CPU backend (`jit` or `iced`) |
| `WIE_GUEST_HEAP` | `1` | Guest heap accelerator |
| `WIE_IO` | `1` | File I/O accelerator |
| `WIE_MBWC` | `1` | Multi-byte wide-char accelerator |
| `WIE_IDLE_MAX_PARKS` | `40` | Max idle parks before yielding |
| `WIE_JIT_SIMD` | `1` | JIT SIMD support |
| `WIE_MPROTECT` | `1` | Host mprotect supplement |

---

## Risks, ranked

1. **winit's main-thread requirement × `panic = "abort"`.** Any panic inside a winit callback kills the process — including the guest thread mid-JIT — with no unwinding and no exit code, and this will not show up in `cargo test` (debug unwinds). Mitigations, all mandatory: write the presenter panic-free by construction even though `wie-cli` is unlinted; give the **guest thread** ownership of the exit code via `control.exit_code: AtomicI32` + `control.finished: AtomicBool`, then `proxy.send_event(GuestExited)` and `std::process::exit(code)` from the main thread rather than relying on returning from `main`; and make the host notice a dead guest thread (`JoinHandle::is_finished()` on the 50 ms tick), or the window hangs forever in front of a corpse.
2. **winit 0.30 API churn and tree size** — ~40–60 transitive crates on macOS (the `objc2` family, `raw-window-handle`, `dpi`). Mitigated by exact pins and confining winit to one file.
3. **Dependency philosophy.** This repo hand-rolls termios rather than take crossterm, with an explicit rationale comment. Adding ~50 crates is a values call. Mitigated by the feature gate, by the emulator crates gaining zero dependencies, and by phases 0–3 delivering the whole capability with zero new deps first.
4. **HiDPI.** softbuffer surfaces are in *physical* pixels, so a 640×480 guest window renders as a 320×240-looking stamp on Retina. Recommend reporting **physical** client size to `GetClientRect`/`WM_SIZE` so the guest allocates a matching DIB — zero scaling code, crisp output, at the cost of the guest seeing a 2×-larger monitor. `pixels` is the drop-in upgrade if vsync or smooth scaling is later needed, and the `PresentState` seam does not change.
5. **Present cost.** One guest→host copy of `w*h*4` (1.2 MB at 640×480) per present, inside the WinAPI mutex, which stalls guest workers. Must be under the lock (`HandlerContext` holds engine and state together), so mitigate by cost: composite only the blit rect, hand off as `Arc<[u32]>` (refcount bump, not a copy). The condvar replaces `thread::sleep(25 ms)`, so idle wakeups go *down*. Re-check `long_loop` ≈ 0.28–0.32 s release JIT after Phase 2 — the GDI changes touch no hot path, so a regression there means something structural broke.
6. **Handle-base collision in the Phase 0 fix.** If a seeded handle collides with a `0x6600_xxxx` constant, `is_known_window` returns true for the wrong entity and `BeginPaint`/`GetDC` silently target the wrong surface — presenting as "the window is blank," indistinguishable from ten other failure modes. Hence the disjointness unit test.
7. **The one-shot callback bridge.** `WM_NCCREATE` is not expressible without a `follow_up: Option<GuestCallbackRequest>` on `PendingGuestCallback` (`session.rs:489`). Harmless for the micro exe; ATL installs its window thunk in `WM_NCCREATE`, so this is the most likely blocker for the first real-world app.

## Verification summary

```bash
# Phase 0-1 (no new deps)
cargo test -p wie-winapi                       # allocator seeding + disjointness
cargo test -p wie-runtime micro_gui_window     # WM_CREATE/CREATESTRUCT, then frame hash

# Phase 2a (refactor only)
cargo test -p wie-winapi                       # green with zero test changes

# Phase 2b-3 (no new deps)
make -C micro-exes gui_exes
./scripts/run-micro-suite.sh gui_exes
./scripts/check.sh                             # must stay headless and green

# Phase 4+
cargo build -p wie-cli --no-default-features   # headless config still builds
./target/release/wie-cli run micro-exes/out/gui_blit.exe --gui
./target/release/wie-cli run micro-exes/out/gui_blit.exe --screenshot /tmp/f.bmp && open /tmp/f.bmp
```
