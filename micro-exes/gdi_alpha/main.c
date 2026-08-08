// MSIMG32 AlphaBlend micro-test: per-pixel source-alpha compositing between
// two memory DCs (windowless — pure DIB math, no GUI).
//
// CRT-linked console program. Exits 0 only when:
//   1. two compatible DCs + 32-bpp 64x64 DIBs create (else 1)
//   2. the src DIB is filled with 50%-alpha red and the dst with opaque
//      black (else 2)
//   3. AlphaBlend returns TRUE (else 3)
//   4. the blended dst pixel (0,0) has red > 0 (else 4)
//
// The DIB pixel format is BGRA little-endian: value 0x80FF0000 stores
// B=0x00, G=0x00, R=0xFF, A=0x80 — "50% alpha red".

#include <windows.h>
#include <wingdi.h>
#include <stdio.h>

// mingw-w64 does not ship msimg32.h: AlphaBlend / TransparentBlt / GradientFill
// and the BLENDFUNCTION/TRIVERTEX/GRADIENT_RECT types are declared in wingdi.h
// (the real exports still come from libmsimg32.a via -lmsimg32).

#define SIZE 64

static void *g_dst_bits;

// Create a memory DC with a 32-bpp top-down 64x64 DIB selected, returning the
// pixel buffer in *bits_out.
static HDC make_dib_dc(void **bits_out) {
    HDC dc = CreateCompatibleDC(NULL);
    if (dc == NULL) {
        return NULL;
    }
    BITMAPINFO bmi;
    bmi.bmiHeader.biSize = sizeof(BITMAPINFOHEADER);
    bmi.bmiHeader.biWidth = SIZE;
    bmi.bmiHeader.biHeight = -SIZE;      // top-down: row 0 is the top
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
        if (dib != NULL) {
            DeleteObject(dib);
        }
        DeleteDC(dc);
        return NULL;
    }
    if (SelectObject(dc, dib) == NULL) {
        DeleteObject(dib);
        DeleteDC(dc);
        return NULL;
    }
    *bits_out = bits;
    return dc;
}

static void fill_pixels(void *bits, unsigned int color) {
    unsigned int *pixels = (unsigned int *)bits;
    int i;
    for (i = 0; i < SIZE * SIZE; i++) {
        pixels[i] = color;
    }
}

int main(void) {
    // 1. Two compatible DCs + 32-bpp DIBs.
    void *src_bits = NULL;
    HDC src_dc = make_dib_dc(&src_bits);
    HDC dst_dc = make_dib_dc(&g_dst_bits);
    if (src_dc == NULL || dst_dc == NULL || src_bits == NULL || g_dst_bits == NULL) {
        printf("gdi_alpha: FAILED to create DCs/DIBs\n");
        return 1;
    }
    printf("gdi_alpha: DCs + 32-bpp DIBs created\n");

    // 2. Src = 50% alpha red (0x80FF0000), dst = opaque black (0xFF000000).
    fill_pixels(src_bits, 0x80FF0000);
    fill_pixels(g_dst_bits, 0xFF000000);
    printf("gdi_alpha: src filled 50%%-alpha red, dst filled opaque black\n");

    // 3. AlphaBlend must succeed. BlendOp = AC_SRC_OVER, constant alpha 255,
    //    AlphaFormat = AC_SRC_ALPHA (per-pixel source alpha).
    BLENDFUNCTION bf;
    bf.BlendOp = AC_SRC_OVER;
    bf.BlendFlags = 0;
    bf.SourceConstantAlpha = 255;
    bf.AlphaFormat = AC_SRC_ALPHA;
    if (!AlphaBlend(dst_dc, 0, 0, SIZE, SIZE, src_dc, 0, 0, SIZE, SIZE, bf)) {
        printf("gdi_alpha: AlphaBlend FAILED\n");
        return 3;
    }
    printf("gdi_alpha: AlphaBlend ok\n");

    // 4. Blending 50% alpha red over black must leave red > 0 at (0,0).
    {
        unsigned int px = ((unsigned int *)g_dst_bits)[0];
        unsigned int red = (px >> 16) & 0xFF;
        printf("gdi_alpha: blended pixel (0,0) = 0x%08X (red=%u)\n", px, red);
        if (red == 0) {
            printf("gdi_alpha: FAILED — no red survived the blend\n");
            return 4;
        }
    }

    DeleteDC(src_dc);
    DeleteDC(dst_dc);
    printf("gdi_alpha: done\n");
    return 0;
}
