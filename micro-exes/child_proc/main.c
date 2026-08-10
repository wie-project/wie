/*
 * Child process for the CreateProcessW micro (spawn_child).
 *
 * CRT-linked Win64 console app. Prints its identity, then exits with the
 * sentinel code 42 via ExitProcess (the parent asserts it).
 *
 * The suite must stage this exe in the SAME bottle as spawn_child.exe,
 * at the drive_c root (the parent spawns `C:\child_proc.exe`).
 */

#include <stdio.h>
#include <windows.h>

int main(int argc, char **argv) {
    printf("child alive\n");
    if (argc >= 1 && argv[0] != NULL) {
        printf("child argv[0] = %s\n", argv[0]);
    } else {
        printf("child argv[0] = <missing>\n");
    }
    fflush(stdout);
    ExitProcess(42);
}
