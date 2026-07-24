// Test: __try/__except catching integer divide-by-zero.
// Requires: llvm-mingw (clang) — GCC's mingw does not support __try/__except.
// Compile with: x86_64-w64-mingw32-clang -fms-extensions -o out.exe main.cpp
// exit 0 = caught and handled, exit 1 = wrong path, exit 2 = unhandled.
extern "C" void __stdcall ExitProcess(unsigned int code);

int main() {
    int caught = 0;
    int x = 1, y = 0;
    __try {
        x = x / y;  // INT_DIVIDE_BY_ZERO
        ExitProcess(2);
    } __except (1) {  // EXCEPTION_EXECUTE_HANDLER
        caught = 1;
    }
    ExitProcess(caught ? 0 : 1);
}
