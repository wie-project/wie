// IMM32 no-op micro-test: every IME API returns the benign "no IME" value.
//
// Freestanding PE64 (void entry, ExitProcess, no CRT). Exits 0 only when
// ImmGetContext returns NULL, ImmGetOpenStatus returns 0, and
// ImmReleaseContext returns nonzero. Distinct codes (1-3) name the first
// check that failed.

#include <windows.h>
#include <imm.h>

void entry(void) {
    // No IME: ImmGetContext(NULL) must return NULL.
    if (ImmGetContext(NULL) != NULL) {
        ExitProcess(1);
    }
    // No IME: the context is never open.
    if (ImmGetOpenStatus(NULL) != 0) {
        ExitProcess(2);
    }
    // Releasing a NULL context still succeeds.
    if (!ImmReleaseContext(NULL, NULL)) {
        ExitProcess(3);
    }
    ExitProcess(0);
}
