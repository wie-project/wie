/*
 * Micro-PE HTTP: wininet.dll surface — InternetOpenW / InternetOpenUrlW /
 * InternetReadFile against a real host HTTP server.
 *
 * CRT-linked console program. Opens a session, issues a GET over
 * InternetOpenUrlW (which auto-sends the request), reads the buffered
 * response, and scans the first chunk (which contains the response head) for
 * an HTTP 200 status.
 *
 * Exit codes:
 *   0 — ok (HTTP 200 seen)
 *   1 — InternetOpenW failed
 *   2 — InternetOpenUrlW failed (connect / URL error)
 *   3 — non-200 response (or no data)
 */

#include <windows.h>
#include <wininet.h>
#include <stdio.h>
#include <string.h>

int main(void) {
    HINTERNET session;
    HINTERNET req;
    char buf[16384];
    DWORD read = 0;
    int total = 0;

    printf("wininet_http: InternetOpenW...\n");
    session = InternetOpenW(L"WIE-wininet-test/1.0",
                            INTERNET_OPEN_TYPE_DIRECT, NULL, NULL, 0);
    if (session == NULL) {
        printf("  FAILED (GetLastError=%lu)\n", GetLastError());
        return 1;
    }
    printf("  ok\n");

    printf("wininet_http: InternetOpenUrlW(http://example.com/)...\n");
    req = InternetOpenUrlW(session, L"http://example.com/", NULL, 0,
                           INTERNET_FLAG_RELOAD, 0);
    if (req == NULL) {
        printf("  FAILED (GetLastError=%lu)\n", GetLastError());
        InternetCloseHandle(session);
        return 2;
    }
    printf("  ok\n");

    printf("wininet_http: InternetReadFile loop...\n");
    /* WIE returns FALSE once the buffered response is exhausted; real
     * Windows returns TRUE with read == 0 at EOF — both end this loop. */
    while (InternetReadFile(req, buf + total,
                            (DWORD)(sizeof(buf) - (size_t)total - 1), &read)
           && read > 0) {
        total += (int)read;
        if (total >= (int)sizeof(buf) - 1) {
            break;
        }
    }
    buf[total] = 0;

    /* The response head (status line + headers) is served as the first
     * bytes of the stream, so the status code is findable here. */
    if (strstr(buf, " 200 ") == NULL && strstr(buf, "200 OK") == NULL) {
        printf("  FAILED (no HTTP 200 in response head)\n%.256s\n", buf);
        InternetCloseHandle(req);
        InternetCloseHandle(session);
        return 3;
    }

    printf("wininet_http: HTTP 200 ok, %d bytes read\n", total);
    InternetCloseHandle(req);
    InternetCloseHandle(session);
    printf("wininet_http: done\n");
    return 0;
}
