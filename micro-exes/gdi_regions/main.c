// GDI32 micro-test: DIB round-trips (GetDIBits/SetDIBits) + region ops.
//
// CRT-linked console program. Exits 0 only when every stage succeeds:
//   1. a 32-bpp 64x64 top-down DIB creates (else 1)
//   2. the DIB is filled with a per-row pattern (else 2)
//   3. GetDIBits copies 64 rows and the first pixel matches the pattern
//      value of row 0 (else 3)
//   4. SetDIBits writes the modified buffer back (else 4)
//   5. the DIB's first pixel reads back as the written value (else 5)
//   6. CreateRectRgn(10,10,50,50) creates (else 6)
//   7. CreateRectRgn(30,30,70,70) creates (else 7)
//   8. CombineRgn(hrgn1, hrgn1, hrgn2, RGN_OR) reports a non-empty region
//      (else 8)
//   9. GetRgnBox(hrgn1) gives (10,10,70,70) (else 9)
//   10. SetRectRgn(hrgn1, 0,0,5,5) returns TRUE (else 10)
//   11. GetRgnBox(hrgn1) gives (0,0,5,5) (else 11)
//   12. DeleteObject(dib) succeeds; return 0.
//
// Row pattern deviation: row r holds pixel value (r + 1), not r, so the
// first-pixel check is a non-zero value that also proves the copy is
// row-aligned (a zero-filled buffer would not fool it).
//
// The region handles are NOT DeleteObject-verified: the dense
// Gdi32DeleteObject classifier only knows the 0x68xx object bases, so it
// returns TRUE without freeing the (tiny) region-table entries — a documented
// leak on the WIE side.

#include <windows.h>
#include <wingdi.h>
#include <stdio.h>

#define SIZE 64
// Row r is filled with (r + 1) so buf[0] is a meaningful non-zero check.
#define PATTERN(r) ((r) + 1)

int main(void) {
    // 1. 32-bpp top-down 64x64 DIB (negative height = top-down).
    HDC dc = CreateCompatibleDC(NULL);
    BITMAPINFO bmi;
    bmi.bmiHeader.biSize = sizeof(BITMAPINFOHEADER);
    bmi.bmiHeader.biWidth = SIZE;
    bmi.bmiHeader.biHeight = -(SIZE);   // top-down: row 0 is the top
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB;
    bmi.bmiHeader.biSizeImage = 0;
    bmi.bmiHeader.biXPelsPerMeter = 0;
    bmi.bmiHeader.biYPelsPerMeter = 0;
    bmi.bmiHeader.biClrUsed = 0;
    bmi.bmiHeader.biClrImportant = 0;
    void *bits = NULL;
    HBITMAP dib = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &bits, NULL, 0);
    if (dib == NULL || bits == NULL) {
        printf("gdi_regions: FAILED to create DIB\n");
        return 1;
    }
    if (dc != NULL) {
        DeleteDC(dc);
    }
    printf("gdi_regions: 32-bpp 64x64 DIB created\n");

    // 2. Fill row r with value (r + 1) — the pattern GetDIBits must return.
    {
        unsigned int *pixels = (unsigned int *)bits;
        int r, c;
        for (r = 0; r < SIZE; r++) {
            for (c = 0; c < SIZE; c++) {
                pixels[r * SIZE + c] = PATTERN(r);
            }
        }
        printf("gdi_regions: DIB filled with per-row pattern\n");
    }

    // 3. GetDIBits must copy all 64 rows and buf[0] must be row 0's value.
    {
        unsigned int buf[SIZE * SIZE];
        int got = GetDIBits(NULL, dib, 0, SIZE, buf, &bmi, DIB_RGB_COLORS);
        if (got != SIZE) {
            printf("gdi_regions: FAILED — GetDIBits returned %d\n", got);
            return 2;
        }
        if (buf[0] != PATTERN(0)) {
            printf("gdi_regions: FAILED — buf[0] = 0x%08X, want 0x%08X\n",
                   buf[0], PATTERN(0));
            return 3;
        }
        printf("gdi_regions: GetDIBits copied 64 rows, first pixel ok\n");
    }

    // 4-5. SetDIBits must write the modified buffer back into the DIB.
    {
        unsigned int buf[SIZE * SIZE];
        int i;
        for (i = 0; i < SIZE * SIZE; i++) {
            buf[i] = 0x11223344;
        }
        int got = SetDIBits(NULL, dib, 0, SIZE, buf, &bmi, DIB_RGB_COLORS);
        if (got != SIZE) {
            printf("gdi_regions: FAILED — SetDIBits returned %d\n", got);
            return 4;
        }
        unsigned int first = ((unsigned int *)bits)[0];
        if (first != 0x11223344) {
            printf("gdi_regions: FAILED — DIB[0] = 0x%08X after SetDIBits\n",
                   first);
            return 5;
        }
        printf("gdi_regions: SetDIBits wrote back, first pixel ok\n");
    }

    // 6-7. Two overlapping rect regions.
    HRGN hrgn1 = CreateRectRgn(10, 10, 50, 50);
    if (hrgn1 == NULL) {
        printf("gdi_regions: FAILED — CreateRectRgn #1\n");
        return 6;
    }
    HRGN hrgn2 = CreateRectRgn(30, 30, 70, 70);
    if (hrgn2 == NULL) {
        printf("gdi_regions: FAILED — CreateRectRgn #2\n");
        return 7;
    }
    printf("gdi_regions: two rect regions created\n");

    // 8. OR-combine into hrgn1: result must be a non-empty region.
    {
        int combined = CombineRgn(hrgn1, hrgn1, hrgn2, RGN_OR);
        if (combined != COMPLEXREGION && combined != SIMPLEREGION) {
            printf("gdi_regions: FAILED — CombineRgn returned %d\n", combined);
            return 8;
        }
        printf("gdi_regions: CombineRgn(RGN_OR) ok (kind %d)\n", combined);
    }

    // 9. The OR bounding box spans both rects.
    {
        RECT rc;
        int got = GetRgnBox(hrgn1, &rc);
        if (got == 0 || rc.left != 10 || rc.top != 10 ||
            rc.right != 70 || rc.bottom != 70) {
            printf("gdi_regions: FAILED — GetRgnBox = (%ld,%ld,%ld,%ld)\n",
                   (long)rc.left, (long)rc.top, (long)rc.right, (long)rc.bottom);
            return 9;
        }
        printf("gdi_regions: GetRgnBox = (10,10,70,70)\n");
    }

    // 10-11. SetRectRgn replaces the contents.
    {
        if (!SetRectRgn(hrgn1, 0, 0, 5, 5)) {
            printf("gdi_regions: FAILED — SetRectRgn returned FALSE\n");
            return 10;
        }
        RECT rc;
        int got = GetRgnBox(hrgn1, &rc);
        if (got == 0 || rc.left != 0 || rc.top != 0 ||
            rc.right != 5 || rc.bottom != 5) {
            printf("gdi_regions: FAILED — GetRgnBox after SetRectRgn = "
                   "(%ld,%ld,%ld,%ld)\n",
                   (long)rc.left, (long)rc.top, (long)rc.right, (long)rc.bottom);
            return 11;
        }
        printf("gdi_regions: SetRectRgn + GetRgnBox = (0,0,5,5)\n");
    }

    // 12. DeleteObject on the DIB (region handles: see header note).
    if (!DeleteObject(dib)) {
        printf("gdi_regions: FAILED — DeleteObject(dib)\n");
        return 12;
    }

    printf("gdi_regions: done\n");
    return 0;
}
