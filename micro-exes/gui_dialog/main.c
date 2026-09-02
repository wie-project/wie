// Freestanding modal-dialog regression test for WIE.
//
// Exercises the "dialog machinery" tier:
//   - RT_DIALOG resource parsing of the MAIN EXE module (the exe carries
//     dialog.rc id 100; DialogBoxParamA resolves hInstance == image base)
//   - the in-guest DialogBoxParam modal stub (CreateDialogParam → WM_INITDIALOG
//     → GetMessage loop → IsDialogMessage → DispatchMessage → EndDialog)
//   - template → window/control creation (two PUSHBUTTONs + one LTEXT)
//   - WM_INITDIALOG with SetDlgItemTextA + GetDlgItemTextA round-trip
//   - IsDialogMessage Enter → WM_COMMAND(IDOK) → EndDialog(1) → result slot
//   - dialog compositing into the owner's present surface (WS_CLIPCHILDREN)
//
// Flow: entry() opens the modal dialog immediately (before the message loop)
// so the headless screenshot path captures it over the owner. The timer keeps
// firing; the modal loop's DispatchMessage runs the owner's timer handler,
// which posts VK_RETURN after TIMER_TICKS — the stub's IsDialogMessage turns
// that into WM_COMMAND(IDOK), the dialog proc calls EndDialog(1), and
// DialogBoxParam returns 1. Exit code 0 proves the whole chain (result == 1
// AND the WM_INITDIALOG sentinel ran).

#include <windows.h>

#define TIMER_TICKS 5
#define IDC_DLG_STATIC 1001

static HWND     g_dlg;
static int      g_dlg_open;
static int g_init_sentinel;
static int g_timer_count;
static int g_selftest;
static int g_host_driven;
static HDC      g_dc;
static HBITMAP  g_dib;
static void    *g_bits;
static int      g_width  = 1280;
static int      g_height = 800;

// Minimal string compare (freestanding: no CRT).
static int eq_str(const char *a, const char *b) {
    while (*a && *b) {
        if (*a != *b) return 0;
        a++;
        b++;
    }
    return *a == *b;
}

// Self-test gate: the CI harness injects WIE_SELFTEST=1, which auto-closes
// the dialog with a posted VK_RETURN after TIMER_TICKS. Interactive runs keep
// the dialog open; the user closes it with Enter/Esc/OK/Cancel through the
// normal IsDialogMessage path.
// WIE_DIALOG_HOSTDRIVEN=1 additionally DISABLES the timer auto-close: the
// host test drives Shift+Tab/Tab focus transitions and posts its own
// VK_RETURN once focus is verified back on the first tab stop. Without this
// the auto-close ENTER can land while focus sits on Cancel (mid-transition),
// clicking IDCANCEL instead of IDOK — a race, not a focus bug.
static int selftest_enabled(void) {
    char buf[16];
    DWORD n = GetEnvironmentVariableA("WIE_SELFTEST", buf, sizeof(buf));
    return n == 1 && buf[0] == '1';
}

static int host_driven_enabled(void) {
    char buf[16];
    DWORD n = GetEnvironmentVariableA("WIE_DIALOG_HOSTDRIVEN", buf, sizeof(buf));
    return n == 1 && buf[0] == '1';
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

    // Solid medium-blue parent background (distinct from BTNFACE dialogs).
    unsigned int *pixels = (unsigned int *)g_bits;
    for (int i = 0; i < g_width * g_height; i++) {
        pixels[i] = 0xFF4080FF; // BGRA: R=0x40 G=0x80 B=0xFF
    }
    return 0;
}

static INT_PTR CALLBACK DlgProc(HWND hwnd, UINT msg, WPARAM wParam, LPARAM lParam) {
    (void)lParam;
    switch (msg) {
    case WM_INITDIALOG:
        // Sentinel + capture the dialog handle for the timer-driven close.
        g_dlg = hwnd;
        g_init_sentinel = 1;
        // GetDlgItem + SetDlgItemText into the template LTEXT (id 1001).
        if (!SetDlgItemTextA(hwnd, IDC_DLG_STATIC, "Dialog static set")) {
            ExitProcess(160);
        }
        {
            char buf[64];
            if (GetDlgItemTextA(hwnd, IDC_DLG_STATIC, buf, sizeof(buf)) <= 0
                || !eq_str(buf, "Dialog static set")) {
                ExitProcess(161);
            }
        }
        return TRUE;

    case WM_COMMAND:
        switch ((int)LOWORD(wParam)) {
        case IDOK:
            EndDialog(hwnd, 1);
            return TRUE;
        case IDCANCEL:
            EndDialog(hwnd, 2);
            return TRUE;
        }
        return FALSE;
    }
    return FALSE;
}

static LRESULT CALLBACK WndProc(HWND hwnd, UINT msg, WPARAM wParam, LPARAM lParam) {
    switch (msg) {
    case WM_TIMER:
        g_timer_count++;
        InvalidateRect(hwnd, NULL, FALSE);
        if (g_selftest && !g_host_driven && g_dlg_open && g_dlg && g_timer_count >= TIMER_TICKS) {
            // The modal loop is running inside DialogBoxParam below: drive
            // the dialog headlessly. Posting VK_RETURN exercises IsDialogMessage
            // (Enter → WM_COMMAND(IDOK) → EndDialog(1)) in the in-guest stub
            // loop, exactly like pressing Enter in the dialog. Interactive
            // runs skip this so the dialog stays open for the user.
            PostMessageA(g_dlg, WM_KEYDOWN, VK_RETURN, 0);
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
            // WS_CLIPCHILDREN clips around the dialog, so a parent repaint
            // cannot erase the dialog/controls.
            BitBlt(hdc, 0, 0, g_width, g_height, g_dc, 0, 0, SRCCOPY);
        }
        EndPaint(hwnd, &ps);
        return 0;
    }

    case WM_DESTROY:
        if (g_dib) DeleteObject(g_dib);
        if (g_dc)  DeleteDC(g_dc);
        PostQuitMessage(1);
        return 0;
    }

    return DefWindowProcA(hwnd, msg, wParam, lParam);
}

void entry(void) {
    HINSTANCE inst = GetModuleHandleA(NULL);
    g_selftest = selftest_enabled();
    g_host_driven = host_driven_enabled();

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
    wc.lpszClassName = "GuiDialogClass";
    wc.hIconSm       = NULL;

    ATOM atom = RegisterClassExA(&wc);
    if (atom == 0) {
        ExitProcess(140);
    }

    HWND hwnd = CreateWindowExA(
        0, "GuiDialogClass", "WIE GUI Dialog",
        WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
        CW_USEDEFAULT, CW_USEDEFAULT, g_width, g_height,
        NULL, NULL, inst, NULL);

    if (hwnd == NULL) {
        ExitProcess(141);
    }

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

    if (SetTimer(hwnd, 1, 50, NULL) == 0) {
        ExitProcess(151);
    }

    ShowWindow(hwnd, SW_SHOW);

    // Open the modal dialog immediately (before the message loop) so the
    // headless --screenshot path captures it over the owner. The timer fires
    // inside the modal loop and posts VK_RETURN to close it after TIMER_TICKS.
    g_dlg_open = 1;
    INT_PTR result = DialogBoxParamA(inst, (LPCSTR)100, hwnd, DlgProc, 0);
    g_dlg_open = 0;

    // Exit code 0 only if DialogBoxParam returned 1 AND the WM_INITDIALOG
    // sentinel ran — this check proves the dialog flow end-to-end.
    if (result == 1 && g_init_sentinel == 1) {
        PostQuitMessage(0);
    } else {
        ExitProcess(170 + (int)result);
    }

    MSG msg;
    while (GetMessageA(&msg, NULL, 0, 0)) {
        TranslateMessage(&msg);
        DispatchMessageA(&msg);
    }

    // The timer drove the close inside the modal loop; it must have fired.
    // Interactive runs may close the dialog (Enter/Esc) before any ticks.
    // Host-driven runs close on the HOST's VK_RETURN (no auto-close timer),
    // so the tick count carries no signal.
    if (g_selftest && !g_host_driven && g_timer_count < TIMER_TICKS) {
        ExitProcess(152);
    }

    ExitProcess(0);
}
