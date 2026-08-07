# C++ Exception Handling on Windows x64

This document explains how WIE dispatches Win64 SEH and MSVC C++ exception handling end to end. WIE hosts the machine-code machinery — `.pdata` function tables, the unwind walk, and the dispatch loop (`wie-winapi::exception`, `seh.rs`) — and does the type-matching itself (`msvc_eh.rs`, `exception/unwind.rs`). The guest's CRT still owns the exception object and the destructor code, which WIE invokes through guest trampolines (UnwindMap actions, MSVC catch funclets); the guest personality functions (`__CxxFrameHandler3` / `__gxx_personality_seh0`) are never called. The sections below walk the mechanism from the compiler's `throw` down to the catch block, then describe where each piece lives in WIE and what the guest CRT still does itself.

## 1. The Core Problem: Why Exceptions Are Not Just a Function Call

A normal WinAPI call in WIE works like this:

```
guest calls HeapAlloc → fake VA hit → host handler runs → return_from_win64_api(RAX) → guest resumes
```

The host handler reads guest registers (RCX, RDX, R8, R9 for arguments), does its work, sets RAX, and returns. The guest continues executing the next instruction. **Only one stack frame is involved** — the caller pops the return address and continues.

A C++ `throw` needs to do something fundamentally different. Consider this code:

```cpp
void inner() { throw std::runtime_error("bad"); }
void outer() { inner(); }

int main() {
    try { outer(); }
    catch (std::runtime_error& e) {
        // execution jumps here directly from inner()
    }
    // execution continues here after catch
}
```

When `inner()` executes `throw`, the stack looks like this (growing downward):

```
Stack:
  [main locals]
  [main's call to outer: return address, saved rbp, ...]
  [outer locals]
  [outer's call to inner: return address, saved rbp, ...]
  [inner locals]                    ← RSP points here
  [throw in progress ...]
```

The `throw` must:

1. **Skip `outer` entirely** — no return to `outer`, no execution of code after the call to `inner`.
2. **Restore `main`'s registers** — RSP, RBP, RBX, RDI, RSI, R12-R15 must be exactly as they were when `main` called `outer`.
3. **Jump to the catch block** inside `main` — which is an address that the compiler computed, not a return address on the stack.

This cannot be done with ordinary `call`/`ret` because:
- Each `ret` can only pop **one** return address. To skip `outer`, you must pop **two** frames' worth of saved data.
- `ret` restores nothing except RIP. Exception handling must restore **all non-volatile registers** — and the values to restore are stored in different places on the stack for each function (saved registers in prologue, stack allocation sizes vary, etc.).
- `ret` always jumps to the return address on the stack. The catch block is **not** that address — it's a completely different code path the compiler generated.

**This is why exceptions need dedicated OS machinery.** The hardware has no instruction for "unwind N frames and restore all registers." The OS builds that from three pieces:
- **Metadata** that describes each function's stack layout (`.pdata`/`.xdata`)
- **An unwinder** that reads that metadata and reverses the prologue (restores registers, pops frames)
- **A dispatcher** that walks the stack, asks each frame "can you handle this?", and unwinds to the one that says yes

---

## 2. Step by Step: What Actually Happens

### Step 1: The compiler turns `throw` into a CRT call

The compiler generates:

```cpp
// What the compiler emits for: throw std::runtime_error("bad");
void* obj = _new(sizeof(runtime_error));  // allocate on heap
runtime_error::runtime_error(obj, "bad");
__cxx_throw_exception(obj, &typeid(runtime_error));
// (never returns)
```

`__cxx_throw_exception` is in the C++ runtime library (libstdc++ for Mingw, msvcrt for MSVC). It constructs the OS-level exception record and calls the OS:

```cpp
void __cxx_throw_exception(void* obj, void* typeinfo) {
    struct {                  // EXCEPTION_RECORD
        uint32_t code;
        uint32_t flags;
        void*   record;
        void*   address;
        uint32_t num_params;
        uint64_t params[15];
    } record;

    record.code = 0xE06D7363;           // "msvc" in ASCII — C++ magic
    record.flags = 1;                    // non-continuable
    record.params[0] = 0x19930522;       // MSVC EH magic
    record.params[1] = (uint64_t)obj;    // the exception object
    record.params[2] = (uint64_t)typeinfo;
    record.num_params = 3;

    RaiseException(&record);             // call the OS
}
```

The C++ magic number `0xE06D7363` is how the dispatcher knows this is a C++ exception and not, say, an access violation (`0xC0000005`) or a deliberate crash.

### Step 2: `RaiseException` enters the OS dispatcher

`RaiseException` stores the `EXCEPTION_RECORD` on the guest stack and calls `RtlDispatchException` with the record and the current register context (CONTEXT). The dispatch loop:

```
RtlDispatchException(EXCEPTION_RECORD*, CONTEXT*):
    frame = current (RIP is inside RaiseException or __cxx_throw_exception)
    while true:
        // Find unwind info for the function containing frame.RIP
        entry = RtlLookupFunctionEntry(frame.RIP)
        if entry == NULL:
            // Leaf function or JIT code — no handler possible, skip it
            frame = unwind_leaf(frame)
            continue

        // Read the UNWIND_INFO header
        info = read_unwind_info(entry.UnwindData)
        if info.Flags & UNW_FLAG_EHANDLER == 0:
            // This function has no exception handler — just unwind past it
            frame = RtlVirtualUnwind(frame, entry)
            continue

        // This function HAS an exception handler. Call it.
        disposition = call_language_specific_handler(
            info.handler_address,    // e.g. __gxx_personality_v0
            &record,                 // the exception
            &frame                   // current register state
        )

        if disposition == "I can handle this":
            // The handler modified frame to point at the catch block
            RtlRestoreContext(&frame)   // set all registers, jump to catch
            // (never returns)
        elif disposition == "not me, keep looking":
            frame = RtlVirtualUnwind(frame, entry)
            continue
        elif disposition == "collided unwind":
            // already being unwound — shouldn't happen on first pass
            abort()
        elif no more frames:
            // No handler found anywhere → terminate process
            RtlRaiseStatus(STATUS_FATAL_APP_EXIT)
```

This loop is the entire exception dispatch mechanism. **The OS doesn't understand C++ types, `catch` blocks, or destructors.** It only knows three things:
1. How to look up a function's unwind info (`RtlLookupFunctionEntry`)
2. How to reverse a function's prologue (`RtlVirtualUnwind`)
3. How to call the function's registered handler

Everything C++-specific (type matching, destructor execution, catch block address) is inside that handler function.

### Step 3: The language-specific handler checks if it can catch this

When the dispatcher finds a frame with `UNW_FLAG_EHANDLER` set, it calls the function pointer stored in the `.xdata`:

```c
// The function pointer stored in .xdata next to the UWOP codes:
void* handler_address;  // points to __gxx_personality_v0 or __CxxFrameHandler3
```

This function receives:

```
__gxx_personality_v0(
    EXCEPTION_RECORD* record,    // contains 0xE06D7363 + the typeinfo pointer
    void*            image_base, // base address of the module
    CONTEXT*         context,    // registers at the throw site
    void*            handler_data // pointer to FuncInfo (MSVC) or LSDA (Itanium)
)
```

The handler:
1. Looks at `record.ExceptionCode`. It's `0xE06D7363`? Good, this is a C++ exception.
2. Extracts the thrown typeinfo from `record.ExceptionInformation[2]`.
3. Reads the function's handler table (`FuncInfo` for MSVC, call-site table for Itanium) from the handler data area in `.xdata`.
4. Finds the entry covering `context.Rip` (the address where the exception was thrown).
5. Iterates the catch types for that entry. Compares each against the thrown typeinfo using RTTI (`type_info::before()` or address comparison).
6. If a type matches:
   - Computes the landing pad address (the catch block's code).
   - Writes the exception object pointer into the CONTEXT's RCX (the catch parameter).
   - Returns `ExceptionCollidedUnwind` (for MSVC) or modifies CONTEXT.Rip to jump to the landing pad.
7. If no type matches:
   - For unwind-only handlers (destructors): records that cleanup is needed.
   - Returns `ExceptionContinueSearch`.

### Step 4: The two-pass model

Windows uses a **two-pass** model for exception handling:

**Pass 1 (search pass):** Walk the stack asking each handler "can you catch this?" Do NOT run destructors. Just find the catching frame. This pass runs with `EXCEPTION_RECORD.Flags` bit 1 set (non-continuable, no side effects).

```
Search pass:
  Walk from inner() up to main():
    main()'s handler: "yes, I have a catch for std::runtime_error"
  → Found catching frame at main()
```

**Pass 2 (unwind pass):** Walk from the throw site TO the catching frame. For each frame in between, call the handler with `UWACTION_TERMINATE` to run destructors, then unwind past it. Stop at the catching frame and jump to the landing pad.

```
Unwind pass:
  inner()  → run destructors for inner's locals, unwind
  outer()  → run destructors for outer's locals, unwind
  main()   → jump to catch block (RSP restored, RIP = catch address)
```

The catch block now executes with the stack in the state it was when `main` called `outer` — as if `outer` had returned normally, but skipping all the code between the call and the catch.

### Step 5: Destructor cleanup

Between the throw and the catch, stack-allocated objects need their destructors called. This is handled by **unwind-only handlers** or **cleanup entries** in the call-site table.

For a function like:

```cpp
void outer() {
    std::string s = "hello";   // destructor must run if exception passes through
    inner();                   // throws
    // this code never executes if inner() throws
}
```

The compiler generates two entries for `outer`:

| RIP range | Action | Handler |
|---|---|---|
| `[call inner ... throw]` | Call destructor of `s`, then re-throw | Yes (cleanup) |
| `[rest of function]` | No handler | No |

During pass 2, when the dispatcher unwinds through `outer`, it calls the handler with the cleanup action. The handler emits code that calls `s.~string()`, then returns "continue unwind."

---

## 3. Why the Language-Specific Handler Is the Hard Part

The `.xdata` can be parsed generically. The UWOP codes are well-defined. The stack walk is arithmetic. All of these are straightforward to implement.

The language-specific handler is hard because it requires **parsing compiler-generated EH data structures that differ between MSVC and Mingw**:

| Compiler | Handler function | Data structure | Magic |
|---|---|---|---|
| **MSVC** | `__CxxFrameHandler3` | `FuncInfo`, `TryBlockMapEntry`, `HandlerType` | `0x19930522` |
| **Mingw-w64** | `__gxx_personality_v0` | Itanium LSDA (call-site table, type table) | DWARF-based `.gcc_except_table` |
| **Clang (Windows)** | `__CxxFrameHandler3` (MSVC-compatible) or custom | Same as MSVC | Same magic |

These data structures are stored in different sections (`.xdata` for MSVC, `.gcc_except_table` for Mingw) and use completely different formats for matching types.

WIE's actual split is **host-side parsing with guest-side actions**:

- **Host side** (`seh.rs` + `exception/`): WIE parses both `FuncInfo` (MSVC) and the Itanium LSDA (Mingw) from guest memory, does the catch type-matching itself, and plans the transfer as a list of steps (`SehStep`). The personality RVA in `.xdata` is used only to locate the language data — `__CxxFrameHandler3` / `__gxx_personality_seh0` are never called by the dispatcher. If guest code reaches the `__c_specific_handler` / `__cxxframehandler` import directly, the host stub (`ucrt/misc.rs::handle_c_specific_handler`) returns continue-search.
- **Guest side**: only the destructor actions (MSVC `UnwindMap` action RVAs) and the MSVC catch funclets execute as guest code, entered via `setup_guest_call` with the SEH continue trampoline as the return address.

The tradeoff: this avoids host-stopping on every CRT import inside a personality function, at the cost of reimplementing the catch-matching logic host-side (`msvc_eh.rs` for `FuncInfo`, `exception/unwind.rs` + `dwarf.rs` for the LSDA).

---

## 4. Architecture: Four Layers

```
┌─────────────────────────────────────────────────────┐
│ Layer 4: C++ Runtime  (__CxxThrowException,         │
│          __CxxFrameHandler3, personality_v0)         │
├─────────────────────────────────────────────────────┤
│ Layer 3: SEH Dispatcher (RtlDispatchException,      │
│          RtlUnwindEx, RaiseException)                │
├─────────────────────────────────────────────────────┤
│ Layer 2: Stack Unwinder (RtlVirtualUnwind,          │
│          RtlLookupFunctionEntry)                     │
├─────────────────────────────────────────────────────┤
│ Layer 1: Metadata     (.pdata / .xdata sections,    │
│          RUNTIME_FUNCTION, UNWIND_INFO)              │
└─────────────────────────────────────────────────────┘
```

WIE must implement layers 1-3 (the OS machinery). Layer 4 (the C++ runtime) belongs to the guest — WIE only needs to bridge into it.

---

## 5. Layer 1: PE Unwind Metadata (`.pdata` / `.xdata`)

Every x64 PE image has an exception directory pointing to two sections:

| Section | Contains | Size per entry |
|---------|----------|----------------|
| `.pdata` | `RUNTIME_FUNCTION[ ]` — start addr, end addr, pointer to unwind info | 12 bytes |
| `.xdata` | `UNWIND_INFO` + array of `UNWIND_CODE[ ]` | variable |

`RUNTIME_FUNCTION` (C struct):

```c
typedef struct _RUNTIME_FUNCTION {
    DWORD BeginAddress;   // RVA of function start
    DWORD EndAddress;     // RVA of function end
    DWORD UnwindData;     // RVA of UNWIND_INFO, or 0 if no unwind data
} RUNTIME_FUNCTION;
```

`UNWIND_INFO` (C struct):

```c
typedef struct _UNWIND_INFO {
    UBYTE Version       : 3;   // always 1
    UBYTE Flags         : 5;   // UNW_FLAG_NHANDLER, UNW_FLAG_EHANDLER, UNW_FLAG_UHANDLER
    UBYTE SizeOfProlog;       // length of function prologue in bytes
    UBYTE CountOfCodes;       // number of UNWIND_CODE entries
    UBYTE FrameRegister : 4;  // nonvolatile register used as frame pointer (0 = none)
    UBYTE FrameOffset   : 4;  // scaled offset from frame register to RSP at entry
    UNWIND_CODE UnwindCode[];  // variable-length array of unwind codes
    // optionally followed by handler data (if Flags & UNW_FLAG_EHANDLER)
} UNWIND_INFO;
```

Each `UNWIND_CODE` is 2 bytes describing one stack operation in the prologue:

| Opcode | Name | Meaning |
|--------|------|---------|
| 0 | `UWOP_PUSH_NONVOL` | Push a nonvolatile integer register (RBP, RBX, RDI, RSI, R12-R15) |
| 1 | `UWOP_ALLOC_LARGE` | `sub rsp, large_value` (>128 bytes, up to 512K) |
| 2 | `UWOP_ALLOC_SMALL` | `sub rsp, small_value` (8-128 bytes in 8-byte steps) |
| 3 | `UWOP_SET_FPREG` | Establish frame pointer: `lea rbp, [rsp + offset]` |
| 4 | `UWOP_SAVE_NONVOL` | Save nonvolatile register at `[rsp + offset]` |
| 5 | `UWOP_SAVE_NONVOL_FAR` | Save at large offset (≥ 64KB frame) |
| 6 | `UWOP_SAVE_XMM128` | Save XMM register at `[rsp + offset]` |
| 7 | `UWOP_SAVE_XMM128_FAR` | Save XMM at large offset |
| 8 | `UWOP_PUSH_MACHFRAME` | Push trap/exception/machine frame (interrupt) |

Unwind codes are stored in reverse execution order (last prologue instruction first).

### WIE responsibility

- **Parse `.pdata`** at session init: `wie_winapi::exception::parse_pdata` reads the raw section bytes into a sorted `Vec<RuntimeFunction>`; `wie-runtime/src/session/init.rs` registers the result as `SyncState.function_tables[image_base]`. `.xdata` is not pre-parsed — it stays in guest memory and is read lazily during unwinding.
- **`exception::lookup_function_entry`**: given a guest RIP, range-check each registered module table, then binary-search by `BeginAddress` for the covering `RuntimeFunction`.
- **Dynamic tables**: only the main module's table is registered today. `RtlAddFunctionTable` for JIT code or loaded DLLs is **not implemented**; a RIP with no covering entry falls back to the leaf unwind path (see section 10).
- **JIT integration**: Cranelift emits no unwind metadata (`unwind_info = false`), and no `RtlInstallFunctionTableCallback` exists — the unwind walk only ever sees guest RIPs, so a JIT block address never reaches `lookup_function_entry` (section 10).

---

## 6. Layer 2: Stack Unwinding (`RtlVirtualUnwind`)

`RtlVirtualUnwind` reverses one function's prologue. Given a CONTEXT with RIP inside a function, it produces the CONTEXT of the caller.

### Algorithm

```
RtlVirtualUnwind(CONTEXT* ctx, RUNTIME_FUNCTION* entry):
    info = read_unwind_info(entry.UnwindData)

    // Start with the current RSP from the context
    let rsp = ctx.Rsp

    // If function uses a frame pointer, compute the original RSP
    if info.FrameRegister != 0:
        let fp_val = ctx.GPR[info.FrameRegister]
        let fp_rsp = fp_val - (info.FrameOffset * 16)
        // Use fp_rsp as the base for UWOP_SAVE_* operations below

    // Process UNWIND_CODEs in forward order (first prologue instruction first).
    // Each code describes one operation. To reverse it, do the opposite.
    let codes = read_unwind_codes(info)
    for code in codes (in order):
        match code.opcode:
            UWOP_PUSH_NONVOL:
                // Reverse: pop register from stack
                rsp -= 8
                ctx.GPR[code.register] = read_guest_memory(rsp)

            UWOP_ALLOC_SMALL:
                rsp += (code.info * 8) + 8

            UWOP_ALLOC_LARGE:
                if code.info == 0:
                    rsp += read_next_code_slot()  // 16-bit scaled
                else:
                    rsp += read_next_two_code_slots()  // 32-bit

            UWOP_SET_FPREG:
                // Already handled above via FrameRegister

            UWOP_SAVE_NONVOL:
                let offset = read_next_code_slot() * 8
                ctx.GPR[code.register] = read_guest_memory(fp_rsp + offset)

            UWOP_SAVE_NONVOL_FAR:
                let offset = read_next_two_code_slots()  // 16-bit scaled * 8
                ctx.GPR[code.register] = read_guest_memory(fp_rsp + offset)

            UWOP_SAVE_XMM128:
                let offset = read_next_code_slot() * 16
                ctx.XMM[code.register] = read_guest_memory_128(fp_rsp + offset)

            UWOP_SAVE_XMM128_FAR:
                let offset = read_next_two_code_slots() * 16
                ctx.XMM[code.register] = read_guest_memory_128(fp_rsp + offset)

            UWOP_PUSH_MACHFRAME:
                // Trap frame: return address + error code pushed by CPU
                // Skip error code (if present) and return address
                rsp += (code.info == 0 ? 24 : 32)

    // After processing all codes, RSP points past the return address.
    // Pop it into RIP.
    ctx.Rip = read_guest_memory(rsp)
    rsp += 8
    ctx.Rsp = rsp

    // If handler info is present, return it
    if info.Flags & UNW_FLAG_EHANDLER:
        return (ctx, handler_function_pointer, handler_data_pointer)
    else:
        return (ctx, null, null)
```

### Edge cases

- **Leaf functions**: no `.pdata` entry. Treat as `RSP += 8; pop RIP`. Return no handler.
- **Chained unwind info** (`UNW_FLAG_CHAININFO`): the unwind info points to another `UNWIND_INFO` for the parent function. Follow the chain.
- **Epilogue in progress**: if RIP is in an epilogue (not the prologue), the unwind codes don't apply — just return `RSP`, pop return address, done.

### WIE responsibility

- Implement `virtual_unwind` as a pure function reading guest memory (`wie-winapi/src/exception/unwind.rs`).
- Maintain an `UnwindContext` (RIP, RSP, GPR[0..15], XMM[0..15]) — no RFLAGS or segment registers (section 9).
- Return handler information (`UnwindResult.handler_rva` / `handler_data` / `exception_data_va`) so the dispatcher can decide whether to match a handler or keep unwinding.
- Leaf functions and chained unwind info are handled; **epilogue detection is not implemented** — the UWOP interpretation assumes RIP is in the prologue region.

---

## 7. Layer 3: SEH Dispatcher

When guest code calls `RaiseException`:

```c
void RaiseException(EXCEPTION_RECORD* record) {
    CONTEXT ctx;
    RtlCaptureContext(&ctx);      // save current registers
    ctx.Rip = exception_address;   // the throw site
    RtlDispatchException(record, &ctx);  // never returns on success
}
```

`RtlDispatchException` implements the two-pass model. WIE reimplements the same two passes host-side with its own symbols — `dispatch_exception`, `search_and_plan`, `continue_pending` (`seh.rs`) — and no guest call into `RtlDispatchException`. The pseudocode below is the canonical Windows flow; the mapping to WIE's actual code is in "WIE responsibility".

### Pass 1: Search

```
RtlDispatchException(record, ctx):
    // Pass 1: find the handler
    frame_ctx = *ctx  // copy
    while true:
        entry = RtlLookupFunctionEntry(frame_ctx.Rip)
        if entry == null:
            // No unwind info — leaf function, skip
            unwind_leaf(&frame_ctx)
            if frame_ctx.Rsp == 0:  // bottom of stack
                goto no_handler
            continue

        info = read_unwind_info(entry.UnwindData)
        if info.Flags & UNW_FLAG_EHANDLER:
            // Frame has a handler — ask it
            disposition = call_handler(
                info.handler, &record, info.image_base,
                &frame_ctx, info.handler_data
            )
            if disposition == HANDLER_FOUND:
                goto pass_2(record, ctx, &frame_ctx)
            // else: continue searching

        // Unwind to the next frame
        RtlVirtualUnwind(&frame_ctx, entry)

    no_handler:
        // Nobody caught it — terminate or call unhandled-exception filter
        RtlRaiseStatus(STATUS_FATAL_APP_EXIT)
```

### Pass 2: Unwind

```
pass_2(record, ctx, target_ctx):
    unwind_ctx = *ctx  // copy from the throw site
    while unwind_ctx.Rip != target_ctx.Rip:
        entry = RtlLookupFunctionEntry(unwind_ctx.Rip)
        info = read_unwind_info(entry.UnwindData)

        // Call the handler with terminate action (run destructors)
        if info.Flags & (UNW_FLAG_EHANDLER | UNW_FLAG_UHANDLER):
            call_handler(
                info.handler, &record, info.image_base,
                &unwind_ctx, info.handler_data, ACTION_TERMINATE
            )
            // Handler ran destructors for this frame, re-throwing

        // Unwind past this frame
        RtlVirtualUnwind(&unwind_ctx, entry)

    // Now at the catching frame — set registers and jump
    RtlRestoreContext(&target_ctx)  // never returns
```

`RtlRestoreContext` writes the CONTEXT's register values into the CPU and jumps to `target_ctx.Rip`. In WIE, this means writing back into the `CpuEngine`'s register file and setting RIP.

### WIE responsibility

- **Entry points.** `KERNEL32!RaiseException` → `handle_raise_exception` (`kernel32/misc/mod.rs`) → `seh::dispatch_exception` (`seh.rs`), which recovers a `ThrowPayload` (MSVC `0xE06D7363` or GCC `0x20474343` — `' GCC'`) from the register arguments or from an `EXCEPTION_RECORD*` in RCX. `msvcrt!_CxxThrowException` → `handle_cxx_throw_exception` (`ucrt/misc.rs`), which passes `pExceptionObject` / `pThrowInfo` to `dispatch_exception_with_payload`. Guest hardware faults (access violation, divide-by-zero) → `seh::dispatch_hardware_fault`, called from the runtime pump (`wie-runtime/src/session/pump.rs`).
- **Search + plan (pass 1).** `dispatch_exception_with_payload` snapshots `ThreadContext`, reads the throw site (the return address on the guest stack), and calls `search_and_plan`. That walks frames with `unwind_one` (→ `exception::lookup_function_entry` + `exception::virtual_unwind`; leaf frames just pop the return address) and resolves a catch with `resolve_landing_pad`: MSVC `FuncInfo` via `msvc_eh::find_msvc_catch`, or the Itanium LSDA via `exception::find_landing_pad_ex`. Instead of returning a disposition, it **plans** pass 2 as a `Vec<SehStep>`: `Action` (MSVC `UnwindMap` destructor RVA), `MsvcCatch` (funclet call), `Jump` (Mingw landing pad).
- **Step execution (pass 2).** `begin_or_finish` stores the steps in `state.kernel.seh_pending[tid]` and `run_next_step` executes the front step. Guest code runs only for destructor actions and MSVC catch funclets, via `setup_guest_call`: it pushes a return address of `seh_continue_trampoline_va` (a fake VA) and sets RDX = establisher frame. When the guest `ret`s into that trampoline, the runtime pump (`pump.rs`) or the MT worker (`mt_runtime.rs`) calls `seh::continue_pending`, which pops the next step — or, for a returned MSVC catch funclet, treats RAX as the continuation IP and resumes the catching function.
- **Register restore.** The terminal `SehStep::Jump` / `SehStep::MsvcCatch` restores the catch frame's GPRs/XMMs, RSP and RIP through `engine.restore_thread_context` + `write_rip` / `write_rsp` / `write_rax` / `write_rdx`. There is no `RtlRestoreContext` symbol.
- **`RtlUnwindEx`.** `handle_rtl_unwind_ex` (`kernel32/misc/mod.rs`) → `seh::forced_unwind_to`. When a Mingw cleanup landing pad left `expect_cleanup_resume` set, this drains the next planned step via `continue_pending`; otherwise it walks frames with `unwind_one` until `target_frame_rsp` / `target_ip` and restores the resulting context.
- **VEH chain**: `RtlAddVectoredExceptionHandler` is **not implemented** — no vectored handlers run before the SEH walk.
- **Handler invocation bridge**: `setup_guest_call` (push RA = `seh_continue_trampoline_va`, RDX = establisher, set RIP). The host never calls the guest language-specific handler, so there is no disposition read back from RAX — the only guest-returned value consumed is the MSVC catch funclet's continuation IP.

---

## 8. Layer 4: C++ Runtime (Guest-Side)

Layer 4 runs inside the guest. WIE only needs to bridge into it.

### The `FuncInfo` structure (MSVC)

Each function with try/catch has a `FuncInfo` table pointed to by the handler data in `.xdata`:

```c
struct FuncInfo {
    uint32_t magicNumber;       // 0x19930520 or 0x19930521
    uint32_t maxState;          // number of unwind states
    UnwindMapEntry* pUnwindMap;   // state → action mapping
    uint32_t nTryBlocks;
    TryBlockMapEntry* pTryBlockMap;  // try blocks with their catch types
    uint32_t nIPMapEntries;
    IPMapEntry* pIPMapEntries;      // code range → state mapping
};
```

### The LSDA / call-site table (Mingw / Itanium ABI)

```c
struct LSDA {
    uint8_t  lpStartEncoding;      // DWARF encoding for landing pad base
    uint8_t  ttypeEncoding;        // DWARF encoding for type info table
    uint8_t  call_site_encoding;   // DWARF encoding for call site entries
    // call-site table:
    //   [start_offset, length, landing_pad, action_index]
    // action table:
    //   [type_index, next_action]  (type 0 = catch all, negative = cleanup)
    // type table:
    //   [typeinfo pointers]  (used with ttype_encoding for RTTI matching)
};
```

### The ThrowInfo (MSVC)

```c
struct ThrowInfo {
    uint32_t attributes;            // 0 = normal, 1 = const object
    void*    pmfnUnwind;            // destructor for the exception object
    int*     pForwardCompat;        // always 0
    CatchableTypeArray* pCatchableTypeArray;  // list of types this throw matches
};
```

### The handler's job

When the dispatcher calls `__CxxFrameHandler3` or `__gxx_personality_v0`, the handler:

1. Reads the function's handler table from guest memory.
2. Finds the entry covering the throw site address.
3. Iterates the catch types for that entry.
4. Compares each against the thrown typeinfo (via RTTI `type_info::before()` or direct pointer comparison for simple types).
5. If matched:
   - Adjusts the exception object pointer (for pointer-to-base conversions).
   - Writes the pointer into the CONTEXT's RCX (the catch-by-value or catch-by-reference parameter).
   - Returns the landing pad address (the catch block).
6. If not matched (but this is a cleanup handler):
   - Runs destructors for objects in scope.
   - Re-throws (lets the unwind continue).

### WIE responsibility

- **Host-side matching, guest-side actions.** WIE parses `FuncInfo` (`msvc_eh.rs`) and the Itanium LSDA (`exception/unwind.rs` + `dwarf.rs`) itself, and plans the dispatch as `SehStep`s. The guest's personality / frame handlers are **not invoked** — only the pieces of guest code those handlers would have run:
  - `SehStep::Action` — MSVC `UnwindMap` destructor RVAs collected by `msvc_eh::collect_unwind_actions`, entered via `setup_guest_call` (RDX = establisher frame, RA = `seh_continue_trampoline_va`).
  - `SehStep::MsvcCatch` — the matched MSVC catch funclet, CALLed with the catch object placed at the `HandlerType.dispCatchObj` slot (`place_msvc_catch_object`); on return, RAX holds the continuation IP (`continue_pending`).
  - `SehStep::Jump` — a Mingw/Itanium landing pad, entered by restoring the catch frame's registers; intermediate cleanup pads end in `_Unwind_Resume` → `RtlUnwindEx` → `forced_unwind_to`, which continues with the next pending step.
- The type matching WIE does host-side: MSVC compares `ThrowInfo` `CatchableType` RVAs against `HandlerType.typeRva` (`msvc_eh::handler_matches`); Mingw compares the thrown typeinfo pointer against the LSDA type table (`match_action` in `exception/unwind.rs`). Catch-all (`...`) matches without a type table.

---

## 9. Register Contexts: ThreadContext and UnwindContext (no CONTEXT64)

WIE does **not** maintain the full OS `CONTEXT64` (1232 bytes). Exception handling uses two smaller register shapes, both indexed by GPR number (RAX=0 … R15=15):

```rust
// wie-cpu/src/regs.rs — thread-switch snapshot, also the dispatch entry/exit shape
pub struct ThreadContext {
    pub gpr: [u64; 16],      // RAX..R15
    pub xmm: [u128; 16],     // XMM0..XMM15
    pub rip: u64,
    pub rflags: Rflags,
}

// wie-winapi/src/exception/mod.rs — what the unwinder walks
pub struct UnwindContext {
    pub rip: u64,
    pub rsp: u64,
    pub gpr: [u64; 16],
    pub xmm: [u128; 16],     // only indices 6..=15 (XMM6–XMM15) are restored
}
```

`RtlCaptureContext` (host stub `handle_rtl_capture_context`, `kernel32/misc/mod.rs`) writes a **partial** `CONTEXT64` subset into the guest buffer so filter expressions that read the context see a plausible shape: ContextFlags at +0x30, Rflags at +0x44, RAX..R15 at +0x78..+0xF8, RIP at +0xF8, XMM0..XMM15 at +0x100 (16 bytes each). This is a compatibility subset — WIE never parses a full `CONTEXT64` back.

### WIE responsibility

- No full `CONTEXT64` struct and no `RegFile ↔ CONTEXT64` conversion exist. The dispatcher snapshots `ThreadContext` (`engine.snapshot_thread_context()`) and walks `UnwindContext`; the final register restore is `engine.restore_thread_context` + `write_rip` / `write_rsp` / `write_rax` / `write_rdx` in `seh.rs::run_next_step`.
- The nonvolatile registers (RBX, RBP, RDI, RSI, R12-R15, XMM6-XMM15) are the only ones the unwinder restores — the UWOP codes only ever name registers the function saved, which are the nonvolatiles. Volatile registers (RAX, RCX, RDX, R8-R11, XMM0-XMM5) can be anything.

---

## 10. JIT Integration

Cranelift-compiled blocks would need unwind metadata only if `lookup_function_entry` could see a JIT block address. Three approaches exist:

1. **Emit `.pdata`/`.xdata` per block** — Cranelift can generate `UNWIND_INFO` when `unwind_info = true` is set in the flags. Each compiled block gets a `RUNTIME_FUNCTION` entry. Register the JIT code range as a function table via `RtlAddFunctionTable`. This is the cleanest approach.

2. **`RtlInstallFunctionTableCallback`** — register a callback that, given a RIP, returns a synthetic `RUNTIME_FUNCTION`. The callback describes the Cranelift frame layout: standard prologue (save LR, push frame pointer), fixed stack allocation. No per-block metadata needed.

3. **Trap and redirect** — if the dispatcher reaches a JIT code address with no function table entry, fall through to the iced interpreter path. The interpreter has a normal `.pdata` entry. This is the simplest MVP but means exceptions in hot JIT code always fall back to iced.

None of the three is used. `unwind_info` stays `false` (`wie-cpu/src/jit/engine.rs`), so compiled blocks register no unwind metadata, and no function-table callback exists. It is not needed — the dispatcher unwinds **guest** stack frames whose RIPs are all guest VAs, so `lookup_function_entry` never sees a JIT block address.

---

## 11. Implementation

### Metadata foundation
- `RuntimeFunction`, `UnwindInfo`, `UnwindCode` structs in `wie-winapi/src/exception/mod.rs` (PE/COFF §5 layouts).
- `.pdata` parsing: `wie_winapi::exception::parse_pdata` (`exception/mod.rs`) reads the raw section bytes into a sorted `Vec<RuntimeFunction>`; `wie-runtime/src/session/init.rs` registers the main module's table as `SyncState.function_tables[image_base]`. Loaded DLLs currently register no table — unwinding through a DLL frame falls back to the leaf path.
- `SyncState.function_tables: HashMap<u64, Vec<RuntimeFunction>>` (`sync_obj.rs`): image base → sorted entries.
- `exception::lookup_function_entry(&sync, control_pc)` → `Option<FunctionEntry>` (`exception/mod.rs`): per-module range check + binary search by `BeginAddress`.

### Virtual unwinding
- `exception::virtual_unwind(read_mem, image_base, entry, ctx)` in `wie-winapi/src/exception/unwind.rs`. Interprets the UWOP codes and returns the caller's `UnwindContext` plus `UnwindResult` (handler RVA, language-data DWORD, embedded-LSDA VA).
- Leaf functions (no unwind data) pop the return address (`unwind_leaf`); chained unwind info (`UNW_FLAG_CHAININFO`) recurses into the chained `UNWIND_INFO`. **Epilogue detection is not implemented.**
- The Mingw LSDA walk lives in the same module: `find_landing_pad_ex` (catch match with type filter + handler switch value) and `find_cleanup_landing_pad` (action-index-0 cleanup pads), with `DW_EH_PE` encodings in `exception/dwarf.rs`.
- The register shape is `UnwindContext` (`exception/mod.rs`), not a `CONTEXT64`; there is no `RegFile ↔ CONTEXT64` conversion (section 9).

### Exception dispatch
- Entry: `kernel32/misc/mod.rs::handle_raise_exception` → `seh::dispatch_exception` (recovers `ThrowPayload`); `ucrt/misc.rs::handle_cxx_throw_exception` → `seh::dispatch_exception_with_payload`; runtime faults → `seh::dispatch_hardware_fault`.
- `seh::search_and_plan` walks frames once (`unwind_one`) and produces `Vec<SehStep>` (`Action` / `MsvcCatch` / `Jump`). `begin_or_finish` stores a `SehPending` per TID in `state.kernel.seh_pending`; `run_next_step` executes the steps; guest continuations return through the `seh_continue_trampoline_va` fake VA → `seh::continue_pending` (`pump.rs` / `mt_runtime.rs`).
- MSVC catch funclets are CALLed (`RDX` = establisher; `RAX` = continuation IP on return) — `SehStep::MsvcCatch` + `continue_pending`. Mingw landing pads are entered by register restore (`SehStep::Jump`); cleanup pads end in `_Unwind_Resume` → `RtlUnwindEx` → `forced_unwind_to`, which drains the next pending step (`has_cleanup_resume`).
- Register restore is `engine.restore_thread_context` + `write_rip` / `write_rsp` / `write_rax` / `write_rdx` in `run_next_step` — there is no `RtlRestoreContext`, and no guest personality call.

### C++ integration
- `handle_cxx_throw_exception` (`ucrt/misc.rs`) builds the C++ exception record and enters SEH dispatch instead of aborting the session.
- Mingw-w64 micro-exes (`throw int; catch(int)`), destructor cleanup between throw and catch, and `__CxxFrameHandler3` FuncInfo parsing (`msvc_eh.rs`) exercise the same path.

### JIT unwind metadata
- Cranelift emits no unwind metadata (`unwind_info = false`, `wie-cpu/src/jit/engine.rs`). JIT blocks do not need it: the unwind walk only ever sees guest RIPs, so a JIT block address never reaches `lookup_function_entry`.

---

## 12. What WIE Does NOT Need to Implement

| Component | Handled by |
|-----------|-----------|
| Type info comparison (RTTI) | Host — `msvc_eh::handler_matches` (CatchableType RVAs) and `match_action` in `exception/unwind.rs` (typeinfo pointer) |
| Destructor calls during unwind | Guest code, dispatched by the host: MSVC `UnwindMap` action RVAs (`SehStep::Action`) and Mingw cleanup pads (`SehStep::Jump`) |
| Exception object construction/destruction | Guest CRT (heap-allocated by `__cxx_throw_exception`) |
| Stack cookie (/GS) checks | Guest code (compiled into the binary) |
| `SetUnhandledExceptionFilter` logic | Host stub `handle_set_unhandled_exception_filter` ignores the filter and returns NULL (`kernel32/misc/mod.rs`) |
| `longjmp` / `setjmp` | Separate mechanism (not exception-based) |

---

## 13. Key References

- Microsoft PE/COFF spec §5 (x64 exception handling)
- `except_x64.c` in Wine source (`dlls/ntdll/unwind/`)
- Rust `pelite` crate for `.pdata` parsing reference
- `_CxxThrowException` / `__CxxFrameHandler3` signatures from MSVC CRT headers
- Windows Internals Part 1, Chapter 3 (exception dispatching)
- Itanium C++ ABI §6 (personality function, LSDA layout) — for Mingw-w64
- `libstdc++-v3/libsupc++/eh_personality.cc` — reference implementation of `__gxx_personality_v0`
