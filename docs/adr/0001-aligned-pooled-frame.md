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

Pad width and pool depth are **default on** (64 px padded per-HWND pool + reused `WgpuPresenter` scratch) — steady zero-alloc with no env required. Opt-out via `WIE_SURFACE_PAD=0` (on-demand alloc) for bisect; Wave3 flipped the gate (was `WIE_SURFACE_PAD=64` opt-in).


## Implementation status (2026-09-02 audit)

**Implemented — with these deviations from the original text:**

1. The 64-px pitch padding is real: `WindowSurface.stride = padded_stride(width)`
   (`width.div_ceil(64) * 64`), `SurfaceFrame` carries `stride`, full-frame
   `write_texture` uploads borrow the buffer zero-copy with
   `bytes_per_row = stride * 4`. Opt-out `WIE_SURFACE_PAD=0` exists as
   described.
2. The pool is NOT a per-HWND `BufferPool` + steady `Arc::try_unwrap`: since
   ADR-0003 the presenter channel pins a clone of every published frame, so
   `try_unwrap` never succeeds in the hand-back. The zero-ALLOC path is the
   per-HWND **spare pool** (displaced channel slots + presented-buffer
   hand-backs); the first cycle pays one clone. `hand_back_unwrap` therefore
   stays 0 in steady state — `hand_back_clone` counts only real clones.
3. The reused `WgpuPresenter` upload scratch is real; region packs pad to the
   COPY width (256-aligned), not the source pitch — a padded surface stride
   would ship the padding tail on every narrow-region upload.
4. Blit paths (GDI fill/BitBlt, DIB blits, text, D3D9 blit_frame, screenshot
   BMP) are stride-aware; `stretch_nearest_strided` handles stretched blits.
