/*
 * Micro-PE USER32 enumeration/caret lane (Tier-2): EnumWindows, FindWindowW,
 * the caret family and DrawIcon through the soft-dispatch handlers in
 * crates/wie-winapi/src/user32/enum_caret.rs.
 *
 * CRT-linked console program. Flow:
 *   1. RegisterClassExW("WIEEnumTest") + CreateWindowExW a top-level window.
 *   2. EnumWindows(cb, 0): cb counts every enumerated window into a static
 *      and returns TRUE; the window from step 1 must be enumerated (>= 1).
 *   3. FindWindowW("WIEEnumTest", NULL) must return that hwnd.
 *   4. CreateCaret(hwnd, 0, 2, 2) / SetCaretPos(10, 20) / GetCaretPos(&pt)
 *      must round-trip (10, 20); DestroyCaret succeeds.
 *   5. DrawIcon(GetDC(hwnd), 0, 0, NULL) must return TRUE; ReleaseDC.
 *   6. DestroyWindow + return 0.
 *
 * The window is created but never shown and no message loop runs — the
 * enumeration only needs the window record to exist.
 *
 * Exit codes:
 *   0  — ok
 *   1  — RegisterClassExW / CreateWindowExW failed
 *   2  — EnumWindows did not enumerate the window
 *   3  — FindWindowW returned the wrong hwnd
 *   4  — CreateCaret failed
 *   5  — SetCaretPos failed
 *   6  — GetCaretPos failed (or wrong position)
 *   7  — DestroyCaret failed
 *   8  — DrawIcon failed
 */

#include <windows.h>
#include <stdio.h>
#include <string.h>

static int g_windows_seen;

static BOOL CALLBACK cb(HWND hwnd, LPARAM lParam) {
    (void)hwnd;
    (void)lParam;
    g_windows_seen++;
    return TRUE;
}

static LRESULT CALLBACK WndProc(HWND hwnd, UINT msg, WPARAM wParam, LPARAM lParam) {
    return DefWindowProcW(hwnd, msg, wParam, lParam);
}

int main(void) {
    WNDCLASSEXW wc;
    HWND hwnd;
    POINT pt;

    printf("user32_enum: RegisterClassExW(WIEEnumTest)...\n");
    memset(&wc, 0, sizeof(wc));
    wc.cbSize = sizeof(wc);
    wc.lpfnWndProc = WndProc;
    wc.hInstance = GetModuleHandleW(NULL);
    wc.lpszClassName = L"WIEEnumTest";
    if (RegisterClassExW(&wc) == 0) {
        printf("  FAILED\n");
        return 1;
    }

    printf("user32_enum: CreateWindowExW...\n");
    hwnd = CreateWindowExW(0, L"WIEEnumTest", L"Enum test", WS_OVERLAPPEDWINDOW,
                           0, 0, 320, 200, NULL, NULL, wc.hInstance, NULL);
    if (hwnd == NULL) {
        printf("  FAILED\n");
        return 1;
    }
    printf("  hwnd=%p\n", (void *)hwnd);

    printf("user32_enum: EnumWindows(cb, 0)...\n");
    g_windows_seen = 0;
    if (!EnumWindows(cb, 0)) {
        printf("  FAILED (EnumWindows returned FALSE)\n");
        return 2;
    }
    printf("  enumerated %d window(s)\n", g_windows_seen);
    if (g_windows_seen < 1) {
        printf("  FAILED (the top-level window was not enumerated)\n");
        return 2;
    }

    printf("user32_enum: FindWindowW(WIEEnumTest, NULL)...\n");
    if (FindWindowW(L"WIEEnumTest", NULL) != hwnd) {
        printf("  FAILED (did not find the created window)\n");
        return 3;
    }
    printf("  ok\n");

    printf("user32_enum: caret round-trip...\n");
    if (!CreateCaret(hwnd, NULL, 2, 2)) {
        printf("  FAILED (CreateCaret)\n");
        return 4;
    }
    if (!SetCaretPos(10, 20)) {
        printf("  FAILED (SetCaretPos)\n");
        return 5;
    }
    pt.x = -1;
    pt.y = -1;
    if (!GetCaretPos(&pt)) {
        printf("  FAILED (GetCaretPos)\n");
        return 6;
    }
    if (pt.x != 10 || pt.y != 20) {
        printf("  FAILED (got %ld,%ld)\n", (long)pt.x, (long)pt.y);
        return 6;
    }
    if (!DestroyCaret()) {
        printf("  FAILED (DestroyCaret)\n");
        return 7;
    }
    printf("  ok\n");

    printf("user32_enum: DrawIcon(GetDC, 0, 0, NULL)...\n");
    {
        HDC dc = GetDC(hwnd);
        if (dc == NULL || !DrawIcon(dc, 0, 0, NULL)) {
            printf("  FAILED\n");
            return 8;
        }
        ReleaseDC(hwnd, dc);
    }
    printf("  ok\n");

    DestroyWindow(hwnd);
    printf("user32_enum: done\n");
    return 0;
}
