/*
 * Micro-PE WS.1: single-process loopback TCP echo through the real WS2_32
 * handlers (crates/wie-winapi/src/ws2_32.rs).
 *
 * CRT-linked console program. One process plays both endpoints:
 *   server = socket/bind(127.0.0.1:0)/listen; learns the port via getsockname;
 *   client = getaddrinfo("localhost", port) → socket/connect;
 *   client sends "ping"; server accept/recv/send "pong"; client verifies.
 *
 * Exit codes:
 *   0  — ok
 *   1  — WSAStartup failed
 *   2  — server socket failed
 *   3  — bind failed
 *   4  — listen failed
 *   5  — getsockname failed (or non-AF_INET / port 0)
 *   6  — client socket failed
 *   7  — getaddrinfo failed (or malformed chain)
 *   8  — connect failed
 *   9  — send(ping) failed
 *   10 — accept failed
 *   11 — server recv mismatch
 *   12 — send(pong) failed
 *   13 — client recv mismatch
 *   14 — closesocket failed
 */

#include <winsock2.h>
#include <ws2tcpip.h>
#include <windows.h>
#include <stdio.h>

int main(void) {
    WSADATA wsa;
    SOCKET server;
    SOCKET client;
    SOCKET conn;
    struct sockaddr_in srv;
    struct sockaddr_in peer;
    int len;
    struct addrinfo hints;
    struct addrinfo *res;
    char buf[16];
    int n;

    printf("ws2_echo: WSAStartup(2.2)...\n");
    if (WSAStartup(MAKEWORD(2, 2), &wsa) != 0) {
        printf("  FAILED\n");
        return 1;
    }
    printf("  ok\n");

    printf("ws2_echo: server socket...\n");
    server = socket(AF_INET, SOCK_STREAM, 0);
    if (server == INVALID_SOCKET) {
        printf("  FAILED\n");
        return 2;
    }
    printf("  ok\n");

    memset(&srv, 0, sizeof(srv));
    srv.sin_family = AF_INET;
    srv.sin_port = 0; /* ephemeral — the OS assigns the port */
    srv.sin_addr.s_addr = inet_addr("127.0.0.1");

    printf("ws2_echo: bind 127.0.0.1:0...\n");
    if (bind(server, (struct sockaddr *)&srv, sizeof(srv)) == SOCKET_ERROR) {
        printf("  FAILED\n");
        return 3;
    }
    printf("ws2_echo: listen(1)...\n");
    if (listen(server, 1) == SOCKET_ERROR) {
        printf("  FAILED\n");
        return 4;
    }

    len = sizeof(peer);
    printf("ws2_echo: getsockname...\n");
    if (getsockname(server, (struct sockaddr *)&peer, &len) == SOCKET_ERROR) {
        printf("  FAILED\n");
        return 5;
    }
    if (peer.sin_family != AF_INET || peer.sin_port == 0) {
        printf("  FAILED (not AF_INET or port 0)\n");
        return 5;
    }
    printf("  server on 127.0.0.1:%u\n", (unsigned)ntohs(peer.sin_port));

    printf("ws2_echo: client socket...\n");
    client = socket(AF_INET, SOCK_STREAM, 0);
    if (client == INVALID_SOCKET) {
        printf("  FAILED\n");
        return 6;
    }
    printf("  ok\n");

    {
        char portbuf[16];
        sprintf(portbuf, "%u", (unsigned)ntohs(peer.sin_port));
        memset(&hints, 0, sizeof(hints));
        hints.ai_family = AF_INET;
        hints.ai_socktype = SOCK_STREAM;
        hints.ai_protocol = IPPROTO_TCP;
        res = NULL;
        printf("ws2_echo: getaddrinfo(\"localhost\", %s)...\n", portbuf);
        if (getaddrinfo("localhost", portbuf, &hints, &res) != 0) {
            printf("  FAILED\n");
            return 7;
        }
        if (res == NULL || res->ai_addr == NULL
            || res->ai_addrlen != sizeof(struct sockaddr_in)) {
            printf("  FAILED (malformed chain)\n");
            return 7;
        }
        printf("  ok\n");

        printf("ws2_echo: connect...\n");
        if (connect(client, res->ai_addr, (int)res->ai_addrlen) == SOCKET_ERROR) {
            printf("  FAILED\n");
            return 8;
        }
        printf("  ok\n");

        freeaddrinfo(res);
        res = NULL;
    }

    printf("ws2_echo: send(\"ping\")...\n");
    n = send(client, "ping", 4, 0);
    if (n != 4) {
        printf("  FAILED (sent %d)\n", n);
        return 9;
    }
    printf("  ok\n");

    printf("ws2_echo: accept...\n");
    conn = accept(server, NULL, NULL);
    if (conn == INVALID_SOCKET) {
        printf("  FAILED\n");
        return 10;
    }
    printf("  ok\n");

    printf("ws2_echo: server recv...\n");
    n = recv(conn, buf, 4, 0);
    if (n != 4) {
        printf("  FAILED (got %d)\n", n);
        return 11;
    }
    if (buf[0] != 'p' || buf[1] != 'i' || buf[2] != 'n' || buf[3] != 'g') {
        printf("  FAILED (bad payload)\n");
        return 11;
    }
    printf("  got \"ping\"\n");

    printf("ws2_echo: send(\"pong\")...\n");
    n = send(conn, "pong", 4, 0);
    if (n != 4) {
        printf("  FAILED\n");
        return 12;
    }
    printf("  ok\n");

    printf("ws2_echo: client recv...\n");
    n = recv(client, buf, 4, 0);
    if (n != 4) {
        printf("  FAILED (got %d)\n", n);
        return 13;
    }
    if (buf[0] != 'p' || buf[1] != 'o' || buf[2] != 'n' || buf[3] != 'g') {
        printf("  FAILED (bad payload)\n");
        return 13;
    }
    printf("  got \"pong\" — round-trip ok\n");

    if (closesocket(conn) == SOCKET_ERROR) {
        printf("ws2_echo: closesocket(conn) FAILED\n");
        return 14;
    }
    if (closesocket(client) == SOCKET_ERROR) {
        printf("ws2_echo: closesocket(client) FAILED\n");
        return 14;
    }
    if (closesocket(server) == SOCKET_ERROR) {
        printf("ws2_echo: closesocket(server) FAILED\n");
        return 14;
    }

    WSACleanup();
    printf("ws2_echo: done\n");
    return 0;
}
