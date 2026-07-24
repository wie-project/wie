// Test: __try/__except catching an access violation (write to NULL).
// Requires: llvm-mingw (clang) — GCC's mingw does not support __try/__except.
// Compile with: x86_64-w64-mingw32-clang -fms-extensions -o out.exe main.cpp
// exit 0 = caught and handled, exit 1 = wrong path, exit 2 = unhandled.
extern "C" void __stdcall ExitProcess(unsigned int code);

int main() {
    int caught = 0;
    __try {
        *(volatile int*)0 = 42;  // ACCESS_VIOLATION
        ExitProcess(2);
    } __except (1) {  // EXCEPTION_EXECUTE_HANDLER
        caught = 1;
    }
    ExitProcess(caught ? 0 : 1);
}
