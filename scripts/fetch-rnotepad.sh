#!/usr/bin/env bash
# One-time setup: clone katahiromz/RNotepad and cross-build notepad.exe.
# Requires: git, cmake, x86_64-w64-mingw32-gcc (Homebrew mingw-w64).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# Source tree lives inside real_exes/ (gitignored) so it never shows in git status.
SRC="$ROOT/real_exes/.rnotepad-src"
DEST="$ROOT/real_exes"

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
