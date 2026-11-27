#!/usr/bin/env python3
"""Per-instruction cost: WIE against Rosetta 2, or one WIE build against another.

Each case is a loop holding UNROLL copies of one instruction form. The same
x86-64 assembly is built into a Windows PE (WIE) and a macOS x86-64 binary
(Rosetta). Times are taken on the host, between the guest's output lines,
because the guest clock can stand still during a run without host stops. The
empty loop is subtracted, so a row is the cost of the instruction itself.

    micro-exes/instr_bench/run.py                    # all cases, WIE vs Rosetta
    micro-exes/instr_bench/run.py load store         # cases whose name matches
    micro-exes/instr_bench/run.py --ab OLD NEW load  # two WIE binaries

Needs Rosetta, clang, x86_64-w64-mingw32-gcc.
"""
import argparse
import os
import pathlib
import statistics
import subprocess
import tempfile
import time

UNROLL = 16
ITERS = 1_000_000
PASSES = 3  # the first pass pays for translation and is discarded

# name -> (setup, body, instructions per body). Windows ABI: rcx = iterations,
# rdx = 4 KiB scratch buffer. Only volatile registers are used; rdx is moved to
# r10 and the counter to r11. {i} is the copy index, for distinct offsets.
CASES = {
    "empty": ("", "", 0),
    "mov_rr": ("", "movq %r8, %rax", 1),
    "add_rr": ("", "addq %r8, %rax", 1),
    "add_ri": ("", "addq $7, %rax", 1),
    "adc_rr": ("", "adcq %r8, %rax", 1),
    "inc": ("", "incq %rax", 1),
    "xor_rr": ("", "xorq %r8, %rax", 1),
    "imul_rr": ("", "imulq %r8, %rax", 1),
    "shl_ri": ("", "shlq $3, %rax", 1),
    "shl_cl": ("movl $3, %ecx", "shlq %cl, %rax", 1),
    "lea": ("", "leaq 5(%rax,%r8,4), %rax", 1),
    "movzx_rr": ("", "movzbl %al, %eax", 1),
    "cmp_rr": ("", "cmpq %r8, %rax", 1),
    "cmp_jcc": ("movl $1, %eax", "cmpq $0, %rax\n je 9f\n 9:", 2),
    "cmp_cmov": ("", "cmpq %r8, %rax\n cmovbq %r8, %rax", 2),
    "cmp_set": ("", "cmpq %r8, %rax\n sete %r9b", 2),
    "div_r32": ("movl $7, %r8d", "xorl %edx, %edx\n divl %r8d", 2),
    "load8": ("", "movzbl {i}(%r10), %eax", 1),
    "load32": ("", "movl {i}*4(%r10), %eax", 1),
    "load64": ("", "movq {i}*8(%r10), %rax", 1),
    "load_idx": ("xorl %r9d, %r9d", "movq {i}*8(%r10,%r9,8), %rax", 1),
    "load_stack": ("", "movq -{i}*8-8(%rsp), %rax", 1),
    "store8": ("", "movb %al, {i}(%r10)", 1),
    "store32": ("", "movl %eax, {i}*4(%r10)", 1),
    "store64": ("", "movq %rax, {i}*8(%r10)", 1),
    "store_stack": ("", "movq %rax, -{i}*8-8(%rsp)", 1),
    "add_mem_r": ("", "addq %rax, {i}*8(%r10)", 1),
    "push_pop": ("", "pushq %rax\n popq %rax", 2),
    "call_ret": ("", "callq {leaf}", 2),
    "call_ind": ("leaq {leaf}(%rip), %r9", "callq *%r9", 2),
    "jmp": ("", "jmp 9f\n nop\n 9:", 1),
    "addsd": ("movl $3, %eax\n cvtsi2sd %eax, %xmm0\n cvtsi2sd %eax, %xmm1", "addsd %xmm1, %xmm0", 1),
    "mulsd": ("movl $1, %eax\n cvtsi2sd %eax, %xmm1\n movl $3, %eax\n cvtsi2sd %eax, %xmm0", "mulsd %xmm1, %xmm0", 1),
    "divsd": ("movl $3, %eax\n cvtsi2sd %eax, %xmm0\n cvtsi2sd %eax, %xmm1",
              "divsd %xmm1, %xmm0", 1),
    "movdqu_load": ("", "movdqu {i}*16(%r10), %xmm0", 1),
    "movdqu_store": ("movl $77, %eax\n movd %eax, %xmm0", "movdqu %xmm0, {i}*16(%r10)", 1),
    "movdqu_rip": ("", "movdqu {const}(%rip), %xmm0", 1),
    "call_leaf2": ("", "callq {leaf2}", 3),
    "paddd": ("movl $3, %eax\n movd %eax, %xmm1", "paddd %xmm1, %xmm0", 1),
}

# The same body with a jump in the middle: a two-block loop, which compiles as a
# loop region instead of a single self-looping block.
for _name in ("load64", "store64", "load_stack", "movdqu_load", "addsd"):
    CASES[_name + "_rgn"] = (CASES[_name][0], CASES[_name][1], CASES[_name][2])


def assembly(prefix):
    """The kernels as AT&T assembly; `prefix` is the platform's symbol prefix."""
    out = [".text"]
    leaf = prefix + "bench_leaf"
    out += [f".globl {leaf}", f"{leaf}:", " retq"]
    # Two instructions, so it is compiled rather than served by the `ret` stub.
    leaf2, const = prefix + "bench_leaf2", prefix + "bench_const"
    out += [f".globl {leaf2}", f"{leaf2}:", " leaq 1(%rax), %rax", " retq",
            ".p2align 4", f"{const}:", " .quad 0x0123456789abcdef, 0x1122334455667788"]
    for name, (setup, body, _) in CASES.items():
        sym = f"{prefix}k_{name}"
        out += [f".globl {sym}", ".p2align 4", f"{sym}:",
                " movq %rcx, %r11", " movq %rdx, %r10",
                " movl $1, %eax", " movl $3, %r8d",
                " pxor %xmm0, %xmm0", " pxor %xmm1, %xmm1",
                " " + setup.replace("{leaf}", leaf),
                " subq $168, %rsp"]
        # Deterministic inputs for the result checksum.
        out += [f" movq $0, -{k * 8 + 8}(%rsp)" for k in range(UNROLL)]
        out += [".p2align 4", "1:"]
        for i in range(UNROLL):
            if name.endswith("_rgn") and i == UNROLL // 2:
                out += [" jmp 8f", " nop", "8:"]
            out.append(" " + body.replace("{i}", str(i)).replace("{leaf2}", leaf2)
                       .replace("{leaf}", leaf).replace("{const}", const))
        out += [" subq $1, %r11", " jnz 1b",
                " movq %rax, 4000(%r10)", " movdqu %xmm0, 4008(%r10)",
                " addq $168, %rsp", " retq"]
    return "\n".join(out) + "\n"


def driver():
    decls = "".join(f"ABI void k_{n}(unsigned long long, void *);\n" for n in CASES)
    table = "".join(f'  {{"{n}", k_{n}}},\n' for n in CASES)
    return f"""
#ifdef _WIN32
#include <windows.h>
#define ABI
static void out(const char *s, int n) {{ DWORD w; WriteFile(GetStdHandle(STD_OUTPUT_HANDLE), s, n, &w, 0); }}
#else
#include <unistd.h>
#define ABI __attribute__((ms_abi))
static void out(const char *s, int n) {{ write(1, s, n); }}
#endif
static void line(void) {{ out(".\\n", 2); }}
static void checksum(unsigned long long h) {{
  char s[19];
  s[0] = '=';
  for (int i = 0; i < 16; i++) s[1 + i] = "0123456789abcdef"[(h >> (60 - 4 * i)) & 15];
  s[17] = '\\n';
  out(s, 18);
}}
{decls}
static const struct {{ const char *name; ABI void (*fn)(unsigned long long, void *); }} cases[] = {{
{table}}};
static _Alignas(4096) unsigned char buffer[4096];
int main(int argc, char **argv) {{
  for (unsigned c = 0; c < sizeof cases / sizeof *cases; c++) {{
    /* argv[1] is one comma-separated list: WIE caps the guest's argv size.
       Hand-rolled split because WIE lacks several CRT string functions. */
    const char *at = argc > 1 ? argv[1] : "";
    int wanted = 0;
    while (*at) {{
      const char *name = cases[c].name;
      while (*name && *at == *name) at++, name++;
      wanted |= !*name && (!*at || *at == ',');
      while (*at && *at != ',') at++;
      if (*at) at++;
    }}
    if (!wanted) continue;
    for (unsigned i = 0; i < sizeof buffer; i++) buffer[i] = (unsigned char)(i * 7 + 1);
    line();
    for (int p = 0; p < {PASSES}; p++) {{ cases[c].fn({ITERS}ULL, buffer); line(); }}
    unsigned long long h = 14695981039346656037ULL;
    for (unsigned i = 0; i < sizeof buffer; i++) h = (h ^ buffer[i]) * 1099511628211ULL;
    checksum(h);
  }}
  return 0;
}}
"""


def sh(*cmd):
    subprocess.run(cmd, check=True)


def build(tmp):
    (tmp / "main.c").write_text(driver())
    (tmp / "win.s").write_text(assembly(""))
    (tmp / "mac.s").write_text(assembly("_"))
    sh("x86_64-w64-mingw32-gcc", "-O2", tmp / "main.c", tmp / "win.s", "-o", tmp / "bench.exe")
    sh("clang", "-arch", "x86_64", "-O2", tmp / "main.c", tmp / "mac.s", "-o", tmp / "bench_mac")


def run(cmd, names):
    """Best ms per case over the warm passes."""
    env = dict(os.environ, WIE_JIT_CACHE="0")
    errors = tempfile.TemporaryFile()
    proc = subprocess.Popen([str(c) for c in cmd] + [",".join(names)], stdout=subprocess.PIPE,
                            stderr=errors, env=env)
    stamps, sums = [], []
    for out in iter(proc.stdout.readline, b""):
        if out.startswith(b"="):
            sums.append(out.strip())
        if out.strip() == b".":
            stamps.append(time.perf_counter())
    if proc.wait() or len(stamps) != len(names) * (PASSES + 1):
        errors.seek(0)
        raise SystemExit(f"{cmd[0]}: exit {proc.returncode}, {len(stamps)} marks\n"
                         + errors.read()[-800:].decode(errors="replace"))
    result = {}
    for k, name in enumerate(names):
        s = stamps[k * (PASSES + 1):(k + 1) * (PASSES + 1)]
        result[name] = min(b - a for a, b in zip(s[1:], s[2:])) * 1000
    return result, dict(zip(names, sums))


def main():
    repo = pathlib.Path(__file__).resolve().parents[2]
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("filters", nargs="*", help="substrings of case names")
    ap.add_argument("--ab", nargs=2, metavar=("OLD", "NEW"), help="compare two wie binaries")
    ap.add_argument("--rounds", type=int, default=3)
    args = ap.parse_args()
    names = [n for n in CASES if n != "empty"
             and (not args.filters or any(f in n for f in args.filters))]
    if not names:
        raise SystemExit("no case matches; cases: " + " ".join(CASES))
    selected = ["empty"] + names
    with tempfile.TemporaryDirectory() as d:
        tmp = pathlib.Path(d)
        build(tmp)
        wie = lambda b: [pathlib.Path(b).resolve(), "run", "--max-api", "5000000",
                         "--root", tmp / "root", tmp / "bench.exe", "--"]
        if args.ab:
            arms = {"old": wie(args.ab[0]), "new": wie(args.ab[1])}
        else:
            arms = {"rosetta": ["arch", "-x86_64", tmp / "bench_mac"],
                    "wie": wie(repo / "target/release/wie")}
        samples = {a: {n: [] for n in selected} for a in arms}
        sums = {}
        for _ in range(args.rounds):
            for arm, cmd in arms.items():
                times, results = run(cmd, selected)
                for name, ms in times.items():
                    samples[arm][name].append(ms)
                for name, value in results.items():
                    sums.setdefault(name, set()).add(value)
    wrong = [n for n, values in sums.items() if len(values) != 1]
    a, b = arms
    print(f"{'case':16}{a + ' ns':>12}{b + ' ns':>12}{b + '/' + a:>14}")
    for name in names:
        per = []
        for arm in (a, b):
            ms = statistics.median(samples[arm][name]) - statistics.median(samples[arm]["empty"])
            per.append(ms * 1e6 / (ITERS * UNROLL * CASES[name][2]))
        # Copies that fold or run in the shadow of the loop counter cost nothing
        # measurable; a ratio over them would be meaningless.
        ratio = f"{per[1] / per[0]:.2f}x" if min(per) > 0.02 else "not measured"
        print(f"{name:16}{per[0]:12.3f}{per[1]:12.3f}{ratio:>14}")
    empty = [statistics.median(samples[x]["empty"]) * 1e6 / ITERS for x in (a, b)]
    print("RESULT MISMATCH: " + " ".join(wrong) if wrong else "results identical in all arms")
    print(f"{'(loop)':16}{empty[0]:12.3f}{empty[1]:12.3f}   ns per iteration, subtracted above")


if __name__ == "__main__":
    main()
