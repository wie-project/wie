#!/usr/bin/env python3
"""Check integer results, flags and call/ret under WIE against Rosetta 2.

Each case starts from fixed registers, runs a short sequence and records RAX,
RDX and the defined flags. The same assembly runs under Rosetta, `WIE_CPU=iced`
and the JIT (hot enough to be compiled). Call cases cover recursion deeper than
any chain limit, indirect calls and one callee returning to several sites.

    micro-exes/instr_bench/gpr_check.py      # exit 1 on any difference
"""
import os
import pathlib
import subprocess
import sys
import tempfile

CF, PF, AF, ZF, SF, OF = 0x1, 0x4, 0x10, 0x40, 0x80, 0x800
ALL = CF | PF | AF | ZF | SF | OF
LOGIC = CF | PF | ZF | SF | OF  # AF is undefined after logic ops and shifts

# name -> (body, flags that are architecturally defined afterwards).
# On entry: rax = r9 = a, rcx = rdx = b, flags from `1 + 0`.
CASES = {}
for size, a, c, d in (("q", "%rax", "%rcx", "%rdx"), ("l", "%eax", "%ecx", "%edx"),
                      ("w", "%ax", "%cx", "%dx"), ("b", "%al", "%cl", "%dl")):
    for op in ("add", "sub", "cmp"):
        CASES[f"{op}{size}"] = (f"{op}{size} {c}, {a}", ALL)
    for op in ("and", "or", "xor", "test"):
        CASES[f"{op}{size}"] = (f"{op}{size} {c}, {a}", LOGIC)
    CASES[f"adc{size}"] = (f"stc\n adc{size} {c}, {a}", ALL)
    CASES[f"sbb{size}"] = (f"stc\n sbb{size} {c}, {a}", ALL)
    CASES[f"neg{size}"] = (f"neg{size} {a}", ALL)
    CASES[f"inc{size}"] = (f"stc\n inc{size} {a}", ALL)
    CASES[f"dec{size}"] = (f"stc\n dec{size} {a}", ALL)
for count in (0, 1, 7, 31, 32, 63, 64):
    for op in ("shl", "shr", "sar"):
        for size, a, width in (("q", "%rax", 64), ("l", "%eax", 32)):
            effective = count % width
            # A zero count leaves every flag; OF is defined only for a count of 1.
            mask = ALL if effective == 0 else (CF | PF | ZF | SF | (OF if effective == 1 else 0))
            CASES[f"{op}{size}_i{count}"] = (f"{op}{size} ${count}, {a}", mask)
            CASES[f"{op}{size}_cl{count}"] = (f"movl ${count}, %ecx\n {op}{size} %cl, {a}", mask)
    # Narrow shifts take a 5-bit count, which can exceed the operand width.
    for op in ("shl", "shr", "sar"):
        for size, a, width in (("w", "%ax", 16), ("b", "%al", 8)):
            effective = count % 32
            mask = ALL if effective == 0 else (PF | ZF | SF | (CF if effective < width else 0)
                                               | (OF if effective == 1 else 0))
            CASES[f"{op}{size}_i{count}"] = (f"{op}{size} ${count}, {a}", mask)
            CASES[f"{op}{size}_cl{count}"] = (f"movl ${count}, %ecx\n {op}{size} %cl, {a}", mask)
    for op in ("rol", "ror"):
        mask = ALL if count % 64 == 0 else (CF | (OF if count % 64 == 1 else 0) | PF | AF | ZF | SF)
        CASES[f"{op}q_{count}"] = (f"{op}q ${count}, %rax", mask)
CASES.update({
    "imul2q": ("imulq %rcx, %rax", CF | OF),
    "imul2l": ("imull %ecx, %eax", CF | OF),
    "mulq": ("mulq %rcx", CF | OF),
    "imulq": ("imulq %rcx", CF | OF),
    "mull": ("mull %ecx", CF | OF),
    "divq": ("xorl %edx, %edx\n orq $1, %rcx\n divq %rcx", 0),
    "idivq": ("orq $3, %rcx\n andq $0x7fffffff, %rcx\n cqto\n idivq %rcx", 0),
    "divl": ("xorl %edx, %edx\n orl $1, %ecx\n divl %ecx", 0),
    "adc_chain": ("addq %rcx, %rax\n adcq %rcx, %rdx\n adcq $0, %rax\n sbbq %rax, %rdx", ALL),
    "lea": ("leaq -7(%rax,%rcx,8), %rax\n leal 5(%rdx,%rdx,2), %edx", ALL),
    "movsx": ("movsbq %al, %rdx\n movswl %cx, %eax\n movslq %eax, %rax", ALL),
    "xchg": ("xchgq %rcx, %rax\n xchgl %eax, %edx", ALL),
    "bsf": ("orq $1024, %rcx\n bsfq %rcx, %rax", ZF),
    "bsr": ("orq $1, %rcx\n bsrq %rcx, %rax", ZF),
    "bt": ("btq %rcx, %rax", CF),
    "bswap": ("bswapq %rax\n bswapl %edx", ALL),
    "push_pop": ("pushq %rax\n pushq %rcx\n popq %rax\n popq %rdx", ALL),
    # call / ret
    "call_fib": ("movq $18, %rcx\n call {p}gc_fib", 0),
    "call_deep": ("movq $20000, %rcx\n call {p}gc_deep", 0),
    "call_sites": ("movq $3, %rcx\n xorl %edx, %edx\n call {p}gc_leaf\n addq %rax, %rdx\n"
                   " call {p}gc_leaf\n shlq $1, %rax\n addq %rax, %rdx\n call {p}gc_leaf\n"
                   " subq %rdx, %rax", 0),
    "call_indirect": ("movq $50, %r10\n xorl %eax, %eax\n 1: leaq {p}gc_leaf(%rip), %r11\n"
                      " testq $1, %r10\n jz 2f\n leaq {p}gc_twice(%rip), %r11\n"
                      " 2: movq %r10, %rcx\n movq %rax, %rdx\n call *%r11\n addq %rdx, %rax\n"
                      " decq %r10\n jnz 1b", 0),
})
for cc in "o no b ae e ne be a s ns p np l ge le g".split():
    CASES[f"set{cc}"] = (f"cmpq %rcx, %rax\n set{cc} %dl\n cmov{cc}q %r9, %rcx\n movq %rcx, %rax", ALL)

INPUTS = [(0x0123456789ABCDEF, 0xFEDCBA9876543210), (0x8000000000000000, 0xFFFFFFFFFFFFFFFF),
          (0x7FFFFFFFFFFFFFFF, 1), (0, 0), (0x80808080_80007F7F, 0x7F7F7F7F_FF80_0180)]

HELPERS = """
{p}gc_leaf: leaq 5(%rcx), %rax
 retq
{p}gc_twice: leaq (%rcx,%rcx), %rax
 retq
{p}gc_fib: cmpq $1, %rcx
 jbe 1f
 pushq %rcx
 decq %rcx
 call {p}gc_fib
 popq %rcx
 pushq %rax
 subq $2, %rcx
 call {p}gc_fib
 popq %rcx
 addq %rcx, %rax
 retq
1: movq %rcx, %rax
 retq
{p}gc_deep: testq %rcx, %rcx
 jz 1f
 pushq %rcx
 decq %rcx
 call {p}gc_deep
 popq %rcx
 addq %rcx, %rax
 retq
1: xorl %eax, %eax
 retq
"""


def sources(tmp):
    names = list(CASES)
    for pre, fn in (("", "win.s"), ("_", "mac.s")):
        out = [".text", HELPERS.replace("{p}", pre)]
        for k, name in enumerate(names):
            body = CASES[name][0].replace("{p}", pre)
            # r8 (the output pointer) survives the body on the stack.
            out += [f".globl {pre}op{k}", f"{pre}op{k}:", " pushq %r8", " movq %rcx, %rax",
                    " movq %rcx, %r9", " movq %rdx, %rcx", " movl $1, %r10d",
                    " addq $0, %r10", " " + body, " pushfq", " popq %r10", " popq %r8",
                    " movq %rax, (%r8)", " movq %rdx, 8(%r8)", " movq %r10, 16(%r8)", " retq"]
        (tmp / fn).write_text("\n".join(out) + "\n")
    decls = "".join(f"ABI void op{k}(u64, u64, u64 *);\n" for k in range(len(names)))
    table = ",".join(f'{{"{n}",op{k},{CASES[n][1]}}}' for k, n in enumerate(names))
    inputs = ",".join(f"{{{a}ULL,{b}ULL}}" for a, b in INPUTS)
    (tmp / "main.c").write_text('''typedef unsigned long long u64;
#ifdef _WIN32
#include <windows.h>
#define ABI
static void out(const char*s,int n){DWORD w;WriteFile(GetStdHandle(STD_OUTPUT_HANDLE),s,n,&w,0);}
#else
#include <unistd.h>
#define ABI __attribute__((ms_abi))
static void out(const char*s,int n){write(1,s,n);}
#endif
''' + decls + "static const struct{const char*n;ABI void(*f)(u64,u64,u64*);u64 mask;}t[]={" + table
        + "};\nstatic const u64 in[][2]={" + inputs + '''};
static char*hex(char*p,u64 v){for(int i=60;i>=0;i-=4)*p++="0123456789abcdef"[(v>>i)&15];return p;}
int main(void){
  for(unsigned k=0;k<sizeof t/sizeof*t;k++){
    int calls = t[k].n[0]=='c' && t[k].n[1]=='a';
    for(unsigned i=0;i<sizeof in/sizeof*in;i++){
      u64 r[3];
      for(int hot=0;hot<(calls?200:5000);hot++) t[k].f(in[i][0],in[i][1],r);
      char l[160],*p=l;const char*n=t[k].n;while(*n)*p++=*n++;
      *p++='@';*p++='0'+i;*p++=' ';p=hex(p,r[0]);*p++=':';p=hex(p,r[1]);*p++=':';p=hex(p,r[2]&t[k].mask);*p++='\\n';
      out(l,(int)(p-l));
    }
  }
  return 0;}
''')


def main():
    repo = pathlib.Path(__file__).resolve().parents[2]
    wie = repo / "target/release/wie"
    with tempfile.TemporaryDirectory() as d:
        tmp = pathlib.Path(d)
        sources(tmp)
        for cmd in (["x86_64-w64-mingw32-gcc", "-O1", "main.c", "win.s", "-o", "t.exe"],
                    ["clang", "-arch", "x86_64", "-O1", "main.c", "mac.s", "-o", "t_mac"]):
            subprocess.run(cmd, check=True, cwd=tmp)

        def run(cmd, **env):
            done = subprocess.run(cmd, capture_output=True, text=True, timeout=600, cwd=tmp,
                                  env=dict(os.environ, WIE_JIT_CACHE="0", **env))
            lines = dict(line.split() for line in done.stdout.splitlines()
                         if "@" in line.split(" ")[0] and len(line.split()) == 2)
            return lines, done

        want, _ = run(["./t_mac"])
        total = len(CASES) * len(INPUTS)
        bad = 0
        for arm, env in (("iced", {"WIE_CPU": "iced"}), ("jit", {})):
            got, done = run([str(wie), "run", "--max-api", "5000000", "--root", str(tmp / "root"),
                             "t.exe"], **env)
            if done.returncode:
                print(f"{arm}: exit {done.returncode}: {done.stderr[-400:]}")
            wrong = [n for n, v in want.items() if got.get(n) != v]
            bad += len(wrong)
            for n in wrong[:12]:
                print(f"{arm:5}{n:16} rosetta {want[n]}\n{'':21}wie     {got.get(n, 'missing')}")
            if len(wrong) > 12:
                print(f"{arm}: {len(wrong) - 12} more: " + " ".join(sorted({n.split('@')[0] for n in wrong})))
        # Say how much of the JIT arm really ran compiled code.
        _, report = run([str(wie), "run", "--max-api", "5000000", "--root", str(tmp / "root"),
                         "t.exe"], WIE_RUNTIME_PROFILE="1")
        for line in (report.stdout + report.stderr).splitlines():
            if line.startswith("insn_coverage:"):
                print(line.split(" stops_per")[0])
        print(f"{len(want)} of {total} results compared, {bad} differences")
        sys.exit(1 if bad or len(want) != total else 0)


if __name__ == "__main__":
    main()
