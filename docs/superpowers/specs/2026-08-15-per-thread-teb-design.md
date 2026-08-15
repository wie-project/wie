# Per-Thread Guest TEB Design

## Goal

Give every guest thread an independent Thread Environment Block (TEB) and GS base so thread-local state, especially `LastErrorValue`, no longer relies on one shared guest mirror.

## Context

WIE currently stores authoritative last-error values per host `GuestThread`, but publishes the active value into one fixed guest address. This transitional mirror preserves existing handlers but permits cross-thread races when guest code reads or writes the shared TEB slot.

## Design

Each guest thread receives a dedicated guest TEB page. The page contains the existing TEB fields at unchanged offsets. The thread's CPU engine uses that page as its GS-base translation for guest stubs and guest code. `GetLastError` and `SetLastError` therefore operate on the active thread's TEB without host-side mirror copies.

`GuestThread` remains the host-side authoritative state during the migration. The runtime initializes the primary TEB exactly as today. Worker creation allocates and initializes a distinct TEB page, associates it with the worker CPU engine, and releases it when the worker terminates.

The transitional shared `ProcessState.last_error`, `absorb_guest_last_error`, `publish_last_error_to_guest`, and shared-mirror cache are removed only after all handler and worker paths use the per-thread TEB. Handler code continues to set the active thread's host last-error through the existing `HandlerContext` state seam until a later cleanup migrates those writes.

## Invariants

- Guest virtual addresses remain software-translated. No host mapping occurs at the guest TEB address.
- The Win64 ABI and all existing TEB field offsets remain unchanged.
- `GetLastError` and `SetLastError` are thread-local across primary and worker threads.
- JIT and iced engines observe identical GS-base behavior.
- Worker teardown releases its TEB allocation after the engine can no longer access it.
- The primary thread keeps its current TEB address and initial zero value for compatibility.

## Failure handling

TEB allocation or mapping failure aborts thread creation through the existing runtime error path. It must not create a worker with a null or shared TEB. A stale worker TEB cannot be reused by another thread until its guest mapping and host association are fully cleared.

## Testing

- Unit-test TEB allocation, initialization, and teardown.
- Test two guest threads writing distinct last-error values and reading them back independently.
- Run the existing last-error, TLS, pthread, and multi-thread micro tests under JIT and iced.
- Run workspace formatting, clippy, tests, and the debug micro-suite.
- Run a debug doomretro smoke after the change.

## Non-goals

This design does not split the GUI presenter lock, introduce `HostView` snapshots, or change D3D9/present ownership. Those changes remain deferred until interactive lock-wait evidence exists.
