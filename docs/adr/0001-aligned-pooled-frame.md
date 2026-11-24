# ADR 0001 — Aligned pooled Arc frame + reusable upload scratch (Q2/D)

Status: Accepted · 2026-08-26 — grill-with-docs Q1–Q2

## Context

Notepad-like GDI and Doom-like D3D9 both must meet Q1/D (wake→present <16.6 ms P95 while long_loop ≥50 M insn/s).
The current publish path (`PresentState::publish` → `Arc<Vec<u32>>`) and `WgpuPresenter::upload_source` both allocate/copy per frame:
- `Arc<Vec<u32>>` creation + `try_unwrap` hand-back fallback clone of 8 MB at 1080p under `WinApiState`.
- `vec![0_u8; h*padded]` + per-row copy for any width not multiple of 64 or any `Some(region)` partial repaint.

## Decision

Pooled `Arc<Vec<u32>>` with 64 px padding + host-reusable scratch:

1. `WindowSurface` width is padded to 64 px (`(w+63)&!63`) so `row_bytes %256==0` → full-frame `write_texture` is `Cow::Borrowed` zero-copy (`bytemuck::cast_slice`) always.
2. `publish` swaps from a per-HWND `BufferPool` (capacity retained); steady-state `Arc::try_unwrap` succeeds, no fresh `Vec` alloc, no host-hold clone via `spare_buffers` queue.
3. `WgpuPresenter` keeps one reused `Vec<u8>` scratch for `Some(region)` uploads (capacity retained).

## Alternatives considered

- **Swap-pool without Arc (B):** kills Arc refcounts but needs a new fence protocol; rejected — Arc keeps `record`/headless/test paths working and `try_unwrap` already nearly zero-copy.
- **Host-mapped staging (C):** guest paints into `wgpu` mapped memory/IOSurface — tightest copy but couples `wie-winapi` to `wgpu` and locks size to device limits.
- **Pool harder only (A):** widen `spare_buffers` — still pays padded repack for unaligned/region.

## Consequences

- Steady full frames: zero alloc, zero extra copy (one `paint` into `Surface`, one DMA).
- ≤63 px row padding waste (~3–6%).
- Region uploads reuse one scratch; intermediate publishes between two vblanks collapse via Q5/C.

## Validation

40 s `WIE_RUNTIME_PROFILE=1` paired capture (`yield|park`), `wake→present` P95, `hand_back_unwrap` vs `clone`, `present_ns_last`, `Arc::try_unwrap` probe, no alloc on 1000-quanta paint window (Q4/C).

## Reversibility

~~Pad width and pool depth are **default on** (64 px padded per-HWND pool + reused `WgpuPresenter` scratch) — steady zero-alloc with no env required. Opt-out via `WIE_SURFACE_PAD=0`.~~

**Superseded 2026-11: the pitch padding is retired and `WIE_SURFACE_PAD` is deleted.** The padding existed so `stride * 4` would be a multiple of wgpu's 256-byte `COPY_BYTES_PER_ROW_ALIGNMENT` and a full-frame `write_texture` could borrow the buffer. That requirement does not exist on the queue path: `Queue::write_texture` validates with "alignment not required for buffer offset or bytes per row" (`wgpu-core-30.0.0/src/device/queue.rs:888-895`), and on Metal `buffer_copy_pitch` is 4 (`wgpu-hal-30.0.0/src/metal/adapter.rs:1498`), so `stage_bytes_per_row == bytes_per_row` for *any* pitch and the single-memcpy fast path is taken. Meanwhile a padded pitch *defeated* that fast path: the upload was `stride * 4 != width * 4`, so wgpu re-copied row by row, and every producer (blit tail, capture stream, GL present) also had to loop per row instead of one `copy_from_slice`. Surfaces are now pitched at the logical width (`surface_stride`) and the upload borrows the frame for regions as well as full frames. The pool depth part of this ADR stands.


## Implementation status (2026-09-02 audit)

**Implemented — with these deviations from the original text:**

1. ~~The 64-px pitch padding is real~~ **Retired** — see "Reversibility". `WindowSurface.stride = surface_stride(width) = width`, `SurfaceFrame` still carries `stride` (callers stay stride-aware, which is what makes the change safe), and `write_texture` borrows the buffer with `bytes_per_row = stride * 4` for a full frame *and* for a region.
2. The pool is NOT a per-HWND `BufferPool` + steady `Arc::try_unwrap`: since
   ADR-0003 the presenter channel pins a clone of every published frame, so
   `try_unwrap` never succeeds in the hand-back. The zero-ALLOC path is the
   per-HWND **spare pool** (displaced channel slots + presented-buffer
   hand-backs); the first cycle pays one clone. `hand_back_unwrap` therefore
   stays 0 in steady state — `hand_back_clone` counts only real clones.
3. ~~The reused `WgpuPresenter` upload scratch is real; region packs pad to the
   COPY width.~~ **Retired** — the scratch survives only as the fallback for a
   frame buffer too short to cover the copy. Regions borrow: the upload is a
   slice at `(top * stride + left)` with `bytes_per_row = stride * 4`, and wgpu
   walks the rows internally (`queue.rs:1003-1026`). Packing first meant the
   region was copied twice.
4. Blit paths (GDI fill/BitBlt, DIB blits, text, D3D9 blit_frame, screenshot
   BMP) are stride-aware; `stretch_nearest_strided` handles stretched blits.
