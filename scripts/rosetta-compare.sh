#!/usr/bin/env bash
# WIE vs Rosetta 2 on identical x86-64 kernel machine code.
# Needs: Rosetta, clang, x86_64-w64-mingw32-gcc, target/release/wie.
# Usage: ./scripts/rosetta-compare.sh [rounds]   (default 5)
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"
src="$repo/micro-exes/rosetta_bench"
wie="$repo/target/release/wie"
rounds="${1:-5}"
out="$(mktemp -d)"

x86_64-w64-mingw32-gcc -O2 -fno-tree-vectorize -fno-jump-tables \
  -fno-asynchronous-unwind-tables -fno-stack-protector -S "$src/kernels.c" -o "$out/kernels.s"
python3 -I "$src/macho_asm.py" "$out/kernels.s" "$out/kernels_mac.s"
x86_64-w64-mingw32-gcc -c "$out/kernels.s" -o "$out/k_win.o"
x86_64-w64-mingw32-gcc -O2 "$src/main.c" "$out/k_win.o" -o "$out/bench.exe"
clang -arch x86_64 -c "$out/kernels_mac.s" -o "$out/k_mac.o"
clang -arch x86_64 -O2 "$src/main.c" "$out/k_mac.o" -o "$out/bench_mac"

cd "$out"
for i in $(seq 1 "$rounds"); do
  arch -x86_64 ./bench_mac >"mac$i.txt"
  WIE_JIT_CACHE=0 "$wie" run --max-api 5000000 --root "$out/root" bench.exe >"wie$i.txt" 2>"wie$i.err"
done
python3 -I "$src/sum.py"
echo "raw output: $out"
