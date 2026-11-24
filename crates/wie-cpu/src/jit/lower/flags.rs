//! Lazy flag computation: ZS/PF/logic/add/sub/inc-dec/not/neg and flag-bit
//! select/replace helpers.

use super::emit::MemEnv;
use super::err::LowerError;
use super::gpr::{bool_to_i64, flag_set, op_width_bits, read_op_mem, sext_to_i64, write_op_mem};
use super::insn::{PendingFlags, ShiftKind};

use crate::regs::Rflags;
use cranelift::prelude::*;
use iced_x86::Instruction;
use std::cell::RefCell;
use std::collections::HashMap;

// The wide signature is a load-bearing JIT lowering helper carrying the whole lowering env.
#[allow(clippy::too_many_arguments)]
pub(super) fn lower_inc_dec_lazy(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: &mut Value,
    pending: &mut PendingFlags,
    mem: &mut MemEnv,
    inc: bool,
) -> Result<(), LowerError> {
    let bits = op_width_bits(instr, 0)?;
    let a_raw = read_op_mem(bcx, instr, 0, gpr, *rflags, mem)?;
    let a = mask_width(bcx, a_raw, bits);
    let one = bcx.ins().iconst(types::I64, 1);
    let res_raw = if inc {
        bcx.ins().iadd(a, one)
    } else {
        bcx.ins().isub(a, one)
    };
    let res = mask_width(bcx, res_raw, bits);
    write_op_mem(bcx, instr, 0, gpr, dirty, *rflags, mem, res, bits)?;
    *pending = if inc {
        PendingFlags::Inc { a, res, bits }
    } else {
        PendingFlags::Dec { a, res, bits }
    };
    Ok(())
}

pub(super) fn lower_not(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: Value,
    mem: &mut MemEnv,
) -> Result<(), LowerError> {
    let bits = op_width_bits(instr, 0)?;
    let a_raw = read_op_mem(bcx, instr, 0, gpr, rflags, mem)?;
    let a = mask_width(bcx, a_raw, bits);
    let not_a = bcx.ins().bnot(a);
    let res = mask_width(bcx, not_a, bits);
    write_op_mem(bcx, instr, 0, gpr, dirty, rflags, mem, res, bits)
}

pub(super) fn lower_neg_lazy(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: &mut Value,
    pending: &mut PendingFlags,
    mem: &mut MemEnv,
) -> Result<(), LowerError> {
    let bits = op_width_bits(instr, 0)?;
    let a_raw = read_op_mem(bcx, instr, 0, gpr, *rflags, mem)?;
    let a = mask_width(bcx, a_raw, bits);
    let zero = bcx.ins().iconst(types::I64, 0);
    let sub = bcx.ins().isub(zero, a);
    let res = mask_width(bcx, sub, bits);
    write_op_mem(bcx, instr, 0, gpr, dirty, *rflags, mem, res, bits)?;
    // NEG is 0 - a (SUB flags).
    *pending = PendingFlags::Sub {
        a: zero,
        b: a,
        res,
        bits,
    };
    Ok(())
}

// --- RFLAGS (match `regs::set_*_flags`) ---

pub(super) fn iconst_u64(bcx: &mut FunctionBuilder<'_>, v: u64) -> Value {
    bcx.ins()
        .iconst(types::I64, i64::from_ne_bytes(v.to_ne_bytes()))
}

// Per-block memo for `mask_width`: `(value index, width) -> masked value`.
// Thread-local because lowering runs on whichever thread owns the engine
// (foreground or a compile worker), and cleared at the start of every block so a
// `Value` index from one function can never be mistaken for another's.
thread_local! {
    static MASK_CACHE: RefCell<HashMap<(u32, u32), Value>> = RefCell::new(HashMap::new());
}

/// Forget the mask memo. Called once per block, before lowering it.
pub(super) fn reset_mask_cache() {
    MASK_CACHE.with(|cache| cache.borrow_mut().clear());
}

/// Truncate `v` to `bits`, reusing an identical mask already computed in this
/// block.
///
/// The memo is not a micro-optimisation in search of a problem — it was counted
/// first. `opt_level = none` means the emitter is the optimiser, so every mask
/// it writes becomes machine code; the arithmetic path masks both operands and
/// the result, and `add_sub_preds` then masks all three *again* when the flags
/// flush, on values that were already truncated by construction. Measured over
/// 2,000 sampled `7za x` blocks: 4,137 mask ops, 373 of them (9.0%) exact
/// repeats of an identical `(value, width)` pair in the same block. Over DOOM
/// Retro's sampled blocks: 109 masks, 5.5% repeats.
///
/// `band` is a pure function of its operands and a value's index is stable
/// within a function, so returning the earlier result is the same value — this
/// removes instructions, it does not change them.
pub(super) fn mask_width(bcx: &mut FunctionBuilder<'_>, v: Value, bits: u32) -> Value {
    if bits >= 64 {
        return v;
    }
    let key = (v.as_u32(), bits);
    if let Some(hit) = MASK_CACHE.with(|cache| cache.borrow().get(&key).copied()) {
        return hit;
    }
    let m = if bits == 32 {
        0xffff_ffff_u64
    } else if bits == 16 {
        0xffff
    } else {
        0xff
    };
    let mv = iconst_u64(bcx, m);
    let masked = bcx.ins().band(v, mv);
    MASK_CACHE.with(|cache| cache.borrow_mut().insert(key, masked));
    masked
}

pub(super) fn sign_bit(bits: u32) -> u64 {
    1_u64 << bits.saturating_sub(1).min(63)
}

pub(super) fn flag_bit(bcx: &mut FunctionBuilder<'_>, flags: Value, bit: Rflags) -> Value {
    let m = iconst_u64(bcx, u64::from(bit));
    bcx.ins().band(flags, m)
}

pub(super) fn replace_flag(
    bcx: &mut FunctionBuilder<'_>,
    flags: Value,
    bit: Rflags,
    on: Value,
) -> Value {
    let clear = iconst_u64(bcx, !u64::from(bit));
    let base = bcx.ins().band(flags, clear);
    bcx.ins().bor(base, on)
}

pub(super) fn select_flag(bcx: &mut FunctionBuilder<'_>, cond: Value, bit: Rflags) -> Value {
    let bit_v = iconst_u64(bcx, u64::from(bit));
    let zero = iconst_u64(bcx, 0);
    bcx.ins().select(cond, bit_v, zero)
}

#[derive(Clone)]
pub(super) struct FlagState {
    pub zf: Value,
    pub sf: Value,
    pub cf: Value,
    pub of: Value,
    pub pf: Value,
    pub af: Value,
    pub old: Value,
}

pub(super) fn decompose_packed(bcx: &mut FunctionBuilder<'_>, rflags: Value) -> FlagState {
    FlagState {
        zf: flag_set(bcx, rflags, Rflags::ZF),
        sf: flag_set(bcx, rflags, Rflags::SF),
        cf: flag_set(bcx, rflags, Rflags::CF),
        of: flag_set(bcx, rflags, Rflags::OF),
        pf: flag_set(bcx, rflags, Rflags::PF),
        af: flag_set(bcx, rflags, Rflags::AF),
        old: rflags,
    }
}

impl FlagState {
    /// Re-derive every per-flag i1 from a freshly written packed carrier.
    /// Call after each `*rflags` mutation so mid-instruction flag readers
    /// (INC/DEC CF-preserve, adc/sbb carry-in) see the current values.
    pub(super) fn resync(&mut self, bcx: &mut FunctionBuilder<'_>, packed: Value) {
        self.zf = flag_set(bcx, packed, Rflags::ZF);
        self.sf = flag_set(bcx, packed, Rflags::SF);
        self.cf = flag_set(bcx, packed, Rflags::CF);
        self.of = flag_set(bcx, packed, Rflags::OF);
        self.pf = flag_set(bcx, packed, Rflags::PF);
        self.af = flag_set(bcx, packed, Rflags::AF);
        self.old = packed;
    }

    /// Compute ADD/SUB-family flags once (predicate-direct) from operands.
    /// When `keep_cf` (INC/DEC) the incoming CF is preserved untouched.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn assign_arith(
        &mut self,
        bcx: &mut FunctionBuilder<'_>,
        dst: Value,
        src: Value,
        result: Value,
        bits: u32,
        sub: bool,
        keep_cf: bool,
    ) {
        let (cf_c, of_c, af_c) = add_sub_preds(bcx, dst, src, result, bits, sub);
        let (zf, sf, pf) = zs_pf_preds(bcx, result, bits);
        self.zf = zf;
        self.sf = sf;
        self.pf = pf;
        self.of = of_c;
        self.af = af_c;
        if !keep_cf {
            self.cf = cf_c;
        }
    }

    /// AND/OR/XOR/TEST flags: CF=OF=0, ZS/PF from result.
    pub(super) fn assign_logic(&mut self, bcx: &mut FunctionBuilder<'_>, result: Value, bits: u32) {
        let (zf, sf, pf) = zs_pf_preds(bcx, result, bits);
        self.zf = zf;
        self.sf = sf;
        self.pf = pf;
        let zero = iconst_u64(bcx, 0);
        self.cf = bcx.ins().icmp_imm(IntCC::Equal, zero, 1); // false
        self.of = bcx.ins().icmp_imm(IntCC::Equal, zero, 1); // false
    }

    /// MUL/IMUL flags: CF = OF = "the full product is not the sign extension of
    /// the low half". ZF/SF/AF/PF are architecturally undefined and keep
    /// whatever the previous writer left, exactly as the carrier does.
    ///
    /// `packed` is the carrier the writer just produced (CF/OF replaced, other
    /// bits preserved); adopting it keeps [`Self::old`] equal to the
    /// architectural word, so a later pack preserves DF/IF/reserved bits from
    /// the right base. Takes the two i1s directly — the previous shape wrote the
    /// carrier and then called [`Self::resync`], re-deriving all six flags
    /// (`band` + `icmp` + `iconst` each) from a word whose other four bits it
    /// had not touched.
    pub(super) fn assign_mul(&mut self, overflow: Value, packed: Value) {
        self.cf = overflow;
        self.of = overflow;
        self.old = packed;
    }

    /// SBB flags, predicate-direct mirror of `flags_sbb`: full-width borrow CF.
    /// `cf` is the carry-in as a WIDENED I64 0/1 lane (select_flag), never an i1.
    pub(super) fn assign_sbb(
        &mut self,
        bcx: &mut FunctionBuilder<'_>,
        d: Value,
        s: Value,
        cf: Value,
        result: Value,
        bits: u32,
    ) {
        // Base ZF/SF/PF/AF/OF from (d - s) via add_sub_preds(sub=true); the CF
        // from the base sub is replaced by the wide borrow below.
        let (_, of_c, af_c) = add_sub_preds(bcx, d, s, result, bits, true);
        let (zf, sf, pf) = zs_pf_preds(bcx, result, bits);
        self.zf = zf;
        self.sf = sf;
        self.pf = pf;
        self.of = of_c;
        self.af = af_c;
        // Correct CF for carry-in: CF = d < s + cf (full width; s+cf may exceed).
        let s_plus_cf = bcx.ins().iadd(s, cf);
        let cf_b = if bits >= 64 {
            // 64-bit: overflow of s+cf means always borrow; else d < s+cf.
            let c_ov = bcx.ins().icmp(IntCC::UnsignedLessThan, s_plus_cf, s); // s+cf wrapped
            let c_lt = bcx.ins().icmp(IntCC::UnsignedLessThan, d, s_plus_cf);
            let c_ovi = bool_to_i64(bcx, c_ov);
            let c_lti = bool_to_i64(bcx, c_lt);
            let any = bcx.ins().bor(c_ovi, c_lti);
            let zero = iconst_u64(bcx, 0);
            bcx.ins().icmp(IntCC::NotEqual, any, zero)
        } else {
            // s/d masked to operand width; s+cf may be 2^bits — then CF always set.
            bcx.ins().icmp(IntCC::UnsignedLessThan, d, s_plus_cf)
        };
        self.cf = cf_b;
    }

    /// ADC flags, predicate-direct mirror of `flags_adc`: iced
    /// `set_add_flags(d, s+cf, result)` then CF from the wide add.
    /// `cf` is the carry-in as a WIDENED I64 0/1 lane (select_flag), never an i1.
    pub(super) fn assign_adc(
        &mut self,
        bcx: &mut FunctionBuilder<'_>,
        d: Value,
        s: Value,
        cf: Value,
        result: Value,
        bits: u32,
    ) {
        let s_eff = bcx.ins().iadd(s, cf);
        let s_eff_m = mask_width(bcx, s_eff, bits);
        // Base flags from add(d, s_eff, result) via add_sub_preds(sub=false);
        // the CF from the base add is replaced by the wide-add CF below.
        let (_, of_c, af_c) = add_sub_preds(bcx, d, s_eff_m, result, bits, false);
        let (zf, sf, pf) = zs_pf_preds(bcx, result, bits);
        self.zf = zf;
        self.sf = sf;
        self.pf = pf;
        self.of = of_c;
        self.af = af_c;
        // Wide-add CF.
        let cf_b = if bits >= 64 {
            let sum_ds = bcx.ins().iadd(d, s);
            let c1 = bcx.ins().icmp(IntCC::UnsignedLessThan, sum_ds, d);
            let sum = bcx.ins().iadd(sum_ds, cf);
            let c2 = bcx.ins().icmp(IntCC::UnsignedLessThan, sum, sum_ds);
            let c1i = bool_to_i64(bcx, c1);
            let c2i = bool_to_i64(bcx, c2);
            let any = bcx.ins().bor(c1i, c2i);
            let zero = iconst_u64(bcx, 0);
            bcx.ins().icmp(IntCC::NotEqual, any, zero)
        } else {
            let t = bcx.ins().iadd(d, s);
            let sum = bcx.ins().iadd(t, cf);
            let sh = iconst_u64(bcx, u64::from(bits));
            let shifted = bcx.ins().ushr(sum, sh);
            let zero = iconst_u64(bcx, 0);
            bcx.ins().icmp(IntCC::NotEqual, shifted, zero)
        };
        self.cf = cf_b;
    }

    /// Shift/rotate flags, mirroring `materialize_shift_flags` semantics but
    /// predicate-direct: reads incoming CF/OF from the existing fs fields and
    /// writes the i1s once. count_mod==0 leaves all fields unchanged.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn assign_shift(
        &mut self,
        bcx: &mut FunctionBuilder<'_>,
        kind: ShiftKind,
        dst: Value,
        result: Value,
        count_mod: Value,
        bits: u32,
    ) {
        let one = iconst_u64(bcx, 1);
        let zero_c = iconst_u64(bcx, 0);
        let is_zero = bcx.ins().icmp_imm(IntCC::Equal, count_mod, 0);
        let is_one = bcx.ins().icmp_imm(IntCC::Equal, count_mod, 1);
        let sign = iconst_u64(bcx, 1_u64 << bits.saturating_sub(1).min(63));
        let sb = iconst_u64(bcx, u64::from(bits.saturating_sub(1)));
        // Incoming CF/OF/ZF/SF/PF: shift flags apply relative to prior state.
        let old_cf = self.cf;
        let old_of = self.of;
        let old_zf = self.zf;
        let old_sf = self.sf;
        let old_pf = self.pf;

        // CF (as an I64 0/1 lane first — Rcl/Rcr OF XOR compares it as a lane).
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
            ShiftKind::Rcl => bcx.ins().band(result, one),
            ShiftKind::Rcr => {
                let rbit = bcx.ins().ushr(result, sb);
                bcx.ins().band(rbit, one)
            }
        };
        // The packed code selects old CF back in for Rcl/Rcr at count 0; the
        // outer select below covers count_mod==0 for every kind uniformly.
        let cf_cond = bcx.ins().icmp_imm(IntCC::NotEqual, cf_bit, 0);
        self.cf = bcx.ins().select(is_zero, old_cf, cf_cond);

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
                // OF = (CF XOR result[top]); under is_one the cf lane equals the
                // shifted-out bit, matching the packed code's use of `cf_bit`.
                let hi = bcx.ins().ushr(result, sb);
                let hi_bit = bcx.ins().band(hi, one);
                bcx.ins().icmp(IntCC::NotEqual, cf_bit, hi_bit)
            }
        };
        // OF updates only when count_mod==1 (packed `of_merged` select).
        self.of = bcx.ins().select(is_one, of_cond, old_of);

        // ZS/PF only for the same kinds the packed writer gates (keeps the
        // pre-existing JIT behavior for Rcl/Rcr, which iced does not set).
        if matches!(
            kind,
            ShiftKind::Shl | ShiftKind::Shr | ShiftKind::Sar | ShiftKind::Rcl | ShiftKind::Rcr
        ) {
            let (zf, sf, pf) = zs_pf_preds(bcx, result, bits);
            self.zf = bcx.ins().select(is_zero, old_zf, zf);
            self.sf = bcx.ins().select(is_zero, old_sf, sf);
            self.pf = bcx.ins().select(is_zero, old_pf, pf);
        }
    }

    /// Pack the SSA flags into the packed rflags carrier, keeping the previous
    /// rflags value for bits we do not track (DF, IF, reserved).
    fn pack(&self, bcx: &mut FunctionBuilder<'_>) -> Value {
        let mut f = clear_flags(
            bcx,
            self.old,
            Rflags::CF | Rflags::ZF | Rflags::SF | Rflags::PF | Rflags::OF | Rflags::AF,
        );
        let cf = select_flag(bcx, self.cf, Rflags::CF);
        f = bcx.ins().bor(f, cf);
        let zf = select_flag(bcx, self.zf, Rflags::ZF);
        f = bcx.ins().bor(f, zf);
        let sf = select_flag(bcx, self.sf, Rflags::SF);
        f = bcx.ins().bor(f, sf);
        let pf = select_flag(bcx, self.pf, Rflags::PF);
        f = bcx.ins().bor(f, pf);
        let of = select_flag(bcx, self.of, Rflags::OF);
        f = bcx.ins().bor(f, of);
        let af = select_flag(bcx, self.af, Rflags::AF);
        bcx.ins().bor(f, af)
    }
}

pub(super) fn pack_state(bcx: &mut FunctionBuilder<'_>, fs: &mut FlagState) -> Value {
    let packed = fs.pack(bcx);
    // Keep the carrier in sync so later packs preserve only the untracked bits
    // (DF, IF, reserved) from the current architectural state, never a stale one.
    fs.old = packed;
    packed
}

/// Re-derive an optional FlagState from a freshly written packed carrier.
/// No-op when the SSA path is off (no state to keep fresh).
pub(super) fn resync_state(
    bcx: &mut FunctionBuilder<'_>,
    flag_state: &mut Option<FlagState>,
    packed: Value,
) {
    if let Some(fs) = flag_state {
        fs.resync(bcx, packed);
    }
}

pub(super) fn pf_cond(bcx: &mut FunctionBuilder<'_>, result: Value) -> Value {
    let mut x = mask_width(bcx, result, 8);
    let s4 = bcx.ins().ushr_imm(x, 4);
    x = bcx.ins().bxor(x, s4);
    let s2 = bcx.ins().ushr_imm(x, 2);
    x = bcx.ins().bxor(x, s2);
    let s1 = bcx.ins().ushr_imm(x, 1);
    x = bcx.ins().bxor(x, s1);
    let one = iconst_u64(bcx, 1);
    let odd = bcx.ins().band(x, one);
    bcx.ins().icmp_imm(IntCC::Equal, odd, 0)
}

pub(super) fn clear_flags(bcx: &mut FunctionBuilder<'_>, old: Value, bits: Rflags) -> Value {
    let m = iconst_u64(bcx, !u64::from(bits));
    bcx.ins().band(old, m)
}

pub(super) fn zs_pf_preds(
    bcx: &mut FunctionBuilder<'_>,
    result: Value,
    bits: u32,
) -> (Value, Value, Value) {
    let r = mask_width(bcx, result, bits);
    let is_z = bcx.ins().icmp_imm(IntCC::Equal, r, 0);
    let sb = iconst_u64(bcx, sign_bit(bits));
    let sign = bcx.ins().band(r, sb);
    let is_s = bcx.ins().icmp_imm(IntCC::NotEqual, sign, 0);
    let is_pf = pf_cond(bcx, r);
    (is_z, is_s, is_pf)
}

pub(super) fn flags_zs_pf(
    bcx: &mut FunctionBuilder<'_>,
    old: Value,
    result: Value,
    bits: u32,
) -> Value {
    let f = clear_flags(bcx, old, Rflags::ZF | Rflags::SF | Rflags::PF);
    let (is_z, is_s, is_pf) = zs_pf_preds(bcx, result, bits);
    let zf = select_flag(bcx, is_z, Rflags::ZF);
    let f = bcx.ins().bor(f, zf);
    let sf = select_flag(bcx, is_s, Rflags::SF);
    let f = bcx.ins().bor(f, sf);
    let pf = select_flag(bcx, is_pf, Rflags::PF);
    bcx.ins().bor(f, pf)
}

pub(super) fn flags_logic(
    bcx: &mut FunctionBuilder<'_>,
    old: Value,
    result: Value,
    bits: u32,
) -> Value {
    let f = clear_flags(
        bcx,
        old,
        Rflags::ZF | Rflags::SF | Rflags::PF | Rflags::CF | Rflags::OF,
    );
    flags_zs_pf(bcx, f, result, bits)
}

pub(super) fn add_sub_preds(
    bcx: &mut FunctionBuilder<'_>,
    dst: Value,
    src: Value,
    result: Value,
    bits: u32,
    sub: bool,
) -> (Value, Value, Value) {
    let d = mask_width(bcx, dst, bits);
    let s = mask_width(bcx, src, bits);
    let r = mask_width(bcx, result, bits);
    let cf_cond = if sub {
        bcx.ins().icmp(IntCC::UnsignedLessThan, d, s)
    } else {
        bcx.ins().icmp(IntCC::UnsignedLessThan, r, d)
    };
    let sb = iconst_u64(bcx, sign_bit(bits));
    let both = if sub {
        let ds = bcx.ins().bxor(d, s);
        let dr = bcx.ins().bxor(d, r);
        bcx.ins().band(ds, dr)
    } else {
        let dr = bcx.ins().bxor(d, r);
        let sr = bcx.ins().bxor(s, r);
        bcx.ins().band(dr, sr)
    };
    let of_bits = bcx.ins().band(both, sb);
    let of_cond = bcx.ins().icmp_imm(IntCC::NotEqual, of_bits, 0);
    let x = bcx.ins().bxor(d, s);
    let y = bcx.ins().bxor(x, r);
    let ten = iconst_u64(bcx, 0x10);
    let af_b = bcx.ins().band(y, ten);
    let af_cond = bcx.ins().icmp_imm(IntCC::NotEqual, af_b, 0);
    (cf_cond, of_cond, af_cond)
}

pub(super) fn pack_add_sub(
    bcx: &mut FunctionBuilder<'_>,
    old: Value,
    cf_cond: Value,
    of_cond: Value,
    af_cond: Value,
    result: Value,
    bits: u32,
) -> Value {
    let f = clear_flags(
        bcx,
        old,
        Rflags::CF | Rflags::ZF | Rflags::SF | Rflags::PF | Rflags::OF | Rflags::AF,
    );
    let cf = select_flag(bcx, cf_cond, Rflags::CF);
    let f = bcx.ins().bor(f, cf);
    let f = flags_zs_pf(bcx, f, result, bits);
    let of = select_flag(bcx, of_cond, Rflags::OF);
    let f = bcx.ins().bor(f, of);
    let af = select_flag(bcx, af_cond, Rflags::AF);
    bcx.ins().bor(f, af)
}

pub(super) fn flags_add(
    bcx: &mut FunctionBuilder<'_>,
    old: Value,
    dst: Value,
    src: Value,
    result: Value,
    bits: u32,
) -> Value {
    let (cf_cond, of_cond, af_cond) = add_sub_preds(bcx, dst, src, result, bits, false);
    pack_add_sub(bcx, old, cf_cond, of_cond, af_cond, result, bits)
}

pub(super) fn flags_sub(
    bcx: &mut FunctionBuilder<'_>,
    old: Value,
    dst: Value,
    src: Value,
    result: Value,
    bits: u32,
) -> Value {
    let (cf_cond, of_cond, af_cond) = add_sub_preds(bcx, dst, src, result, bits, true);
    pack_add_sub(bcx, old, cf_cond, of_cond, af_cond, result, bits)
}

#[cfg(test)]
mod mask_memo_tests {
    use super::*;
    use cranelift_codegen::ir::{Function, Opcode};

    /// Build a one-parameter function builder and hand back the block param.
    fn env() -> (Function, FunctionBuilderContext) {
        let mut func = Function::new();
        func.signature.params.push(AbiParam::new(types::I64));
        (func, FunctionBuilderContext::new())
    }

    fn count_bands(func: &Function) -> usize {
        func.layout
            .blocks()
            .flat_map(|b| func.layout.block_insts(b).collect::<Vec<_>>())
            .filter(|&i| func.dfg.insts[i].opcode() == Opcode::Band)
            .count()
    }

    #[test]
    fn masking_the_same_value_three_times_emits_one_band() {
        let (mut func, mut fctx) = env();
        let mut bcx = FunctionBuilder::new(&mut func, &mut fctx);
        let entry = bcx.create_block();
        bcx.append_block_params_for_function_params(entry);
        bcx.switch_to_block(entry);
        bcx.seal_block(entry);
        let v = bcx.block_params(entry)[0];

        reset_mask_cache();
        let first = mask_width(&mut bcx, v, 32);
        let second = mask_width(&mut bcx, v, 32);
        let third = mask_width(&mut bcx, v, 32);
        bcx.ins().return_(&[]);
        bcx.seal_all_blocks();
        bcx.finalize();

        assert_eq!(first, second, "the memo returns the same value");
        assert_eq!(second, third);
        assert_eq!(count_bands(&func), 1, "three identical masks, one `band`");
    }

    #[test]
    fn different_widths_are_different_masks() {
        let (mut func, mut fctx) = env();
        let mut bcx = FunctionBuilder::new(&mut func, &mut fctx);
        let entry = bcx.create_block();
        bcx.append_block_params_for_function_params(entry);
        bcx.switch_to_block(entry);
        bcx.seal_block(entry);
        let v = bcx.block_params(entry)[0];

        reset_mask_cache();
        let wide = mask_width(&mut bcx, v, 32);
        let narrow = mask_width(&mut bcx, v, 8);
        bcx.ins().return_(&[]);
        bcx.seal_all_blocks();
        bcx.finalize();

        assert_ne!(wide, narrow, "a width must not hit another width's memo");
        assert_eq!(count_bands(&func), 2);
    }

    /// A cleared memo must emit a fresh mask: `Value` indices restart at 0 in
    /// the next function, so a surviving entry would mask the wrong value.
    #[test]
    fn clearing_the_memo_forces_a_fresh_band() {
        let (mut func, mut fctx) = env();
        let mut bcx = FunctionBuilder::new(&mut func, &mut fctx);
        let entry = bcx.create_block();
        bcx.append_block_params_for_function_params(entry);
        bcx.switch_to_block(entry);
        bcx.seal_block(entry);
        let v = bcx.block_params(entry)[0];

        reset_mask_cache();
        let first = mask_width(&mut bcx, v, 16);
        reset_mask_cache();
        let second = mask_width(&mut bcx, v, 16);
        bcx.ins().return_(&[]);
        bcx.seal_all_blocks();
        bcx.finalize();

        assert_ne!(first, second, "a cleared memo emits a new `band`");
        assert_eq!(count_bands(&func), 2);
    }
}
