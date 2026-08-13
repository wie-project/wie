#!/usr/bin/env bash
# Fetch a real Windows app into real_exes/ for testing (the apps are
# gitignored — run this once per app, or pass --force to re-fetch).
#
# Usage:
#   ./scripts/fetch.sh <app> [--force]
#   ./scripts/fetch.sh --list | --help
#
# Apps:
#   7za       — 7-Zip Extra x64 console PE        [curl + p7zip]
#   2048      — mevdschee/2048.c terminal game    [curl + mingw-w64]
#   notepad   — katahiromz/RNotepad               [git + cmake + mingw-w64]
#   doomretro — DOOM Retro v6.3 win64 (SDL2/GL)   [curl + unzip]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$ROOT/real_exes"

usage() {
  sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
}

# ---------------------------------------------------------------- 7-Zip
fetch_7za() {
  # One-time setup: download the Windows x64 7-Zip Extra PE into real_exes/.
  # Requires: curl, Homebrew p7zip (for unpacking the Extra archive).
  if [[ -f "$DEST/7za.exe" ]]; then
    echo "7za.exe already present at $DEST/7za.exe"
    file "$DEST/7za.exe"
    exit 0
  fi

  if ! command -v 7za &>/dev/null; then
    echo "Installing p7zip (needed to unpack the Extra archive)…"
    brew install p7zip
  fi

  mkdir -p "$DEST"

  VER=26.02
  VER_COMPACT=2602
  TMP="/tmp/7z-extra-$$"
  mkdir -p "$TMP"

  curl -fL -o "$TMP/extra.7z" \
    "https://github.com/ip7z/7zip/releases/download/${VER}/7z${VER_COMPACT}-extra.7z"

  7za x -y -o"$TMP/out" "$TMP/extra.7z"

  cp -f "$TMP/out/x64/7za.exe"  "$DEST/"
  cp -f "$TMP/out/x64/7za.dll"  "$DEST/"
  cp -f "$TMP/out/x64/7zxa.dll" "$DEST/"

  rm -rf "$TMP"

  echo "downloaded:"
  file "$DEST/7za.exe"
}

# ------------------------------------------------------------------ 2048
fetch_2048() {
  # Fetch and compile the terminal version of 2048 from mevdschee/2048.c
  # into real_exes/. Requires: curl, x86_64-w64-mingw32-gcc (Homebrew mingw-w64).
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
  curl -fL -o "$DEST/2048.c" \
    "https://raw.githubusercontent.com/mevdschee/2048.c/master/2048.c"

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
}

# ---------------------------------------------------------------- Notepad
fetch_notepad() {
  # One-time setup: clone katahiromz/RNotepad and cross-build notepad.exe.
  # Requires: git, cmake, x86_64-w64-mingw32-gcc (Homebrew mingw-w64).
  # Source tree lives inside real_exes/ (gitignored) so it never shows in git status.
  SRC="$DEST/.rnotepad-src"

  if [[ -f "$DEST/notepad.exe" ]]; then
    echo "notepad.exe already present at $DEST/notepad.exe"
    file "$DEST/notepad.exe"
    exit 0
  fi

  mkdir -p "$DEST"

  if [[ ! -d "$SRC" ]]; then
    git clone --depth 1 https://github.com/katahiromz/RNotepad "$SRC"
  fi

  # Drop any stale configure (e.g. one run without CMAKE_SYSTEM_NAME that cached
  # native Apple flags like -arch arm64, which mingw-gcc rejects).
  if [[ -f "$SRC/build/CMakeCache.txt" ]]; then
    rm -rf "$SRC/build"
  fi

  # CMAKE_SYSTEM_NAME=Windows keeps CMake from injecting macOS-only flags
  # (-arch arm64) into the mingw compiler command line.
  # -mcrtdll=msvcrt-os: Homebrew mingw-w64 defaults to the UCRT; the plan and
  # the real Win7 notepad target assume msvcrt.dll, which WIE supports
  # (micro-exes use it). Note the -os variant is required: Homebrew aliases
  # libmsvcrt.a to the UCRT import lib (byte-identical to libucrt.a), and plain
  # -mcrtdll=msvcrt is silently ignored, still linking api-ms-win-crt-*.
  # libmsvcrt-os.a is the real msvcrt.dll import library.
  cmake -S "$SRC" -B "$SRC/build" -G "Unix Makefiles" \
    -DCMAKE_SYSTEM_NAME=Windows \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_C_COMPILER=x86_64-w64-mingw32-gcc \
    -DCMAKE_RC_COMPILER=x86_64-w64-mingw32-windres \
    -DCMAKE_C_FLAGS="-mcrtdll=msvcrt-os"

  cmake --build "$SRC/build" --parallel

  # Upstream output name varies (notepad.exe vs RNotepad.exe); pick whichever exists.
  EXE="$(find "$SRC/build" -maxdepth 2 -iname '*.exe' -type f | head -n 1)"
  if [[ -z "$EXE" ]]; then
    echo "error: no .exe produced in $SRC/build" >&2
    exit 1
  fi

  cp -f "$EXE" "$DEST/notepad.exe"
  echo "built:"
  file "$DEST/notepad.exe"
}

# ------------------------------------------------------------ DOOM Retro
fetch_doomretro() {
  # One-time setup: download the DOOM Retro v6.3 win64 bundle (game exe +
  # WAD + SDL2/SDL2_mixer + audio codecs) into real_exes/doomretro/.
  # Requires: curl, unzip.
  DIR="$DEST/doomretro"
  if [[ -f "$DIR/doomretro.exe" ]]; then
    echo "doomretro already present at $DIR/doomretro.exe"
    file "$DIR/doomretro.exe"
    exit 0
  fi

  mkdir -p "$DIR"
  TMP="/tmp/doomretro-$$"
  mkdir -p "$TMP"

  curl -fL -o "$TMP/doomretro.zip"     "https://github.com/bradharding/doomretro/releases/download/v6.3/doomretro-6.3-win64.zip"

  unzip -q "$TMP/doomretro.zip" -d "$DIR"

  rm -rf "$TMP"

  echo "downloaded:"
  file "$DIR/doomretro.exe"
  echo "note: needs a Doom WAD (doom1.wad etc.) next to the exe to actually run"
}

# ------------------------------------------------------------------ main
main() {
  local app="${1:-}"
  local force=""
  if [[ "${2:-}" == "--force" ]]; then
    force="--force"
  fi

  case "$app" in
    "" | -h | --help | help)
      usage
      [[ "$app" == "" ]] && exit 1 || exit 0
      ;;
    --list | list)
      echo "Available apps:"
      echo "  7za      — 7-Zip Extra x64 console PE        [curl + p7zip]"
      echo "  2048     — mevdschee/2048.c terminal game    [curl + mingw-w64]"
      echo "  notepad  — katahiromz/RNotepad               [git + cmake + mingw-w64]"
      echo "  doomretro — DOOM Retro v6.3 win64 (SDL2/GL)   [curl + unzip]"
      echo
      echo "Fetch one:  ./scripts/fetch.sh <app>"
      exit 0
      ;;
    7za | 7zip | 7-Zip)
      fetch_7za
      ;;
    2048)
      fetch_2048 "$force"
      ;;
    notepad | rnotepad)
      fetch_notepad
      ;;
    doomretro | doom)
      fetch_doomretro
      ;;
    *)
      echo "Error: unknown app '$app'" >&2
      echo >&2
      usage >&2
      exit 1
      ;;
  esac
}

main "$@"
