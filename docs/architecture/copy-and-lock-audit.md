# Copy and lock audit: where the emulator still moves data it need not

Four read-only audits (present/frame path, WinAPI-handler marshalling, JIT
lowering, SIMD/kernels) ranked every site by *host work per guest byte*, with a
mechanism and a `file:line` for each. This document records what landed, what it
was measured at, and what is left — including the items that are real but large
enough to need their own round.

Target is macOS/aarch64: the "use NEON" instruction is not decoration, it is the
only host that ships.

## 1. What landed, and why it was worth it

| change | mechanism | evidence |
|---|---|---|
| Handler string reads: `read_guest_str`, `fputs`, `puts` | `mem_read` per **byte** → one lock per 4 KiB page, read straight into the destination (no staging array) | `fputs` was the #1 handler by time in both micro baselines (`baselines/yield.txt:358`) |
| `wcslen`, `wcscmp`, `wcsncmp` | `mem_read` per UTF-16 **unit** → borrowed page slices, no allocation for the length | same defect shape; `wcslen` is on every path/text op |
| `gl_frame_into` (GL present) | per-pixel masked scalar loop + two divisions/pixel → `mask_words_to_0rgb` and `stretch_nearest_masked_strided` (NEON, shared with the software present path) | ~2 M bounds-checked iterations at 1080p per `wglSwapBuffers` |
| Present upload | packed every region into scratch, then wgpu copied it again → borrow the frame at its own pitch, `origin = (left, top)` | `write_texture` needs no `bytes_per_row` alignment (`wgpu-core-30.0.0 queue.rs:888-895`; Metal `buffer_copy_pitch` = 4) |
| Surface pitch (ADR-0001) | 64-px pad **retired**; `surface_stride(width) == width`, `WIE_SURFACE_PAD` deleted | the pad *defeated* wgpu's single-memcpy fast path (`queue.rs:982-1003`) and forced per-row copies in three producers |
| `ensure_surface` hand-back | copied the whole previous frame into the next paint base every frame → full-surface painters skip it | the channel pins a clone of every published frame, so `try_unwrap` never wins; 8.3 MB at 1080p |
| Streaming `ReadFile` | zeroed `Vec` → fill → truncate → `mem_write` → read straight into guest memory via `host_slice_mut` | verified: `7za x` extracts byte-identical corpus files |
| Interpreted residue (`exec::step`) | took the memory read lock **per interpreted instruction** → the engine's cached pointer, as `run_compiled` already did | this is a startup/spin path, i.e. the shape DOOM Retro sits in |
| JIT pin hoist | six pin/generation loads at the entry of every block, including blocks with no memory operand | `opt_level = none`, so nothing removed the dead loads |
| `lower/tlb.rs` | `page_tlb_entry_walk(k).or_else(\|\| page_tlb_entry(k))` — the same function twice | the fallback could not fire; it cost a second walk per miss |

### The one measured default flip

`WIE_JIT_MEM` defaulted to `pin` on a DOOM Retro *startup* helper count
(21 M → 2.4 M). Three independent paired samples say that arm is the slow one:

| sample | class | median ratio (sticky ÷ pin) | spread | pairs |
|---|---|---|---|---|
| binary A | `7za h` | 0.745 | 0.721–0.750 | 6/6 |
| binary A | `7za x` | 0.775 | 0.766–0.779 | 6/6 |
| binary B | `7za h` | 0.759 | 0.745–0.766 | 8/8 |
| binary B | `7za x` | 0.771 | 0.766–0.776 | 8/8 |
| current tree | `7za h` (pin opt-in) | **1.331** | 1.320–1.335 | 6/6 |
| current tree | `7za x` (pin opt-in) | **1.304** | 1.289–1.325 | 6/6 |

Guest instruction counts are unchanged; the loss is IR volume (per-compile cost
is higher at every block-size bucket). `sticky` is the default, `pin` is opt-in,
and `pin` returns only on throughput evidence from the GPU-bound class it was
meant for.

## 2. What is left, ranked

Three of the four items closed. One was refuted by measurement, and the thing
that measurement exposed was fixed instead. What remains is at the bottom.

### Closed

- **The XMM split-pair round trip.** `store_xmm_pair` always rebuilt the vector
  from `(lo, hi)` with a splat, two `insertlane`s and a `bitcast`, even when the
  caller still held the vector it had just produced. `PackedXmm { vec, lo, hi }`
  plus `store_xmm_packed` write through the vector when it exists (`vec: Some`)
  and keep the old rebuild otherwise (`vec: None`). The store itself is
  unchanged — same `MemEnv`, same point in the block — because the write-through
  is a fault-semantics guard and removing it is what caused the notepad CRLF
  bug; only the *recomputation* is gone, which is four host instructions around
  every packed integer op.
- **D3D9 buffer and texture snapshots.** `BufferRecord::data` and
  `TextureRecord::{pixels, mips}` are now `Arc<Vec<_>>` written with
  `Arc::make_mut`, so a draw that does not write a texture refcount-bumps
  instead of cloning 4 MB. `stream_base` is a *suffix*, so a bare `Arc` clone
  would have shifted every vertex; the payload is a `StreamBytes { bytes,
  offset }` handle and `as_slice()` reports the suffix. `state/d3d9.rs` needed
  no change (`Arc::unwrap_or_clone` at Release).
- **The dead shadow return stack.** `JitCtx::shadow_sp`/`shadow_ret`,
  `SHADOW_DEPTH`, `OFF_SHADOW_*`, `emit::shadow_push`/`shadow_pop_check`,
  `trampolines::shadow_pop_check` and its three reset sites are gone:
  `shadow_pop_check` returned `ret_va` unchanged and `chain_tail` never
  consulted it, so it was a three-block diamond on every `ret` for a value
  nothing read. Removing it exposed a real gap it had been hiding —
  `emit_fingerprint` did not cover the `JitCtx` layout at all, so a restored
  blob would have used stale offsets. It now mixes `size_of::<JitCtx>()` and
  `JitConfig::FRAME_LAYOUT_VERSION`.

### Refuted with numbers, and what replaced it

**32-bit ALU typing in the JIT** was ranked first here on the premise that
masked 32-bit ALU ops are the dominant class. `WIE_JIT_DUMP_IR=<path>` now
dumps CLIF (every 50th compile, capped at 2000 blocks, so it samples the whole
run rather than its prologue), and counting `band` against `iconst` widths says
the premise is false for everything measurable: **zero 32-bit masks** in both
7-Zip `x` and DOOM Retro, against 4,137 and 109 **8-bit** masks. There is
nothing to retype on these guests, so the I32-plus-known-zero-extended plan is
documented and not done — worth revisiting only when a guest actually shows
32-bit masks in the dump.

What the dump did find is a smaller, real waste: **9.0 % (7-Zip) / 5.5 % (DOOM
Retro) of masks were exact `(value, width)` duplicates inside one block**,
because the arithmetic lowering masks its operands and result and
`add_sub_preds` then re-masks all three. `mask_width` now memoises per block in
a `thread_local!` cleared by `flags::reset_mask_cache()` at block start.

### Still open, ranked

1. **`wcscpy`/`wcscat`/`wcsncpy`/`wcsnicmp`/`wcsrchr`/`wcsstr`** still read per
   unit, and `handle_strstr` grows a `Vec` byte by byte. The page-sliced
   `utf16_len`/`utf16_cmp` helpers are the shape to follow.
2. **D3D9 readback row buffers** (`d3d9/texture.rs:603,687-698,868`): one
   allocation per scan line inside a LockRect loop.
3. **Scalar fills that cannot vectorise as written**: `fill_rect_surface`
   (`gdi32/blit.rs:555-562`), `fill_backbuffer_rect` (`d3d9/raster.rs:77-90`),
   `host_fill_pattern` (`exec/string.rs:190-215`). Each stores one u32 per
   iteration; a splat + 4-wide store is the change `write_run` already made for
   the present path. *Settle first:* whether the autovectoriser already fires
   (repo notes say LTO suppresses it) — `cargo asm` on the one function.

## 3. Deliberately not done

- **Vectorising the rasterisers.** Measured at 0.023–0.22 ms/frame; the loop is
  not the cost.
- **Re-vectorising the GDI text blend.** Already measured: NEON was slower
  (1.51 vs 1.2 ns/px).
- **Removing the `mask_bgra_to_0rgb` alpha AND** from the software present path.
  It is one NEON AND per 4 pixels on a memory-bound copy.
- **Chain-store elimination, TLB warm-start, SMC drain, `guest_va == host_va`.**
  Already refuted with numbers in `performance-ceiling.md` §5; do not re-open.

## 4. How this was verified

`cargo nextest run --workspace` (2059 tests) · `cargo clippy --workspace
--all-targets` (zero warnings, `deny`-level table in `Cargo.toml`) ·
`scripts/check.sh --only file-sizes --only dep-direction --only winapi-port
--only state-bindings`. Behavioural evidence where a test cannot reach it:
`7za x` extraction compared byte-for-byte against the corpus, and
`micro_gui_window`'s resting-frame hash for the present changes.
