// Freestanding multiline EDIT regression test for WIE.
//
// Exercises the EDIT control's message core (the Phase 2 notepad foundation)
// against a multiline child (WS_VSCROLL | ES_MULTILINE | ES_AUTOVSCROLL |
// ES_NOHIDESEL):
//   - SetWindowTextA seeding + EM_GETLINECOUNT
//   - EM_LINEFROMCHAR on known char indices (and the caret-relative -1 form)
//   - EM_SETSEL / EM_GETSEL round-trip (reversal normalization + select-all)
//   - EM_REPLACESEL splicing the selection
//   - EM_GETMODIFY tracking edits (clean reset → edit → dirty)
//   - WM_COPY → IsClipboardFormatAvailable(CF_TEXT) true
//   - EM_CANUNDO / EM_UNDO round-trip (single-level; SetWindowText clears undo)
//   - WM_SETTEXT resetting the buffer (line count returns to 1)
//
// Self-test (WIE_SELFTEST=1): the timer drives one assertion group per exit
// code 100-107 (matching gui_demo's step-code pattern). The first failed
// group exits with its code + 200 (300-307) so CI pinpoints the failing
// message; when every group passes the timer budget ends with a clean
// PostQuitMessage → exit 0. Interactive runs keep the window open and quit
// on 'q' / close.

#include <windows.h>

#define IDC_EDIT     1
#define TIMER_TICKS  5

static HWND g_edit;
static int  g_selftest;
static int  g_timer_count;

// Self-test gate: the CI harness injects WIE_SELFTEST=1, which runs the
// scripted timer-driven assertion suite and auto-quits. Interactive runs keep
// the window open; 'q' / close quit.
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

// The 5-line seeding text. Char layout the EM_LINEFROMCHAR checks rely on:
//   "first line\n"    chars  0..10  (line 0)
//   "second line\n"   chars 11..22  (line 1)
//   "third line\n"    chars 23..33  (line 2)
//   "fourth line\n"   chars 34..45  (line 3)
//   "fifth line"      chars 46..55  (line 4)
#define LONG_TEXT "first line\nsecond line\nthird line\nfourth line\nfifth line"

// Run the eight assertion groups in order. Returns 0 when all pass, else the
// first failed group's exit code (300..307 = group code + 200).
static int run_selftest(void) {
    char buf[128];
    DWORD sel_start = 0, sel_end = 0;

    // ── 100: SetWindowText seeds 5 lines; EM_GETLINECOUNT agrees. ──
    if (!SetWindowTextA(g_edit, LONG_TEXT)) return 300;
    if (SendMessageA(g_edit, EM_GETLINECOUNT, 0, 0) != 5) return 300;
    if (GetWindowTextA(g_edit, buf, sizeof(buf)) == 0 ||
        !eq_str(buf, LONG_TEXT)) return 300;

    // ── 101: EM_LINEFROMCHAR maps char indices to 0-based lines. ──
    if (SendMessageA(g_edit, EM_LINEFROMCHAR, 0, 0) != 0) return 301;
    if (SendMessageA(g_edit, EM_LINEFROMCHAR, 11, 0) != 1) return 301;
    if (SendMessageA(g_edit, EM_LINEFROMCHAR, 34, 0) != 3) return 301;
    if (SendMessageA(g_edit, EM_LINEFROMCHAR, 46, 0) != 4) return 301;
    if (SendMessageA(g_edit, EM_LINEFROMCHAR, 56, 0) != 4) return 301; // past-end clamp
    SendMessageA(g_edit, EM_SETSEL, 0, 0);                            // caret → 0
    if (SendMessageA(g_edit, EM_LINEFROMCHAR, (WPARAM)-1, 0) != 0) return 301;

    // ── 102: EM_SETSEL / EM_GETSEL round-trip. ──
    SendMessageA(g_edit, EM_SETSEL, 5, 12);
    sel_start = sel_end = 0;
    SendMessageA(g_edit, EM_GETSEL, (WPARAM)&sel_start, (LPARAM)&sel_end);
    if (sel_start != 5 || sel_end != 12) return 302;
    // A reversed EM_SETSEL normalizes to the same range.
    SendMessageA(g_edit, EM_SETSEL, 12, 5);
    sel_start = sel_end = 0;
    SendMessageA(g_edit, EM_GETSEL, (WPARAM)&sel_start, (LPARAM)&sel_end);
    if (sel_start != 5 || sel_end != 12) return 302;
    // (0, -1) selects the whole buffer (56 chars).
    SendMessageA(g_edit, EM_SETSEL, 0, -1);
    sel_start = sel_end = 0;
    SendMessageA(g_edit, EM_GETSEL, (WPARAM)&sel_start, (LPARAM)&sel_end);
    if (sel_start != 0 || sel_end != 56) return 302;

    // ── 103: EM_REPLACESEL splices the selection. ──
    if (!SetWindowTextA(g_edit, LONG_TEXT)) return 303;
    SendMessageA(g_edit, EM_SETSEL, 11, 22); // select "second line"
    SendMessageA(g_edit, EM_REPLACESEL, 0, (LPARAM)"S");
    if (GetWindowTextA(g_edit, buf, sizeof(buf)) == 0 ||
        !eq_str(buf, "first line\nS\nthird line\nfourth line\nfifth line")) return 303;

    // ── 104: EM_GETMODIFY tracks edits. ──
    SendMessageA(g_edit, EM_SETMODIFY, 0, 0);
    if (SendMessageA(g_edit, EM_GETMODIFY, 0, 0) != 0) return 304;
    SendMessageA(g_edit, EM_SETSEL, 0, 1);
    SendMessageA(g_edit, EM_REPLACESEL, 0, (LPARAM)"x");
    if (SendMessageA(g_edit, EM_GETMODIFY, 0, 0) != 1) return 304;
    SendMessageA(g_edit, EM_SETMODIFY, 0, 0); // restore clean for later groups

    // ── 105: WM_COPY → IsClipboardFormatAvailable(CF_TEXT). ──
    if (!SetWindowTextA(g_edit, LONG_TEXT)) return 305;
    if (IsClipboardFormatAvailable(CF_TEXT)) return 305; // empty before the copy
    SendMessageA(g_edit, EM_SETSEL, 0, -1);              // select all
    SendMessageA(g_edit, WM_COPY, 0, 0);
    if (!IsClipboardFormatAvailable(CF_TEXT)) return 305;

    // ── 106: EM_CANUNDO / EM_UNDO round-trip (single-level). ──
    if (!SetWindowTextA(g_edit, LONG_TEXT)) return 306;
    if (SendMessageA(g_edit, EM_CANUNDO, 0, 0) != 0) return 306; // SetWindowText clears undo
    SendMessageA(g_edit, EM_SETSEL, 11, 22);
    SendMessageA(g_edit, EM_REPLACESEL, 0, (LPARAM)"S");
    if (GetWindowTextA(g_edit, buf, sizeof(buf)) == 0 ||
        !eq_str(buf, "first line\nS\nthird line\nfourth line\nfifth line")) return 306;
    if (SendMessageA(g_edit, EM_CANUNDO, 0, 0) != 1) return 306; // editable change → undoable
    SendMessageA(g_edit, EM_UNDO, 0, 0);
    if (GetWindowTextA(g_edit, buf, sizeof(buf)) == 0 ||
        !eq_str(buf, LONG_TEXT)) return 306;
    if (SendMessageA(g_edit, EM_CANUNDO, 0, 0) != 0) return 306; // undo consumed the snapshot

    // ── 107: WM_SETTEXT resets the buffer (line count back to 1). ──
    if (!SetWindowTextA(g_edit, LONG_TEXT)) return 307;
    if (SendMessageA(g_edit, EM_GETLINECOUNT, 0, 0) != 5) return 307;
    if (!SetWindowTextA(g_edit, "just one line")) return 307;
    if (SendMessageA(g_edit, EM_GETLINECOUNT, 0, 0) != 1) return 307;
    if (GetWindowTextA(g_edit, buf, sizeof(buf)) == 0 ||
        !eq_str(buf, "just one line")) return 307;

    return 0;
}

static LRESULT CALLBACK WndProc(HWND hwnd, UINT msg, WPARAM wParam, LPARAM lParam) {
    switch (msg) {
    case WM_COMMAND:
        // EN_CHANGE from the edit arrives through the bridged callback while
        // EM_REPLACESEL / EM_UNDO run; the selftest asserts on the edit's
        // state directly, so notifications are ignored.
        return 0;

    case WM_TIMER:
        g_timer_count++;
        InvalidateRect(hwnd, NULL, FALSE);
        if (g_selftest && g_timer_count >= TIMER_TICKS) {
            int code = run_selftest();
            if (code != 0) {
                ExitProcess(code);
            }
            PostQuitMessage(0);
        }
        return 0;

    case WM_CHAR:
        if (wParam == 'q' || wParam == 'Q') {
            DestroyWindow(hwnd);
        }
        return 0;

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
    wc.lpszClassName = "GuiEditClass";
    wc.hIconSm       = NULL;

    ATOM atom = RegisterClassExA(&wc);
    if (atom == 0) {
        ExitProcess(110);
    }

    HWND hwnd = CreateWindowExA(
        0, "GuiEditClass", "WIE GUI Edit",
        WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
        CW_USEDEFAULT, CW_USEDEFAULT, 640, 420,
        NULL, NULL, inst, NULL);
    if (hwnd == NULL) {
        ExitProcess(111);
    }

    // Multiline EDIT child — the exact style set Task 2.8 targets (the
    // notepad-style editing surface: vertical scroll, multiline, auto-scroll
    // at the end, and a selection that stays visible without focus).
    g_edit = CreateWindowExA(
        0, "EDIT", "",
        WS_CHILD | WS_VISIBLE | WS_VSCROLL | ES_MULTILINE | ES_AUTOVSCROLL | ES_NOHIDESEL,
        16, 16, 600, 380, hwnd, (HMENU)IDC_EDIT, inst, NULL);
    if (g_edit == NULL) {
        ExitProcess(112);
    }

    if (SetTimer(hwnd, 1, 50, NULL) == 0) {
        ExitProcess(113);
    }

    ShowWindow(hwnd, SW_SHOW);

    MSG msg;
    while (GetMessageA(&msg, NULL, 0, 0)) {
        TranslateMessage(&msg);
        DispatchMessageA(&msg);
    }

    // PostQuitMessage only happens from the completed selftest (or WM_DESTROY
    // in interactive mode); reaching here without the timer budget means the
    // selftest never ran to completion.
    if (g_selftest && g_timer_count < TIMER_TICKS) {
        ExitProcess(120);
    }

    ExitProcess(0);
}
