//! Oleaut32 tests: VarAdd and VarBstrFromI4.
use super::*;

// ── OLEAUT32 ──────────────────────────────────────────────────────

#[test]
fn test_var_add() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let presult = 0x3000;
    let plhs = 0x4000;
    let prhs = 0x5000;
    // lhs = VT_I4, value = 10
    engine.mem_write(plhs, &(3_u16).to_le_bytes()).ok(); // VT_I4
    engine
        .mem_write(plhs.wrapping_add(8), &10_u64.to_le_bytes())
        .ok();
    // rhs = VT_I4, value = 20
    engine.mem_write(prhs, &(3_u16).to_le_bytes()).ok();
    engine
        .mem_write(prhs.wrapping_add(8), &20_u64.to_le_bytes())
        .ok();
    write_regs(&mut engine, presult, plhs, prhs, 0, STACK_TOP);
    let r = {
        let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
        oleaut32::dispatch_oleaut32(&mut ctx, "VarAdd")
    }
    .expect("dispatch")
    .expect("handled");
    assert_eq!(r.return_value, 0); // S_OK
    let mut result_vt = [0_u8; 2];
    engine.mem_read(presult, &mut result_vt).ok();
    assert_eq!(u16::from_le_bytes(result_vt), 3); // VT_I4
    let mut result_val = [0_u8; 8];
    engine
        .mem_read(presult.wrapping_add(8), &mut result_val)
        .ok();
    assert_eq!(i64::from_le_bytes(result_val), 30);
}

#[test]
fn test_var_bstr_from_i4() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let presult = 0x3000;
    // VarBstrFromI4(42, 0, 0, &result)
    write_regs(&mut engine, presult, 42, 0, 0, STACK_TOP);
    let r = {
        let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
        oleaut32::dispatch_oleaut32(&mut ctx, "VarBstrFromI4")
    }
    .expect("dispatch")
    .expect("handled");
    assert_eq!(r.return_value, 0); // S_OK
    let mut vt = [0_u8; 2];
    engine.mem_read(presult, &mut vt).ok();
    assert_eq!(u16::from_le_bytes(vt), 8); // VT_BSTR
}
