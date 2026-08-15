/*
 * Regression for the fast-sync dispatch tail (pump.rs): a failed
 * MultiByteToWideChar (output buffer too small) must publish
 * ERROR_INSUFFICIENT_BUFFER so the next GetLastError observes it. The unified
 * HeapAlloc/HeapFree/MultiByteToWideChar tail writes last error into the guest
 * TEB after the handler runs; GetLastError is an in-guest stub that reads the
 * TEB, so the failure is only visible if the publish happened.
 *
 * Exit codes:
 *   0 — ok
 *   1 — MultiByteToWideChar did not fail on the too-small buffer
 *   2 — GetLastError after the failed conversion != ERROR_INSUFFICIENT_BUFFER
 *   3 — the size query (NULL/0) failed or returned the wrong unit count
 *   4 — GetLastError after the successful size query != 0
 */

#include <windows.h>

#define CHECK(cond, code)                                                      \
  do {                                                                         \
    if (!(cond))                                                               \
      ExitProcess(code);                                                       \
  } while (0)

void entry(void) {
  wchar_t out[2];
  int n;

  // 1. Too-small buffer: MultiByteToWideChar must fail (returns 0)...
  SetLastError(0);
  n = MultiByteToWideChar(CP_ACP, 0, "hello", 5, out, 2);
  CHECK(n == 0, 1);

  // ...and publish ERROR_INSUFFICIENT_BUFFER (122) for the next GetLastError.
  CHECK(GetLastError() == ERROR_INSUFFICIENT_BUFFER, 2);

  // 2. Size query (NULL output, 0 length) still succeeds and must NOT leave
  //    ERROR_INSUFFICIENT_BUFFER behind.
  SetLastError(0);
  n = MultiByteToWideChar(CP_ACP, 0, "hello", 5, NULL, 0);
  CHECK(n == 5, 3);
  CHECK(GetLastError() == 0, 4);

  ExitProcess(0);
}
