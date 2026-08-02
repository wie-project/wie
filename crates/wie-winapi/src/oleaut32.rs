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
        _ => Ok(None),
    }
}

fn ret(engine: &mut dyn wie_cpu::CpuEngine, value: u64) -> Result<WinApiHandlerResult> {
    let return_address = engine
        .return_from_win64_api(value)
        .context("OLEAUT32 return")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: value,
    })
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
    let raw = state.heap_state.heap.alloc_coherent(engine, total);
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
        return ret(engine, 0);
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
    ret(engine, bstr)
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
    ret(engine, bstr)
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
            .free_coherent(engine, bstr.wrapping_sub(4));
    }
    ret(engine, 0)
}

/// `UINT SysStringLen(BSTR)`.
fn handle_sys_string_len(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let bstr = engine.read_rcx()?;
    if bstr == 0 {
        return ret(engine, 0);
    }
    let mut len_bytes = [0_u8; 4];
    engine.mem_read(bstr.wrapping_sub(4), &mut len_bytes)?;
    let byte_len = u32::from_le_bytes(len_bytes);
    ret(engine, u64::from(byte_len.wrapping_shr(1)))
}

/// `UINT SysStringByteLen(BSTR)`.
fn handle_sys_string_byte_len(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let bstr = engine.read_rcx()?;
    if bstr == 0 {
        return ret(engine, 0);
    }
    let mut len_bytes = [0_u8; 4];
    engine.mem_read(bstr.wrapping_sub(4), &mut len_bytes)?;
    ret(engine, u64::from(u32::from_le_bytes(len_bytes)))
}

/// `void VariantInit(VARIANTARG*)` — set VT_EMPTY.
fn handle_variant_init(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let pvar = engine.read_rcx()?;
    if pvar != 0 {
        engine.mem_write(pvar, &[0_u8; 24])?;
    }
    ret(engine, 0)
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
    ret(engine, 0) // S_OK
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
        return ret(engine, 0x8007_0057);
    }
    if dest == src {
        return ret(engine, 0);
    }

    let src_vt = read_vt(engine, src)?;

    // Free any resources currently owned by dest.
    variant_clear_at(engine, state, dest)?;

    if src_vt == VT_BSTR {
        let src_bstr = read_bstr_field(engine, src)?;
        let new_bstr = dup_bstr(engine, state, src_bstr)?;
        if src_bstr != 0 && new_bstr == 0 {
            // Out of memory.
            return ret(engine, 0x8007_000E);
        }
        // vt = VT_BSTR, reserved zeros, bstrVal = new_bstr
        engine.mem_write(dest, &VT_BSTR.to_le_bytes())?;
        engine.mem_write(dest.wrapping_add(2), &[0_u8; 6])?;
        engine.mem_write(dest.wrapping_add(VARIANT_DATA_OFF), &new_bstr.to_le_bytes())?;
        // Zero high padding of the 24-byte VARIANT if any remainder exists.
        // Data field is 8 bytes at +8; total 16 used + 8 pad already covered by clear.
        return ret(engine, 0);
    }

    // Simple / non-owning types: bitwise copy of the 24-byte x64 VARIANT.
    // (VT_EMPTY, integers, bool, R8, FILETIME-as-i64, etc.)
    let mut buf = [0_u8; VARIANT_SIZE];
    engine.mem_read(src, &mut buf)?;
    engine.mem_write(dest, &buf)?;
    ret(engine, 0)
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
        return ret(engine, 0x8007_0057); // E_INVALIDARG
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
                return ret(engine, 0x8002_0011); // DISP_E_DIVBYZERO
            }
            lhs_val / rhs_val
        }
        "varmod" | "mod" => {
            if rhs_val == 0 {
                engine.mem_write(presult, &[0_u8; VARIANT_SIZE])?;
                return ret(engine, 0x8002_0011);
            }
            lhs_val % rhs_val
        }
        _ => 0,
    };
    write_variant_num(engine, presult, out_vt, result)?;
    ret(engine, 0) // S_OK
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
        return ret(engine, 0x8007_0057);
    }
    let s = match src_vt {
        VT_I4 => format!("{}", raw_val as i32),
        VT_R4 => format!(
            "{}",
            f32::from_bits(u32::try_from(raw_val & 0xffff_ffff).unwrap_or(0))
        ),
        VT_R8 | VT_DATE => format!("{}", f64::from_bits(raw_val)),
        _ => return ret(engine, 0x8002_0003), // E_INVALIDARG
    };
    let units: Vec<u16> = s.encode_utf16().collect();
    let bstr = alloc_bstr(engine, state, &units)?;
    engine.mem_write(presult, &VT_BSTR.to_le_bytes())?;
    engine.mem_write(presult.wrapping_add(2), &[0_u8; 6])?;
    engine.mem_write(presult.wrapping_add(VARIANT_DATA_OFF), &bstr.to_le_bytes())?;
    engine.mem_write(presult.wrapping_add(16), &[0_u8; 8])?;
    ret(engine, 0) // S_OK
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
        return ret(engine, 0x8007_0057);
    }
    let vt = read_vt(engine, psrc)?;
    if vt != VT_BSTR {
        // Try to read a numeric source and convert directly.
        let (_svt, val) = read_variant_num(engine, psrc)?;
        write_variant_num(engine, presult, out_vt, val)?;
        return ret(engine, 0);
    }
    let bstr = read_bstr_field(engine, psrc)?;
    if bstr == 0 {
        write_variant_num(engine, presult, out_vt, 0)?;
        return ret(engine, 0);
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
    ret(engine, 0)
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
        return ret(engine, 0x8007_0057);
    }
    let (_svt, val) = read_variant_num(engine, psrc)?;
    write_variant_num(engine, presult, out_vt, val)?;
    ret(engine, 0)
}
