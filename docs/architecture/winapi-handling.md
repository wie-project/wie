# WinAPI handling (`wie-winapi`)

`wie-winapi` is the Windows API surface: KERNEL32 / USER32 / GDI32 / D3D9 / comdlg32 / UCRT and friends, dispatched through a dense ID table and executed against host state. It also owns the guest heap, the filesystem bottle, sync objects, SEH/MSVC C++ EH, and the presentation state that the GUI paints into.

## The fake-VA window

Every IAT slot in the guest image is rewritten to a VA inside `FAKE_API_BASE = 0x0000_7000_0000_0000` (a 4 MiB window, 16-byte stride). The runtime keeps a stop bitmap over the window; hitting a set bit stops the CPU and returns control to the session, which decodes the VA:

```mermaid
flowchart LR
    A["guest: call [IAT slot]"] --> B["fake VA hit<br/>(stop bit set)"]
    B --> C["fake_va::decode — O(1)<br/>kind | payload"]
    C --> D{"kind"}
    D -->|Export(id)| E["dispatch_winapi_id<br/>(dense u16 match)"]
    D -->|Alias(id)| E
    D -->|Unresolved(idx)| F["soft table → UnsupportedApi"]
    D -->|Com{iface,method}| G["D3D9 vtable dispatch"]
    D -->|Special(u)| H["runtime trampolines<br/>(callback return / SEH continue)"]
    E --> I["handler → WinApiHandlerResult"]
    I --> J["return_from_win64_api<br/>RAX = value, RIP = popped return"]
```

The five `FakeVa` kinds: `Export` (primary handler), `Alias` (host fallback to the same `WinApiId`), `Unresolved` (soft slot index), `Com` (D3D9 interface method), `Special` (runtime trampolines). There is no string compare on the hot path — the guest's own call instruction reaches the handler directly.

## One API call, end to end

```mermaid
sequenceDiagram
    participant G as guest code
    participant C as CpuEngine
    participant R as runtime session
    participant D as dispatch_winapi_id
    participant H as handler (e.g. kernel32)
    participant S as WinApiState

    G->>C: call [IAT] (fake VA)
    C-->>R: stop (hit bitmap)
    R->>D: decode → WinApiId
    D->>H: HandlerContext { engine, environment, state }
    H->>S: read/write state, guest memory
    H->>S: guest_struct views (zerocopy, layout-asserted)
    H-->>R: WinApiHandlerResult { return_address, return_value }
    R->>C: RAX = value; RIP = pop(guest stack)
    C-->>G: resume at caller's next instruction
```

Handler conventions:

- **`HandlerContext`** bundles the CPU engine, the `WinApiEnvironment` (image base, command line VA, heap handle), and `&mut WinApiState`.
- **Handlers never write registers.** They return `WinApiHandlerResult { return_address, return_value }`; the runtime writes RAX and jumps. `return_from_win64_api` pops 8 bytes off the guest stack as the return address — so a handler must not push anything on the stack itself.
- **Guest structs** are read through typed zero-copy views (`zerocopy::KnownLayout`), with layouts cross-compile-time-verified against mingw constants. A wrong offset is a guest crash or worse — `WIN32_FIND_DATA` once sent 7-Zip into infinite recursion.

## The dispatch table

`WinApiId` is a dense `#[repr(u16)]` enum — 507 variants, discriminant = dispatch index. The file is **maintained by hand** (the old generator script is gone): adding an API means a variant, a dispatch arm, the handler in the right DLL module, and registration in the fake-VA registry. Two discriminant holes (109/110) mean `WINAPI_ID_COUNT` is derived from the last variant, not `EnumCount`.

`GetProcAddress`-resolvable APIs live in `dynamic_apis.rs`: `PREPLANTED_SOFT_APIS` are stable-index soft slots (order is ABI — append only). The `Interlocked*` family has **no handler at all** — it resolves entirely through soft slots.

## Strings: the A/W split

| Path | Read (guest → host) | Write (host → guest) |
| --- | --- | --- |
| **A (ANSI)** | UTF-8 first (mingw guests emit UTF-8 literals), then WHATWG CP1252 fallback | CP1252 encode, `?` for unmappable (Windows ACP behaviour) |
| **W (wide)** | UTF-16LE, lossy | UTF-16LE |

The A path is deliberately **asymmetric**: reads favour mingw UTF-8, writes are faithful CP1252. Unicode that must round-trip belongs on the W path — the GUI demo's dialog echo exercises exactly that contract.

## Guest heap

24 size classes (16–65536 bytes, powers-of-two-ish ladder), segregated free-lists + bump allocation from the guest arena, with a `live` map for leak accounting. `alloc_coherent`/`free_coherent` hand the guest a host-allocated block it can fill via normal guest writes (used by D3D9 `LockRect`). Optional accelerators (`WIE_GUEST_HEAP/IO/MBWC`) rewire hot APIs to real guest code.

## Files: the bottle

Guest `C:\` maps to `{root}/drive_c/` (`WIE_ROOT` / `--root`); a `D:` host bridge maps to `WIE_DRIVE_D`. Missing-bottle enforcement is a process-global latch (`BOTTLE_MISSING_ENFORCED`): a file-touching app without a bottle stops with a clear error. The native Open/Save panels are the boundary — a file you pick is mounted at a unique `Z:\pickN\…` path (deduplicated per host file) and read/written in place. The guest has no other way to reach your filesystem: symlink escapes and unmapped paths fail closed.

## SEH and MSVC C++ EH

Win64 SEH dispatch is host-side except the guest's actual handler bodies: the host walks a copy of the guest's `.pdata` function tables, runs unwind-map actions via guest trampolines, and — the surprising part — **CALLs MSVC catch funclets** (they return their continuation IP in RAX; jumping in would make `ret` pop garbage). `msvc_eh.rs` parses `__CxxThrowException` payloads and reconstructs catch frames. Per-TID pending state keeps concurrent throws from overwriting each other. The full walk-through lives in [`docs/cpp-exceptions.md`](../cpp-exceptions.md).

## State

`WinApiState` is the process-global bucket, with a lazy `DllStateMap` (fixed 9-slot array of `Option<Box<dyn Any>>`): Console, Window, D3D9, Pthread, Gdi, Present, Clipboard, Registry, DragDrop. Sub-state is accessed by `DllId` + `get_or_init::<T>`.

## Gotchas

- The fake-VA window IS the IAT — there is no separate resolver on the hot path.
- `WIE_GUEST_HEAP/IO/MBWC` rewiring is IAT-only; the in-guest stub code must exist in the guest image.
- Page-safe string reads stop at 4 KiB page boundaries (never fault on unmapped tails).
- The D3D9 COM vtable encoding preserves unknown interface bytes, so unimplemented extensions get a trace name instead of a panic.
