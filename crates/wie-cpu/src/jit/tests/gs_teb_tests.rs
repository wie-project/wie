use super::*;
use crate::IcedCpu;

// --- Per-thread TEB: GS-relative accesses resolve per engine ---

const GS_TEB_CODE: u64 = 0x2001_0000;
/// Worker TEB page (page-aligned, disjoint from the primary [`crate::GS_BASE`]).
const GS_TEB_WORKER: u64 = 0x0000_7000_0040_C000;
/// Seed value in the primary TEB last-error slot.
const GS_PRIMARY_ERR: u32 = 0x1111_1111;
/// Seed value in the worker TEB last-error slot.
const GS_WORKER_ERR: u32 = 0x2222_2222;
/// Value a store test writes through `mov [gs:0x68], ecx`.
const GS_NEW_ERR: u32 = 0x3333_3333;

/// Map the code page + primary/worker TEB pages and seed distinct
/// last-error values into each TEB's `TEB_LAST_ERROR_OFFSET` slot.
fn gs_teb_setup(engine: &mut dyn CpuEngine) {
    map_code_page(engine, GS_TEB_CODE);
    engine
        .virtual_alloc(
            crate::GS_BASE,
            0x1000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_READWRITE,
        )
        .expect("primary TEB page");
    engine
        .virtual_alloc(
            GS_TEB_WORKER,
            0x1000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_READWRITE,
        )
        .expect("worker TEB page");
    let off = crate::guest_layout::TEB_LAST_ERROR_OFFSET;
    engine
        .mem_write(crate::GS_BASE + off, &GS_PRIMARY_ERR.to_le_bytes())
        .expect("primary last-error");
    engine
        .mem_write(GS_TEB_WORKER + off, &GS_WORKER_ERR.to_le_bytes())
        .expect("worker last-error");
}

/// Run `code` (trailing `nop; ud2` appended) on iced and the JIT, both
/// bound to `GS_TEB_WORKER` via `set_gs_base`; returns both engines so
/// callers can inspect registers and guest memory.
fn gs_teb_dual(code: &[u8], setup: impl Fn(&mut RegFile)) -> (IcedCpu, JitCpu) {
    let mut full = Vec::with_capacity(code.len() + 3);
    full.extend_from_slice(code);
    full.extend_from_slice(&[0x90, 0x0f, 0x0b]); // nop filler + ud2 terminator
    let n_insns = decode_count(&full);

    // iced reference, bound to the worker TEB page.
    crate::exec::iced_decode_cache_flush();
    let mut iced = IcedCpu::open_x86_64();
    gs_teb_setup(&mut iced);
    iced.mem_write(GS_TEB_CODE, &full).expect("iced code");
    setup(iced.regs_mut());
    iced.set_gs_base(GS_TEB_WORKER);
    iced.write_rip(GS_TEB_CODE).expect("iced rip");
    for _ in 0..n_insns {
        iced.step_once().expect("iced step");
    }

    // JIT, bound to the worker TEB page.
    let mut cpu = JitCpu::open_x86_64();
    gs_teb_setup(&mut cpu);
    cpu.mem_write(GS_TEB_CODE, &full).expect("jit code");
    setup(&mut cpu.thread.regs);
    cpu.set_gs_base(GS_TEB_WORKER);
    cpu.write_rip(GS_TEB_CODE).expect("jit rip");
    let (result, _retired) = cpu.step_one().expect("jit step");
    assert!(
        matches!(result, StepResult::Continue),
        "jit result {result:?}"
    );
    assert!(
        cpu.has_ready_at(GS_TEB_CODE),
        "block must compile, not run iced"
    );
    assert_eq!(
        cpu.stats().exec.iced_insns,
        0,
        "block ran on iced instead of JIT"
    );

    (iced, cpu)
}

/// `mov eax, [gs:0x68]` reads the BOUND TEB's last-error on both engines.
#[test]
fn gs_relative_load_reads_bound_teb_on_both_engines() {
    // 65 8b 05 <disp32> — mov eax, [gs:0x68].
    let code = [0x65, 0x8b, 0x04, 0x25, 0x68, 0x00, 0x00, 0x00];
    let (iced, jit) = gs_teb_dual(&code, |_| {});
    assert_eq!(
        iced.regs().gpr(0),
        u64::from(GS_WORKER_ERR),
        "iced reads the worker TEB slot"
    );
    assert_eq!(
        jit.thread.regs.gpr(0),
        u64::from(GS_WORKER_ERR),
        "jit reads the worker TEB slot"
    );
}

/// With the default binding (`GS_BASE`) the same access reads the PRIMARY
/// TEB — the primary thread's behavior is preserved.
#[test]
fn gs_relative_load_defaults_to_primary_teb_on_both_engines() {
    let full = [
        0x65, 0x8b, 0x04, 0x25, 0x68, 0x00, 0x00, 0x00, 0x90, 0x0f, 0x0b,
    ];
    let n_insns = decode_count(&full);

    crate::exec::iced_decode_cache_flush();
    let mut iced = IcedCpu::open_x86_64();
    gs_teb_setup(&mut iced);
    iced.mem_write(GS_TEB_CODE, &full).expect("iced code");
    iced.write_rip(GS_TEB_CODE).expect("iced rip");
    for _ in 0..n_insns {
        iced.step_once().expect("iced step");
    }
    assert_eq!(
        iced.regs().gpr(0),
        u64::from(GS_PRIMARY_ERR),
        "iced default binding reads the primary TEB"
    );

    let mut cpu = JitCpu::open_x86_64();
    gs_teb_setup(&mut cpu);
    cpu.mem_write(GS_TEB_CODE, &full).expect("jit code");
    cpu.write_rip(GS_TEB_CODE).expect("jit rip");
    let (result, _retired) = cpu.step_one().expect("jit step");
    assert!(
        matches!(result, StepResult::Continue),
        "jit result {result:?}"
    );
    assert!(cpu.has_ready_at(GS_TEB_CODE), "block must compile");
    assert_eq!(
        cpu.thread.regs.gpr(0),
        u64::from(GS_PRIMARY_ERR),
        "jit default binding reads the primary TEB"
    );
}

/// `mov [gs:0x68], ecx` writes the BOUND TEB's last-error, leaving the
/// primary slot untouched, on both engines.
#[test]
fn gs_relative_store_writes_bound_teb_on_both_engines() {
    // 65 89 0d <disp32> — mov [gs:0x68], ecx.
    let code = [0x65, 0x89, 0x0c, 0x25, 0x68, 0x00, 0x00, 0x00];
    let (iced, jit) = gs_teb_dual(&code, |r| r.set_gpr_public(1, u64::from(GS_NEW_ERR)));
    let off = crate::guest_layout::TEB_LAST_ERROR_OFFSET;

    let mut worker = [0_u8; 4];
    let mut primary = [0_u8; 4];
    iced.guest_mem_arc()
        .read()
        .unwrap()
        .read(GS_TEB_WORKER + off, &mut worker)
        .expect("read iced worker slot");
    iced.guest_mem_arc()
        .read()
        .unwrap()
        .read(crate::GS_BASE + off, &mut primary)
        .expect("read iced primary slot");
    assert_eq!(
        u32::from_le_bytes(worker),
        GS_NEW_ERR,
        "iced store reached the worker TEB slot"
    );
    assert_eq!(
        u32::from_le_bytes(primary),
        GS_PRIMARY_ERR,
        "iced store left the primary TEB slot untouched"
    );

    {
        let mem = jit.shared_jit().mem.read().unwrap();
        mem.read(GS_TEB_WORKER + off, &mut worker)
            .expect("read jit worker slot");
        mem.read(crate::GS_BASE + off, &mut primary)
            .expect("read jit primary slot");
    }
    assert_eq!(
        u32::from_le_bytes(worker),
        GS_NEW_ERR,
        "jit store reached the worker TEB slot"
    );
    assert_eq!(
        u32::from_le_bytes(primary),
        GS_PRIMARY_ERR,
        "jit store left the primary TEB slot untouched"
    );
}

/// The GetLastError micro-stub trampoline resolves the TEB last-error slot
/// against the engine's GS base: one compiled stub serves BOTH the primary
/// binding and a worker binding.
#[test]
fn last_error_stub_trampoline_uses_engine_gs_base() {
    // mov eax, [gs:0x68]; ret — the planted GetLastError stub body
    // (ModRM 04 + SIB 25 forces the base-less disp32 form).
    let get = [0x65, 0x8b, 0x04, 0x25, 0x68, 0x00, 0x00, 0x00, 0xc3];
    let mut cpu = JitCpu::open_x86_64();
    gs_teb_setup(&mut cpu);
    // The stub ends in `ret`, so the guest stack must hold a return address.
    let stack = 0x2002_0000_u64;
    cpu.virtual_alloc(
        stack,
        0x1000,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_READWRITE,
    )
    .expect("stack");
    let ret_slot = stack + 0xff0;
    cpu.mem_write(ret_slot, &0x2002_1000_u64.to_le_bytes())
        .expect("return address");
    cpu.mem_write(GS_TEB_CODE, &get).expect("stub code");

    // Worker binding: the stub reads the worker TEB slot.
    cpu.set_gs_base(GS_TEB_WORKER);
    cpu.write_rsp(ret_slot).expect("rsp");
    cpu.write_rip(GS_TEB_CODE).expect("rip");
    let (result, _retired) = cpu.step_one().expect("step");
    assert!(matches!(result, StepResult::Continue), "result {result:?}");
    assert!(cpu.has_ready_at(GS_TEB_CODE), "stub compiled as a block");
    assert_eq!(
        cpu.stats().exec.iced_insns,
        0,
        "stub must run the hand-written trampoline, not iced"
    );
    assert_eq!(
        cpu.thread.regs.gpr(0),
        u64::from(GS_WORKER_ERR),
        "worker binding reads the worker TEB"
    );

    // Primary binding on the SAME compiled stub: reads the primary slot.
    cpu.set_gs_base(crate::GS_BASE);
    cpu.write_rsp(ret_slot).expect("rsp");
    cpu.write_rip(GS_TEB_CODE).expect("rip");
    let (result, _retired) = cpu.step_one().expect("step");
    assert!(matches!(result, StepResult::Continue), "result {result:?}");
    assert_eq!(
        cpu.thread.regs.gpr(0),
        u64::from(GS_PRIMARY_ERR),
        "primary binding reads the primary TEB"
    );
    assert_eq!(
        cpu.stats().compile.compiles,
        1,
        "one compiled stub served both thread bindings"
    );
}

/// The SetLastError micro-stub trampoline stores into the engine-bound
/// TEB page, never the fixed primary page.
#[test]
fn last_error_store_trampoline_writes_engine_teb() {
    // mov [gs:0x68], ecx; ret — the planted SetLastError stub body.
    let set = [0x65, 0x89, 0x0c, 0x25, 0x68, 0x00, 0x00, 0x00, 0xc3];
    let mut cpu = JitCpu::open_x86_64();
    gs_teb_setup(&mut cpu);
    let stack = 0x2002_0000_u64;
    cpu.virtual_alloc(
        stack,
        0x1000,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_READWRITE,
    )
    .expect("stack");
    let ret_slot = stack + 0xff0;
    cpu.mem_write(ret_slot, &0x2002_1000_u64.to_le_bytes())
        .expect("return address");
    cpu.mem_write(GS_TEB_CODE, &set).expect("stub code");
    cpu.thread.regs.set_gpr_public(1, u64::from(GS_NEW_ERR));
    cpu.set_gs_base(GS_TEB_WORKER);
    cpu.write_rsp(ret_slot).expect("rsp");
    cpu.write_rip(GS_TEB_CODE).expect("rip");
    let (result, _retired) = cpu.step_one().expect("step");
    assert!(matches!(result, StepResult::Continue), "result {result:?}");
    assert_eq!(
        cpu.stats().exec.iced_insns,
        0,
        "stub must run the hand-written trampoline, not iced"
    );

    let off = crate::guest_layout::TEB_LAST_ERROR_OFFSET;
    let mut worker = [0_u8; 4];
    let mut primary = [0_u8; 4];
    {
        let mem = cpu.shared_jit().mem.read().unwrap();
        mem.read(GS_TEB_WORKER + off, &mut worker)
            .expect("read worker slot");
        mem.read(crate::GS_BASE + off, &mut primary)
            .expect("read primary slot");
    }
    assert_eq!(
        u32::from_le_bytes(worker),
        GS_NEW_ERR,
        "store reached the worker TEB slot"
    );
    assert_eq!(
        u32::from_le_bytes(primary),
        GS_PRIMARY_ERR,
        "store left the primary TEB slot untouched"
    );
}
