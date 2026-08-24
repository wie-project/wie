// Regression micro for the MSVC CRT strncpy-core byte loop shape seen in
// Doom Retro (doomretro.exe 0x14013a580..0x14013a5d3):
//
//   rcx = dst, rdx = count, r8 = src, r9 = dst
//   subq  %r9, %r8              ; delta = src - dst
//   L: movzbl (%r8,%rcx), %eax  ; SIB(base=r8, index=rcx, scale=1) load src[i]
//      movb   %al, (%rcx)       ; store dst[i]
//      leaq   0x1(%rcx), %rcx
//      testb  %al, %al / je done
//      subq   $1, %rdx / jne L
//
// Under WIE this shape once corrupted DEHACKED string values with a
// phase-locked stride-2 doubling ("%.2f." arrived as "%"+"%"+"2"+"2"+"."+".").
//
// Exit codes: 0 = pass; otherwise 10/30 + (mismatch offset + 1).

#include <windows.h>

#define HOT_ITERS 500

static unsigned char far_dst[256];
static unsigned char near_buf[192];

static void my_memset(unsigned char *p, unsigned char v, unsigned n) {
  for (unsigned i = 0; i < n; i++)
    p[i] = v;
}

static unsigned my_len(const char *s) {
  unsigned n = 0;
  while (s[n])
    n++;
  return n;
}

static __attribute__((noinline)) void crt_copy(unsigned char *dst,
                                               const unsigned char *src,
                                               unsigned long long count) {
  __asm__ volatile("movq %[d], %%rcx\n"
                   "movq %[s], %%r8\n"
                   "movq %[c], %%rdx\n"
                   "movq %%rcx, %%r9\n"
                   "subq %%r9, %%r8\n"
                   "1:\n"
                   "movzbl (%%r8,%%rcx), %%eax\n"
                   "movb %%al, (%%rcx)\n"
                   "leaq 1(%%rcx), %%rcx\n"
                   "testb %%al, %%al\n"
                   "je 2f\n"
                   "subq $1, %%rdx\n"
                   "jne 1b\n"
                   "2:\n"
                   :
                   : [d] "r"((unsigned long long)(uintptr_t)dst),
                     [s] "r"((unsigned long long)(uintptr_t)src),
                     [c] "r"(count)
                   : "rax", "rcx", "rdx", "r8", "r9", "cc", "memory");
}

static int check(const unsigned char *out, const char *expect) {
  unsigned n = my_len(expect);
  for (unsigned i = 0; i <= n; i++)
    if (out[i] != (unsigned char)expect[i])
      return (int)i + 1;
  return 0;
}

// MSVC CRT strcpy-core epilogue: one qword load then paired byte stores from
// AL/AH with `shr $16` between pairs (doomretro.exe 0x140155468..0x1401554ff).
// This is the shape whose `%ah` stores JIT-lowering once corrupted.
static __attribute__((noinline)) void copy8_epilogue(unsigned char *dst,
                                                     const unsigned char *src) {
  __asm__ volatile("movq (%[s]), %%rax\n"
                   "movb %%al, 0(%[d])\n"
                   "movb %%ah, 1(%[d])\n"
                   "shrq $16, %%rax\n"
                   "movb %%al, 2(%[d])\n"
                   "movb %%ah, 3(%[d])\n"
                   "shrq $16, %%rax\n"
                   "movb %%al, 4(%[d])\n"
                   "movb %%ah, 5(%[d])\n"
                   "shrq $16, %%rax\n"
                   "movb %%al, 6(%[d])\n"
                   "movb %%ah, 7(%[d])\n"
                   :
                   : [s] "r"((unsigned long long)(uintptr_t)src),
                     [d] "r"((unsigned long long)(uintptr_t)dst)
                   : "rax", "cc", "memory");
}

// High-byte register traffic beyond plain stores:
//   mov r8,r8 (AH->R8B), movzx r32,r8 (DH), xchg %ah,%r9b
static __attribute__((noinline)) unsigned
high_byte_regs(unsigned char seed) {
  unsigned res;
  __asm__ volatile("movl %[seed], %%eax\n"
                   "shll $8, %%eax\n"          /* AH = seed, AL = 0 */
                   "movb $0x11, %%r8b\n"
                   "movb %%ah, %%r8b\n"        /* R8B must become seed */
                   "movzbl %%r8b, %%ecx\n"
                   "movl %%ecx, %[res]\n"
                   : [res] "=m"(res)
                   : [seed] "r"((unsigned)seed)
                   : "eax", "r8", "ecx", "cc");
  return res;
}

static __attribute__((noinline)) unsigned
high_byte_movzx(unsigned char seed) {
  unsigned res;
  __asm__ volatile("movl %[seed], %%edx\n"
                   "shll $8, %%edx\n"          /* DH = seed */
                   "movzbl %%dh, %%eax\n"      /* EAX = zero-extended DH */
                   "movl %%eax, %[res]\n"
                   : [res] "=m"(res)
                   : [seed] "r"((unsigned)seed)
                   : "eax", "edx", "cc");
  return res;
}

static __attribute__((noinline)) unsigned
high_byte_xchg(unsigned char seed) {
  unsigned res;
  __asm__ volatile("movl %[seed], %%eax\n"
                   "shll $8, %%eax\n"          /* AH = seed */
                   "movb $0x22, %%r9b\n"
                   "xchgb %%ah, %%r9b\n"       /* AH <-> R9B */
                   "movzbl %%r9b, %%eax\n"     /* R9B should now be seed */
                   "movl %%eax, %[res]\n"
                   : [res] "=m"(res)
                   : [seed] "r"((unsigned)seed)
                   : "eax", "r9", "cc");
  return res;
}

void entry(void) {
  const char *g = "Gamma correction level %.2f.";
  const char *v = "Vivisection";
  const char *k = "Knee-Deep In ZDoom";

  // Heat the block so the JIT compiles it (bug was invisible on iced).
  for (int t = 0; t < HOT_ITERS; t++) {
    my_memset(far_dst, 0xAA, sizeof far_dst);
    crt_copy(far_dst, (const unsigned char *)g, 128);
    my_memset(near_buf, 0xAA, sizeof near_buf);
    crt_copy(near_buf + 3, (const unsigned char *)g, 100);
  }

  // Far-apart buffers: large |delta| between src and dst.
  my_memset(far_dst, 0xAA, sizeof far_dst);
  crt_copy(far_dst, (const unsigned char *)g, 128);
  int bad = check(far_dst, g);
  if (bad)
    ExitProcess(10 + bad);

  // Near buffers: small delta, still non-overlapping.
  my_memset(near_buf, 0xAA, sizeof near_buf);
  crt_copy(near_buf + 3, (const unsigned char *)v, 64);
  bad = check(near_buf + 3, v);
  if (bad)
    ExitProcess(30 + bad);

  // Odd-length string crossing the anomaly phase.
  my_memset(far_dst, 0xAA, sizeof far_dst);
  crt_copy(far_dst, (const unsigned char *)k, 128);
  bad = check(far_dst, k);
  if (bad)
    ExitProcess(50 + bad);

  // AH-store epilogue regression: every odd byte must come from AH, not a
  // duplicate of the preceding AL store. Drive hot so the JIT compiles it.
  for (int t = 0; t < HOT_ITERS; t++) {
    my_memset(far_dst, 0xAA, sizeof far_dst);
    copy8_epilogue(far_dst, (const unsigned char *)"Vivisection");
  }
  {
    const char *e = "Vivisection";
    my_memset(far_dst, 0xAA, sizeof far_dst);
    copy8_epilogue(far_dst, (const unsigned char *)e);
    for (unsigned i = 0; i < 8; i++)
      if (far_dst[i] != (unsigned char)e[i])
        ExitProcess(70 + (int)i + 1);
  }
  {
    const unsigned char q[8] = {'%', '.', '2', 'f', '.', 0, 0x77, 0x88};
    const unsigned char exp[6] = {'%', '.', '2', 'f', '.', 0};
    for (int t = 0; t < HOT_ITERS; t++)
      copy8_epilogue(far_dst, q);
    my_memset(far_dst, 0xAA, sizeof far_dst);
    copy8_epilogue(far_dst, q);
    for (unsigned i = 0; i < 6; i++)
      if (far_dst[i] != exp[i])
        ExitProcess(90 + (int)i + 1);
  }

  // High-byte register moves/extensions (hot loop for JIT compilation).
  const unsigned char seed = 'V';
  for (int t = 0; t < HOT_ITERS; t++) {
    if (high_byte_regs(seed) != (unsigned)seed)
      ExitProcess(110);
    if (high_byte_movzx(seed) != (unsigned)seed)
      ExitProcess(120);
    if (high_byte_xchg(seed) != (unsigned)seed)
      ExitProcess(130);
  }
  if (high_byte_regs('Q') != 'Q')
    ExitProcess(111);
  if (high_byte_movzx('Q') != 'Q')
    ExitProcess(121);
  if (high_byte_xchg('Q') != 'Q')
    ExitProcess(131);

  ExitProcess(0);
}
