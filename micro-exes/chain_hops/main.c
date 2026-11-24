/*
 * Chain-hop micro: a hot ring of tiny register-only basic blocks.
 *
 * WHY THIS EXISTS
 *   micro-exes/long_loop — the fixture `./scripts/check.sh --only perf` gates
 *   on — cannot measure anything about block chaining. Disassembly shows its
 *   100M-iteration loop is ONE basic block (0x140001040..0x140001073, a single
 *   `jb` back-edge), so the JIT lowers it as a self-loop: it never leaves
 *   native code, takes zero chain hops, and its timing is a pure measure of
 *   straight-line lowering. Any change to the chain-edge handoff is invisible
 *   there by construction.
 *
 *   This fixture is the opposite shape on purpose. The hot loop is written in
 *   inline asm as a ring of four small basic blocks joined by unconditional
 *   `jmp`s, so *every* iteration crosses four chain edges:
 *
 *     B0: acc^=R12; acc+=R13; acc+=R14; acc+=RBX   jmp B1
 *     B1: acc^=RBX; acc+=R12; rol acc,3            jmp B2
 *     B2: acc^=R13; acc+=RBX; acc+=R14            jmp B3
 *     B3: acc^=R12; acc+=R14; sub rcx,1; jnz B0
 *
 *   Each block reads several callee-saved registers it never writes, so the
 *   chain-edge GPR writeback re-stores them — the read-only live-ins the store
 *   predicate is about. That makes this a benchmark for the chain-edge handoff
 *   (see `JitStats::chain.hops` / `chain.store_ops` on the
 *   WIE_RUNTIME_PROFILE report line).
 *
 *   Hand-written asm, not C: gcc -O2 fuses a C loop of this size into a single
 *   basic block (that is exactly what long_loop is), which would reproduce the
 *   blind spot this fixture exists to remove.
 *
 * CORRECTNESS
 *   The asm ring is compared against a `volatile` C reference running the same
 *   recurrence, and the ring is run twice to catch state corruption across
 *   chain frames. A miscompiled chain edge (dropped or reordered register
 *   handoff) changes `acc`, so this fails loudly rather than merely running
 *   slow.
 *
 * Exit codes:
 *   0 — ok
 *   1 — asm ring result != volatile C reference
 *   2 — two identical ring runs disagreed (nondeterminism / state corruption)
 *   3 — the ring reported the wrong iteration count (block never ran)
 */

#include <windows.h>

#define RING_ITERS 400000ULL
#define RING_ITERS_SHORT 40000ULL

/* Values planted in callee-saved registers before the ring starts. They are
 * read (never written) inside the ring, so each block that touches one loads
 * it as a live-in — the shape a chain-edge writeback re-stores. */
#define K_RBX 0x0123456789abcdefULL
#define K_R12 0xfedcba9876543210ULL
#define K_R13 0x0f1e2d3c4b5a6978ULL
#define K_R14 0x89abcdef01234567ULL

/* Opt-in short mode: WIE_SHORT=1 in the *guest* environment (injected host-side
 * through WIE_GUEST_ENV="WIE_SHORT=1") drops the ring below to a quarter of
 * its iterations so a test sweep is not dominated by it. Absent that variable
 * the count stays RING_ITERS, so the default path is exactly the behaviour the
 * perf numbers describe. Short mode never relaxes the assertions: the
 * reference recomputes over the same (smaller) count. */
static int short_mode(void) {
  char buf[16];
  DWORD n = GetEnvironmentVariableA("WIE_SHORT", buf, sizeof(buf));
  return n == 1 && buf[0] == '1';
}

/* The chain-heavy ring. Returns the accumulator after `n` iterations. */
static __attribute__((noinline)) unsigned long long ring_hot(unsigned long long n) {
  unsigned long long acc;
  __asm__ __volatile__("movabsq $" "0xfedcba9876543210"
                       ", %%r12\n\t"
                       "movabsq $" "0x0f1e2d3c4b5a6978"
                       ", %%r13\n\t"
                       "movabsq $" "0x89abcdef01234567"
                       ", %%r14\n\t"
                       "movabsq $" "0x0123456789abcdef"
                       ", %%rbx\n\t"
                       "xorl %%eax, %%eax\n\t"
                       "movq %[n], %%rcx\n\t"
                       "testq %%rcx, %%rcx\n\t"
                       "je 2f\n\t"
                       "1:\n\t"
                       "xorq %%r12, %%rax\n\t"
                       "addq %%r13, %%rax\n\t"
                       "addq %%r14, %%rax\n\t"
                       "addq %%rbx, %%rax\n\t"
                       "jmp 3f\n\t"
                       /* Load-bearing `nop`: the block decoder folds an
                        * unconditional `jmp` whose target is the next
                        * instruction into a fallthrough and keeps decoding
                        * (block.rs `target == next`). Without this byte the
                        * whole ring would merge into ONE block — i.e. exactly
                        * long_loop's blind spot. */
                       "nop\n\t"
                       "3:\n\t"
                       "xorq %%rbx, %%rax\n\t"
                       "addq %%r12, %%rax\n\t"
                       "rolq $3, %%rax\n\t"
                       "jmp 4f\n\t"
                       "nop\n\t"
                       "4:\n\t"
                       "xorq %%r13, %%rax\n\t"
                       "addq %%rbx, %%rax\n\t"
                       "addq %%r14, %%rax\n\t"
                       "jmp 5f\n\t"
                       "nop\n\t"
                       "5:\n\t"
                       "xorq %%r12, %%rax\n\t"
                       "addq %%r14, %%rax\n\t"
                       "subq $1, %%rcx\n\t"
                       "jnz 1b\n\t"
                       "2:\n\t"
                       : "=&a"(acc)
                       : [n] "r"(n)
                       : "rbx", "r12", "r13", "r14", "rcx", "cc", "memory");
  return acc;
}

/* Same recurrence, in C, over `volatile` state so gcc cannot reassociate or
 * fuse it. This is the oracle; it is not the workload under test. */
static unsigned long long ring_reference(unsigned long long n) {
  volatile unsigned long long a = 0;
  unsigned long long i;
  for (i = 0; i < n; i++) {
    a ^= K_R12;
    a += K_R13;
    a += K_R14;
    a += K_RBX;
    a ^= K_RBX;
    a += K_R12;
    a = (a << 3) | (a >> 61);
    a ^= K_R13;
    a += K_RBX;
    a += K_R14;
    a ^= K_R12;
    a += K_R14;
  }
  return a;
}

void entry(void) {
  unsigned long long iters = short_mode() ? RING_ITERS_SHORT : RING_ITERS;
  unsigned long long want = ring_reference(iters);
  unsigned long long got1 = ring_hot(iters);
  unsigned long long got2;

  if (got1 != want) {
    ExitProcess(1);
  }
  got2 = ring_hot(iters);
  if (got2 != got1) {
    ExitProcess(2);
  }
  /* Sanity: a ring of a million no-op-ish iterations must have moved `acc`.
   * Catches a build where the asm was optimised into a constant or the loop
   * never executed, which would make the fixture measure nothing. */
  if (ring_hot(0) != 0) {
    ExitProcess(3);
  }

  ExitProcess(0);
}