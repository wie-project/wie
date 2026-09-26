#include <windows.h>

// Opt-in short mode: WIE_SHORT=1 in the *guest* environment (the host injects
// it with WIE_GUEST_ENV="WIE_SHORT=1") drops the spin below to SHORT_LIMIT so
// a test sweep is not dominated by waiting on it. When WIE_SHORT is absent —
// i.e. every normal run and every test that does not ask for short mode — the
// limit stays the full 100M iterations, so the default path is exactly the
// historical behaviour. Short mode is opt-in only; it never relaxes the
// loop's own correctness (there is nothing to assert here beyond ExitProcess).
static int short_mode(void) {
    char buf[16];
    DWORD n = GetEnvironmentVariableA("WIE_SHORT", buf, sizeof(buf));
    return n == 1 && buf[0] == '1';
}

#define LIMIT_DEFAULT 100000000ULL
#define LIMIT_SHORT   1000000ULL

void entry(void) {
  volatile unsigned long long counter = 0;
  volatile unsigned long long limit = LIMIT_DEFAULT;
  if (short_mode()) {
    limit = LIMIT_SHORT;
  }
  if (counter < limit) {
    do {
      volatile unsigned long long tmp = counter ^ 0xDEADBEEF;
      tmp = tmp * 3 + 1;
      (void)tmp;

      counter++;
    } while (counter < limit);
  }

  ExitProcess(0);
}
