/*
 * Micro-PE OLE: CoInitializeEx / CoCreateGuid / StringFromCLSID round-trip /
 * SafeArray round-trip through the ole32 + oleaut32 handlers.
 *
 * CRT-linked console program (printf). Exit codes:
 *   0  — ok
 *   1  — CoInitializeEx failed
 *   2  — CoCreateGuid failed (or zero GUID)
 *   3  — StringFromCLSID/CLSIDFromString round-trip mismatch
 *   4  — SafeArrayCreate returned NULL
 *   5  — SafeArrayAccessData failed
 *   6  - SafeArrayUnaccessData failed
 *   7  - SafeArrayGetElement mismatch
 *   8  - SafeArrayGetLBound/UBound mismatch
 *   9  - SafeArrayDestroy failed
 */

#include <stdio.h>
#include <string.h>

/*
 * mingw-w64 >= 14 declares SafeArrayCreate with Wine's (VARTYPE, UINT,
 * SAFEARRAYBOUND*) prototype, but the real oleaut32.dll ABI — the one WIE
 * implements — is (UINT cDims, const SAFEARRAYBOUND*, ULONG cbElements).
 * windows.h pulls oleauto.h in transitively (ole2.h -> wtypes.h), so rename
 * the header's declaration away first, then declare the SDK-correct one.
 */
#define SafeArrayCreate SafeArrayCreate_mingw_wine
#include <windows.h>
#include <objbase.h>
#undef SafeArrayCreate

SAFEARRAY *WINAPI SafeArrayCreate(UINT cDims, const SAFEARRAYBOUND *rgsabound,
                                  ULONG cbElements);

static int guid_is_zero(const GUID *g)
{
    /* volatile loads defeat -O2 SSE folding (psrldq), which WIE's CPU
     * backend does not implement; scalar byte OR is instruction-neutral. */
    volatile const unsigned char *b = (const volatile unsigned char *)g;
    unsigned int acc = 0;
    int i;

    for (i = 0; i < 16; i++)
        acc |= (unsigned int)b[i];
    return acc == 0;
}

int main(void)
{
    HRESULT hr;
    GUID guid;
    GUID guid2;
    LPOLESTR wstr = NULL;

    printf("ole_com: CoInitializeEx...\n");
    hr = CoInitializeEx(NULL, 0);
    if (hr != S_OK && hr != S_FALSE) {
        printf("  FAILED (0x%08lx)\n", (unsigned long)hr);
        return 1;
    }
    printf("  ok\n");

    printf("ole_com: CoCreateGuid...\n");
    hr = CoCreateGuid(&guid);
    if (FAILED(hr)) {
        printf("  FAILED (0x%08lx)\n", (unsigned long)hr);
        return 2;
    }
    if (guid_is_zero(&guid)) {
        printf("  FAILED (zero guid)\n");
        return 2;
    }
    printf("  ok\n");

    printf("ole_com: StringFromCLSID / CLSIDFromString...\n");
    hr = StringFromCLSID(&guid, &wstr);
    if (FAILED(hr) || wstr == NULL) {
        printf("  FAILED (0x%08lx)\n", (unsigned long)hr);
        return 3;
    }
    hr = CLSIDFromString(wstr, &guid2);
    if (FAILED(hr)) {
        printf("  FAILED (0x%08lx)\n", (unsigned long)hr);
        return 3;
    }
    if (memcmp(&guid, &guid2, sizeof(guid)) != 0) {
        printf("  FAILED (guid mismatch)\n");
        return 3;
    }
    CoTaskMemFree(wstr);
    wstr = NULL;
    printf("  ok\n");

    printf("ole_com: SafeArray round-trip...\n");
    {
        SAFEARRAYBOUND bound;
        SAFEARRAY *psa;
        INT *pv = NULL;
        INT v;
        LONG i0;
        LONG l, u;

        bound.cElements = 4;
        bound.lLbound = 0;
        psa = SafeArrayCreate(1, &bound, sizeof(INT));
        if (psa == NULL) {
            printf("  FAILED (SafeArrayCreate)\n");
            return 4;
        }
        hr = SafeArrayAccessData(psa, (void **)&pv);
        if (FAILED(hr) || pv == NULL) {
            printf("  FAILED (SafeArrayAccessData 0x%08lx)\n", (unsigned long)hr);
            return 5;
        }
        pv[0] = 10;
        pv[1] = 20;
        pv[2] = 30;
        pv[3] = 40;
        hr = SafeArrayUnaccessData(psa);
        if (FAILED(hr)) {
            printf("  FAILED (SafeArrayUnaccessData 0x%08lx)\n", (unsigned long)hr);
            return 6;
        }
        for (i0 = 0; i0 < 4; i0++) {
            v = 0;
            hr = SafeArrayGetElement(psa, &i0, &v);
            if (FAILED(hr) || v != (i0 + 1) * 10) {
                printf("  FAILED (GetElement[%ld] hr=0x%08lx v=%d)\n",
                       (long)i0, (unsigned long)hr, v);
                return 7;
            }
        }
        hr = SafeArrayGetLBound(psa, 1, &l);
        if (FAILED(hr)) {
            printf("  FAILED (GetLBound 0x%08lx)\n", (unsigned long)hr);
            return 8;
        }
        hr = SafeArrayGetUBound(psa, 1, &u);
        if (FAILED(hr)) {
            printf("  FAILED (GetUBound 0x%08lx)\n", (unsigned long)hr);
            return 8;
        }
        if (l != 0 || u != 3) {
            printf("  FAILED (bounds %ld..%ld)\n", (long)l, (long)u);
            return 8;
        }
        hr = SafeArrayDestroy(psa);
        if (FAILED(hr)) {
            printf("  FAILED (SafeArrayDestroy 0x%08lx)\n", (unsigned long)hr);
            return 9;
        }
    }
    printf("  ok\n");

    CoUninitialize();
    printf("ole_com: done\n");
    return 0;
}
