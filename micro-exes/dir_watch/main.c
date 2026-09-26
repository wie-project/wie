/*
 * CRT-linked console micro: directory change notifications.
 *
 * CreateDirectoryW(C:\watch_test) → FindFirstChangeNotificationW → a worker
 * thread creates C:\watch_test\file.txt after ~200 ms → WaitForSingleObject
 * must return WAIT_OBJECT_0 → FindNextChangeNotification → close → exit 0.
 *
 * Exercises: FindFirstChangeNotificationW, FindNextChangeNotification,
 * FindCloseChangeNotification, WaitForSingleObject on the notification handle.
 *
 * Exit codes:
 *   0 — ok
 *   1 — CreateDirectoryW failed (and the dir does not already exist)
 *   2 — FindFirstChangeNotificationW failed
 *   3 — CreateThread failed
 *   4 — WaitForSingleObject returned something other than WAIT_OBJECT_0
 *   5 — FindNextChangeNotification failed
 *   6 — FindCloseChangeNotification failed
 *   7 — worker thread failed to create/write the file
 */

#include <windows.h>
#include <stdio.h>

static const wchar_t DIR_PATH[] = L"C:\\watch_test";
static const wchar_t FILE_PATH[] = L"C:\\watch_test\\file.txt";

/*
 * Opt-in short mode: WIE_SHORT=1 in the guest environment (injected host-side
 * through WIE_GUEST_ENV="WIE_SHORT=1") shortens the worker's pre-change
 * settle delay below. When WIE_SHORT is absent the delay is the full 200 ms,
 * so the default path is exactly the historical behaviour. Short mode only
 * trims the *wait*; the assertion (WaitForSingleObject must report the
 * change) is unchanged either way — WIE's directory-watch object queues
 * pending records, so a change that lands before the wait is still latched.
 */
#define SETTLE_MS_DEFAULT 200
#define SETTLE_MS_SHORT   50

static int settle_ms(void) {
    char buf[16];
    DWORD n = GetEnvironmentVariableA("WIE_SHORT", buf, sizeof(buf));
    return (n == 1 && buf[0] == '1') ? SETTLE_MS_SHORT : SETTLE_MS_DEFAULT;
}

static DWORD WINAPI create_file_worker(LPVOID param) {
    HANDLE file;
    DWORD written = 0;
    const char payload[] = "dir watch event\n";

    (void)param;
    /* Let the primary reach WaitForSingleObject before the change fires. */
    Sleep((DWORD)settle_ms());

    file = CreateFileW(FILE_PATH, GENERIC_WRITE, 0, NULL, CREATE_ALWAYS,
                       FILE_ATTRIBUTE_NORMAL, NULL);
    if (file == INVALID_HANDLE_VALUE) {
        return 7;
    }
    if (!WriteFile(file, payload, (DWORD)sizeof(payload) - 1, &written, NULL)) {
        CloseHandle(file);
        return 7;
    }
    CloseHandle(file);
    return 0;
}

int main(void) {
    HANDLE change = INVALID_HANDLE_VALUE;
    HANDLE thread = NULL;
    DWORD tid = 0;
    DWORD wait;
    DWORD worker_code = 0;

    /* Re-runs are fine: ERROR_ALREADY_EXISTS means the directory is there. */
    if (!CreateDirectoryW(DIR_PATH, NULL)) {
        if (GetLastError() != ERROR_ALREADY_EXISTS) {
            printf("dir_watch: CreateDirectoryW failed (error %lu)\n", GetLastError());
            return 1;
        }
    }

    change = FindFirstChangeNotificationW(
        DIR_PATH, TRUE, FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE);
    if (change == INVALID_HANDLE_VALUE || change == NULL) {
        printf("dir_watch: FindFirstChangeNotificationW failed (error %lu)\n", GetLastError());
        return 2;
    }

    thread = CreateThread(NULL, 0, create_file_worker, NULL, 0, &tid);
    if (thread == NULL) {
        printf("dir_watch: CreateThread failed (error %lu)\n", GetLastError());
        FindCloseChangeNotification(change);
        return 3;
    }

    wait = WaitForSingleObject(change, INFINITE);
    if (wait != WAIT_OBJECT_0) {
        printf("dir_watch: WaitForSingleObject returned %lu\n", wait);
        CloseHandle(thread);
        FindCloseChangeNotification(change);
        return 4;
    }

    if (!FindNextChangeNotification(change)) {
        printf("dir_watch: FindNextChangeNotification failed (error %lu)\n", GetLastError());
        CloseHandle(thread);
        FindCloseChangeNotification(change);
        return 5;
    }

    if (!FindCloseChangeNotification(change)) {
        printf("dir_watch: FindCloseChangeNotification failed (error %lu)\n", GetLastError());
        CloseHandle(thread);
        return 6;
    }

    /* The worker must have written the file; join it to confirm. */
    WaitForSingleObject(thread, INFINITE);
    if (!GetExitCodeThread(thread, &worker_code) || worker_code != 0) {
        printf("dir_watch: worker failed (code %lu)\n", worker_code);
        CloseHandle(thread);
        return 7;
    }
    CloseHandle(thread);

    /* Best-effort cleanup so re-runs start from a clean directory. */
    DeleteFileW(FILE_PATH);
    RemoveDirectoryW(DIR_PATH);

    return 0;
}
