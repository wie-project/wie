// Comprehensive GUI regression test for WIE.
//
// Keeps the original core (the resize/BitBlt regression): a deterministic
// gradient-pattern DIB, recreated on WM_CREATE/WM_SIZE, blitted on WM_PAINT,
// quitting on 'q' / close. On top of that it exercises every GUI capability
// tier, each proven by the exit code:
//   - Text:   memory-DC double buffer — CreateFontIndirectA + SelectObject +
//             TextOutA/DrawTextA render into the DIB, plus a
//             GetTextExtentPoint32A + DrawTextA DT_CALCRECT self-check
//   - Timer:  SetTimer(50 ms); WM_TIMER increments a counter and drives the
//             self-test sequence below
//   - Menu:   CreateMenu + AppendMenuA("&About"/"&Quit") + SetMenu +
//             GetMenu/GetMenuState storage checks; WM_COMMAND 101 opens the
//             About dialog, WM_COMMAND 100 quits through the menu path
//   - Control: a child BUTTON (id 42) + STATIC; SetWindowTextA/GetWindowTextA
//             round-trip; at timer tick 3 the guest sends BM_CLICK to the
//             button expecting WM_COMMAND(42)
//   - Dialog: the About command opens DialogBoxParamA (template id 100:
//             OK + Cancel + LTEXT). WM_INITDIALOG does a
//             SetDlgItemTextA/GetDlgItemTextA round-trip; at timer tick 7 the
//             guest posts VK_RETURN to the dialog so IsDialogMessage delivers
//             WM_COMMAND(IDOK) → EndDialog(1). After the dialog returns 1 the
//             guest sends WM_COMMAND(100) to quit via the menu path.
//
// Exit code 0 only if ALL of: >= TIMER_TICKS_MIN timer ticks, the control
// click delivered WM_COMMAND(42), the About command opened the dialog, the
// dialog returned 1, and the Quit command routed. Distinct non-zero codes
// (150-154) report the first stage that did not run; setup failures exit
// 101-161 directly. The frame painted before any timer tick is deterministic
// (gradient + text + child controls; no counter text is rendered) and its
// hash is CI-gated in micro_gui_window.rs.

#include <windows.h>

#define ID_BUTTON       42
#define ID_MENU_ABOUT   101
#define ID_MENU_QUIT    100
#define IDC_DLG_STATIC  1001

#define TIMER_TICKS_MIN     5
#define TICK_BUTTON_CLICK   3
#define TICK_OPEN_DIALOG    4
#define TICK_DLG_CLOSE      7

static HINSTANCE g_inst;
static HDC      g_dc;
static HBITMAP  g_dib;
static void    *g_bits;
static int      g_width  = 1280;
static int      g_height = 800;

static HFONT  g_font;
static HWND   g_button;
static HWND   g_static;
static HWND   g_dlg;
static int    g_dlg_open;

static int g_timer_count;
static int g_click_ok;
static int g_about_ok;
static int g_dialog_ok;
static int g_menu_quit_ok;
static int g_selftest;

// Self-test gate: the CI harness injects WIE_SELFTEST=1, which runs the
// scripted timer-driven sequence below and auto-quits. Interactive runs (var
// absent or != "1") keep the window open for the user; the timer drives
// nothing and the window quits only on 'q' / close.
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

// Write the deterministic gradient pattern into the (already selected) DIB.
static void write_gradient(int width, int height) {
    unsigned int *pixels = (unsigned int *)g_bits;
    for (int y = 0; y < height; y++) {
        for (int x = 0; x < width; x++) {
            unsigned char r = (unsigned char)((x * 255) / width);
            unsigned char g = (unsigned char)((y * 255) / height);
            unsigned char b = (unsigned char)(((x + y) * 127) / (width + height));
            // Write BGRA (Windows DIB format).
            pixels[y * width + x] = b | (g << 8) | (r << 16) | (0xFF << 24);
        }
    }
}

// Draw the deterministic text block into the DIB (after the gradient).
// A no-op until setup_text() has installed the font; called again from
// recreate_dib so a WM_SIZE re-creation keeps the text.
static int render_text_into_dib(void) {
    if (g_dib == NULL || g_font == NULL) {
        return 0;
    }
    if (TextOutA(g_dc, 8, 8, "Hello WIE", 9) != 9) {
        return 126;
    }
    if (TextOutA(g_dc, 8, 44, "Blit frame", 10) != 10) {
        return 127;
    }
    {
        RECT rc = { 8, 80, 320, 112 };
        int h = DrawTextA(g_dc, "Draw text", 9, &rc, DT_SINGLELINE | DT_NOCLIP);
        if (h < 16 || h > 48) {
            return 128;
        }
    }
    return 0;
}

// Recreate the DIB at a new size and rewrite the deterministic pattern.
// Used by both WM_CREATE (initial) and WM_SIZE (resize).
static int recreate_dib(int width, int height) {
    if (g_dib) {
        DeleteObject(g_dib);
        g_dib = NULL;
        g_bits = NULL;
    }
    g_width  = width;
    g_height = height;

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
        return 111;
    }
    if (SelectObject(g_dc, g_dib) == NULL) {
        return 112;
    }

    write_gradient(g_width, g_height);
    (void)render_text_into_dib();
    return 0;
}

// Create the 24 px bold system font, select it into g_dc, and run the text
// self-checks (extent and DrawText measure must match the proportional font's
// px metrics within sane bounds — system fonts vary by macOS version).
static int setup_text(void) {
    LOGFONTA lf;
    lf.lfHeight         = 24;
    lf.lfWidth          = 0;
    lf.lfEscapement     = 0;
    lf.lfOrientation    = 0;
    lf.lfWeight         = 700;      // FW_BOLD
    lf.lfItalic         = 0;
    lf.lfUnderline      = 0;
    lf.lfStrikeOut      = 0;
    lf.lfCharSet        = ANSI_CHARSET;
    lf.lfOutPrecision   = OUT_DEFAULT_PRECIS;
    lf.lfClipPrecision  = CLIP_DEFAULT_PRECIS;
    lf.lfQuality        = DEFAULT_QUALITY;
    lf.lfPitchAndFamily = FIXED_PITCH | FF_MODERN;
    lf.lfFaceName[0]    = 0;

    g_font = CreateFontIndirectA(&lf);
    if (g_font == NULL) {
        return 120;
    }
    if (SelectObject(g_dc, g_font) == NULL) {
        return 121;
    }

    // Extent must reflect the 24 px proportional font (sane bounds; system
    // font metrics vary by macOS version).
    {
        SIZE size;
        if (!GetTextExtentPoint32A(g_dc, "Hello", 5, &size)) {
            return 122;
        }
        if (size.cx < 40 || size.cx > 200 || size.cy < 16 || size.cy > 48) {
            return 123;
        }
    }

    // DrawTextA with DT_CALCRECT must measure the 24 px line.
    {
        RECT rc = { 0, 0, 0, 0 };
        int h = DrawTextA(g_dc, "Calc", 4, &rc, DT_CALCRECT | DT_SINGLELINE);
        if (h < 16 || h > 48) {
            return 124;
        }
        if (rc.right < 32 || rc.right > 160 || rc.bottom < 16 || rc.bottom > 48) {
            return 125;
        }
    }

    SetTextColor(g_dc, RGB(255, 255, 255));   // white on black
    SetBkColor(g_dc, RGB(0, 0, 0));
    SetBkMode(g_dc, OPAQUE);

    return render_text_into_dib();
}

static INT_PTR CALLBACK DlgProc(HWND hwnd, UINT msg, WPARAM wParam, LPARAM lParam) {
    (void)lParam;
    switch (msg) {
    case WM_INITDIALOG:
        // Sentinel + capture the dialog handle for the timer-driven close.
        g_dlg = hwnd;
        // GetDlgItem + SetDlgItemText into the template LTEXT (id 1001).
        if (!SetDlgItemTextA(hwnd, IDC_DLG_STATIC, "About WIE blit")) {
            ExitProcess(160);
        }
        {
            char buf[64];
            if (GetDlgItemTextA(hwnd, IDC_DLG_STATIC, buf, sizeof(buf)) <= 0
                || !eq_str(buf, "About WIE blit")) {
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
    case WM_CREATE: {
        // Create a compatible DC and a 32-bpp top-down DIB.
        g_dc = CreateCompatibleDC(NULL);
        if (g_dc == NULL) {
            ExitProcess(110);
        }
        {
            int rc = recreate_dib(g_width, g_height);
            if (rc != 0) {
                ExitProcess(rc);
            }
        }
        return 0;
    }

    case WM_SIZE: {
        // lParam = MAKELPARAM(cx, cy) — new client size.
        int w = LOWORD(lParam);
        int h = HIWORD(lParam);
        if (w > 0 && h > 0) {
            (void)recreate_dib(w, h);
        }
        return 0;
    }

    case WM_COMMAND: {
        int id = (int)LOWORD(wParam);
        if (id == ID_BUTTON) {
            // The simulated control click reached the parent.
            g_click_ok = 1;
            return 0;
        }
        if (id == ID_MENU_QUIT) {
            // Menu "Quit" routed: leave through the menu path.
            g_menu_quit_ok = 1;
            DestroyWindow(hwnd);
            return 0;
        }
        if (id == ID_MENU_ABOUT) {
            // Menu "About" routed: open the modal dialog. The in-guest
            // DialogBoxParam stub runs the modal loop here; the owner's
            // WM_TIMER keeps dispatching inside it and posts VK_RETURN after
            // TICK_DLG_CLOSE ticks so IsDialogMessage delivers
            // WM_COMMAND(IDOK) → EndDialog(1).
            g_about_ok = 1;
            g_dlg_open = 1;
            INT_PTR result = DialogBoxParamA(g_inst, (LPCSTR)100, hwnd, DlgProc, 0);
            g_dlg_open = 0;
            if (result == 1) {
                g_dialog_ok = 1;
                // Every stage proven: quit through the menu Quit path.
                SendMessageA(hwnd, WM_COMMAND, (WPARAM)ID_MENU_QUIT, 0);
            }
            return 0;
        }
        return 0;
    }

    case WM_TIMER:
        if (!g_selftest) {
            // Interactive: the timer drives nothing — no tick counter, no
            // self-click, no dialog auto-close, no auto-quit. The About menu
            // still opens the dialog for the user; only they close it.
            return 0;
        }
        g_timer_count++;
        InvalidateRect(hwnd, NULL, FALSE);
        if (g_timer_count == TICK_BUTTON_CLICK) {
            // Self-test: programmatic activation through the real SendMessage
            // path. The host control WndProc turns BM_CLICK into
            // WM_COMMAND(BN_CLICKED, id 42) to this window.
            SendMessageA(g_button, BM_CLICK, 0, 0);
        } else if (g_timer_count == TICK_OPEN_DIALOG) {
            // Open the About dialog through the menu's WM_COMMAND path.
            SendMessageA(hwnd, WM_COMMAND, (WPARAM)ID_MENU_ABOUT, 0);
        }
        if (g_dlg_open && g_dlg && g_timer_count >= TICK_DLG_CLOSE) {
            // Drive the dialog headlessly: posting VK_RETURN exercises
            // IsDialogMessage (Enter → WM_COMMAND(IDOK) → EndDialog(1)).
            PostMessageA(g_dlg, WM_KEYDOWN, VK_RETURN, 0);
        }
        return 0;

    case WM_PAINT: {
        PAINTSTRUCT ps;
        HDC hdc = BeginPaint(hwnd, &ps);

        HDC hdcMem = CreateCompatibleDC(hdc);
        if (g_dib) {
            SelectObject(hdcMem, g_dib);
            BitBlt(hdc, 0, 0, g_width, g_height, hdcMem, 0, 0, SRCCOPY);
        }
        DeleteDC(hdcMem);

        EndPaint(hwnd, &ps);
        return 0;
    }

    case WM_CHAR:
        if (wParam == 'q' || wParam == 'Q') {
            DestroyWindow(hwnd);
        }
        return 0;

    case WM_DESTROY:
        // Exit code 0 only if every stage ran; distinct codes name the
        // first stage that did not.
        {
            int code = 0;
            if (g_timer_count < TIMER_TICKS_MIN) {
                code = 150;
            } else if (!g_click_ok) {
                code = 151;
            } else if (!g_about_ok) {
                code = 152;
            } else if (!g_dialog_ok) {
                code = 153;
            } else if (!g_menu_quit_ok) {
                code = 154;
            }
            PostQuitMessage(code);
        }
        if (g_dib) DeleteObject(g_dib);
        if (g_dc)  DeleteDC(g_dc);
        return 0;
    }

    return DefWindowProcA(hwnd, msg, wParam, lParam);
}

void entry(void) {
    g_inst = GetModuleHandleA(NULL);
    g_selftest = selftest_enabled();

    WNDCLASSEXA wc;
    wc.cbSize        = sizeof(WNDCLASSEXA);
    wc.style         = CS_HREDRAW | CS_VREDRAW;
    wc.lpfnWndProc   = WndProc;
    wc.cbClsExtra    = 0;
    wc.cbWndExtra    = 0;
    wc.hInstance     = g_inst;
    wc.hIcon         = NULL;
    wc.hCursor       = NULL;
    wc.hbrBackground = (HBRUSH)(COLOR_WINDOW + 1);
    wc.lpszMenuName  = NULL;
    wc.lpszClassName = "GuiBlitClass";
    wc.hIconSm       = NULL;

    ATOM atom = RegisterClassExA(&wc);
    if (atom == 0) {
        ExitProcess(101);
    }

    HWND hwnd = CreateWindowExA(
        0, "GuiBlitClass", "WIE GUI Blit",
        WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
        CW_USEDEFAULT, CW_USEDEFAULT, g_width, g_height,
        NULL, NULL, g_inst, NULL);

    if (hwnd == NULL) {
        ExitProcess(102);
    }

    // Text tier: font + self-checks, then draw the text block into the DIB.
    {
        int rc = setup_text();
        if (rc != 0) {
            ExitProcess(rc);
        }
    }

    // Menu tier: a two-item menu bar with storage checks.
    HMENU menu = CreateMenu();
    if (menu == NULL) {
        ExitProcess(130);
    }
    if (!AppendMenuA(menu, MF_STRING, ID_MENU_ABOUT, "&About")) {
        ExitProcess(131);
    }
    if (!AppendMenuA(menu, MF_STRING, ID_MENU_QUIT, "&Quit")) {
        ExitProcess(132);
    }
    if (!SetMenu(hwnd, menu)) {
        ExitProcess(133);
    }
    DrawMenuBar(hwnd);
    // SetMenu stores items globally; GetMenuState proves both are reachable.
    if (GetMenuState(menu, ID_MENU_QUIT, MF_BYCOMMAND) == (UINT)-1
        || GetMenuState(menu, ID_MENU_ABOUT, MF_BYCOMMAND) == (UINT)-1) {
        ExitProcess(134);
    }

    // Control tier: a child button + static at fixed positions.
    g_button = CreateWindowExA(
        0, "BUTTON", "Self Test",
        WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON,
        100, 160, 120, 30,
        hwnd, (HMENU)ID_BUTTON, g_inst, NULL);
    if (g_button == NULL) {
        ExitProcess(140);
    }

    g_static = CreateWindowExA(
        0, "STATIC", "Static label",
        WS_CHILD | WS_VISIBLE,
        20, 128, 280, 20,
        hwnd, NULL, g_inst, NULL);
    if (g_static == NULL) {
        ExitProcess(141);
    }

    // Child-model round-trips.
    if (GetParent(g_button) != hwnd) {
        ExitProcess(142);
    }
    if (GetDlgCtrlID(g_button) != ID_BUTTON) {
        ExitProcess(143);
    }
    if (!IsChild(hwnd, g_button)) {
        ExitProcess(144);
    }

    // SetWindowTextA → GetWindowTextA round-trip on the button.
    if (!SetWindowTextA(g_button, "Go!")) {
        ExitProcess(145);
    }
    {
        char buf[64];
        int n = GetWindowTextA(g_button, buf, sizeof(buf));
        if (n <= 0 || !eq_str(buf, "Go!")) {
            ExitProcess(146);
        }
    }

    if (SetTimer(hwnd, 1, 50, NULL) == 0) {
        ExitProcess(147);
    }

    ShowWindow(hwnd, SW_SHOW);

    int exit_code = 0;
    MSG msg;
    while (GetMessageA(&msg, NULL, 0, 0)) {
        TranslateMessage(&msg);
        DispatchMessageA(&msg);
    }

    // WM_DESTROY posted the proof code; 'q'/close quit early with non-zero.
    exit_code = (int)msg.wParam;

    ExitProcess(exit_code);
}
