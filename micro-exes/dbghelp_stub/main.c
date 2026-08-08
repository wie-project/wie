/*
 * Micro-PE: the DBGHELP symbol flow — SymInitializeW → SymFromAddrW →
 * SymCleanup.
 *
 * WIE loads no symbols: initialization must succeed, a lookup for an
 * arbitrary address must fail gracefully, and cleanup must succeed.
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

void entry(void) {
  HANDLE process;
  DWORD64 displacement = 0;
  /* Static storage: zero-filled .bss keeps the micro freestanding (no heap). */
  static SYMBOL_INFOW symbol;

  process = GetCurrentProcess();
  if (!SymInitializeW(process, NULL, TRUE)) {
    ExitProcess(1);
  }

  symbol.SizeOfStruct = sizeof(SYMBOL_INFOW);
  symbol.MaxNameLen = sizeof(symbol.Name);
  if (SymFromAddrW(process, 0x140001000, &displacement, &symbol) != 0) {
    ExitProcess(2);
  }

  if (!SymCleanup(process)) {
    ExitProcess(3);
  }

  ExitProcess(0);
}
