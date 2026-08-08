// Freestanding GUI control regression test for WIE.
//
// Exercises the "child window model" tier:
//   - CreateWindowExA with the built-in "BUTTON" and "STATIC" classes
//   - GetParent / GetDlgCtrlID / IsChild child-model round-trips
//   - SetWindowTextA / GetWindowTextA on a control (control_text buffer)
//   - Host-side control WndProc painting (button face + border + caption,
//     static text) into the ancestor's surface
//   - WS_CLIPCHILDREN: the parent's WM_PAINT BitBlt is clipped around the
//     children, so a parent repaint cannot erase the controls
//   - Programmatic click via SendMessageA(button, BM_CLICK): the host control
//     WndProc delivers WM_COMMAND(BN_CLICKED) to the parent, which destroys
//     the window — exit 0 proves the whole chain.
//
// The guest only calls PostQuitMessage from WM_DESTROY, which only happens
// after the WM_COMMAND path fires, so reaching exit code 0 proves the
// control→command→parent flow worked end-to-end.

#include <windows.h>

#define ID_BUTTON 42
#define TIMER_TICKS 5

static HWND   g_button;
static HWND   g_static;
static HDC    g_dc;
static HBITMAP g_dib;
static void  *g_bits;
static int g_timer_count = 0;
static int g_selftest;
static int g_width  = 1280;
static int g_height = 800;

// Self-test gate: the CI harness injects WIE_SELFTEST=1, which runs the
// scripted timer-driven self-click that destroys the window. Interactive runs
// keep the window open; the real button still fires WM_COMMAND on a real
// click (closing the window) and 'q' / close quit too.
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

// Create the backing DIB and fill it with the parent background color.
static int recreate_dib(void) {
    if (g_dib) {
        DeleteObject(g_dib);
        g_dib = NULL;
        g_bits = NULL;
    }
    BITMAPINFO bmi;
    bmi.bmiHeader.biSize          = sizeof(BITMAPINFOHEADER);
    bmi.bmiHeader.biWidth         = g_width;
    bmi.bmiHeader.biHeight        = -g_height;    // top-down
    bmi.bmiHeader.biPlanes        = 1;
    bmi.bmiHeader.biBitCount      = 32;
    bmi.bmiHeader.biCompression   = BI_RGB;
    bmi.bmiHeader.biSizeImage     = 0;
    bmi.bmiHeader.biXPelsPerMeter = 0;
    bmi.bmiHeader.biYPelsPerMeter = 0;
    bmi.bmiHeader.biClrUsed       = 0;
    bmi.bmiHeader.biClrImportant  = 0;

    g_dib = CreateDIBSection(g_dc, &bmi, DIB_RGB_COLORS, &g_bits, NULL, 0);
    if (g_dib == NULL) {
        return 130;
    }
    if (SelectObject(g_dc, g_dib) == NULL) {
        return 131;
    }

    // Solid medium-blue parent background (distinct from the BTNFACE button).
    unsigned int *pixels = (unsigned int *)g_bits;
    for (int i = 0; i < g_width * g_height; i++) {
        pixels[i] = 0xFF4080FF; // BGRA: R=0x40 G=0x80 B=0xFF
    }
    return 0;
}

static LRESULT CALLBACK WndProc(HWND hwnd, UINT msg, WPARAM wParam, LPARAM lParam) {
    switch (msg) {
    case WM_COMMAND: {
        int id = (int)LOWORD(wParam);
        if (id == ID_BUTTON) {
            // The simulated button click reached the parent: close.
            DestroyWindow(hwnd);
        }
        return 0;
    }

    case WM_TIMER:
        g_timer_count++;
        InvalidateRect(hwnd, NULL, FALSE);
        if (g_selftest && g_timer_count >= TIMER_TICKS) {
            // Self-test: programmatic activation through the real SendMessage
            // path. The host control WndProc turns BM_CLICK into
            // WM_COMMAND(BN_CLICKED) to this window → DestroyWindow.
            SendMessageA(g_button, BM_CLICK, 0, 0);
        }
        return 0;

    case WM_CHAR:
        if (wParam == 'q' || wParam == 'Q') {
            DestroyWindow(hwnd);
        }
        return 0;

    case WM_SIZE: {
        int w = LOWORD(lParam);
        int h = HIWORD(lParam);
        if (w > 0 && h > 0) {
            g_width = w;
            g_height = h;
            recreate_dib();
        }
        return 0;
    }

    case WM_PAINT: {
        PAINTSTRUCT ps;
        HDC hdc = BeginPaint(hwnd, &ps);
        if (g_dib) {
            // WS_CLIPCHILDREN clips this blit around the button/static, so
            // the controls survive the parent's repaint.
            BitBlt(hdc, 0, 0, g_width, g_height, g_dc, 0, 0, SRCCOPY);
        }
        EndPaint(hwnd, &ps);
        return 0;
    }

    case WM_DESTROY:
        if (g_dib) DeleteObject(g_dib);
        if (g_dc)  DeleteDC(g_dc);
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
    wc.lpszClassName = "GuiControlClass";
    wc.hIconSm       = NULL;

    ATOM atom = RegisterClassExA(&wc);
    if (atom == 0) {
        ExitProcess(140);
    }

    HWND hwnd = CreateWindowExA(
        0, "GuiControlClass", "WIE GUI Control",
        WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
        CW_USEDEFAULT, CW_USEDEFAULT, g_width, g_height,
        NULL, NULL, inst, NULL);

    if (hwnd == NULL) {
        ExitProcess(141);
    }

    // Backing DC + DIB (solid parent background).
    g_dc = CreateCompatibleDC(NULL);
    if (g_dc == NULL) {
        ExitProcess(142);
    }
    {
        int rc = recreate_dib();
        if (rc != 0) {
            ExitProcess(rc);
        }
    }

    // Child controls (built-in classes, resolved by name).
    g_button = CreateWindowExA(
        0, "BUTTON", "Click me",
        WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON,
        20, 20, 100, 30,
        hwnd, (HMENU)ID_BUTTON, inst, NULL);
    if (g_button == NULL) {
        ExitProcess(143);
    }

    g_static = CreateWindowExA(
        0, "STATIC", "Hello from STATIC",
        WS_CHILD | WS_VISIBLE,
        20, 60, 200, 30,
        hwnd, NULL, inst, NULL);
    if (g_static == NULL) {
        ExitProcess(144);
    }

    // Child-model round-trips.
    if (GetParent(g_button) != hwnd) {
        ExitProcess(145);
    }
    if (GetDlgCtrlID(g_button) != ID_BUTTON) {
        ExitProcess(146);
    }
    if (!IsChild(hwnd, g_button)) {
        ExitProcess(147);
    }
    if (IsChild(g_button, hwnd)) {
        ExitProcess(148);
    }

    // SetWindowTextA → GetWindowTextA round-trip on the button.
    if (!SetWindowTextA(g_button, "Press me")) {
        ExitProcess(149);
    }
    {
        char buf[64];
        int n = GetWindowTextA(g_button, buf, sizeof(buf));
        if (n <= 0 || !eq_str(buf, "Press me")) {
            ExitProcess(150);
        }
    }

    if (SetTimer(hwnd, 1, 50, NULL) == 0) {
        ExitProcess(151);
    }

    ShowWindow(hwnd, SW_SHOW);

    MSG msg;
    while (GetMessageA(&msg, NULL, 0, 0)) {
        TranslateMessage(&msg);
        DispatchMessageA(&msg);
    }

    // PostQuitMessage only happens from WM_DESTROY, which only happens after
    // the simulated click fired WM_COMMAND — this check proves the chain.
    // Interactive runs quit on 'q' or a real button click at any time.
    if (g_selftest && g_timer_count < TIMER_TICKS) {
        ExitProcess(152);
    }

    ExitProcess(0);
}
