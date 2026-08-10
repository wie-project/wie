/*
 * Micro-PE OLE clipboard: OleInitialize → host-synthesized IDataObject
 * round-trip (SetData/GetData through the clipboard store) → OleFlushClipboard.
 *
 * CRT-linked console program (printf). The IDataObject the guest calls into is
 * the HOST-synthesized object returned by OleGetClipboard — its vtable is
 * written by the host into guest memory, so the micro only calls methods on
 * host-provided objects (no hand-written guest vtable). Exit codes:
 *   0  — ok
 *   1  — CoInitialize failed
 *   2  — OleInitialize failed
 *   3  — OleSetClipboard(NULL) flush-clear failed
 *   4  — OleGetClipboard failed / returned NULL
 *   5  — QueryInterface did not return the same object
 *   6  — AddRef/Release did not return 1
 *   7  — SetData failed
 *   8  — GetData failed
 *   9  — GetData payload did not round-trip ("hello ole")
 *  10  — OleSetClipboard(p) store failed
 *  11  — OleGetClipboard did not return the stored object
 *  12  — OleFlushClipboard failed
 */

#include <windows.h>
#include <objbase.h>
#include <oleidl.h>
#include <stdio.h>
#include <string.h>

/* IID_IDataObject normally comes from -luuid; the build links only -lole32,
 * so declare the GUID locally (the host QI answers any IID with the same
 * object, so the value is for documentation). */
static const GUID IID_IDataObject_local = {0x0000010E, 0x0000, 0x0000,
                                           {0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46}};

int main(void)
{
    HRESULT hr;
    IDataObject *p = NULL;
    IDataObject *q = NULL;
    FORMATETC fmt;
    STGMEDIUM stg;
    char payload[] = "hello ole";

    printf("ole_clip: CoInitialize...\n");
    hr = CoInitialize(NULL);
    if (hr != S_OK && hr != S_FALSE) {
        printf("  FAILED (0x%08lx)\n", (unsigned long)hr);
        return 1;
    }
    printf("  ok\n");

    printf("ole_clip: OleInitialize...\n");
    hr = OleInitialize(NULL);
    if (hr != S_OK && hr != S_FALSE) {
        printf("  FAILED (0x%08lx)\n", (unsigned long)hr);
        return 2;
    }
    printf("  ok\n");

    printf("ole_clip: OleSetClipboard(NULL) flush-clear...\n");
    hr = OleSetClipboard(NULL);
    if (FAILED(hr)) {
        printf("  FAILED (0x%08lx)\n", (unsigned long)hr);
        return 3;
    }
    printf("  ok\n");

    printf("ole_clip: OleGetClipboard (host-synthesized IDataObject)...\n");
    hr = OleGetClipboard(&p);
    if (FAILED(hr) || p == NULL) {
        printf("  FAILED (hr=0x%08lx p=%p)\n", (unsigned long)hr, (void *)p);
        return 4;
    }
    printf("  p=%p\n", (void *)p);

    printf("ole_clip: QueryInterface/AddRef/Release...\n");
    hr = p->lpVtbl->QueryInterface(p, &IID_IDataObject_local, (void **)&q);
    if (FAILED(hr) || q != p) {
        printf("  FAILED (QI hr=0x%08lx q=%p)\n", (unsigned long)hr, (void *)q);
        return 5;
    }
    if (p->lpVtbl->AddRef(p) != 1 || p->lpVtbl->Release(p) != 1) {
        printf("  FAILED (refcount)\n");
        return 6;
    }
    printf("  ok\n");

    printf("ole_clip: IDataObject::SetData(CF_TEXT)...\n");
    memset(&fmt, 0, sizeof(fmt));
    fmt.cfFormat = CF_TEXT;
    fmt.dwAspect = DVASPECT_CONTENT;
    fmt.lindex = -1;
    fmt.tymed = TYMED_HGLOBAL;
    memset(&stg, 0, sizeof(stg));
    stg.tymed = TYMED_HGLOBAL;
    stg.hGlobal = (HGLOBAL)payload; /* host reads the bytes at this VA */
    hr = p->lpVtbl->SetData(p, &fmt, &stg, TRUE);
    if (FAILED(hr)) {
        printf("  FAILED (0x%08lx)\n", (unsigned long)hr);
        return 7;
    }
    printf("  ok\n");

    printf("ole_clip: IDataObject::GetData(CF_TEXT)...\n");
    memset(&stg, 0, sizeof(stg));
    hr = p->lpVtbl->GetData(p, &fmt, &stg);
    if (FAILED(hr) || stg.hGlobal == NULL) {
        printf("  FAILED (hr=0x%08lx hGlobal=%p)\n", (unsigned long)hr, stg.hGlobal);
        return 8;
    }
    if (strcmp((const char *)stg.hGlobal, "hello ole") != 0) {
        printf("  FAILED (got \"%s\")\n", (const char *)stg.hGlobal);
        return 9;
    }
    printf("  got \"%s\"\n", (const char *)stg.hGlobal);

    printf("ole_clip: OleSetClipboard(p) / OleGetClipboard round-trip...\n");
    hr = OleSetClipboard(p);
    if (FAILED(hr)) {
        printf("  FAILED (0x%08lx)\n", (unsigned long)hr);
        return 10;
    }
    q = NULL;
    hr = OleGetClipboard(&q);
    if (FAILED(hr) || q != p) {
        printf("  FAILED (hr=0x%08lx q=%p)\n", (unsigned long)hr, (void *)q);
        return 11;
    }
    printf("  ok\n");

    printf("ole_clip: OleFlushClipboard...\n");
    hr = OleFlushClipboard();
    if (FAILED(hr)) {
        printf("  FAILED (0x%08lx)\n", (unsigned long)hr);
        return 12;
    }
    printf("  ok\n");

    p->lpVtbl->Release(p);
    OleUninitialize();
    CoUninitialize();
    printf("ole_clip: done\n");
    return 0;
}
