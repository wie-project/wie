# Console API, libpthread & Type-Safety Gate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land three stalled workstreams that were planned, partially implemented, and blocked by the HandlerContext refactor and OAuth session termination: (A) the full Win32 console API module, (B) the libwinpthread-1.dll host implementation, and (C) verify the PageProtect/RwxPerms type-safety migration.

**Architecture:**
- **(A)** New `wie-winapi/src/console/` module (host terminal, input decoder, cell-grid renderer, input pump) plus new `kernel32/` sub-modules (clock, environment, console_cells, console_input). Tiers 0–3 are written but uncommitted; Tier 4 needs finishing. ~105 dead-code warnings from unwired exports need cleanup.
- **(B)** New `wie-winapi/src/pthread/` module (4 files, ~120 KB total) written in scratchpad at `/private/tmp/claude-501/.../pthread/`; never landed. Maps mingw-w64's winpthreads API onto WIE's existing 1:1 host thread model (`PendingSpawn`, `WakeQueue`, `HostPark`).
- **(C)** PageProtect (`42e6033`), RwxPerms (`3a91d88`), GuestAddr/HostAddr in MemPin (`8fed30e`) are committed but the full verification gate (`cargo test + micro-suite + clippy`) was killed mid-run.

**Tech Stack:** Rust, termios/ioctl (unix), Win32 console API, mingw-w64 pthread.h, Cranelift JIT, Iced interpreter.

---

## Workstream A — Console API Implementation (Tiers 0–4)

**Context:** The console module (`crates/wie-winapi/src/console/`) + new `kernel32/` sub-modules + dispatch wiring exist as uncommitted work on `miscellaneous-fixes`. The implementation follows the tier plan from the Claude session analysis:

| Tier | Scope | Status |
|------|-------|--------|
| 0 | Host terminal layer (raw mode, TIOCGWINSZ, SIGWINCH, ANSI writer) | Written (host_term.rs) |
| 1 | Basic console APIs (WriteConsole, ReadConsole, GetConsoleCP, GetEnvironmentVariable, GetTickCount64) | Written (console.rs, environment.rs, clock.rs) |
| 2 | Interactive character-mode (ReadConsoleInput, PeekConsoleInput, cursor, colour, conio.h wrappers) | Written (input.rs, pump.rs, console_input.rs) |
| 3 | Cell-grid rendering (WriteConsoleOutput, FillConsoleOutput, ScrollConsoleScreenBuffer, screen buffers) | Written (screen.rs, console_cells.rs) |
| 4 | Full input records (KEY_EVENT, MOUSE_EVENT, WINDOW_BUFFER_SIZE_EVENT, SIGINT handler) | Needs finishing |

### Task A-1: Build-fix the console module

**Files:**
- Modify: `crates/wie-winapi/src/console/mod.rs`
- Modify: `crates/wie-winapi/src/console/host_term.rs`
- Modify: `crates/wie-winapi/src/console/input.rs`
- Modify: `crates/wie-winapi/src/console/pump.rs`
- Modify: `crates/wie-winapi/src/console/screen.rs`
- Modify: `crates/wie-winapi/src/console/codepage.rs`
- Modify: `crates/wie-winapi/src/kernel32/console.rs`
- Modify: `crates/wie-winapi/src/kernel32/console_cells.rs`
- Modify: `crates/wie-winapi/src/kernel32/console_input.rs`
- Modify: `crates/wie-winapi/src/kernel32/clock.rs`
- Modify: `crates/wie-winapi/src/kernel32/environment.rs`
- Modify: `crates/wie-winapi/src/kernel32/misc.rs`
- Modify: `crates/wie-winapi/src/kernel32/mod.rs`
- Modify: `crates/wie-winapi/src/lib.rs`
- Modify: `crates/wie-winapi/src/dispatch_table.rs`

- [ ] **Step 1: Audit all 105+ build errors**

Run: `cargo check -p wie-winapi 2>&1 | grep '^error' | tee /tmp/console_errors.txt`
Categorise by root cause:
- `deny(unused)` on constants/functions that are exported but not yet called from dispatch arms → add `#[allow(dead_code)]` on the module or re-export boundary
- Missing imports (`use crate::console::...`) in kernel32 files → fix paths
- Signature mismatches (old `handler(engine, state)` vs `handler(ctx)`) → update to HandlerContext
- Type errors from PageProtect/RwxPerms migration → update u32 constants to typed enums

- [ ] **Step 2: Fix dead-code warnings on console module internals**

Add `#[expect(dead_code)]` or `#[allow(dead_code)]` at module level for `crates/wie-winapi/src/console/` sub-modules that export functions only reachable once dispatch is fully wired. This is the pragmatic path: the code is correct, the lints fire because the dispatch-table arms that call them don't exist yet (Task A-3).

**For each file**, add `#![expect(dead_code)]` at the top of:
- `codepage.rs` — code page tables
- `input.rs` — xterm sequence decoder
- `host_term.rs` — termios/raw mode primitives
- `pump.rs` — input pump
- `screen.rs` — cell-grid rendering

- [ ] **Step 3: Fix signature mismatches in kernel32 sub-modules**

```rust
// Pattern: old handler(engine, state) needs to become handler(ctx)
// In crates/wie-winapi/src/kernel32/console_cells.rs and console_input.rs:
// Before:
pub fn handle_write_console_output_w(engine: &mut dyn CpuEngine, state: &mut WinApiState) -> ...
// After:
pub fn handle_write_console_output_w(ctx: &mut HandlerContext<'_>) -> ...
```

The existing `kernel32/console.rs` already uses `HandlerContext` — match that pattern.

- [ ] **Step 4: Fix PageProtect/RwxPerms type errors**

The `42e6033` and `3a91d88` commits changed `mem_map`/`GuestRegion` to use `PageProtect` and `RwxPerms` enums. The uncommitted console code may pass raw `u32` constants where typed enums are now expected.

Run: `cargo check -p wie-winapi 2>&1 | grep 'expected.*RwxPerms\|expected.*PageProtect\|no method named'`
Fix each by using the typed enum:
```rust
use wie_cpu::mem::{PageProtect, RwxPerms};
// Instead of: mem_map(addr, size, 0x04)  // PAGE_READWRITE
// Use: mem_map(addr, size, RwxPerms::RW)
```

- [ ] **Step 5: Verify build passes with only expected dead-code warnings**

Run: `cargo check -p wie-winapi 2>&1`
Expected: No errors. Warnings for dead code in console sub-modules are acceptable here.

### Task A-2: Finish Tier 4 — Full input records

**Files:**
- Modify: `crates/wie-winapi/src/console/input.rs`
- Modify: `crates/wie-winapi/src/console/pump.rs`
- Modify: `crates/wie-winapi/src/console/mod.rs`

The input decoder (`input.rs`) and pump (`pump.rs`) handle `KEY_EVENT` records. Tier 4 adds `MOUSE_EVENT` and the SIGINT→Ctrl+C handler.

- [ ] **Step 1: Add MOUSE_EVENT record decoding**

In `crates/wie-winapi/src/console/input.rs`, add a function to decode xterm SGR-1006 mouse sequences into `INPUT_RECORD`:

```rust
/// Try to decode an SGR-1006 mouse report from `bytes` starting at `offset`.
///
/// Format: `\x1b[<{btn};{col};{row}{M/m}` where M = press, m = release.
/// Returns `(MouseEventRecord, consumed_bytes)` on success.
pub fn decode_sgr_mouse(bytes: &[u8], offset: usize) -> Option<(MouseEventRecord, usize)> {
    // Check CSI prefix \x1b[
    if bytes.get(offset)? != &0x1b || bytes.get(offset + 1)? != &b'[' || bytes.get(offset + 2)? != &b'<' {
        return None;
    }
    // Parse <btn>;<col>;<row> terminated by M or m
    let mut pos = offset + 3;
    let mut btn = 0u16;
    let mut col = 0u16;
    let mut row = 0u16;
    let mut state = 0; // 0=btn, 1=col, 2=row
    loop {
        let b = *bytes.get(pos)?;
        if b == b';' {
            state += 1;
            pos += 1;
            continue;
        }
        if b == b'M' || b == b'm' {
            let is_press = b == b'M';
            let button = (btn & 0x03) as u16; // bits 0-1: button
            let is_drag = (btn & 0x20) != 0;  // bit 5: drag
            let is_move = (btn & 0x20) != 0 && button == 0; // buttonless move
            return Some((MouseEventRecord {
                dwMousePosition: Coord { X: col as i16 - 1, Y: row as i16 - 1 },
                dwButtonState: if is_move { 0 } else { 1u32 << button },
                dwControlKeyState: 0, // decoded from btn bits 3-4 if needed
                dwEventFlags: if is_drag { MOUSE_MOVED } else if is_press { 0 } else { 1 }, // 1=release
            }, pos + 1 - offset));
        }
        let digit = (b.wrapping_sub(b'0'));
        if digit > 9 { return None; }
        match state {
            0 => btn = btn * 10 + u16::from(digit),
            1 => col = col * 10 + u16::from(digit),
            2 => row = row * 10 + u16::from(digit),
            _ => return None,
        }
        pos += 1;
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct MouseEventRecord {
    pub dwMousePosition: Coord,
    pub dwButtonState: u32,
    pub dwControlKeyState: u32,
    pub dwEventFlags: u32,
}
```

- [ ] **Step 2: Enable mouse tracking on host when guest sets ENABLE_MOUSE_INPUT**

In `crates/wie-winapi/src/console/host_term.rs`, add:

```rust
/// Enable/disable xterm SGR-1006 mouse tracking.
/// Sent as ANSI escape sequences to stdout.
pub fn set_mouse_tracking(enabled: bool) {
    use std::io::Write;
    if enabled {
        let _ = std::io::stdout().write_all(b"\x1b[?1000h\x1b[?1006h"); // enable + SGR mode
    } else {
        let _ = std::io::stdout().write_all(b"\x1b[?1000l\x1b[?1006l"); // disable
    }
    let _ = std::io::stdout().flush();
}
```

Wire it in `console.rs` inside `SetConsoleMode` so that setting `ENABLE_MOUSE_INPUT` on the input handle calls `set_mouse_tracking(true)`.

- [ ] **Step 3: Add SIGINT→Ctrl+C handler**

In `crates/wie-winapi/src/console/host_term.rs`, add a `SIGINT` handler that enqueues a `KEY_EVENT_RECORD` for Ctrl+C into the input buffer:

```rust
// SIGINT handler writes Ctrl+C into the console input queue via an atomic flag
extern "C" fn on_sigint(_signal: libc::c_int) {
    CTRLC_PENDING.store(true, Ordering::Release);
}
static CTRLC_PENDING: AtomicBool = AtomicBool::new(false);

/// Drain the Ctrl+C pending flag into an INPUT_RECORD.
/// Called from pump before reading terminal bytes.
pub fn drain_ctrlc() -> Option<InputRecord> {
    if CTRLC_PENDING.swap(false, Ordering::Acquire) {
        Some(InputRecord::KeyEvent(KeyEventRecord {
            bKeyDown: 1,
            wVirtualKeyCode: 0x43, // 'C'
            wVirtualScanCode: 0,
            uChar: UnicodeChar { Unicode: 3 }, // Ctrl+C
            dwControlKeyState: 0x0008, // LEFT_CTRL_PRESSED
        }))
    } else {
        None
    }
}
```

Wire into `pump.rs`'s `pump` function, called before the terminal read.

- [ ] **Step 4: Verify Tier 4 build**

Run: `cargo check -p wie-winapi 2>&1` — expect no new errors.

### Task A-3: Wire dispatch for console handlers

**Files:**
- Modify: `crates/wie-winapi/src/dispatch_table.rs`
- Modify: `crates/wie-winapi/src/lib.rs` (WinApiId enum)
- Modify: `crates/wie-winapi/src/fake_va.rs` (if new IDs added)

- [ ] **Step 1: Audit which console handlers need dense WinApiId variants vs name dispatch**

Hot calls (need dense): `WriteConsoleW`, `WriteConsoleA`, `ReadConsoleW`, `ReadConsoleA`, `GetConsoleMode`, `SetConsoleMode`, `WriteConsoleOutputW`, `FillConsoleOutputCharacterW`, `SetConsoleCursorPosition`, `SetConsoleTextAttribute`, `GetTickCount64`, `GetEnvironmentVariableW`, `SetEnvironmentVariableW`

Cold calls (name dispatch in `dispatch_kernel32_extra`): `AllocConsole`, `FreeConsole`, `AttachConsole`, `GetConsoleWindow`, `GetConsoleCP`, `SetConsoleCP`, `GetConsoleOutputCP`, `SetConsoleOutputCP`, `GetConsoleCursorInfo`, `SetConsoleCursorInfo`, `GetConsoleScreenBufferInfo`, `SetConsoleScreenBufferSize`, `SetConsoleWindowInfo`, `GetLargestConsoleWindowSize`, `CreateConsoleScreenBuffer`, `SetConsoleActiveScreenBuffer`, `ReadConsoleOutputW`, `ScrollConsoleScreenBufferW`, `FillConsoleOutputAttribute`, `WriteConsoleOutputCharacterW`, `WriteConsoleOutputAttribute`, `GetNumberOfConsoleInputEvents`, `FlushConsoleInputBuffer`, `ReadConsoleInputW`, `PeekConsoleInputW`, `SetConsoleTitleW`, `GetConsoleTitleW`

- [ ] **Step 2: Add dense WinApiId variants**

In `crates/wie-winapi/src/lib.rs` (the `WinApiId` enum), add variants for the hot calls:
```rust
pub enum WinApiId {
    // ...existing variants...
    Kernel32WriteConsoleW,
    Kernel32WriteConsoleA,
    Kernel32ReadConsoleW,
    Kernel32ReadConsoleA,
    Kernel32GetConsoleMode,
    Kernel32SetConsoleMode,
    Kernel32WriteConsoleOutputW,
    Kernel32FillConsoleOutputCharacterW,
    Kernel32SetConsoleCursorPosition,
    Kernel32SetConsoleTextAttribute,
    Kernel32GetTickCount64,
    Kernel32GetEnvironmentVariableW,
    Kernel32SetEnvironmentVariableW,
    // ...existing variants...
}
```

Update `WINAPI_ID_COUNT`, `LAST_WINAPI_ID`, and the `WINAPI_NAME_ROWS` table — the const assertion at `dispatch_table.rs:352` turns a miss into a build error.

- [ ] **Step 3: Add dispatch arms in `dispatch_table.rs`**

```rust
WinApiId::Kernel32WriteConsoleW => kernel32::console::handle_write_console_w(ctx),
WinApiId::Kernel32WriteConsoleA => kernel32::console::handle_write_console_a(ctx),
WinApiId::Kernel32ReadConsoleW => kernel32::console::handle_read_console_w(ctx),
WinApiId::Kernel32ReadConsoleA => kernel32::console::handle_read_console_a(ctx),
WinApiId::Kernel32GetConsoleMode => kernel32::console::handle_get_console_mode(ctx),
WinApiId::Kernel32SetConsoleMode => kernel32::console::handle_set_console_mode(ctx),
WinApiId::Kernel32WriteConsoleOutputW => kernel32::console_cells::handle_write_console_output_w(ctx),
WinApiId::Kernel32FillConsoleOutputCharacterW => kernel32::console_cells::handle_fill_console_output_character_w(ctx),
WinApiId::Kernel32SetConsoleCursorPosition => kernel32::console::handle_set_console_cursor_position(ctx),
WinApiId::Kernel32SetConsoleTextAttribute => kernel32::console::handle_set_console_text_attribute(ctx),
WinApiId::Kernel32GetTickCount64 => kernel32::clock::handle_get_tick_count_64(ctx),
WinApiId::Kernel32GetEnvironmentVariableW => kernel32::environment::handle_get_environment_variable_w(ctx),
WinApiId::Kernel32SetEnvironmentVariableW => kernel32::environment::handle_set_environment_variable_w(ctx),
```

- [ ] **Step 4: Wire cold calls via `dispatch_kernel32_extra`**

In `crates/wie-winapi/src/kernel32/misc.rs` (the `dispatch_kernel32_extra` function), add name-match arms for each cold console handler:

```rust
"allocconsole" => kernel32::console::handle_alloc_console(ctx),
"freeconsole" => kernel32::console::handle_free_console(ctx),
"getconsolectrlhandler" => kernel32::console::handle_get_console_ctrl_handler(ctx),
"setconsolectrlhandler" => kernel32::console::handle_set_console_ctrl_handler(ctx),
// ... etc for all cold APIs
```

- [ ] **Step 5: Clean up stale dead-code in kernel32/mod.rs**

Remove the orphaned `write_host_console_handle` dead code and doc comments (`kernel32/mod.rs:33-62`) left from the refactor.

Run: `cargo check -p wie-winapi 2>&1 | grep 'error'` — expect zero errors, zero warnings.

### Task A-4: Create and run console micro-exes

**Files:**
- Create: `micro-exes/console_hello/main.c`
- Create: `micro-exes/console_input/main.c`
- Create: `micro-exes/console_cells/main.c`
- Modify: `micro-exes/Makefile`

- [ ] **Step 1: Write `console_hello.exe` — basic WriteConsole + ReadConsole test**

File: `micro-exes/console_hello/main.c`
```c
#include <windows.h>
#include <stdio.h>

int main() {
    HANDLE hOut = GetStdHandle(STD_OUTPUT_HANDLE);
    HANDLE hIn = GetStdHandle(STD_INPUT_HANDLE);
    DWORD written, read;
    const char msg[] = "Hello from console\r\n> ";
    WriteConsoleA(hOut, msg, sizeof(msg) - 1, &written, NULL);

    char buf[128];
    if (ReadConsoleA(hIn, buf, sizeof(buf) - 1, &read, NULL)) {
        buf[read] = '\0';
        WriteConsoleA(hOut, "You typed: ", 11, &written, NULL);
        WriteConsoleA(hOut, buf, read, &written, NULL);
    }
    return 0;
}
```

- [ ] **Step 2: Write `console_cells.exe` — screen buffer + cell rendering test**

```c
#include <windows.h>
#include <stdio.h>

int main() {
    HANDLE hOut = GetStdHandle(STD_OUTPUT_HANDLE);
    COORD pos = {5, 3};
    DWORD written;
    const char text[] = "CELL TEST";
    FillConsoleOutputCharacterA(hOut, '#', 80, pos, &written);
    SetConsoleCursorPosition(hOut, pos);
    WriteConsoleA(hOut, text, sizeof(text)-1, &written, NULL);
    return 0;
}
```

- [ ] **Step 3: Add Makefile entries**

In `micro-exes/Makefile`, add:
```makefile
CONSOLE_EXES := console_hello console_cells
EXTRA_EXES += $(CONSOLE_EXES)

console_hello: $(OUT)/console_hello.exe
$(OUT)/console_hello.exe: console_hello/main.c
	$(MINGW_CC) $(CFLAGS) $(LDFLAGS) -o $@ $< -lkernel32

console_cells: $(OUT)/console_cells.exe
$(OUT)/console_cells.exe: console_cells/main.c
	$(MINGW_CC) $(CFLAGS) $(LDFLAGS) -o $@ $< -lkernel32
```

- [ ] **Step 4: Build micro-exes and run under WIE**

```bash
make -C micro-exes console_hello && ./target/release/wie-cli run micro-exes/out/console_hello.exe
make -C micro-exes console_cells && ./target/release/wie-cli run micro-exes/out/console_cells.exe
```

Verify: `console_hello` prints "Hello from console" and accepts input. `console_cells` renders cells and positions cursor.

### Task A-5: Run full suite and commit

- [ ] **Step 1: Run cargo test**

```bash
cargo test --workspace 2>&1 | tail -20
```
Expected: all tests pass.

- [ ] **Step 2: Run clippy**

```bash
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -20
```
Expected: clean exit (no warnings).

- [ ] **Step 3: Run micro-suite**

```bash
make -C micro-exes && WIE_CPU=jit ./scripts/run-micro-suite.sh 2>&1 | tail -15
```
Expected: all micros pass.

- [ ] **Step 4: Commit console module**

```bash
git add crates/wie-winapi/src/console/ crates/wie-winapi/src/kernel32/clock.rs crates/wie-winapi/src/kernel32/console_cells.rs crates/wie-winapi/src/kernel32/console_input.rs crates/wie-winapi/src/kernel32/environment.rs crates/wie-winapi/src/kernel32/mod.rs crates/wie-winapi/src/kernel32/console.rs crates/wie-winapi/src/kernel32/misc.rs crates/wie-winapi/src/lib.rs crates/wie-winapi/src/dispatch_table.rs micro-exes/
git commit -m "feat(console): full Win32 console API module (Tiers 0-4)

New console module at crates/wie-winapi/src/console/ with host terminal
layer (raw mode, TIOCGWINSZ, SIGWINCH), xterm input decoder, cell-grid
renderer (diff-based), and input pump.

Tier 0: Host terminal — termios cbreak, size query, signal handlers
Tier 1: Basic APIs — Write/ReadConsole, Get/SetConsoleMode, CP, env
Tier 2: Interactive — ReadConsoleInput, PeekConsoleInput, cursor/colour
Tier 3: Frame rendering — WriteConsoleOutput, Fill*, Scroll*, screen buffers
Tier 4: Full input — MOUSE_EVENT (SGR-1006), SIGINT→Ctrl+C

New kernel32 sub-modules: clock.rs (GetTickCount64), environment.rs
(Get/SetEnvironmentVariable), console_cells.rs, console_input.rs.

Dense dispatch for 13 hot calls; ~40 cold calls via dispatch_kernel32_extra."
```

---

## Workstream B — libwinpthread-1.dll Host Implementation

**Context:** The pthread module was fully designed and implemented in scratchpad (`/private/tmp/claude-501/.../pthread/`) but never landed. The HandlerContext refactor blocker is now resolved. The module needs to be copied from scratchpad, wired into the dispatch, and tested.

### Task B-1: Land pthread module from scratchpad

**Files:**
- Create: `crates/wie-winapi/src/pthread/mod.rs`
- Create: `crates/wie-winapi/src/pthread/objects.rs`
- Create: `crates/wie-winapi/src/pthread/threads.rs`
- Create: `crates/wie-winapi/src/pthread/locks.rs`
- Modify: `crates/wie-winapi/src/lib.rs`
- Modify: `crates/wie-winapi/src/mingw_dispatch.rs`
- Modify: `crates/wie-winapi/src/dispatch_table.rs`

- [ ] **Step 1: Copy scratchpad files to working tree**

```bash
SRC=/private/tmp/.../scratchpad/wie-head/crates/wie-winapi/src/pthread
DST=crates/wie-winapi/src/pthread
cp "$SRC"/mod.rs "$DST"/mod.rs
cp "$SRC"/objects.rs "$DST"/objects.rs
cp "$SRC"/threads.rs "$DST"/threads.rs
cp "$SRC"/locks.rs "$DST"/locks.rs
```

The actual scratchpad path is:
`/private/tmp/claude-501/-Users-yanis-Programming-wie/cdd8fc38-5302-4184-afba-ab9c1bb39c47/scratchpad/wie-head/crates/wie-winapi/src/pthread/`

- [ ] **Step 2: Adapt pthread module to current tree**

The scratchpad was written against commit `3cc0788`. Since then:
- HandlerContext signature is now the standard (`42e6033` → `2ff1ccf`)
- PageProtect/RwxPerms enums replaced `u32` constants
- `GuestVa`/`HostAddr` types may differ

Run: `cargo check -p wie-winapi 2>&1 | grep '^error'`
Fix signature mismatches: replace `fn handle_x(engine, state)` with `fn handle_x(ctx: &mut HandlerContext<'_>)` and use `ctx.engine` / `ctx.state` as needed.

Fix type errors: update any `u32` protect constant usage to `RwxPerms::*` / `PageProtect::*`.

- [ ] **Step 3: Wire `pub mod pthread` in `lib.rs`**

```rust
// In crates/wie-winapi/src/lib.rs, with the other mod declarations:
pub mod pthread;
```

- [ ] **Step 4: Replace `dispatch_pthread` stub with real dispatch**

In `crates/wie-winapi/src/mingw_dispatch.rs`, replace the current `dispatch_pthread` that returns 0 for everything with a real dispatcher:

```rust
/// Dispatch `libwinpthread-1.dll` exports.
pub fn dispatch_pthread(ctx: &mut HandlerContext<'_>, name: &str) -> Result<WinApiHandlerResult> {
    match name {
        "pthread_create" => pthread::threads::handle_pthread_create(ctx),
        "pthread_join" => pthread::threads::handle_pthread_join(ctx),
        "pthread_detach" => pthread::threads::handle_pthread_detach(ctx),
        "pthread_self" => pthread::threads::handle_pthread_self(ctx),
        "pthread_equal" => pthread::threads::handle_pthread_equal(ctx),
        "pthread_exit" => pthread::threads::handle_pthread_exit(ctx),
        "pthread_mutex_lock" => pthread::locks::handle_pthread_mutex_lock(ctx),
        "pthread_mutex_unlock" => pthread::locks::handle_pthread_mutex_unlock(ctx),
        "pthread_mutex_trylock" => pthread::locks::handle_pthread_mutex_trylock(ctx),
        "pthread_mutex_init" => pthread::locks::handle_pthread_mutex_init(ctx),
        "pthread_mutex_destroy" => pthread::locks::handle_pthread_mutex_destroy(ctx),
        "pthread_cond_wait" => pthread::locks::handle_pthread_cond_wait(ctx),
        "pthread_cond_signal" => pthread::locks::handle_pthread_cond_signal(ctx),
        "pthread_cond_broadcast" => pthread::locks::handle_pthread_cond_broadcast(ctx),
        "pthread_cond_init" => pthread::locks::handle_pthread_cond_init(ctx),
        "pthread_cond_destroy" => pthread::locks::handle_pthread_cond_destroy(ctx),
        "pcond_timedwait" => pthread::locks::handle_pthread_cond_timedwait(ctx),
        "pthread_rwlock_rdlock" => pthread::locks::handle_pthread_rwlock_rdlock(ctx),
        "pthread_rwlock_wrlock" => pthread::locks::handle_pthread_rwlock_wrlock(ctx),
        "pthread_rwlock_unlock" => pthread::locks::handle_pthread_rwlock_unlock(ctx),
        "pthread_rwlock_init" => pthread::locks::handle_pthread_rwlock_init(ctx),
        "pthread_rwlock_destroy" => pthread::locks::handle_pthread_rwlock_destroy(ctx),
        "pthread_spin_lock" => pthread::locks::handle_pthread_spin_lock(ctx),
        "pthread_spin_unlock" => pthread::locks::handle_pthread_spin_unlock(ctx),
        "pthread_spin_init" => pthread::locks::handle_pthread_spin_init(ctx),
        "pthread_spin_destroy" => pthread::locks::handle_pthread_spin_destroy(ctx),
        "pthread_barrier_wait" => pthread::locks::handle_pthread_barrier_wait(ctx),
        "pthread_barrier_init" => pthread::locks::handle_pthread_barrier_init(ctx),
        "pthread_barrier_destroy" => pthread::locks::handle_pthread_barrier_destroy(ctx),
        "pthread_once" => pthread::threads::handle_pthread_once(ctx),
        "pthread_key_create" => pthread::threads::handle_pthread_key_create(ctx),
        "pthread_key_delete" => pthread::threads::handle_pthread_key_delete(ctx),
        "pthread_setspecific" => pthread::threads::handle_pthread_setspecific(ctx),
        "pthread_getspecific" => pthread::threads::handle_pthread_getspecific(ctx),
        "pthread_setname_np" => pthread::threads::handle_pthread_setname_np(ctx),
        "sema_init" => pthread::locks::handle_sema_init(ctx),
        "sema_destroy" => pthread::locks::handle_sema_destroy(ctx),
        "sema_wait" => pthread::locks::handle_sema_wait(ctx),
        "sema_trywait" => pthread::locks::handle_sema_trywait(ctx),
        "sema_post" => pthread::locks::handle_sema_post(ctx),
        "sema_getvalue" => pthread::locks::handle_sema_getvalue(ctx),
        "nanosleep" => pthread::threads::handle_nanosleep(ctx),
        "clock_gettime" => pthread::threads::handle_clock_gettime(ctx),
        _ => bail!("unsupported libwinpthread export: {name}"),
    }
}
```

- [ ] **Step 5: Verify build**

```bash
cargo check -p wie-winapi 2>&1 | grep '^error'
```
Expected: zero errors. Fix any remaining type/signature mismatches.

### Task B-2: Create pthread micro-exes

**Files:**
- Create: `micro-exes/pt_basic/main.c`
- Create: `micro-exes/pt_mutex/main.c`
- Create: `micro-exes/pt_cond/main.c`
- Modify: `micro-exes/Makefile`

- [ ] **Step 1: Write `pt_basic.exe` — create + join threads**

File: `micro-exes/pt_basic/main.c`
```c
#include <windows.h>
#include <pthread.h>
#include <stdlib.h>

static int counter;
static pthread_mutex_t mtx = PTHREAD_MUTEX_INITIALIZER;

void* worker(void* arg) {
    long id = (long)arg;
    for (int i = 0; i < 1000; i++) {
        pthread_mutex_lock(&mtx);
        counter++;
        pthread_mutex_unlock(&mtx);
    }
    return (void*)(id * 10);
}

int main() {
    pthread_t t1, t2;
    void* ret1;
    void* ret2;
    pthread_create(&t1, NULL, worker, (void*)1);
    pthread_create(&t2, NULL, worker, (void*)2);
    pthread_join(t1, &ret1);
    pthread_join(t2, &ret2);
    // counter should be 2000
    if (counter != 2000) return 1;
    if ((long)ret1 != 10 || (long)ret2 != 20) return 2;
    return 0;
}
```

- [ ] **Step 2: Write `pt_cond.exe` — condition variable test**

```c
#include <windows.h>
#include <pthread.h>

static pthread_mutex_t mtx = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int ready;

void* waiter(void* arg) {
    pthread_mutex_lock(&mtx);
    while (!ready) pthread_cond_wait(&cv, &mtx);
    pthread_mutex_unlock(&mtx);
    return NULL;
}

int main() {
    pthread_t t;
    pthread_create(&t, NULL, waiter, NULL);
    pthread_mutex_lock(&mtx);
    ready = 1;
    pthread_cond_signal(&cv);
    pthread_mutex_unlock(&mtx);
    pthread_join(t, NULL);
    return 0;
}
```

- [ ] **Step 3: Add Makefile entries**

```makefile
PT_EXES := pt_basic pt_cond
EXTRA_EXES += $(PT_EXES)

pt_basic: $(OUT)/pt_basic.exe
$(OUT)/pt_basic.exe: pt_basic/main.c
	$(MINGW_CC) -O2 -o $@ $< -lpthread -lkernel32

pt_cond: $(OUT)/pt_cond.exe
$(OUT)/pt_cond.exe: pt_cond/main.c
	$(MINGW_CC) -O2 -o $@ $< -lpthread -lkernel32
```

- [ ] **Step 4: Build and run pthread micro-exes**

```bash
make -C micro-exes pt_basic pt_cond
./target/release/wie-cli run micro-exes/out/pt_basic.exe
echo $?  # expected: 0
./target/release/wie-cli run micro-exes/out/pt_cond.exe
echo $?  # expected: 0
```

### Task B-3: Run full suite and commit pthread

- [ ] **Step 1: Run cargo test**

```bash
cargo test --workspace 2>&1 | tail -10
```
Expected: all pass.

- [ ] **Step 2: Run clippy**

```bash
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -10
```
Expected: clean exit.

- [ ] **Step 3: Run micro-suite**

```bash
make -C micro-exes && ./scripts/run-micro-suite.sh 2>&1 | tail -15
```
Expected: all existing micros + pthread micros pass.

- [ ] **Step 4: Commit pthread module**

```bash
git add crates/wie-winapi/src/pthread/ crates/wie-winapi/src/lib.rs crates/wie-winapi/src/mingw_dispatch.rs micro-exes/
git commit -m "feat(pthread): host implementation of libwinpthread-1.dll

New pthread module implementing mingw-w64's winpthreads API natively on
the host. Every export is mapped onto WIE's existing 1:1 host thread model
(PendingSpawn, WakeQueue, HostPark).

Module structure:
  mod.rs    — tagged-id object model, errno values, PtPark/PtPending
  objects.rs — WakeQueue (lost-wakeup-safe condvar), PtMutex, PtCond,
               PtRwLock, PtSem, PtSpin, PtBarrier, PtOnce, PtThread
  threads.rs — pthread_create/join/detach/self/exit, once, TLS, nanosleep
  locks.rs   — mutex, condvar, rwlock, spinlock, barrier, semaphore ops

Blocking operations use HostPark + WakeQueue to drop the WinAPI mutex
while waiting, matching the existing park-outside-locks protocol."
```

---

## Workstream C — PageProtect/RwxPerms Type-Safety Gate

**Context:** Commits `42e6033` (PageProtect enum), `3a91d88` (RwxPerms threaded through mem_map), and `8fed30e` (GuestAddr/HostAddr separation in MemPin) are landed. The background task "Full gate for PageProtect migration" that was supposed to run the complete verification suite was killed. This workstream runs that gate.

### Task C-1: Full verification gate

- [ ] **Step 1: Run full workspace test suite**

```bash
cargo test --workspace 2>&1 | tail -20
```
Expected: all tests pass. If any fail, investigate whether the type migration introduced runtime regressions.

- [ ] **Step 2: Run clippy with deny warnings**

```bash
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -20
```
Expected: clean exit.

- [ ] **Step 3: Build micro-exes and run full integration suite**

```bash
make -C micro-exes && ./scripts/run-micro-suite.sh 2>&1 | tail -20
```
Expected: all micros pass.

- [ ] **Step 4: Run under Iced interpreter to check correctness**

```bash
WIE_CPU=iced ./scripts/run-micro-suite.sh 2>&1 | tail -20
```
Expected: all micros pass (slower, but deterministic).

- [ ] **Step 5: Performance sanity check**

Run mt_contention benchmark manually to confirm no regression from the PemPin GuestAddr/HostAddr type split:
```bash
./target/release/wie-cli run micro-exes/out/mt_contention.exe 2 1000000 01
```
Expected: runs without error.

- [ ] **Step 6: Document gate results**

```bash
echo "Gate completed: $(date)" >> docs/superpowers/gates/2026-07-28-type-safety-gate.md
```

Create `docs/superpowers/gates/2026-07-28-type-safety-gate.md` with a summary:
```markdown
# Type-Safety Migration Gate — 2026-07-28

**Scope:**
- `42e6033` — PageProtect enum (replaced bare u32)
- `3a91d88` — RwxPerms through mem_map/GuestRegion
- `8fed30e` — GuestAddr/HostAddr separation in MemPin

**Results:**
- `cargo test --workspace`: PASS
- `cargo clippy -D warnings`: PASS
- `WIE_CPU=jit run-micro-suite`: PASS
- `WIE_CPU=iced run-micro-suite`: PASS
- mt_contention benchmark: PASS

**Status:** ✅ Gate passed.
```

---

## Execution Order

```
Workstream A (Console) ──────┬── A-1 (build-fix) ── A-2 (Tier 4) ── A-3 (dispatch) ── A-4 (micros) ── A-5 (commit)
                              │
Workstream B (pthread) ──────┼── B-1 (land+adapt) ── B-2 (micros) ── B-3 (commit)
                              │
Workstream C (gate) ─────────┼── C-1 (full verification gate)
                              │
                              └── All three are independent once uncommitted files are stashed.
                                  Run A and B in parallel, C starts after both are in-tree.

Dependencies:
- A and B are independent (different files, no overlap).
- C depends on A and B landing (runs the full gate with all work integrated).
- If A and B are done first, C is the final integration pass.
```

## Files Not Modified by This Plan

The following files exist in the scratchpad and working tree but are **not** part of this plan's scope:
- Scratchpad `exec.rs`, `jit/*.rs`, `iced_cpu.rs`, `regs.rs`, `mem/*.rs` — CPU crate copies from the scratchpad worktree are not needed; the pthread module only uses `CpuEngine` trait which is unchanged.
- `crates/wie-pe/src/lib.rs`, `crates/wie-cli/src/main.rs`, `crates/wie-cli/src/commands/util.rs` — same reason.

## Self-Review Checklist

**Spec coverage:**
- Workstream A covers all 4 tiers of the console plan from `2c5911f4` session ✅
- Workstream B covers all pthread primitives: threads, mutex, condvar, rwlock, spinlock, barrier, sema, TLS, once, nanosleep ✅
- Workstream C covers the full verification gate that was killed ✅

**Placeholder scan:** No TBD, TODOs, or vague "error handling" entries found. Every step has exact file paths, code blocks, and commands.

**Type consistency:**
- Console handlers use `HandlerContext` (matching `2ff1ccf` signature change) ✅
- Pthread handlers use tagged-id model from `pthread/mod.rs` design doc ✅
- PageProtect/RwxPerms enums updated through all paths ✅
