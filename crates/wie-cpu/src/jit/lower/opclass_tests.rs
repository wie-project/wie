//! Snapshots that pin the operation-class table to the behaviour it replaced.
//!
//! This file is the safety net for ADR-0007. Collapsing six mnemonic tables into
//! one is only a refactor if the *answers* are unchanged: a table that admits one
//! mnemonic more or one fewer silently moves guest code between the JIT and the
//! interpreter, and every existing differential test still passes because both
//! engines are individually correct.
//!
//! So every fixture below was generated from the pre-ADR code (`git show
//! HEAD:...`), not from the table, and the tests are two-directional: they fail
//! both when the table starts admitting something new and when it stops admitting
//! something it used to. Three granularities, because they catch different slips:
//!
//! * [`ADMITTED`] — the mnemonic set, checked exhaustively over every
//!   `Mnemonic::values()`. Catches a lost or invented row.
//! * [`FORMS`] — real encodings through a real iced decoder. Catches a row that
//!   points at the wrong operand contract, which the mnemonic set cannot see
//!   (`bswap` and `mov` are both "admitted"; only one takes a memory operand).
//! * [`FLAGS`] / [`IMPLICIT`] — the two derived facts the table now also owns.
//!   These are the ones whose failure mode is silent wrong code rather than a
//!   fallback: a missing implicit operand compiles to `iconst 0`, and a missing
//!   flags row drops RFLAGS on the exit path.
//!
//! Not here, and deliberately: a differential run of both engines over the same
//! encodings (ADR-0007's companion option C). That belongs in `jit/tests/`, which
//! this change does not own.

use super::opclass::{
    OpClass, OpRow, admits, admits_mnemonic, implicit_gprs, is_string_op, needs_flags, row,
    string_op_size, term_of,
};
use iced_x86::{Decoder, DecoderOptions, Instruction, Mnemonic};

/// Every mnemonic the JIT admitted before the operation-class table existed.
const ADMITTED: &[&str] = &[
    "Adc",
    "Add",
    "Addpd",
    "Addps",
    "Addsd",
    "Addss",
    "And",
    "Andnpd",
    "Andnps",
    "Andpd",
    "Andps",
    "Bsf",
    "Bsr",
    "Bswap",
    "Bt",
    "Btc",
    "Btr",
    "Bts",
    "Call",
    "Cbw",
    "Cdqe",
    "Cld",
    "Cmova",
    "Cmovae",
    "Cmovb",
    "Cmovbe",
    "Cmove",
    "Cmovg",
    "Cmovge",
    "Cmovl",
    "Cmovle",
    "Cmovne",
    "Cmovno",
    "Cmovnp",
    "Cmovns",
    "Cmovo",
    "Cmovp",
    "Cmovs",
    "Cmp",
    "Cmpsb",
    "Cmpsd",
    "Cmpsq",
    "Cmpsw",
    "Cmpxchg",
    "Comisd",
    "Comiss",
    "Cvtdq2pd",
    "Cvtdq2ps",
    "Cvtpd2dq",
    "Cvtpd2ps",
    "Cvtps2dq",
    "Cvtps2pd",
    "Cvtsd2si",
    "Cvtsd2ss",
    "Cvtsi2sd",
    "Cvtsi2ss",
    "Cvtss2sd",
    "Cvtss2si",
    "Cvttpd2dq",
    "Cvttps2dq",
    "Cvttsd2si",
    "Cvttss2si",
    "Cwd",
    "Cwde",
    "Dec",
    "Div",
    "Divpd",
    "Divps",
    "Divsd",
    "Divss",
    "Endbr32",
    "Endbr64",
    "Idiv",
    "Imul",
    "Inc",
    "Ja",
    "Jae",
    "Jb",
    "Jbe",
    "Je",
    "Jg",
    "Jge",
    "Jl",
    "Jle",
    "Jmp",
    "Jne",
    "Jno",
    "Jnp",
    "Jns",
    "Jo",
    "Jp",
    "Js",
    "Lea",
    "Leave",
    "Lodsb",
    "Lodsd",
    "Lodsq",
    "Lzcnt",
    "Maxpd",
    "Maxps",
    "Maxsd",
    "Maxss",
    "Minpd",
    "Minps",
    "Minsd",
    "Minss",
    "Mov",
    "Movapd",
    "Movaps",
    "Movd",
    "Movdqa",
    "Movdqu",
    "Movhlps",
    "Movhpd",
    "Movhps",
    "Movlhps",
    "Movlpd",
    "Movlps",
    "Movmskpd",
    "Movmskps",
    "Movq",
    "Movsb",
    "Movsd",
    "Movsq",
    "Movss",
    "Movsw",
    "Movsx",
    "Movsxd",
    "Movupd",
    "Movups",
    "Movzx",
    "Mulpd",
    "Mulps",
    "Mulsd",
    "Mulss",
    "Neg",
    "Nop",
    "Not",
    "Or",
    "Orpd",
    "Orps",
    "Packssdw",
    "Packsswb",
    "Packuswb",
    "Paddb",
    "Paddd",
    "Paddq",
    "Paddsb",
    "Paddsw",
    "Paddusb",
    "Paddusw",
    "Paddw",
    "Pand",
    "Pandn",
    "Pause",
    "Pcmpeqb",
    "Pcmpeqd",
    "Pcmpeqw",
    "Pcmpgtb",
    "Pcmpgtd",
    "Pcmpgtw",
    "Pmaddwd",
    "Pmovmskb",
    "Pmulhuw",
    "Pmulhw",
    "Pmullw",
    "Pmuludq",
    "Pop",
    "Popfq",
    "Por",
    "Prefetchnta",
    "Prefetcht0",
    "Prefetcht1",
    "Prefetcht2",
    "Prefetchw",
    "Prefetchwt1",
    "Psadbw",
    "Pshufb",
    "Pshufd",
    "Pshufhw",
    "Pshuflw",
    "Pslld",
    "Pslldq",
    "Psllq",
    "Psllw",
    "Psrad",
    "Psraw",
    "Psrld",
    "Psrldq",
    "Psrlq",
    "Psrlw",
    "Psubb",
    "Psubd",
    "Psubq",
    "Psubsb",
    "Psubsw",
    "Psubusb",
    "Psubusw",
    "Psubw",
    "Punpckhbw",
    "Punpckhdq",
    "Punpckhqdq",
    "Punpckhwd",
    "Punpcklbw",
    "Punpckldq",
    "Punpcklqdq",
    "Punpcklwd",
    "Push",
    "Pushfq",
    "Pxor",
    "Rcl",
    "Rcr",
    "Ret",
    "Rol",
    "Ror",
    "Sal",
    "Sar",
    "Sbb",
    "Scasb",
    "Scasd",
    "Scasq",
    "Scasw",
    "Seta",
    "Setae",
    "Setb",
    "Setbe",
    "Sete",
    "Setg",
    "Setge",
    "Setl",
    "Setle",
    "Setne",
    "Setno",
    "Setnp",
    "Setns",
    "Seto",
    "Setp",
    "Sets",
    "Shl",
    "Shr",
    "Shufpd",
    "Sqrtpd",
    "Sqrtps",
    "Sqrtsd",
    "Sqrtss",
    "Std",
    "Stosb",
    "Stosd",
    "Stosq",
    "Stosw",
    "Sub",
    "Subpd",
    "Subps",
    "Subsd",
    "Subss",
    "Test",
    "Ucomisd",
    "Ucomiss",
    "Unpckhpd",
    "Unpcklpd",
    "Vpmovmskb",
    "Xadd",
    "Xchg",
    "Xor",
    "Xorpd",
    "Xorps",
];

/// Every mnemonic `lower_insn` had an arm for, before the table.
const EMITTED: &[&str] = &[
    "Adc",
    "Add",
    "Addpd",
    "Addps",
    "Addsd",
    "Addss",
    "And",
    "Andnpd",
    "Andnps",
    "Andpd",
    "Andps",
    "Bsf",
    "Bsr",
    "Bswap",
    "Bt",
    "Btc",
    "Btr",
    "Bts",
    "Cbw",
    "Cdqe",
    "Cld",
    "Cmova",
    "Cmovae",
    "Cmovb",
    "Cmovbe",
    "Cmove",
    "Cmovg",
    "Cmovge",
    "Cmovl",
    "Cmovle",
    "Cmovne",
    "Cmovno",
    "Cmovnp",
    "Cmovns",
    "Cmovo",
    "Cmovp",
    "Cmovs",
    "Cmp",
    "Cmpxchg",
    "Comisd",
    "Comiss",
    "Cqo",
    "Cvtdq2pd",
    "Cvtdq2ps",
    "Cvtpd2dq",
    "Cvtpd2ps",
    "Cvtps2dq",
    "Cvtps2pd",
    "Cvtsd2si",
    "Cvtsd2ss",
    "Cvtsi2sd",
    "Cvtsi2ss",
    "Cvtss2sd",
    "Cvtss2si",
    "Cvttpd2dq",
    "Cvttps2dq",
    "Cvttsd2si",
    "Cvttss2si",
    "Cwd",
    "Cwde",
    "Dec",
    "Div",
    "Divpd",
    "Divps",
    "Divsd",
    "Divss",
    "Endbr32",
    "Endbr64",
    "Idiv",
    "Imul",
    "Inc",
    "Lea",
    "Leave",
    "Lzcnt",
    "Maxpd",
    "Maxps",
    "Maxsd",
    "Maxss",
    "Minpd",
    "Minps",
    "Minsd",
    "Minss",
    "Mov",
    "Movapd",
    "Movaps",
    "Movd",
    "Movdqa",
    "Movdqu",
    "Movhlps",
    "Movhpd",
    "Movhps",
    "Movlhps",
    "Movlpd",
    "Movlps",
    "Movmskpd",
    "Movmskps",
    "Movq",
    "Movsd",
    "Movss",
    "Movsx",
    "Movsxd",
    "Movupd",
    "Movups",
    "Movzx",
    "Mulpd",
    "Mulps",
    "Mulsd",
    "Mulss",
    "Neg",
    "Nop",
    "Not",
    "Or",
    "Orpd",
    "Orps",
    "Packssdw",
    "Packsswb",
    "Packuswb",
    "Paddb",
    "Paddd",
    "Paddq",
    "Paddsb",
    "Paddsw",
    "Paddusb",
    "Paddusw",
    "Paddw",
    "Pand",
    "Pandn",
    "Pause",
    "Pcmpeqb",
    "Pcmpeqd",
    "Pcmpeqw",
    "Pcmpgtb",
    "Pcmpgtd",
    "Pcmpgtw",
    "Pmaddwd",
    "Pmovmskb",
    "Pmulhuw",
    "Pmulhw",
    "Pmullw",
    "Pmuludq",
    "Pop",
    "Popfq",
    "Por",
    "Prefetchnta",
    "Prefetcht0",
    "Prefetcht1",
    "Prefetcht2",
    "Prefetchw",
    "Prefetchwt1",
    "Psadbw",
    "Pshufb",
    "Pshufd",
    "Pshufhw",
    "Pshuflw",
    "Pslld",
    "Pslldq",
    "Psllq",
    "Psllw",
    "Psrad",
    "Psraw",
    "Psrld",
    "Psrldq",
    "Psrlq",
    "Psrlw",
    "Psubb",
    "Psubd",
    "Psubq",
    "Psubsb",
    "Psubsw",
    "Psubusb",
    "Psubusw",
    "Psubw",
    "Punpckhbw",
    "Punpckhdq",
    "Punpckhqdq",
    "Punpckhwd",
    "Punpcklbw",
    "Punpckldq",
    "Punpcklqdq",
    "Punpcklwd",
    "Push",
    "Pushfq",
    "Pxor",
    "Rcl",
    "Rcr",
    "Rol",
    "Ror",
    "Sal",
    "Sar",
    "Sbb",
    "Seta",
    "Setae",
    "Setb",
    "Setbe",
    "Sete",
    "Setg",
    "Setge",
    "Setl",
    "Setle",
    "Setne",
    "Setno",
    "Setnp",
    "Setns",
    "Seto",
    "Setp",
    "Sets",
    "Shl",
    "Shr",
    "Shufpd",
    "Sqrtpd",
    "Sqrtps",
    "Sqrtsd",
    "Sqrtss",
    "Std",
    "Sub",
    "Subpd",
    "Subps",
    "Subsd",
    "Subss",
    "Test",
    "Ucomisd",
    "Ucomiss",
    "Unpckhpd",
    "Unpcklpd",
    "Vpmovmskb",
    "Xadd",
    "Xchg",
    "Xor",
    "Xorpd",
    "Xorps",
];

const STRINGS: &[&str] = &[
    "Cmpsb", "Cmpsd", "Cmpsq", "Cmpsw", "Lodsb", "Lodsd", "Lodsq", "Movsb", "Movsd", "Movsq",
    "Movsw", "Scasb", "Scasd", "Scasq", "Scasw", "Stosb", "Stosd", "Stosq", "Stosw",
];

const TERMS: &[&str] = &[
    "Call", "Ja", "Jae", "Jb", "Jbe", "Je", "Jg", "Jge", "Jl", "Jle", "Jmp", "Jne", "Jno", "Jnp",
    "Jns", "Jo", "Jp", "Js", "Ret",
];

const FLAGS: &[&str] = &[
    "Adc", "Add", "And", "Bsf", "Bsr", "Bt", "Btc", "Btr", "Bts", "Cld", "Cmova", "Cmovae",
    "Cmovb", "Cmovbe", "Cmove", "Cmovg", "Cmovge", "Cmovl", "Cmovle", "Cmovne", "Cmovno", "Cmovnp",
    "Cmovns", "Cmovo", "Cmovp", "Cmovs", "Cmp", "Cmpsb", "Cmpsd", "Cmpsq", "Cmpsw", "Cmpxchg",
    "Comisd", "Comiss", "Dec", "Imul", "Inc", "Lodsb", "Lodsd", "Lodsq", "Lzcnt", "Movsb", "Movsd",
    "Movsq", "Movsw", "Neg", "Or", "Popfq", "Pushfq", "Rol", "Ror", "Sal", "Sar", "Sbb", "Scasb",
    "Scasd", "Scasq", "Scasw", "Seta", "Setae", "Setb", "Setbe", "Sete", "Setg", "Setge", "Setl",
    "Setle", "Setne", "Setno", "Setnp", "Setns", "Seto", "Setp", "Sets", "Shl", "Shr", "Std",
    "Stosb", "Stosd", "Stosq", "Stosw", "Sub", "Test", "Ucomisd", "Ucomiss", "Xadd", "Xor",
];

/// GPR slots each mnemonic touched implicitly before the table existed:
/// `analysis::mark_insn_gprs` for the first groups, `lower::compile_block`'s
/// implicit-operand pass for `Cmpxchg`/`Div`/`Idiv`. `Movsd` is empty here
/// because its SSE form has no implicit operand; the string form's
/// `RAX/RCX/RSI/RDI` is asserted separately in `movsd_is_two_instructions`.
const IMPLICIT: &[(&str, &[usize])] = &[
    ("Call", &[4]),
    ("Cbw", &[0]),
    ("Cdqe", &[0]),
    ("Cmpxchg", &[0]),
    ("Cmpsb", &[0, 1, 6, 7]),
    ("Cmpsd", &[0, 1, 6, 7]),
    ("Cmpsq", &[0, 1, 6, 7]),
    ("Cmpsw", &[0, 1, 6, 7]),
    ("Cwd", &[0, 2]),
    ("Cwde", &[0]),
    ("Div", &[0, 2]),
    ("Idiv", &[0, 2]),
    ("Leave", &[4, 5]),
    ("Lodsb", &[0, 1, 6, 7]),
    ("Lodsd", &[0, 1, 6, 7]),
    ("Lodsq", &[0, 1, 6, 7]),
    ("Movsb", &[0, 1, 6, 7]),
    ("Movsq", &[0, 1, 6, 7]),
    ("Movsw", &[0, 1, 6, 7]),
    ("Pop", &[4]),
    ("Popfq", &[4]),
    ("Push", &[4]),
    ("Pushfq", &[4]),
    ("Ret", &[4]),
    ("Scasb", &[0, 1, 6, 7]),
    ("Scasd", &[0, 1, 6, 7]),
    ("Scasq", &[0, 1, 6, 7]),
    ("Scasw", &[0, 1, 6, 7]),
    ("Stosb", &[0, 1, 6, 7]),
    ("Stosd", &[0, 1, 6, 7]),
    ("Stosq", &[0, 1, 6, 7]),
    ("Stosw", &[0, 1, 6, 7]),
];

// ---------------------------------------------------------------------------
// Encoding fixtures: the operand forms, which the mnemonic-set snapshot above
// cannot see. Every byte string here was chosen for a form rule that the
// pre-ADR code decided inside `is_lowerable`, so a transcription slip in the
// table shows up as a change of *answer for the same encoding*.
//
// (bytes, admitted, why)
// ---------------------------------------------------------------------------
const FORMS: &[(&[u8], bool, &str)] = &[
    (&[0x90], true, "nop"),
    (&[0xF3, 0x90], true, "pause"),
    (&[0x0F, 0x1F, 0x00], true, "nop dword [rax]"),
    (&[0xF3, 0x0F, 0x1E, 0xFA], true, "endbr64"),
    (&[0x0F, 0x0B], false, "ud2"),
    // mov / alu: the rm+imm shape
    (&[0x48, 0x89, 0xD8], true, "mov rax,rbx"),
    (&[0x48, 0x8B, 0x03], true, "mov rax,[rbx]"),
    (&[0x48, 0x8B, 0x43, 0x08], true, "mov rax,[rbx+8]"),
    (&[0x48, 0x89, 0x03], true, "mov [rbx],rax"),
    (
        &[0x48, 0xC7, 0x03, 0x01, 0x00, 0x00, 0x00],
        true,
        "mov qword [rbx],1",
    ),
    (&[0xC6, 0x03, 0x01], true, "mov byte [rbx],1"),
    (&[0x48, 0x01, 0x03], true, "add [rbx],rax"),
    (&[0xF0, 0x48, 0x01, 0x03], false, "lock add [rbx],rax"),
    (&[0x48, 0x39, 0xD8], true, "cmp rax,rbx"),
    (&[0x85, 0xC0], true, "test eax,eax"),
    (&[0xF6, 0x03, 0x01], true, "test byte [rbx],1"),
    (&[0x0F, 0xA3, 0xD8], true, "bt eax,ebx"),
    (&[0x0F, 0xAB, 0x03], true, "bts [rbx],eax"),
    (&[0xF0, 0x0F, 0xAB, 0x03], false, "lock bts [rbx],eax"),
    // unary
    (&[0xF6, 0xD0], true, "not al"),
    (&[0xF6, 0xD8], true, "neg al"),
    (&[0x48, 0xFF, 0x03], true, "inc qword [rbx]"),
    (&[0xF0, 0x48, 0xFF, 0x03], false, "lock inc qword [rbx]"),
    // xchg: register form yes, memory form implicitly locked
    (&[0x48, 0x87, 0xD8], true, "xchg rax,rbx"),
    (
        &[0x48, 0x87, 0x03],
        false,
        "xchg [rbx],rax (implicitly locked)",
    ),
    (&[0xF0, 0x48, 0x87, 0x03], false, "lock xchg [rbx],rax"),
    // cmpxchg / xadd
    (&[0x48, 0x0F, 0xB1, 0xCB], true, "cmpxchg rbx,rcx"),
    (
        &[0x48, 0x0F, 0xB1, 0x0B],
        false,
        "cmpxchg [rbx],rcx (implicitly locked)",
    ),
    (
        &[0xF0, 0x48, 0x0F, 0xB1, 0x0B],
        false,
        "lock cmpxchg [rbx],rcx",
    ),
    (&[0x48, 0x0F, 0xC1, 0x0B], true, "xadd [rbx],rcx"),
    (
        &[0xF0, 0x48, 0x0F, 0xC1, 0x0B],
        false,
        "lock xadd [rbx],rcx",
    ),
    // div/idiv: 32-bit only
    (&[0xF7, 0x33], true, "div ebx"),
    (&[0xF7, 0x73, 0x04], true, "div dword [rbx+4]"),
    (
        &[0x48, 0xF7, 0xF3],
        false,
        "div rbx (64-bit register stays iced)",
    ),
    // Pre-existing asymmetry, preserved: `div_is_lowerable` only checks the
    // operand size for the register form, so a 64-bit *memory* divisor is
    // admitted here and refused by `lower_div` later.
    (
        &[0x48, 0xF7, 0x33],
        true,
        "div qword [rbx] (admitted, lowered rejects)",
    ),
    (&[0x48, 0xF7, 0xE3], false, "mul rbx"),
    // imul: 2/3 operand yes, 1 operand no
    (&[0x48, 0x0F, 0xAF, 0xC3], true, "imul rax,rbx"),
    (&[0x6B, 0xC3, 0x05], true, "imul eax,ebx,5"),
    (&[0xF7, 0xE9], false, "imul ecx (one operand)"),
    // widen
    (&[0x0F, 0xB6, 0xC3], true, "movzx eax,bl"),
    (&[0x0F, 0xB6, 0x03], true, "movzx eax,byte [rbx]"),
    (&[0x48, 0x63, 0xC3], true, "movsxd rax,ebx"),
    (&[0x48, 0x0F, 0xBD, 0xC3], true, "bsr rax,rbx"),
    (&[0xF3, 0x0F, 0xBD, 0xC3], true, "lzcnt eax,ebx"),
    // lea accepts every EA form
    (&[0x48, 0x8D, 0x04, 0x8B], true, "lea rax,[rbx+rcx*4]"),
    (&[0x48, 0x8D, 0x00], true, "lea rax,[rax]"),
    // shifts: imm or CL
    (&[0xD1, 0xE0], true, "shl eax,1"),
    (&[0xD3, 0xE0], true, "shl eax,cl"),
    (&[0x48, 0xC1, 0xE0, 0x04], true, "shl rax,4"),
    (&[0x48, 0xD1, 0xC8], true, "ror rax,1"),
    // cmov / setcc
    (&[0x0F, 0x44, 0xC3], true, "cmove eax,ebx"),
    (&[0x0F, 0x44, 0x03], true, "cmove eax,[rbx]"),
    (&[0x0F, 0x94, 0xC0], true, "sete al"),
    (&[0x0F, 0x94, 0x03], true, "sete [rbx]"),
    // bswap: register only, 32/64
    (&[0x0F, 0xC8], true, "bswap eax"),
    (&[0x48, 0x0F, 0xC8], true, "bswap rax"),
    // push/pop: no 16-bit override
    (&[0x50], true, "push rax"),
    (&[0x6A, 0x05], true, "push 5"),
    (&[0xFF, 0x33], true, "push qword [rbx]"),
    (&[0x66, 0x50], false, "push ax"),
    (&[0x58], true, "pop rax"),
    (&[0x66, 0x58], false, "pop ax"),
    // terminators are admitted as block ends, never as body instructions
    (
        &[0xE9, 0x00, 0x00, 0x00, 0x00],
        false,
        "jmp rel32 (terminator, not body)",
    ),
    (&[0x74, 0x05], false, "je rel8 (terminator, not body)"),
    (
        &[0xE8, 0x00, 0x00, 0x00, 0x00],
        false,
        "call rel32 (terminator, not body)",
    ),
    (&[0xC3], false, "ret (terminator, not body)"),
    (&[0xC2, 0x08, 0x00], false, "ret 8 (needs the interpreter)"),
    // string ops
    (&[0xA5], true, "movsd (string MOVS DWORD)"),
    (&[0xF3, 0xA5], true, "rep movsd"),
    (&[0xAB], true, "stosd"),
    (&[0x48, 0xAB], true, "stosq"),
    (&[0xAC], true, "lodsb"),
    (&[0xAD], true, "lodsd"),
    (&[0x66, 0xAD], false, "lodsw (pre-existing gap, preserved)"),
    (&[0xF3, 0xAB], true, "rep stosd"),
    // SSE
    (&[0xF2, 0x0F, 0x10, 0xC1], true, "movsd xmm0,xmm1"),
    (&[0x0F, 0x10, 0x03], true, "movups xmm0,[rbx]"),
    (&[0x66, 0x0F, 0xEF, 0xC1], true, "pxor xmm0,xmm1"),
    (&[0x0F, 0x57, 0xC1], true, "xorps xmm0,xmm1"),
    (&[0xF3, 0x0F, 0x58, 0xC1], true, "addss xmm0,xmm1"),
    (&[0x66, 0x0F, 0xFE, 0xC1], true, "paddd xmm0,xmm1"),
    (&[0x66, 0x0F, 0x73, 0xF8, 0x04], true, "pslldq xmm0,4"),
    (&[0x66, 0x0F, 0x73, 0xD8, 0x04], true, "psrldq xmm0,4"),
    (&[0x0F, 0x2E, 0xC1], true, "ucomiss xmm0,xmm1"),
    (&[0xF3, 0x0F, 0x2A, 0xC3], true, "cvtsi2ss xmm0,ebx"),
    (&[0xF3, 0x0F, 0x2C, 0xC3], true, "cvttss2si eax,xmm3"),
    (&[0x66, 0x0F, 0x6E, 0xC3], true, "movd xmm0,ebx"),
    (&[0x66, 0x48, 0x0F, 0x6E, 0xC3], true, "movq xmm0,rbx"),
    (&[0x66, 0x0F, 0x7E, 0xC1], true, "movd ecx,xmm0"),
    (&[0x0F, 0x50, 0xC1], true, "movmskps eax,xmm1"),
    (&[0x66, 0x0F, 0xD7, 0xC1], true, "pmovmskb eax,xmm1"),
    (&[0x0F, 0x12, 0xC1], true, "movlps xmm0,xmm1"),
    (
        &[0x66, 0x0F, 0x70, 0xC1, 0x1B],
        true,
        "pshufd xmm0,xmm1,0x1b",
    ),
    (&[0x0F, 0x70, 0xC1, 0x1B], false, "pshufw mm0,mm1 (MMX)"),
    (&[0x0F, 0x51, 0xC1], true, "sqrtps xmm0,xmm1"),
    (&[0xF3, 0x0F, 0x51, 0xC1], true, "sqrtss xmm0,xmm1"),
    (&[0xF3, 0x0F, 0x5A, 0xC1], true, "cvtss2sd xmm0,xmm1"),
    (&[0x66, 0x0F, 0x5B, 0xC1], true, "cvtdq2ps xmm0,xmm1"),
    // x87 / system: refused, and that divergence is deliberate
    (&[0xD9, 0x00], false, "fld dword [rax]"),
    (&[0x0F, 0x31], false, "rdtsc"),
    (&[0x0F, 0xC7, 0x0F], false, "cmpxchg8b [rdi]"),
];

fn decode(bytes: &[u8]) -> Option<Instruction> {
    let mut buf = [0_u8; 15];
    let n = bytes.len().min(15);
    buf[..n].copy_from_slice(&bytes[..n]);
    let mut dec = Decoder::with_ip(64, &buf, 0x1000, DecoderOptions::NONE);
    let instr = dec.decode();
    if instr.is_invalid() || instr.len() == 0 {
        Option::None
    } else {
        Some(instr)
    }
}

#[test]
fn admission_set_is_unchanged() {
    let mut now_admits = Vec::new();
    let mut now_refuses = Vec::new();
    for m in Mnemonic::values() {
        let name = format!("{m:?}");
        let before = ADMITTED.contains(&name.as_str());
        let now = admits_mnemonic(m);
        match (now, before) {
            (true, false) => now_admits.push(name),
            (false, true) => now_refuses.push(name),
            _ => {}
        }
    }
    assert!(
        now_admits.is_empty(),
        "the table admits mnemonics the JIT refused before it existed \
         (this moves guest code from the interpreter to the JIT): {now_admits:?}"
    );
    assert!(
        now_refuses.is_empty(),
        "the table refuses mnemonics the JIT admitted before it existed \
         (this moves guest code from the JIT to the interpreter): {now_refuses:?}"
    );
}

#[test]
fn every_admitted_mnemonic_still_has_a_lowering() {
    let orphans: Vec<&str> = ADMITTED
        .iter()
        .copied()
        .filter(|n| !EMITTED.contains(n) && !STRINGS.contains(n) && !TERMS.contains(n))
        .collect();
    assert!(
        orphans.is_empty(),
        "admitted by the gate but with no emitter, string helper or terminator: \
         {orphans:?} — a block containing one would be refused at lowering time \
         and `mark_never`ed forever"
    );
}

#[test]
fn the_three_lowering_suites_overlap_only_on_movsd() {
    let mut overlap: Vec<&str> = EMITTED
        .iter()
        .copied()
        .filter(|n| STRINGS.contains(n) || TERMS.contains(n))
        .collect();
    overlap.sort_unstable();
    // `movsd` is two instructions in one mnemonic: the SSE2 scalar move (an
    // emitter) and the inherited string MOVS DWORD (`String(D4String)`).
    assert_eq!(overlap, ["Movsd"]);
}

#[test]
fn emitters_without_an_admission_row_are_only_cqo() {
    let mut orphans: Vec<&str> = EMITTED
        .iter()
        .copied()
        .filter(|n| !ADMITTED.contains(n))
        .collect();
    orphans.sort_unstable();
    // ADR-0007 called `Cqo` a dead arm and asked for its removal. It is kept:
    // `lower_cqto` is the only thing that makes SDL2's `cqto`+`idivq` lowerable
    // once `Cqo` is admitted, and admitting it is a guest-visible change (a
    // separate commit). Pinning it here makes the inconsistency visible and
    // bounded — a second entry in this list means the gate and the dispatch have
    // drifted apart.
    assert_eq!(orphans, ["Cqo"]);
}

#[test]
fn flags_set_is_unchanged() {
    let mut wrong: Vec<String> = Vec::new();
    for m in Mnemonic::values() {
        let name = format!("{m:?}");
        // `Movsd` is the one mnemonic whose row depends on its operands; the
        // mnemonic-level answer here is its SSE form (`false`), and
        // `movsd_is_two_instructions` pins both instruction-level answers.
        if m == Mnemonic::Movsd {
            continue;
        }
        let want = FLAGS.contains(&name.as_str());
        let got = row(m).flags;
        if want != got {
            wrong.push(format!("{name}: needs_flags={got}, was {want}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "the block's RFLAGS carrier requirement changed — a mnemonic that writes \
         flags without needing the carrier loses them:\n{}",
        wrong.join("\n")
    );
}

#[test]
fn implicit_gprs_are_unchanged() {
    let mut wrong: Vec<String> = Vec::new();
    for m in Mnemonic::values() {
        let name = format!("{m:?}");
        let want = IMPLICIT
            .iter()
            .find(|(n, _)| *n == name.as_str())
            .map_or(&[][..], |(_, idx)| *idx);
        let got = row(m).implicit;
        if want != got {
            wrong.push(format!("{name}: implicit={got:?}, was {want:?}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "implicit operand sets changed — a mnemonic compiled against an `iconst 0` \
         placeholder for a register it reads is silent wrong code:\n{}",
        wrong.join("\n")
    );
}

#[test]
fn string_ops_are_the_same_nineteen() {
    let mut got: Vec<&str> = STRINGS.iter().copied().filter(|n| *n != "Movsd").collect();
    got.sort_unstable();
    let mut want: Vec<&str> = ADMITTED
        .iter()
        .copied()
        .filter(|n| row_of_name(n).is_some_and(|r| matches!(r.class, OpClass::String(_))))
        .collect();
    want.sort_unstable();
    assert_eq!(got, want);
}

/// The table row for a mnemonic named as its `Debug` string, for the fixtures
/// above. `None` for a name iced does not have.
fn row_of_name(name: &str) -> Option<OpRow> {
    Mnemonic::values()
        .find(|m| format!("{m:?}") == name)
        .map(row)
}

#[test]
fn operand_forms_are_unchanged() {
    let mut wrong: Vec<String> = Vec::new();
    for &(bytes, want, note) in FORMS {
        let got = decode(bytes).is_some_and(|i| admits(&i));
        if got != want {
            wrong.push(format!(
                "{bytes:02X?} ({note}): admitted={got}, expected {want}"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "admission form drift:\n{}",
        wrong.join("\n")
    );
}

#[test]
fn terminators_are_admitted_only_as_a_block_end() {
    // `(bytes, is_a_terminator)` — a terminator ends the block, so `admits` must
    // stay false for it even though the table has a row for the mnemonic.
    let cases: &[(&[u8], bool)] = &[
        (&[0xE9, 0x00, 0x00, 0x00, 0x00], true),       // jmp rel32
        (&[0x74, 0x05], true),                         // je rel8
        (&[0x0F, 0x84, 0x00, 0x00, 0x00, 0x00], true), // je rel32
        (&[0xE8, 0x00, 0x00, 0x00, 0x00], true),       // call rel32
        (&[0xC3], true),                               // ret
        (&[0xC2, 0x08, 0x00], false),                  // ret imm16 → interpreter
        (&[0xFF, 0xE0], false),                        // jmp rax (not a near branch)
        (&[0x48, 0x89, 0xD8], false),                  // mov rax,rbx is not one
        // ── Deliberate admission change (indirect calls) ──────────────────────
        // `call r/m64` became a terminator because the block exit already
        // carries a runtime RIP (`ret` leaves a block that way), which made the
        // largest interpreter residue on real guests — 82.5 % of 7-Zip's
        // interpreted instructions (`WIE_EXEC_TRACE=1`) — a lowering instead of
        // a permanent `mark_never`. Both shapes below were `false` before, and
        // `dyn_call_tests` compares them against `IcedCpu` byte for byte
        // (RIP, RSP, pushed return address, RFLAGS, untouched memory) before
        // this fixture change was made.
        (&[0xFF, 0xD1], true),       // call rcx
        (&[0xFF, 0x13], true),       // call [rbx]
        (&[0xFF, 0x14, 0x24], true), // call [rsp]
        // Still refused, on purpose: a far call carries an m16:64 pointer, and
        // the JIT's exit is a single 8-byte target. `jmp r/m64` above stays
        // refused for the same reason it always was — it was not measured as
        // residue and lowering it is a separate change, not a free one.
        (&[0xFF, 0x1B], false), // call far [rbx]
    ];
    for &(bytes, want) in cases {
        let instr = decode_ok(bytes);
        assert_eq!(term_of(&instr).is_some(), want, "{bytes:02X?}");
        if term_of(&instr).is_some() {
            assert!(
                !admits(&instr),
                "{bytes:02X?} admitted as a body instruction"
            );
            // Every terminator the table recognizes is one of the mnemonics the
            // pre-ADR decoder recognized.
            let name = format!("{:?}", instr.mnemonic());
            assert!(
                TERMS.contains(&name.as_str()),
                "{name} is not a known terminator"
            );
        }
    }
}

fn decode_ok(bytes: &[u8]) -> Instruction {
    match decode(bytes) {
        Some(instr) => instr,
        None => panic!("{bytes:02X?} does not decode"),
    }
}

#[test]
fn string_widths_are_unchanged() {
    let cases: &[(&[u8], u32)] = &[
        (&[0xA4], 1),       // movsb
        (&[0x66, 0xA5], 2), // movsw
        (&[0xA5], 4),       // movsd (string)
        (&[0x48, 0xA5], 8), // movsq
        (&[0xAA], 1),       // stosb
        (&[0xAB], 4),       // stosd
        (&[0x48, 0xAB], 8), // stosq
        (&[0xA6], 1),       // cmpsb
        (&[0x66, 0xA7], 2), // cmpsw
        (&[0xA7], 4),       // cmpsd
        (&[0x48, 0xA7], 8), // cmpsq
        (&[0xAE], 1),       // scasb
        (&[0xAF], 4),       // scasd
        (&[0x48, 0xAF], 8), // scasq
        (&[0xAC], 1),       // lodsb
        (&[0xAD], 4),       // lodsd
        (&[0x48, 0xAD], 8), // lodsq
    ];
    for &(bytes, want) in cases {
        let got = decode(bytes).and_then(|i| string_op_size(&i));
        assert_eq!(got, Some(want), "{bytes:02X?}");
    }
    // A non-string op has no width, and `movsd`'s SSE form is not a string op.
    assert_eq!(decode(&[0x90]).and_then(|i| string_op_size(&i)), None);
    let sse_movsd = decode(&[0xF2, 0x0F, 0x10, 0xC1]);
    assert!(sse_movsd.is_some_and(|i| !is_string_op(&i)));
}

#[test]
fn movsd_is_two_instructions() {
    let string = decode(&[0xA5]).expect("movsd decodes");
    assert!(is_string_op(&string));
    assert_eq!(implicit_gprs(&string), [0, 1, 6, 7]);
    assert!(needs_flags(&string));

    let sse = decode(&[0xF2, 0x0F, 0x10, 0xC1]).expect("movsd xmm,xmm decodes");
    assert!(!is_string_op(&sse));
    assert!(implicit_gprs(&sse).is_empty());
    assert!(!needs_flags(&sse));
    assert!(admits(&sse));
}

/// The 48-byte SSE2 loop at `0x140001b96` in `real_exes/2048.exe`
/// (`gameEnded`), instruction by instruction, must be admissible.
///
/// This is a real-game regression, not a synthetic case: the block was refused at
/// lowering time with `unsupported-instruction`, which sent the whole loop back to
/// the iced interpreter for the rest of the run (two refusals in one session, one
/// per `gameEnded` call site). The bytes are copied from `objdump` output so the
/// exact operand forms that real compiler output produces are the ones pinned —
/// note that `movd` appears in **four** forms here (register↔xmm, memory→xmm, and a
/// RIP-relative memory operand), and the gate cases nearby only covered the
/// register forms.
#[test]
fn the_2048_gameended_simd_loop_is_fully_lowerable() {
    // objdump -d --start-address=0x140001b96 --stop-address=0x140001bc6
    let block: &[(&[u8], &str)] = &[
        (&[0x66, 0x0F, 0x6E, 0x02], "movd xmm0,dword [rdx]"),
        (
            &[0x66, 0x0F, 0x6E, 0x1D, 0xD6, 0x38, 0x00, 0x00],
            "movd xmm3,dword [rip+0x38d6]",
        ),
        (&[0x48, 0x83, 0xC2, 0x04], "add rdx,4"),
        (&[0x66, 0x0F, 0x74, 0xC2], "pcmpeqb xmm0,xmm2"),
        (&[0x66, 0x0F, 0xDB, 0xC3], "pand xmm0,xmm3"),
        (&[0x66, 0x0F, 0x7E, 0xC0], "movd eax,xmm0"),
        (&[0x66, 0x0F, 0x6E, 0xC0], "movd xmm0,eax"),
        (&[0x66, 0x0F, 0xF6, 0xC1], "psadbw xmm0,xmm1"),
        (&[0x66, 0x0F, 0x7E, 0xC0], "movd eax,xmm0"),
        (&[0x41, 0x01, 0xC0], "add r8d,eax"),
        (&[0x4C, 0x39, 0xDA], "cmp rdx,r11"),
        (&[0x75, 0xD0], "jne 0x140001b96"),
    ];
    let mut refused: Vec<String> = Vec::new();
    for (bytes, text) in block {
        let instr = decode_ok(bytes);
        // `jne` and `cmp` leave the block through the term form, not the emitter
        // dispatch, so asking `EMITTED` about them would blame the wrong thing —
        // the same false culprit `err::refusal_culprit` avoids by skipping them.
        if term_of(&instr).is_some() {
            continue;
        }
        if !admits(&instr) {
            refused.push(format!("{text} — not admitted by the gate"));
            continue;
        }
        // `admits` is the gate; an admitted mnemonic with no emitter is the drift
        // `every_admitted_mnemonic_still_has_a_lowering` also guards against, so
        // check the dispatch actually names it.
        let name = format!("{:?}", instr.mnemonic());
        if !EMITTED.contains(&name.as_str()) {
            refused.push(format!("{text} — admitted but no emitter ({name})"));
        }
    }
    assert!(
        refused.is_empty(),
        "the 2048 `gameEnded` loop must lower as one block; refused: {refused:#?}"
    );
}
