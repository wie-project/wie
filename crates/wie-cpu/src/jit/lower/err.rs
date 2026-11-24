//! `LowerError`: the typed reason a block's lowering refused to compile.
//!
//! Every lowering helper in this module tree used to fail with a bare
//! `String` — 38 distinct ad-hoc `format!` templates across `gpr`, `sse`,
//! `sse_fp`, `mem`, `emit`, `string`, `analysis` and the dispatch itself. A
//! caller could only `{e}` it into a log line, so the *reason* was available
//! exclusively to a human reader, and the one distinction that matters was not
//! representable at all: "the JIT does not implement this instruction" versus
//! "the JIT implements it but not this operand form".
//!
//! That distinction is the whole value of the type. It is what
//! [`refusal_reason`] reports, what [`note_refusal`] counts, and what a caller
//! can now match on instead of scanning text — the same argument
//! [`crate::CpuError::Win32`] documents for the interpreter side, applied here
//! so that `jit/lower` does not become a second, contradictory convention.

use super::opclass;
use crate::jit::config::JitConfig;
use crate::jit::pipeline::OPCODE_HISTO;
use iced_x86::Instruction;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// Why one instruction could not be lowered.
///
/// Three values, because three different things can go wrong and they want
/// three different responses. `Verifier` carries a `String` for the same
/// reason `CpuError::Message` does: the text is produced by Cranelift (or by an
/// internal invariant) and is genuinely dynamic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LowerError {
    /// The JIT does not implement this **instruction**. The operation-class
    /// table has no lowering row for the mnemonic, or (the drift case) it has
    /// one and [`super::insn::lower_insn`] has no arm for it. Either way the
    /// block falls back to the interpreter, and the *fix* is to write an
    /// emitter — not to reject one operand shape.
    Unsupported,

    /// The JIT does implement the instruction but not this **operand form**.
    ///
    /// The payload is the shape's name, not a rendered message: `"bswap_width"`,
    /// `"div_width"`, `"sse_store_width"`. Static names are greppable, stable
    /// across runs, and testable; that is the point of not formatting them. The
    /// dynamic detail (which width, which register) is recoverable from the
    /// instruction that produced the error, which every reporting site has.
    Form(&'static str),

    /// The lowered IR could not be built: Cranelift's `define_function` /
    /// `finalize_definitions` returned an error, or a lowering helper needed a
    /// module-level import the block analyzer had not declared.
    ///
    /// Internal-invariant failures are given an `"internal: …"` prefix so they
    /// remain distinguishable from Cranelift text inside the same variant —
    /// the alternative was a fourth variant, i.e. a fourth convention.
    Verifier(String),
}

impl LowerError {
    /// The JIT does not implement this instruction.
    pub(crate) const fn unsupported() -> Self {
        Self::Unsupported
    }

    /// The JIT implements the instruction but not the operand form named by
    /// `shape`.
    pub(crate) const fn form(shape: &'static str) -> Self {
        Self::Form(shape)
    }

    /// The IR could not be built or verified; `msg` is Cranelift's text, or an
    /// `"internal: …"` invariant report.
    pub(crate) fn verifier(msg: impl Into<String>) -> Self {
        Self::Verifier(msg.into())
    }

    /// A short, stable name for which of the three fired.
    ///
    /// Used as a log field and in the refusal counters; never formatted into a
    /// user-facing sentence.
    pub(crate) const fn kind(&self) -> &'static str {
        match self {
            Self::Unsupported => "unsupported-instruction",
            Self::Form(_) => "unsupported-form",
            Self::Verifier(_) => "verifier",
        }
    }
}

impl fmt::Display for LowerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => write!(f, "unsupported instruction"),
            Self::Form(shape) => write!(f, "unsupported operand form: {shape}"),
            Self::Verifier(msg) => write!(f, "verifier: {msg}"),
        }
    }
}

/// Refusals recorded per variant.
///
/// Written on the failure path only (a rejected block is compiled at most once
/// per VA, so this is nowhere near hot), and read by the refusal-report test.
/// `OPCODE_HISTO` answers *which mnemonic*; these answer *why*, which the
/// mnemonic-keyed histogram cannot express.
pub(super) static REFUSALS_UNSUPPORTED: AtomicU64 = AtomicU64::new(0);
pub(super) static REFUSALS_FORM: AtomicU64 = AtomicU64::new(0);
pub(super) static REFUSALS_VERIFIER: AtomicU64 = AtomicU64::new(0);

/// Current refusal counts as `(unsupported, form, verifier)`.
///
/// The mnemonic histogram answers *which instruction* and cannot say *why*, so
/// the report renders this split beside it: `Unsupported` is routine (an x87
/// guest) and not a defect, while a `Form` refusal means the JIT does that
/// instruction and not that operand shape. Collapsing them was the reason a
/// refusal used to be indistinguishable from a codegen bug.
pub(crate) fn refusal_counts() -> (u64, u64, u64) {
    (
        REFUSALS_UNSUPPORTED.load(Ordering::Relaxed),
        REFUSALS_FORM.load(Ordering::Relaxed),
        REFUSALS_VERIFIER.load(Ordering::Relaxed),
    )
}

/// One line naming the mnemonic, the operand form and which variant fired.
///
/// This is the reporting path the commit that introduced the unlowerable
/// histogram (`80f5960`) left as a single mnemonic bucket: a refusal used to be
/// visible only as "this mnemonic was refused", never as "we do this mnemonic
/// and not this form of it".
pub(super) fn refusal_reason(instr: &Instruction, err: &LowerError) -> String {
    let m = instr.mnemonic();
    match err {
        // The table has no row, so the decoder should never have put this
        // instruction in a block. Distinguishing this from the drift case below
        // is what keeps a hand-built block from looking like a gate bug.
        LowerError::Unsupported if !opclass::admits_mnemonic(m) => {
            format!("mnemonic {m:?} is not lowerable by this JIT")
        }
        // The table admits it and the dispatch has no arm: exactly the drift
        // ADR-0007 exists to make impossible.
        LowerError::Unsupported => {
            format!("mnemonic {m:?} is admitted by the gate but has no emitter")
        }
        LowerError::Form(shape) => format!("operand form {shape} of {m:?} is not lowerable"),
        LowerError::Verifier(msg) => format!("IR for {m:?} could not be built: {msg}"),
    }
}

/// Record one refusal: per-variant counter, the mnemonic's histogram bucket,
/// and a debug line naming the reason.
///
/// The histogram bucket is bumped here rather than at the refusal sites, so a
/// `Form` refusal — which used to be invisible, because only the dispatch's
/// catch-all arm recorded anything — now shows up in the same
/// `WIE_JIT_OPCODE_HISTO` report as an `Unsupported` one. The counters and the
/// log line are what separate the two.
pub(super) fn note_refusal(instr: &Instruction, err: &LowerError) {
    note_refusal_counted(instr, err, JitConfig::get().opcode_hist_enabled(), "lower");
}

/// [`note_refusal`] with the histogram gate injected, so the recording path is
/// testable without depending on process-wide env.
fn note_refusal_counted(instr: &Instruction, err: &LowerError, hist: bool, site: &'static str) {
    let counter = match err {
        LowerError::Unsupported => &REFUSALS_UNSUPPORTED,
        LowerError::Form(_) => &REFUSALS_FORM,
        LowerError::Verifier(_) => &REFUSALS_VERIFIER,
    };
    counter.fetch_add(1, Ordering::Relaxed);
    // Same gate, same array and same per-block count as the pre-existing
    // `lower_insn` catch-all: the refusal is recorded once per rejected block,
    // and deliberately not sampled (a rejected block is compiled at most once
    // per VA, so 1/64 sampling would hide the signal) nor counted into
    // `OPCODE_SAMPLES`, which is the iced-residue denominator.
    if hist && let Some(bucket) = OPCODE_HISTO.get(instr.mnemonic() as usize) {
        bucket.fetch_add(1, Ordering::Relaxed);
    }
    tracing::debug!(
        rip = format_args!("{:#x}", instr.ip()),
        kind = err.kind(),
        site,
        reason = %refusal_reason(instr, err),
        "jit lowering refused an instruction — the block falls back to the interpreter"
    );
}

/// Note a refusal and hand the error straight back, so call sites stay
/// `?`-shaped: `.map_err(|e| noted(&d.instr, e))?`.
pub(super) fn noted(instr: &Instruction, err: LowerError) -> LowerError {
    note_refusal(instr, &err);
    err
}

/// Why the **admission gate** refused `instr`, or `None` when it admits it.
///
/// The gate ([`opclass::admits`]) refuses before any emitter runs, so it has no
/// `LowerError` of its own to hand up — but it is the only place that can tell
/// the two classes apart, and it refuses the majority of real guest code: a
/// block that *starts* at an unlowerable instruction is `NotPure` and marked
/// `Never` without ever entering the lowerer. That made the per-variant
/// counters report zero on a guest whose refusals were almost all gate
/// refusals — the report could not see the residue it exists to rank.
///
/// The classification is the same two values the emitter path uses, derived
/// from the table rather than guessed:
/// - a `Refused` row (no implementation at all) is [`LowerError::Unsupported`];
/// - a row that exists but whose operand contract rejects this instruction is
///   [`LowerError::Form`], carrying the contract's stable name, so
///   "`movaps` store form" and "`fld`" report differently.
pub(crate) fn gate_refusal(instr: &Instruction) -> Option<LowerError> {
    let row = opclass::row_of(instr);
    if row.admits(instr) {
        return None;
    }
    Some(match row.class {
        opclass::OpClass::Gpr(form) => LowerError::Form(form.name()),
        opclass::OpClass::Sse(form) => LowerError::Form(form.name()),
        opclass::OpClass::String(form) => LowerError::Form(form.name()),
        // A terminator row whose shape `term_of` rejects (`ret imm16`, a far
        // branch) is the same class of failure as a bad operand form.
        opclass::OpClass::Term(form) => LowerError::Form(form.name()),
        // No row at all, or the always-admitting `Noop` class (unreachable:
        // `Noop` admits every encoding).
        opclass::OpClass::Noop | opclass::OpClass::Refused => LowerError::Unsupported,
    })
}

/// Record one refusal the *admission gate* made, for the same report the
/// emitter's refusals feed.
///
/// Called from `block::decode_pure_gpr_block` where a non-admitted instruction
/// ends (and therefore refuses) a block. Nothing here changes what compiles:
/// the gate's verdict is already taken, and this only makes it visible.
pub(crate) fn note_gate_refusal(instr: &Instruction) {
    if let Some(err) = gate_refusal(instr) {
        note_refusal_counted(instr, &err, JitConfig::get().opcode_hist_enabled(), "gate");
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::jit::lower::emit::MemEnv;
    use crate::jit::lower::flags::FlagState;
    use crate::jit::lower::gpr::lower_div;
    use crate::jit::lower::insn::{PendingFlags, lower_insn};
    use crate::jit::lower::mem::call_load;
    use cranelift::prelude::*;
    use cranelift_codegen::ir::{Function, MemFlagsData};
    use iced_x86::{Decoder, DecoderOptions, Mnemonic};

    fn decode(bytes: &[u8]) -> Instruction {
        let mut buf = [0_u8; 15];
        let n = bytes.len().min(15);
        buf[..n].copy_from_slice(&bytes[..n]);
        let mut dec = Decoder::with_ip(64, &buf, 0x1000, DecoderOptions::NONE);
        let instr = dec.decode();
        assert!(!instr.is_invalid() && instr.len() != 0, "{bytes:02X?}");
        instr
    }

    /// A minimal lowering environment: everything a single helper needs, with
    /// every optional import absent. Mirrors what `compile_block` sets up, minus
    /// the parts only a whole block uses.
    fn with_env<R>(f: impl FnOnce(&mut FunctionBuilder<'_>, &mut MemEnv) -> R) -> R {
        let mut func = Function::new();
        func.signature.params.push(AbiParam::new(types::I64));
        let mut fctx = FunctionBuilderContext::new();
        let mut bcx = FunctionBuilder::new(&mut func, &mut fctx);
        let entry = bcx.create_block();
        bcx.append_block_params_for_function_params(entry);
        bcx.switch_to_block(entry);
        bcx.seal_block(entry);
        let ctx_ptr = bcx.block_params(entry)[0];
        let exit = bcx.create_block();
        let flags = MemFlagsData::trusted();
        let mut env = MemEnv {
            ctx_ptr,
            load_ref: None,
            store_ref: None,
            string_ref: None,
            host_span_ref: None,
            f32_ref: None,
            f64_ref: None,
            flags,
            guest_flags: flags,
            exit,
            ucrt_refs: [None; 7],
            sse_int_ref: None,
            sse_shift_ref: None,
            sse_pshufb_lo_ref: None,
            sse_pshufb_hi_ref: None,
            sse_fp_unop_ref: None,
            sse_fp_binop_ref: None,
            sse_cvt_ref: None,
            div64_ref: None,
            stack_pin: None,
            data_pins: Vec::new(),
            refill: None,
            super_stack: None,
            fault_flags: None,
            fault_pending: PendingFlags::None,
        };
        f(&mut bcx, &mut env)
    }

    /// `Unsupported` is what the dispatch produces for a mnemonic it has no arm
    /// for. `ud2` is the simplest such instruction and is also refused by the
    /// gate, so the two refusals agree about it.
    #[test]
    fn unsupported_is_reachable_from_the_dispatch() {
        let instr = decode(&[0x0F, 0x0B]); // ud2
        assert!(!opclass::admits_mnemonic(Mnemonic::Ud2));
        let err = with_env(|bcx, mem| {
            let mut gpr = [mem.ctx_ptr; 16];
            let mut dirty = [false; 16];
            let mut rflags = mem.ctx_ptr;
            let mut pending = PendingFlags::None;
            let mut flag_state: Option<FlagState> = None;
            let mut xmm = [mem.ctx_ptr; 32];
            lower_insn(
                bcx,
                &instr,
                &mut gpr,
                &mut dirty,
                &mut rflags,
                &mut pending,
                &mut flag_state,
                mem,
                &mut xmm,
            )
        });
        assert_eq!(err, Err(LowerError::Unsupported));
    }

    /// `Form` is reachable from real guest code, not only from a synthetic call:
    /// the gate admits `div word [rbx]` (the memory form has no size check) and
    /// `lower_div` refuses everything below 32 bits. That pair is exactly the
    /// class this variant exists to name, and it was silent before.
    #[test]
    fn form_is_reachable_for_a_gate_admitted_instruction() {
        let instr = decode(&[0x66, 0xF7, 0x33]); // div word [rbx]
        assert_eq!(instr.mnemonic(), Mnemonic::Div);
        assert!(
            opclass::admits(&instr),
            "the gate admits this form — an admitted-but-unlowerable pair is the point"
        );
        let err = with_env(|bcx, mem| {
            let mut gpr = [mem.ctx_ptr; 16];
            let mut dirty = [false; 16];
            let mut rflags = mem.ctx_ptr;
            lower_div(bcx, &instr, &mut gpr, &mut dirty, &mut rflags, mem)
        });
        assert_eq!(err, Err(LowerError::Form("div_width")));
    }

    /// `Verifier` is reachable whenever a helper import the block analyzer
    /// promised is absent — here a load in a block that never declared one.
    #[test]
    fn verifier_is_reachable_when_a_required_helper_is_absent() {
        let instr = decode(&[0x8B, 0x03]); // mov eax,[rbx]
        let err = with_env(|bcx, mem| {
            let gpr = [mem.ctx_ptr; 16];
            let addr = mem.ctx_ptr;
            call_load(bcx, mem, &gpr, mem.ctx_ptr, addr, 4, instr.ip())
        });
        match err {
            Err(LowerError::Verifier(msg)) => {
                assert!(msg.starts_with("internal: "), "{msg}");
            }
            other => panic!("expected a verifier error, got {other:?}"),
        }
    }

    /// The reporting path names which variant fired, and keeps the
    /// pre-existing "not lowerable" / "admitted but no emitter" split so a
    /// hand-built block does not read like a gate bug.
    #[test]
    fn the_report_names_which_variant_fired() {
        let admitted = decode(&[0x66, 0xF7, 0x33]); // div word [rbx] is admitted
        let refused = decode(&[0x0F, 0x0B]); // ud2 is not

        let unsupported = LowerError::Unsupported;
        let form = LowerError::Form("div_width");
        let verifier = LowerError::Verifier("internal: test".to_string());

        let drift = refusal_reason(&admitted, &unsupported);
        assert!(
            drift.contains("admitted by the gate but has no emitter"),
            "{drift}"
        );
        let absent = refusal_reason(&refused, &unsupported);
        assert!(absent.contains("not lowerable by this JIT"), "{absent}");

        let form_line = refusal_reason(&admitted, &form);
        assert!(
            form_line.contains("div_width") && form_line.contains("Div"),
            "{form_line}"
        );
        let verifier_line = refusal_reason(&admitted, &verifier);
        assert!(verifier_line.contains("internal: test"), "{verifier_line}");

        // The three reasons are mutually distinguishable by name and by text.
        assert_ne!(unsupported.kind(), form.kind());
        assert_ne!(form.kind(), verifier.kind());
        assert!(unsupported.to_string().contains("instruction"));
        assert!(form.to_string().contains("operand form"));
        assert!(verifier.to_string().contains("verifier"));
        assert_ne!(drift, form_line);
    }

    /// Both variants reach the same mnemonic bucket — so a `Form` refusal now
    /// appears in the `WIE_JIT_OPCODE_HISTO` report, which it never did before —
    /// while the counters keep them apart.
    #[test]
    fn refusals_are_counted_per_variant_and_bucket_the_mnemonic() {
        let instr = decode(&[0x66, 0xF7, 0x33]);
        let bucket = instr.mnemonic() as usize;
        let before = (
            REFUSALS_UNSUPPORTED.load(Ordering::Relaxed),
            REFUSALS_FORM.load(Ordering::Relaxed),
            REFUSALS_VERIFIER.load(Ordering::Relaxed),
            OPCODE_HISTO.get(bucket).map(|b| b.load(Ordering::Relaxed)),
        );

        note_refusal_counted(&instr, &LowerError::Unsupported, true, "lower");
        note_refusal_counted(&instr, &LowerError::Form("div_width"), true, "lower");
        note_refusal_counted(
            &instr,
            &LowerError::Verifier("internal: t".to_string()),
            true,
            "lower",
        );

        assert_eq!(REFUSALS_UNSUPPORTED.load(Ordering::Relaxed), before.0 + 1);
        assert_eq!(REFUSALS_FORM.load(Ordering::Relaxed), before.1 + 1);
        assert_eq!(REFUSALS_VERIFIER.load(Ordering::Relaxed), before.2 + 1);

        let after = OPCODE_HISTO.get(bucket).map(|b| b.load(Ordering::Relaxed));
        if let (Some(before), Some(after)) = (before.3, after) {
            assert!(
                after >= before + 3,
                "every refusal kind must reach the mnemonic bucket: {before} -> {after}"
            );
        }

        // The gate-off case must not touch the histogram (lazy 2048-slot
        // allocation) but must still count.
        let untouched = OPCODE_HISTO.get(bucket).map(|b| b.load(Ordering::Relaxed));
        note_refusal_counted(&instr, &LowerError::Form("div_width"), false, "lower");
        assert_eq!(
            OPCODE_HISTO.get(bucket).map(|b| b.load(Ordering::Relaxed)),
            untouched
        );
        assert_eq!(REFUSALS_FORM.load(Ordering::Relaxed), before.1 + 2);
    }

    /// The admission gate refuses *before* any emitter runs, so its verdict
    /// never became a `LowerError` and the per-variant counters read zero on a
    /// guest whose refusals were almost all gate refusals. The gate now
    /// classifies through the same table: no row is `Unsupported`, a row whose
    /// operand contract rejects this encoding is `Form`.
    #[test]
    fn gate_refusals_use_the_same_two_variants_as_the_emitter() {
        // `ud2` has no row: the JIT does not implement the instruction.
        let absent = decode(&[0x0F, 0x0B]);
        assert_eq!(gate_refusal(&absent), Some(LowerError::Unsupported));

        // `push ax`: the JIT implements `push` and refuses the 16-bit form.
        let wrong_form = decode(&[0x66, 0x50]);
        assert_eq!(wrong_form.mnemonic(), Mnemonic::Push);
        assert!(opclass::admits_mnemonic(Mnemonic::Push));
        match gate_refusal(&wrong_form) {
            Some(LowerError::Form(shape)) => assert_eq!(shape, "gpr_push"),
            other => panic!("expected a form refusal, got {other:?}"),
        }

        // An admitted instruction is not a refusal at all.
        assert_eq!(gate_refusal(&decode(&[0x8B, 0x03])), None);
    }

    /// Reachability of both variants *through the gate reporting path*, which
    /// is what the refusal report in `jit::diag` renders.
    #[test]
    fn the_gate_path_reaches_the_variant_counters_and_names_the_reason() {
        let absent = decode(&[0x0F, 0x0B]);
        let wrong_form = decode(&[0x66, 0x50]);
        let before = refusal_counts();

        note_gate_refusal(&absent);
        note_gate_refusal(&wrong_form);

        let after = refusal_counts();
        assert_eq!(after.0, before.0 + 1, "one unsupported gate refusal");
        assert_eq!(after.1, before.1 + 1, "one form gate refusal");
        assert_eq!(
            after.2, before.2,
            "the gate cannot produce a verifier error"
        );

        let reason = refusal_reason(&wrong_form, &LowerError::Form("gpr_push"));
        assert!(
            reason.contains("gpr_push") && reason.contains("Push"),
            "{reason}"
        );
    }
}
