# Tier 0: a single-pass emitter for the entry arm

Status: **plan, not built.** What is in the tree is the part that can be pinned by
test before any code is produced — the whitelist classifier (`baseline::tier0_fit`,
`Tier0Fit`), the two pieces of x86 semantics an emitter gets silently wrong
(`tier0_result`, `tier0_flags`), a `DEBUG`-gated census in
`baseline::try_compile_baseline`, and six tests including two that are mutation-checked.
No ARM64 is emitted, so there is **no per-block-microsecond after-number** and the
0.1–0.3 ms bar in `performance-ceiling.md` §11.5 is not met.

Scope: `baseline::try_compile_baseline`, the one arm that compiles *synchronously on
the guest thread*. Bounds and requirement set: §11.5. Whether the tier-0 code should
exist at all: `compile-selection-policy.md` §5.

---

## 1. Why the entry arm is the target

Compile time in this engine is three different things and only one is on the critical
path. From bounded DOOM Retro runs (`WIE_RUNTIME_PROFILE=1`, 120 s, `--max-api
400000000`, loadavg 18–27, so **counts are load-bearing and seconds are inflated**):

| quantity | value | on the critical path? |
|---|---|---|
| background compiles | 9 070–10 023 blocks, `compile_us` 64–95 s | no — overlapped by 4 workers |
| guest-visible stalls | 1 242–1 426 waits, 1.83–2.07 s (1.5–1.7 %) | yes, but small |
| **inline compiles** (`eager` + `inline`) | **4 854–5 026 blocks** | **yes — synchronous on the guest thread** |

So the number worth attacking is ~5 000 blocks compiled inline, and its cost has three
measured regimes on the same block set: **1.23 ms/block** (`compile_us` 9.69 s over
7 854 compiles, a 7.6 s run), **2.6 ms** (the figure `compile-selection-policy.md` §5.1
originally assumed, now labelled as an assumption), and **4.6–6.8 ms** in the 120 s
runs where four workers compete with a guest retiring 18 G instructions. Same 5 000
blocks, **6–25 s** of guest-thread serial time depending on contention.

## 2. The measurement that sizes the emitter

A census of **4 225 blocks** reaching `try_compile_baseline` in one 7.6 s DOOM run
(`RUST_LOG=wie_cpu::jit::baseline=debug`, one `tier0 census` line per block with its
first blocker):

| outcome | blocks | share |
|---|---|---|
| **`alu_only`** — GPR ALU, no memory operand | **1 083** | **25.6 %** |
| **`with_mem`** — same instruction set, ≥1 memory operand | **690** | **16.3 %** |
| `term:call` | 1 013 | 24.0 % |
| `term:ret` | 556 | 13.2 % |
| `mnemonic:Lea` | 222 | 5.3 % |
| `term:calldyn` | 195 | 4.6 % |
| `mnemonic:Push` / `Pop` | 158 / 15 | 4.1 % |
| `mnemonic:Movzx` / `Movsxd` / `Movsx` | 56 / 49 / 16 | 2.9 % |
| `mnemonic:Shr` / `Sar` / `Shl` | 44 / 8 / 7 | 1.4 % |
| `Movups` 20, `Bt`/`Btr`/`Bts` 20, `Imul` 8, `Cmov*` 8, `Cdqe` 5, `Stosq` 2, `Div` 1 | | ~1.6 % |

The `none` blocker (i.e. nothing blocking, the two rows above) is **1 773 = 1 083 +
690**, which is the check that the classifier and its diagnostic now agree.

**Two conclusions, and they are the whole design:**

1. **42 % of the entry arm is terminator shape** (`call`/`ret`/`calldyn`). No
   single-pass emitter covers those without call support, so **58 % is the hard
   ceiling** for any first increment, whatever its opcode table. Whether to support
   call terminators is therefore the first design decision, not a detail — it doubles
   or does not double the addressable set.
2. **Inside that ceiling, the memory subset is not the small one.** `with_mem` is
   **690 blocks (16.3 %)** against `alu_only`'s 1 083 (25.6 %) — the same order of
   magnitude, not the rounding error the first (broken) census suggested. `Lea` (222),
   `Push`/`Pop` (173) and `Movsx*`/`Movzx` (121) are all address-generation or stack
   shapes on top of that. So "GPR ALU now, memory later" would ship an emitter for 26 %
   of the arm and leave a comparable chunk behind: **the first increment should take
   memory operands too**, which is also the increment that needs the probe/refill
   contract (`mem.rs`) and therefore the one with a real correctness surface.

### 2.1 The classifier bug the census found, and why it is recorded

The first version reported `alu_only` = 8 and `with_mem` = 17 — 0.6 %, and that was
**wrong**: `decode_pure_gpr_block` includes the terminator in `insns`, and the
classifier scanned every instruction through the mnemonic table, which has no
`Jcc`/`Jmp`. **1 748 blocks (41 %) were rejected by their own `jcc`**, which is why
`mnemonic:Jne`/`Je`/`Jmp` appeared in the ranking at all. The tell was two counters
agreeing: `blocker = "none"` was exactly 25 = 8 + 17.

The fix is `tier0_body` — one definition of "the body is everything but the
terminator", shared by the classifier and its diagnostic, precisely because they
disagreed. Two tests pin it: `add rax, rbx ; jne` must classify `AluOnly`, and a block
ending in `call` must still be `Other` so the fix cannot swallow terminator
classification. Reverting `tier0_body` fails the first with `left: Other, right:
AluOnly`, which is the bug reproducing.

**Corrected, on the same 4 225-block sample: `alu_only` 1 083 (25.6 %), `with_mem` 690
(16.3 %), so 42 % of the entry arm is already expressible** by the current whitelist
shape. The 1 748 terminator entries are gone from the ranking entirely.

The lesson is the reason the diagnostic exists: a count of "other" is not actionable,
a *rank-ordered blocker list* is, and building it before the backend is what caught a
bug in the thing that was about to be committed.

## 3. Design

**Contract** (`baseline.rs` module doc, unchanged by this work): install the block
`Ready` immediately, enqueue the tier-1 Cranelift job so the background worker's later
install overwrites it, and leave the self-loop tier-up path alone. The emitter is
therefore a *fast first translation*, not a final one — see §6 for what that implies
about §11.5's break-even.

**Emission shape** — slot-based, no register allocator: every guest GPR touches
`JitCtx` at its `OFF_*` slot, every instruction is `ldr / op / str` over that file, and
the block's exit stores what the exit contract requires and sets `ctx.rip`. The reasons
this is the right shape here and not a fast one:

- no allocator, no CLIF, no `define_function`, no relocations, no module mutex — the
  cost being attacked *is* Cranelift's front end and codegen, so reusing either defeats
  the purpose;
- `performance-ceiling.md` §11.5 already bounds the runtime price of that choice at
  **1.5–3× host instructions** for the same guest instructions, and states the
  break-even against a ~1 ms compile at **≈100 000 executions**;
- allocating into its own buffer also removes most of the engine-mutex wait that §11.5
  measured at 26 % of `x`'s compile path.

**Fallback is exhaustive and mandatory.** `tier0_fit` returns `Other`/`NotEligible` →
today's Cranelift path, unchanged. The emitter must never guess an encoding for a shape
it does not recognise; the failure mode of a wrong guess is silent wrong code, and this
is the same rule `WIE_JIT_REGION` and `WIE_JIT_CODE_CACHE` follow.

**Correctness plan** — three layers, because a tier-0 block is guest-observable in four
places (GPRs, the RFLAGS word, memory effects, the exit rip):

1. **Unit tests of the semantics**, already in place: 32-bit zero-extension vs 16-bit
   preservation (`tier0_result`), `inc`/`dec` preserving `CF`, logical ops clearing
   `CF`/`OF`/`AF`, parity from the low byte. Each is mutation-checked — removing the
   `CF` preservation fails with `inc 1 keeps the prior CF`; making parity whole-word
   fails with `low byte 0x00 is even: PF set`.
2. **A differential block test**: for every whitelisted shape, run the block through
   tier-0 and through Cranelift on the same `JitCtx` from the same input state, and
   compare all four observable sets. This is the test that catches a wrong flag verdict
   in the *emitted* code rather than in the reference, and it is the gate the emitter
   must pass before it is enabled anywhere.
3. **Whole-guest differentials**: the existing JIT-vs-iced fixtures and the micro-suite
   under both settings, since a block that is right in isolation can still disagree
   about the flag carrier across a block boundary.

## 4. Measurement plan

- **Per-block microseconds, before and after, on the same block set** — the after
  number does not exist yet. Report it per shape class (ALU/`Lea`/`Push`/`movzx`) so a
  slow class cannot hide behind a fast majority.
- Bar: 0.1–0.3 ms/block is the target from §11.5; the first milestone is **< 0.5 ms for
  the entry-time subset**, with the fallback fraction reported alongside, because a fast
  emitter that refuses half the arm is not a win.
- `compiles`, `eager`, `compile_us`, `init_ms` — not frames. The host is shared and a
  frame number taken now measures contention.

## 5. Non-goals

- SSE/AVX, x87, string ops, `Div`/`Imul` beyond a single-operand immediate: the census
  puts them under 2 % combined and each carries its own state contract.
- Replacing the Cranelift path for anything: tier 0 exists only because a *slow first
  translation of a shape we fully understand* is worse than a fast one that is later
  replaced.
- OSR, and any change to how loops are entered (`compile-selection-policy.md` §5).

## 6. Open questions, in the order they must be settled

1. **Call terminators: in or out?** 42 % of the entry arm. `term:call` needs the fast-API
   resolution and the return-site contract; if it is out, say the ceiling is 58 % in the
   first version and quote that number every time the emitter's coverage is discussed.
2. **Is the emitted code final, and therefore is §11.5's 100 000-execution break-even
   the right model?** The module docs say two different things and the code settles it:
   `try_compile_baseline`'s contract says "the caller ... enqueues a tier-1 Cranelift job
   for the same RIP" with last-writer-wins, but the call site in `pipeline.rs` enqueues
   **only when `is_loop`** — every other baseline block is installed `Ready` and never
   re-examined (`insert_ready` does not enqueue; that is also what the "why regions are
   not formed in here" note says). So today **tier-0 output would be final for 4 854 −
   (self-loops) blocks**, and §11.5's break-even applies exactly as written: a 1.5–3×
   runtime penalty that only pays on `h`-shaped guests, not on `x`-shaped ones.
   Making the penalty transient is a two-line change at that call site, but it adds one
   background compile per baseline block and risks the saturated-queue failure §11.5's
   A1 measured (4 837 deferrals, 1 377 cooldown timeouts). **Decide this before writing
   the emitter**: it is the difference between "wins on `h`, needs measurement on `x`"
   and "wins on both", and it is not a decision the emitter can make for itself.
3. **Where does the code live?** A separate buffer per §11.5's mutex argument, which
   means the code-memory path in `code-memory-on-darwin.md` applies to it (page-aligned
   starts, `MAP_JIT`, the aliasing defect) — not a detail to discover late.
4. **`Push`/`Pop` and the stack-pin contract.** They are 4 % of the arm and the only
   blockers that *write* the guest stack; whatever the emitter does there must agree
   with `mem.rs`'s pin/refill contract, or the region path and the block path will
   disagree about the same stack.
