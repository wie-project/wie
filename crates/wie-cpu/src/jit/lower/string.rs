//! Bulk string lowering: inline Neon copy for small REP MOVS/STOS, `stos` splat,
//! and `lower_string`.

use super::super::config::JitConfig;
use super::OFF_RFLAGS;
use super::emit::{MemEnv, check_fault_after_ucrt};
use super::flags::iconst_u64;

use super::super::block::string_op_size;

use crate::exec::{self, StringOpKind};
use crate::regs::Rflags;
use cranelift::codegen::ir::BlockArg;
use cranelift::prelude::*;
use iced_x86::Instruction;

/// Emit dual-path inline Neon copy for small REP MOVS/STOS (16–64 bytes).
///
/// Fast path: soft-translate spans + unrolled `I8X16` stores. Slow path: `wie_jit_string`.
/// Returns exit RIP SSA value, or `None` if preconditions fail (no IR emitted).
// The wide signature is a load-bearing JIT lowering helper carrying the whole lowering env.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_lower_inline_rep(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    rflags: &mut Value,
    gpr_loaded: &mut [bool; 16],
    mem: &mut MemEnv,
    kind: StringOpKind,
    size: u32,
) -> Option<Value> {
    if !JitConfig::get().string_inline_enabled() || !JitConfig::get().simd_enabled() {
        return None;
    }
    // Only the two block-copyable kinds; SCAS/CMPS/LODS need element semantics.
    let is_movs = match kind {
        StringOpKind::Movs => true,
        StringOpKind::Stos => false,
        StringOpKind::Lods | StringOpKind::Scas | StringOpKind::Cmps => return None,
    };
    if !exec::RepPrefix::from_instr(instr).rep {
        return None;
    }
    if !matches!(size, 1 | 2 | 4 | 8) {
        return None;
    }
    let span_ref = mem.host_span_ref?;
    let string_ref = mem.string_ref?;
    // Ensure RSI/RDI/RCX/(RAX for STOS) are in SSA before building CFG.
    if !gpr_loaded[1] || !gpr_loaded[7] {
        return None;
    }
    if is_movs && !gpr_loaded[6] {
        return None;
    }
    if !is_movs && !gpr_loaded[0] {
        return None;
    }

    // DF clear + byte_len in [8, 64]. Lengths are handled exactly (including
    // non-multiples of 16) by `emit_inline_copy_chunks`; the floor is 8 because
    // that is the smallest unit the overlapping-tail scheme covers.
    let df_mask = iconst_u64(bcx, u64::from(Rflags::DF));
    let df_bits = bcx.ins().band(*rflags, df_mask);
    let df_clear = bcx.ins().icmp_imm(IntCC::Equal, df_bits, 0);
    let rcx = gpr[1];
    let size_v = iconst_u64(bcx, u64::from(size));
    let byte_len = bcx.ins().imul(rcx, size_v);
    let min_len = iconst_u64(bcx, 8);
    let max64 = iconst_u64(bcx, 64);
    let ge_min = bcx
        .ins()
        .icmp(IntCC::UnsignedGreaterThanOrEqual, byte_len, min_len);
    let le_max = bcx
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, byte_len, max64);
    let len_ok = bcx.ins().band(ge_min, le_max);
    let eligible = bcx.ins().band(df_clear, len_ok);

    let cont_fast = bcx.create_block();
    let cont_slow = bcx.create_block();
    let done = bcx.create_block();
    // done params: exit_rip + gpr[0,1,6,7] + rflags (string can touch these)
    bcx.append_block_param(done, types::I64); // rip
    bcx.append_block_param(done, types::I64); // rax
    bcx.append_block_param(done, types::I64); // rcx
    bcx.append_block_param(done, types::I64); // rsi
    bcx.append_block_param(done, types::I64); // rdi
    bcx.append_block_param(done, types::I64); // rflags

    bcx.ins().brif(eligible, cont_fast, &[], cont_slow, &[]);

    // ---- fast path ----
    bcx.switch_to_block(cont_fast);
    bcx.seal_block(cont_fast);
    let rdi = gpr[7];
    let write_one = iconst_u64(bcx, 1);
    let call_dst = bcx
        .ins()
        .call(span_ref, &[mem.ctx_ptr, rdi, byte_len, write_one]);
    let dst_host = bcx.inst_results(call_dst)[0];
    let dst_ok = bcx.ins().icmp_imm(IntCC::NotEqual, dst_host, 0);
    let do_copy = bcx.create_block();
    bcx.ins().brif(dst_ok, do_copy, &[], cont_slow, &[]);

    bcx.switch_to_block(do_copy);
    bcx.seal_block(do_copy);

    let new_rax = gpr[0];
    let new_rcx = iconst_u64(bcx, 0);
    let (new_rsi, new_rdi) = if is_movs {
        let zero = iconst_u64(bcx, 0);
        let call_src = bcx
            .ins()
            .call(span_ref, &[mem.ctx_ptr, gpr[6], byte_len, zero]);
        let src_host = bcx.inst_results(call_src)[0];
        let src_ok = bcx.ins().icmp_imm(IntCC::NotEqual, src_host, 0);
        // Chunked (and overlapping-tail) copying only matches x86 `rep movs`
        // byte-ascending semantics when source and destination do not overlap:
        // a forward byte copy propagates a pattern where a 16-byte block copy
        // does not. Require disjoint host ranges and fall back to the element
        // loop otherwise.
        let dst_end = bcx.ins().iadd(dst_host, byte_len);
        let src_end = bcx.ins().iadd(src_host, byte_len);
        let dst_before_src = bcx
            .ins()
            .icmp(IntCC::UnsignedLessThanOrEqual, dst_end, src_host);
        let src_before_dst = bcx
            .ins()
            .icmp(IntCC::UnsignedLessThanOrEqual, src_end, dst_host);
        let disjoint = bcx.ins().bor(dst_before_src, src_before_dst);
        let src_usable = bcx.ins().band(src_ok, disjoint);
        let copy_body = bcx.create_block();
        bcx.ins().brif(src_usable, copy_body, &[], cont_slow, &[]);
        bcx.switch_to_block(copy_body);
        bcx.seal_block(copy_body);
        emit_inline_copy_chunks(
            bcx,
            mem,
            dst_host,
            byte_len,
            InlineCopySrc::Move { src_host },
        );
        (
            bcx.ins().iadd(gpr[6], byte_len),
            bcx.ins().iadd(gpr[7], byte_len),
        )
    } else {
        let pattern = stos_splat_pattern(bcx, mem, gpr[0], size)?;
        emit_inline_copy_chunks(
            bcx,
            mem,
            dst_host,
            byte_len,
            InlineCopySrc::Fill { pattern },
        );
        (gpr[6], bcx.ins().iadd(gpr[7], byte_len))
    };
    let next = iconst_u64(bcx, instr.next_ip());
    bcx.ins().jump(
        done,
        &[
            BlockArg::Value(next),
            BlockArg::Value(new_rax),
            BlockArg::Value(new_rcx),
            BlockArg::Value(new_rsi),
            BlockArg::Value(new_rdi),
            BlockArg::Value(*rflags),
        ],
    );

    // ---- slow path: existing bulk helper ----
    bcx.switch_to_block(cont_slow);
    bcx.seal_block(cont_slow);
    for i in 0..16 {
        if gpr_loaded[i] {
            let off = i64::try_from(i.saturating_mul(8)).unwrap_or(0);
            let p = bcx.ins().iadd_imm(mem.ctx_ptr, off);
            bcx.ins().store(mem.flags, gpr[i], p, 0);
        }
    }
    let rflags_ptr = bcx.ins().iadd_imm(mem.ctx_ptr, i64::from(OFF_RFLAGS));
    bcx.ins().store(mem.flags, *rflags, rflags_ptr, 0);
    // Reached only when the REP prefix is present (checked on entry), so the
    // shared encoder always sets the `rep` bit here.
    let flags = exec::RepPrefix::from_instr(instr).to_abi();
    let op_v = iconst_u64(bcx, kind.to_abi());
    let size_c = iconst_u64(bcx, u64::from(size));
    let flags_v = iconst_u64(bcx, flags);
    let ip_v = iconst_u64(bcx, instr.ip());
    let call = bcx
        .ins()
        .call(string_ref, &[mem.ctx_ptr, op_v, size_c, flags_v, ip_v]);
    let stay = bcx.inst_results(call)[0];
    let mut slow_gpr = *gpr;
    for &i in &[0_usize, 1, 6, 7] {
        let off = i64::try_from(i.saturating_mul(8)).unwrap_or(0);
        let p = bcx.ins().iadd_imm(mem.ctx_ptr, off);
        slow_gpr[i] = bcx.ins().load(types::I64, mem.flags, p, 0);
    }
    let slow_flags = bcx.ins().load(types::I64, mem.flags, rflags_ptr, 0);
    check_fault_after_ucrt(bcx, mem, &slow_gpr, slow_flags);
    let stay_nz = bcx.ins().icmp_imm(IntCC::NotEqual, stay, 0);
    let cur_ip = iconst_u64(bcx, instr.ip());
    let next_ip = iconst_u64(bcx, instr.next_ip());
    let exit_rip = bcx.ins().select(stay_nz, cur_ip, next_ip);
    bcx.ins().jump(
        done,
        &[
            BlockArg::Value(exit_rip),
            BlockArg::Value(slow_gpr[0]),
            BlockArg::Value(slow_gpr[1]),
            BlockArg::Value(slow_gpr[6]),
            BlockArg::Value(slow_gpr[7]),
            BlockArg::Value(slow_flags),
        ],
    );

    bcx.switch_to_block(done);
    bcx.seal_block(done);
    let params = bcx.block_params(done);
    gpr[0] = params[1];
    gpr[1] = params[2];
    gpr[6] = params[3];
    gpr[7] = params[4];
    *rflags = params[5];
    gpr_loaded[0] = true;
    gpr_loaded[1] = true;
    gpr_loaded[6] = true;
    gpr_loaded[7] = true;
    Some(params[0])
}

/// Width of one unrolled store in an inline REP copy.
///
/// Only these two widths are emitted, so a plain integer width would admit
/// values (7, 32, 0) the emitter cannot honour.
#[derive(Clone, Copy)]
pub(super) enum CopyUnit {
    Bytes8,
    Bytes16,
}

/// Source of the bytes an inline REP block copy writes.
///
/// Replaces an `Option<Value>` that overloaded `None` to mean "MOVS, read from
/// a separate `src_host` argument" and `Some(v)` to mean "STOS, splat `v`" —
/// which additionally required passing `dst_host` as the source for fills.
/// Encoding the source in the variant removes that dummy argument.
#[derive(Clone, Copy)]
pub(super) enum InlineCopySrc {
    /// MOVS: load from `src_host + off`, matching the destination offset.
    Move { src_host: Value },
    /// STOS: store `pattern`, an `I8X16` splat whose every 8-byte half also
    /// carries the fill value (so an 8-byte unit can reuse lane 0).
    Fill { pattern: Value },
}

/// Emit an exact copy/fill of `byte_len` bytes for `byte_len` in [8, 64].
pub(super) fn emit_inline_copy_chunks(
    bcx: &mut FunctionBuilder<'_>,
    mem: &MemEnv,
    dst_host: Value,
    byte_len: Value,
    src: InlineCopySrc,
) {
    // Full 16-byte chunks at constant offsets 0/16/32/48.
    for chunk in 0..4_u64 {
        let off = iconst_u64(bcx, chunk.saturating_mul(16));
        let need = iconst_u64(bcx, chunk.saturating_mul(16).saturating_add(16));
        let take = bcx
            .ins()
            .icmp(IntCC::UnsignedGreaterThanOrEqual, byte_len, need);
        let do_chunk = bcx.create_block();
        let next_chunk = bcx.create_block();
        bcx.ins().brif(take, do_chunk, &[], next_chunk, &[]);
        bcx.switch_to_block(do_chunk);
        bcx.seal_block(do_chunk);
        emit_one_unit(bcx, mem, dst_host, off, CopyUnit::Bytes16, src);
        bcx.ins().jump(next_chunk, &[]);
        bcx.switch_to_block(next_chunk);
        bcx.seal_block(next_chunk);
    }

    // Overlapping 16-byte tail for lengths that are not a multiple of 16.
    //
    // Without this, a length like 20 stored only chunk 0 (bytes 0..16) while the
    // caller still advanced RSI/RDI by 20 and zeroed RCX — the trailing
    // `len & 15` bytes were silently never written. Copying the *last* 16 bytes
    // at `len - 16` closes the gap; the overlap with an already-written chunk
    // re-stores identical bytes, which is why MOVS additionally requires the
    // host ranges to be disjoint (checked by the caller).
    {
        let rem = bcx.ins().band_imm(byte_len, 15);
        let has_tail = bcx.ins().icmp_imm(IntCC::NotEqual, rem, 0);
        let big_enough = bcx
            .ins()
            .icmp_imm(IntCC::UnsignedGreaterThanOrEqual, byte_len, 16);
        let need_tail = bcx.ins().band(has_tail, big_enough);
        let do_tail = bcx.create_block();
        let after_tail = bcx.create_block();
        bcx.ins().brif(need_tail, do_tail, &[], after_tail, &[]);
        bcx.switch_to_block(do_tail);
        bcx.seal_block(do_tail);
        let tail_off = bcx.ins().iadd_imm(byte_len, -16);
        emit_one_unit(bcx, mem, dst_host, tail_off, CopyUnit::Bytes16, src);
        bcx.ins().jump(after_tail, &[]);
        bcx.switch_to_block(after_tail);
        bcx.seal_block(after_tail);
    }

    // Sub-16 lengths [8, 15]: an 8-byte lead plus an overlapping 8-byte tail
    // covers [0, len) exactly. Extends the inline fast path below the old
    // 16-byte floor, capturing small CRT `memcpy`/`memset` fragments that
    // previously fell through to the bulk helper.
    {
        let small = bcx.ins().icmp_imm(IntCC::UnsignedLessThan, byte_len, 16);
        let ge8 = bcx
            .ins()
            .icmp_imm(IntCC::UnsignedGreaterThanOrEqual, byte_len, 8);
        let need_small = bcx.ins().band(small, ge8);
        let do_small = bcx.create_block();
        let after_small = bcx.create_block();
        bcx.ins().brif(need_small, do_small, &[], after_small, &[]);
        bcx.switch_to_block(do_small);
        bcx.seal_block(do_small);
        let zero_off = iconst_u64(bcx, 0);
        emit_one_unit(bcx, mem, dst_host, zero_off, CopyUnit::Bytes8, src);
        let tail8 = bcx.ins().iadd_imm(byte_len, -8);
        emit_one_unit(bcx, mem, dst_host, tail8, CopyUnit::Bytes8, src);
        bcx.ins().jump(after_small, &[]);
        bcx.switch_to_block(after_small);
        bcx.seal_block(after_small);
    }
}

/// Store one unit at `dst_host + off`, sourcing per [`InlineCopySrc`].
pub(super) fn emit_one_unit(
    bcx: &mut FunctionBuilder<'_>,
    mem: &MemEnv,
    dst_host: Value,
    off: Value,
    unit: CopyUnit,
    src: InlineCopySrc,
) {
    let dp = bcx.ins().iadd(dst_host, off);
    let value = match (unit, src) {
        (CopyUnit::Bytes16, InlineCopySrc::Fill { pattern }) => pattern,
        (CopyUnit::Bytes16, InlineCopySrc::Move { src_host }) => {
            let sp = bcx.ins().iadd(src_host, off);
            bcx.ins().load(types::I8X16, mem.guest_flags, sp, 0)
        }
        (CopyUnit::Bytes8, InlineCopySrc::Fill { pattern }) => {
            // Reuse the I8X16 splat: every 8-byte half carries the pattern.
            let as_i64x2 =
                bcx.ins()
                    .bitcast(types::I64X2, super::bitcast_flags(mem.guest_flags), pattern);
            bcx.ins().extractlane(as_i64x2, 0)
        }
        (CopyUnit::Bytes8, InlineCopySrc::Move { src_host }) => {
            let sp = bcx.ins().iadd(src_host, off);
            bcx.ins().load(types::I64, mem.guest_flags, sp, 0)
        }
    };
    bcx.ins().store(mem.guest_flags, value, dp, 0);
}

pub(super) fn stos_splat_pattern(
    bcx: &mut FunctionBuilder<'_>,
    mem: &MemEnv,
    rax: Value,
    size: u32,
) -> Option<Value> {
    match size {
        1 => {
            let m = iconst_u64(bcx, 0xff);
            let b = bcx.ins().band(rax, m);
            let b8 = bcx.ins().ireduce(types::I8, b);
            Some(bcx.ins().splat(types::I8X16, b8))
        }
        2 => {
            let m = iconst_u64(bcx, 0xffff);
            let w = bcx.ins().band(rax, m);
            let w16 = bcx.ins().ireduce(types::I16, w);
            let s = bcx.ins().splat(types::I16X8, w16);
            Some(
                bcx.ins()
                    .bitcast(types::I8X16, super::bitcast_flags(mem.flags), s),
            )
        }
        4 => {
            let m = iconst_u64(bcx, 0xffff_ffff);
            let d = bcx.ins().band(rax, m);
            let d32 = bcx.ins().ireduce(types::I32, d);
            let s = bcx.ins().splat(types::I32X4, d32);
            Some(
                bcx.ins()
                    .bitcast(types::I8X16, super::bitcast_flags(mem.flags), s),
            )
        }
        8 => {
            let s = bcx.ins().splat(types::I64X2, rax);
            Some(
                bcx.ins()
                    .bitcast(types::I8X16, super::bitcast_flags(mem.flags), s),
            )
        }
        _ => None,
    }
}

/// Lower a string op via host bulk helper; returns exit RIP value.
pub(super) fn lower_string(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    rflags: &mut Value,
    gpr_loaded: &mut [bool; 16],
    mem: &mut MemEnv,
) -> Result<Value, String> {
    let raw_size = string_op_size(instr).ok_or("string size")?;
    let mnemonic = instr.mnemonic();
    let (kind, size) = StringOpKind::from_mnemonic(mnemonic, raw_size)
        .ok_or_else(|| format!("string op {mnemonic:?}"))?;

    // Dual-path inline for small REP MOVS/STOS when helpers available.
    if matches!(kind, StringOpKind::Stos | StringOpKind::Movs)
        && JitConfig::get().string_inline_enabled()
        && mem.host_span_ref.is_some()
        && let Some(rip) =
            try_lower_inline_rep(bcx, instr, gpr, rflags, gpr_loaded, mem, kind, size)
    {
        return Ok(rip);
    }

    let string_ref = mem.string_ref.ok_or("string helper missing")?;
    let flags = exec::RepPrefix::from_instr(instr).to_abi();

    // Flush SSA GPRs + flags into JitCtx for the host helper.
    for i in 0..16 {
        if gpr_loaded[i] {
            let off = i64::try_from(i.saturating_mul(8)).unwrap_or(0);
            let p = bcx.ins().iadd_imm(mem.ctx_ptr, off);
            bcx.ins().store(mem.flags, gpr[i], p, 0);
        }
    }
    let rflags_ptr = bcx.ins().iadd_imm(mem.ctx_ptr, i64::from(OFF_RFLAGS));
    bcx.ins().store(mem.flags, *rflags, rflags_ptr, 0);

    let op_v = iconst_u64(bcx, kind.to_abi());
    let size_v = iconst_u64(bcx, u64::from(size));
    let flags_v = iconst_u64(bcx, flags);
    let ip_v = iconst_u64(bcx, instr.ip());
    let call = bcx
        .ins()
        .call(string_ref, &[mem.ctx_ptr, op_v, size_v, flags_v, ip_v]);
    let stay = bcx.inst_results(call)[0];

    // Reload GPRs / flags first so a fault exit carries partial string progress.
    for &i in &[0_usize, 1, 6, 7] {
        let off = i64::try_from(i.saturating_mul(8)).unwrap_or(0);
        let p = bcx.ins().iadd_imm(mem.ctx_ptr, off);
        gpr[i] = bcx.ins().load(types::I64, mem.flags, p, 0);
        gpr_loaded[i] = true;
    }
    *rflags = bcx.ins().load(types::I64, mem.flags, rflags_ptr, 0);

    // Fault check (exit_args now hold post-helper state).
    check_fault_after_ucrt(bcx, mem, gpr, *rflags);

    let next = iconst_u64(bcx, instr.next_ip());
    let cur = iconst_u64(bcx, instr.ip());
    let stay_nz = bcx.ins().icmp_imm(IntCC::NotEqual, stay, 0);
    Ok(bcx.ins().select(stay_nz, cur, next))
}
