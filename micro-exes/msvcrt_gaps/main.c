/*
 * Micro-PE: msvcrt.dll import-census gaps. notepad.exe imports these; WIE did
 * not handle them before feat/dll-coverage:
 *   fgetwc, getc, vfprintf, _wcmdln (data import), __CxxFrameHandler.
 *
 * CRT-linked console program. Stdin is injected by the host test
 * (crates/wie-runtime/tests/micro_msvcrt_gaps.rs) as "AB", so the reads are
 * sequenced: fgetwc consumes 'A', getc consumes 'B', vfprintf formats the
 * tail (its result is checked, not printed — %lc output has a console
 * artifact).
 *
 * _wcmdln is a CRT data export not directly reachable from C (mingw links
 * __p__wcmdln, not _wcmdln); its census flip is verified by the load path
 * resolving `crt_data_import_va("_wcmdln")` for binaries that import it.
 * __CxxFrameHandler is likewise exercised by the cpp_exes suite (GCC), whose
 * imports resolve through the same dispatcher.
 *
 * Exit codes:
 *   0 — ok
 *   1 — fgetwc mismatch (expected L'A')
 *   2 — getc mismatch (expected 'B')
 *   3 — vfprintf returned <= 0
 */

#include <windows.h>
#include <stdio.h>
#include <wchar.h>
#include <stdarg.h>

/* vfprintf is a real msvcrt.dll export; wrapping it in a varargs helper makes
 * the linker emit the import. */
static int emit_vfprintf(FILE *stream, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int n = vfprintf(stream, fmt, ap);
    va_end(ap);
    return n;
}

int main(void) {
    wint_t wc = fgetwc(stdin);
    if (wc != L'A') {
        return 1;
    }

    int c = getc(stdin);
    if (c != 'B') {
        return 2;
    }

    int n = emit_vfprintf(stdout, "vf=%d\n", 42);
    if (n <= 0) {
        return 3;
    }

    printf("msvcrt_gaps: ok\n");
    return 0;
}
