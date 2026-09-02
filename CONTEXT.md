# CONTEXT — Ubiquitous Language

Glossary only. No implementation detail, no spec, no scratch pad.

| Term | Definition |
| --- | --- |
| **Frame** | `SurfaceFrame` the host presents — 0RGB `pixels` plus `region` and `background_color`. The unit the host `Present` consumes. |
| **Surface** | `WindowSurface` scratch per HWND — `width×height` `Vec<u32>` that accumulates `Paint` blits before a `Publish`. |
| **Paint** | Guest GDI/D3D9 writes into a `Surface` (`fill_rect_surface`, `blit_frame`, D3D9 raster). |
| **Publish** | Move/slice of a `Surface` → `Frame` via `PresentState::publish` / `drain_pending_publishes` / `reconcile_and_publish`, bumping `generation`/`dirty`. |
| **Present** | Host `WgpuPresenter::present` upload+blit of a `Frame` to the swapchain (vblank/`Fifo`). |
| **Dirty region** | `IRect` union of writes since last `Publish` on a `Surface`; stored in `Surface::dirty` and emitted as `Frame::region` hint. `None` = whole surface. |
| **Allocation** | Any host heap `malloc`/`Vec` grow/`Arc` alloc/`HashMap` resize after steady state (after first `Publish` + `generation` stabilize). |
| **Copy** | Any `memcpy`/`copy_from_slice`/padded repack that duplicates pixel bytes outside `Paint`. `Publish` moves must be swaps, not copies. |
| **Vblank coalescing** | Latest-wins per-HWND queue where multiple `Publish`es between two `Present`s collapse into one `Frame` via region union (Q5/C). |
| **Direct register hand-off** | Block-to-block keeps live guest GPRs/`rflags` in native regs (`x19–x28`/`x14`); `JitCtx` spills only on dispatcher miss/host stop/SMC (Q8/C). |
