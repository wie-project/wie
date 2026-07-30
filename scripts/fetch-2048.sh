#!/usr/bin/env bash
# Fetch and compile the terminal version of 2048 from mevdschee/2048.c
# into real_exes/.
# Requires: curl, x86_64-w64-mingw32-gcc (Homebrew mingw-w64).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$ROOT/real_exes"
SOURCE="https://raw.githubusercontent.com/mevdschee/2048.c/master/2048.c"

FORCE="${1:-}"
if [[ -f "$DEST/2048.exe" ]] && [[ -f "$DEST/2048.c" ]] && [[ "$FORCE" != "--force" ]]; then
  echo "2048 already present at $DEST/2048.exe (pass --force to re-fetch)"
  file "$DEST/2048.exe"
  exit 0
fi

if ! command -v x86_64-w64-mingw32-gcc &>/dev/null; then
  echo "Error: x86_64-w64-mingw32-gcc not found."
  echo "Install with: brew install mingw-w64"
  exit 1
fi

mkdir -p "$DEST"

echo "Downloading 2048.c from mevdschee/2048.c…"
curl -fL -o "$DEST/2048.c" "$SOURCE"

echo "Patching for mingw-w64 (replace POSIX termios with Windows console API)…"
SRC="$DEST/2048.c"
python3 << PYEOF
with open("$SRC") as f:
    src = f.read()

# Replace POSIX includes
src = src.replace('#define _XOPEN_SOURCE 500 // for: usleep\n', '')
src = src.replace('#include <unistd.h>', '#include <windows.h>')
src = src.replace('#include <termios.h>', '#include <conio.h>')

# Replace setBufferedInput with Windows console mode version
old_func = "void setBufferedInput(bool enable)\n{\n\tstatic bool enabled = true;\n\tstatic struct termios old;\n\tstruct termios new;\n\n\tif (enable && !enabled)\n\t{\n\t\t// restore the former settings\n\t\ttcsetattr(STDIN_FILENO, TCSANOW, &old);\n\t\t// set the new state\n\t\tenabled = true;\n\t}\n\telse if (!enable && enabled)\n\t{\n\t\t// get the terminal settings for standard input\n\t\ttcgetattr(STDIN_FILENO, &new);\n\t\t// we want to keep the old setting to restore them at the end\n\t\told = new;\n\t\t// disable canonical mode (buffered i/o) and local echo\n\t\tnew.c_lflag &= (~ICANON & ~ECHO);\n\t\t// set the new settings immediately\n\t\ttcsetattr(STDIN_FILENO, TCSANOW, &new);\n\t\t// set the new state\n\t\tenabled = false;\n\t}\n}"

new_func = "void setBufferedInput(bool enable)\n{\n\tstatic HANDLE hStdin = INVALID_HANDLE_VALUE;\n\tstatic DWORD oldMode = 0;\n\tstatic bool enabled = true;\n\tif (enable && !enabled) {\n\t\tif (hStdin != INVALID_HANDLE_VALUE)\n\t\t\tSetConsoleMode(hStdin, oldMode);\n\t\tenabled = true;\n\t} else if (!enable && enabled) {\n\t\thStdin = GetStdHandle(STD_INPUT_HANDLE);\n\t\tGetConsoleMode(hStdin, &oldMode);\n\t\tSetConsoleMode(hStdin, oldMode & ~(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT));\n\t\tenabled = false;\n\t}\n}"

src = src.replace(old_func, new_func)

# Replace usleep with Sleep
src = src.replace('usleep(150 * 1000)', 'Sleep(150)')

with open("$SRC", 'w') as f:
    f.write(src)
PYEOF

echo "Compiling with x86_64-w64-mingw32-gcc…"
x86_64-w64-mingw32-gcc -O2 -o "$DEST/2048.exe" "$DEST/2048.c"

echo "done:"
file "$DEST/2048.exe"
