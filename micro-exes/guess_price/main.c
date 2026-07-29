// Guess the Price — normal C with CRT, headers, and standard I/O.
// Compile: x86_64-w64-mingw32-gcc -O2 -o out.exe main.c
#include <stdio.h>
#include <stdlib.h>
#include <windows.h>
#include <time.h>

int main() {
    srand(GetTickCount());
    int price = rand() % 1000 + 1;
    int guess, attempts = 0;

    printf("=== Guess the Price ===\n");
    printf("I'm thinking of a number between 1 and 1000.\n");

    while (1) {
        printf("\nEnter your guess: ");
        fflush(stdout);
        char line[32];
        if (!fgets(line, sizeof(line), stdin)) break;
        guess = atoi(line);
        attempts++;

        if (guess < price)      printf("Higher!\n");
        else if (guess > price) printf("Lower!\n");
        else {
            printf("Correct!\n");
            printf("You found it in %d attempts!\n", attempts);
            break;
        }
    }

    printf("Thanks for playing!\n");
    return 0;
}
