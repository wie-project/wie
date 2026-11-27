"""Rewrite mingw (COFF) assembly so the macOS x86-64 assembler accepts it.

Instructions are untouched: only COFF directives are dropped and C symbols get
the Mach-O leading underscore.
"""
import re
import sys

NAMES = "k_alu k_sib k_fib f0 f1 f2 f3 k_indirect k_branch k_fp k_bytes k_chase".split()

out = []
for line in open(sys.argv[1]).read().split("\n"):
    if re.match(r"\.(file|def|ident|seh_|section|linkonce)", line.strip()):
        continue
    for name in NAMES:
        line = re.sub(r"(?<![\w.$])%s(?![\w.$])" % name, "_" + name, line)
    out.append(line)
open(sys.argv[2], "w").write("\n".join(out))
