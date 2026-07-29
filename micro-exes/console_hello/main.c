#include <windows.h>
#include <stdio.h>

int main() {
    HANDLE hOut = GetStdHandle(STD_OUTPUT_HANDLE);
    HANDLE hIn = GetStdHandle(STD_INPUT_HANDLE);
    DWORD written, read;
    const char msg[] = "Hello from console\r\n> ";
    WriteConsoleA(hOut, msg, sizeof(msg) - 1, &written, NULL);

    char buf[128];
    if (ReadConsoleA(hIn, buf, sizeof(buf) - 1, &read, NULL)) {
        buf[read] = '\0';
        WriteConsoleA(hOut, "You typed: ", 11, &written, NULL);
        WriteConsoleA(hOut, buf, read, &written, NULL);
    }
    return 0;
}
