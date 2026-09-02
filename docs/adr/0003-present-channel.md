# ADR 0003 — Present channel: lock-free host frame delivery (Wave 1a)

Status: Accepted · 2026-09-02 — architecture-review Wave 1a

## Context

Every publish, take and present-timing read ran under the process-wide
`WinApiState` mutex ("the big lock"). The host's per-frame `take_frame` took
the same lock the guest holds for entire WinAPI calls — so a long paint
handler (or a D3D9 Present under a heavy frame) stalled the winit presenter,
and conversely the presenter's lock hold delayed guest execution. This is the
root cause of the "JIT and rendering at the same time" ceiling: rendering is
serialized behind guest work instead of overlapping it
(`architecture-review-games-and-apps.md` painpoint C1/A1).

## Decision

Extract the host-visible frame state into a `PresentChannel`
(`wie-winapi/src/present/mod.rs`) shared as an `Arc` with the runtime's
`GuestHandle`:

1. **Latest-wins slot per HWND** inside one `Mutex<ChannelInner>`, plus the
   presenter timing counters, z-order/z-rev mirrors and `windows_rev`. The
   guest side takes big-lock → channel (publish); the presenter takes
   channel only. Lock-order inversion is impossible because the presenter
   never acquires the big lock.
2. **Wake gate**: `published_seq` / `taken_seq` `AtomicU64` pairs; a publish
   wakes the presenter iff `prev == taken` (the host had drained every
   earlier publish). Suppressed publishes are covered by the in-flight
   redraw taking the LATEST slot; `GuestHandle::pending_frames()` re-arms
   the redraw loop after each take (`RedrawRequested` in `wie-cli`) to close
   the publish-between-wake-and-take window. The gate is global (not
   per-HWND) — correct because the Frame handler requests redraws for ALL
   windows.
3. **Spare pool per HWND**: displaced channel slots and hand-backs of
   presented buffers recycle as the next paint base. With the channel pinning
   a clone of every published frame, the old `Arc::try_unwrap` hand-back
   never fires; the zero-ALLOC path is now the spare pool (one clone only on
   the very first cycle). `hand_back_clone` counts real clones only.

## Consequences

- `take_frame`, `record_present`, z-order reads and windows revision reads no
  longer touch the big lock — presenter-side lock-wait stats drop to zero
  (regression-tested in `session::window::tests`).
- The channel holds an `Arc` clone per published frame until displaced, so
  `published` stays guest-side and composite accumulation flows through
  spares.
- Publish path is unchanged for callers: `publish` mirrors the frame into
  the channel (refcount bump, no pixel copy) and decides the wake.

## Validation

`cargo nextest` — present module tests (wake gate, spare recycling, pointer
identity across the two-publish cycle), `wie-cli` upload tests (stride-aware
zero-copy/pack), micro GUI suite (composite accumulation, resting-frame
hash). Perf: `WIE_RUNTIME_PROFILE=1` capture — `present_ns` counters moved to
the channel; presenter waits on `shared_winapi` must stay at zero.

## Reversibility

The channel is additive: `PresentState` keeps `published` and the big-lock
paths; removing the channel restores the pre-Wave-1a presenter
(`take_frame` under the big lock) by reverting `GuestHandle` field wiring.
