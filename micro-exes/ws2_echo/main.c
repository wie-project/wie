/*
 * Micro-PE WS.1: single-process loopback TCP echo through the real WS2_32
 * handlers (crates/wie-winapi/src/ws2_32.rs).
 *
 * Freestanding PE64. One process plays both endpoints:
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

static void format_port(unsigned short port, char out[8]) {
    char tmp[8];
    int n = 0;
    int i;
    if (port == 0) {
        tmp[n++] = '0';
    } else {
        while (port > 0) {
            tmp[n++] = (char)('0' + (port % 10));
            port = (unsigned short)(port / 10);
        }
    }
    i = 0;
    while (n > 0 && i < 7) {
        out[i++] = tmp[--n];
    }
    out[i] = 0;
}

static void zero_bytes(void *p, int len) {
    char *c = (char *)p;
    int i;
    for (i = 0; i < len; i++) {
        c[i] = 0;
    }
}

void entry(void) {
    WSADATA wsa;
    SOCKET server;
    SOCKET client;
    SOCKET conn;
    struct sockaddr_in srv;
    struct sockaddr_in peer;
    int len;
    struct addrinfo hints;
    struct addrinfo *res;
    char portbuf[8];
    char buf[16];
    int n;

    if (WSAStartup(MAKEWORD(2, 2), &wsa) != 0) {
        ExitProcess(1);
    }

    server = socket(AF_INET, SOCK_STREAM, 0);
    if (server == INVALID_SOCKET) {
        ExitProcess(2);
    }

    zero_bytes(&srv, sizeof(srv));
    srv.sin_family = AF_INET;
    srv.sin_port = 0; /* ephemeral — the OS assigns the port */
    srv.sin_addr.s_addr = inet_addr("127.0.0.1");

    if (bind(server, (struct sockaddr *)&srv, sizeof(srv)) == SOCKET_ERROR) {
        ExitProcess(3);
    }
    if (listen(server, 1) == SOCKET_ERROR) {
        ExitProcess(4);
    }

    len = sizeof(peer);
    if (getsockname(server, (struct sockaddr *)&peer, &len) == SOCKET_ERROR) {
        ExitProcess(5);
    }
    if (peer.sin_family != AF_INET || peer.sin_port == 0) {
        ExitProcess(5);
    }

    client = socket(AF_INET, SOCK_STREAM, 0);
    if (client == INVALID_SOCKET) {
        ExitProcess(6);
    }

    format_port(ntohs(peer.sin_port), portbuf);
    zero_bytes(&hints, sizeof(hints));
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_protocol = IPPROTO_TCP;
    res = NULL;
    if (getaddrinfo("localhost", portbuf, &hints, &res) != 0) {
        ExitProcess(7);
    }
    if (res == NULL || res->ai_addr == NULL
        || res->ai_addrlen != sizeof(struct sockaddr_in)) {
        ExitProcess(7);
    }

    if (connect(client, res->ai_addr, (int)res->ai_addrlen) == SOCKET_ERROR) {
        ExitProcess(8);
    }

    freeaddrinfo(res);
    res = NULL;

    n = send(client, "ping", 4, 0);
    if (n != 4) {
        ExitProcess(9);
    }

    conn = accept(server, NULL, NULL);
    if (conn == INVALID_SOCKET) {
        ExitProcess(10);
    }

    n = recv(conn, buf, 4, 0);
    if (n != 4) {
        ExitProcess(11);
    }
    if (buf[0] != 'p' || buf[1] != 'i' || buf[2] != 'n' || buf[3] != 'g') {
        ExitProcess(11);
    }

    n = send(conn, "pong", 4, 0);
    if (n != 4) {
        ExitProcess(12);
    }

    n = recv(client, buf, 4, 0);
    if (n != 4) {
        ExitProcess(13);
    }
    if (buf[0] != 'p' || buf[1] != 'o' || buf[2] != 'n' || buf[3] != 'g') {
        ExitProcess(13);
    }

    if (closesocket(conn) == SOCKET_ERROR) {
        ExitProcess(14);
    }
    if (closesocket(client) == SOCKET_ERROR) {
        ExitProcess(14);
    }
    if (closesocket(server) == SOCKET_ERROR) {
        ExitProcess(14);
    }

    WSACleanup();
    ExitProcess(0);
}
