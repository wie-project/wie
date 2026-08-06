// Interactive all-components GUI demo for WIE.
//
// One window exercising every control tier together, live:
//   - EDIT (text input; type, then Apply copies it to a label)
//   - COMBOBOX (dropdown; selection updates a label)
//   - LISTBOX (Add appends the edit text; double-click reports it)
//   - BUTTONs (Apply / Add / About / Dialog…)
//   - STATIC labels (edit echo, combo echo, list echo, dialog echo)
//   - modal dialog (template id 100: its own EDIT + OK/Cancel; OK returns
//     the text to the main window)
//   - menu (File ▸ Exit — mirrored into the macOS top bar)
//   - timer (repaints; drives the self-test)
//
// Self-test (WIE_SELFTEST=1): scripted flow — click the Dialog button (the
// real bridged WM_COMMAND click path, not a direct call), the timer posts
// Enter to close the dialog, verify the dialog text echoed back, then set
// the edit text, select a combo item, and exit 0 only if every step
// verified (incl. a UTF-16 round-trip through the W-string boundary; the
// ANSI A path stays cp1252-faithful, so unicode coverage uses the W path).
// Interactive runs keep the window open and quit on 'q' / Exit / close.

#include <windows.h>

#define IDM_EXIT 2
#define IDC_EDIT       10
#define IDC_COMBO      11
#define IDC_LIST       12
#define IDC_BTN_APPLY  13
#define IDC_BTN_ADD    14
#define IDC_BTN_ABOUT  15
#define IDC_BTN_DIALOG 16
#define IDC_LBL_EDIT   20
#define IDC_LBL_COMBO  21
#define IDC_LBL_LIST   22
#define IDC_LBL_DLG    23
#define IDC_DLG_EDIT   1003
#define TIMER_TICKS     6

static HWND g_hwnd;
static HINSTANCE g_inst;
static HWND g_edit, g_combo, g_list, g_btn_dialog;
static int g_selftest;
static int g_timer_count;
static wchar_t g_dialog_text_w[128];
static int g_dialog_result;

static int selftest_enabled(void) {
    char buf[16];
    DWORD n = GetEnvironmentVariableA("WIE_SELFTEST", buf, sizeof(buf));
    return n == 1 && buf[0] == '1';
}

static int eq_str(const char *a, const char *b) {
    while (*a && *b) {
        if (*a != *b) return 0;
        a++;
        b++;
    }
    return *a == *b;
}

static int eq_str_w(const wchar_t *a, const wchar_t *b) {
    while (*a && *b) {
        if (*a != *b) return 0;
        a++;
        b++;
    }
    return *a == *b;
}

// Set the echo label text; the control paints through the host WndProc.
static void set_label(HWND parent, int id, const char *text) {
    SetDlgItemTextA(parent, id, text);
}

static void set_label_w(HWND parent, int id, const wchar_t *text) {
    SetDlgItemTextW(parent, id, text);
}

static void sync_edit_label(void) {
    char buf[128];
    GetWindowTextA(g_edit, buf, sizeof(buf));
    char out[160];
    out[0] = 'E';
    out[1] = ':';
    out[2] = ' ';
    int i = 3;
    for (int j = 0; buf[j] && i < (int)sizeof(out) - 1; j++, i++) {
        out[i] = buf[j];
    }
    out[i] = 0;
    set_label(g_hwnd, IDC_LBL_EDIT, out);
}

static void sync_combo_label(void) {
    int sel = (int)SendMessageA(g_combo, CB_GETCURSEL, 0, 0);
    if (sel < 0) {
        set_label(g_hwnd, IDC_LBL_COMBO, "C: (none)");
        return;
    }
    char buf[64];
    SendMessageA(g_combo, CB_GETLBTEXT, (WPARAM)sel, (LPARAM)buf);
    char out[80];
    out[0] = 'C';
    out[1] = ':';
    out[2] = ' ';
    int i = 3;
    for (int j = 0; buf[j] && i < (int)sizeof(out) - 1; j++, i++) {
        out[i] = buf[j];
    }
    out[i] = 0;
    set_label(g_hwnd, IDC_LBL_COMBO, out);
}

static void sync_list_label(void) {
    int sel = (int)SendMessageA(g_list, LB_GETCURSEL, 0, 0);
    if (sel < 0) {
        set_label(g_hwnd, IDC_LBL_LIST, "L: (none)");
        return;
    }
    char buf[64];
    SendMessageA(g_list, LB_GETTEXT, (WPARAM)sel, (LPARAM)buf);
    char out[80];
    out[0] = 'L';
    out[1] = ':';
    out[2] = ' ';
    int i = 3;
    for (int j = 0; buf[j] && i < (int)sizeof(out) - 1; j++, i++) {
        out[i] = buf[j];
    }
    out[i] = 0;
    set_label(g_hwnd, IDC_LBL_LIST, out);
}

static void on_apply(void) {
    sync_edit_label();
}

static void on_add(void) {
    char buf[128];
    GetWindowTextA(g_edit, buf, sizeof(buf));
    if (buf[0] != 0) {
        SendMessageA(g_list, LB_ADDSTRING, 0, (LPARAM)buf);
    }
}

// Modal dialog proc: template 100 has an EDIT (1003) + OK/Cancel.
static INT_PTR CALLBACK DlgProc(HWND hwnd, UINT msg, WPARAM wParam, LPARAM lParam) {
    switch (msg) {
    case WM_INITDIALOG:
        // Unicode echo via the W path (lossless by design; the ANSI A path is
        // cp1252-faithful and would degrade U+2713 to '?').
        SetDlgItemTextW(hwnd, IDC_DLG_EDIT, L"dialog text — ✓");
        return 1;
    case WM_COMMAND:
        if (LOWORD(wParam) == 1) { // IDOK
            GetDlgItemTextW(hwnd, IDC_DLG_EDIT, g_dialog_text_w, sizeof(g_dialog_text_w) / sizeof(wchar_t));
            g_dialog_result = 1;
            EndDialog(hwnd, 1);
            return 1;
        }
        if (LOWORD(wParam) == 2) { // IDCANCEL
            g_dialog_result = 0;
            EndDialog(hwnd, 0);
            return 1;
        }
        return 0;
    }
    return 0;
}

static void on_dialog(void) {
    g_dialog_text_w[0] = 0;
    g_dialog_result = -1;
    INT_PTR result = DialogBoxParamA(g_inst, (LPCSTR)100, g_hwnd, DlgProc, 0);
    g_dialog_result = (int)result;
    if (result == 1) {
        wchar_t out[160];
        out[0] = L'D';
        out[1] = L':';
        out[2] = L' ';
        int i = 3;
        for (int j = 0; g_dialog_text_w[j] && i < (int)(sizeof(out) / sizeof(wchar_t)) - 1; j++, i++) {
            out[i] = g_dialog_text_w[j];
        }
        out[i] = 0;
        set_label_w(g_hwnd, IDC_LBL_DLG, out);
    } else {
        set_label_w(g_hwnd, IDC_LBL_DLG, L"D: (cancelled)");
    }
}

static void on_about(void) {
    set_label(g_hwnd, IDC_LBL_DLG, "D: About — all controls alive");
}

static LRESULT CALLBACK WndProc(HWND hwnd, UINT msg, WPARAM wParam, LPARAM lParam) {
    switch (msg) {
    case WM_COMMAND: {
        int id = (int)LOWORD(wParam);
        if (id == IDM_EXIT) {
            DestroyWindow(hwnd);
            return 0;
        }
        if (id == IDC_BTN_APPLY) {
            on_apply();
            return 0;
        }
        if (id == IDC_BTN_ADD) {
            on_add();
            return 0;
        }
        if (id == IDC_BTN_ABOUT) {
            on_about();
            return 0;
        }
        if (id == IDC_BTN_DIALOG) {
            on_dialog();
            return 0;
        }
        if (id == IDC_COMBO && HIWORD(wParam) == CBN_SELCHANGE) {
            sync_combo_label();
            return 0;
        }
        if (id == IDC_LIST && (HIWORD(wParam) == LBN_SELCHANGE || HIWORD(wParam) == LBN_DBLCLK)) {
            sync_list_label();
            return 0;
        }
        return 0;
    }

    case WM_TIMER:
        g_timer_count++;
        if (g_selftest) {
            if (g_timer_count == 1) {
                // Click the Dialog button through the REAL bridged click
                // path (BM_CLICK → host button WndProc → WM_COMMAND into the
                // guest WndProc → DialogBoxParam). The modal loop runs inside
                // this dispatch; the next tick's Enter closes the dialog.
                SendMessageA(g_btn_dialog, BM_CLICK, 0, 0);
            } else if (g_timer_count == 2) {
                // The click is synchronous, so the dialog's modal loop is
                // running right now. Close it with a REAL mouse click on the
                // OK button (WM_LBUTTONDOWN/UP → BN_CLICKED →
                // WM_COMMAND(IDOK) → EndDialog) — the exact path a user's
                // click takes.
                HWND dlg = GetActiveWindow();
                if (dlg && dlg != g_hwnd) {
                    HWND ok = GetDlgItem(dlg, 1);
                    if (ok) {
                        PostMessageA(ok, WM_LBUTTONDOWN, MK_LBUTTON, 0);
                        PostMessageA(ok, WM_LBUTTONUP, 0, 0);
                    }
                }
            }
            if (g_timer_count >= TIMER_TICKS) {
                // Verify every component echoed. Each step exits with a
                // distinct code so CI failures pinpoint the component.
                char buf[128];
                // 1. Dialog text echo (UTF-16 round-trip through the W-string
                // boundary: the literal is UTF-16 in the PE, the host stores
                // it UTF-8, and the label read-back must match exactly).
                wchar_t wbuf[128];
                GetWindowTextW(GetDlgItem(g_hwnd, IDC_LBL_DLG), wbuf, 128);
                int ok = g_dialog_result == 1 && eq_str_w(wbuf, L"D: dialog text — ✓");
                if (ok) {
                    // 2. Edit echo.
                    SetWindowTextA(g_edit, "Hello WIE");
                    SendMessageA(g_combo, CB_SETCURSEL, 1, 0);
                    on_apply();
                    GetWindowTextA(GetDlgItem(g_hwnd, IDC_LBL_EDIT), buf, sizeof(buf));
                    ok = eq_str(buf, "E: Hello WIE");
                } else {
                    ExitProcess(100); // step 1 failed (dialog echo)
                }
                if (ok) {
                    // 3. Combo echo.
                    SendMessageA(g_combo, CB_SETCURSEL, 2, 0);
                    sync_combo_label();
                    GetWindowTextA(GetDlgItem(g_hwnd, IDC_LBL_COMBO), buf, sizeof(buf));
                    ok = eq_str(buf, "C: Combo Item 3");
                } else {
                    ExitProcess(101); // step 2 failed (edit echo)
                }
                if (ok) {
                    // 4. List add + echo.
                    on_add();
                    SendMessageA(g_list, LB_SETCURSEL, 0, 0);
                    sync_list_label();
                    GetWindowTextA(GetDlgItem(g_hwnd, IDC_LBL_LIST), buf, sizeof(buf));
                    ok = eq_str(buf, "L: Hello WIE");
                } else {
                    ExitProcess(102); // step 3 failed (combo echo)
                }
                if (ok) {
                    PostQuitMessage(0);
                } else {
                    ExitProcess(103); // step 4 failed (list echo)
                }
            }
        }
        InvalidateRect(hwnd, NULL, FALSE);
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
    wc.lpszClassName = "GuiDemoClass";
    wc.hIconSm       = NULL;

    if (RegisterClassExA(&wc) == 0) {
        ExitProcess(101);
    }

    g_hwnd = CreateWindowExA(
        0, "GuiDemoClass", "WIE GUI Demo — all components",
        WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
        CW_USEDEFAULT, CW_USEDEFAULT, 640, 420,
        NULL, NULL, g_inst, NULL);
    if (g_hwnd == NULL) {
        ExitProcess(102);
    }

    // Menu: File ▸ Exit (mirrored into the macOS top bar).
    {
        HMENU menu = CreateMenu();
        HMENU file = CreatePopupMenu();
        AppendMenuA(file, MF_STRING, IDM_EXIT, "Exit");
        AppendMenuA(menu, MF_POPUP, (UINT_PTR)file, "File");
        SetMenu(g_hwnd, menu);
    }

    // All components, live on one window.
    CreateWindowExA(0, "STATIC", "Text:", WS_CHILD | WS_VISIBLE,
                    16, 14, 48, 20, g_hwnd, NULL, g_inst, NULL);
    g_edit = CreateWindowExA(0, "EDIT", "",
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL,
                    70, 12, 180, 22, g_hwnd, (HMENU)IDC_EDIT, g_inst, NULL);
    CreateWindowExA(0, "BUTTON", "Apply",
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON,
                    256, 12, 70, 22, g_hwnd, (HMENU)IDC_BTN_APPLY, g_inst, NULL);
    CreateWindowExA(0, "STATIC", "Combo:", WS_CHILD | WS_VISIBLE,
                    16, 44, 48, 20, g_hwnd, NULL, g_inst, NULL);
    g_combo = CreateWindowExA(0, "COMBOBOX", "",
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST,
                    70, 42, 180, 120, g_hwnd, (HMENU)IDC_COMBO, g_inst, NULL);
    CreateWindowExA(0, "BUTTON", "Add to list",
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON,
                    256, 42, 90, 22, g_hwnd, (HMENU)IDC_BTN_ADD, g_inst, NULL);
    CreateWindowExA(0, "STATIC", "List:", WS_CHILD | WS_VISIBLE,
                    16, 74, 48, 20, g_hwnd, NULL, g_inst, NULL);
    g_list = CreateWindowExA(0, "LISTBOX", "",
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_BORDER | LBS_NOTIFY,
                    70, 72, 180, 140, g_hwnd, (HMENU)IDC_LIST, g_inst, NULL);
    g_btn_dialog = CreateWindowExA(0, "BUTTON", "Dialog…",
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON,
                    256, 72, 90, 22, g_hwnd, (HMENU)IDC_BTN_DIALOG, g_inst, NULL);
    CreateWindowExA(0, "BUTTON", "About",
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON,
                    256, 100, 90, 22, g_hwnd, (HMENU)IDC_BTN_ABOUT, g_inst, NULL);
    CreateWindowExA(0, "STATIC", "E: (edit echo)", WS_CHILD | WS_VISIBLE,
                    16, 224, 400, 20, g_hwnd, (HMENU)IDC_LBL_EDIT, g_inst, NULL);
    CreateWindowExA(0, "STATIC", "C: (combo echo)", WS_CHILD | WS_VISIBLE,
                    16, 246, 400, 20, g_hwnd, (HMENU)IDC_LBL_COMBO, g_inst, NULL);
    CreateWindowExA(0, "STATIC", "L: (list echo)", WS_CHILD | WS_VISIBLE,
                    16, 268, 400, 20, g_hwnd, (HMENU)IDC_LBL_LIST, g_inst, NULL);
    CreateWindowExA(0, "STATIC", "D: (dialog echo)", WS_CHILD | WS_VISIBLE,
                    16, 290, 400, 20, g_hwnd, (HMENU)IDC_LBL_DLG, g_inst, NULL);

    // Combo items.
    SendMessageA(g_combo, CB_ADDSTRING, 0, (LPARAM)"Combo Item 1");
    SendMessageA(g_combo, CB_ADDSTRING, 0, (LPARAM)"Combo Item 2");
    SendMessageA(g_combo, CB_ADDSTRING, 0, (LPARAM)"Combo Item 3");

    if (SetTimer(g_hwnd, 1, 100, NULL) == 0) {
        ExitProcess(103);
    }

    ShowWindow(g_hwnd, SW_SHOW);

    // Self-test: the first WM_TIMER click drives the Dialog button through
    // the REAL bridged click path (BM_CLICK → host button WndProc →
    // WM_COMMAND into the guest WndProc → DialogBoxParam), so the modal loop
    // runs inside a guest callback exactly like an interactive click. The
    // second tick closes it with Enter (DEFPUSHBUTTON OK).

    MSG msg;
    while (GetMessageA(&msg, NULL, 0, 0)) {
        TranslateMessage(&msg);
        DispatchMessageA(&msg);
    }

    ExitProcess(0);
}
