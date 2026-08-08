/*
 * Micro-PE: the DBGHELP symbol flow — SymInitializeW → SymFromAddrW →
 * SymCleanup.
 *
 * CRT-linked console program. WIE loads no symbols: initialization must
 * succeed, a lookup for an arbitrary address must fail gracefully, and
 * cleanup must succeed.
 *
 * Exit codes:
 *   0  — ok
 *   1  — SymInitializeW returned zero
 *   2  — SymFromAddrW returned nonzero (lookup should fail)
 *   3  — SymCleanup returned zero
 *
 * Docs: SymInitializeW, SymFromAddrW, SymCleanup (Microsoft Learn).
 * Clean room.
 */

#include <windows.h>
#include <dbghelp.h>
#include <stdio.h>

int main(void) {
  HANDLE process;
  DWORD64 displacement = 0;
  SYMBOL_INFOW symbol;

  process = GetCurrentProcess();

  printf("dbghelp_stub: SymInitializeW...\n");
  if (!SymInitializeW(process, NULL, TRUE)) {
    printf("  FAILED\n");
    return 1;
  }
  printf("  ok\n");

  printf("dbghelp_stub: SymFromAddrW(0x140001000)...\n");
  symbol.SizeOfStruct = sizeof(SYMBOL_INFOW);
  symbol.MaxNameLen = sizeof(symbol.Name);
  if (SymFromAddrW(process, 0x140001000, &displacement, &symbol) != 0) {
    printf("  FAILED — expected graceful miss\n");
    return 2;
  }
  printf("  ok (no symbols loaded — lookup fails gracefully)\n");

  printf("dbghelp_stub: SymCleanup...\n");
  if (!SymCleanup(process)) {
    printf("  FAILED\n");
    return 3;
  }
  printf("  ok\n");

  printf("dbghelp_stub: done\n");
  return 0;
}
