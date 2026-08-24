# WIE JIT Architecture Review — Path to 100M+ insn/s

## Current state

Doom Retro startup: 364M JIT instructions in ~22s of emu time ≈ **16.5M insn/s**.
Target: **100M insn/s** (6× improvement). This document analyzes where the time
goes and what architectural changes are needed.

---

## 1. Where the time actually goes (release profile, Doom Retro startup)

| Component | Time | % of wall | Evidence |
|---|---|---|---|
| JIT-compiled code execution | ~15s | 68% | 364M insns ÷ effective rate |
| Background compile stalls | 1.1s | 5% | bg_stall_us=1.14s |
| Compile time (BG worker) | ~3.4s | 15% | compile_us=3.45s (overlapped) |
| Handler dispatch (74k stops) | ~2s | 9% | host_stops=237k × ~10µs/stop |
| Iced interpreter fallback | ~0.8s | 3% | iced=4.7M insns |
| Memory helper calls | ~2.1s | 9% | load=19M helpers × ~110ns |

These overlap: BG compile runs on another core; handler dispatch is inside
emu_ms. The critical path is: **JIT code execution + memory ops + block
dispatch overhead**.

### The fundamental problem

735M guest instructions at 100M/s = 7.35s. At current 16.5M/s = 44s.
The gap is **per-instruction overhead**, not per-block overhead.

Breaking down the ~60ns/instruction average:
- Block dispatch + register reload/storeback: ~10ns/block × 1 insn/block avg
- Memory operand resolution (sticky/pin probe or helper): ~20-50ns per mem op
- Flag computation and materialization: ~5-15ns per flag-setting insn
- Register writeback to JitCtx on chain boundaries: ~5ns per GPR
- Helper function call for unresolved loads/stores: ~100ns each

---

## 2. Architectural changes ranked by impact

### Tier 1: Eliminate the JitCtx round-trip (~2-3× throughput)

Currently every compiled block:
1. Loads all live GPRs from JitCtx into native registers on entry
2. Executes guest instructions using native registers
3. Stores dirty GPRs back to JitCtx on exit

Between chained blocks, this is a full save/restore cycle even when only
1-2 registers changed. The fix is **direct block-to-block register passing**:

```
Instead of:  block A → store regs to ctx → dispatcher → load regs from ctx → block B
Do:          block A → pass native registers directly → block B
```

This requires:
- A fixed native ABI for block entry (e.g., rax=gpr0, rcx=gpr1, ...)
- Only spilled/restored registers touch memory
- Flags passed via a native register (e.g., r14) not memory
- RIP passed as return address or indirect jump target

Expected impact: eliminates ~10-20ns of ctx load/store per block boundary.
With ~750k block transitions (364M insns / ~480 insns/block), saves ~15ms.
More importantly: enables keeping hot values in native registers across blocks.

### Tier 1b: Reduce memory operand resolution cost (~1.5-2×)

34% of loads go through the Rust helper path. Each helper call is a foreign
function call with context pointer, address computation, and bounds checking —
~110ns per operation.

Fixes:
a) **Wider pin coverage**: ensure the top-N most-accessed regions have pins.
   Currently stack + 2 heap pins cover most accesses but some fall through.
b) **Larger sticky TLB**: increase STICKY_WAYS from current value to reduce
   thrashing when alternating between distant pages.
c) **Address-space layout optimization**: place frequently-accessed
   allocations in the same arena so a single pin covers them.

The 21M→2.4M improvement from Pin mode proves this works. Extending it to
cover ALL hot addresses would eliminate ~2s of helper-call overhead.

### Tier 1c: Eliminate redundant flag computation

x86 flags are expensive because each ALU instruction sets 6 flags. Current
approach uses "lazy" evaluation: compute SF/ZF/CF/OF only when needed by a
conditional branch. But the implementation materializes them as integer
operations on a packed rflags value.

Better: track flags as Cranelift SSA values (not packed bits) so that:
- `test r,r` just sets ZF = (r == 0), SF = (r < 0) as i1 values
- `je` consumes ZF directly without unpacking
- Unneeded flags are never computed
- No packing/unpacking of the rflags bitmask

This requires changing PendingFlags from a bitmask representation to
individual SSA booleans. Expected impact: ~5-15% of insns set flags,
each saving ~5-10ns of unnecessary bit manipulation.

### Tier 2: Larger compilation units (~1.3-1.5×)

Current: blocks end at branch targets, typically 5-50 instructions.
Longer blocks amortize entry/exit overhead over more useful work.

Approaches:
a) **Trace compilation**: follow the hottest path through branches, compiling
   a straight-line trace of 100-500 insns. Cold branches become side exits.
b) **Loop-invariant block merging**: combine basic blocks that always execute
   sequentially into one compiled unit.
c) **Inline small functions**: if a called function is < 20 insns and doesn't
   recurse, inline its body into the caller's compiled block.

Impact: fewer block transitions = less dispatch overhead. Also enables
better instruction scheduling within larger compilation units.

### Tier 3: Instruction specialization (~1.2-1.4×)

Common x86 patterns that can be lowered more efficiently:

- **movzx/movsx chains**: `movzx eax, byte [mem]` followed by operations —
  fuse the zero-extension into the memory load
- **Compare-and-branch fusion**: `cmp eax, imm32; je target` — lower as a
  single compare-and-branch instead of separate cmp + jcc
- **Load-op-store fusion**: `add [mem], reg` — single RMW operation instead
  of load + add + store
- **Flag-independent paths**: for `inc/dec`, don't read CF input

### Tier 4: Codegen improvements

- **Register allocation**: use Cranelift's better register allocator
  (`regalloc_algorithm = checker` or `backtracking`)
- **Instruction selection**: enable ISLE optimizations for aarch64
- **Constant propagation**: fold multi-instruction constant sequences

---

## 3. What NOT to do

- Don't switch to a different backend (LLVM, DynASM) — migration cost too high
- Don't implement a custom ISA-specific assembler — Cranelift handles this
- Don't add caching layers that increase memory pressure — already memory-bound
- Don't remove correctness checks (bounds, generation) — these are safety-critical

---

## 4. Implementation roadmap

### Phase 1 (highest ROI, lowest risk)
- [ ] Change default jit_mem_mode to Pin (already done)
- [ ] Set bg_wait_timeout to 1ms (already done)
- [ ] Increase STICKY_WAYS if currently < 4
- [ ] Audit pin slot selection: are the RIGHT regions pinned?

### Phase 2 (moderate effort, high impact)
- [ ] Direct register passing between chained blocks
- [ ] Lazy flags as SSA booleans instead of packed rflags
- [ ] Trace compilation for hot loops

### Phase 3 (larger effort, incremental gains)
- [ ] Compare-and-branch fusion
- [ ] Load-op-store fusion
- [ ] Small function inlining

### Target: 100M insn/s

Phase 1 alone should reach ~30M insn/s (2× current).
Phase 2 adds direct register passing + lazy flags → ~50-80M insn/s.
Phase 3 pushes past 100M insn/s for hot loops.

Cold startup (the current bottleneck) benefits most from Phase 1 + 2
because it's dominated by first-execution overhead, not steady-state
hot loop throughput.

---

## 5. Key metrics to track

```rust
// Add to JitStats:
pub insns_per_sec: u64,         // overall throughput
pub block_transitions: u64,     // how many times we enter a new block
pub chained_transitions: u64,   // how many were direct chains vs dispatcher
pub helper_load_ns: u64,        // cumulative time in load helpers
pub helper_store_ns: u64,       // cumulative time in store helpers
pub reg_writeback_count: u64,   // GPR stores to ctx on exit
```

Profile after each change to verify improvement.
