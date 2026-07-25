// C++ exception in two threads: both throw and catch independently.
// Exits 0 if both catches execute.
// Compile: x86_64-w64-mingw32-g++ -static -O2 -o cpp_threads.exe main.cpp
// Note: runs with WIE_CPU=iced (JIT has intermittent setjmp/longjmp issue).
#include <windows.h>

static volatile int g_t1_result = 1;
static volatile int g_t2_result = 1;

// Separate function to prevent JIT from optimizing away the assignment.
static void set_result(volatile int* p, int v) { *p = v; }

static DWORD WINAPI thread1(LPVOID) {
    try {
        throw 1;
    } catch (int val) {
        set_result(&g_t1_result, val - 1);
    }
    ExitThread(0);
    return 0;
}

static DWORD WINAPI thread2(LPVOID) {
    try {
        throw 1;
    } catch (int val) {
        set_result(&g_t2_result, val - 1);
    }
    ExitThread(0);
    return 0;
}

int main() {
    HANDLE h1, h2;
    DWORD tid1 = 0, tid2 = 0;

    h1 = CreateThread(NULL, 0, thread1, NULL, 0, &tid1);
    if (!h1 || h1 == INVALID_HANDLE_VALUE) ExitProcess(1);
    h2 = CreateThread(NULL, 0, thread2, NULL, 0, &tid2);
    if (!h2 || h2 == INVALID_HANDLE_VALUE) ExitProcess(2);

    WaitForSingleObject(h1, INFINITE);
    WaitForSingleObject(h2, INFINITE);

    if (g_t1_result != 0) ExitProcess(3);
    if (g_t2_result != 0) ExitProcess(4);

    CloseHandle(h1);
    CloseHandle(h2);
    ExitProcess(0);
    return 0;
}
