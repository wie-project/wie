// Guess the Price — number guessing game ("juste prix").
// Compile: x86_64-w64-mingw32-gcc -O2 -ffreestanding -fno-stack-protector
//   -fno-asynchronous-unwind-tables -nostdlib
//   -Wl,--subsystem,console -Wl,--entry,entry -o out.exe main.c -lkernel32

void __stdcall ExitProcess(unsigned int);
void* __stdcall GetStdHandle(unsigned int);
int __stdcall ReadFile(void*, void*, unsigned long, unsigned long*, void*);
int __stdcall WriteFile(void*, const void*, unsigned long, unsigned long*, void*);
unsigned long __stdcall GetTickCount(void);

static void* hOut, *hIn;
static unsigned int rng;
static void seed(unsigned int s) { rng = s; }
static unsigned int rnd(unsigned int m) {
    rng = rng * 1103515245 + 12345;
    return (rng >> 16) % m;
}

static void puts(const char* s) {
    unsigned long n, l=0; while(s[l]) l++;
    WriteFile(hOut, s, l, &n, 0);
    WriteFile(hOut, "\r\n", 2, &n, 0);
}
static void putint(unsigned int v) {
    char b[12], t[12]; int i=0,j=0; unsigned long n;
    if (!v) { WriteFile(hOut, "0",1,&n,0); return; }
    while(v) { t[i++]='0'+v%10; v/=10; }
    while(i) b[j++]=t[--i]; b[j]=0;
    WriteFile(hOut,b,j,&n,0);
}

static int read_line(char* buf, int max) {
    unsigned long n; int i=0;
    while (i < max-1) {
        char c;
        if (!ReadFile(hIn, &c, 1, &n, 0) || n == 0) break;
        if (c == '\n') break;
        if (c == '\r') { ReadFile(hIn, &c, 1, &n, 0); break; }
        buf[i++] = c;
    }
    buf[i] = 0;
    return i;
}
static unsigned int parse_uint(const char* s) {
    unsigned int v = 0;
    while (*s >= '0' && *s <= '9') v = v * 10 + (*s++ - '0');
    return v;
}

void entry(void) {
    hOut = GetStdHandle(0xFFFFFFF5); // STD_OUTPUT_HANDLE
    hIn  = GetStdHandle(0xFFFFFFF6); // STD_INPUT_HANDLE
    seed(GetTickCount());
    unsigned int price = rnd(1000) + 1;
    char buf[32]; int attempts = 0;

    puts("=== Guess the Price ===");
    puts("I'm thinking of a number between 1 and 1000.");

    while (1) {
        puts("Enter your guess:");
        if (read_line(buf, sizeof(buf)) == 0) break;
        unsigned int guess = parse_uint(buf); attempts++;
        if (guess == 0 && buf[0] != '0') { puts("Invalid."); continue; }
        if (guess < price) puts("Higher!");
        else if (guess > price) puts("Lower!");
        else {
            puts("Correct!");
            unsigned long n;
            WriteFile(hOut, "You found it in ", 16, &n, 0);
            putint(attempts);
            WriteFile(hOut, " attempts!\r\n", 12, &n, 0);
            break;
        }
    }
    puts("Thanks for playing!");
    ExitProcess(0);
}
