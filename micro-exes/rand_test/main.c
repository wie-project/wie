#include <stdlib.h>
#include <stdio.h>

int main() {
    srand(42);
    int r1 = rand();
    int r2 = rand();
    int r3 = rand();
    // Windows UCRT: srand(42) → first rand() = 0xAF (175), then 0x190 (400), then 0x45CD (17869)
    if (r1 == 0xAF && r2 == 0x190 && r3 == 0x45CD) {
        return 0;  // success
    }
    return 1;  // failure
}
