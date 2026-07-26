// REP MOVS/STOS length sweep — regression guard for the JIT inline-REP path.
//
// The JIT lowers small `rep movs`/`rep stos` to unrolled 16-byte SIMD stores
// (`WIE_STRING_INLINE`, on by default). That path once only emitted *full*
// 16-byte chunks while accepting any length in [16, 64], so a length such as
// 20 or 51 silently lost its trailing `len & 15` bytes even though RCX was
// zeroed and RSI/RDI advanced the full count.
//
// This micro drives both instructions across every length in [1, MAX_LEN] and
// checks (a) all requested bytes were transferred and (b) the byte one past the
// end was NOT touched. The outer repeat count exists so the containing block
// becomes JIT-hot; the bug is invisible while the loop still runs on iced.
//
// Exit codes: 0 = pass. Otherwise see `fail()` below.

#include <windows.h>

#define MAX_LEN 70
#define HOT_ITERS 400
#define GUARD 0xAA

static volatile unsigned char src[256];
static volatile unsigned char dst[256];

// 1..MAX_LEN        -> movs lost/garbled a byte at this length
// 100+len           -> movs wrote past the requested length
// 200+len           -> stos lost/garbled a byte at this length
// 300+len           -> stos wrote past the requested length
static void fail(unsigned code) { ExitProcess(code); }

static void reset(void) {
  for (int i = 0; i < 256; i++) {
    src[i] = (unsigned char)(i + 1);
    dst[i] = GUARD;
  }
}

static void check_movs(int n) {
  reset();
  void *volatile d = (void *)dst;
  void *volatile s = (void *)src;
  unsigned long long cnt = (unsigned long long)n;
  __asm__ __volatile__("cld\n\t"
                       "rep movsb\n\t"
                       : "+D"(d), "+S"(s), "+c"(cnt)
                       :
                       : "memory");
  for (int i = 0; i < n; i++) {
    if (dst[i] != (unsigned char)(i + 1))
      fail((unsigned)n);
  }
  if (dst[n] != GUARD)
    fail((unsigned)(100 + n));
}

static void check_stos(int n) {
  reset();
  void *volatile d = (void *)dst;
  unsigned long long cnt = (unsigned long long)n;
  unsigned long long val = 0x5C;
  __asm__ __volatile__("cld\n\t"
                       "rep stosb\n\t"
                       : "+D"(d), "+c"(cnt)
                       : "a"(val)
                       : "memory");
  for (int i = 0; i < n; i++) {
    if (dst[i] != 0x5C)
      fail((unsigned)(200 + n));
  }
  if (dst[n] != GUARD)
    fail((unsigned)(300 + n));
}

void entry(void) {
  for (int iter = 0; iter < HOT_ITERS; iter++) {
    for (int n = 1; n <= MAX_LEN; n++) {
      check_movs(n);
      check_stos(n);
    }
  }
  ExitProcess(0);
}
