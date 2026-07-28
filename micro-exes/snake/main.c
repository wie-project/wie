/*
 * snake.c - A classic Snake game in portable C.
 *
 * Builds and runs on Windows, Linux, and macOS using only the
 * standard C library plus the platform's own C runtime headers
 * (conio.h/windows.h on Windows, termios.h on POSIX) for
 * non-blocking keyboard input. No third-party libraries required.
 *
 * Build:
 *   Linux/macOS:      gcc -std=c11 -O2 -o snake snake.c
 *   Windows (MinGW):  gcc -std=c11 -O2 -o snake.exe snake.c
 *   Windows (MSVC):   cl /std:c11 snake.c
 *
 * Controls: WASD or arrow keys to move, 'q' to quit.
 */

#if !defined(_WIN32)
    /* Expose usleep() and other POSIX.1-2008 declarations from unistd.h;
     * must be defined before any system header is included. */
    #define _POSIX_C_SOURCE 200809L
#endif

#include <stdio.h>
#include <stdlib.h>
#include <stdbool.h>
#include <string.h>
#include <time.h>

#if defined(_WIN32)
    #include <conio.h>
    #include <windows.h>
#else
    #include <unistd.h>
    #include <termios.h>
    #include <sys/select.h>
#endif

/* ---------- Configuration ---------- */

enum {
    BOARD_WIDTH   = 30,
    BOARD_HEIGHT  = 20,
    MAX_SNAKE_LEN = BOARD_WIDTH * BOARD_HEIGHT,
    TICK_MS_START = 130,
    TICK_MS_MIN   = 60,
    FRAME_BUF_SIZE = 4096
};

typedef enum { DIR_UP, DIR_DOWN, DIR_LEFT, DIR_RIGHT } Direction;

typedef struct {
    int x, y;
} Point;

typedef struct {
    Point cells[MAX_SNAKE_LEN]; /* cells[0] is the head */
    int   length;
    Direction dir;
} Snake;

typedef struct {
    Snake snake;
    Point food;
    int   score;
    bool  game_over;
    unsigned tick_ms;
} Game;

/* ---------- Platform layer ----------
 * Every OS-specific detail of terminal handling is isolated here.
 * The rest of the program (game logic + rendering) is plain,
 * portable C with no #ifdef in sight.
 */

#if defined(_WIN32)

static void platform_cleanup(void) { }

static void platform_init(void) {
    /* conio.h's _kbhit/_getch need no setup on Windows. */
}

static bool platform_key_available(void) {
    return _kbhit() != 0;
}

static int platform_read_key(void) {
    int ch = _getch();
    /* Arrow keys arrive as a two-byte sequence: 0xE0 (or 0) then a code. */
    if (ch == 0 || ch == 0xE0) {
        ch = _getch();
        switch (ch) {
            case 72: return 'w'; /* up    */
            case 80: return 's'; /* down  */
            case 75: return 'a'; /* left  */
            case 77: return 'd'; /* right */
            default: return -1;
        }
    }
    return ch;
}

static void platform_sleep_ms(unsigned ms) { Sleep(ms); }
static void platform_clear_screen(void)    { system("cls"); }

#else /* POSIX: Linux, macOS, BSD */

static struct termios g_original_termios;

static void platform_cleanup(void) {
    tcsetattr(STDIN_FILENO, TCSANOW, &g_original_termios);
    printf("\033[?25h"); /* show cursor again */
    fflush(stdout);
}

static void platform_init(void) {
    tcgetattr(STDIN_FILENO, &g_original_termios);
    struct termios raw = g_original_termios;
    raw.c_lflag &= ~(unsigned)(ICANON | ECHO); /* no line buffering, no echo */
    raw.c_cc[VMIN]  = 0;
    raw.c_cc[VTIME] = 0;
    tcsetattr(STDIN_FILENO, TCSANOW, &raw);
    printf("\033[?25l"); /* hide cursor */
    atexit(platform_cleanup);
}

static bool platform_key_available(void) {
    fd_set fds;
    FD_ZERO(&fds);
    FD_SET(STDIN_FILENO, &fds);
    struct timeval tv = {0, 0};
    return select(STDIN_FILENO + 1, &fds, NULL, NULL, &tv) > 0;
}

static int platform_read_key(void) {
    unsigned char ch;
    if (read(STDIN_FILENO, &ch, 1) != 1) return -1;

    /* Arrow keys arrive as ESC '[' 'A'/'B'/'C'/'D'. */
    if (ch == 27) {
        unsigned char seq[2];
        if (read(STDIN_FILENO, &seq[0], 1) != 1) return 27;
        if (read(STDIN_FILENO, &seq[1], 1) != 1) return 27;
        if (seq[0] == '[') {
            switch (seq[1]) {
                case 'A': return 'w';
                case 'B': return 's';
                case 'C': return 'd';
                case 'D': return 'a';
            }
        }
        return 27;
    }
    return ch;
}

static void platform_sleep_ms(unsigned ms) {
    struct timespec ts = { .tv_sec = ms / 1000, .tv_nsec = (long)(ms % 1000) * 1000000L };
    nanosleep(&ts, NULL);
}
static void platform_clear_screen(void) { printf("\033[H\033[J"); }

#endif

/* ---------- Game logic (fully portable) ---------- */

static Point direction_delta(Direction d) {
    switch (d) {
        case DIR_UP:    return (Point){ 0, -1 };
        case DIR_DOWN:  return (Point){ 0,  1 };
        case DIR_LEFT:  return (Point){-1,  0 };
        case DIR_RIGHT: return (Point){ 1,  0 };
    }
    return (Point){0, 0};
}

static bool opposite_directions(Direction a, Direction b) {
    return (a == DIR_UP && b == DIR_DOWN) || (a == DIR_DOWN && b == DIR_UP) ||
           (a == DIR_LEFT && b == DIR_RIGHT) || (a == DIR_RIGHT && b == DIR_LEFT);
}

static bool point_in_snake(const Snake *s, Point p) {
    for (int i = 0; i < s->length; i++) {
        if (s->cells[i].x == p.x && s->cells[i].y == p.y) return true;
    }
    return false;
}

static void spawn_food(Game *g) {
    Point p;
    do {
        p.x = rand() % BOARD_WIDTH;
        p.y = rand() % BOARD_HEIGHT;
    } while (point_in_snake(&g->snake, p));
    g->food = p;
}

static void game_init(Game *g) {
    g->snake.length = 3;
    g->snake.dir    = DIR_RIGHT;
    for (int i = 0; i < g->snake.length; i++) {
        g->snake.cells[i] = (Point){ BOARD_WIDTH / 2 - i, BOARD_HEIGHT / 2 };
    }
    g->score     = 0;
    g->game_over = false;
    g->tick_ms   = TICK_MS_START;
    spawn_food(g);
}

static void game_handle_input(Game *g) {
    int last_key = -1;
    /* Drain every buffered keystroke and keep only the most recent
     * valid one, so a burst of queued input can't turn the snake
     * back on itself through an intermediate direction. */
    while (platform_key_available()) {
        int key = platform_read_key();
        if (key != -1) last_key = key;
    }
    if (last_key == -1) return;

    Direction requested;
    switch (last_key) {
        case 'w': case 'W': requested = DIR_UP;    break;
        case 's': case 'S': requested = DIR_DOWN;  break;
        case 'a': case 'A': requested = DIR_LEFT;  break;
        case 'd': case 'D': requested = DIR_RIGHT; break;
        case 'q': case 'Q': g->game_over = true;   return;
        default: return;
    }
    if (!opposite_directions(requested, g->snake.dir)) {
        g->snake.dir = requested;
    }
}

static void game_step(Game *g) {
    Snake *s = &g->snake;
    Point delta = direction_delta(s->dir);
    Point new_head = { s->cells[0].x + delta.x, s->cells[0].y + delta.y };

    /* Wall collision */
    if (new_head.x < 0 || new_head.x >= BOARD_WIDTH ||
        new_head.y < 0 || new_head.y >= BOARD_HEIGHT) {
        g->game_over = true;
        return;
    }

    bool eating = (new_head.x == g->food.x && new_head.y == g->food.y);

    /* Self collision. The current tail cell will move out of the way
     * unless we're growing this turn, so it's excluded from the check. */
    int check_len = eating ? s->length : s->length - 1;
    for (int i = 0; i < check_len; i++) {
        if (s->cells[i].x == new_head.x && s->cells[i].y == new_head.y) {
            g->game_over = true;
            return;
        }
    }

    if (s->length > 1) {
        memmove(&s->cells[1], &s->cells[0], (size_t)(s->length - 1) * sizeof(Point));
    }
    s->cells[0] = new_head;

    if (eating) {
        if (s->length < MAX_SNAKE_LEN) s->length++;
        g->score += 10;
        if (g->tick_ms > TICK_MS_MIN) g->tick_ms -= 3;
        spawn_food(g);
    }
}

/* ---------- Rendering ---------- */

static void game_render(const Game *g) {
    /* Build the whole frame in one buffer, then flush it in a single
     * write. Printing cell-by-cell would cause visible flicker. */
    static char frame[FRAME_BUF_SIZE];
    char *p = frame;

    p += sprintf(p, "Score: %d   (WASD/arrows to move, q to quit)\n", g->score);

    p += sprintf(p, "+");
    for (int x = 0; x < BOARD_WIDTH; x++) *p++ = '-';
    p += sprintf(p, "+\n");

    for (int y = 0; y < BOARD_HEIGHT; y++) {
        *p++ = '|';
        for (int x = 0; x < BOARD_WIDTH; x++) {
            char c = ' ';
            if (g->food.x == x && g->food.y == y) {
                c = '*';
            } else {
                for (int i = 0; i < g->snake.length; i++) {
                    if (g->snake.cells[i].x == x && g->snake.cells[i].y == y) {
                        c = (i == 0) ? 'O' : 'o';
                        break;
                    }
                }
            }
            *p++ = c;
        }
        *p++ = '|';
        *p++ = '\n';
    }

    p += sprintf(p, "+");
    for (int x = 0; x < BOARD_WIDTH; x++) *p++ = '-';
    p += sprintf(p, "+\n");
    *p = '\0';

    platform_clear_screen();
    fputs(frame, stdout);
    fflush(stdout);
}

/* ---------- Entry point ---------- */

int main(void) {
    srand((unsigned)time(NULL));
    platform_init();

    Game game;
    game_init(&game);

    while (!game.game_over) {
        game_handle_input(&game);
        game_step(&game);
        game_render(&game);
        platform_sleep_ms(game.tick_ms);
    }

    platform_clear_screen();
    printf("Game over! Final score: %d\n", game.score);

#if defined(_WIN32)
    /* Keep the console window open long enough to read the score
     * when the game was launched by double-click rather than from
     * an already-open terminal. */
    printf("Press any key to exit...");
    _getch();
#endif

    return 0;
}
