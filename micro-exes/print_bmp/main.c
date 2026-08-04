// Print-DC pipeline regression test for WIE (P1a).
//
// Exercises the print-job core end to end, the generic-app CreateDCW path:
//   - CreateDCW(L"WINSPOOL", ...) → a DcKind::Print DC + default-letter job
//   - StartDocW with a DOCINFOW (exercises cbSize + lpszDocName reads)
//   - StartPage / EndPage × 2 (two white 300-DPI letter canvases)
//   - SelectObject(NULL_BRUSH) + SelectObject(BLACK_PEN) + Rectangle — the
//     stock-object selection + 1-px stroke path RNotepad's header uses
//   - SetMapMode(MM_TEXT) — the mode store/return path
//   - EndDoc → the host writes page-1.bmp / page-2.bmp under WIE_PRINT_TO
//   - DeleteDC drops the job
//
// The pixel CONTENT (text rasterization) is P1b's assertion; this guest
// asserts the page STRUCTURE: every API returns success, and a distinct
// non-zero exit code names the first failure. The host script checks the two
// BMP files' dimensions (2550×3300) and white pages.

#include <windows.h>

void entry(void) {
    DOCINFOW di;
    HDC hdc;
    int i;

    hdc = CreateDCW(L"WINSPOOL", NULL, NULL, NULL);
    if (hdc == NULL) {
        ExitProcess(101);
    }

    di.cbSize        = sizeof(DOCINFOW);
    di.lpszDocName   = L"WIE Print Test";
    di.lpszOutput    = NULL;
    di.lpszDatatype  = NULL;
    di.fwType        = 0;

    if (StartDocW(hdc, &di) <= 0) {
        ExitProcess(102);
    }

    for (i = 0; i < 2; i++) {
        if (StartPage(hdc) <= 0) {
            ExitProcess(103 + i);
        }

        // RNotepad-style header rect: no fill, black 1-px border.
        SelectObject(hdc, GetStockObject(NULL_BRUSH));
        SelectObject(hdc, GetStockObject(BLACK_PEN));
        Rectangle(hdc, 100, 100, 400, 300);

        if (SetMapMode(hdc, MM_TEXT) != MM_TEXT) {
            ExitProcess(105 + i);
        }

        if (EndPage(hdc) <= 0) {
            ExitProcess(107 + i);
        }
    }

    if (EndDoc(hdc) <= 0) {
        ExitProcess(109);
    }

    DeleteDC(hdc);
    ExitProcess(0);
}
