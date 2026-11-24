//! The hoisted guard, held to the probes it replaces.
//!
//! The hoist replaces a probe with an interval test over a descriptor loaded once
//! per block. That is only a performance change if two things hold, and both are
//! asserted here on **real compiled code** — a Cranelift function built from the
//! same lowering helpers the emitter calls, run against a real `JitCtx`:
//!
//! * the fast path accepts exactly the addresses the probe it replaces accepts —
//!   differentially, against the probe itself rather than a model of it, on every
//!   boundary (`lo - 1`, `lo`, `hi`, `hi + 1`, the wrap at `u64::MAX`, a page
//!   crossing, a dead descriptor);
//! * when it does not accept, control reaches the chain unchanged and the access
//!   is answered or refused *there*.
//!
//! Every test therefore asserts **which edge was taken**, not just the answer: the
//! compiled probe returns a distinct sentinel for "today's chain ran" (`MISS`),
//! for "the generation moved" (`STALE`) and for "the access was done inline"
//! (`DONE`, or the loaded bytes). A fast path taken when it should not have been
//! is a memory-safety bug in the emulator, so these tests are written to fail
//! loudly on it rather than to confirm a hit rate.
use super::super::{GuestVa, HostAddr, MemPin, REFILL_SLOTS, RefillSlot, refill_slot};
use super::*;
use crate::exec::StepResult;
use crate::mem::protect;
use crate::mem::{MEM_COMMIT, MEM_RESERVE};
use crate::{CpuEngine, JitCpu};
use cranelift_codegen::ir::Function;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module, default_libcall_names};
use std::sync::atomic::{AtomicU64, Ordering};

/// "The hoisted guard refused, today's chain answered" — the only sentinel a
/// fallback may produce.
const MISS: u64 = 0xdead_beef_dead_0001;
/// "The generation check failed" — must be reachable only through the stale
/// edge, never through the interval test.
const STALE: u64 = 0xdead_beef_dead_0002;
/// "The access was performed inline."
const DONE: u64 = 0x5eed_0000_0000_0001;
/// Data canary: distinct from every sentinel, so a wrongly-taken fast path
/// cannot pass by reading the sentinel's own value.
const CANARY: u64 = 0x1122_3344_5566_7788;

const DATA_BASE: u64 = 0x6b00_0000;
const DATA_STRIDE: u64 = 0x1_0000;
static NEXT_DATA: AtomicU64 = AtomicU64::new(0);

/// One compiled probe: `(ctx, addr, value) -> u64`.
type ProbeFn = extern "C" fn(*mut JitCtx, u64, u64) -> u64;

/// Which guard to emit for the operand under test.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// Today's chain: `hoisted_pin_probe` → `refill_tlb_probe` → miss.
    Today,
    /// The hoist with the in-frame generation check: span, gen compare, chain.
    Hoisted,
    /// **Exactly the phase-1 wiring** for `call_load`/`call_store`: validate the
    /// span, fall into the chain on a miss, and no generation compare at all —
    /// `home_span_probe`, not `hoisted_stage`. The two tests that name this shape
    /// are the evidence for dropping the compare.
    HoistedPlain,
}

/// Which descriptor the operand's translation comes from, and therefore which
/// stages the chain behind it contains.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// Span from the pin; chain = the pin probe alone, so "the chain refused" is
    /// the pin's own decision.
    Pin,
    /// Span from the pin, chain = pin → refill, i.e. the real chain order. Used to
    /// show that a refused span is answered by the stage *after* the one it was
    /// published from.
    PinThenRefill,
    /// Span from the refill slot; chain = pin → refill.
    Slot,
}

/// Mutates the frame between the prologue and the access, the way a host
/// `VirtualProtect` would: the generation moves and the pin's permission
/// word loses W. Registered as a JIT symbol only in the hook tests.
extern "C" fn protect_hook(ctx: *mut JitCtx) {
    // SAFETY: the test passes the live frame it owns.
    let ctx = unsafe { &mut *ctx };
    ctx.mem_gen = ctx.mem_gen.wrapping_add(1);
    ctx.pins[0].allow = TLB_PROT_R;
}

fn scratch_module() -> JITModule {
    let mut b = settings::builder();
    b.set("opt_level", "none").expect("opt_level");
    b.set("is_pic", "false").expect("is_pic");
    b.set("use_colocated_libcalls", "false").expect("libcalls");
    b.set("enable_probestack", "false").expect("probestack");
    b.set("unwind_info", "false").expect("unwind_info");
    let isa = cranelift_native::builder()
        .expect("host ISA")
        .finish(settings::Flags::new(b))
        .expect("build ISA");
    let mut builder = JITBuilder::with_isa(isa, default_libcall_names());
    // Resolved only by the shapes built with `hook = true`; registered always so
    // the module needs no second construction path.
    builder.symbol("hoist_protect_hook", protect_hook as *const u8);
    JITModule::new(builder)
}

/// The block a hit runs: perform the access, report `DONE` (store) or the
/// bytes (load).
fn hit(
    bcx: &mut FunctionBuilder<'_>,
    size: u32,
    write: bool,
    val: Value,
    host: Value,
) -> Option<Value> {
    let flags = MemFlagsData::trusted();
    if write {
        store_guest_bytes(bcx, flags, host, size, val);
        Some(iconst_u64(bcx, DONE))
    } else {
        Some(load_guest_bytes(bcx, flags, host, size))
    }
}

/// Today's chain for one operand, unchanged: pin stage → (optionally) refill
/// stage → the helper stand-in (here: the `MISS` sentinel, which is what
/// `wie_jit_load`/`wie_jit_store` would answer on a failed translation too).
#[allow(clippy::too_many_arguments)]
fn today_chain(
    bcx: &mut FunctionBuilder<'_>,
    merge: Block,
    addr: Value,
    size: u32,
    write: bool,
    val: Value,
    pin: &HoistedPin,
    refill: &RefillProbe,
    with_refill: bool,
) {
    let flags = MemFlagsData::trusted();
    emit_probe_stage(
        bcx,
        merge,
        |bcx, miss| hoisted_pin_probe(bcx, pin, addr, size, write, miss),
        |bcx, host| hit(bcx, size, write, val, host),
    );
    if with_refill {
        emit_probe_stage(
            bcx,
            merge,
            |bcx, miss| refill_tlb_probe(bcx, refill, flags, addr, size, write, miss),
            |bcx, host| hit(bcx, size, write, val, host),
        );
    }
    let m = iconst_u64(bcx, MISS);
    bcx.ins().jump(merge, &[BlockArg::Value(m)]);
}

/// One compiled probe plus its emitted code.
///
/// `insns` is `code_info().total_size / 4`: aarch64 instructions are four bytes
/// and nothing pads inside a function, so that is the exact **laid-out** host
/// instruction count. `code` is the same bytes — unrelocated, since the only
/// external reference is the optional hook — for disassembly.
struct Probe {
    run: ProbeFn,
    insns: usize,
    code: Vec<u8>,
}

/// Build one probe function.
///
/// `hook` inserts a call to [`protect_hook`] *after* the descriptor loads and
/// before the access — a protection change between two accesses, with the first
/// one being the prologue's own read of the descriptor.
fn build(shape: Shape, origin: Origin, size: u32, write: bool, hook: bool) -> Probe {
    let mut module = scratch_module();
    let ptr_ty = module.target_config().pointer_type();
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(ptr_ty));
    sig.params.push(AbiParam::new(types::I64));
    sig.params.push(AbiParam::new(types::I64));
    sig.returns.push(AbiParam::new(types::I64));
    let id = module
        .declare_function("probe", Linkage::Export, &sig)
        .expect("declare probe");
    let mut func = Function::new();
    func.signature = sig;
    let hook_ref = hook.then(|| {
        let mut hs = module.make_signature();
        hs.params.push(AbiParam::new(ptr_ty));
        let hid = module
            .declare_function("hoist_protect_hook", Linkage::Import, &hs)
            .expect("declare hook");
        module.declare_func_in_func(hid, &mut func)
    });

    let mut ctx = module.make_context();
    let mut fctx = FunctionBuilderContext::new();
    {
        let mut bcx = FunctionBuilder::new(&mut func, &mut fctx);
        let entry = bcx.create_block();
        bcx.append_block_params_for_function_params(entry);
        bcx.switch_to_block(entry);
        bcx.seal_block(entry);
        let ctx_ptr = bcx.block_params(entry)[0];
        let addr = bcx.block_params(entry)[1];
        let val = bcx.block_params(entry)[2];
        let flags = MemFlagsData::trusted();
        let merge = bcx.create_block();
        bcx.append_block_param(merge, types::I64);

        let pin = hoist_pin_slot(&mut bcx, ctx_ptr, flags, 0);
        let refill = hoist_refill(&mut bcx, ctx_ptr, flags);
        let with_refill = origin != Origin::Pin;
        match shape {
            Shape::Today => today_chain(
                &mut bcx,
                merge,
                addr,
                size,
                write,
                val,
                &pin,
                &refill,
                with_refill,
            ),
            Shape::HoistedPlain => {
                let span = span_of_pin(&mut bcx, &pin, size, write);
                // The phase-1 wiring, verbatim.
                let fail = bcx.create_block();
                let host = home_span_probe(&mut bcx, &span, addr, fail);
                if let Some(v) = hit(&mut bcx, size, write, val, host) {
                    bcx.ins().jump(merge, &[BlockArg::Value(v)]);
                } else {
                    bcx.ins().jump(merge, &[]);
                }
                bcx.switch_to_block(fail);
                bcx.seal_block(fail);
                today_chain(
                    &mut bcx,
                    merge,
                    addr,
                    size,
                    write,
                    val,
                    &pin,
                    &refill,
                    with_refill,
                );
            }
            Shape::Hoisted => {
                let span = match origin {
                    Origin::Pin | Origin::PinThenRefill => span_of_pin(&mut bcx, &pin, size, write),
                    Origin::Slot => {
                        let slot = hoist_refill_slot(&mut bcx, &refill, flags, addr, write);
                        span_of_slot(&mut bcx, &slot, size)
                    }
                };
                if let Some(fr) = hook_ref {
                    bcx.ins().call(fr, &[ctx_ptr]);
                }
                hoisted_stage(
                    &mut bcx,
                    &span,
                    refill.mem_gen,
                    ctx_ptr,
                    flags,
                    addr,
                    merge,
                    |bcx| {
                        let s = iconst_u64(bcx, STALE);
                        bcx.ins().jump(merge, &[BlockArg::Value(s)]);
                    },
                    |bcx, host| hit(bcx, size, write, val, host),
                );
                // Positioned in the fallback block: today's chain, byte for
                // byte — the hoist only adds an edge into it.
                today_chain(
                    &mut bcx,
                    merge,
                    addr,
                    size,
                    write,
                    val,
                    &pin,
                    &refill,
                    with_refill,
                );
            }
        }
        bcx.switch_to_block(merge);
        bcx.seal_block(merge);
        let r = bcx.block_params(merge)[0];
        bcx.ins().return_(&[r]);
        bcx.seal_all_blocks();
        bcx.finalize();
    }
    ctx.func = func;
    module.define_function(id, &mut ctx).expect("define probe");
    let (insns, code) = {
        let cc = ctx.compiled_code().expect("compiled");
        (
            usize::try_from(cc.code_info().total_size).expect("code size fits") / 4,
            cc.code_buffer().to_vec(),
        )
    };
    module.clear_context(&mut ctx);
    module.finalize_definitions().expect("finalize");
    let ptr = module.get_finalized_function(id);
    // SAFETY: `ptr` is the entry point of the function just defined, and its
    // signature is the one declared above.
    let f: ProbeFn = unsafe { std::mem::transmute(ptr) };
    std::mem::forget(module);
    Probe {
        run: f,
        insns,
        code,
    }
}

struct Fixture {
    cpu: JitCpu,
    /// Guest base of the two mapped pages.
    data: u64,
    /// Host base of the first page (== `data`'s translation).
    host: *mut u8,
}

/// Two RW pages and a canary at `+0x40`, with `ctx.pins` left **empty**: any
/// descriptor present after a `dispatcher_entry` can only have come from
/// `jit_region_pins`, which is what makes
/// [`a_reprotect_between_executions_is_caught`] an end-to-end test rather than a
/// test of a fixture.
fn bare_pages() -> Fixture {
    let idx = NEXT_DATA.fetch_add(1, Ordering::Relaxed);
    let data = DATA_BASE.saturating_add(idx.saturating_mul(DATA_STRIDE));
    let mut cpu = JitCpu::open_x86_64();
    cpu.virtual_alloc(
        data,
        0x2000,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_READWRITE,
    )
    .expect("map the fixture pages");
    cpu.mem_write(data.saturating_add(0x40), &CANARY.to_ne_bytes())
        .expect("stamp the canary");
    let host = cpu.host_span(data, 0x1000, false).expect("host base");
    Fixture { cpu, data, host }
}

/// [`bare_pages`] plus a hand-built pin over page 0 in slot 0, for the tests that
/// are about the guard rather than about the descriptor's provenance.
///
/// `gen_offset` non-zero plants a pin whose generation does not match the frame's
/// (a dead descriptor); it is `^`-ed onto the frame's value rather than set, so
/// the test keeps working whatever the frame's generation happens to be.
fn fixture(gen_offset: u64, allow: u64) -> Fixture {
    let mut fx = bare_pages();
    let stamp = fx.cpu.thread.ctx.mem_gen ^ gen_offset;
    fx.cpu.thread.ctx.pins[0] = MemPin {
        guest_base: GuestVa::new(fx.data),
        guest_end: GuestVa::new(fx.data.saturating_add(0x2000)),
        host_base: HostAddr::from_ptr(fx.host),
        mem_gen: stamp,
        allow,
    };
    fx
}

fn run(f: ProbeFn, cpu: &mut JitCpu, addr: u64, val: u64) -> u64 {
    f(std::ptr::from_mut(&mut cpu.thread.ctx), addr, val)
}

fn read_u64(cpu: &mut JitCpu, addr: u64) -> u64 {
    let mut buf = [0_u8; 8];
    cpu.mem_read(addr, &mut buf).expect("read u64");
    u64::from_ne_bytes(buf)
}

/// What a zero-extending load of `size` bytes at `addr` must return — the same
/// widening `load_guest_bytes` performs, spelled out so a 1/2/4-byte access can be
/// checked against the bytes it actually covers.
fn read_sized(cpu: &mut JitCpu, addr: u64, size: u32) -> u64 {
    let mut buf = [0_u8; 8];
    let n = usize::try_from(size).expect("size fits");
    cpu.mem_read(addr, &mut buf[..n]).expect("read sized");
    let mut out = 0_u64;
    for (i, b) in buf[..n].iter().enumerate() {
        out |= u64::from(*b) << (8 * i);
    }
    out
}

/// Pin the slot to a page, the way the helper does on a cold miss.
fn install_slot(cpu: &mut JitCpu, page: u64, host: *mut u8, allow: u64) {
    let key = page >> 12;
    cpu.thread.ctx.refill[refill_slot(key)] =
        RefillSlot::new(key ^ cpu.thread.ctx.mem_gen, host, allow);
}

/// Three `nop`s: a block whose only job is to be *stepped*, because running any
/// block is how the dispatcher does its entry work.
const NOP3: [u8; 3] = [0x90, 0x90, 0x90];
/// Where the `nop` block jumps; the dispatcher stops before executing it.
const STOP_OFF: u64 = 0x100;
const CODE_BASE: u64 = 0x6c00_0000;
const CODE_STRIDE: u64 = 0x1_0000;
static NEXT_CODE: AtomicU64 = AtomicU64::new(0);

fn code_page(cpu: &mut JitCpu, insn: [u8; 3]) -> u64 {
    let idx = NEXT_CODE.fetch_add(1, Ordering::Relaxed);
    let base = CODE_BASE.saturating_add(idx.saturating_mul(CODE_STRIDE));
    cpu.virtual_alloc(
        base,
        usize::try_from(CODE_STRIDE).expect("stride fits"),
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_EXECUTE_READWRITE,
    )
    .expect("map the fixture's code page");
    let stop = usize::try_from(STOP_OFF).expect("stop offset fits");
    let mut page = vec![0x90_u8; stop + 2];
    page[0..3].copy_from_slice(&insn);
    let rel = i32::try_from(STOP_OFF.saturating_sub(8)).expect("rel32 fits");
    page[3] = 0xe9;
    page[4..8].copy_from_slice(&rel.to_le_bytes());
    page[stop] = 0x0f;
    page[stop + 1] = 0x0b;
    cpu.mem_write(base, &page).expect("write the fixture bytes");
    base
}

/// Run one block through the dispatcher.
///
/// This is not a convenience: `run_compiled` is where the frame's
/// `ctx.mem_gen` is stamped and where `ctx.pins` is rebuilt when the generation
/// moved, so stepping a block is the *only* faithful way to perform "the next
/// dispatcher entry" without owning `emit.rs`. Nothing about the probe under test
/// is assumed by doing it this way — the block being stepped is an unrelated
/// `nop`.
fn dispatcher_entry(cpu: &mut JitCpu, code: u64) {
    cpu.write_rip(code).expect("set rip");
    let (result, _retired) = cpu.step_one().expect("step the nop block");
    assert!(
        matches!(result, StepResult::Continue),
        "the nop block must retire, got {result:?}"
    );
}

/// The engine's **own** descriptor for `addr`, straight out of `ctx.pins` after a
/// dispatcher entry — never a hand-built `MemPin`.
fn engine_pin(cpu: &JitCpu, addr: u64) -> Option<MemPin> {
    cpu.thread.ctx.pins.iter().copied().find(|p| {
        !p.is_empty() && p.guest_base.0 <= addr && addr.saturating_add(8) <= p.guest_end.0
    })
}

/// A protection change between two executions of the **same compiled block** is
/// caught by the prologue, which re-reads the descriptor on every entry.
///
/// The machinery is the engine's, not the test's: `virtual_protect` bumps the
/// generation and invalidates the TLB (`cc913ea`: every protection change,
/// including a refused one's undo), and `dispatcher_entry` runs `run_compiled`,
/// which re-stamps `ctx.mem_gen` and rebuilds `ctx.pins` from `jit_region_pins`.
/// So execution 2 publishes its span from a descriptor that lost `W` — and it
/// must not take the fast path, and must not write the page.
///
/// This is the test that lets the in-frame generation compare be dropped: the
/// span is a pure function of the descriptor, the descriptor is re-read per
/// entry, and every protection change reaches the descriptor before the next
/// entry. See also `only_an_in_frame_move_needs_the_generation_compare`.
#[test]
fn a_reprotect_between_executions_is_caught() {
    // No hand-built descriptor: `ctx.pins` starts empty, so every pin below is the
    // engine's own.
    let mut fx = bare_pages();
    assert!(
        engine_pin(&fx.cpu, fx.data).is_none(),
        "the fixture must not pre-plant a descriptor, or the test proves less"
    );
    let page = fx.data;
    let nop = code_page(&mut fx.cpu, NOP3);
    // Let the dispatcher build the frame for the current generation, then borrow
    // *its* descriptor for the page under test.
    dispatcher_entry(&mut fx.cpu, nop);
    let live = engine_pin(&fx.cpu, page.saturating_add(0x40))
        .expect("the engine must pin the fixture's data page");
    assert!(
        live.allow & TLB_PROT_W != 0,
        "the page starts writable, or the first execution proves nothing"
    );
    fx.cpu.thread.ctx.pins[0] = live;
    let gen0 = fx.cpu.thread.ctx.mem_gen;

    // Execution 1: the span is published from a writable descriptor.
    let probe = build(Shape::HoistedPlain, Origin::Pin, 8, true, false);
    let at1 = page.saturating_add(0x100);
    assert_eq!(
        run(probe.run, &mut fx.cpu, at1, 0xaaaa_bbbb_cccc_dddd),
        DONE
    );
    assert_eq!(read_u64(&mut fx.cpu, at1), 0xaaaa_bbbb_cccc_dddd);

    // The guest's own VirtualProtect: read-only, through the real machinery.
    fx.cpu
        .virtual_protect(page, 0x1000, protect::PAGE_READONLY)
        .expect("re-protect the page read-only");

    // The next dispatcher entry: re-stamp the generation, rebuild the pins.
    dispatcher_entry(&mut fx.cpu, nop);
    let gen1 = fx.cpu.thread.ctx.mem_gen;
    assert_ne!(
        gen1, gen0,
        "a protection change must move the frame's generation (cc913ea)"
    );
    match engine_pin(&fx.cpu, page.saturating_add(0x40)) {
        Some(p) => assert_eq!(
            p.allow & TLB_PROT_W,
            0,
            "the rebuilt descriptor must have lost W"
        ),
        None => panic!("the engine must still describe the page it re-protected"),
    }
    let after = engine_pin(&fx.cpu, page.saturating_add(0x40)).expect("pin");
    fx.cpu.thread.ctx.pins[0] = after;

    // Execution 2, same compiled block: the span must be empty, the store refused.
    let at2 = page.saturating_add(0x140);
    assert_eq!(
        run(probe.run, &mut fx.cpu, at2, 0x1234_5678_9abc_def0),
        MISS,
        "a re-protected page must not be served from the span"
    );
    assert_eq!(
        read_u64(&mut fx.cpu, at2),
        0,
        "…and the store must not land"
    );
    // The read direction is still allowed, and is still served.
    let read_probe = build(Shape::HoistedPlain, Origin::Pin, 8, false, false);
    assert_eq!(
        run(read_probe.run, &mut fx.cpu, page.saturating_add(0x40), 0),
        CANARY,
        "a read-only page must still be readable through the hoist"
    );
}

/// What the in-frame generation compare buys, and what it does not.
///
/// The compare reads `JitCtx::mem_gen` at the access and compares it with the
/// value the prologue loaded. Nothing inside a `run_compiled` frame writes that
/// field except the dispatcher, so the only way to make the two disagree is for
/// something to re-stamp it mid-frame — which is what this test does, harmlessly
/// (the page itself is untouched, so a take is not a memory-safety failure here,
/// it is the observation).
///
/// Two rows, same frame mutation, both shapes:
///
/// * with the compare (`Shape::Hoisted`) the access goes to the generation edge;
/// * without it — the phase-1 wiring, `Shape::HoistedPlain` — the access is served.
///
/// So the compare observes exactly one thing, the thing that cannot happen today.
/// The re-protect case that *can* happen is
/// [`a_reprotect_between_executions_is_caught`], and the prologue catches it.
#[test]
fn only_an_in_frame_move_needs_the_generation_compare() {
    // Row 1: the compare fires.
    let mut fx = fixture(0, TLB_PROT_R | TLB_PROT_W);
    let at = fx.data.saturating_add(0x180);
    let checked = build(Shape::Hoisted, Origin::Pin, 8, true, true);
    assert_eq!(
        run(checked.run, &mut fx.cpu, at, 0x5555_aaaa_5555_aaaa),
        STALE,
        "the in-frame compare must refuse an access whose frame moved under it"
    );

    // Row 2: without it, the same access is served.
    let mut fx = fixture(0, TLB_PROT_R | TLB_PROT_W);
    let at = fx.data.saturating_add(0x180);
    let plain = build(Shape::HoistedPlain, Origin::Pin, 8, true, true);
    assert_eq!(
        run(plain.run, &mut fx.cpu, at, 0x5555_aaaa_5555_aaaa),
        DONE,
        "without the compare nothing observes the moved frame — which is the point"
    );
}

/// A live pin, a pinned load: the fast path must serve it and read the bytes
/// that are actually there.
#[test]
fn hoisted_pin_span_reads_the_pinned_bytes() {
    let mut fx = fixture(0, TLB_PROT_R | TLB_PROT_W);
    let f = build(Shape::Hoisted, Origin::Pin, 8, false, false);
    let got = run(f.run, &mut fx.cpu, fx.data.saturating_add(0x40), 0);
    assert_ne!(got, MISS, "a live pin must not fall into the chain");
    assert_ne!(got, STALE, "the generation did not move");
    assert_eq!(got, CANARY, "the fast path read the wrong host bytes");
}

/// …and a pinned store lands exactly once, at the translated address.
#[test]
fn hoisted_pin_span_writes_the_pinned_bytes() {
    let mut fx = fixture(0, TLB_PROT_R | TLB_PROT_W);
    let f = build(Shape::Hoisted, Origin::Pin, 8, true, false);
    let at = fx.data.saturating_add(0x80);
    let stored = 0x0bad_f00d_dead_beef_u64;
    assert_eq!(run(f.run, &mut fx.cpu, at, stored), DONE);
    assert_eq!(read_u64(&mut fx.cpu, at), stored);
    assert_eq!(
        read_u64(&mut fx.cpu, fx.data.saturating_add(0x40)),
        CANARY,
        "the store must not have moved to another address"
    );
}

/// A pin whose generation is stale publishes an empty span: the guard must
/// never take the fast path, whatever the address.
#[test]
fn stale_pin_generation_never_takes_the_fast_path() {
    let mut fx = fixture(1, TLB_PROT_R | TLB_PROT_W);
    let f = build(Shape::Hoisted, Origin::Pin, 8, false, false);
    for addr in [
        fx.data,
        fx.data.saturating_add(0x40),
        fx.data.saturating_add(0xff8),
        0,
        u64::MAX,
    ] {
        let got = run(f.run, &mut fx.cpu, addr, 0);
        assert_eq!(
            got, MISS,
            "a stale pin generation must fall to the chain at {addr:#x}"
        );
    }
}

/// A permission change between the descriptor loads and the access is caught
/// by the generation check — and the store does **not** land.
#[test]
fn protection_flip_between_two_accesses_is_caught() {
    let mut fx = fixture(0, TLB_PROT_R | TLB_PROT_W);
    let addr = fx.data.saturating_add(0x100);
    let stored = 0x1111_2222_3333_4444_u64;

    // Without the flip, this pin allows the store and the span is non-empty.
    let plain = build(Shape::Hoisted, Origin::Pin, 8, true, false);
    assert_eq!(run(plain.run, &mut fx.cpu, addr, stored), DONE);
    assert_eq!(read_u64(&mut fx.cpu, addr), stored);

    // With it, the generation moves after the span was published: the access
    // must take the *stale* edge, so nothing is written.
    let flipped = build(Shape::Hoisted, Origin::Pin, 8, true, true);
    let frozen = fx.cpu.thread.ctx.mem_gen;
    fx.cpu.thread.ctx.pins[0].allow = TLB_PROT_R | TLB_PROT_W;
    let other = 0x5555_6666_7777_8888_u64;
    assert_eq!(
        run(
            flipped.run,
            &mut fx.cpu,
            fx.data.saturating_add(0x140),
            other
        ),
        STALE,
        "a generation bump between the prologue and the access must exit"
    );
    assert_ne!(fx.cpu.thread.ctx.mem_gen, frozen, "the hook must have run");
    assert_eq!(
        read_u64(&mut fx.cpu, fx.data.saturating_add(0x140)),
        0,
        "a refused access must not have written the bytes"
    );
}

/// The interval test is a page test: a page-resident access is served, one
/// that would run past the page fails the `hi` clamp and takes the chain.
#[test]
fn cross_page_access_and_page_key_change_take_the_chain() {
    let mut fx = fixture(0, TLB_PROT_R | TLB_PROT_W);
    fx.cpu.thread.ctx.pins[0] = MemPin::EMPTY;
    let host = fx.host;
    let data = fx.data;
    install_slot(&mut fx.cpu, data, host, TLB_PROT_R | TLB_PROT_W);
    let f = build(Shape::Hoisted, Origin::Slot, 8, false, false);

    assert_eq!(
        run(f.run, &mut fx.cpu, data.saturating_add(0x40), 0),
        CANARY,
        "the slot's own page must be served inline"
    );
    // Last 8 bytes of the page: `hi = page_base + 4096 - 8`.
    assert_eq!(
        run(f.run, &mut fx.cpu, data.saturating_add(0xff8), 0),
        0,
        "the last whole access on the page must still be inline"
    );
    // 8 bytes starting at page_end-4: crosses into the next page.
    assert_eq!(
        run(f.run, &mut fx.cpu, data.saturating_add(0xffc), 0),
        MISS,
        "an access that crosses the page must not be served from this home"
    );
    // A different page: the published home is for page 0 only.
    assert_eq!(
        run(f.run, &mut fx.cpu, data.saturating_add(0x1040), 0),
        MISS,
        "a home published for one page must not answer another"
    );
}

/// A stack pin that no longer covers the address is caught by the interval —
/// including a pin that is shorter than the access, which is the shape that
/// would wrap `hi` if it were computed without the `end >= lo + size` clamp.
#[test]
fn moved_or_too_short_pin_is_caught() {
    let mut fx = fixture(0, TLB_PROT_R | TLB_PROT_W);
    let data = fx.data;
    let host = fx.host;
    let stamp = fx.cpu.thread.ctx.mem_gen;
    let sub = |lo: u64, len: u64| MemPin {
        guest_base: GuestVa::new(lo),
        guest_end: GuestVa::new(lo.saturating_add(len)),
        host_base: HostAddr::from_ptr(host),
        mem_gen: stamp,
        allow: TLB_PROT_R | TLB_PROT_W,
    };
    let f = build(Shape::Hoisted, Origin::Pin, 8, false, false);

    // `[data + 0x100, data + 0x180)`: hi = 0x178 for an 8-byte access.
    fx.cpu.thread.ctx.pins[0] = sub(data.saturating_add(0x100), 0x80);
    assert_ne!(
        run(f.run, &mut fx.cpu, data.saturating_add(0x100), 0),
        MISS,
        "the pin's own base must be served"
    );
    assert_ne!(
        run(f.run, &mut fx.cpu, data.saturating_add(0x178), 0),
        MISS,
        "the last whole access inside the pin must be served"
    );
    for outside in [0x0ff, 0x179, 0x180, 0x200, 0xffc] {
        assert_eq!(
            run(f.run, &mut fx.cpu, data.saturating_add(outside), 0),
            MISS,
            "an address outside the pin ({outside:#x}) must fall to the chain"
        );
    }

    // A pin shorter than the access: `end - lo < size`, so `end - size`
    // wraps. Nothing may be served from it, at any address.
    fx.cpu.thread.ctx.pins[0] = sub(data.saturating_add(0x100), 4);
    for addr in [
        data.saturating_add(0x100),
        data.saturating_add(0x102),
        data.saturating_add(0x104),
        data,
        0,
        u64::MAX,
    ] {
        assert_eq!(
            run(f.run, &mut fx.cpu, addr, 0),
            MISS,
            "a pin shorter than the access must never be served ({addr:#x})"
        );
    }
}

/// A dead descriptor publishes an empty span — `host_base` null, no
/// permissions, or a stage that cannot hold the access — and an empty span
/// must be unreachable, which is the property the whole encoding rests on.
#[test]
fn empty_span_is_never_taken() {
    let mut fx = fixture(0, TLB_PROT_R | TLB_PROT_W);
    let data = fx.data;
    let dead = [
        MemPin::EMPTY,
        MemPin {
            allow: 0,
            ..fx.cpu.thread.ctx.pins[0]
        },
        MemPin {
            host_base: HostAddr::NULL,
            ..fx.cpu.thread.ctx.pins[0]
        },
    ];
    let load = build(Shape::Hoisted, Origin::Pin, 8, false, false);
    let store = build(Shape::Hoisted, Origin::Pin, 8, true, false);
    for pin in dead {
        fx.cpu.thread.ctx.pins[0] = pin;
        for addr in [0, 1, 0xfff, data, data.saturating_add(0x40), u64::MAX - 3] {
            assert_eq!(
                run(load.run, &mut fx.cpu, addr, 0),
                MISS,
                "a dead descriptor must never serve a load ({addr:#x})"
            );
            assert_eq!(
                run(store.run, &mut fx.cpu, addr, 0xfeed),
                MISS,
                "a dead descriptor must never serve a store ({addr:#x})"
            );
        }
    }
    // The canary is untouched, and the frame is still the one the test built.
    assert_eq!(read_u64(&mut fx.cpu, data.saturating_add(0x40)), CANARY);
}

/// The hoisted slot resolution duplicates `refill_tlb_probe`'s index arithmetic
/// (golden-ratio scatter, shift, mask, tag fold), and a divergence between the two
/// would send the home to a slot the chain never fills — silently, and only for
/// some page keys. So: for several pages, install exactly the slot the host-side
/// [`refill_slot`] names and require the hoisted guard to find it, then install at
/// the *neighbouring* index and require it not to.
#[test]
fn slot_index_matches_the_host_side_refill_slot() {
    let mut fx = fixture(0, TLB_PROT_R | TLB_PROT_W);
    fx.cpu.thread.ctx.pins[0] = MemPin::EMPTY;
    let host = fx.host;
    let page0 = fx.data;
    let hoisted = build(Shape::Hoisted, Origin::Slot, 8, false, false);
    let blank = RefillSlot::EMPTY;
    for page in 0..4_u64 {
        let base = page0.saturating_add(page.saturating_mul(0x1000));
        let key = base >> 12;
        let want = refill_slot(key);
        // The slot the host names: the guard must resolve through it.
        fx.cpu.thread.ctx.refill = [blank; REFILL_SLOTS];
        fx.cpu.thread.ctx.refill[want] = RefillSlot::new(
            key ^ fx.cpu.thread.ctx.mem_gen,
            host,
            TLB_PROT_R | TLB_PROT_W,
        );
        assert_ne!(
            run(hoisted.run, &mut fx.cpu, base.saturating_add(0x40), 0),
            MISS,
            "page {page}: the guard must resolve slot {want}, the one the host names"
        );
        // Any other slot: nothing to find.
        fx.cpu.thread.ctx.refill = [blank; REFILL_SLOTS];
        let other = (want + 1) % REFILL_SLOTS;
        fx.cpu.thread.ctx.refill[other] = RefillSlot::new(
            key ^ fx.cpu.thread.ctx.mem_gen,
            host,
            TLB_PROT_R | TLB_PROT_W,
        );
        assert_eq!(
            run(hoisted.run, &mut fx.cpu, base.saturating_add(0x40), 0),
            MISS,
            "page {page}: a slot at index {other} must not answer for slot {want}"
        );
    }
}

/// A refused span must still be answered: the chain behind it re-derives the
/// translation from scratch, and can therefore reach a stage the span was *not*
/// published from. Here the span comes from a dead pin, and the refill slot —
/// which the chain reaches second — must still serve the access.
///
/// This is the property that keeps the hoist from becoming a second translator
/// that can only say no, and it is why the fallback is the existing chain rather
/// than a new one: the same `today_chain` function feeds both shapes.
#[test]
fn a_refused_span_still_resolves_through_the_chain() {
    let mut fx = fixture(0, TLB_PROT_R | TLB_PROT_W);
    let data = fx.data;
    let host = fx.host;
    fx.cpu.thread.ctx.pins[0] = MemPin::EMPTY;
    install_slot(&mut fx.cpu, data, host, TLB_PROT_R | TLB_PROT_W);
    let at = data.saturating_add(0x40);
    let today = build(Shape::Today, Origin::PinThenRefill, 8, false, false);
    let hoisted = build(Shape::Hoisted, Origin::PinThenRefill, 8, false, false);
    let a = run(today.run, &mut fx.cpu, at, 0);
    let b = run(hoisted.run, &mut fx.cpu, at, 0);
    assert_ne!(a, MISS, "the refill stage must answer when the pin is dead");
    assert_eq!(
        b, a,
        "a refused span must reach the same chain and get the same answer"
    );
    assert_eq!(b, CANARY, "…and the bytes it read must be the mapped ones");
}

/// The differential the hoist has to win: on every boundary of three pin
/// shapes and every access width, the hoisted guard accepts exactly the
/// accesses the probe it replaces accepts — and when it accepts, the bytes it
/// reads are the bytes at the translated address.
#[test]
fn span_agrees_with_the_probe_it_replaces() {
    let mut fx = fixture(0, TLB_PROT_R | TLB_PROT_W);
    let data = fx.data;
    let host = fx.host;
    let stamp = fx.cpu.thread.ctx.mem_gen;
    // Every 8-byte slot on page 0 holds its own address, so a wrongly
    // translated read cannot coincide with a right one.
    for off in (0..0x1000_u64).step_by(8) {
        fx.cpu
            .mem_write(
                data.saturating_add(off),
                &data.saturating_add(off).to_ne_bytes(),
            )
            .expect("stamp the page");
    }
    let shapes = [
        MemPin {
            guest_base: GuestVa::new(data),
            guest_end: GuestVa::new(data.saturating_add(0x2000)),
            host_base: HostAddr::from_ptr(host),
            mem_gen: stamp,
            allow: TLB_PROT_R | TLB_PROT_W,
        },
        MemPin {
            guest_base: GuestVa::new(data.saturating_add(0x100)),
            guest_end: GuestVa::new(data.saturating_add(0x180)),
            host_base: HostAddr::from_ptr(host),
            mem_gen: stamp,
            allow: TLB_PROT_R,
        },
        MemPin::EMPTY,
    ];
    let addrs = [
        0,
        1,
        data.saturating_sub(8),
        data.saturating_sub(1),
        data,
        data.saturating_add(1),
        data.saturating_add(0xf8),
        data.saturating_add(0xff),
        data.saturating_add(0x100),
        data.saturating_add(0x104),
        data.saturating_add(0x178),
        data.saturating_add(0x179),
        data.saturating_add(0x180),
        data.saturating_add(0xff0),
        data.saturating_add(0xff8),
        data.saturating_add(0xffc),
        data.saturating_add(0x1000),
        data.saturating_add(0x1fff),
        u64::MAX - 7,
        u64::MAX,
    ];
    for size in [1_u32, 2, 4, 8] {
        let today = build(Shape::Today, Origin::Pin, size, false, false);
        let hoisted = build(Shape::Hoisted, Origin::Pin, size, false, false);
        for pin in shapes {
            fx.cpu.thread.ctx.pins[0] = pin;
            for addr in addrs {
                let a = run(today.run, &mut fx.cpu, addr, 0);
                let b = run(hoisted.run, &mut fx.cpu, addr, 0);
                assert_eq!(
                    a == MISS,
                    b == MISS,
                    "size {size}: pin {:#x}..{:#x}, addr {addr:#x}: probe {} / hoist {}",
                    pin.guest_base.0,
                    pin.guest_end.0,
                    if a == MISS { "missed" } else { "hit" },
                    if b == MISS { "missed" } else { "hit" },
                );
                if b != MISS {
                    // The value the guard produced must be the bytes the probe's
                    // own translation points at: `host_base + (addr - guest_base)`
                    // is guest address `data + (addr - guest_base)` here, because
                    // both pins cover the fixture pages at bias `data - guest_base`.
                    let want = data.saturating_add(addr.saturating_sub(pin.guest_base.0));
                    assert_eq!(
                        b,
                        read_sized(&mut fx.cpu, want, size),
                        "size {size}: addr {addr:#x} was served from the wrong host address"
                    );
                }
            }
        }
    }
}

/// The instruction budget, measured from the emitted code rather than from CLIF.
///
/// `insns` is the exact **laid-out** host instruction count of each whole
/// function (`code_info().total_size / 4`; aarch64 instructions are four bytes and
/// nothing pads inside a function). Laid-out is the right unit for this test
/// because the hoist *adds* a guard and keeps the chain as its fallback: what must
/// not grow without bound is the added text, so the assertion is on the delta, and
/// the sum over shapes is recorded (and, with `WIE_HOIST_DUMP=<dir>`, written out)
/// so a future change to the backend or to the span cannot silently inflate it.
///
/// The **executed** count is a different question and this test cannot answer it —
/// it needs a disassembly to separate the hot path from the two cold blocks. The
/// numbers are in the report that accompanies this change, taken from these dumps.
#[test]
fn hoisted_guard_instruction_counts() {
    let today_pin = build(Shape::Today, Origin::Pin, 8, false, false);
    let today_slot = build(Shape::Today, Origin::Slot, 8, false, false);
    let hoist_pin = build(Shape::Hoisted, Origin::Pin, 8, false, false);
    let hoist_slot = build(Shape::Hoisted, Origin::Slot, 8, false, false);
    if let Ok(dir) = std::env::var("WIE_HOIST_DUMP") {
        for (name, p) in [
            ("today_pin", &today_pin),
            ("today_slot", &today_slot),
            ("hoist_pin", &hoist_pin),
            ("hoist_slot", &hoist_slot),
        ] {
            let at = std::path::Path::new(&dir);
            std::fs::write(at.join(format!("{name}.count")), format!("{}\n", p.insns))
                .expect("write the count");
            std::fs::write(at.join(format!("{name}.bin")), &p.code).expect("write the code");
        }
    }
    // The hoist *adds* text and keeps today's chain as the fallback, so the
    // laid-out delta is not a win and is not claimed to be one — what it must not
    // do is grow without bound. Both deltas are dominated by the cold half:
    //
    //   pin:  +30 = 16 prologue (span from the descriptor) + 7 guard (3 shared
    //              generation check, 4 interval) + 7 cold (stale sentinel, entry
    //              to the chain)
    //   slot: +43 = 16 prologue + 7 guard + 7 cold + ~13 because the refill hash
    //              the chain no longer needs *fast* is still emitted once as the
    //              fallback, so the table arithmetic exists twice
    //
    // Measured on this shape with Cranelift 0.133 at `opt_level = none`; the
    // bounds are those values plus headroom for a schedule change.
    for (name, today, hoisted, added) in [
        ("pin", &today_pin, &hoist_pin, 34),
        ("slot", &today_slot, &hoist_slot, 48),
    ] {
        let delta = hoisted.insns.saturating_sub(today.insns);
        assert!(
            delta <= added,
            "{name}: the hoisted shape adds {delta} host instructions \
             ({} vs {}), more than the {added} the guard and its cold half may \
             take",
            hoisted.insns,
            today.insns
        );
        assert!(
            delta > 0,
            "{name}: the hoisted shape must contain the guard (delta {delta})"
        );
    }
}

// ---------------------------------------------------------------------------
// The shape of the block that motivated the hoist: **four** width-4 stores in
// one block, guarded from one pin. `0x1800cef0b` (a dword fill in SDL2, the
// block §9 of the scaled-blit attribution measured) is exactly this — its last
// compile carries eight `store region0`, four per path, and every guarded
// operand reads pin slot 0.
//
// One span per `(width, direction)` is published at entry and each access
// re-validates against it, so the property that matters is not "the block is on
// the fast path" but "each of the four is checked on its own": one address
// outside the span must not ride on its neighbours' pass. A fast path wrongly
// taken is a memory-safety bug in the emulator, so the third test makes the
// wrong answer *land* somewhere observable instead of faulting the process.
// ---------------------------------------------------------------------------

/// `(ctx, a0, a1, a2, a3, value) -> u64`: four stores, one span, four checks.
type Probe4Fn = extern "C" fn(*mut JitCtx, u64, u64, u64, u64, u64) -> u64;

/// A compiled four-store block and its laid-out host instruction count.
struct Probe4 {
    run: Probe4Fn,
    insns: usize,
}

/// One span published at entry (what `block_homes` publishes per `(width,
/// direction)`), then four stores that each re-validate against it and fall into
/// today's chain — alone, on their own failure. This is the phase-1 wiring's
/// shape with the operand count the real block has.
fn build_four_stores() -> Probe4 {
    let mut module = scratch_module();
    let ptr_ty = module.target_config().pointer_type();
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(ptr_ty));
    for _ in 0..4 {
        sig.params.push(AbiParam::new(types::I64));
    }
    sig.params.push(AbiParam::new(types::I64));
    sig.returns.push(AbiParam::new(types::I64));
    let id = module
        .declare_function("probe4", Linkage::Export, &sig)
        .expect("declare probe4");
    let mut func = Function::new();
    func.signature = sig;

    let mut ctx = module.make_context();
    let mut fctx = FunctionBuilderContext::new();
    {
        let mut bcx = FunctionBuilder::new(&mut func, &mut fctx);
        let entry = bcx.create_block();
        bcx.append_block_params_for_function_params(entry);
        bcx.switch_to_block(entry);
        bcx.seal_block(entry);
        let params = bcx.block_params(entry).to_vec();
        let ctx_ptr = params[0];
        let addr: Vec<Value> = params[1..5].to_vec();
        let val = params[5];
        let flags = MemFlagsData::trusted();

        // Prologue: the descriptor loads and *one* span for the whole block —
        // the point of the hoist, and the reason four operands cost one guard's
        // worth of prologue rather than four.
        let pin = hoist_pin_slot(&mut bcx, ctx_ptr, flags, 0);
        let refill = hoist_refill(&mut bcx, ctx_ptr, flags);
        let span = span_of_pin(&mut bcx, &pin, 4, true);

        let done = bcx.create_block();
        bcx.append_block_param(done, types::I64);
        let fails: Vec<Block> = (0..4).map(|_| bcx.create_block()).collect();

        for (i, fail) in fails.iter().enumerate() {
            let host = home_span_probe(&mut bcx, &span, addr[i], *fail);
            let distinct = bcx
                .ins()
                .iadd_imm(val, i64::try_from(i).expect("operand index fits"));
            store_guest_bytes(&mut bcx, flags, host, 4, distinct);
        }
        let d = iconst_u64(&mut bcx, DONE);
        bcx.ins().jump(done, &[BlockArg::Value(d)]);

        // Each fallback is today's chain, byte for byte, entered only from its
        // own operand's failed predicate.
        for (i, fail) in fails.iter().enumerate() {
            bcx.switch_to_block(*fail);
            bcx.seal_block(*fail);
            today_chain(&mut bcx, done, addr[i], 4, true, val, &pin, &refill, false);
        }

        bcx.switch_to_block(done);
        bcx.seal_block(done);
        let r = bcx.block_params(done)[0];
        bcx.ins().return_(&[r]);
        bcx.seal_all_blocks();
        bcx.finalize();
    }
    ctx.func = func;
    module.define_function(id, &mut ctx).expect("define probe4");
    let insns = {
        let cc = ctx.compiled_code().expect("compiled");
        usize::try_from(cc.code_info().total_size).expect("code size fits") / 4
    };
    module.clear_context(&mut ctx);
    module.finalize_definitions().expect("finalize");
    let ptr = module.get_finalized_function(id);
    // SAFETY: `ptr` is the entry point of the function just defined above, and
    // its signature is the five-parameter one declared for it.
    let run: Probe4Fn = unsafe { std::mem::transmute(ptr) };
    std::mem::forget(module);
    Probe4 { run, insns }
}

/// [`fixture`], except the pin covers **page 0 only**.
///
/// That is what makes a wrongly-taken fast path observable instead of fatal: an
/// address in page 1 is outside the span but still mapped and RW, so the span
/// accepting it would write page 1's host memory — which the canary catches —
/// rather than faulting the test process.
fn narrow_fixture(gen_offset: u64) -> Fixture {
    let mut fx = bare_pages();
    let stamp = fx.cpu.thread.ctx.mem_gen ^ gen_offset;
    fx.cpu.thread.ctx.pins[0] = MemPin {
        guest_base: GuestVa::new(fx.data),
        guest_end: GuestVa::new(fx.data.saturating_add(0x1000)),
        host_base: HostAddr::from_ptr(fx.host),
        mem_gen: stamp,
        allow: TLB_PROT_R | TLB_PROT_W,
    };
    fx.cpu
        .mem_write(fx.data.saturating_add(0x1040), &CANARY.to_ne_bytes())
        .expect("stamp the page-1 canary");
    fx
}

/// Four in-span stores share the one published span and all four are done
/// inline, each writing its own word.
#[test]
fn four_stores_share_one_span_and_all_take_it() {
    let mut fx = narrow_fixture(0);
    let p = build_four_stores();
    let data = fx.data;
    let addrs = [0x100_u64, 0x200, 0x300, 0x380];
    let out = (p.run)(
        std::ptr::from_mut(&mut fx.cpu.thread.ctx),
        data.saturating_add(addrs[0]),
        data.saturating_add(addrs[1]),
        data.saturating_add(addrs[2]),
        data.saturating_add(addrs[3]),
        0xaaaa_0000,
    );
    assert_eq!(
        out, DONE,
        "all four in-span stores must be done inline (function lays out to {} host instructions)",
        p.insns
    );
    for (i, off) in addrs.iter().enumerate() {
        assert_eq!(
            read_sized(&mut fx.cpu, data.saturating_add(*off), 4),
            0xaaaa_0000 + u64::try_from(i).expect("index fits"),
            "operand {i} must have written its own word"
        );
    }
    assert_eq!(
        read_u64(&mut fx.cpu, data.saturating_add(0x1040)),
        CANARY,
        "nothing may write page 1"
    );
}

/// The operand outside the span goes to the chain and is refused there, while
/// the three inside it are still served — the per-access check, not a
/// block-wide one.
///
/// If the span were applied to all four unconditionally, operand 3 would write
/// page 1 at `host + 0x1000` and the canary would come back changed.
#[test]
fn one_operand_outside_the_span_does_not_ride_on_its_neighbours() {
    let mut fx = narrow_fixture(0);
    let p = build_four_stores();
    let data = fx.data;
    let inside = [0x100_u64, 0x200, 0x300];
    let out = (p.run)(
        std::ptr::from_mut(&mut fx.cpu.thread.ctx),
        data.saturating_add(inside[0]),
        data.saturating_add(inside[1]),
        data.saturating_add(inside[2]),
        // Page 1, outside the pin — and aimed at the canary, so a span wrongly
        // applied to this operand destroys a word we read back.
        data.saturating_add(0x1040),
        0xbbbb_0000,
    );
    // The memory-safety assertion comes first: under a block-wide span this is
    // the one that fires, and its message names the corruption rather than a
    // return value.
    assert_eq!(
        read_u64(&mut fx.cpu, data.saturating_add(0x1040)),
        CANARY,
        "a span wrongly taken for the fourth operand would have written page 1"
    );
    assert_eq!(
        out, MISS,
        "the out-of-span operand must reach the chain, which refuses it"
    );
    for (i, off) in inside.iter().enumerate() {
        assert_eq!(
            read_sized(&mut fx.cpu, data.saturating_add(*off), 4),
            0xbbbb_0000 + u64::try_from(i).expect("index fits"),
            "operand {i} is inside the span and must still be served"
        );
    }
}

/// A descriptor whose generation does not match the frame publishes an empty
/// span, so all four operands take the chain and nothing is written.
#[test]
fn a_dead_pin_descriptor_sends_all_four_to_the_chain() {
    let mut fx = narrow_fixture(1);
    let p = build_four_stores();
    let data = fx.data;
    let addrs = [0x100_u64, 0x200, 0x300, 0x380];
    let out = (p.run)(
        std::ptr::from_mut(&mut fx.cpu.thread.ctx),
        data.saturating_add(addrs[0]),
        data.saturating_add(addrs[1]),
        data.saturating_add(addrs[2]),
        data.saturating_add(addrs[3]),
        0xcccc_0000,
    );
    assert_eq!(out, MISS, "an empty span must never be taken");
    for off in addrs {
        assert_eq!(
            read_sized(&mut fx.cpu, data.saturating_add(off), 4),
            0,
            "no operand may write through a dead descriptor"
        );
    }
    assert_eq!(
        read_u64(&mut fx.cpu, data.saturating_add(0x1040)),
        CANARY,
        "page 1 must be untouched"
    );
}
