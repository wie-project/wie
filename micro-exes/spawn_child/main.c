/*
 * CreateProcessW micro: spawn `C:\child_proc.exe` in-process, wait for it,
 * and assert its exit code is 42 (the child's sentinel).
 *
 * CRT-linked Win64 console app. Exercises the full process-wait surface:
 * CreateProcessW → WaitForSingleObject(process) → GetExitCodeProcess →
 * CloseHandle(both). Exits 0 on success, nonzero with a printed reason
 * otherwise.
 *
 * The suite must stage child_proc.exe in the SAME bottle as this exe, at
 * the drive_c root (the spawn path is a compile-time `C:\child_proc.exe`).
 */

#include <windows.h>
#include <stdio.h>

int main(void) {
    PROCESS_INFORMATION pi;
    STARTUPINFOW si;
    DWORD wait_result;
    DWORD exit_code = 0;

    ZeroMemory(&pi, sizeof(pi));
    ZeroMemory(&si, sizeof(si));
    si.cb = sizeof(si);

    if (!CreateProcessW(
            L"C:\\child_proc.exe", /* lpApplicationName */
            NULL,                  /* lpCommandLine */
            NULL,                  /* lpProcessAttributes */
            NULL,                  /* lpThreadAttributes */
            FALSE,                 /* bInheritHandles */
            0,                     /* dwCreationFlags */
            NULL,                  /* lpEnvironment (inherit) */
            NULL,                  /* lpCurrentDirectory (inherit) */
            &si,                   /* lpStartupInfo */
            &pi)) {                /* lpProcessInformation */
        printf("spawn_child: CreateProcessW failed: %u\n",
               (unsigned)GetLastError());
        return 1;
    }

    if (pi.hProcess == NULL || pi.dwProcessId == 0) {
        printf("spawn_child: PROCESS_INFORMATION not filled\n");
        return 2;
    }

    wait_result = WaitForSingleObject(pi.hProcess, INFINITE);
    if (wait_result != WAIT_OBJECT_0) {
        printf("spawn_child: WaitForSingleObject returned %u\n",
               (unsigned)wait_result);
        return 3;
    }

    if (!GetExitCodeProcess(pi.hProcess, &exit_code)) {
        printf("spawn_child: GetExitCodeProcess failed\n");
        return 4;
    }
    printf("spawn_child: child exit code = %u\n", (unsigned)exit_code);
    if (exit_code != 42) {
        printf("spawn_child: expected 42\n");
        return 5;
    }

    CloseHandle(pi.hThread);
    CloseHandle(pi.hProcess);
    printf("spawn_child: OK\n");
    return 0;
}
