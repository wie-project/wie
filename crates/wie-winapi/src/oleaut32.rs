//! Minimal `OLEAUT32` surface for real tools (7za BSTR helpers).
//!
//! Clean-room stubs for ordinal imports used by MSVC-linked CLI tools.
//!
//! **Critical:** `VariantCopy` must deep-copy `VT_BSTR`. A shallow 24-byte memcpy
//! makes `CPropVariant::InternalCopy` share one BSTR; the source destructor then
//! frees it and leaves a dangling pointer in the destination — 7za method props
//! with non-numeric values (`-md=64k`, `-m0=Copy`, …) throw `E_INVALIDARG`.

use crate::{HandlerContext, WinApiHandlerResult, WinApiState};
use anyhow::{Context, Result};
use std::sync::Mutex;

/// `VARENUM` / `VARTYPE` constants.
const VT_I2: u16 = 2;
const VT_I4: u16 = 3;
const VT_R4: u16 = 4;
const VT_R8: u16 = 5;
const VT_DATE: u16 = 7;
const VT_BSTR: u16 = 8;
const VT_BOOL: u16 = 11;
const VT_I8: u16 = 20;
const VT_UI4: u16 = 19;

/// HRESULT codes the stubs return (winerror.h).
/// `E_INVALIDARG` — one or more arguments are invalid.
const E_INVALIDARG: u64 = 0x8007_0057;
/// `E_OUTOFMEMORY` — the operation could not allocate memory.
const E_OUTOFMEMORY: u64 = 0x8007_000E;
/// `DISP_E_DIVBYZERO` — a variant arithmetic operation divided by zero.
const DISP_E_DIVBYZERO: u64 = 0x8002_0011;
/// `DISP_E_MEMBERNOTFOUND` — fallback for an unsupported source VARTYPE.
const DISP_E_MEMBERNOTFOUND: u64 = 0x8002_0003;

/// Soft-dispatch path for OLEAUT32 (name or `ORDINAL N`).
pub fn dispatch_oleaut32(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    // OLEAUT32 export ordinals (Wine / Windows): 2 Alloc, 4 AllocLen, 6 Free,
    // 7 StringLen, 8 VariantInit, 9 VariantClear, 10 VariantCopy, 11 CopyInd.
    // SysStringByteLen is ordinal **149**, not 8.
    let key = match n.as_str() {
        "sysallocstring" | "ordinal 2" => "sysallocstring",
        "sysreallocstring" | "ordinal 3" => "sysreallocstring",
        "sysallocstringlen" | "ordinal 4" | "sysallocstringbytelen" | "ordinal 150" => {
            "sysallocstringlen"
        }
        "sysreallocstringlen" | "ordinal 5" => "sysreallocstringlen",
        "sysfreestring" | "ordinal 6" => "sysfreestring",
        "sysstringlen" | "ordinal 7" => "sysstringlen",
        "variantinit" | "ordinal 8" => "variantinit",
        "variantclear" | "ordinal 9" => "variantclear",
        // Ordinal 11 = VariantCopyInd; deep-copy path is enough for current guests.
        "variantcopy" | "ordinal 10" | "variantcopyind" | "ordinal 11" => "variantcopy",
        "sysstringbytelen" | "ordinal 149" => "sysstringbyteslen",
        other => other,
    };
    match key {
        "sysallocstring" => Ok(Some(handle_sys_alloc_string(ctx)?)),
        "sysallocstringlen" | "sysreallocstring" | "sysreallocstringlen" => {
            Ok(Some(handle_sys_alloc_string_len(ctx)?))
        }
        "sysfreestring" => Ok(Some(handle_sys_free_string(ctx)?)),
        "sysstringlen" => Ok(Some(handle_sys_string_len(ctx)?)),
        "sysstringbyteslen" => Ok(Some(handle_sys_string_byte_len(ctx)?)),
        "variantinit" => Ok(Some(handle_variant_init(ctx)?)),
        "variantclear" => Ok(Some(handle_variant_clear(ctx)?)),
        "variantcopy" => Ok(Some(handle_variant_copy(ctx)?)),
        // Variant arithmetic
        "varadd" | "varsub" | "varmul" | "vardiv" | "varmod" => {
            Ok(Some(handle_var_math(ctx, key)?))
        }
        // Variant type conversions: BSTR ← num
        "varbstrfromi4" => Ok(Some(handle_var_bstr_from_num(ctx, VT_I4)?)),
        "varbstrfromr4" => Ok(Some(handle_var_bstr_from_num(ctx, VT_R4)?)),
        "varbstrfromr8" | "varbstrfromdate" => Ok(Some(handle_var_bstr_from_num(ctx, VT_R8)?)),
        // Variant type conversions: num ← BSTR
        "vari4frombstr" | "vari4fromr8" => Ok(Some(handle_var_num_from_bstr(ctx, VT_I4)?)),
        "varr4frombstr" => Ok(Some(handle_var_num_from_bstr(ctx, VT_R4)?)),
        "varr8frombstr" | "vardatefrombstr" => Ok(Some(handle_var_num_from_bstr(ctx, VT_R8)?)),
        // Variant type conversions: num ← num
        "varr8fromi4" => Ok(Some(handle_var_num_from_num(ctx, VT_R8, VT_I4)?)),
        "vardatefromi4" => Ok(Some(handle_var_num_from_num(ctx, VT_DATE, VT_I4)?)),
        "vardatefromr8" => Ok(Some(handle_var_num_from_num(ctx, VT_DATE, VT_R8)?)),
        // SafeArray family
        "safearraycreate" => Ok(Some(handle_safe_array_create(ctx)?)),
        "safearraydestroy" => Ok(Some(handle_safe_array_destroy(ctx)?)),
        "safearrayaccessdata" => Ok(Some(handle_safe_array_access_data(ctx)?)),
        "safearrayunaccessdata" => Ok(Some(handle_safe_array_unaccess_data(ctx)?)),
        "safearraygetelement" => Ok(Some(handle_safe_array_get_element(ctx)?)),
        "safearrayputelement" => Ok(Some(handle_safe_array_put_element(ctx)?)),
        "safearraygetlbound" => Ok(Some(handle_safe_array_get_lbound(ctx)?)),
        "safearraygetubound" => Ok(Some(handle_safe_array_get_ubound(ctx)?)),
        "safearraygetdim" => Ok(Some(handle_safe_array_get_dim(ctx)?)),
        // IDispatch stubs
        "dispgetidsofnames" => Ok(Some(handle_disp_get_ids_of_names(ctx)?)),
        "dispinvoke" => Ok(Some(handle_disp_invoke(ctx)?)),
        _ => Ok(None),
    }
}

/// BSTR layout: 4-byte length prefix (byte count), then UTF-16 data + 2-byte NUL.
fn alloc_bstr(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    units: &[u16],
) -> Result<u64> {
    let byte_len = u32::try_from(units.len().saturating_mul(2)).unwrap_or(0);
    // header(4) + data + NUL(2) + slop
    let total = 4_u64
        .saturating_add(u64::from(byte_len))
        .saturating_add(2)
        .saturating_add(8);
    let raw = state
        .heap_state
        .heap
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .alloc_coherent(engine, total);
    if raw == 0 {
        return Ok(0);
    }
    let data = raw.wrapping_add(4);
    engine.mem_write(data.wrapping_sub(4), &byte_len.to_le_bytes())?;
    let mut bytes = Vec::with_capacity(units.len().saturating_mul(2).saturating_add(2));
    for u in units {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    engine.mem_write(data, &bytes)?;
    Ok(data)
}

/// `BSTR SysAllocString(const OLECHAR*)`.
fn handle_sys_alloc_string(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let src = engine.read_rcx()?;
    if src == 0 {
        return ctx.finish(0);
    }
    let mut units = Vec::new();
    let mut i = 0_u64;
    loop {
        let mut b = [0_u8; 2];
        engine.mem_read(src.wrapping_add(i.wrapping_mul(2)), &mut b)?;
        let w = u16::from_le_bytes(b);
        if w == 0 {
            break;
        }
        units.push(w);
        i = i.saturating_add(1);
        if i > 1_000_000 {
            break;
        }
    }
    let bstr = alloc_bstr(engine, state, &units)?;
    ctx.finish(bstr)
}

/// `BSTR SysAllocStringLen(const OLECHAR*, UINT)`.
fn handle_sys_alloc_string_len(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let src = engine.read_rcx()?;
    let len = engine.read_rdx()? & 0xffff_ffff;
    let n = usize::try_from(len).unwrap_or(0);
    let mut units = vec![0_u16; n];
    if src != 0 && n > 0 {
        for (i, u) in units.iter_mut().enumerate() {
            let mut b = [0_u8; 2];
            let off = u64::try_from(i).unwrap_or(0).wrapping_mul(2);
            engine.mem_read(src.wrapping_add(off), &mut b)?;
            *u = u16::from_le_bytes(b);
        }
    }
    let bstr = alloc_bstr(engine, state, &units)?;
    ctx.finish(bstr)
}

/// `void SysFreeString(BSTR)`.
fn handle_sys_free_string(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let bstr = engine.read_rcx()?;
    if bstr != 0 {
        // Free the allocation that includes the 4-byte length prefix.
        let _ = state
            .heap_state
            .heap
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .free_coherent(engine, bstr.wrapping_sub(4));
    }
    ctx.finish(0)
}

/// `UINT SysStringLen(BSTR)`.
fn handle_sys_string_len(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let bstr = engine.read_rcx()?;
    if bstr == 0 {
        return ctx.finish(0);
    }
    let mut len_bytes = [0_u8; 4];
    engine.mem_read(bstr.wrapping_sub(4), &mut len_bytes)?;
    let byte_len = u32::from_le_bytes(len_bytes);
    ctx.finish(u64::from(byte_len.wrapping_shr(1)))
}

/// `UINT SysStringByteLen(BSTR)`.
fn handle_sys_string_byte_len(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let bstr = engine.read_rcx()?;
    if bstr == 0 {
        return ctx.finish(0);
    }
    let mut len_bytes = [0_u8; 4];
    engine.mem_read(bstr.wrapping_sub(4), &mut len_bytes)?;
    ctx.finish(u64::from(u32::from_le_bytes(len_bytes)))
}

/// `void VariantInit(VARIANTARG*)` — set VT_EMPTY.
fn handle_variant_init(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let pvar = engine.read_rcx()?;
    if pvar != 0 {
        engine.mem_write(pvar, &[0_u8; 24])?;
    }
    ctx.finish(0)
}

/// x64 `VARIANT` / `PROPVARIANT` payload size (vt + reserved + union).
const VARIANT_SIZE: usize = 24;
/// Offset of the union (`bstrVal`, `ulVal`, …) on x64.
const VARIANT_DATA_OFF: u64 = 8;

/// Read the numeric value from a VARIANT at data offset 8 as i64 (sign-extended
/// for integers, bitcast for floats).
fn read_variant_num(engine: &mut dyn wie_cpu::CpuEngine, pvar: u64) -> Result<(u16, i64)> {
    let vt = read_vt(engine, pvar)?;
    let mut raw = [0_u8; 8];
    engine.mem_read(pvar.wrapping_add(VARIANT_DATA_OFF), &mut raw)?;
    let bits = u64::from_le_bytes(raw);
    let val = match vt {
        VT_I2 => i64::from(i16::try_from(bits & 0xffff).unwrap_or(0)),
        VT_I4 | VT_BOOL => i64::from(i32::try_from(bits & 0xffff_ffff).unwrap_or(0)),
        VT_UI4 => i64::from(u32::try_from(bits & 0xffff_ffff).unwrap_or(0)),
        VT_I8 => i64::from_le_bytes(raw),
        VT_R4 => {
            let f =
                f32::from_le_bytes(u32::try_from(bits & 0xffff_ffff).unwrap_or(0).to_le_bytes());
            f as i64
        }
        VT_R8 | VT_DATE => {
            let d = f64::from_le_bytes(raw);
            d as i64
        }
        _ => 0,
    };
    Ok((vt, val))
}

/// Write a numeric value back into a VARIANT, setting the appropriate vt.
fn write_variant_num(
    engine: &mut dyn wie_cpu::CpuEngine,
    pvar: u64,
    vt: u16,
    val: i64,
) -> Result<()> {
    engine.mem_write(pvar, &vt.to_le_bytes())?;
    engine.mem_write(pvar.wrapping_add(2), &[0_u8; 6])?;
    let bits = match vt {
        VT_I2 => (val as i16) as u64,
        VT_I4 | VT_BOOL => (val as i32) as u64,
        VT_UI4 => val as u32 as u64,
        VT_R4 => (val as f32).to_bits() as u64,
        VT_R8 | VT_DATE => (val as f64).to_bits(),
        _ => val as u64,
    };
    engine.mem_write(pvar.wrapping_add(VARIANT_DATA_OFF), &bits.to_le_bytes())?;
    // Zero the reserved 8 bytes at +16 (VT_DECIMAL is not used).
    engine.mem_write(pvar.wrapping_add(16), &[0_u8; 8])?;
    Ok(())
}

fn read_vt(engine: &mut dyn wie_cpu::CpuEngine, pvar: u64) -> Result<u16> {
    let mut vt_bytes = [0_u8; 2];
    engine.mem_read(pvar, &mut vt_bytes)?;
    Ok(u16::from_le_bytes(vt_bytes))
}

fn read_bstr_field(engine: &mut dyn wie_cpu::CpuEngine, pvar: u64) -> Result<u64> {
    let mut b = [0_u8; 8];
    engine.mem_read(pvar.wrapping_add(VARIANT_DATA_OFF), &mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn free_bstr_if_any(engine: &mut dyn wie_cpu::CpuEngine, state: &mut WinApiState, bstr: u64) {
    if bstr != 0 {
        let _ = state
            .heap_state
            .heap
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .free_coherent(engine, bstr.wrapping_sub(4));
    }
}

/// Clear a guest `VARIANT` in place (shared by `VariantClear` and `VariantCopy`).
fn variant_clear_at(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    pvar: u64,
) -> Result<()> {
    if pvar == 0 {
        return Ok(());
    }
    let vt = read_vt(engine, pvar)?;
    if vt == VT_BSTR {
        let bstr = read_bstr_field(engine, pvar)?;
        free_bstr_if_any(engine, state, bstr);
    }
    engine.mem_write(pvar, &[0_u8; VARIANT_SIZE])?;
    Ok(())
}

/// Deep-copy a BSTR (length prefix + UTF-16 payload), or 0 for null source.
fn dup_bstr(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    src_bstr: u64,
) -> Result<u64> {
    if src_bstr == 0 {
        return Ok(0);
    }
    let mut len_bytes = [0_u8; 4];
    engine.mem_read(src_bstr.wrapping_sub(4), &mut len_bytes)?;
    let byte_len = u32::from_le_bytes(len_bytes);
    let n_units = usize::try_from(byte_len.wrapping_shr(1)).unwrap_or(0);
    let mut units = vec![0_u16; n_units];
    for (i, u) in units.iter_mut().enumerate() {
        let mut b = [0_u8; 2];
        let off = u64::try_from(i).unwrap_or(0).wrapping_mul(2);
        engine.mem_read(src_bstr.wrapping_add(off), &mut b)?;
        *u = u16::from_le_bytes(b);
    }
    alloc_bstr(engine, state, &units)
}

/// `HRESULT VariantClear(VARIANTARG*)` — free owned resources and set `VT_EMPTY`.
fn handle_variant_clear(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let pvar = engine.read_rcx()?;
    variant_clear_at(engine, state, pvar)?;
    ctx.finish(0) // S_OK
}

/// `HRESULT VariantCopy(VARIANTARG* dest, const VARIANTARG* src)`.
///
/// Must **deep-copy** `VT_BSTR` (and clear `dest` first). Shallow memcpy is wrong:
/// 7-Zip `CPropVariant::InternalCopy` relies on this for method property values.
fn handle_variant_copy(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let dest = engine.read_rcx()?;
    let src = engine.read_rdx()?;
    if dest == 0 || src == 0 {
        // Real OLEAUT32 returns `E_INVALIDARG` for null pointers.
        return ctx.finish(E_INVALIDARG);
    }
    if dest == src {
        return ctx.finish(0);
    }

    let src_vt = read_vt(engine, src)?;

    // Free any resources currently owned by dest.
    variant_clear_at(engine, state, dest)?;

    if src_vt == VT_BSTR {
        let src_bstr = read_bstr_field(engine, src)?;
        let new_bstr = dup_bstr(engine, state, src_bstr)?;
        if src_bstr != 0 && new_bstr == 0 {
            // Out of memory.
            return ctx.finish(E_OUTOFMEMORY);
        }
        // vt = VT_BSTR, reserved zeros, bstrVal = new_bstr
        engine.mem_write(dest, &VT_BSTR.to_le_bytes())?;
        engine.mem_write(dest.wrapping_add(2), &[0_u8; 6])?;
        engine.mem_write(dest.wrapping_add(VARIANT_DATA_OFF), &new_bstr.to_le_bytes())?;
        // Zero high padding of the 24-byte VARIANT if any remainder exists.
        // Data field is 8 bytes at +8; total 16 used + 8 pad already covered by clear.
        return ctx.finish(0);
    }

    // Simple / non-owning types: bitwise copy of the 24-byte x64 VARIANT.
    // (VT_EMPTY, integers, bool, R8, FILETIME-as-i64, etc.)
    let mut buf = [0_u8; VARIANT_SIZE];
    engine.mem_read(src, &mut buf)?;
    engine.mem_write(dest, &buf)?;
    ctx.finish(0)
}

// ── Variant arithmetic ─────────────────────────────────────────────────────

/// Shared handler for VarAdd / VarSub / VarMul / VarDiv / VarMod.
///
/// Win64: RCX = result VARIANT*, RDX = lhs VARIANT*, R8 = rhs VARIANT*.
fn handle_var_math(ctx: &mut HandlerContext<'_>, op: &str) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let presult = engine.read_rcx()?;
    let plhs = engine.read_rdx()?;
    let prhs = engine.read_r8()?;
    if presult == 0 || plhs == 0 || prhs == 0 {
        return ctx.finish(E_INVALIDARG);
    }
    let (lvt, lhs_val) = read_variant_num(engine, plhs)?;
    let (rvt, rhs_val) = read_variant_num(engine, prhs)?;
    // Promote to the widest type present.
    let out_vt = std::cmp::max(lvt, rvt);
    let out_vt = if out_vt < VT_I2 { VT_I4 } else { out_vt };
    let result = match op {
        "varadd" | "add" => lhs_val.wrapping_add(rhs_val),
        "varsub" | "sub" => lhs_val.wrapping_sub(rhs_val),
        "varmul" | "mul" => lhs_val.wrapping_mul(rhs_val),
        "vardiv" | "div" => {
            if rhs_val == 0 {
                // Clear result to VT_EMPTY and return error
                engine.mem_write(presult, &[0_u8; VARIANT_SIZE])?;
                return ctx.finish(DISP_E_DIVBYZERO);
            }
            lhs_val / rhs_val
        }
        "varmod" | "mod" => {
            if rhs_val == 0 {
                engine.mem_write(presult, &[0_u8; VARIANT_SIZE])?;
                return ctx.finish(DISP_E_DIVBYZERO);
            }
            lhs_val % rhs_val
        }
        _ => 0,
    };
    write_variant_num(engine, presult, out_vt, result)?;
    ctx.finish(0) // S_OK
}

// ── Variant type conversions: BSTR ← number ────────────────────────────────

/// Converts a numeric VARIANT to a BSTR VARIANT (VarBstrFromI4, etc.).
///
/// Win64: RCX = result VARIANT*, RDX = input number (as VT argument variant).
fn handle_var_bstr_from_num(
    ctx: &mut HandlerContext<'_>,
    src_vt: u16,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let presult = engine.read_rcx()?;
    let raw_val = engine.read_rdx()?;
    if presult == 0 {
        return ctx.finish(E_INVALIDARG);
    }
    let s = match src_vt {
        VT_I4 => format!("{}", raw_val as i32),
        VT_R4 => format!(
            "{}",
            f32::from_bits(u32::try_from(raw_val & 0xffff_ffff).unwrap_or(0))
        ),
        VT_R8 | VT_DATE => format!("{}", f64::from_bits(raw_val)),
        _ => return ctx.finish(DISP_E_MEMBERNOTFOUND),
    };
    let units: Vec<u16> = s.encode_utf16().collect();
    let bstr = alloc_bstr(engine, state, &units)?;
    engine.mem_write(presult, &VT_BSTR.to_le_bytes())?;
    engine.mem_write(presult.wrapping_add(2), &[0_u8; 6])?;
    engine.mem_write(presult.wrapping_add(VARIANT_DATA_OFF), &bstr.to_le_bytes())?;
    engine.mem_write(presult.wrapping_add(16), &[0_u8; 8])?;
    ctx.finish(0) // S_OK
}

// ── Variant type conversions: number ← BSTR ────────────────────────────────

/// Parses a BSTR VARIANT into a numeric VARIANT (VarI4FromBstr, etc.).
///
/// Win64: RCX = result VARIANT*, RDX = source VARIANT*.
fn handle_var_num_from_bstr(
    ctx: &mut HandlerContext<'_>,
    out_vt: u16,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let presult = engine.read_rcx()?;
    let psrc = engine.read_rdx()?;
    if presult == 0 || psrc == 0 {
        return ctx.finish(E_INVALIDARG);
    }
    let vt = read_vt(engine, psrc)?;
    if vt != VT_BSTR {
        // Try to read a numeric source and convert directly.
        let (_svt, val) = read_variant_num(engine, psrc)?;
        write_variant_num(engine, presult, out_vt, val)?;
        return ctx.finish(0);
    }
    let bstr = read_bstr_field(engine, psrc)?;
    if bstr == 0 {
        write_variant_num(engine, presult, out_vt, 0)?;
        return ctx.finish(0);
    }
    let mut len_bytes = [0_u8; 4];
    engine.mem_read(bstr.wrapping_sub(4), &mut len_bytes)?;
    let byte_len = u32::from_le_bytes(len_bytes);
    let n_units = usize::try_from(byte_len.wrapping_shr(1)).unwrap_or(0);
    let mut units = vec![0_u16; n_units];
    for (i, u) in units.iter_mut().enumerate() {
        let mut b = [0_u8; 2];
        engine.mem_read(
            bstr.wrapping_add(u64::try_from(i).unwrap_or(0).wrapping_mul(2)),
            &mut b,
        )?;
        *u = u16::from_le_bytes(b);
    }
    let s = String::from_utf16_lossy(&units);
    let val: i64 = match out_vt {
        VT_I4 | VT_UI4 => s.trim().parse::<i64>().unwrap_or(0),
        VT_R4 => s.trim().parse::<f32>().unwrap_or(0.0) as i64,
        VT_R8 | VT_DATE => s.trim().parse::<f64>().unwrap_or(0.0) as i64,
        _ => 0,
    };
    write_variant_num(engine, presult, out_vt, val)?;
    ctx.finish(0)
}

// ── Variant type conversions: num ← num ────────────────────────────────────

/// Converts between numeric VARIANT types (VarR8FromI4, etc.).
///
/// Win64: RCX = result VARIANT*, RDX = source VARIANT*.
fn handle_var_num_from_num(
    ctx: &mut HandlerContext<'_>,
    out_vt: u16,
    _in_vt: u16,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let presult = engine.read_rcx()?;
    let psrc = engine.read_rdx()?;
    if presult == 0 || psrc == 0 {
        return ctx.finish(E_INVALIDARG);
    }
    let (_svt, val) = read_variant_num(engine, psrc)?;
    write_variant_num(engine, presult, out_vt, val)?;
    ctx.finish(0)
}

// ── SafeArray family ───────────────────────────────────────────────────────

/// One live SafeArray. The element buffer lives in **guest** memory (a process
/// heap allocation) so `SafeArrayAccessData` can hand the guest a real
/// writable VA — the guest reads/writes elements in place.
struct SafeArrayData {
    /// Dimensions in order: `(cElements, lLbound)`.
    dims: Vec<(u32, u32)>,
    /// Guest VA of the element buffer.
    data_va: u64,
    /// Bytes per element.
    element_size: u64,
}

/// Live SafeArrays keyed by fake `SAFEARRAY*` handle. Static (not per-session)
/// because the shared state file owns the DllId table — same pattern as the
/// UCRT `strtok` static save slot.
static SAFE_ARRAYS: Mutex<Vec<(u64, SafeArrayData)>> = std::sync::Mutex::new(Vec::new());

/// Next fake `SAFEARRAY*` handle (counter from `0x5300_0000`).
fn next_sa_handle() -> u64 {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0x5300_0000);
    COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn sa_table() -> std::sync::MutexGuard<'static, Vec<(u64, SafeArrayData)>> {
    SAFE_ARRAYS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Zero `len` bytes of guest memory (fresh heap bump may hold stale freelist
/// data; SafeArrayCreate must present a zeroed buffer like the real one).
fn zero_guest(engine: &mut dyn wie_cpu::CpuEngine, va: u64, len: u64) -> Result<()> {
    let mut remaining = len;
    let mut cursor = va;
    let chunk = vec![0_u8; 4096];
    while remaining > 0 {
        let n = remaining.min(4096);
        let n_usize = usize::try_from(n).unwrap_or(0);
        let slice = chunk.get(..n_usize).context("zero chunk bounds")?;
        engine.mem_write(cursor, slice)?;
        cursor = cursor.wrapping_add(n);
        remaining = remaining.wrapping_sub(n);
    }
    Ok(())
}

/// Flat byte offset for `rgIndices`, or `None` if any index is out of bounds.
///
/// Row-major with the FIRST dimension fastest: offset =
/// Σ (idx[i] - lbound[i]) × stride[i], stride[i] = Π count[j] for j > i.
fn flat_offset(
    engine: &mut dyn wie_cpu::CpuEngine,
    data: &SafeArrayData,
    rg_indices: u64,
) -> Result<Option<u64>> {
    let mut offset = 0_u64;
    let mut stride = 1_u64;
    for (i, (count, lbound)) in data.dims.iter().enumerate().rev() {
        let mut idx_bytes = [0_u8; 4];
        let off = u64::try_from(i).unwrap_or(0).wrapping_mul(4);
        engine.mem_read(rg_indices.wrapping_add(off), &mut idx_bytes)?;
        let idx = u32::from_le_bytes(idx_bytes);
        if idx < *lbound || idx >= lbound.wrapping_add(*count) {
            return Ok(None);
        }
        let rel = u64::from(idx.wrapping_sub(*lbound));
        offset = offset.wrapping_add(rel.wrapping_mul(stride));
        stride = stride.wrapping_mul(u64::from(*count));
    }
    Ok(Some(offset.wrapping_mul(data.element_size)))
}

/// `SAFEARRAY *SafeArrayCreate(UINT cDims, SAFEARRAYBOUND *rgBounds, ULONG cbElements)`
fn handle_safe_array_create(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c_dims = engine.read_rcx()? & 0xffff_ffff;
    let rg_bounds = engine.read_rdx()?;
    let cb_elements = engine.read_r8()? & 0xffff_ffff;
    let n_dims = usize::try_from(c_dims).unwrap_or(0);
    if n_dims == 0 || n_dims > 32 || rg_bounds == 0 {
        return ctx.finish(0);
    }
    let mut dims = Vec::with_capacity(n_dims);
    let mut total_bytes = 1_u64;
    for i in 0..n_dims {
        let mut c_bytes = [0_u8; 4];
        let mut l_bytes = [0_u8; 4];
        let entry_va = rg_bounds.wrapping_add(u64::try_from(i).unwrap_or(0).wrapping_mul(8));
        engine.mem_read(entry_va, &mut c_bytes)?;
        engine.mem_read(entry_va.wrapping_add(4), &mut l_bytes)?;
        let count = u32::from_le_bytes(c_bytes);
        let lbound = u32::from_le_bytes(l_bytes);
        dims.push((count, lbound));
        total_bytes = total_bytes.saturating_mul(u64::from(count));
    }
    total_bytes = total_bytes.saturating_mul(cb_elements);
    let data_va = ctx
        .heap
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .alloc_coherent(engine, total_bytes);
    if data_va == 0 {
        return ctx.finish(0); // OOM — SafeArrayCreate returns NULL
    }
    zero_guest(engine, data_va, total_bytes)?;
    let handle = next_sa_handle();
    sa_table().push((
        handle,
        SafeArrayData {
            dims,
            data_va,
            element_size: cb_elements,
        },
    ));
    ctx.finish(handle)
}

/// `HRESULT SafeArrayDestroy(SAFEARRAY *psa)`
fn handle_safe_array_destroy(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let psa = engine.read_rcx()?;
    let mut table = sa_table();
    let idx = table.iter().position(|(h, _)| *h == psa);
    if let Some(i) = idx {
        let (_, data) = table.swap_remove(i);
        if data.data_va != 0 {
            let _ = ctx
                .heap
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .free_coherent(engine, data.data_va);
        }
        ctx.finish(0) // S_OK
    } else {
        ctx.finish(E_INVALIDARG)
    }
}

/// `HRESULT SafeArrayAccessData(SAFEARRAY *psa, void **ppvData)`
fn handle_safe_array_access_data(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let psa = engine.read_rcx()?;
    let ppv = engine.read_rdx()?;
    let data_va = sa_table()
        .iter()
        .find(|(h, _)| *h == psa)
        .map(|(_, d)| d.data_va)
        .unwrap_or(0);
    if data_va != 0 {
        if ppv != 0 {
            engine.mem_write(ppv, &data_va.to_le_bytes())?;
        }
        ctx.finish(0) // S_OK
    } else {
        ctx.finish(E_INVALIDARG)
    }
}

/// `HRESULT SafeArrayUnaccessData(SAFEARRAY *psa)` — the guest wrote in place.
fn handle_safe_array_unaccess_data(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let psa = engine.read_rcx()?;
    let found = sa_table().iter().any(|(h, _)| *h == psa);
    ctx.finish(if found { 0 } else { E_INVALIDARG })
}

/// `HRESULT SafeArrayGetElement(SAFEARRAY *psa, LONG *rgIndices, void *pvOut)`
fn handle_safe_array_get_element(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let psa = engine.read_rcx()?;
    let rg_indices = engine.read_rdx()?;
    let pv_out = engine.read_r8()?;
    let table = sa_table();
    let Some(data) = table.iter().find(|(h, _)| *h == psa).map(|(_, d)| d) else {
        return ctx.finish(E_INVALIDARG);
    };
    let Some(off) = flat_offset(engine, data, rg_indices)? else {
        return ctx.finish(E_INVALIDARG);
    };
    let n = usize::try_from(data.element_size).unwrap_or(0);
    let mut buf = vec![0_u8; n];
    engine.mem_read(data.data_va.wrapping_add(off), &mut buf)?;
    if pv_out != 0 {
        engine.mem_write(pv_out, &buf)?;
    }
    ctx.finish(0) // S_OK
}

/// `HRESULT SafeArrayPutElement(SAFEARRAY *psa, LONG *rgIndices, void *pvIn)`
fn handle_safe_array_put_element(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let psa = engine.read_rcx()?;
    let rg_indices = engine.read_rdx()?;
    let pv_in = engine.read_r8()?;
    let table = sa_table();
    let Some(data) = table.iter().find(|(h, _)| *h == psa).map(|(_, d)| d) else {
        return ctx.finish(E_INVALIDARG);
    };
    let Some(off) = flat_offset(engine, data, rg_indices)? else {
        return ctx.finish(E_INVALIDARG);
    };
    let n = usize::try_from(data.element_size).unwrap_or(0);
    let mut buf = vec![0_u8; n];
    if pv_in != 0 {
        engine.mem_read(pv_in, &mut buf)?;
    }
    engine.mem_write(data.data_va.wrapping_add(off), &buf)?;
    ctx.finish(0) // S_OK
}

/// `HRESULT SafeArrayGetLBound(SAFEARRAY *psa, UINT nDim, LONG *plLbound)`
fn handle_safe_array_get_lbound(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let psa = engine.read_rcx()?;
    let n_dim = engine.read_rdx()?;
    let pl = engine.read_r8()?;
    let table = sa_table();
    let Some(data) = table.iter().find(|(h, _)| *h == psa).map(|(_, d)| d) else {
        return ctx.finish(E_INVALIDARG);
    };
    let dim_idx = usize::try_from(n_dim.wrapping_sub(1)).unwrap_or(0);
    let Some((_, lbound)) = data.dims.get(dim_idx).copied() else {
        return ctx.finish(E_INVALIDARG);
    };
    if pl != 0 {
        engine.mem_write(pl, &lbound.to_le_bytes())?;
    }
    ctx.finish(0) // S_OK
}

/// `HRESULT SafeArrayGetUBound(SAFEARRAY *psa, UINT nDim, LONG *plUbound)`
fn handle_safe_array_get_ubound(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let psa = engine.read_rcx()?;
    let n_dim = engine.read_rdx()?;
    let pl = engine.read_r8()?;
    let table = sa_table();
    let Some(data) = table.iter().find(|(h, _)| *h == psa).map(|(_, d)| d) else {
        return ctx.finish(E_INVALIDARG);
    };
    let dim_idx = usize::try_from(n_dim.wrapping_sub(1)).unwrap_or(0);
    let Some((count, lbound)) = data.dims.get(dim_idx).copied() else {
        return ctx.finish(E_INVALIDARG);
    };
    let ubound = lbound.wrapping_add(count).wrapping_sub(1);
    if pl != 0 {
        engine.mem_write(pl, &ubound.to_le_bytes())?;
    }
    ctx.finish(0) // S_OK
}

/// `UINT SafeArrayGetDim(SAFEARRAY *psa)`
fn handle_safe_array_get_dim(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let psa = engine.read_rcx()?;
    let dims = sa_table()
        .iter()
        .find(|(h, _)| *h == psa)
        .map(|(_, d)| d.dims.len())
        .unwrap_or(0);
    ctx.finish(u64::try_from(dims).unwrap_or(0))
}

// ── IDispatch stubs ────────────────────────────────────────────────────────

/// `HRESULT DispGetIDsOfNames(riid, rgszNames, cNames, lcid, rgDispId)`
///
/// KISS: every name is unknown — write `DISPID_UNKNOWN` (-1) to each slot and
/// return `DISP_E_UNKNOWNNAME`; guests fall back to `DISPID_UNKNOWN` anyway.
fn handle_disp_get_ids_of_names(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _riid = engine.read_rcx()?;
    let _names = engine.read_rdx()?;
    let c_names = (engine.read_r8()? & 0xffff_ffff).min(4096);
    let _lcid = engine.read_r9()?;
    let rsp = engine.read_rsp()?;
    let mut slot = [0_u8; 8];
    engine.mem_read(rsp.wrapping_add(0x28), &mut slot)?;
    let rg_disp_id = u64::from_le_bytes(slot);
    let unknown = 0xffff_ffff_u32; // DISPID_UNKNOWN = (LONG)-1
    for i in 0..c_names {
        let off = i.wrapping_mul(4);
        if rg_disp_id != 0 {
            engine.mem_write(rg_disp_id.wrapping_add(off), &unknown.to_le_bytes())?;
        }
    }
    ctx.finish(0x8002_0006) // DISP_E_UNKNOWNNAME
}

/// `HRESULT DispInvoke(...)` — KISS: `E_NOTIMPL` (args accepted, unused).
fn handle_disp_invoke(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ctx.finish(0x8000_4001) // E_NOTIMPL
}
