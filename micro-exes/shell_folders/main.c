/*
 * SHELL32 micro-PE: CSIDL folder mapping + SHGetFileInfo on a real guest file.
 *
 * CRT-linked console program. Exercises the shell32 handlers in
 * crates/wie-winapi/src/shell32.rs:
 *   1. SHGetSpecialFolderPathW(CSIDL_PERSONAL) → TRUE, non-empty path
 *   2. SHGetSpecialFolderPathW(CSIDL_APPDATA)  → TRUE
 *   3. SHGetFileInfoW on C:\Users\WIE\Documents\shell_test.txt (created
 *      first) → non-zero, FILE_ATTRIBUTE_NORMAL set, display name non-empty
 *   4. SHAddToRecentDocs(SHARD_PATHW) — must not crash
 *
 * Exit codes:
 *   0  — ok
 *   1  — SHGetSpecialFolderPathW(CSIDL_PERSONAL) failed or empty
 *   2  — SHGetSpecialFolderPathW(CSIDL_APPDATA) failed
 *   3  — SHGetFileInfoW failed (returned 0) or no FILE_ATTRIBUTE_NORMAL
 *   4  — SHGetFileInfoW returned an empty display name
 *   5  — (reserved) SHAddToRecentDocs crashed
 */

#include <windows.h>
#include <shlobj.h>
#include <shellapi.h>
#include <stdio.h>
#include <string.h>

int main(void) {
    WCHAR personal[MAX_PATH];
    WCHAR appdata[MAX_PATH];
    WCHAR path[MAX_PATH];
    SHFILEINFOW sfi;
    HANDLE h;
    DWORD written;

    printf("shell_folders: SHGetSpecialFolderPathW(CSIDL_PERSONAL)...\n");
    if (!SHGetSpecialFolderPathW(NULL, personal, CSIDL_PERSONAL, FALSE)) {
        printf("  FAILED\n");
        return 1;
    }
    if (personal[0] == L'\0') {
        printf("  FAILED (empty path)\n");
        return 1;
    }
    printf("  ok: %ls\n", personal);

    printf("shell_folders: SHGetSpecialFolderPathW(CSIDL_APPDATA)...\n");
    if (!SHGetSpecialFolderPathW(NULL, appdata, CSIDL_APPDATA, FALSE)) {
        printf("  FAILED\n");
        return 2;
    }
    if (appdata[0] == L'\0') {
        printf("  FAILED (empty path)\n");
        return 2;
    }
    printf("  ok: %ls\n", appdata);

    printf("shell_folders: create test file in Documents...\n");
    /* The seed skeleton already provides Documents; ignore ERROR_ALREADY_EXISTS. */
    CreateDirectoryW(L"C:\\Users\\WIE\\Documents", NULL);
    wcscpy(path, personal);
    wcscat(path, L"\\shell_test.txt");
    h = CreateFileW(path, GENERIC_WRITE, 0, NULL, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
    if (h == INVALID_HANDLE_VALUE) {
        printf("  FAILED (CreateFileW)\n");
        return 3;
    }
    if (!WriteFile(h, "hello", 5, &written, NULL) || written != 5) {
        printf("  FAILED (WriteFile)\n");
        CloseHandle(h);
        return 3;
    }
    CloseHandle(h);

    printf("shell_folders: SHGetFileInfoW(%ls)...\n", path);
    if (!SHGetFileInfoW(path, 0, &sfi, sizeof(sfi), SHGFI_DISPLAYNAME | SHGFI_ATTRIBUTES)) {
        printf("  FAILED (returned 0)\n");
        DeleteFileW(path);
        return 3;
    }
    if ((sfi.dwAttributes & FILE_ATTRIBUTE_NORMAL) == 0) {
        printf("  FAILED (FILE_ATTRIBUTE_NORMAL not set, got 0x%lx)\n",
               (unsigned long)sfi.dwAttributes);
        DeleteFileW(path);
        return 3;
    }
    if (sfi.szDisplayName[0] == L'\0') {
        printf("  FAILED (empty display name)\n");
        DeleteFileW(path);
        return 4;
    }
    printf("  ok: name=%ls attributes=0x%lx\n", sfi.szDisplayName,
           (unsigned long)sfi.dwAttributes);

    printf("shell_folders: SHAddToRecentDocs(SHARD_PATHW)...\n");
    SHAddToRecentDocs(SHARD_PATHW, path);
    printf("  ok\n");

    DeleteFileW(path);
    printf("shell_folders: done\n");
    return 0;
}
