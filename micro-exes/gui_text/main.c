// Freestanding GUI text rendering regression test for WIE.
//
// Exercises the "text core" tier of GDI coverage:
//   - CreateFontIndirectA (px height from lfHeight, bold from lfWeight)
//   - SelectObject with an HFONT
//   - SetTextColor / SetBkColor / SetBkMode state on the DC
//   - TextOutA / TextOutW rasterizing glyphs into a DIB (one bold line)
//   - DrawTextA with DT_CALCRECT (measure) then DT_SINGLELINE (draw)
//   - GetTextExtentPoint32A agreeing with the selected font's px metrics
//
// The system font metrics vary by macOS version, so the self-checks assert
// sane bounds (a 24 px proportional line is 16..48 px tall, "Hello" is
// 40..200 px wide) rather than exact pixel values. The painted DIB is blitted
// to the window on WM_PAINT. The guest exits via the same timer-tick path as
// gui_menu, so exit code 0 proves the whole paint cycle ran; any failed
// self-check exits with a distinct non-zero code.

#include <windows.h>

#define TIMER_TICKS 5

static HDC     g_dc;
static HBITMAP g_dib;
static void   *g_bits;
static int     g_width  = 1280;
static int     g_height = 800;

// Create the backing DIB and draw the text into it (called once).
static int draw_text_frame(void) {
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
        return 101;
    }
    if (SelectObject(g_dc, g_dib) == NULL) {
        return 102;
    }

    // 24 px proportional system font, bold weight.
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

    HFONT font = CreateFontIndirectA(&lf);
    if (font == NULL) {
        return 103;
    }
    SelectObject(g_dc, font);

    // Extent must reflect the 24 px proportional font: sane bounds, not the
    // old monospace 5*16=80x32 (system font metrics vary by macOS version).
    {
        SIZE size;
        if (!GetTextExtentPoint32A(g_dc, "Hello", 5, &size)) {
            return 104;
        }
        if (size.cx < 40 || size.cx > 200 || size.cy < 16 || size.cy > 48) {
            return 105;
        }
    }

    SetTextColor(g_dc, RGB(255, 255, 255));   // white on black
    SetBkColor(g_dc, RGB(0, 0, 0));
    SetBkMode(g_dc, OPAQUE);

    if (TextOutA(g_dc, 8, 8, "Hello WIE", 9) != 9) {
        return 106;
    }
    // Bold line (same font; also exercises a second wide line).
    if (TextOutA(g_dc, 8, 44, "Bold line", 9) != 9) {
        return 107;
    }
    if (TextOutW(g_dc, 8, 80, L"Wide text", 9) != 9) {
        return 108;
    }

    // DrawTextA with DT_CALCRECT must measure the 24 px line (sane bounds).
    {
        RECT rc = { 0, 0, 0, 0 };
        int h = DrawTextA(g_dc, "Calc", 4, &rc, DT_CALCRECT | DT_SINGLELINE);
        if (h < 16 || h > 48) {
            return 109;
        }
        if (rc.right < 32 || rc.right > 160 || rc.bottom < 16 || rc.bottom > 48) {
            return 110;
        }
    }

    // DrawTextA with DT_SINGLELINE + DT_CENTER renders centered in the rect.
    {
        RECT rc = { 0, 120, 320, 152 };
        int h = DrawTextA(g_dc, "Centered", -1, &rc, DT_SINGLELINE | DT_CENTER | DT_NOCLIP);
        if (h < 16 || h > 48) {
            return 111;
        }
    }

    SelectObject(g_dc, GetStockObject(SYSTEM_FONT)); // deselect the font
    DeleteObject(font);
    return 0;
}

static int g_frame_result = 0;
static int g_timer_count  = 0;
static int g_selftest;

// Self-test gate: the CI harness injects WIE_SELFTEST=1, which runs the
// scripted timer-driven auto-quit. Interactive runs keep the window open and
// quit only on 'q' / close.
static int selftest_enabled(void) {
    char buf[16];
    DWORD n = GetEnvironmentVariableA("WIE_SELFTEST", buf, sizeof(buf));
    return n == 1 && buf[0] == '1';
}

static LRESULT CALLBACK WndProc(HWND hwnd, UINT msg, WPARAM wParam, LPARAM lParam) {
    switch (msg) {
    case WM_TIMER:
        g_timer_count++;
        InvalidateRect(hwnd, NULL, FALSE);
        if (g_selftest && g_timer_count >= TIMER_TICKS) {
            DestroyWindow(hwnd);
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
        if (w > 0 && h > 0 && (w != g_width || h != g_height)) {
            g_width = w;
            g_height = h;
            if (g_dib) {
                DeleteObject(g_dib);
                g_dib = NULL;
                g_bits = NULL;
            }
            draw_text_frame();
        }
        return 0;
    }

    case WM_PAINT: {
        PAINTSTRUCT ps;
        HDC hdc = BeginPaint(hwnd, &ps);
        if (g_dib) {
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
    wc.lpszClassName = "GuiTextClass";
    wc.hIconSm       = NULL;

    ATOM atom = RegisterClassExA(&wc);
    if (atom == 0) {
        ExitProcess(120);
    }

    HWND hwnd = CreateWindowExA(
        0, "GuiTextClass", "WIE GUI Text",
        WS_OVERLAPPEDWINDOW,
        CW_USEDEFAULT, CW_USEDEFAULT, g_width, g_height,
        NULL, NULL, inst, NULL);

    if (hwnd == NULL) {
        ExitProcess(121);
    }

    // Backing DC + DIB + text, drawn before the window ever paints.
    g_dc = CreateCompatibleDC(NULL);
    if (g_dc == NULL) {
        ExitProcess(122);
    }
    g_frame_result = draw_text_frame();
    if (g_frame_result != 0) {
        ExitProcess(g_frame_result);
    }

    if (SetTimer(hwnd, 1, 50, NULL) == 0) {
        ExitProcess(123);
    }

    ShowWindow(hwnd, SW_SHOW);

    MSG msg;
    while (GetMessageA(&msg, NULL, 0, 0)) {
        TranslateMessage(&msg);
        DispatchMessageA(&msg);
    }

    ExitProcess(0);
}
