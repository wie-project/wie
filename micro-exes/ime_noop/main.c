// IMM32 no-op micro-test: every IME API returns the benign "no IME" value.
//
// CRT-linked console program. Exits 0 only when ImmGetContext returns NULL,
// ImmGetOpenStatus returns 0, and ImmReleaseContext returns nonzero.
// Distinct codes (1-3) name the first check that failed.

#include <windows.h>
#include <imm.h>
#include <stdio.h>

int main(void) {
    // No IME: ImmGetContext(NULL) must return NULL.
    printf("ime_noop: ImmGetContext(NULL)...\n");
    if (ImmGetContext(NULL) != NULL) {
        printf("  FAILED — expected NULL\n");
        return 1;
    }
    printf("  ok (NULL — no IME)\n");

    // No IME: the context is never open.
    printf("ime_noop: ImmGetOpenStatus(NULL)...\n");
    if (ImmGetOpenStatus(NULL) != 0) {
        printf("  FAILED — expected 0\n");
        return 2;
    }
    printf("  ok (0 — not open)\n");

    // Releasing a NULL context still succeeds.
    printf("ime_noop: ImmReleaseContext(NULL, NULL)...\n");
    if (!ImmReleaseContext(NULL, NULL)) {
        printf("  FAILED — expected TRUE\n");
        return 3;
    }
    printf("  ok (TRUE)\n");

    printf("ime_noop: done\n");
    return 0;
}
