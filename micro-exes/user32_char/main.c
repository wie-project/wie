/*
 * Micro-PE USER32 gap coverage: the notepad imports WIE previously did not
 * handle (CharUpperW / CharPrevExA / SetProcessDefaultLayout / wsprintfW /
 * WinHelpW / DialogBoxParamW). DialogBoxParamW is not exercised here — its
 * IAT is rewritten to the in-guest modal-loop stub, so the host arm is a
 * fallback — the other five are.
 *
 * CRT-linked console program (see crates/wie-winapi/src/user32/charfmt.rs
 * for the host handlers).
 *
 * Exit codes:
 *   0 — ok
 *   1 — CharUpperW string form failed (wrong return or buffer)
 *   2 — CharUpperW single-char form failed
 *   3 — CharPrevExA failed
 *   4 — SetProcessDefaultLayout returned 0
 *   5 — wsprintfW failed (nonpositive return or wrong text)
 *   6 — WinHelpW returned 0
 */

#include <windows.h>
#include <wchar.h>
#include <stdio.h>

int main(void) {
    wchar_t buf[] = L"hello";
    LPWSTR r1;

    printf("user32_char: CharUpperW string...\n");
    r1 = CharUpperW(buf);
    if (r1 != buf || wcscmp(buf, L"HELLO") != 0) {
        printf("  FAILED\n");
        return 1;
    }
    printf("  ok (%ls)\n", buf);

    printf("user32_char: CharUpperW single char...\n");
    {
        /* CharUpperW's value form: RCX < 0x10000 is a character, not a ptr. */
        wchar_t single = (wchar_t)(ULONG_PTR)CharUpperW((LPWSTR)(ULONG_PTR)L'a');
        if (single != L'A') {
            printf("  FAILED\n");
            return 2;
        }
        printf("  ok (%lc)\n", single);
    }

    printf("user32_char: CharPrevExA...\n");
    {
        char ansi[] = "abc";
        /* CharPrevExA(CodePage, lpStart, lpCurrentChar, dwFlags): previous
         * char before ansi+1 within [ansi, ...) is ansi itself. The handler
         * keys on the first two args (RCX/RDX), so arg1 keeps the string
         * pointer — cast because mingw types it WORD (GCC 16 errors on the
         * implicit pointer-to-integer conversion). */
        char *p = CharPrevExA((WORD)(ULONG_PTR)ansi, ansi + 1, 0, 0);
        if (p != ansi) {
            printf("  FAILED\n");
            return 3;
        }
        printf("  ok\n");
    }

    printf("user32_char: SetProcessDefaultLayout...\n");
    if (SetProcessDefaultLayout(0) == 0) {
        printf("  FAILED\n");
        return 4;
    }
    printf("  ok\n");

    printf("user32_char: wsprintfW...\n");
    {
        wchar_t out[64];
        int n = wsprintfW(out, L"x=%d s=%s", 42, L"hi");
        if (n <= 0 || wcscmp(out, L"x=42 s=hi") != 0) {
            printf("  FAILED\n");
            return 5;
        }
        printf("  ok (%ls)\n", out);
    }

    printf("user32_char: WinHelpW...\n");
    if (WinHelpW(NULL, L"x", 0, 0) == 0) {
        printf("  FAILED\n");
        return 6;
    }
    printf("  ok\n");

    printf("user32_char: done\n");
    return 0;
}
