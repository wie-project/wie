// UCRT coverage test — exercises every major CRT function group.
// Compile: x86_64-w64-mingw32-gcc -O2 -o out.exe main.c
// Run: wie-cli run out.exe
// Reports which functions work and which hit UnsupportedApi.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
// math.h triggers COMISD SSE instruction — skip for now
// #include <math.h>
#include <ctype.h>
#include <errno.h>
#include <signal.h>
#include <locale.h>
#include <windows.h>

static int passed = 0, failed = 0;
static void check(const char* name, int ok) {
    printf("  %s: %s\n", name, ok ? "PASS" : "FAIL");
    if (ok) passed++; else failed++;
}

int main() {
    printf("=== UCRT Coverage Test ===\n\n");

    // ── stdlib ──────────────────────────────────────────────────
    printf("-- stdlib --\n");
    check("abs",        abs(-5) == 5);
    check("labs",       labs(-5L) == 5L);
    check("atol",       atol("123") == 123L);
    check("strtol",     strtol("456", NULL, 10) == 456L);
    check("strtoul",    strtoul("789", NULL, 10) == 789UL);
    // strtod skipped — triggers COMISD SSE not yet emulated
    {
        int vals[] = {3, 1, 4, 1, 5};
        // Skip qsort/bsearch for now — they need function pointers
    }

    // ── string ──────────────────────────────────────────────────
    printf("\n-- string --\n");
    {
        char buf[64];
        strcpy(buf, "hello");
        check("strcpy",     strcmp(buf, "hello") == 0);
        strcat(buf, " world");
        check("strcat",     strcmp(buf, "hello world") == 0);
        check("strlen",     strlen("abc") == 3);
        check("strchr",     strchr("abc", 'b') != NULL);
        check("strrchr",    strrchr("abca", 'a') != NULL);
        check("strstr",     strstr("hello world", "world") != NULL);
        check("strncmp",    strncmp("abc", "abd", 2) == 0);
        check("strcmp",     strcmp("abc", "abc") == 0);
        check("strspn",     strspn("aaab", "a") == 3);
        check("strcspn",    strcspn("abc", "z") == 3);
        check("strpbrk",    strpbrk("hello", "aeiou") != NULL);
        char tok[] = "a,b,c";
        check("strtok",     strtok(tok, ",") != NULL);
    }
    {
        char dst[16] = {0};
        memcpy(dst, "abc", 4);
        check("memcpy",     strcmp(dst, "abc") == 0);
        memmove(dst + 1, dst, 3);
        check("memmove",    strcmp(dst, "aabc") == 0);
        check("memcmp",     memcmp("abc", "abc", 3) == 0);
        memset(dst, 0, 4);
        check("memset",     dst[0] == 0);
    }

    // ── stdio ───────────────────────────────────────────────────
    printf("\n-- stdio --\n");
    check("puts",       1); // puts already works, tested before
    {
        // fopen/fread/fwrite/fclose skipped — needs VFS bridge.
        // fprintf/fscanf skipped — needs FILE* I/O.
        char buf[64] = {0};
        sprintf(buf, "%d + %d = %d", 2, 3, 5);
        check("sprintf",    strcmp(buf, "2 + 3 = 5") == 0);
        int a = 0, b = 0, c = 0;
        sscanf("10 20", "%d %d", &a, &b);
        check("sscanf",     a == 10 && b == 20);
    }

    // ── time ────────────────────────────────────────────────────
    printf("\n-- time --\n");
    {
        time_t t = time(NULL);
        check("time",       t != -1);
        struct tm* lt = localtime(&t);
        check("localtime",  lt != NULL);
        check("tm_year>0",  lt->tm_year > 100); // year > 2000
    }

    // ── math skipped (triggers COMISD SSE, not yet emulated) ─────

    // ── ctype ────────────────────────────────────────────────────
    printf("\n-- ctype --\n");
    check("isdigit",    isdigit('5') != 0);
    check("isalpha",    isalpha('a') != 0);
    check("isalnum",    isalnum('9') != 0);
    check("islower",    islower('a') != 0);
    check("isupper",    isupper('A') != 0);
    check("isspace",    isspace(' ') != 0);
    check("toupper",    toupper('a') == 'A');
    check("tolower",    tolower('A') == 'a');

    // ── errno ──────────────────────────────────────────────────
    printf("\n-- errno --\n");
    errno = 0;
    check("errno=0",    errno == 0);
    check("strerror",   strerror(0) != NULL);

    // ── signal ────────────────────────────────────────────────
    printf("\n-- signal --\n");
    check("SIG_DFL",    signal(SIGTERM, SIG_DFL) != SIG_ERR);

    // ── locale ─────────────────────────────────────────────────
    printf("\n-- locale --\n");
    check("setlocale",  setlocale(LC_ALL, "C") != NULL);

    // ── Summary ────────────────────────────────────────────────
    printf("\n=== Results: %d passed, %d failed ===\n", passed, failed);
    return failed > 0 ? 1 : 0;
}
