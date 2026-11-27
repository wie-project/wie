#!/usr/bin/env python3
"""Check vector instruction results under WIE against Rosetta 2.

Each case loads two fixed 16-byte inputs, runs one instruction and prints the
result. The same assembly runs under Rosetta, `WIE_CPU=iced` and the JIT. A
JIT-against-interpreter test cannot catch semantics the two share; this can.

    micro-exes/instr_bench/sse_check.py      # exit 1 on any difference
"""
import os
import pathlib
import subprocess
import sys
import tempfile

PLAIN = """punpcklbw punpcklwd punpckldq punpcklqdq punpckhbw punpckhwd punpckhdq punpckhqdq
paddb paddw paddd paddq paddsb paddsw paddusb paddusw psubb psubw psubd psubq psubsb psubsw
psubusb psubusw pand pandn por pxor packuswb packsswb packssdw
pmullw pmulhw pmulhuw pmuludq pmaddwd psadbw
pcmpeqb pcmpeqw pcmpeqd pcmpgtb pcmpgtw pcmpgtd pminub pmaxub pminsw pmaxsw pavgb pavgw
pshufb unpcklps unpckhps unpcklpd unpckhpd andps orps xorps andnps
movhlps movlhps movss movsd addps mulps subps addpd mulpd psllw pslld psllq psrlw psrld psrlq
psraw psrad""".split()
IMMEDIATE = [(op, n) for op in "psllw pslld psllq psrlw psrld psrlq psraw psrad pslldq psrldq".split()
             for n in (0, 3, 8, 17)]
SHUFFLES = [(op, n) for op in "pshufd pshuflw pshufhw shufps".split()
            for n in (0x00, 0x4E, 0xD8, 0x1B)] + [("shufpd", 1), ("shufpd", 2)]
BY_REGISTER = [(op, n) for op in "psllw pslld psllq psrlw psrld psrlq psraw psrad".split()
               for n in (3, 17, 70)]
CASES = ([(o, f"{o} %xmm1, %xmm0") for o in PLAIN]
         + [(f"{o}_r{n}", f"movl ${n}, %eax\n movd %eax, %xmm1\n {o} %xmm1, %xmm0")
            for o, n in BY_REGISTER]
         + [(f"{o}_{n}", f"{o} ${n}, %xmm0") for o, n in IMMEDIATE]
         + [(f"{o}_{n:02x}", f"{o} ${n}, %xmm1, %xmm0") for o, n in SHUFFLES])


def write_sources(tmp):
    cases = CASES
    for pre, fn in (("", "win.s"), ("_", "mac.s")):
        out = [".text"]
        for k, (_, body) in enumerate(cases):
            out += [f".globl {pre}op{k}", f"{pre}op{k}:", " movdqu (%rcx), %xmm0",
                    " movdqu (%rdx), %xmm1", " " + body, " movdqu %xmm0, (%r8)", " retq"]
        (tmp / fn).write_text("\n".join(out) + "\n")
    os.chdir(tmp)
    decl="".join(f"ABI void op{k}(const void*,const void*,void*);\n" for k in range(len(cases)))
    tab=",".join(f'{{"{n}",op{k}}}' for k,(n,_) in enumerate(cases))
    open("main.c","w").write('''#ifdef _WIN32
#include <windows.h>
#define ABI
static void out(const char*s,int n){DWORD w;WriteFile(GetStdHandle(STD_OUTPUT_HANDLE),s,n,&w,0);}
#else
#include <unistd.h>
#define ABI __attribute__((ms_abi))
static void out(const char*s,int n){write(1,s,n);}
#endif
'''+decl+'static const struct{const char*n;ABI void(*f)(const void*,const void*,void*);}t[]={'+tab+'''};
static volatile unsigned char seed=0;
int main(void){
  static unsigned char a[16],b[16],r[16];
  static const unsigned char edge_a[16]={0x7f,0x00,0x80,0xff,0xff,0x7f,0x00,0x80,0x7f,0x80,0x01,0xff,0x00,0x00,0x00,0x80};
  static const unsigned char edge_b[16]={0x80,0xff,0x7f,0x00,0x00,0x80,0xff,0x7f,0x01,0x00,0xff,0xff,0x08,0x00,0x00,0x00};
  for(int input=0;input<3;input++){
    for(int i=0;i<16;i++){
      a[i]=input==0?(unsigned char)(0x11*i+seed+3):input==1?edge_a[i]:0xff;
      b[i]=input==0?(unsigned char)(0xe0-7*i+seed):input==1?edge_b[i]:0xff;}
    for(unsigned k=0;k<sizeof t/sizeof*t;k++){
      /* Hot enough to be compiled: a first sighting is only interpreted. */
      for(int hot=0;hot<20000;hot++) t[k].f(a,b,r);
      char l[96];int p=0;const char*n=t[k].n;while(*n)l[p++]=*n++;l[p++]='@';l[p++]='0'+input;l[p++]=' ';
      for(int i=0;i<16;i++){l[p++]="0123456789abcdef"[r[i]>>4];l[p++]="0123456789abcdef"[r[i]&15];} l[p++]='\\n'; out(l,p);}
  }
  return 0;}
''')


def main():
    repo = pathlib.Path(__file__).resolve().parents[2]
    wie = repo / "target/release/wie"
    with tempfile.TemporaryDirectory() as d:
        tmp = pathlib.Path(d)
        write_sources(tmp)
        for cmd in (["x86_64-w64-mingw32-gcc", "-O1", "-fno-tree-vectorize", "main.c", "win.s", "-o", "t.exe"],
                    ["clang", "-arch", "x86_64", "-O1", "-fno-vectorize", "-fno-slp-vectorize",
                     "main.c", "mac.s", "-o", "t_mac"]):
            subprocess.run(cmd, check=True)

        def run(cmd, **env):
            out = subprocess.run(cmd, capture_output=True, text=True, timeout=120,
                                 env=dict(os.environ, WIE_JIT_CACHE="0", **env)).stdout
            return dict(line.split() for line in out.splitlines()
                        if len(line.split()) == 2 and len(line.split()[1]) == 32)

        want = run(["arch", "-x86_64", "./t_mac"])
        bad = 0
        for arm, env in (("iced", {"WIE_CPU": "iced"}), ("jit", {})):
            got = run([str(wie), "run", "--root", str(tmp / "root"), "t.exe"], **env)
            for name, value in want.items():
                if got.get(name) != value:
                    bad += 1
                    print(f"{arm:5}{name:14} rosetta {value}  wie {got.get(name, 'missing')}")
        # Say how much of the JIT arm really ran compiled code.
        report = subprocess.run([str(wie), "run", "--root", str(tmp / "root"), "t.exe"],
                                capture_output=True, text=True, timeout=300,
                                env=dict(os.environ, WIE_JIT_CACHE="0", WIE_RUNTIME_PROFILE="1"))
        for line in (report.stdout + report.stderr).splitlines():
            if line.startswith("insn_coverage:"):
                print(line.split(" stops_per")[0])
        total = 3 * len(CASES)
        print(f"{len(want)} of {total} results compared, {bad} differences")
        sys.exit(1 if bad or len(want) != total else 0)


if __name__ == "__main__":
    main()
