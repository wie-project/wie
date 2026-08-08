# Snake

The classic snake game from the micro-exes suite.

## Build

```bash
make -C micro-exes snake
```

## Run

```bash
./target/release/wie run --persistent --max-api 100000000 micro-exes/out/snake.exe
```

**Controls:** `WASD` or arrow keys, `q` to quit.

Also builds natively on macOS for comparison:

```bash
clang -std=c11 -O2 -o snake micro-exes/snake/main.c && ./snake
```

## Notes

The source lives at `micro-exes/interactive/snake.c` (also mirrored at
the original `micro-exes/snake/main.c`). The game uses the same input
and ANSI rendering approach as [2048](2048.md).
