# Instruction-selection audit — the ten hottest guest blocks of DOOM Retro's render phase

Date: 2026-11-24. Repo: `/Users/yanis/Programming/wie`. Host: macOS 26.5.2, Apple Silicon, 8 cores.
Audited binary: `target/release/wie` as built at 18:16:58 on 2026-11-24 from the working tree at
HEAD `bc38820` **plus the uncommitted `crates/wie-cpu/src/jit/lower/` work in flight at that
moment** (`analysis.rs`, `emit.rs`, `flags.rs`, `gpr.rs`, `insn.rs`, `mod.rs`). That is a caveat,
not a detail: the disassembly below is a snapshot of the tree *with* that work, so re-run §8's
commands once it lands before trusting a specific instruction offset.

Scope: read-only. No `.rs` file was modified for this audit, and no test suite was run.

---

## 0. What this document answers

The owner's target is 2–3 host cycles per guest instruction; this run's render phase executes
**346.5 M guest instructions per second** on this host, i.e. roughly 10 host cycles per guest
instruction, and the blocks below are the ones that set that number. This audit takes the **top ten
guest blocks by sampled guest time in the render phase**, disassembles each emitted aarch64 block,
and answers three questions per block:

1. how many host instructions does it cost, and per guest instruction;
2. where the host code is worse than the guest's own optimal encoding, with both sides shown;
3. where *our lowering* forces the worse form — i.e. which `lower/*.rs` site to change.

The headline: **the render phase is 84 % six tight guest loops of 6–33 instructions, and each costs
42–76 host instructions per guest instruction** (≤117 for the CRT `memset` block). **≈17 % of
render-phase guest time is removable outright** in seven mechanical classes (ranked in §7), and a
further ≈16 % is host-stack spill traffic that follows from the same live-value pressure. The
frame-offset class from the previous pass is confirmed already optimal (§6.1) — that negative is
repeated here so it is not re-attempted.

**Settled since this audit, with measurements** — details, provenance and addresses in
[`scaled-blit-block-attribution.md`](scaled-blit-block-attribution.md) §12:

* **§5.1 (probe predicates) is spent.** The emitted guard on the hottest block is now
  3 instructions per guarded access, 6 per iteration (`sub`/`cmp`/`b.lo`, bias folded
  into the addressing mode), and the mapping-generation term is folded into the
  descriptor at block entry. A per-iteration `mem_gen` compare would *add* 3
  instructions, so the "hoist it out of the loop" item has no work left in it.
* **§5.3 (EFLAGS rebuilt bit by bit) is spent for the hot path.** Pack and PF are both
  **0 instructions per iteration** on that block: the four pack sites and the three
  parity ladders are all in blocks that are not backward-reachable from the back edge.
* **The flag share in §5.3 is superseded.** Flag materialisation is 14.6 % of that
  block's 239-instruction iteration, not 37.3 % or 44.3 % — those are shares of the
  *pre-deferral* 266- and 174-instruction revisions of the same block.
* **AF cannot be deferred** (239 / 245 / 251 across three binaries; the loss is reload
  pressure from a register cliff, not read count), and the OF/CF carrier is negative
  expected value even though OF/CF has zero hot consumers.


---

## 1. Method (exact commands)

All numbers are **single-run**, on this machine, while two other agents were building (load average
10.2 on 8 cores at the start of the session). Sampling *shares* are robust to that; absolute rates
are not, and every absolute figure below is labelled.

```bash
# 1. render-phase sampling: three 2 s windows at t=60/80/100 s of one live GUI run
WIE_RUNTIME_PROFILE=1 WIE_JIT_MAP=/tmp/rmap1.txt \
  ./target/release/wie run --bottle doomretro --gui doomretro.exe \
    -- -iwad 'C:\Program Files\DoomRetro\freedoom2.wad' &
PID=$!; sleep 60; sample $PID 2 -file /tmp/rs_60.txt; \
       sleep 20; sample $PID 2 -file /tmp/rs_80.txt; \
       sleep 20; sample $PID 2 -file /tmp/rs_100.txt; kill -INT $PID
```

`sample` leaves are attributed to a guest block by mapping each sampled host address through
`WIE_JIT_MAP` (`host_addr guest_start insns` per compiled block; the emitted block owns the
16 KiB JIT slot at its host address). Only the `wie-guest-primary` thread's leaves count; `nosym`
and host leaves are excluded from the shares.

```bash
# 2. the same run shape, disassembling each top block from the live process
WIE_JIT_MAP=/tmp/rmap2.txt ./target/release/wie run ... --gui doomretro.exe -- -iwad ... &
sleep 70
for g in 0x1800d8fb0 0x1800a1c60 ...; do
  A=$(grep -E "^0x[0-9a-f]+ $g " /tmp/rmap2.txt | tail -1 | awk '{print $1}'); echo "$g $A"
done
lldb -p $PID --batch \
  -o "disassemble --start-address <host> --count 1400" ... -o detach -o quit > /tmp/host_asm.txt

# 3. the CLIF side of one block (bypasses the dump stride and the 2000-block cap)
WIE_JIT_DUMP_IR=/tmp/clif_memcpy.txt WIE_JIT_DUMP_IR_RIP=0x1800a1c60 \
  ./target/release/wie run ... --gui doomretro.exe -- -iwad ... &
sleep 62; kill -INT $PID

# 4. the guest side (static, from the shipped PEs)
/opt/homebrew/opt/llvm/bin/llvm-objdump --triple=x86_64-pc-windows-msvc -d \
  --start-address=0x1800d8fb0 --stop-address=0x1800d8fc8 real_exes/doomretro/SDL2.dll
```

Emulation facts for the audited run (102.8 s wall, `WIE_RUNTIME_PROFILE=1`):

| metric | value |
| --- | --- |
| frames published | 544 |
| guest instructions | 32.58 G (JIT 99.8 %) |
| guest-thread emu time | 94.04 s (97.9 %), `handler_ms=2014` (2.1 %) |
| guest instruction rate | **346.5 M/s** (single run, loaded host) |
| guest instructions per frame | 59.9 M overall; **37.3 M in the render phase** (12.29 G of it is the one-time tint-table build measured in the previous pass) |

Sampling resolution: 4 914 main-thread samples pooled, 4 258 of them guest-JIT leaves. The 95 %
half-width is ±1.44 pp on the 36 % block and ±0.57 pp on the 3.7 % block, so **ranks 1–7 are real
and ranks 8–10 are noise** (they are three more blocks of the *same* function as rank 6; they are
listed because the request was ten, not because their individual shares mean anything).

---

## 2. The population

`0x1800xxxxx` is `SDL2.dll` (image base `0x180000000`); `0x1400xxxxx` is `doomretro.exe`
(`0x140000000`). **80 % of the render phase is SDL2's software blit path**, not DOOM Retro's own
code — DOOM Retro renders 320×200 into a surface and SDL scales/converts it to the window.

| # | guest block | module | guest insns | share (±95 %) | host insns (block) | host insns (loop body) | host/guest |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | `0x1800d8fb0` | SDL2 | 8 | 35.94 ±1.44 % | 606 | 510 | **63.8** |
| 2 | `0x1800a1c60` | SDL2 | 6 | 16.22 ±1.11 % | 550 | 455 | **75.8** |
| 3 | `0x1800ab2e1` | SDL2 | 33 | 11.88 ±0.97 % | ≥1400 ¹ | ≥1400 ¹ | ≤42.4 ¹ |
| 4 | `0x1800cef0b` | SDL2 | 10 | 11.09 ±0.94 % | 601 | 509 | **50.9** |
| 5 | `0x1800ab355` | SDL2 | 11 | 5.29 ±0.67 % | 570 | 570 | **51.8** |
| 6 | `0x1400f8dd0` | doomretro | 7 | 3.70 ±0.57 % | 565 | 468 | **66.9** |
| 7 | `0x1401550c0` | doomretro | 12 | 0.71 ±0.25 % | ≥1400 ¹ | ≥1400 ¹ | ≤117 ¹ |
| 8 | `0x1400f8de3` | doomretro | 5 | 0.12 ±0.10 % | 341 | 341 | 68.2 |
| 9 | `0x1400f8d9a` | doomretro | 16 | 0.12 ±0.10 % | 611 | 611 | 38.2 |
| 10 | `0x1400f8dc2` | doomretro | 12 | 0.08 ±0.07 % | 500 | 500 | 41.7 |

¹ my disassembly window was 1400 instructions per block; these two blocks are larger than that, so
their host counts and ratios are **lower bounds**. (`0x1401550c0` is the CRT `memset`: 16-byte SSE
stores unrolled ×8, so a 12-instruction guest iteration whose host form is over 1400 instructions in
the block.)

"Loop body" = the largest internal back edge (the guest `jne`), i.e. **the code re-executed once per
guest iteration**. For every one of these blocks the back edge lands *after* the prologue but
*before* the per-access guard sequences, so the body includes the probe and flag work for every
guest instruction — that is what the per-iteration cost is.

### 2.1 What the guest code actually is

```asm
; #1 0x1800d8fb0  SDL2, 36 %  — scaled blit, 16.16 fixed-point source stepping
1800d8fb0: mov %rdx,%rcx ; add %r10,%rdx ; shr $0x10,%rcx
1800d8fba: mov (%r9,%rcx,4),%ecx ; mov %ecx,(%rax)
1800d8fc0: add $0x4,%rax ; sub $0x1,%r8d ; jne 1800d8fb0

; #2 0x1800a1c60  SDL2, 16 %  — SDL_memcpy4, one 32-bit word per iteration
1800a1c60: mov (%rdx),%eax ; add $0x4,%rdx ; mov %eax,(%r9)
1800a1c69: add $0x4,%r9 ; sub $0x1,%r8 ; jne 1800a1c60

; #3/#5 0x1800ab2e1 / 0x1800ab355  SDL2, 17 % combined — Blit1to4: palette byte → ARGB word,
;        Duff's-device unrolled ×4 (two unrolled copies plus a `jmp` back in the tail block)
1800ab2e1: movzbl (%r9),%ecx ; mov (%r10,%rcx,4),%edx ; mov %edx,(%r8)
1800ab2ec: add $0x4,%r8 ; inc %r9        (×4, then jmp 1800ab2e1)

; #4 0x1800cef0b  SDL2, 11 %  — dword fill, unrolled ×4
1800cef0b: mov %r8d,(%rax) ; add $0x4,%rax   (×4) ; sub $0x1,%r11 ; jne 1800cef0b

; #6/#8/#9/#10 0x1400f8d31..0x1400f8df7  doomretro, 4 % combined — R_DrawMaskedColumn:
;        post scan (cmpb $-1), clipping with cmovg, and the byte copy loop
1400f8dd0: movzbl (%r8),%ecx ; lea 0x1(%r8),%r8 ; mov %cl,(%rax)
1400f8dda: dec %edx ; add %r13,%rax ; test %edx,%edx ; jg 1400f8dd0

; #7 0x1401550c0  doomretro, 0.7 %  — CRT memset (SSE2)
1401550c0: movdqa %xmm0,(%rcx) ; ... ×8 ; add $0x80,%rcx ; sub $0x80,%r8
1401550f5: cmp $0x80,%r8 ; jae 1401550c0
```

Every one of these is the worst case for an emulator: a **6–12 instruction loop with 1–2 guest
memory accesses and 2–4 flag-writing ALU ops, executed once per output pixel**. There is no
instruction-level parallelism to hide lowering overhead behind. This is the population that decides
the frame rate, and it is not the population the JIT was tuned on (`7za`'s LZMA inner loops are
memory-heavy and hide the same overhead).

---

## 3. One iteration, annotated

Block #2 (`SDL_memcpy4`, 6 guest instructions, 16 % of the frame), host offsets relative to the
block's host address. The guest instruction is `add $0x4,%rdx` — a 64-bit add that writes six flags:

```
+0x0f4  ldur  x3, [sp, #0x8]      ; guest RDX, reloaded from the host stack
+0x0f8  add   x1, x3, #0x0
+0x0fc  stur  x3, [sp, #0x8]      ; and stored straight back
+0x100  add   x4, x1, #0x4        ; ← the guest's add
+0x104  cmp   x4, x1              ; re-derive CF, because `add` does not set flags
+0x108  cset  x2, hs              ; CF as a 0/1 value
+0x10c  ldur  x0, [sp, #0x98]     ; guard predicate 1 (spilled bound)
+0x110  cmp   x1, x0
+0x114  cset  x3, hs
+0x118  ldur  x5, [sp, #0x90]     ; guard predicate 2
+0x11c  cmp   x4, x5
+0x120  cset  x4, ls
+0x124  ldur  x5, [sp, #0x80]     ; guard predicate 3
+0x128  and   x6, x5, #0x1
+0x12c  cmp   x6, #0x0
+0x130  cset  x5, ne
+0x134  ldur  x6, [sp, #0x78]     ; guard predicate 4
+0x138  and   w2, w6, w2          ; ┐
+0x13c  and   w2, w2, w3          │ combine 4 predicates into one byte
+0x140  and   w2, w2, w4          │
+0x144  and   w3, w2, w5          ┘
+0x148  sub   x2, x1, x0
+0x14c  tst   w3, #0xff           ; are all four true?
+0x150  b.ne  slow_path
+0x154  and   x0, x1, #0xfff      ; page offset               ┐
+0x158  lsr   x3, x1, #12         ; page key                  │
+0x15c  add   x2, x0, #0x4        ; page-cross check          │ refill probe
+0x160  cmp   x2, #0x1000         ;                          │
+0x164  cset  x2, ls              ;                          │
+0x168  mov   x4, #0x7c15         ; GOLDEN constant,         │
+0x16c  movk  x4, #0x7f4a, lsl #16;  4 instructions         │
+0x170  movk  x4, #0x79b9, lsl #32;  per probe              │
+0x174  movk  x4, #0x9e37, lsl #48;                          │
+0x178  mul   x4, x3, x4          ;                          │
+0x17c  lsr   x4, x4, #51         ;                          │
+0x180  and   x6, x4, #0x1fe0     ;                          │
+0x184  ldr   x5, [x24, x6]       ; slot.tag                 │
+0x188  add   x4, x24, #0x8       ;                          │
+0x18c  ldr   x4, [x4, x6]        ; slot.base                │
+0x190  add   x7, x24, #0x10      ;                          │
+0x194  ldr   x6, [x7, x6]        ; slot.prot                │
+0x198  ldur  x7, [sp, #0x70]     ;                          │
+0x19c  eor   x3, x3, x7          ;                          │
+0x1a0  cmp   x5, x3              ;                          │
+0x1a4  cset  x3, eq              ;                          │
+0x1a8  and   x5, x6, #0x1        ;                          │
+0x1ac  cmp   x5, #0x0            ;                          │
+0x1b0  cset  x5, ne              ;                          │
+0x1b4  and   w3, w3, w5          ;                          │
+0x1b8  and   w2, w3, w2          ;                          ┘
+0x1bc  tst   w2, #0xff
+0x1c0  b.ne  slow_path
```

Fifty-two host instructions for **one** guest `add`, whose own work is one instruction. The same
shape — 20–25 instructions of predicate materialisation, then a probe — repeats for every guest
memory access and every guest flag write. The block CLIF is **505 ops for 6 guest instructions
(84 ops/guest)**, 31 CLIF blocks, 17 `brif`, 2 helper calls; the aarch64 is 550 instructions, i.e.
Cranelift folded the constants and offsets as well as anyone could — the volume is *ours*, upstream
of the backend.

---

## 4. Census per block

Host instructions in the loop body, by mnemonic (top entries; `[sp]` numbers are the spilled-value
traffic, see §5.4):

| block | body | spill ¹ | `str` | `ldr` | `cmp` | `and` | `cset`/`csel` | `mov`/`movk` | `orr` (pack) | `b*` | `add`/`sub` |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `0x1800d8fb0` | 510 | 101 | 46 | 37 | 43 | 47 | 40 | 67 | 12 | 34 | 27 |
| `0x1800a1c60` | 455 | 108 | 46 | 37 | 36 | 33 | 33 | 51 | 6 | 34 | 27 |
| `0x1800ab2e1` | ≥1400 | 110 | 31 | 125 | 140 | 198 | 138 | 214 | 7 | 112 | 132 |
| `0x1800cef0b` | 509 | 106 | 51 | 31 | 38 | 47 | 35 | 71 | 6 | 30 | 41 |
| `0x1800ab355` | 570 | 55 | 25 | 56 | 52 | 71 | 50 | 87 | 6 | 39 | 53 |
| `0x1400f8dd0` | 468 | 104 | 44 | 35 | 39 | 39 | 37 | 57 | 6 | 34 | 27 |
| `0x1401550c0` | ≥1400 | 215 | 43 | 105 | 121 | 168 | 121 | 257 | 6 | 105 | 101 |

¹ `ldur`/`ldp`/`stur`/`stp` against `[sp, …]` — the host-stack spill traffic of §5.4.
`str` here is mostly the **JitCtx writeback** (`str x0,[x27,#0xd68]`) and the second half of split
16-byte stores, not guest stores; `ldr [xN,#imm]` is the guest-state load and the refill-table slot
read. The guest's own arithmetic accounts for ≤2 instructions per guest instruction (`add`/`sub`),
which is the scale the rest of the table should be read against.

---

## 5. The miss catalogue

Each entry: what the guest asked for, what we emit, what a good JIT emits, how many occurrences in
the 12 audited loop bodies, and how many of the top-10 blocks are affected.

Two different percentages, kept apart because conflating them inflates the case:

* **class size** — the weighted share of render-phase guest time the class's instructions occupy,
  `Σ_block share × (class instructions / loop-body instructions)`;
* **removable** — the part of that which a correct lowering of the same semantics actually deletes,
  i.e. class size minus the instructions the better form still has to emit (one `cmp` and one branch
  per predicate instead of `cmp`+`cset`+`and`, etc.).

Both assume uniform IPC inside a body. That assumption is the weakest link in the ranking and is
stated so it can be argued with.

### 5.1 Predicate conjunctions materialised into a byte — size 11.0 %, removable 5.9 %, 12/12 blocks

177 conjunctions, 230 predicates, 1 036 host instructions per frame-equivalent. This is the biggest
single class and it is *ours*: `lower/mem.rs:389 refill_tlb_probe` returns
`band(band(key_ok, prot_ok), in_page)` — independent i1s ANDed — and `emit_probe_stage` then tests
the aggregate. Cranelift materialises each i1 as a 0/1 register (`cset`), ANDs them as 32-bit
integers, and tests the aggregate (`tst wN,#0xff` + `b.ne`).

The refill stage of block #2 (`movl (%rdx),%eax`, one guest load) — the `ldr`/`and`/`lsr` that
compute the slot index are necessary, the three predicates are not:

```
+0x154  and   x0, x1, #0xfff     ; page_off
+0x158  lsr   x3, x1, #12        ; page_key
+0x15c  add   x2, x0, #0x4       ; end = page_off + 4
+0x160  cmp   x2, #0x1000        ; ┐ predicate 1: does not cross a page
+0x164  cset  x2, ls             ; ┘
+0x168  mov   x4, #0x7c15        ; (see §5.5)
...
+0x184  ldr   x5, [x24, x6]      ; slot.tag
+0x188  add   x4, x24, #0x8
+0x18c  ldr   x4, [x4, x6]       ; slot.base
+0x190  add   x7, x24, #0x10
+0x194  ldr   x6, [x7, x6]       ; slot.prot
+0x198  ldur  x7, [sp, #0x70]    ; spilled mem_gen
+0x19c  eor   x3, x3, x7
+0x1a0  cmp   x5, x3             ; ┐ predicate 2: tag == key ^ gen
+0x1a4  cset  x3, eq             ; ┘
+0x1a8  and   x5, x6, #0x1       ; ┐ predicate 3: prot has R
+0x1ac  cmp   x5, #0x0           ; │
+0x1b0  cset  x5, ne             ; ┘
+0x1b4  and   w3, w3, w5         ; combine
+0x1b8  and   w2, w3, w2
+0x1bc  tst   w2, #0xff          ; all three?
+0x1c0  b.ne  slow_path
```

The stack-pin stage earlier in the same block (`+0x0f8..+0x150`, quoted in §3) has **four**
predicates and costs 22 instructions including four spill reloads. Per condition, what it should be:

```asm
cmp   x2, #0x1000            ; page fits
b.hi  slow
cmp   x5, x3                 ; tag matches
b.ne  slow
and   x5, x6, #0x1           ; (prot test needs one registration only)
cbz   x5, slow               ; → 7 instructions instead of 19, no 0/1 registers, no and-tree
```

6 instructions instead of 12–14 for the conjunction of each stage ⇒ **~6 instructions saved per
conjunction**, 177 conjunctions ⇒ ~1 060 instructions per frame-equivalent — the 5.9 % in the
heading. (`ccmp` chains it further: `cmp x2,#0x1000; ccmp x5,x3,#4,ls; ccmp x5,x7,#4,ne; b.ne slow`
— four instructions, one branch, no materialisation at all.)

*Blocks touched: all 12. Site: `lower/mem.rs` (`refill_tlb_probe`, `hoisted_pin_probe`,
`emit_probe_stage`).*

### 5.2 Guest flag into a GPR: `cmp` + `cset` where `adds`/`subs` would do — size 10.7 %, removable 5.3 %, 12/12

545 occurrences of `cmp` immediately followed by `cset`. The guest `add $0x4,%rdx` writes six flags;
we compute the result with a non-flag-setting `add`, then *re-derive* the carry with `cmp` and
materialise it with `cset` (§3, `+0x100..+0x108`). aarch64 `adds x4, x1, #4` sets N/Z/C/V in one
instruction; the 64-bit add's carry is exactly `C`.

```asm
add   x4, x1, #0x4      ; we emit this
cmp   x4, x1            ; plus this
cset  x2, hs            ; and this  → 3 instructions
adds  x4, x1, #0x4      ; a good JIT: 1 (CF is native)
cset  x2, hs            ; the value, only if something reads it as a value
```

Second form in the same class: `cmp x24,#0x0` + `cset x4,ne` = 2 instructions for what `tst`/`subs`
already computed, and 3 when wrapped by `bool_to_i64` (`gpr.rs:41` → `select(b,1,0)` → `mov #1; mov
#0; csel`, where `cset` alone is 1).

*Blocks touched: all 12. Sites: `lower/flags.rs` (predicates), `lower/gpr.rs:41 bool_to_i64`,
`lower/gpr.rs:47 flag_set`, `lower/insn.rs` (ALU result path).*

**Correction, 2026-11-24 (after `90779b0`).** Two things in the section above are wrong, and they
were wrong in the direction that inflated this item's value:

1. The `cmp`+`cset` pair cited at `+0x100..+0x108` of `0x1800a1c60` is **not** a re-derived carry for
   `add $0x4,%rdx` — it is the memory probe's `no_wrap` predicate. The class is real (it now takes
   CF/OF from `adds`/`subs`), but that block's flag cost is one burst plus one pack per iteration,
   ~59 of ~550 host instructions (**11 %**), not the headline figure.
2. "38 %" was a count of every mnemonic that *could* belong to flag construction; the attributed
   number is ~21 % of the body, by two independent methods (CLIF backward slice from the store into
   `JitCtx+128` = 117/560; attributed machine code = 124/573) — see
   `docs/architecture/xmm-pair-ceiling.md`. ~21 % is the figure to quote for EFLAGS construction;
   the wide census number is a different counting rule, not a conflicting measurement.

### 5.3 The EFLAGS word is rebuilt bit by bit — size 5.5 %, removable 2.7 % (more if packing moves fully to block boundaries), 12/12

101 flag-bit inserts (17 completed packs) in the 12 bodies. One pack, CLIF and machine:

```
v294 = iconst.i64 -2262            ; clear mask
v295 = band.i64 v115, v294
v296 = iconst 1 ; v297 = iconst 0
v298 = select v259, v296, v297     ; CF
v299 = bor v295, v298
v300 = 64 ; v301 = 0
v302 = select v274, v300, v301     ; ZF   … ×6 flags
```
```asm
mov  x21, #0x1 ; mov x22, #0x0 ; cmp x3, x4 ; csel x21, x21, x22, lo ; orr x13, x13, x21   ; CF
mov  x14, #0x40 ; mov x21, #0x0 ; cmp ...  ; csel ...              ; orr ...              ; ZF
...                                                                                       ; ×6
```

24 instructions per pack; `select_flag` (`flags.rs:164`) + `FlagState::pack` (`flags.rs:455`) are
the sites. A JIT that keeps the six predicates and packs only at the architectural boundary
(`pushfq`, a helper call, a block exit into a non-SSA consumer) pays this once per *block*, not once
per flag-writing instruction. *Blocks touched: 12/12.*

### 5.4 The spill storm — size 15.7 %, 12/12 — a *consequence*, not a fix

1 074 host-stack accesses (`ldur`/`ldp`/`stur`/`stp` against `[sp, …]`) across the 12 bodies; 55–215
per body, and 20–24 % of the instructions of every body I could measure in full. The same slot is frequently reloaded twice within a
few instructions: block #1 reloads `[sp, #0x90]` at `+0x314` and again at `+0x350`.

Cause: register pressure. Every probe conjunction (5.1) and every flag value (5.2/5.3) is a live
GPR-valued temporary, so Cranelift's register allocator — with no scheduler and no rematerialisation
at `opt_level=none` — parks ~20 guest values on the host stack for the whole loop body. This is why
the class is listed with its size but **not** offered as an independent fix: implementing 5.1–5.3
removes most of the live temps and should collapse this mechanically. Treat 15.7 % as an upper
bound on what that buys, not as "108 instructions to delete".

### 5.5 The refill hash constant is rematerialised per probe — size 2.1 %, removable 1.6 %, 10/12

```
+0x374  mov   x10, #0x7c15            ; ┐
+0x378  movk  x10, #0x7f4a, lsl #16   │ 4 instructions for GOLDEN_RATIO_64
+0x37c  movk  x10, #0x79b9, lsl #32   │
+0x380  movk  x10, #0x9e37, lsl #48   ┘
+0x384  mul   x10, x9, x10
+0x388  lsr   x10, x10, #51
+0x38c  and   x12, x10, #0x1fe0
```

8 instructions of slot index per probe, 4 of them the constant. Site:
`lower/mem.rs:424 imul_imm(page_key, GOLDEN_RATIO_64)`. `mul` has no immediate form on aarch64, so
the constant must be in a register — but it is *block-invariant* and `mem.rs:342 hoist_refill`
already hoists `mem_gen` and the table base for exactly this reason. Hoisting the multiplier the
same way saves 3 per probe (54 probes in the audited bodies ⇒ ~160 instructions).
*Blocks touched: 10/12.*

### 5.6 Redundant boolean re-normalisation — size 1.0 %, removable 1.0 %, 8/12

```
and  x6, x5, #0x1     ; already 0/1
cmp  x6, #0x0         ; provably redundant
cset x5, ne           ; provably redundant
```
44 occurrences. `flag_set` (`gpr.rs:47`) extracts bit 0 with `band`, which already yields 0/1, and
then `bool_to_i64` (`gpr.rs:41`) converts it back. 2 wasted instructions each.
*Blocks touched: 8/12.*

### 5.7 `and #0xffffffff` on a value that is already zero-extended — size 0.44 %, removable ≤0.44 %, 7/12

```
and  x1, x25, #0xffffffff
```
66 occurrences. Two sources, and they need different treatment: `flags.rs:123 mask_width(…, 32)`
(a deliberate truncation to the guest's operand width) and `gpr.rs:441 extend_value(src_bits=32)`,
where the value came from a 32-bit load or a 32-bit ALU op and is therefore already zero-extended.
Only the second is provably dead; the counts above are not split, so treat this class as needing a
def-use check before the fix is claimed.

### 5.8 Extending load followed by a redundant mask — provable no-op, 0.08 %, 6/12

```
ldrb w0, [x24, x0]    ; zero-extends into x0 by construction
and  x0, x0, #0xff    ; exact identity
```
12 occurrences. `mem.rs:440 load_guest_bytes` already emits `load.i8` + `uextend` (→ `ldrb`), and
`gpr.rs:437 lower_movx` then calls `extend_value(8)`, which emits `band(src, 0xff)`
(`gpr.rs:451-453`). The mask is dead whenever the source is a fresh load of exactly `src_bits` — a
one-line guard at `gpr.rs:450`. Small, but it is a *proven* no-op and costs nothing to remove.

### 5.9 SSE stores: a 16-byte guest store becomes two probes and two 8-byte stores — 0.7 % (rank 7)

`movdqa %xmm0,(%rcx)` (CRT `memset`) is lowered by `sse.rs:105 store_sse_mem` → `mem.rs call_store`
**twice**, 8 bytes each:

```
+0x01e0  mov   x2, #0x8               ; size = 8
+0x01e4  mov/movk/movk x4, …          ; helper + ip for the slow path
+0x020c  blr   x5
+0x0238  str   x1, [x3, x0]           ; lo half
+0x0248  str   x3, [x1, x0]           ; hi half, second probe
```

Two refill probes, two guard conjunctions and two `str x` per guest store, where aarch64 has `str q`
for exactly this and WIE's own frame store (`analysis.rs store_xmm_packed`) already emits `str q21,
[x20, #0x9c0]`. This block is only 0.7 % of *this* frame, but the class applies to every SSE
`memcpy`/`memset` the guest runs — hoisting the split out of `store_sse_mem` for the page-resident
case is a general win, and the pattern to copy is in `analysis.rs`.

---

## 6. Negative results (each with the disassembly behind it)

### 6.1 Frame-offset addressing is already optimal

Re-verified across this population: every JitCtx access is a displacement-form access and **no**
frame address is materialised with `add`. Examples from the audited blocks: `ldr x25,[x0]`,
`ldr x28,[x0,#0xb08]`, `ldr x2,[x0,#0xaf8]`, `str x0,[x27,#0xd68]`, `ldr q0,[x0,#0x9c0]`. The only
`add`-then-load pairs are the refill table's `[base + reg + 8]` / `[base + reg + 16]` accesses
(`add x4,x24,#0x8 ; ldr x4,[x4,x6]`), which aarch64 cannot encode in one instruction. **Do not
spend another day on the `iadd_imm` + `load(…, 0)` shape: the backend folds it, and rewriting it in
`analysis.rs` changes the CLIF count and zero machine instructions.**

### 6.2 `tst wN, #0xff` + `b.cond` — already 2 instructions, and `tbz` cannot help

177 occurrences, all with an **8-bit** mask (the conjunction byte of §5.1). `tst` is the `ands`
alias, so the pair is already minimal; `tbz`/`tbnz` test *one* bit and do not apply. Single-bit
`tst` + branch (where `tbz` would save one instruction): **0 occurrences** in the 12 bodies.

### 6.3 The other instruction-selection classes are simply absent

Counted across the 12 loop bodies and both modules:

| class | occurrences |
| --- | --- |
| `cmp x, #0` + `b.cond` where `cbz`/`cbnz` would save an instruction | **0** |
| `lsr` + `and` where `ubfx` would save an instruction | **0** |
| `mul` + `add` where `madd` would save an instruction | **0** (the only multiplies are the probe hash and the palette-block guest `imul`s) |
| `tst` single-bit + branch → `tbz` | **0** |
| narrow load that is not already an extending load (`ldrb`/`ldrh`/`ldrsw`) | **0** — the loads are already extending; only the redundant mask of §5.8 remains |
| byte swap (`rev`) | **0** |

`csel`/`cset` where a branch is emitted instead: not found in this population. The emitter already
uses `csel` (`csel x21, x8, x9, eq` in the palette block the previous pass audited, `csel` in the
column-draw clipping). The waste here is the *opposite* direction — values materialised where a
branch or a condition code would do.

---

## 7. Ranked fix list

Sorted by `(removable frame-%) × (blocks of the top ten touched)`. "class" is the §5 *size* (how much
of the frame the class's instructions occupy), "removable" is what the better form actually deletes,
and "saved/occ" is read off the disassembly - not estimated.

| # | fix | site | class | removable | blocks | product | saved/occ | dependency / risk |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| **1** | **Probe predicates: one `brif` (or `ccmp` chain) per condition instead of `cmp`+`cset`+`and`+`tst`** | `mem.rs:389 refill_tlb_probe`, `hoisted_pin_probe`, `emit_probe_stage` | 11.0 | **5.9** | **12** | **71** | ~7 insn × 177 | none semantically: the conditions are already independent i1s and the fast path is the all-true arm. Costs CLIF blocks (17 `brif` → ~26). |
| **2** | **Flags: `adds`/`subs`/`ands` instead of result-then-`cmp`-then-`cset`; keep the predicates, don't hand them to GPRs** | `flags.rs` (predicates), `gpr.rs:41`, `gpr.rs:47`, `insn.rs` ALU | 10.7 | 5.3 | 12 | 64 | 1–2 insn × 545 | flags_ssa's file. Interacts with #3 (same redesign). |
| 3 | EFLAGS pack only at the architectural boundary | `flags.rs:164 select_flag`, `flags.rs:455 pack` | 5.5 | 2.7 | 12 | 33 | 4 insn × 101 bits (half retained conservatively) | flags_ssa's file; must keep `pushfq`/helper-visible rflags exact. |
| 4 | Hoist the probe's golden-ratio multiplier per block (as `hoist_refill` already does for `mem_gen`/table base) | `mem.rs:424` | 2.1 | 1.6 | 10 | 16 | 3 insn × 54 | none — block-invariant constant. |
| 5 | Drop the `cmp #0`+`cset ne` re-normalisation after `band #0x1` | `gpr.rs:47` + `gpr.rs:41` | 1.0 | 1.0 | 8 | 8 | 2 insn × 44 | needs `flag_set`'s result to be consumed as a 0/1 value (it is). |
| 6 | `and #0xffffffff` only where the value is not already zero-extended | `gpr.rs:441`, `flags.rs:123` | 0.44 | ≤0.44 | 7 | 3 | 1 insn × 66 | needs a def-use split first (two sources, §5.7). |
| 7 | Skip `band` after an extending load of the same width | `gpr.rs:450-453` | 0.08 | 0.08 | 6 | 0.5 | 1 insn × 12 | none — provable no-op. |
| — | *(spill storm)* | consequence of 1–3 | 15.7 | — | 12 | — | — | **not an independent fix**; re-measure after 1–3. |
| — | *(SSE 16-byte store split)* | `sse.rs:105` | 0.7 here | — | 1 | — | 2 probes → 1, 2 `str x` → 1 `str q` | different population: matters for any SSE `memcpy`/`memset`, not for these ten blocks. |

**Why #1 and not #2, or the spill storm.** #1 has the largest product, it needs no semantic argument
(the probe already has a fall-through-to-helper structure and its conditions are independent
predicates, so a per-condition branch is the same decision tree with fewer registrations), and it is
in a file nobody has in flight this round, so it can land without waiting on `flags_ssa`. It also
attacks the spill storm indirectly: four live 0/1 registers per probe are exactly what pushes guest
values onto the host stack. #2 is nearly as large and is a deeper fix, but it is the *same*
redesign as #3 and belongs to `flags.rs`; if `flags_ssa` lands "keep NZCV, pack at exits" then #2
and #3 collapse into that one change and the spill figure falls with them.

**Combined ceiling:** 5.9 + 5.3 + 2.7 + 1.6 + 1.0 + 0.44 + 0.08 ≈ **17 % of render-phase guest
time removable without touching semantics** — about 1.2x on the frame count if nothing else moves —
on top of which the spill layer (15.7 % of the frame in instructions) should shrink by however much
of it items 1–3 were creating. Those two figures are **not additive**: the spill instructions are
largely *caused* by the live-value pressure of items 1–3, so counting both would double-count the
same machine instructions. Re-measure, do not add.

## 8. Reproduction

```bash
cargo build -p wie-cli --release           # release matters: sample addresses, not just speed

# population
WIE_RUNTIME_PROFILE=1 WIE_JIT_MAP=/tmp/rmap.txt \
  ./target/release/wie run --bottle doomretro --gui doomretro.exe \
  -- -iwad 'C:\Program Files\DoomRetro\freedoom2.wad' &
sleep 60; sample $! 2 -file /tmp/s60.txt; sleep 20; sample $! 2 -file /tmp/s80.txt
kill -INT %1     # the profile block prints frames_published and the guest instruction total

# one block's machine code, from the live process (addresses are per-run: ASLR on the JIT arena)
A=$(grep -E "^0x[0-9a-f]+ 0x1800a1c60 " /tmp/rmap.txt | tail -1 | awk '{print $1}')
lldb -p $PID --batch -o "disassemble --start-address $A --count 1400" -o detach -o quit

# one block's CLIF (bypasses stride and cap: a named range is exact)
WIE_JIT_DUMP_IR=/tmp/ir.txt WIE_JIT_DUMP_IR_RIP=0x1800a1c60 ./target/release/wie run ... &
```

Caveats, stated once:

- **Single run, loaded host.** The machine carried load average 10.2 (two concurrent agent builds)
  for the whole session. Shares are fine (they are ratios of samples inside one run); the 346.5 M
  guest-insn/s rate and any absolute time are not comparable across sessions.
- **Binary state.** The audited `target/release/wie` (18:16) was built from the working tree, which
  contained `flags_ssa`'s uncommitted `lower/` edits. Instruction offsets will shift when that
  lands; the classes and counts are structural and should be re-counted, not assumed.
- **`sample` cannot trace one iteration.** The per-iteration cost above comes from the emitted
  code's own loop structure (the internal back edge), not from per-instruction tracing; the sampled
  offset histogram of each block agrees with that loop region (e.g. block #2's hot offsets
  `+0x0f8..+0x7c4` sit inside the `+0x0f8..+0x810` body).
- **Two blocks exceed the 1400-instruction dump window** (ranks 3 and 7), so their host counts and
  host/guest ratios are lower bounds.
- The uniform-IPC assumption in every "frame-%" figure is stated in §5. It is what makes the ranking
  arithmetic rather than intuition, and it is also its main weakness: a class concentrated in the
  instruction-cache-hostile part of a body will be over-rated.
