/*
 * Micro-PE font-enumeration lane: EnumFontFamiliesExW through the real
 * host-fontdb enumeration in crates/wie-winapi/src/gdi32/enumerate.rs.
 *
 * CRT-linked console program. Flow:
 *   1. CreateCompatibleDC(NULL) for a memory DC.
 *   2. LOGFONTW lf = {0}; lf.lfCharSet = DEFAULT_CHARSET (1) — enumerate all.
 *   3. EnumFontFamiliesExW(hdc, &lf, cb, 0, 0): cb increments a static counter
 *      for every enumerated family and returns TRUE (continue).
 *   4. Assert the API returned non-zero AND the counter > 0 (the host font
 *      database always has at least one family).
 *   5. Print the count and exit 0.
 *
 * Exit codes:
 *   0  — ok (counter > 0, API returned non-zero)
 *   1  — CreateCompatibleDC failed
 *   2  — EnumFontFamiliesExW returned 0
 *   3  — callback never invoked (counter == 0)
 */

#include <windows.h>
#include <stdio.h>
#include <string.h>

static int g_families_seen;

static int CALLBACK cb(const LOGFONTW *lpelfe, const TEXTMETRICW *lpntme,
                       DWORD fontType, LPARAM lParam) {
    (void)lpelfe;
    (void)lpntme;
    (void)fontType;
    (void)lParam;
    g_families_seen++;
    return 1; /* TRUE: keep enumerating */
}

int main(void) {
    HDC hdc;
    LOGFONTW lf;
    int result;

    printf("font_enum: CreateCompatibleDC(NULL)...\n");
    hdc = CreateCompatibleDC(NULL);
    if (hdc == NULL) {
        printf("  FAILED\n");
        return 1;
    }

    memset(&lf, 0, sizeof(lf));
    lf.lfCharSet = DEFAULT_CHARSET;

    printf("font_enum: EnumFontFamiliesExW(DEFAULT_CHARSET)...\n");
    result = EnumFontFamiliesExW(hdc, &lf, cb, 0, 0);
    printf("font_enum: returned %d, %d families enumerated\n", result,
           g_families_seen);

    if (result == 0) {
        printf("  FAILED: EnumFontFamiliesExW returned 0\n");
        return 2;
    }
    if (g_families_seen == 0) {
        printf("  FAILED: callback never invoked\n");
        return 3;
    }

    DeleteDC(hdc);
    printf("font_enum: ok\n");
    return 0;
}
