# The XMM split-pair ceiling

**Verdict: drop the round trip.** Mechanically counted, one guest packed op costs
**8 pair↔vector conversion instructions around 1 instruction of real work**
(10 host instructions total, verified instruction-by-instruction in the emitted
AArch64), and **17.3 %** of the one real packed-SIMD loop in the population. But
that loop, and all **128** other compiled blocks that contain a packed-integer
site, have **0 leaf samples** in a 100 s run: **99.7 %** of sampled guest time is
in four scalar blocks with **0.0 %** conversion traffic. The measured ceiling on
this workload is **0.0 % of guest time**.

The two things this measurement did find are cheap and land in code that is
measured hot or measured expensive: an un-folded `movi.16b` in every rebuild
(§3.2), and the `cmp`/`cset`/mask split of the EFLAGS share (§4) — 21 % by the
narrow count, 36 % by the wide one, and the 15-point gap is attributable
instruction by instruction.

Date: 2026-11-24, HEAD `f5a954d`, host macOS 26.5.2 / Apple Silicon, release
build, `opt_level=none`, `WIE_JIT_REGION` off. Every run bounded by
`timeout -k 5 -s INT 120` and started from an idle host.

---

## 1. Where the SIMD actually is

Two runs, each ≤120 s, each after the previous process had exited:

```sh
# run 1 — discovery: host-address map + a 3 s window of leaf samples at t≈75 s
WIE_JIT_MAP=/tmp/x1map.txt WIE_JIT_DUMP_IR=/tmp/x1.clif \
timeout -k 5 -s INT 120 ./target/release/wie run --bottle doomretro --gui \
  doomretro.exe -- -iwad 'C:\Program Files\DoomRetro\freedoom2.wad'
#   → sample <pid> 3, then attribute each leaf PC through the map
```

Leaf-sample attribution (1 919 leaf samples, 94.9 % inside a compiled block):

| guest block | guest insns | leaf samples | share |
| --- | --- | --- | --- |
| `0x1800d8fb0` SDL2 scaled blit | 8 | 985 | **54.1 %** |
| `0x1800a1c60` SDL2 `memcpy4` | 6 | 499 | **27.4 %** |
| `0x1800cef0b` SDL2 dword fill | 10 | 266 | **14.6 %** |
| `0x1400f8dd0` DOOM `R_DrawMaskedColumn` | 7 | 66 | **3.6 %** |
| `0x1800a6a10` | 3 | 5 | 0.3 % |

Not one of them contains a packed-integer instruction. To make that a measurement
rather than an absence, I enumerated every `padd*`/`psub*`/`pand*`/`por`/`pxor`/
`pmul*`/`pack*`/`punpck*`/`pshuf*`/`ps*ll`/`ps*r*` site in both images with
`llvm-objdump` (**1 761** sites), intersected with the 13 256 blocks the run
compiled, and looked for samples:

* **128** compiled blocks contain a packed-integer site.
* **All 128 have 0 leaf samples.**

The packed population is real, and cold: DOOM Retro's colour/tint table build
(`0x1400df3b1`, 59 guest instructions, a loop of 64 iterations doing
`cvtdq2ps`/`mulps`/`paddd`/`punpckldq`/`packuswb`) and SDL2's conversion helpers.

The only SIMD in a block that does get sampled is `0x1401550c0` — the CRT
`memset`, 8 × `movdqa %xmm0,(%rcx)` per iteration — and its problem is **not**
the round trip: measured in this run, that block has **2** conversions in 1 200
host instructions (0.2 %), **1** `ldr q` and **0** `str q`. Each 16-byte guest
store is still lowered as two 8-byte stores through two refill probes
(`str x1, [x3, x0]` / `str x3, [x10, x2]` — the shape audit §5.9 records, still
present). That is a `store_sse_mem` item, not this one.

---

## 2. Conversions per guest SIMD instruction, counted

### 2.1 The lowering, from a one-instruction fixture

`WIE_JIT_DUMP_IR=/tmp/padd.clif WIE_JIT_DUMP_IR_RIP=0x20000000 cargo test -p
wie-cpu --lib simd_packed_arith_matches_iced` dumps the CLIF of a block whose
only guest instruction is `paddb xmm0, xmm1` (fixture base `SIMD_BASE`). The whole
lowering, in order:

```text
v27 = load.i8x16 [ctx+2496]     ; XMM0 as one 128-bit load
v28 = bitcast.i64x2             ; ┐ load split
v29 = extractlane v28, 0        ; │
v30 = extractlane v28, 1        ; ┘
v33 = load.i8x16 [ctx+2512]     ; XMM1
v34 = bitcast.i64x2
v35 = extractlane v34, 0        ; ┐ load split
v36 = extractlane v34, 1        ; ┘
v39 = splat.i64x2 0             ; ┐ rebuild A
v40 = insertlane v39, v29, 0    ; │
v41 = insertlane v40, v30, 1    ; ┘
v42 = bitcast.i8x16             ; (free)
v43 = bitcast.i8x16             ; (free, second bitcast from pair_to_vec)
v45 = splat.i64x2 0             ; ┐ rebuild B
v46 = insertlane v45, v35, 0    ; │
v47 = insertlane v46, v36, 1    ; ┘
v48 = bitcast.i8x16             ; (free)
v49 = bitcast.i8x16             ; (free)
v50 = iadd v43, v49             ; ← the guest's paddb
v51 = bitcast.i8x16             ; ┐ result split
v52 = bitcast.i64x2             ; │
v53 = extractlane v52, 0        ; │
v54 = extractlane v52, 1        ; ┘
store v50, [ctx+2496]           ; write-through (fault guard), str q
```

Conversion inventory per guest packed op: **2 splats + 4 `insertlane` +
6 `extractlane`** = 12 lane-data-movement ops, 10 `bitcast`s (free), 1 lane op,
1 store. When the same XMM is already in the SSA bank (subsequent ops in a block,
or a loop), the two load splits disappear and the count is **8 conversions**
(2 splats + 4 inserts + 2 extracts) per packed op.

### 2.2 What that costs in host instructions

`WIE_JIT_MAP` → `lldb -p <pid> -o 'disassemble …'` on the packed loop's host
address (`0x17b940000` in that run). One guest `paddd` is, verbatim:

```text
0x17b940180  movi.16b v0, #0x0        ; splat          ← insertlane 0 does not fold
0x17b940184  mov.d   v0[0], x6        ; insertlane 0
0x17b940188  mov.d   v0[1], x7        ; insertlane 1
0x17b94018c  movi.16b v1, #0x0        ; splat
0x17b940190  mov.d   v1[0], x2        ; insertlane 0
0x17b940194  mov.d   v1[1], x3        ; insertlane 1
0x17b940198  add.4s  v0, v0, v1       ; ← the guest's paddd
0x17b94019c  mov.d   x2, v0[0]        ; extractlane 0
0x17b9401a0  mov.d   x3, v0[1]        ; extractlane 1
0x17b9401a4  str     q0, [x27, #0x9c0] ; write-through store
```

**10 host instructions: 8 conversions, 1 real op, 1 store.** So the conversion
CLIF count is the host count 1:1 — `movi.16b` for a splat, `mov.d` in both
directions, `bitcast` free — which is why the CLIF count is used as a host count
here and nowhere else in this document.

### 2.3 The real block

Same block, whole function: **1 470 host instructions**, of which
**254 are conversions** (70 `movi.16b` + 184 `mov.d`) — **17.3 %** — against
**20 lane-typed vector ALU ops** (`add.4s`, `sshl.4s`, `dup.4s`, …). That is
**12.7 conversions per vector op**, higher than the 8 of §2.1 because the block
also contains `movd`/`pshufd`/`packuswb` patterns that each rebuild the vector
twice (§2.4).

### 2.4 Pays versus does not pay

Guest SIMD census of that 59-instruction loop (all 59 decoded, the block ends at
`jle 0x1400df3b1`):

| guest op | count | pays? |
| --- | --- | --- |
| `paddd`, `punpckldq`, `punpcklqdq`, `pshufd`, `mulps`, `cvtdq2ps`, `packuswb` | 5 + 6 + 3 + 1 + 4 + 4 + … | **yes** — the x86 semantic is lane work on one 128-bit value |
| `movd` (xmm↔gpr) | 13 | **no** — the guest asked for an xmm↔gpr move; the one conversion is the semantics |
| `movdqa`/`movups` | 5 | no — memory form, `store_sse_mem`/`load_sse_mem` |

254 conversions / 40 SIMD instructions = 6.4; / 27 paying instructions = 9.4.
Worth noting: the **no-pay class is not free either** — an x86 `movd r9d, xmm2`
means "low lane = GPR, upper lanes zero", which AArch64 does in **one**
instruction (`fmov d0, x`) but WIE emits as `movi.16b` + 2 `mov.d` + `str q` = 4.
The split costs 3 instructions even where the conversion is architecturally
required.

---

## 3. Converted, and what the fix would be

### 3.1 Per iteration and as a percentage

| population | conversions per iteration | % of the block body |
| --- | --- | --- |
| cold packed loop `0x1400df3b1` (59 guest insns) | 254 | **17.3 %** |
| hot scalar block `0x1800d8fb0` (54 % of time) | 0 | **0.0 %** |
| hot CRT memset `0x1401550c0` (SIMD store path) | 2 | **0.2 %** |

Weighted by measured guest time: **0.0 %**. Drop.

### 3.2 The fix is a type change, not a lowering change — and that is decidable

The distinction the brief asks for is not ambiguous, and the disassembly answers
it: **Cranelift's 128-bit type is real and the AArch64 backend already keeps it in
one host register.** Evidence from this very block: `add.4s v0, v0, v1`,
`sshl.4s v3, v0, v1`, `dup.4s v0, v0[0]`, `str q0, [x27, #0x9c0]`,
`ldr q0, [x27, #0x9c0]`. There is no backend limitation and nothing to fold; the
split exists only because the SSA bank is `xmm: [Value; 32]` of **two I64 halves**
(`analysis.rs`, `read_xmm_pair`/`store_xmm_pair`/`pair_to_i8x16`).

Consequently "keep the split and elide only the conversions" is **not** an
available option: `pair_to_vec` runs per guest op because `xmm[2i]`/`xmm[2i+1]`
are the only carried representation — there is no "vector already in a lane
register" slot in the bank to reuse. `PackedXmm { vec, lo, hi }` only reuses the
vector *within* one instruction's store, which is why the round trip is still
there. The realistic fix is:

* **`xmm: [Value; 32]` (I64 lo/hi) → `[Value; 16]` (I8X16 or I64X2).** Packed ops
  become one `iadd.i8x16`/`add.4s` with no conversions; the `JitCtx` write-through
  is already `str q`; loads are already `ldr q`. Pair extraction survives only at
  the architectural boundaries — `movd`/`movq`/**`pmovmskb`/`movmskps`** and the
  scalar-FP ops that need lane 0 — where it costs 1 instruction, not 3.
* Cost of that change: every `read_xmm_pair`/`store_xmm_pair`/`pair_to_i8x16`/
  `pair_to_vec` call site in `sse.rs`, `sse_fp.rs`, `analysis.rs`; the
  `WIE_JIT_SIMD=0` fallback becomes either a second bank layout or goes away; and
  the interpreter/JIT parity suite (`simd_parity_tests.rs`, 40-odd fixtured
  mnemonics) re-verifies it. That is a large diff *for a population with zero
  samples.*

### 3.3 The cheap half, visible in the same 10 instructions

`movi.16b v0, #0` is emitted **separately before every rebuild** — 2 of the 10
instructions per packed op, 20 % of the round trip, and it is provably
redundant: `insertlane 0` into a zeroed `i64x2` is `fmov d0, x`, which zeroes bits
64…127 as a side effect. In CLIF terms, `pair_to_i8x16`'s
`splat(I64X2, 0) + insertlane(0)` should be one `insertlane` on an undefined
vector (or the backend should fold `splat 0` + `insertlane 0`, which it does not
do at `opt_level=none`). One function, no type change, −2 instructions per
packed op, −20 % of the packed block's instruction count. Still 0 % on this
workload, which is why it is a note and not the recommendation.

---

## 4. The EFLAGS share, and where the 21 % and 38 % numbers differ

Measured on the block that owns **54.1 %** of guest time, `0x1800d8fb0` (SDL2's
16.16 scaled blit: `mov`, `add`, `shr`, `mov`, `mov`, `add`, `sub`, `jne`), from
the CLIF dump of that exact RIP (`WIE_JIT_DUMP_IR_RIP=0x1800d8f00-0x1800d9000`)
and from its machine code (573 instructions, `lldb`).

### 4.1 Two counts, two rules

| method | share | how |
| --- | --- | --- |
| **CLIF backward slice** from the store into `JitCtx+128` | **117 / 560 = 20.9 %** | every def transitively feeding the packed `rflags` word |
| of which **flag-only** (no consumer outside the cone) | **112 = 20.0 %** | shared with the guest result: 5 = 0.9 % |
| **machine code, attributed** | **124 / 573 = 21.6 %** | `cmp`/`cmn`/`tst` 45 + `cset`/`csel` 27 + ops whose immediate is a flag mask 42 + `movk`/`movz` constant build 10 |
| **machine code, wide mnemonic census** | **206 / 573 = 36.0 %** | every `cmp/cset/and/orr/eor/mov/movk/uxtb/lsr` |

So the region lane's **21.5 %** and this block's **20.9–21.6 %** are the same
quantity measured two ways, and the audit's **~38 %** is the *wide* census. The
15-point gap is attributable, instruction class by instruction class: of the 134
instructions in those classes, 52 are the flag machinery counted above and ~82
(14 %) are `and`/`orr`/`eor` address and result masking, guest `mov`s, `movk`
64-bit constant builds, and `uxtb` zero-extends. **Use 21 % for "EFLAGS
construction"; the extra 15 points is not flags.**

### 4.2 The shapes inside the cone (what to change, in `flags.rs`)

The cone's opcode histogram is the actionable part:

```text
iconst.i64 41 · band 14 · uextend.i64 12 · bor 12 · bxor 10 · ishl 10 · icmp 8 · ushr 6
```

Reading the ordered slice:

1. **Block-entry decompose: 24 ops.** Six `(iconst, band, iconst, icmp)` groups,
   one per carried bit — ZF@0x40, SF@0x80, CF@0x1, OF@0x800, PF@0x4, AF@0x10 —
   paid at the top of *every* block that touches flags, before any guest
   instruction runs. Lazy `FlagState` already exists for the region body; the
   block path pays this eagerly.
2. **`iconst` is the largest single class: 41 of 117 cone ops (35 %).** At
   `opt_level=none` each is a `mov`/`movz`/`movk`: the block's machine code has
   **41 `mov` + 14 `movk` = 55 instructions (9.6 %)** supporting them, including
   the 4-instruction `movz`+3×`movk` build of `0x8000_0000_0000_0000`. `mask_width`
   already memoises per block (`thread_local!`, cleared by
   `reset_mask_cache()`); **`iconst_u64` does not**. That is the cheapest single
   change in this block.
3. **The typed-overflow path is firing**: `v142 = sadd_overflow`,
   `v144 = uadd_overflow`, `v328 = ssub_overflow`, `v330 = usub_overflow` — CF/OF
   from the host's own `adds`/`subs` + `cset`, as `flags.rs::overflow_preds`
   intends. Nothing to fix there.
4. **`shrq $0x10` pays 5 ops for an OF nothing reads**: `iadd`, `bxor`(`a`,`b`),
   `bxor`(that, `result`), `iconst 16`+`band`, `icmp ne 0`. The only flag
   consumer in the block is the `jne`, i.e. ZF, and OF is not live in
   (`live_in.rflags == false` for this block's entry word). Gating the shift's OF
   on its liveness is ~5 ops per shift, 1 % of the block.
5. **Per flag-insert: 3 ops.** `select_flag` = `band(cond, bit)` →
   `uextend` → `ishl` → `bor` (12 `uextend` + 10 `ishl` + 12 `bor` in the cone).
   An `csel`-based insert (`csel old|bit, old&~bit, cond`) is 2, and a `bif`
   pair is 2; the shift+or placement is the row that is 3.
6. **The parity fold: 9 ops** — `band 255`, `ushr 4`, `bxor`, `ushr 2`, `bxor`,
   `ushr 1`, `bxor`, `band`, `icmp` — per flag-writing ALU op. x86 PF is rarely
   read; §4.2's item 2 (constants) and this one together are ~14 % of the block.

---

## 5. What would change the verdict

* A hot packed-SIMD block. Today the packed population is 128 compiled blocks,
  0 samples. The measurement to re-run is §1's leaf sample + intersection: it
  takes one 120 s run.
* A guest class where SIMD *is* the frame — SDL2's audio mixer (`paddw`/`pmullw`
  in `SDL_MixAudio`) was never sampled here because the run has no audio device
  open, and a video-decode/filtering workload would look different. If such a
  class appears, the arithmetic is already done: **8 conversions per packed op**
  (§2.1), so a 5-op SIMD loop becomes 40 conversions + 5 ops — the ratio is what
  makes it or breaks it.
* The `movdqa`-store item (§1) does not need this verdict: `store_sse_mem`
  emitting two 8-byte probes for one 16-byte guest store is measured hot in the
  memset bursts and already has the pattern to copy (`analysis.rs`'s `str q`).

## Appendix: reproducing

```sh
# discovery: map + leaf samples (bounded, idle host)
WIE_JIT_MAP=/tmp/x1map.txt WIE_JIT_DUMP_IR=/tmp/x1.clif \
  timeout -k 5 -s INT 120 ./target/release/wie run --bottle doomretro --gui \
    doomretro.exe -- -iwad 'C:\Program Files\DoomRetro\freedoom2.wad' &
sleep 75; sample <pid> 3 -file /tmp/x1samp.txt

# the exact lowering of one guest packed op
WIE_JIT_DUMP_IR=/tmp/padd.clif WIE_JIT_DUMP_IR_RIP=0x20000000 \
  cargo test -p wie-cpu --lib simd_packed_arith_matches_iced
#   (fixture code base is SIMD_BASE = 0x2000_0000, jit/tests/mod.rs)

# a hot block's CLIF (the flags slice)
WIE_JIT_DUMP_IR=/tmp/x2.clif WIE_JIT_DUMP_IR_RIP=0x1800d8f00-0x1800d9000 \
  timeout -k 5 -s INT 120 ./target/release/wie run …

# machine code of the same blocks
lldb -p <pid> --batch -o 'disassemble --start-address <host> --count 1500' \
  -o detach -o quit

# every packed-SIMD site in the shipped images
llvm-objdump --triple=x86_64-pc-windows-msvc -d real_exes/doomretro/SDL2.dll \
  | grep -E '\b(padd\w+|pand|por|pxor|pmull\w+|pack\w+|punpck\w+|pshuf\w+)\b'
```

Counting rules: a "conversion" is `splat`/`insertlane`/`extractlane` in CLIF and
`movi.16b`/`mov.d` in AArch64, verified 1:1 in §2.2 (`bitcast` is free at both
levels). The EFLAGS cone is a backward slice from the `store` into `JitCtx+128`;
"flag-only" means every use of the def lies inside that slice.
