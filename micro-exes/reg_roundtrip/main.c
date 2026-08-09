/*
 * Micro-PE for the ADVAPI32 registry lane: create → set → query → flush →
 * delete → close, through the real handlers in
 * crates/wie-winapi/src/advapi32.rs.
 *
 * CRT-linked console program (mingw, -ladvapi32). Exit codes:
 *   0 — ok
 *   1 — RegCreateKeyExW failed (or disposition != REG_CREATED_NEW_KEY)
 *   2 — RegSetValueExW failed
 *   3 — RegQueryValueExW failed (or wrong type / payload)
 *   4 — RegFlushKey failed
 *   5 — RegDeleteKeyW failed
 *   6 — RegCloseKey failed
 */

#include <windows.h>
#include <winreg.h>
#include <stdio.h>
#include <wchar.h>

int main(void) {
    HKEY hk = NULL;
    DWORD disp = 0;
    DWORD type = 0;
    WCHAR buf[32];
    DWORD len = sizeof(buf);
    LONG status;

    printf("reg_roundtrip: RegCreateKeyExW(HKCU\\Software\\WIETest)...\n");
    status = RegCreateKeyExW(HKEY_CURRENT_USER, L"Software\\WIETest", 0, NULL,
                             0, KEY_ALL_ACCESS, NULL, &hk, &disp);
    if (status != ERROR_SUCCESS || disp != REG_CREATED_NEW_KEY) {
        printf("  FAILED (status=%ld disp=%lu)\n", status,
               (unsigned long)disp);
        return 1;
    }
    printf("  created (disposition=%lu)\n", (unsigned long)disp);

    printf("reg_roundtrip: RegSetValueExW(Value=\"hello\")...\n");
    status = RegSetValueExW(hk, L"Value", 0, REG_SZ, (const BYTE *)L"hello", 12);
    if (status != ERROR_SUCCESS) {
        printf("  FAILED (status=%ld)\n", status);
        return 2;
    }
    printf("  ok\n");

    printf("reg_roundtrip: RegQueryValueExW(Value)...\n");
    status = RegQueryValueExW(hk, L"Value", NULL, &type, (LPBYTE)buf, &len);
    if (status != ERROR_SUCCESS || type != REG_SZ || wcscmp(buf, L"hello") != 0) {
        printf("  FAILED (status=%ld type=%lu len=%lu)\n", status,
               (unsigned long)type, (unsigned long)len);
        return 3;
    }
    printf("  got \"hello\"\n");

    printf("reg_roundtrip: RegFlushKey...\n");
    status = RegFlushKey(hk);
    if (status != ERROR_SUCCESS) {
        printf("  FAILED (status=%ld)\n", status);
        return 4;
    }
    printf("  ok\n");

    printf("reg_roundtrip: RegDeleteKeyW(HKCU\\Software\\WIETest)...\n");
    status = RegDeleteKeyW(HKEY_CURRENT_USER, L"Software\\WIETest");
    if (status != ERROR_SUCCESS) {
        printf("  FAILED (status=%ld)\n", status);
        return 5;
    }
    printf("  ok\n");

    printf("reg_roundtrip: RegCloseKey...\n");
    status = RegCloseKey(hk);
    if (status != ERROR_SUCCESS) {
        printf("  FAILED (status=%ld)\n", status);
        return 6;
    }
    printf("  ok\n");

    printf("reg_roundtrip: done\n");
    return 0;
}
