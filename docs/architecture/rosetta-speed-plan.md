# Rosetta speed: the measured gap

Date: 2026-11-27. Branch `feat/dll-coverage` with its uncommitted work. Apple M3,
Low Power Mode off, on battery, load average ~3.

Goal (owner): at least match Rosetta 2 in all aspects, before DOOM is worth playing.

## 1. Baseline

Same x86-64 kernel machine code on both arms: mingw compiles `kernels.c` to
assembly once; it is linked into a PE for WIE and assembled for macOS x86-64 for
Rosetta. Reproduce with `./scripts/rosetta-compare.sh [rounds]`
(sources in `micro-exes/rosetta_bench/`).

Medians of 5 interleaved rounds, ms, second (warm) pass. `WIE_JIT_CACHE=0`.
Results (return values) were identical on both arms.

| kernel | shape | Rosetta | WIE | WIE / Rosetta |
| --- | --- | --- | --- | --- |
| `fib` | direct `call`/`ret` | 40.9 | 1182 | **29×** |
| `bytes` | byte load, table lookup, store | 27.4 | 518 | **19×** |
| `indirect` | call through a function-pointer table | 65.5 | 1031 | **16×** |
| `sib` | scale-8 loads/stores, cache resident (SIB-addressing shape) | 12.5 | 137 | **11×** |
| `branch` | data-dependent branches | 164 | 1249 | **7.6×** |
| `fp` | scalar double arithmetic | 52.6 | 334 | **6.3×** |
| `alu` | `volatile` stack ALU loop | 44.5 | 97 | **2.2×** (noisy: one 3-round rerun had a 37.8 ms minimum) |
| `chase` | pointer chase over 32 MiB | 164–192 | 165–172 | **≈1×** (host-timed, see §4) |

Limits: eight synthetic kernels, one compiler, one host. They are not weighted by
how much of a real app each shape is.

## 2. What it changes

- **Hot self-loops are not at Rosetta parity.** `sib` is 11× slower.
  An earlier plan inferred parity from an absolute ns/insn figure that was never
  compared with a translated or native run.
- **Memory-latency-bound code is at parity** (`chase`). The gap is in emitted code
  and control transfer, not in guest cache misses.
- Two families dominate: control transfer (`fib`, `indirect`, `branch`) and memory
  access (`bytes`, `sib`). `fp` is a third.
- `alu` is probably bounded by store-to-load forwarding on both arms
  (not verified), so do not read 2.2× as the floor.

## 3. Targets and next step

Target per row: WIE / Rosetta ≤ 1.0. Gate every rung on its own row here, and on
no regression in the others. DOOM (`-iwad freedoom2.wad -warp 01`) is the secondary gate.

No rung is chosen yet. First attribute each gap:

1. Give the benchmark a kernel selector, so one process measures one kernel.
2. Per kernel, read `WIE_RUNTIME_PROFILE=1` counters: block entries, `cache_hits`,
   chain hops, `load=`/`store=` helper calls, `iced`.
3. Ablate: `WIE_JIT_CHAIN=0` (control transfer), `WIE_JIT_MEM=slow` (memory).
4. Dump the IR of the `k_sib` and `k_fib` blocks; count host instructions per
   memory operand and per `ret`.

Known lead: there is no return-address prediction. The shadow return stack was
removed in `f1920dc` because nothing read it; `ret` goes through a chain-table
lookup. (`jit/mod.rs` module docs still describe the stack.)

## 4. Open items

- **Guest `QueryPerformanceCounter` can stand still.** It read 0.00 ms across
  `chase` in both passes while the host measured ~170 ms, also with
  `WIE_CLOCK_MEMO_US=0`. QPC is an in-guest stub that copies a table slot
  (`guest_stubs/encode.rs`); a slot that is not refreshed during a long run
  without host stops would explain it, **unverified**. Other rows agreed with host
  timestamps to within 5 %.
- **`printf` gaps:** the default mingw `printf` printed nothing for `%.2f` and
  `lu` for `%llu`; with `__USE_MINGW_ANSI_STDIO=1` the guest stops on the
  unsupported `_lock_file`.
- **Owner decision:** every WIE memory access is probed because of the
  soft-translate invariant (`CLAUDE.md`). Rosetta does no such probe. Whether the
  memory rows can reach 1× under that invariant is unproven.
- Compile cost is not covered here (about 39 s of compile time in a 75 s DOOM run,
  on background threads).

## 5. Changes made, 2026-11-27 (uncommitted)

| change | measured effect |
| --- | --- |
| Skip the stack probe stage after a stack home miss (`jit/lower/mem.rs`) | global/heap loads and stores 1.36 → 0.91 ns |
| Loop regions on by default (`WIE_JIT_REGION=0` disables); rejection log at `DEBUG` | `branch` 1213 → 395 ms; `7za t`/`x` about 2× faster |
| Region limits 8 → 32 blocks, 192 → 512 instructions | 16-block loops: `jmp` 12.6 → 0.15 ns, `jcc` 10.3 → 0.09 ns; `7za a`/`t`/`x` unchanged |
| `MAX_CHAIN_DEPTH` 48 → 32 | `fib` −35 %, `indirect` −43 % |
| Chain-hop counters emitted only while profiling | 2–3 % on `fib`, `indirect` |

DOOM `-warp 01` to frame 900, measured with only the first two changes: median
75.0 → 65.4 s, `compile_us` 39 → 6 s.

**Trade-off: short programs are slower.** `emu_ms`, median of 5, old → new:
`cpp_dtor` 6.3 → 9.8, `cpp_throw` 3.2 → 7.8, `crt_hello` 1.1 → 1.6, while `7za i`
goes 195 → 49. With `WIE_JIT_REGION=0` the three short programs are back at the
old figures. A higher visit threshold for regions is the untried remedy.

Fault state inside a region was checked once by hand: a store faulting in a
3-block folded loop gave a fault report identical to `WIE_CPU=iced` in all 72
fields. No test pins this. There is no buildable SEH access-violation guest.

Unit tests keep regions off (`cfg(test)`), so they do not cover the shipping shape.

Checked and not changed: return-address signing (no measurable cost).

Unresolved: `push_pop` reads 0.40–0.50 ns on both the old and new binary and is
too noisy to call. `sum.py`/`run.py` compare timings; they are not a
correctness oracle for the per-instruction cases.

## 6. Per-instruction costs

`micro-exes/instr_bench/run.py [names]` (about 20 s for all; `--ab OLD NEW` for
two WIE builds). One loop per instruction form, 16 copies, empty loop
subtracted. Largest gaps left against Rosetta:

| form | WIE ns | about Rosetta × |
| --- | --- | --- |
| `call`/`ret`, indirect `call` | 12 | 20–45× (block chaining) |
| 128-bit `movdqu` | 2.3 | 20–33× |
| global/heap load, store, add-to-memory | 0.9–1.9 | 7–14× (stack: 1×) |
| `cmp` + `set`/`cmov` | 1.2 | 5–10× |
| `addsd`, `mulsd`, `paddd` | 3.7–4.2 | 4–9× |

## 7. Other gaps found

Guest CRT lacks `strcspn` and `_lock_file`. Six micro guests (`read_file`,
`modules`, `vfs_roundtrip`, `msvcrt_gaps`, `child_proc`, `spawn_child`) fail in
`run-micro-suite.sh exe <name>` with the same exit codes under `WIE_CPU=iced`, so
the JIT is not the cause. Cause not investigated.
`check.sh --only file-sizes` already failed at `HEAD`.

## 8. Second pass, 2026-11-27 (uncommitted)

### Correctness bugs found by the new checksums

All were wrong identically under the JIT and `WIE_CPU=iced`, so no
differential test could see them. `micro-exes/instr_bench/sse_check.py` compares
157 vector cases with Rosetta; all now match.

- `packsswb`, `packssdw`, `packuswb` interleaved their inputs.
- Shifts by a register count used a different count per lane.
- `psraw`/`psrad` by the lane width or more gave 0 instead of the sign.
- `shufpd` picked halves from the wrong operands (a unit test pinned the bug).
- `pminub`, `pmaxub`, `pminsw`, `pmaxsw`, `pavgb`, `pavgw`, `unpcklps`,
  `unpckhps`, `shufps` were not implemented and were **silently skipped**. They
  are now interpreter-only.

An unimplemented instruction now **stops the run** (`WIE_DEGRADE=1` opts back
into skipping). `ud2` therefore stops too. Not re-run: DOOM, notepad, x87-heavy
guests.

### Speed

| change | effect |
| --- | --- |
| Last-page cache per access site in loops (`jit/lower/site_homes.rs`, 8 sites), stack homes in regions | `sib` 107 → 65 ms, `bytes` 380 → 234 ms; `7za t`/`x` about 3.5 % slower, mostly compile time |
| Reuse the vector/float value behind an XMM half pair | `paddd` 4.1 → 0.50 ns, `addsd` 3.5 → 0.90 ns, `fp` 335 → 182 ms |

Checks on this pass: `wie-winapi` 1401/1401; `wie-runtime` 195/196 (the one
failure, `n2_write_and_read_file_in_bottle`, also fails on a clean `HEAD`);
`sse_check.py` 471 results with compiled code, 0 differences; `run.py` results
identical between Rosetta and WIE for every case. `a_region_store_site_does_not_
outlive_a_reprotect` covers re-protection in region mode. Peer-thread churn
against site caches is **untested**: `mt_contention 4 64 40` showed 0/20 failures
on the start-of-day binary too, so it no longer reproduces anything.

Not done: 16-byte memory accesses are still split in two (`movdqu` ≈ 2 ns), XMM
registers are still written through to the context every instruction, and
`call`/`ret` is unchanged.
