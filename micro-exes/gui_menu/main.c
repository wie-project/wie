// Freestanding GUI menu + timer regression test for WIE.
//
// Exercises the "mechanical tier" of Win32 GUI coverage:
//   - CreateMenu / CreatePopupMenu / AppendMenuA (MF_POPUP + MF_STRING) /
//     SetMenu / DrawMenuBar / GetMenuState — a true hierarchical menu bar
//     (File ▸ Exit, Help ▸ About), mirrored into the macOS top bar
//   - SetTimer + WM_TIMER tick synthesis (host-clock driven)
//   - WM_COMMAND routing (IDM_EXIT -> DestroyWindow)
//   - GetClassNameA round-trip
//   - SetClassLongPtrA / GetClassLongPtrA storage
//   - GetSystemMetrics full table
//   - WM_PAINT draws a hint pointing at the menu bar (the window body
//     itself stays empty — the menus live in the macOS top bar)
//   - WM_PAINT synthesis from InvalidateRect
//
// The guest only calls PostQuitMessage after TIMER_TICKS WM_TIMER messages,
// so reaching exit code 0 proves the timer synthesis path fired.

#include <windows.h>

#define IDM_EXIT 2
#define IDM_ABOUT 3
#define TIMER_TICKS 5

static int g_timer_count = 0;
static int g_selftest;

// Self-test gate: the CI harness injects WIE_SELFTEST=1, which runs the
// scripted timer-driven auto-quit. Interactive runs keep the window open and
// quit only on 'q' / the Exit menu / close.
static int selftest_enabled(void) {
    char buf[16];
    DWORD n = GetEnvironmentVariableA("WIE_SELFTEST", buf, sizeof(buf));
    return n == 1 && buf[0] == '1';
}

// Minimal string compare (freestanding: no CRT).
static int eq_str(const char *a, const char *b) {
    while (*a && *b) {
        if (*a != *b) return 0;
        a++;
        b++;
    }
    return *a == *b;
}

static LRESULT CALLBACK WndProc(HWND hwnd, UINT msg, WPARAM wParam, LPARAM lParam) {
    switch (msg) {
    case WM_COMMAND: {
        int id = (int)LOWORD(wParam);
        if (id == IDM_EXIT) {
            DestroyWindow(hwnd);
        }
        return 0;
    }

    case WM_TIMER: {
        g_timer_count++;
        // Mark dirty; the next empty GetMessage synthesizes WM_PAINT.
        InvalidateRect(hwnd, NULL, FALSE);
        if (g_selftest && g_timer_count >= TIMER_TICKS) {
            DestroyWindow(hwnd);
        }
        return 0;
    }

    case WM_CHAR:
        if (wParam == 'q' || wParam == 'Q') {
            DestroyWindow(hwnd);
        }
        return 0;

    case WM_PAINT: {
        PAINTSTRUCT ps;
        HDC hdc = BeginPaint(hwnd, &ps);

        // The window body is deliberately empty: the menu lives in the
        // macOS top bar, mirrored from the guest's File/Help menus.
        // Just point the user at it.
        SetBkMode(hdc, TRANSPARENT);
        RECT rc = { 20, 20, 1260, 200 };
        DrawTextA(hdc, "Use the File and Help menus in the menu bar above.",
                  -1, &rc, DT_LEFT | DT_SINGLELINE);
        rc.top = 44;
        DrawTextA(hdc, "File > Exit closes the window.", -1, &rc,
                  DT_LEFT | DT_SINGLELINE);

        EndPaint(hwnd, &ps);
        return 0;
    }

    case WM_DESTROY:
        PostQuitMessage(0);
        return 0;
    }

    return DefWindowProcA(hwnd, msg, wParam, lParam);
}

void entry(void) {
    HINSTANCE inst = GetModuleHandleA(NULL);
    g_selftest = selftest_enabled();

    WNDCLASSEXA wc;
    wc.cbSize        = sizeof(WNDCLASSEXA);
    wc.style         = CS_HREDRAW | CS_VREDRAW;
    wc.lpfnWndProc   = WndProc;
    wc.cbClsExtra    = 0;
    wc.cbWndExtra    = 0;
    wc.hInstance     = inst;
    wc.hIcon         = NULL;
    wc.hCursor       = NULL;
    wc.hbrBackground = (HBRUSH)(COLOR_WINDOW + 1);
    wc.lpszMenuName  = NULL;
    wc.lpszClassName = "GuiMenuClass";
    wc.hIconSm       = NULL;

    ATOM atom = RegisterClassExA(&wc);
    if (atom == 0) {
        ExitProcess(101);
    }

    HWND hwnd = CreateWindowExA(
        0, "GuiMenuClass", "WIE GUI Menu",
        WS_OVERLAPPEDWINDOW,
        CW_USEDEFAULT, CW_USEDEFAULT, 1280, 800,
        NULL, NULL, inst, NULL);

    if (hwnd == NULL) {
        ExitProcess(102);
    }

    // GetClassNameA must round-trip the registered class name.
    {
        char name[64];
        int len = GetClassNameA(hwnd, name, sizeof(name));
        if (len <= 0 || !eq_str(name, "GuiMenuClass")) {
            ExitProcess(103);
        }
    }

    // SetClassLongPtrA / GetClassLongPtrA storage round-trip.
    {
        SetClassLongPtrA(hwnd, GCL_CBWNDEXTRA, 0x12345678);
        LONG_PTR now = GetClassLongPtrA(hwnd, GCL_CBWNDEXTRA);
        if (now != 0x12345678) {
            ExitProcess(104);
        }
    }

    // GetSystemMetrics must return a sane screen width.
    if (GetSystemMetrics(SM_CXSCREEN) <= 0) {
        ExitProcess(105);
    }

    // True hierarchical menu bar: File ▸ Exit, Help ▸ About. Each popup is
    // created separately and appended with MF_POPUP (its id is the submenu
    // handle); the host mirrors top-level popups into the macOS menu bar.
    HMENU menu = CreateMenu();
    if (menu == NULL) {
        ExitProcess(106);
    }
    HMENU file_menu = CreatePopupMenu();
    HMENU help_menu = CreatePopupMenu();
    if (file_menu == NULL || help_menu == NULL) {
        ExitProcess(107);
    }
    AppendMenuA(file_menu, MF_STRING, IDM_EXIT, "Exit");
    AppendMenuA(help_menu, MF_STRING, IDM_ABOUT, "About");
    AppendMenuA(menu, MF_POPUP, (UINT_PTR)file_menu, "File");
    AppendMenuA(menu, MF_POPUP, (UINT_PTR)help_menu, "Help");
    SetMenu(hwnd, menu);
    DrawMenuBar(hwnd);

    // The stored items must be visible through GetMenuState (queried on the
    // popup they live in).
    if (GetMenuState(file_menu, IDM_EXIT, MF_BYCOMMAND) == (UINT)-1) {
        ExitProcess(108);
    }
    if (GetMenuState(help_menu, IDM_ABOUT, MF_BYCOMMAND) == (UINT)-1) {
        ExitProcess(109);
    }

    if (SetTimer(hwnd, 1, 50, NULL) == 0) {
        ExitProcess(110);
    }

    ShowWindow(hwnd, SW_SHOW);

    MSG msg;
    while (GetMessageA(&msg, NULL, 0, 0)) {
        TranslateMessage(&msg);
        DispatchMessageA(&msg);
    }

    // PostQuitMessage only happens from WM_DESTROY, which only happens after
    // TIMER_TICKS WM_TIMER messages — so this check proves timers fired.
    // Interactive runs quit on 'q'/close at any time, so the check applies
    // only under self-test.
    if (g_selftest && g_timer_count < TIMER_TICKS) {
        ExitProcess(111);
    }

    ExitProcess(0);
}
