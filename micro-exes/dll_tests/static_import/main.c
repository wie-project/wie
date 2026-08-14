/*
 * DLL test: static (IAT) import of a guest DLL, resolved at process init.
 *
 * The exe links dll_static_import_funcs.dll via an import library, so its IAT
 * imports `add` statically — no LoadLibrary/GetProcAddress needed. WIE's
 * pass-2 static-import resolution must patch the IAT slot to the real export
 * before entry runs.
 *
 * The second half is the FreeLibrary-pin regression: a statically-linked dep
 * must survive guest FreeLibrary (its image stays mapped — the IAT still
 * points into it), so re-calling add() after FreeLibrary must still work.
 *
 * Exit codes:
 *   0 — all checks passed
 *   3 — add returned wrong value (IAT slot not patched / placeholder hit)
 *   4 — LoadLibrary on the static dep failed
 *   5 — FreeLibrary returned FALSE
 *   6 — add returned wrong value after FreeLibrary (image was unmapped)
 */

#include <windows.h>

__declspec(dllimport) int add(int a, int b);

void entry(void) {
    HMODULE dll;
    int result;

    result = add(2, 3);
    if (result != 5) {
        ExitProcess(3);
    }

    /* The static dep is already loaded; LoadLibrary must return its handle. */
    dll = LoadLibraryA("dll_static_import_funcs.dll");
    if (dll == NULL) {
        ExitProcess(4);
    }

    /* FreeLibrary must succeed WITHOUT unmapping the static dep's image. */
    if (!FreeLibrary(dll)) {
        ExitProcess(5);
    }

    /* The IAT still points into the pinned image — this must still work. */
    result = add(2, 3);
    if (result != 5) {
        ExitProcess(6);
    }

    ExitProcess(0);
}
