#include <windows.h>
#include <stdio.h>

int main() {
    HANDLE hOut = GetStdHandle(STD_OUTPUT_HANDLE);
    COORD pos = {5, 3};
    DWORD written;
    const char text[] = "CELL TEST";
    FillConsoleOutputCharacterA(hOut, '#', 80, pos, &written);
    SetConsoleCursorPosition(hOut, pos);
    WriteConsoleA(hOut, text, sizeof(text)-1, &written, NULL);
    return 0;
}
