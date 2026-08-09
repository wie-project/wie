/*
 * Micro-PE COMCTL32: InitCommonControlsEx(ICC_BAR_CLASSES) + a real
 * CreateToolbarEx under a created parent window.
 *
 * CRT-linked console program (printf) — see micro-exes/ws2_echo/main.c.
 * The guest drives the host's comctl32 handlers
 * (crates/wie-winapi/src/comctl32.rs): the toolbar is created as a
 * ToolbarWindow32 child of the parent and both windows are destroyed. There
 * is deliberately no message loop — the run_micro_exe harness drives the
 * guest straight to ExitProcess, so "done" proves the whole create/destroy
 * chain ran without a single GetMessage.
 *
 * Exit codes:
 *   0 — ok
 *   1 — InitCommonControlsEx failed
 *   2 — parent window creation failed (RegisterClassExW or CreateWindowExW)
 *   3 — CreateToolbarEx returned NULL
 *   4 — DestroyWindow(toolbar) failed
 *   5 — DestroyWindow(parent) failed
 */

#include <windows.h>
#include <commctrl.h>
#include <stdio.h>

static LRESULT CALLBACK ParentWndProc(HWND hwnd, UINT msg, WPARAM wParam, LPARAM lParam) {
    return DefWindowProcW(hwnd, msg, wParam, lParam);
}

int main(void) {
    HINSTANCE inst;

    printf("comctl_toolbar: InitCommonControlsEx(ICC_BAR_CLASSES)...\n");
    {
        INITCOMMONCONTROLSEX icc;
        icc.dwSize = sizeof(icc);
        icc.dwICC  = ICC_BAR_CLASSES;
        if (!InitCommonControlsEx(&icc)) {
            printf("  FAILED\n");
            return 1;
        }
    }
    printf("  ok\n");

    inst = GetModuleHandleA(NULL);

    printf("comctl_toolbar: RegisterClassExW + CreateWindowExW parent...\n");
    {
        WNDCLASSEXW wc;
        wc.cbSize        = sizeof(wc);
        wc.style         = 0;
        wc.lpfnWndProc   = ParentWndProc;
        wc.cbClsExtra    = 0;
        wc.cbWndExtra    = 0;
        wc.hInstance     = inst;
        wc.hIcon         = NULL;
        wc.hCursor       = NULL;
        wc.hbrBackground = (HBRUSH)(COLOR_WINDOW + 1);
        wc.lpszMenuName  = NULL;
        wc.lpszClassName = L"ComctlToolbarParent";
        wc.hIconSm       = NULL;
        if (RegisterClassExW(&wc) == 0) {
            printf("  FAILED (RegisterClassExW)\n");
            return 2;
        }
    }

    HWND parent = CreateWindowExW(
        0, L"ComctlToolbarParent", L"WIE Comctl Toolbar",
        WS_OVERLAPPEDWINDOW,
        CW_USEDEFAULT, CW_USEDEFAULT, 320, 200,
        NULL, NULL, inst, NULL);
    if (parent == NULL) {
        printf("  FAILED (CreateWindowExW)\n");
        return 2;
    }
    printf("  ok (parent=%p)\n", (void *)parent);

    printf("comctl_toolbar: CreateToolbarEx(WS_CHILD|WS_VISIBLE)...\n");
    HWND toolbar = CreateToolbarEx(
        parent,                  /* hwnd parent */
        WS_CHILD | WS_VISIBLE,   /* ws */
        1,                       /* wID */
        0,                       /* nBitmaps (no bitmap strip) */
        NULL,                    /* hBMInst */
        0,                       /* wBMID */
        NULL,                    /* lpButtons */
        0,                       /* iNumButtons */
        0, 0,                    /* dxButton, dyButton */
        0, 0,                    /* dxBitmap, dyBitmap */
        sizeof(TBBUTTON));       /* uStructSize */
    if (toolbar == NULL) {
        printf("  FAILED (CreateToolbarEx)\n");
        return 3;
    }
    printf("  ok (toolbar=%p)\n", (void *)toolbar);

    if (!DestroyWindow(toolbar)) {
        printf("  FAILED (DestroyWindow toolbar)\n");
        return 4;
    }
    if (!DestroyWindow(parent)) {
        printf("  FAILED (DestroyWindow parent)\n");
        return 5;
    }

    printf("comctl_toolbar: done\n");
    return 0;
}
