/*
 * Micro-PE for the ADVAPI32 IsTextUnicode lane — the notepad.exe import gap,
 * handled in crates/wie-winapi/src/advapi32.rs.
 *
 * CRT-linked console program (mingw, -ladvapi32). Exercises the handler
 * against three buffers:
 *   UTF-16LE text           — must look like Unicode, with a UNICODE_MASK flag
 *   plain ASCII text        — must NOT look like Unicode
 *   UTF-16LE byte-order mark — must set IS_TEXT_UNICODE_SIGNATURE
 *
 * The input value of `flags` is the requested-test mask (MSDN); the handler
 * writes back the flags it actually set, ANDed with that mask.
 *
 * Exit codes:
 *   0 — ok
 *   1 — wide buffer not detected as Unicode (or no UNICODE_MASK flag)
 *   2 — ASCII buffer detected as Unicode
 *   3 — BOM buffer not detected (or SIGNATURE flag missing)
 */

#include <windows.h>
#include <stdio.h>

#define WANT_MASK (IS_TEXT_UNICODE_UNICODE_MASK | IS_TEXT_UNICODE_REVERSE_MASK \
                   | IS_TEXT_UNICODE_NULL_BYTES | IS_TEXT_UNICODE_ODD_LENGTH)

int main(void) {
    wchar_t uni[] = L"Hello Unicode";
    char ascii[] = "plain ascii text here";
    BYTE bom[] = {0xFF, 0xFE, 'a', 0, 'b', 0};
    int flags;

    flags = WANT_MASK;
    if (!IsTextUnicode(uni, sizeof(uni), &flags)
        || (flags & IS_TEXT_UNICODE_UNICODE_MASK) == 0) {
        printf("istextunicode: wide FAILED (ret 0 or no UNICODE_MASK flag); "
               "flags=0x%x\n", flags);
        return 1;
    }
    printf("istextunicode: wide -> unicode, flags=0x%x\n", flags);

    flags = WANT_MASK;
    if (IsTextUnicode(ascii, sizeof(ascii), &flags)) {
        printf("istextunicode: ascii FAILED (detected as Unicode); "
               "flags=0x%x\n", flags);
        return 2;
    }
    printf("istextunicode: ascii -> not unicode, flags=0x%x\n", flags);

    flags = WANT_MASK;
    if (!IsTextUnicode(bom, sizeof(bom), &flags)
        || (flags & IS_TEXT_UNICODE_SIGNATURE) == 0) {
        printf("istextunicode: bom FAILED (ret 0 or no SIGNATURE flag); "
               "flags=0x%x\n", flags);
        return 3;
    }
    printf("istextunicode: bom -> signature, flags=0x%x\n", flags);

    printf("istextunicode: done\n");
    return 0;
}
