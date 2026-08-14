//! Per-instruction lowering (`lower_insn`) with lazy-flag machinery and the
//! deferred-shift/flag state.

use super::emit::MemEnv;
use super::flags::{
    clear_flags, flag_bit, flags_add, flags_logic, flags_sub, flags_zs_pf, iconst_u64,
    lower_inc_dec_lazy, lower_neg_lazy, lower_not, replace_flag, select_flag,
};
use super::gpr::{
    Arith, lower_arith, lower_arith_lazy, lower_bit_test_op, lower_bsf, lower_bsr, lower_bswap,
    lower_cbw, lower_cmp_test_lazy, lower_cmpxchg, lower_cwd, lower_cwde_cdqe, lower_div,
    lower_imul, lower_lea, lower_leave, lower_lzcnt, lower_mov, lower_movx, lower_pop, lower_popfq,
    lower_push, lower_pushfq, lower_xadd, lower_xchg, sext_to_i64,
};
use super::sse::{
    lower_sse_int_binop, lower_sse_mov, lower_sse_movd, lower_sse_movhlps, lower_sse_movhps,
    lower_sse_movq, lower_sse_pmovmskb, lower_sse_pshufd, lower_sse_pshuflw_hw, lower_sse_punpck,
    lower_sse_punpck_lanes, lower_sse_shufpd, sse_int_op, sse_shift_op,
};
use super::sse_fp::{
    FloatBinOp, FloatWidth, lower_sse_bitwise, lower_sse_byte_shift, lower_sse_comis,
    lower_sse_cvt_fp_to_gpr, lower_sse_cvt_gpr_to_fp, lower_sse_cvt_packed, lower_sse_cvtdq2pd,
    lower_sse_cvtpd_packed, lower_sse_cvtps2pd, lower_sse_cvt_scalar_preserve,
    lower_sse_fp_binop_packed, lower_sse_fp_binop_scalar, lower_sse_fp_unop_packed,
    lower_sse_fp_unop_scalar, lower_sse_packed_fp, lower_sse_pshufb, lower_sse_scalar_fp,
    lower_sse_shift,
};
use super::{SseBit, lower_cmov, lower_setcc, lower_shift_lazy};

use crate::exec::{self};
use crate::regs::Rflags;
use cranelift::prelude::*;
use iced_x86::{Instruction, Mnemonic};

#[derive(Clone, Copy)]
pub(super) enum ShiftKind {
    Shl,
    Shr,
    Sar,
    Rol,
    Ror,
    Rcl,
    Rcr,
}

/// Deferred flag computation from the last flag-writing ALU (lazy flags).
#[derive(Clone, Copy)]
pub(super) enum PendingFlags {
    None,
    Add {
        a: Value,
        b: Value,
        res: Value,
        bits: u32,
    },
    Sub {
        a: Value,
        b: Value,
        res: Value,
        bits: u32,
    },
    Logic {
        res: Value,
        bits: u32,
    },
    /// INC: add 1, but **preserve CF** on flush.
    Inc {
        a: Value,
        res: Value,
        bits: u32,
    },
    /// DEC: sub 1, preserve CF.
    Dec {
        a: Value,
        res: Value,
        bits: u32,
    },
    /// Shift/rotate: materialize CF/OF/(ZF/SF/PF) on flush.
    /// `count_mod == 0` is never stored (flags unchanged → leave prior pending).
    Shift {
        kind: ShiftKind,
        dst: Value,
        res: Value,
        count_mod: Value,
        bits: u32,
    },
}

pub(super) fn flush_pending(
    bcx: &mut FunctionBuilder<'_>,
    rflags: &mut Value,
    pending: &mut PendingFlags,
) {
    match *pending {
        PendingFlags::None => {}
        PendingFlags::Add { a, b, res, bits } => {
            *rflags = flags_add(bcx, *rflags, a, b, res, bits);
        }
        PendingFlags::Sub { a, b, res, bits } => {
            *rflags = flags_sub(bcx, *rflags, a, b, res, bits);
        }
        PendingFlags::Logic { res, bits } => {
            *rflags = flags_logic(bcx, *rflags, res, bits);
        }
        PendingFlags::Inc { a, res, bits } => {
            let one = iconst_u64(bcx, 1);
            let cf = flag_bit(bcx, *rflags, Rflags::CF);
            let with = flags_add(bcx, *rflags, a, one, res, bits);
            *rflags = replace_flag(bcx, with, Rflags::CF, cf);
        }
        PendingFlags::Dec { a, res, bits } => {
            let one = iconst_u64(bcx, 1);
            let cf = flag_bit(bcx, *rflags, Rflags::CF);
            let with = flags_sub(bcx, *rflags, a, one, res, bits);
            *rflags = replace_flag(bcx, with, Rflags::CF, cf);
        }
        PendingFlags::Shift {
            kind,
            dst,
            res,
            count_mod,
            bits,
        } => {
            *rflags = materialize_shift_flags(bcx, *rflags, kind, dst, res, count_mod, bits);
        }
    }
    *pending = PendingFlags::None;
}

/// Materialize shift/rotate flags. If `count_mod == 0`, returns `old_rflags` unchanged.
pub(super) fn materialize_shift_flags(
    bcx: &mut FunctionBuilder<'_>,
    old_rflags: Value,
    kind: ShiftKind,
    dst: Value,
    result: Value,
    count_mod: Value,
    bits: u32,
) -> Value {
    let one = iconst_u64(bcx, 1);
    let zero_c = iconst_u64(bcx, 0);
    let is_zero = bcx.ins().icmp_imm(IntCC::Equal, count_mod, 0);
    let is_one = bcx.ins().icmp_imm(IntCC::Equal, count_mod, 1);
    let sign = iconst_u64(bcx, 1_u64 << bits.saturating_sub(1).min(63));
    let sb = iconst_u64(bcx, u64::from(bits.saturating_sub(1)));
    // Capture old CF before the match (needed by Rcl/Rcr).
    let old_cf = flag_bit(bcx, old_rflags, Rflags::CF);

    let cf_bit = match kind {
        ShiftKind::Shl => {
            let cm1 = bcx.ins().isub(count_mod, one);
            let t = bcx.ins().ishl(dst, cm1);
            let cf = bcx.ins().ushr(t, sb);
            bcx.ins().band(cf, one)
        }
        ShiftKind::Shr => {
            let cm1 = bcx.ins().isub(count_mod, one);
            let cf = bcx.ins().ushr(dst, cm1);
            bcx.ins().band(cf, one)
        }
        ShiftKind::Sar => {
            let signed = sext_to_i64(bcx, dst, bits);
            let cm1 = bcx.ins().isub(count_mod, one);
            let cf = bcx.ins().ushr(signed, cm1);
            bcx.ins().band(cf, one)
        }
        ShiftKind::Rol => bcx.ins().band(result, one),
        ShiftKind::Ror => {
            let cf = bcx.ins().ushr(result, sb);
            bcx.ins().band(cf, one)
        }
        ShiftKind::Rcl => {
            // Rcl CF = old CF when count_mod==0, else low bit of result
            let rbit = bcx.ins().band(result, one);
            bcx.ins().select(is_zero, old_cf, rbit)
        }
        ShiftKind::Rcr => {
            // Rcr CF = old CF when count_mod==0, else high bit of result
            let rbit = bcx.ins().ushr(result, sb);
            let rbit = bcx.ins().band(rbit, one);
            bcx.ins().select(is_zero, old_cf, rbit)
        }
    };

    let of_cond = match kind {
        ShiftKind::Shl => {
            let x = bcx.ins().bxor(result, dst);
            let b = bcx.ins().band(x, sign);
            bcx.ins().icmp_imm(IntCC::NotEqual, b, 0)
        }
        ShiftKind::Shr => {
            let b = bcx.ins().band(dst, sign);
            bcx.ins().icmp_imm(IntCC::NotEqual, b, 0)
        }
        ShiftKind::Sar => bcx.ins().icmp_imm(IntCC::Equal, zero_c, 1), // false
        ShiftKind::Rol => {
            let hi_sh = bcx.ins().ushr(result, sb);
            let hi = bcx.ins().band(hi_sh, one);
            let lo = bcx.ins().band(result, one);
            bcx.ins().icmp(IntCC::NotEqual, hi, lo)
        }
        ShiftKind::Ror => {
            let hi_sh = bcx.ins().ushr(result, sb);
            let b1 = bcx.ins().band(hi_sh, one);
            let sb2 = iconst_u64(bcx, u64::from(bits.saturating_sub(2)));
            let lo_sh = bcx.ins().ushr(result, sb2);
            let b2 = bcx.ins().band(lo_sh, one);
            bcx.ins().icmp(IntCC::NotEqual, b1, b2)
        }
        ShiftKind::Rcl | ShiftKind::Rcr => {
            // Rcl/Rcr OF = (CF XOR result[63]) when count_mod==1
            let hi = bcx.ins().ushr(result, sb);
            let hi_bit = bcx.ins().band(hi, one);
            bcx.ins().icmp(IntCC::NotEqual, cf_bit, hi_bit)
        }
    };
    let of_new = select_flag(bcx, of_cond, Rflags::OF);
    let old_of = flag_bit(bcx, old_rflags, Rflags::OF);
    let of_merged = bcx.ins().select(is_one, of_new, old_of);

    let mut new_flags = old_rflags;
    let cf_set = bcx.ins().icmp_imm(IntCC::NotEqual, cf_bit, 0);
    let cf_on = select_flag(bcx, cf_set, Rflags::CF);
    new_flags = replace_flag(bcx, new_flags, Rflags::CF, cf_on);
    new_flags = replace_flag(bcx, new_flags, Rflags::OF, of_merged);
    if matches!(
        kind,
        ShiftKind::Shl | ShiftKind::Shr | ShiftKind::Sar | ShiftKind::Rcl | ShiftKind::Rcr
    ) {
        new_flags = flags_zs_pf(bcx, new_flags, result, bits);
    }
    // count_mod == 0: architectural flags unchanged
    bcx.ins().select(is_zero, old_rflags, new_flags)
}

// The wide signature is a load-bearing JIT lowering helper carrying the whole lowering env.
#[allow(clippy::too_many_arguments)]
pub(super) fn lower_insn(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: &mut Value,
    pending: &mut PendingFlags,
    mem: &mut MemEnv,
    xmm: &mut [Value; 32],
) -> Result<(), String> {
    match instr.mnemonic() {
        // True no-ops: NOP/endbranch and the prefetch cache hints (no arch state).
        Mnemonic::Nop
        | Mnemonic::Endbr64
        | Mnemonic::Endbr32
        | Mnemonic::Prefetchnta
        | Mnemonic::Prefetcht0
        | Mnemonic::Prefetcht1
        | Mnemonic::Prefetcht2
        | Mnemonic::Prefetchw
        | Mnemonic::Prefetchwt1 => Ok(()),
        // Non-flag ops: leave pending (may be overwritten later).
        Mnemonic::Mov => lower_mov(bcx, instr, gpr, dirty, *rflags, mem),
        Mnemonic::Movzx => lower_movx(bcx, instr, gpr, dirty, *rflags, mem, false),
        Mnemonic::Movsx | Mnemonic::Movsxd => {
            lower_movx(bcx, instr, gpr, dirty, *rflags, mem, true)
        }
        // Sign-extend helpers on the accumulator family.
        Mnemonic::Cwde | Mnemonic::Cdqe => lower_cwde_cdqe(bcx, instr, gpr, dirty),
        Mnemonic::Cbw => lower_cbw(bcx, gpr, dirty),
        Mnemonic::Cwd => lower_cwd(bcx, gpr, dirty),
        Mnemonic::Lea => lower_lea(bcx, instr, gpr, dirty),
        Mnemonic::Push => lower_push(bcx, instr, gpr, dirty, *rflags, mem),
        Mnemonic::Pop => lower_pop(bcx, instr, gpr, dirty, *rflags, mem),
        // PUSHFQ/POPFQ/LEAVE: need live flags (push) or overwrite them (pop).
        Mnemonic::Pushfq => {
            flush_pending(bcx, rflags, pending);
            lower_pushfq(bcx, gpr, dirty, *rflags, mem, instr.ip())
        }
        Mnemonic::Popfq => {
            // Overwrites full RFLAGS — drop pending without materializing.
            *pending = PendingFlags::None;
            lower_popfq(bcx, gpr, dirty, rflags, mem, instr.ip())
        }
        Mnemonic::Leave => lower_leave(bcx, gpr, dirty, *rflags, mem, instr.ip()),
        Mnemonic::Cld => {
            // DF only; pending ALU flags stay deferred.
            *rflags = clear_flags(bcx, *rflags, Rflags::DF);
            Ok(())
        }
        Mnemonic::Std => {
            // Set DF; preserve all other flags (including deferred pending).
            let bit = iconst_u64(bcx, u64::from(Rflags::DF));
            let cleared = clear_flags(bcx, *rflags, Rflags::DF);
            *rflags = bcx.ins().bor(cleared, bit);
            Ok(())
        }
        Mnemonic::Bswap => lower_bswap(bcx, instr, gpr, dirty),
        Mnemonic::Xchg => lower_xchg(bcx, instr, gpr, dirty, *rflags, mem),
        Mnemonic::Not => lower_not(bcx, instr, gpr, dirty, *rflags, mem),
        // Bit test ops: flush pending flags, set CF directly.
        Mnemonic::Bt | Mnemonic::Bts | Mnemonic::Btr | Mnemonic::Btc => {
            flush_pending(bcx, rflags, pending);
            lower_bit_test_op(bcx, instr, gpr, dirty, rflags, mem)
        }
        // Xadd: exchange and add — flush flags, swap dst↔src, set flags as ADD.
        Mnemonic::Xadd => {
            flush_pending(bcx, rflags, pending);
            lower_xadd(bcx, instr, gpr, dirty, rflags, mem)
        }
        // CmpXchg: compare and exchange — flush flags, atomically compare with accumulator.
        Mnemonic::Cmpxchg => {
            flush_pending(bcx, rflags, pending);
            lower_cmpxchg(bcx, instr, gpr, dirty, rflags, mem)
        }
        // Bsr/Bsf: bit scans — flush flags, scan for the set-bit index.
        Mnemonic::Bsr => {
            flush_pending(bcx, rflags, pending);
            lower_bsr(bcx, instr, gpr, dirty, rflags, mem)
        }
        Mnemonic::Bsf => {
            flush_pending(bcx, rflags, pending);
            lower_bsf(bcx, instr, gpr, dirty, rflags, mem)
        }
        // Lzcnt: count leading zeros — flush flags, zero src yields width.
        Mnemonic::Lzcnt => {
            flush_pending(bcx, rflags, pending);
            lower_lzcnt(bcx, instr, gpr, dirty, rflags, mem)
        }
        // Lazy-capable ALU (overwrite pending without materializing).
        Mnemonic::Add => lower_arith_lazy(bcx, instr, gpr, dirty, rflags, pending, mem, Arith::Add),
        Mnemonic::Sub => lower_arith_lazy(bcx, instr, gpr, dirty, rflags, pending, mem, Arith::Sub),
        Mnemonic::Xor => lower_arith_lazy(bcx, instr, gpr, dirty, rflags, pending, mem, Arith::Xor),
        Mnemonic::And => lower_arith_lazy(bcx, instr, gpr, dirty, rflags, pending, mem, Arith::And),
        Mnemonic::Or => lower_arith_lazy(bcx, instr, gpr, dirty, rflags, pending, mem, Arith::Or),
        Mnemonic::Cmp => lower_cmp_test_lazy(bcx, instr, gpr, rflags, pending, mem, true),
        Mnemonic::Test => lower_cmp_test_lazy(bcx, instr, gpr, rflags, pending, mem, false),
        // Need live CF / complex flags → flush then eager.
        Mnemonic::Adc | Mnemonic::Sbb => {
            flush_pending(bcx, rflags, pending);
            lower_arith(
                bcx,
                instr,
                gpr,
                dirty,
                rflags,
                mem,
                if instr.mnemonic() == Mnemonic::Adc {
                    Arith::Adc
                } else {
                    Arith::Sbb
                },
            )
        }
        // Inc/dec: lazy with CF preserved on flush (Intel: INC/DEC do not touch CF).
        Mnemonic::Inc => lower_inc_dec_lazy(bcx, instr, gpr, dirty, rflags, pending, mem, true),
        Mnemonic::Dec => lower_inc_dec_lazy(bcx, instr, gpr, dirty, rflags, pending, mem, false),
        Mnemonic::Neg => {
            // Neg is 0-sub; can lazy as Sub{0,a,res}.
            lower_neg_lazy(bcx, instr, gpr, dirty, rflags, pending, mem)
        }
        Mnemonic::Imul => {
            flush_pending(bcx, rflags, pending);
            lower_imul(bcx, instr, gpr, dirty, rflags, mem)
        }
        Mnemonic::Div | Mnemonic::Idiv => {
            flush_pending(bcx, rflags, pending);
            lower_div(bcx, instr, gpr, dirty, rflags, mem)
        }
        // Shift/rotate: compute result now; defer flag packing (unless count_mod==0).
        Mnemonic::Shl | Mnemonic::Sal => {
            lower_shift_lazy(bcx, instr, gpr, dirty, rflags, pending, mem, ShiftKind::Shl)
        }
        Mnemonic::Shr => {
            lower_shift_lazy(bcx, instr, gpr, dirty, rflags, pending, mem, ShiftKind::Shr)
        }
        Mnemonic::Sar => {
            lower_shift_lazy(bcx, instr, gpr, dirty, rflags, pending, mem, ShiftKind::Sar)
        }
        Mnemonic::Rol => {
            lower_shift_lazy(bcx, instr, gpr, dirty, rflags, pending, mem, ShiftKind::Rol)
        }
        Mnemonic::Ror => {
            lower_shift_lazy(bcx, instr, gpr, dirty, rflags, pending, mem, ShiftKind::Ror)
        }
        Mnemonic::Rcl => {
            lower_shift_lazy(bcx, instr, gpr, dirty, rflags, pending, mem, ShiftKind::Rcl)
        }
        Mnemonic::Rcr => {
            lower_shift_lazy(bcx, instr, gpr, dirty, rflags, pending, mem, ShiftKind::Rcr)
        }
        m @ (Mnemonic::Cmove
        | Mnemonic::Cmovne
        | Mnemonic::Cmova
        | Mnemonic::Cmovae
        | Mnemonic::Cmovb
        | Mnemonic::Cmovbe
        | Mnemonic::Cmovg
        | Mnemonic::Cmovge
        | Mnemonic::Cmovl
        | Mnemonic::Cmovle
        | Mnemonic::Cmovo
        | Mnemonic::Cmovno
        | Mnemonic::Cmovs
        | Mnemonic::Cmovns
        | Mnemonic::Cmovp
        | Mnemonic::Cmovnp) => {
            flush_pending(bcx, rflags, pending);
            lower_cmov(bcx, instr, gpr, dirty, *rflags, mem, m)
        }
        m @ (Mnemonic::Sete
        | Mnemonic::Setne
        | Mnemonic::Seta
        | Mnemonic::Setae
        | Mnemonic::Setb
        | Mnemonic::Setbe
        | Mnemonic::Setg
        | Mnemonic::Setge
        | Mnemonic::Setl
        | Mnemonic::Setle
        | Mnemonic::Seto
        | Mnemonic::Setno
        | Mnemonic::Sets
        | Mnemonic::Setns
        | Mnemonic::Setp
        | Mnemonic::Setnp) => {
            flush_pending(bcx, rflags, pending);
            lower_setcc(bcx, instr, gpr, dirty, *rflags, mem, m)
        }
        Mnemonic::Movaps
        | Mnemonic::Movups
        | Mnemonic::Movdqa
        | Mnemonic::Movdqu
        | Mnemonic::Movapd
        | Mnemonic::Movupd => lower_sse_mov(bcx, instr, gpr, *rflags, mem, xmm, 16, false),
        Mnemonic::Movss => lower_sse_mov(bcx, instr, gpr, *rflags, mem, xmm, 4, true),
        Mnemonic::Movsd => lower_sse_mov(bcx, instr, gpr, *rflags, mem, xmm, 8, true),
        Mnemonic::Movq => lower_sse_movq(bcx, instr, gpr, dirty, *rflags, mem, xmm),
        Mnemonic::Movd => lower_sse_movd(bcx, instr, gpr, dirty, *rflags, mem, xmm),
        // Pmovmskb/Vpmovmskb: pack 16 xmm byte sign bits into the low GPR half.
        Mnemonic::Pmovmskb | Mnemonic::Vpmovmskb => lower_sse_pmovmskb(bcx, instr, gpr, dirty, xmm),
        Mnemonic::Movhps => lower_sse_movhps(bcx, instr, gpr, dirty, *rflags, mem, xmm),
        Mnemonic::Movhlps | Mnemonic::Movlhps => lower_sse_movhlps(bcx, instr, xmm, mem),
        Mnemonic::Xorps | Mnemonic::Xorpd | Mnemonic::Pxor => {
            lower_sse_bitwise(bcx, instr, gpr, *rflags, mem, xmm, SseBit::Xor)
        }
        Mnemonic::Andps | Mnemonic::Andpd | Mnemonic::Pand => {
            lower_sse_bitwise(bcx, instr, gpr, *rflags, mem, xmm, SseBit::And)
        }
        Mnemonic::Orps | Mnemonic::Orpd | Mnemonic::Por => {
            lower_sse_bitwise(bcx, instr, gpr, *rflags, mem, xmm, SseBit::Or)
        }
        Mnemonic::Andnps | Mnemonic::Andnpd | Mnemonic::Pandn => {
            lower_sse_bitwise(bcx, instr, gpr, *rflags, mem, xmm, SseBit::Andn)
        }
        Mnemonic::Addss => lower_sse_scalar_fp(
            bcx,
            instr,
            gpr,
            *rflags,
            mem,
            xmm,
            FloatBinOp::Add,
            FloatWidth::F32,
        ),
        Mnemonic::Subss => lower_sse_scalar_fp(
            bcx,
            instr,
            gpr,
            *rflags,
            mem,
            xmm,
            FloatBinOp::Sub,
            FloatWidth::F32,
        ),
        Mnemonic::Mulss => lower_sse_scalar_fp(
            bcx,
            instr,
            gpr,
            *rflags,
            mem,
            xmm,
            FloatBinOp::Mul,
            FloatWidth::F32,
        ),
        Mnemonic::Divss => lower_sse_scalar_fp(
            bcx,
            instr,
            gpr,
            *rflags,
            mem,
            xmm,
            FloatBinOp::Div,
            FloatWidth::F32,
        ),
        Mnemonic::Addsd => lower_sse_scalar_fp(
            bcx,
            instr,
            gpr,
            *rflags,
            mem,
            xmm,
            FloatBinOp::Add,
            FloatWidth::F64,
        ),
        Mnemonic::Subsd => lower_sse_scalar_fp(
            bcx,
            instr,
            gpr,
            *rflags,
            mem,
            xmm,
            FloatBinOp::Sub,
            FloatWidth::F64,
        ),
        Mnemonic::Mulsd => lower_sse_scalar_fp(
            bcx,
            instr,
            gpr,
            *rflags,
            mem,
            xmm,
            FloatBinOp::Mul,
            FloatWidth::F64,
        ),
        Mnemonic::Divsd => lower_sse_scalar_fp(
            bcx,
            instr,
            gpr,
            *rflags,
            mem,
            xmm,
            FloatBinOp::Div,
            FloatWidth::F64,
        ),
        Mnemonic::Addps => lower_sse_packed_fp(
            bcx,
            instr,
            gpr,
            *rflags,
            mem,
            xmm,
            FloatBinOp::Add,
            FloatWidth::F32,
        ),
        Mnemonic::Subps => lower_sse_packed_fp(
            bcx,
            instr,
            gpr,
            *rflags,
            mem,
            xmm,
            FloatBinOp::Sub,
            FloatWidth::F32,
        ),
        Mnemonic::Mulps => lower_sse_packed_fp(
            bcx,
            instr,
            gpr,
            *rflags,
            mem,
            xmm,
            FloatBinOp::Mul,
            FloatWidth::F32,
        ),
        Mnemonic::Divps => lower_sse_packed_fp(
            bcx,
            instr,
            gpr,
            *rflags,
            mem,
            xmm,
            FloatBinOp::Div,
            FloatWidth::F32,
        ),
        Mnemonic::Addpd => lower_sse_packed_fp(
            bcx,
            instr,
            gpr,
            *rflags,
            mem,
            xmm,
            FloatBinOp::Add,
            FloatWidth::F64,
        ),
        Mnemonic::Subpd => lower_sse_packed_fp(
            bcx,
            instr,
            gpr,
            *rflags,
            mem,
            xmm,
            FloatBinOp::Sub,
            FloatWidth::F64,
        ),
        Mnemonic::Mulpd => lower_sse_packed_fp(
            bcx,
            instr,
            gpr,
            *rflags,
            mem,
            xmm,
            FloatBinOp::Mul,
            FloatWidth::F64,
        ),
        Mnemonic::Divpd => lower_sse_packed_fp(
            bcx,
            instr,
            gpr,
            *rflags,
            mem,
            xmm,
            FloatBinOp::Div,
            FloatWidth::F64,
        ),
        Mnemonic::Punpcklqdq | Mnemonic::Punpckhqdq => lower_sse_punpck(bcx, instr, xmm, mem),
        Mnemonic::Punpcklbw
        | Mnemonic::Punpcklwd
        | Mnemonic::Punpckldq
        | Mnemonic::Punpckhbw
        | Mnemonic::Punpckhwd
        | Mnemonic::Punpckhdq => lower_sse_punpck_lanes(bcx, instr, gpr, *rflags, mem, xmm),
        Mnemonic::Pshufd => lower_sse_pshufd(bcx, instr, gpr, *rflags, mem, xmm),
        Mnemonic::Shufpd => lower_sse_shufpd(bcx, instr, gpr, *rflags, mem, xmm),
        Mnemonic::Pshuflw | Mnemonic::Pshufhw => {
            lower_sse_pshuflw_hw(bcx, instr, gpr, *rflags, mem, xmm)
        }
        Mnemonic::Pshufb => lower_sse_pshufb(bcx, instr, gpr, *rflags, mem, xmm),
        // Packed integer arithmetic / compare / pack.
        Mnemonic::Paddb
        | Mnemonic::Paddw
        | Mnemonic::Paddd
        | Mnemonic::Paddq
        | Mnemonic::Psubb
        | Mnemonic::Psubw
        | Mnemonic::Psubd
        | Mnemonic::Psubq
        | Mnemonic::Paddsb
        | Mnemonic::Paddsw
        | Mnemonic::Paddusb
        | Mnemonic::Paddusw
        | Mnemonic::Psubsb
        | Mnemonic::Psubsw
        | Mnemonic::Psubusb
        | Mnemonic::Psubusw
        | Mnemonic::Pmullw
        | Mnemonic::Pmulhw
        | Mnemonic::Pmulhuw
        | Mnemonic::Pmuludq
        | Mnemonic::Pmaddwd
        | Mnemonic::Pcmpeqb
        | Mnemonic::Pcmpeqw
        | Mnemonic::Pcmpeqd
        | Mnemonic::Pcmpgtb
        | Mnemonic::Pcmpgtw
        | Mnemonic::Pcmpgtd
        | Mnemonic::Packsswb
        | Mnemonic::Packssdw
        | Mnemonic::Packuswb => {
            let op = sse_int_op(instr.mnemonic()).ok_or("sse int op")?;
            lower_sse_int_binop(bcx, instr, gpr, *rflags, mem, xmm, op)
        }
        // Packed shifts (imm8 or variable XMM count).
        Mnemonic::Psllw
        | Mnemonic::Pslld
        | Mnemonic::Psllq
        | Mnemonic::Psrlw
        | Mnemonic::Psrld
        | Mnemonic::Psrlq
        | Mnemonic::Psraw
        | Mnemonic::Psrad => {
            let op = sse_shift_op(instr.mnemonic()).ok_or("sse shift op")?;
            lower_sse_shift(bcx, instr, gpr, *rflags, mem, xmm, op)
        }
        // Whole-XMM byte shifts (imm8 count).
        Mnemonic::Psrldq | Mnemonic::Pslldq => {
            lower_sse_byte_shift(bcx, instr, gpr, *rflags, mem, xmm)
        }
        // Scalar FP sqrt / min / max.
        Mnemonic::Sqrtss => {
            lower_sse_fp_unop_scalar(bcx, instr, gpr, *rflags, mem, xmm, exec::SseFpUnOp::Sqrtss)
        }
        Mnemonic::Sqrtsd => {
            lower_sse_fp_unop_scalar(bcx, instr, gpr, *rflags, mem, xmm, exec::SseFpUnOp::Sqrtsd)
        }
        Mnemonic::Sqrtps => {
            lower_sse_fp_unop_packed(bcx, instr, gpr, *rflags, mem, xmm, exec::SseFpUnOp::Sqrtps)
        }
        Mnemonic::Sqrtpd => {
            lower_sse_fp_unop_packed(bcx, instr, gpr, *rflags, mem, xmm, exec::SseFpUnOp::Sqrtpd)
        }
        Mnemonic::Minss => {
            lower_sse_fp_binop_scalar(bcx, instr, gpr, *rflags, mem, xmm, exec::SseFpBinOp::Minss)
        }
        Mnemonic::Maxss => {
            lower_sse_fp_binop_scalar(bcx, instr, gpr, *rflags, mem, xmm, exec::SseFpBinOp::Maxss)
        }
        Mnemonic::Minsd => {
            lower_sse_fp_binop_scalar(bcx, instr, gpr, *rflags, mem, xmm, exec::SseFpBinOp::Minsd)
        }
        Mnemonic::Maxsd => {
            lower_sse_fp_binop_scalar(bcx, instr, gpr, *rflags, mem, xmm, exec::SseFpBinOp::Maxsd)
        }
        Mnemonic::Minps => {
            lower_sse_fp_binop_packed(bcx, instr, gpr, *rflags, mem, xmm, exec::SseFpBinOp::Minps)
        }
        Mnemonic::Maxps => {
            lower_sse_fp_binop_packed(bcx, instr, gpr, *rflags, mem, xmm, exec::SseFpBinOp::Maxps)
        }
        Mnemonic::Minpd => {
            lower_sse_fp_binop_packed(bcx, instr, gpr, *rflags, mem, xmm, exec::SseFpBinOp::Minpd)
        }
        Mnemonic::Maxpd => {
            lower_sse_fp_binop_packed(bcx, instr, gpr, *rflags, mem, xmm, exec::SseFpBinOp::Maxpd)
        }
        // FP compare → RFLAGS (flush deferred ALU flags first).
        Mnemonic::Comiss | Mnemonic::Ucomiss | Mnemonic::Comisd | Mnemonic::Ucomisd => {
            flush_pending(bcx, rflags, pending);
            let is_double = matches!(instr.mnemonic(), Mnemonic::Comisd | Mnemonic::Ucomisd);
            lower_sse_comis(bcx, instr, gpr, rflags, mem, xmm, is_double)
        }
        // Integer ↔ FP converts.
        Mnemonic::Cvtsi2ss | Mnemonic::Cvtsi2sd => {
            let is_double = instr.mnemonic() == Mnemonic::Cvtsi2sd;
            lower_sse_cvt_gpr_to_fp(bcx, instr, gpr, dirty, *rflags, mem, xmm, is_double)
        }
        Mnemonic::Cvttss2si | Mnemonic::Cvtss2si | Mnemonic::Cvttsd2si | Mnemonic::Cvtsd2si => {
            let is_double = matches!(instr.mnemonic(), Mnemonic::Cvttsd2si | Mnemonic::Cvtsd2si);
            let trunc = matches!(instr.mnemonic(), Mnemonic::Cvttss2si | Mnemonic::Cvttsd2si);
            lower_sse_cvt_fp_to_gpr(bcx, instr, gpr, dirty, *rflags, mem, xmm, is_double, trunc)
        }
        Mnemonic::Cvtps2dq | Mnemonic::Cvtdq2ps | Mnemonic::Cvttps2dq => {
            let op = match instr.mnemonic() {
                Mnemonic::Cvtps2dq => exec::SseCvtOp::Cvtps2dq,
                Mnemonic::Cvtdq2ps => exec::SseCvtOp::Cvtdq2ps,
                _ => exec::SseCvtOp::Cvttps2dq,
            };
            lower_sse_cvt_packed(bcx, instr, gpr, *rflags, mem, xmm, op)
        }
        // CVTDQ2PD: two dwords → two doubles (native f64 conversion).
        Mnemonic::Cvtdq2pd => lower_sse_cvtdq2pd(bcx, instr, gpr, *rflags, mem, xmm),
        // CVTPS2PD: two singles → two doubles (native f32→f64 promote).
        Mnemonic::Cvtps2pd => lower_sse_cvtps2pd(bcx, instr, gpr, *rflags, mem, xmm),
        // CVTPD2DQ / CVTPD2PS: two doubles → dwords / singles (host helper).
        Mnemonic::Cvtpd2dq => lower_sse_cvtpd_packed(
            bcx,
            instr,
            gpr,
            *rflags,
            mem,
            xmm,
            exec::SseCvtOp::Cvtpd2dq,
        ),
        Mnemonic::Cvtpd2ps => lower_sse_cvtpd_packed(
            bcx,
            instr,
            gpr,
            *rflags,
            mem,
            xmm,
            exec::SseCvtOp::Cvtpd2ps,
        ),
        // Scalar converts: low lane only, upper destination bits preserved.
        Mnemonic::Cvtsd2ss => lower_sse_cvt_scalar_preserve(
            bcx,
            instr,
            gpr,
            *rflags,
            mem,
            xmm,
            exec::SseCvtOp::Cvtsd2ss,
            8,
        ),
        Mnemonic::Cvtss2sd => lower_sse_cvt_scalar_preserve(
            bcx,
            instr,
            gpr,
            *rflags,
            mem,
            xmm,
            exec::SseCvtOp::Cvtss2sd,
            4,
        ),
        other => Err(format!("not lowerable {other:?}")),
    }
}
