/*
 * Micro-PE HTTP.1: raw HTTP/1.1 GET over the real WS2_32 handlers.
 *
 * CRT-linked console program. Resolves "example.com" through the host
 * resolver, connects, sends a minimal GET, prints the status line and
 * response body, and reports the result.
 *
 * Exit codes:
 *   0 — ok (HTTP 200)
 *   1 — WSAStartup failed
 *   2 — getaddrinfo failed
 *   3 — socket failed
 *   4 — connect failed
 *   5 — send failed
 *   6 — recv failed
 *   7 — non-200 status
 */

#include <winsock2.h>
#include <ws2tcpip.h>
#include <windows.h>
#include <stdio.h>
#include <string.h>

int main(void) {
    WSADATA wsa;
    struct addrinfo hints;
    struct addrinfo *res = NULL;
    SOCKET s;
    char req[256];
    char buf[4096];
    int n;
    int total = 0;

    printf("http_get: WSAStartup(2.2)...\n");
    if (WSAStartup(MAKEWORD(2, 2), &wsa) != 0) {
        printf("  FAILED\n");
        return 1;
    }
    printf("  ok\n");

    memset(&hints, 0, sizeof(hints));
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_protocol = IPPROTO_TCP;

    printf("http_get: getaddrinfo(\"example.com\", \"80\")...\n");
    if (getaddrinfo("example.com", "80", &hints, &res) != 0) {
        printf("  FAILED\n");
        return 2;
    }
    printf("  resolved to %s\n",
           inet_ntoa(((struct sockaddr_in *)res->ai_addr)->sin_addr));

    printf("http_get: socket...\n");
    s = socket(AF_INET, SOCK_STREAM, 0);
    if (s == INVALID_SOCKET) {
        printf("  FAILED\n");
        return 3;
    }
    printf("  ok\n");

    printf("http_get: connect...\n");
    if (connect(s, res->ai_addr, (int)res->ai_addrlen) == SOCKET_ERROR) {
        printf("  FAILED (ws err %d)\n", WSAGetLastError());
        return 4;
    }
    printf("  connected\n");
    freeaddrinfo(res);

    sprintf(req,
            "GET / HTTP/1.1\r\n"
            "Host: example.com\r\n"
            "Connection: close\r\n"
            "\r\n");
    printf("http_get: send %d bytes...\n", (int)strlen(req));
    n = send(s, req, (int)strlen(req), 0);
    if (n < 0) {
        printf("  FAILED\n");
        return 5;
    }
    printf("  ok\n");

    printf("http_get: recv response...\n");
    while ((n = recv(s, buf + total, (int)sizeof(buf) - total - 1, 0)) > 0) {
        total += n;
        if (total >= (int)sizeof(buf) - 1) {
            break;
        }
    }
    if (n < 0) {
        printf("  FAILED\n");
        return 6;
    }
    buf[total] = 0;

    /* Status line is the first CRLF-terminated line. */
    {
        char *status = buf;
        char *end = strchr(status, '\r');
        if (end != NULL) {
            *end = 0;
        }
        printf("  status: %s\n", status);
        if (strstr(status, " 200 ") == NULL) {
            printf("http_get: FAILED (non-200)\n");
            return 7;
        }
    }

    printf("http_get: %d bytes total\n", total);
    printf("http_get: done\n");
    closesocket(s);
    WSACleanup();
    return 0;
}
