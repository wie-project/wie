/*
 * DLL providing exported functions for the static-import test.
 *
 * Compiled as: x86_64-w64-mingw32-gcc -shared -o dll_static_import_funcs.dll dll.c
 */

#include <windows.h>

__declspec(dllexport) int add(int a, int b) {
    return a + b;
}

BOOL WINAPI DllMain(HINSTANCE hinstDLL, DWORD fdwReason, LPVOID lpvReserved) {
    (void)hinstDLL;
    (void)fdwReason;
    (void)lpvReserved;
    return TRUE;
}
