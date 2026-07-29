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

echo "Compiling with x86_64-w64-mingw32-gcc…"
x86_64-w64-mingw32-gcc -O2 -o "$DEST/2048.exe" "$DEST/2048.c"

echo "done:"
file "$DEST/2048.exe"
