# The inline TLB probe is the wrong data structure

Measured 2026-11-24 on DOOM Retro (release JIT, `WIE_JIT_MEM` default), 70 s run,
`WIE_JIT_MEM_TRACE=1`:

```
mem_path helpers=83586465 load=62874822 store=20711643
  resolve: sticky=13667 multi=83487360 pin=55260 walk=27158 cross=3020 slow=0
  sticky_miss: key=83569767 gen=0 prot=11 swaps=83569746
```

Read that second line carefully: **the two-way sticky probe hit 0.016 % of the
time.** 83.5 M of 83.6 M helper calls fell through the sticky cascade into the
4-way set-associative bucket (`multi`), which resolved them. The bucket is doing
all the work; the structure the emitted probe actually consults is useless.

## Why

`STICKY_WAYS = 2` (`lower/mod.rs:105`), MRU-ordered, and it is *replaced* on
every multi-hit (`tlb_set_hot`). A guest with a working set above two pages —
DOOM Retro's renderer touches a frame buffer, a colormap, texture columns, the
stack and the heap inside one block — thrashes it every access. `swaps=83.6 M`
against `sticky=13 667` hits is thrash, not a tuning miss.

So the emitted fast path is: cascade two dead ways (≈ 20 CLIF ops, four loads,
three `band`s, a `brif`), fail, call `wie_jit_load` (a Rust call), which runs
`classify_addr_vs_pins` (a loop over every pin, *diagnostic only*), re-checks the
sticky ways it just failed (another 4-way scan with `saturating_add` counters on
every branch), rebuilds the MRU list, *then* finds the page in the bucket.

## What it should be

A **direct-mapped refill table in the `JitCtx`**, sized to the working set
(256 entries = 4 KB of tags + 4 KB of host bases), indexed by hashing the page
key:

```
slot  = (page_key * 0x9E3779B97F4A7C15) >> (64 - 8)      // or page_key & 255
tag   = slot.tag ^ mem_gen                                 // one compare covers
hit   = tag == (page_key ^ mem_gen)                        // key *and* generation
prot  = slot.prot & need                                   // R/W permission
ok    = hit & prot & !cross_page
host  = slot.base + (addr & 0xFFF)
```

That is 2 loads + 2 `icmp`/`band` + 1 `brif` on a hit — fewer ops than the
current two-way cascade *and* a hit rate within a few percent of the bucket.
The permission word and the generation fold into the tag so a stale or
denied page costs no extra compare.

Keep the cascade as the fallback for the rare conflict, exactly as today.

## Predicted effect, and how to check it

The helper call disappears for ~99 % of accesses instead of 0.02 %, which
removes: one host call per access, `classify_addr_vs_pins`, the second sticky
scan, the MRU rebuild, and every `mem_path` counter bump.

Check in this order, and stop at the first one that does not move:

1. `WIE_JIT_MEM_TRACE=1` histogram: `sticky` should become the dominant bucket
   and `helpers` should collapse by ~2 orders of magnitude.
2. Guest-thread `sample` profile: `jit::lower::mem::*` + `mem::pagemap` out of
   the top entries.
3. DOOM Retro frames, and `insn_coverage: total` per wall second.

## Non-goals

- Do not touch fault semantics: a cross-page or denied access must still take
  the helper and still report through `set_fault`.
- Do not remove `mem_gen` from the tag: it is what makes `VirtualProtect`,
  `VirtualFree` and remaps invalidate the inline path.
- The diagnostic counters stay, but behind one branch: they exist for
  `WIE_JIT_MEM_TRACE` and nothing else reads them.

## Shipped

Done, in the shape above: 256 slots × 32 B in `JitCtx::refill`, tag folded with
`mem_gen`, address `page_key ^ mem_gen`, index from the top bits of a
golden-ratio multiply of the page key, populated from every resolution point
(bucket hit, pin hit, walk hit, sticky-victim hit) and cleared on a generation
change next to the pin rebuild.

Counted from `WIE_JIT_DUMP_IR` at `opt_level = none`, per memory operand:

| | CLIF instructions | slot loads | `icmp` | `band` | branches | `iconst` |
|---|---|---|---|---|---|---|
| 2-way sticky cascade | 71 | 9 | 9 | 11 | 7 | 22 |
| direct-mapped refill | **29** | 3 | 3 | 5 | 2 | 9 |

and `ctx+OFF_REFILL` / `ctx+OFF_MEM_GEN` are hoisted once per block instead of
being re-derived per access. `tlb_page_ptr` was split so `classify_addr_vs_pins`
and `classify_sticky_miss` (diagnostics) run only when `mem_path_on` is set.

The 4-way bucket, `pin_resolve`, the fault paths and the host-side sticky ways
are unchanged: the sticky set is now the MRU victim list the helper consults
*after* an inline miss, and `WIE_JIT_MEM=slow` still bypasses the whole thing
for bisection.
