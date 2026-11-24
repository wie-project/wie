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

Ordered by expected payoff per unit of risk. Each is real — these are not
nice-to-haves invented to fill a list.

1. **32-bit ALU typing in the JIT.** `mask_width` ANDs both operands *and* the
   result of every 32-bit ALU/cmp/test/inc/dec with `0xffffffff` as an I64 value
   (`lower/flags.rs:93-105`). `0xffffffff` is not an encodable AArch64 logical
   immediate, so the backend materialises it in a register, and at
   `opt_level = none` nothing folds the repeats. Doing the op in I32 plus a
   per-register known-zero-extended bit should take `add ecx,eax` from ~5 host
   instructions to ~2. This is the dominant x86-64 instruction class and no
   experiment has touched it. *Needs:* a disassembly knob to confirm the shapes
   before and after (`Context::compile` already takes `want_disasm`).
2. **The XMM split-pair round trip.** XMM lives as 32 scalar I64 halves, so one
   `paddb` becomes ~17 host ops around one real one — convert both operands,
   operate, convert back, then re-convert to store
   (`sse.rs:846-860`, `analysis.rs:101-160`). Pays on SSE-heavy guests (the DOOM
   Retro class), ~0 on 7-Zip (SSE is 0.36 % of its instructions). *Needs:* the
   write-through to stay (it is a fault-semantics guard, and removing it once
   caused the notepad CRLF bug); only the repack in `store_xmm_pair` is free.
3. **D3D9 buffer and texture snapshots.** Each buffer-form draw clones the whole
   vertex stream and index buffer (`d3d9/draw.rs:131-158`); each recorded draw
   clones every texture's texels and every mip (`d3d9/capture.rs:258-268`) — 4 MB
   per draw for a 1024² texture. Render targets already use `Arc<Vec<u32>>` with
   `make_mut`; buffers and textures are the same fix. Only when the capture
   stream is on.
4. **`wcscpy`/`wcscat`/`wcsncpy`/`wcsnicmp`/`wcsrchr`/`wcsstr`** still read per
   unit, and `handle_strstr` also grows a `Vec` byte by byte. The page-sliced
   helpers this round added (`utf16_len`, `utf16_cmp`) are the shape to follow.
5. **The dead shadow return stack.** `shadow_push` on every guest `call`,
   `shadow_pop_check` (a three-block diamond) on every `ret` — and nothing reads
   the result: `shadow_pop_check` returns `ret_va` unchanged and `chain_tail`
   never consults it. `docs/architecture/cpu-and-memory.md:53` still claims it
   "validates ret targets". Removal touches `frame_entry_tests`.
6. **D3D9 readback row buffers** (`d3d9/texture.rs:603,687-698,868`): one
   allocation per scan line inside a LockRect loop.
7. **Scalar fills that cannot vectorise as written**: `fill_rect_surface`
   (`gdi32/blit.rs:555-562`), `fill_backbuffer_rect` (`d3d9/raster.rs:77-90`),
   `host_fill_pattern` (`exec/string.rs:190-215`). Each stores one u32 per
   iteration; a splat + 4-wide store is the same change `write_run` already made
   for the present path. *Settle first:* whether the autovectoriser already
   fires (repo notes say LTO suppresses it) — `cargo asm` on the one function.

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

`cargo nextest run --workspace` (2053 tests) · `cargo clippy --workspace
--all-targets` (zero warnings, `deny`-level table in `Cargo.toml`) ·
`scripts/check.sh --only file-sizes --only dep-direction --only winapi-port
--only state-bindings`. Behavioural evidence where a test cannot reach it:
`7za x` extraction compared byte-for-byte against the corpus, and
`micro_gui_window`'s resting-frame hash for the present changes.
