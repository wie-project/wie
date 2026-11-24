//! `JitEngine` construction: one Cranelift `JITModule` per opt-level tier,
//! each with its own import declarations.
//!
//! # Why one module per tier, not one module with two opt levels
//!
//! `cranelift_jit::JITModule` binds its `TargetIsa` — and therefore its
//! `opt_level` — in a **private field with no setter** (checked against
//! cranelift-jit 0.133.1: `pub struct JITModule { isa: OwnedTargetIsa, .. }`,
//! and `JITBuilder::with_isa` is construction-only). So a single module can
//! only ever compile at ONE opt level, and per-block tier-up needs one module
//! per level. `TargetIsa::flags()` is a dead end for a *live* module: mutating
//! the flags behind an `Arc` would require reaching into that private field,
//! i.e. a layout-dependent `unsafe` transmute of a `#[repr(Rust)]` struct with
//! no API guarantee. Two modules cost one extra `JITModule` allocation and keep
//! the whole thing on public API.
//!
//! The cost of two modules is that a `cranelift_module::FuncId` is only
//! meaningful **inside its declaring module** (`Module::declare_func_in_func`
//! indexes that module's own `compiled_functions`). That invariant is enforced
//! by tier-tagging `JitShared::chain_ids` and by compiling against a
//! same-tier-only snapshot — see [`super::shared::JitShared::chain_map_for`].
//!
//! Fields are `pub(super)` because `lower.rs` mutates [`IsaEngine`] fields
//! directly during block compilation (visible across `crate::jit`).

#![allow(
    unsafe_code, // Cranelift finalized fn pointers + host mem helpers
    private_interfaces // JitShared/PerThreadJitState expose crate-private types
)]

use super::UcrtImportIds;
use super::config::JitConfig;
use super::fast_api::{
    wie_ucrt_fflush, wie_ucrt_free, wie_ucrt_fwrite, wie_ucrt_iob, wie_ucrt_malloc,
    wie_ucrt_memcpy, wie_ucrt_strlen,
};
use super::lower::{
    wie_div64, wie_f32_binop, wie_f64_binop, wie_jit_chain_lookup, wie_jit_host_span, wie_jit_load,
    wie_jit_store, wie_jit_string, wie_sse_cvt, wie_sse_fp_binop, wie_sse_fp_unop,
    wie_sse_int_binop, wie_sse_pshufb_hi, wie_sse_pshufb_lo, wie_sse_shift,
};
use super::tier::{OptTier, TIER_OPT_LEVEL};

/// Persisted-machine-code format, guards and counters (`WIE_JIT_CODE_CACHE`).
///
/// Declared here rather than in [`super::mod`] so the code cache can reach the
/// per-tier module internals it restores into without widening any module's
/// visibility past `crate::jit`.
mod code_cache;

#[allow(unused_imports)] // wired up by the JitShared plumbing in a follow-up edit
pub(crate) use code_cache::{CodeCache, CodeCacheCounts};
pub(super) use code_cache::{CodeRestoreOutcome, PersistedCode};

use cranelift_module::{FuncId, Module, ModuleDeclarations, ModuleReloc};

/// Pre-relocation emit artifacts of one just-defined function, lifted off the
/// `CompiledCode` before `cranelift-jit` patches host addresses into it.
///
/// This is the *only* point in the JIT where the emitted block exists in a
/// relocatable form. After `finalize_definitions()` the code carries this
/// process's absolute addresses and is useless as a cross-run artifact, so the
/// capture has to happen inside the define (see [`CapturingModule`]).
pub(crate) struct CapturedCode {
    /// The function this belongs to, valid in the capturing module.
    pub(super) func_id: FuncId,
    /// Code alignment cranelift-jit used for the blob; replayed verbatim so a
    /// restored function is placed exactly like a compiled one.
    pub(super) align: u64,
    /// Unrelocated machine code.
    pub(super) bytes: Vec<u8>,
    /// Relocations for [`Self::bytes`], in cranelift's own order.
    pub(super) relocs: Vec<ModuleReloc>,
}

/// A [`cranelift_jit::JITModule`] that keeps a copy of every function's
/// pre-relocation bytes, so the on-disk code cache has something to persist.
///
/// # Why this wrapper exists
///
/// `cranelift-jit` applies relocations *inside*
/// `define_function_with_control_plane` and keeps the patched bytes private.
/// `Module::define_function_bytes` is the public inverse — it takes bytes plus
/// relocs and defines a function from them — but nothing exposes the input side
/// of the first call. Overriding the one trait method that performs the define
/// is therefore the whole interception seam; no change to the lowering path is
/// needed, because `Module::define_function` (what [`super::lower`] calls)
/// forwards to `define_function_with_control_plane`.
///
/// Everything else is reached through [`Deref`], so the lowering path's calls
/// (`declare_func_in_func`, `declare_anonymous_function`,
/// `finalize_definitions`, `get_finalized_function`, `clear_context`) keep
/// resolving to the inner `JITModule` unchanged.
pub(crate) struct CapturingModule {
    inner: cranelift_jit::JITModule,
    /// Code emitted by the most recent successful define, not yet drained.
    ///
    /// One slot is enough because every compile holds `JitShared::engine`
    /// exclusively for the whole define → finalize → drain sequence.
    captured: Option<CapturedCode>,
}

impl std::ops::Deref for CapturingModule {
    type Target = cranelift_jit::JITModule;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::ops::DerefMut for CapturingModule {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl CapturingModule {
    fn new(inner: cranelift_jit::JITModule) -> Self {
        Self {
            inner,
            captured: None,
        }
    }

    /// Take the code emitted for `func_id`, if that is what was captured last.
    ///
    /// Any capture belonging to a different function is **discarded, not
    /// returned**: a stale capture paired with the wrong `FuncId` would attach
    /// one block's code to another's record, which is exactly the kind of silent
    /// miscompile the cache exists to avoid.
    pub(super) fn take_captured(&mut self, func_id: FuncId) -> Option<CapturedCode> {
        match &self.captured {
            Some(c) if c.func_id == func_id => self.captured.take(),
            Some(_) => {
                self.captured = None;
                None
            }
            None => None,
        }
    }
}

impl cranelift_module::Module for CapturingModule {
    fn isa(&self) -> &dyn cranelift_codegen::isa::TargetIsa {
        cranelift_module::Module::isa(&self.inner)
    }

    fn declarations(&self) -> &ModuleDeclarations {
        cranelift_module::Module::declarations(&self.inner)
    }

    fn declare_function(
        &mut self,
        name: &str,
        linkage: cranelift_module::Linkage,
        signature: &cranelift_codegen::ir::Signature,
    ) -> cranelift_module::ModuleResult<FuncId> {
        cranelift_module::Module::declare_function(&mut self.inner, name, linkage, signature)
    }

    fn declare_anonymous_function(
        &mut self,
        signature: &cranelift_codegen::ir::Signature,
    ) -> cranelift_module::ModuleResult<FuncId> {
        cranelift_module::Module::declare_anonymous_function(&mut self.inner, signature)
    }

    /// Explicitly forwarded rather than relying on the trait's default.
    ///
    /// The default already routes through
    /// [`Self::define_function_with_control_plane`] (and therefore through the
    /// capture), but the capture is load-bearing enough that depending on
    /// method-resolution order between `DerefMut` and this trait is not a
    /// trade worth making: if `DerefMut` ever won, every blob would silently
    /// come back empty and the cache would look like a 0% hit rate.
    fn define_function(
        &mut self,
        func: FuncId,
        ctx: &mut cranelift_codegen::Context,
    ) -> cranelift_module::ModuleResult<()> {
        let mut cp = cranelift_codegen::control::ControlPlane::default();
        self.define_function_with_control_plane(func, ctx, &mut cp)
    }

    /// Capture, then delegate. Capturing *after* the inner define is safe
    /// because `Module::define_function_with_control_plane` documents that the
    /// `Context` holds the compiled function on return — and `CompiledCode`'s
    /// buffer is the un-relocated one; `JITModule` patches a *copy* inside the
    /// blob it allocates.
    fn define_function_with_control_plane(
        &mut self,
        func: FuncId,
        ctx: &mut cranelift_codegen::Context,
        ctrl_plane: &mut cranelift_codegen::control::ControlPlane,
    ) -> cranelift_module::ModuleResult<()> {
        let result = cranelift_module::Module::define_function_with_control_plane(
            &mut self.inner,
            func,
            ctx,
            ctrl_plane,
        );
        // A failed define leaves the context without a `CompiledCode`; keep
        // whatever was captured before rather than a half-updated slot.
        if let Err(e) = result {
            return Err(e);
        }
        self.captured = ctx.compiled_code().map(|cc| CapturedCode {
            func_id: func,
            align: (cc.buffer.alignment as u64)
                .max(self.inner.isa().function_alignment().minimum as u64)
                .max(self.inner.isa().symbol_alignment()),
            bytes: cc.code_buffer().to_vec(),
            relocs: cc
                .buffer
                .relocs()
                .iter()
                .map(|r| ModuleReloc::from_mach_reloc(r, &ctx.func, func))
                .collect(),
        });
        result
    }

    fn define_function_bytes(
        &mut self,
        func_id: FuncId,
        alignment: u64,
        bytes: &[u8],
        relocs: &[ModuleReloc],
    ) -> cranelift_module::ModuleResult<()> {
        cranelift_module::Module::define_function_bytes(
            &mut self.inner,
            func_id,
            alignment,
            bytes,
            relocs,
        )
    }

    fn define_data(
        &mut self,
        data_id: cranelift_module::DataId,
        data: &cranelift_module::DataDescription,
    ) -> cranelift_module::ModuleResult<()> {
        cranelift_module::Module::define_data(&mut self.inner, data_id, data)
    }

    fn declare_data(
        &mut self,
        name: &str,
        linkage: cranelift_module::Linkage,
        writable: bool,
        tls: bool,
    ) -> cranelift_module::ModuleResult<cranelift_module::DataId> {
        cranelift_module::Module::declare_data(&mut self.inner, name, linkage, writable, tls)
    }

    fn declare_anonymous_data(
        &mut self,
        writable: bool,
        tls: bool,
    ) -> cranelift_module::ModuleResult<cranelift_module::DataId> {
        cranelift_module::Module::declare_anonymous_data(&mut self.inner, writable, tls)
    }
}

/// The per-tier Cranelift machinery: one `JITModule`, one compile `Context`,
/// and that module's own import `FuncId`s.
pub(crate) struct IsaEngine {
    pub(super) module: CapturingModule,
    pub(super) ctx: cranelift_codegen::Context,
    pub(super) func_ctx: cranelift::prelude::FunctionBuilderContext,
    /// Shared signature: `(i64 ctx_ptr)` — host C ABI (callable from Rust).
    pub(super) block_sig: cranelift::codegen::ir::Signature,
    /// Host `wie_jit_load` import.
    pub(super) load_id: cranelift_module::FuncId,
    /// Host `wie_jit_store` import.
    pub(super) store_id: cranelift_module::FuncId,
    /// Host bulk string helper.
    pub(super) string_id: cranelift_module::FuncId,
    /// Soft-translated host span for inline string copies.
    pub(super) host_span_id: cranelift_module::FuncId,
    /// Scalar f32 binop helper.
    pub(super) f32_id: cranelift_module::FuncId,
    /// Scalar f64 binop helper.
    pub(super) f64_id: cranelift_module::FuncId,
    /// Packed integer SSE2 lane op helper (SIMD-off path + pack/pmul*).
    pub(super) sse_int_id: cranelift_module::FuncId,
    pub(super) div64_id: cranelift_module::FuncId,
    /// Packed SSE2 shift helper (imm + variable count).
    pub(super) sse_shift_id: cranelift_module::FuncId,
    /// `pshufb` result low half (full 16-byte table + mask).
    pub(super) sse_pshufb_lo_id: cranelift_module::FuncId,
    /// `pshufb` result high half.
    pub(super) sse_pshufb_hi_id: cranelift_module::FuncId,
    /// FP unary (sqrt) helper.
    pub(super) sse_fp_unop_id: cranelift_module::FuncId,
    /// FP min/max helper.
    pub(super) sse_fp_binop_id: cranelift_module::FuncId,
    /// Integer↔FP convert helper.
    pub(super) sse_cvt_id: cranelift_module::FuncId,
    /// Host chain-table lookup (`wie_jit_chain_lookup`).
    pub(super) lookup_id: cranelift_module::FuncId,
    /// UCRT fast-path imports (malloc, free, memcpy, …).
    pub(super) ucrt: UcrtImportIds,
}

/// The engine: a base-tier module plus, when tiering is armed, a tier-up
/// module compiled at [`TIER_OPT_LEVEL`].
pub(crate) struct JitEngine {
    /// Compiles at `WIE_JIT_OPT` (default `none`). Present iff the JIT works
    /// at all.
    pub(super) base: IsaEngine,
    /// Compiles at [`TIER_OPT_LEVEL`], used only by blocks that earn the tier
    /// (self-loops). `None` when tiering is off, when the budget is zero, or
    /// when the two levels coincide — in all three cases every compile resolves
    /// to `OptTier::Base` and this is exactly the pre-tiering behaviour.
    pub(super) tier: Option<IsaEngine>,
}

impl JitEngine {
    pub(super) fn new() -> Result<Self, String> {
        let base = IsaEngine::new(OptTier::Base.opt_level())?;
        // A tier module is only worth building when it can actually be used:
        // the knob is on, the budget allows at least one tier-up, and the two
        // opt levels differ (otherwise tiering is inert — every decision would
        // resolve to the base level the process already uses).
        let armed = JitConfig::get().tier_enabled() && JitConfig::get().tier_budget() > 0;
        let tier = if armed && OptTier::Base.opt_level() != TIER_OPT_LEVEL {
            match IsaEngine::new(TIER_OPT_LEVEL) {
                Ok(e) => Some(e),
                // Losing the tier module must NOT cost us the whole JIT: base
                // compiles are still correct, just unoptimized.
                Err(e) => {
                    tracing::warn!(error = %e, "jit tier module unavailable; tier-up disabled");
                    None
                }
            }
        } else {
            None
        };
        Ok(Self { base, tier })
    }

    /// The module that compiles at `tier`. `None` only if a tier-up was
    /// decided without a tier module existing — which [`Self::new`] makes
    /// impossible, and which callers treat as "fall back to interpretation"
    /// rather than as a silent retarget.
    pub(super) fn module_for(&mut self, tier: OptTier) -> Option<&mut IsaEngine> {
        match tier {
            OptTier::Base => Some(&mut self.base),
            OptTier::Speed => self.tier.as_mut(),
        }
    }

    /// Whether a tier-up module exists (i.e. tiering is armed at all).
    pub(super) fn has_tier_module(&self) -> bool {
        self.tier.is_some()
    }

    /// Mutable handle on the base module, for identity queries that do not need
    /// to know which tier owns the answer.
    pub(super) fn base_mut(&mut self) -> &mut IsaEngine {
        &mut self.base
    }
}

impl IsaEngine {
    /// Build one module at `opt_level`. Everything except the opt level is
    /// identical between tiers, so the emitted code for the same block differs
    /// only in optimisation strength.
    pub(super) fn new(opt_level: &'static str) -> Result<Self, String> {
        use cranelift::prelude::*;
        use cranelift_codegen::settings::Configurable;
        use cranelift_jit::{JITBuilder, JITModule};
        use cranelift_module::{Linkage, Module, default_libcall_names};

        let mut flag_builder = settings::builder();
        // Per-module: one module per opt level (see the module docs).
        flag_builder
            .set("opt_level", opt_level)
            .map_err(|e| e.to_string())?;
        // Cranelift IR verifier (`WIE_JIT_VERIFIER`; default
        // `cfg!(debug_assertions)` — on in debug, off in release). It runs per
        // compiled function and checks CLIF SSA/dominance, types, use-before-def
        // and operand constraints, so it is the only thing that turns a
        // lowering bug into a diagnosable error instead of wrong host code. It
        // never checked x86 *semantics* — the JIT-vs-iced differential is that
        // gate. Profile default: dev-profile `nextest` compiles with it on
        // (CI keeps the guard) while release guests do not pay the measured
        // -37% boot cost; `WIE_JIT_VERIFIER=1` re-arms it in a release build.
        // Nothing in the repo records it ever firing, so the release default
        // means "no known bug is masked", not "it is redundant".
        flag_builder
            .set(
                "enable_verifier",
                if JitConfig::get().verifier_enabled() {
                    "true"
                } else {
                    "false"
                },
            )
            .map_err(|e| e.to_string())?;
        flag_builder
            .set("is_pic", "false")
            .map_err(|e| e.to_string())?;
        flag_builder
            .set("use_colocated_libcalls", "false")
            .map_err(|e| e.to_string())?;
        flag_builder
            .set("enable_probestack", "false")
            .map_err(|e| e.to_string())?;
        // Guest frames are not host-unwound; skip metadata tax.
        flag_builder
            .set("unwind_info", "false")
            .map_err(|e| e.to_string())?;
        // Not a Wasm sandbox heap — soft-translate already bounds guest accesses.
        flag_builder
            .set("enable_heap_access_spectre_mitigation", "false")
            .map_err(|e| e.to_string())?;

        let mut isa_builder =
            cranelift_native::builder().map_err(|msg| format!("host ISA unsupported: {msg}"))?;
        // Apple Silicon: cranelift_native already enables LSE/PAC/FP16 + macOS PAC B-key.
        // Re-assert PAC signing so JIT call/return stays ABI-consistent if detect fails.
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            isa_builder
                .enable("sign_return_address")
                .map_err(|e| e.to_string())?;
            isa_builder
                .enable("sign_return_address_with_bkey")
                .map_err(|e| e.to_string())?;
            isa_builder.enable("has_pauth").map_err(|e| e.to_string())?;
        }
        let isa = isa_builder
            .finish(settings::Flags::new(flag_builder))
            .map_err(|e| e.to_string())?;

        let mut builder = JITBuilder::with_isa(isa, default_libcall_names());
        // SAFETY: function pointers are valid for the process lifetime.
        builder.symbol("wie_jit_load", wie_jit_load as *const u8);
        builder.symbol("wie_jit_store", wie_jit_store as *const u8);
        builder.symbol("wie_jit_string", wie_jit_string as *const u8);
        builder.symbol("wie_jit_host_span", wie_jit_host_span as *const u8);
        builder.symbol("wie_f32_binop", wie_f32_binop as *const u8);
        builder.symbol("wie_f64_binop", wie_f64_binop as *const u8);
        builder.symbol("wie_sse_int_binop", wie_sse_int_binop as *const u8);
        builder.symbol("wie_div64", wie_div64 as *const u8);
        builder.symbol("wie_sse_shift", wie_sse_shift as *const u8);
        builder.symbol("wie_sse_pshufb_lo", wie_sse_pshufb_lo as *const u8);
        builder.symbol("wie_sse_pshufb_hi", wie_sse_pshufb_hi as *const u8);
        builder.symbol("wie_sse_fp_unop", wie_sse_fp_unop as *const u8);
        builder.symbol("wie_sse_fp_binop", wie_sse_fp_binop as *const u8);
        builder.symbol("wie_sse_cvt", wie_sse_cvt as *const u8);
        builder.symbol("wie_jit_chain_lookup", wie_jit_chain_lookup as *const u8);
        builder.symbol("wie_ucrt_malloc", wie_ucrt_malloc as *const u8);
        builder.symbol("wie_ucrt_free", wie_ucrt_free as *const u8);
        builder.symbol("wie_ucrt_memcpy", wie_ucrt_memcpy as *const u8);
        builder.symbol("wie_ucrt_strlen", wie_ucrt_strlen as *const u8);
        builder.symbol("wie_ucrt_iob", wie_ucrt_iob as *const u8);
        builder.symbol("wie_ucrt_fwrite", wie_ucrt_fwrite as *const u8);
        builder.symbol("wie_ucrt_fflush", wie_ucrt_fflush as *const u8);
        let mut module = JITModule::new(builder);

        // Host default call-conv (AppleAarch64 / SystemV) — must match Rust `extern "C"`.
        let mut block_sig = module.make_signature();
        block_sig.params.push(AbiParam::new(types::I64));

        // load: (ctx, addr, size, insn_ip) -> i64
        let mut load_sig = module.make_signature();
        load_sig.params.push(AbiParam::new(types::I64));
        load_sig.params.push(AbiParam::new(types::I64));
        load_sig.params.push(AbiParam::new(types::I64));
        load_sig.params.push(AbiParam::new(types::I64));
        load_sig.returns.push(AbiParam::new(types::I64));
        let load_id = module
            .declare_function("wie_jit_load", Linkage::Import, &load_sig)
            .map_err(|e| e.to_string())?;

        // store: (ctx, addr, size, value, insn_ip)
        let mut store_sig = module.make_signature();
        store_sig.params.push(AbiParam::new(types::I64));
        store_sig.params.push(AbiParam::new(types::I64));
        store_sig.params.push(AbiParam::new(types::I64));
        store_sig.params.push(AbiParam::new(types::I64));
        store_sig.params.push(AbiParam::new(types::I64));
        let store_id = module
            .declare_function("wie_jit_store", Linkage::Import, &store_sig)
            .map_err(|e| e.to_string())?;

        // string: (ctx, op, size, flags, insn_ip) -> stay
        let mut string_sig = module.make_signature();
        string_sig.params.push(AbiParam::new(types::I64));
        string_sig.params.push(AbiParam::new(types::I64));
        string_sig.params.push(AbiParam::new(types::I64));
        string_sig.params.push(AbiParam::new(types::I64));
        string_sig.params.push(AbiParam::new(types::I64));
        string_sig.returns.push(AbiParam::new(types::I64));
        let string_id = module
            .declare_function("wie_jit_string", Linkage::Import, &string_sig)
            .map_err(|e| e.to_string())?;

        // host_span: (ctx, guest_va, len, write) -> host_ptr_or_0
        let mut span_sig = module.make_signature();
        span_sig.params.push(AbiParam::new(types::I64));
        span_sig.params.push(AbiParam::new(types::I64));
        span_sig.params.push(AbiParam::new(types::I64));
        span_sig.params.push(AbiParam::new(types::I64));
        span_sig.returns.push(AbiParam::new(types::I64));
        let host_span_id = module
            .declare_function("wie_jit_host_span", Linkage::Import, &span_sig)
            .map_err(|e| e.to_string())?;

        // f32/f64 binop: (op, a, b) -> r
        let mut f_sig = module.make_signature();
        f_sig.params.push(AbiParam::new(types::I64));
        f_sig.params.push(AbiParam::new(types::I64));
        f_sig.params.push(AbiParam::new(types::I64));
        f_sig.returns.push(AbiParam::new(types::I64));
        let f32_id = module
            .declare_function("wie_f32_binop", Linkage::Import, &f_sig)
            .map_err(|e| e.to_string())?;
        let f64_id = module
            .declare_function("wie_f64_binop", Linkage::Import, &f_sig)
            .map_err(|e| e.to_string())?;

        // sse int/shift/fp-minmax binop: (op, a, b) -> r — same shape as f_sig.
        let sse_int_id = module
            .declare_function("wie_sse_int_binop", Linkage::Import, &f_sig)
            .map_err(|e| e.to_string())?;
        // 64-bit DIV/IDIV: (op, hi, lo, divisor) -> (r << 64) | q.
        let mut div64_sig = module.make_signature();
        for _ in 0..4 {
            div64_sig.params.push(AbiParam::new(types::I64));
        }
        div64_sig.returns.push(AbiParam::new(types::I64));
        let div64_id = module
            .declare_function("wie_div64", Linkage::Import, &div64_sig)
            .map_err(|e| e.to_string())?;
        let sse_shift_id = module
            .declare_function("wie_sse_shift", Linkage::Import, &f_sig)
            .map_err(|e| e.to_string())?;
        let sse_fp_binop_id = module
            .declare_function("wie_sse_fp_binop", Linkage::Import, &f_sig)
            .map_err(|e| e.to_string())?;

        // fp unop / cvt: (op, a) -> r.
        let mut sse2_sig = module.make_signature();
        sse2_sig.params.push(AbiParam::new(types::I64));
        sse2_sig.params.push(AbiParam::new(types::I64));
        sse2_sig.returns.push(AbiParam::new(types::I64));
        let sse_fp_unop_id = module
            .declare_function("wie_sse_fp_unop", Linkage::Import, &sse2_sig)
            .map_err(|e| e.to_string())?;
        let sse_cvt_id = module
            .declare_function("wie_sse_cvt", Linkage::Import, &sse2_sig)
            .map_err(|e| e.to_string())?;

        // pshufb halves: (a_lo, a_hi, b_lo, b_hi) -> r.
        let mut sse4_sig = module.make_signature();
        for _ in 0..4 {
            sse4_sig.params.push(AbiParam::new(types::I64));
        }
        sse4_sig.returns.push(AbiParam::new(types::I64));
        let sse_pshufb_lo_id = module
            .declare_function("wie_sse_pshufb_lo", Linkage::Import, &sse4_sig)
            .map_err(|e| e.to_string())?;
        let sse_pshufb_hi_id = module
            .declare_function("wie_sse_pshufb_hi", Linkage::Import, &sse4_sig)
            .map_err(|e| e.to_string())?;

        // chain lookup: (ctx, va) -> fn_ptr
        let mut lookup_sig = module.make_signature();
        lookup_sig.params.push(AbiParam::new(types::I64));
        lookup_sig.params.push(AbiParam::new(types::I64));
        lookup_sig.returns.push(AbiParam::new(types::I64));
        let lookup_id = module
            .declare_function("wie_jit_chain_lookup", Linkage::Import, &lookup_sig)
            .map_err(|e| e.to_string())?;

        // UCRT: (ctx, …args) -> rax  /  free is void
        let mut sig_ctx1 = module.make_signature();
        sig_ctx1.params.push(AbiParam::new(types::I64)); // ctx
        sig_ctx1.params.push(AbiParam::new(types::I64)); // a0
        sig_ctx1.returns.push(AbiParam::new(types::I64));
        let mut sig_ctx1_void = module.make_signature();
        sig_ctx1_void.params.push(AbiParam::new(types::I64));
        sig_ctx1_void.params.push(AbiParam::new(types::I64));
        let mut sig_ctx3 = module.make_signature();
        sig_ctx3.params.push(AbiParam::new(types::I64));
        sig_ctx3.params.push(AbiParam::new(types::I64));
        sig_ctx3.params.push(AbiParam::new(types::I64));
        sig_ctx3.params.push(AbiParam::new(types::I64));
        sig_ctx3.returns.push(AbiParam::new(types::I64));
        let mut sig_ctx4 = module.make_signature();
        sig_ctx4.params.push(AbiParam::new(types::I64));
        for _ in 0..4 {
            sig_ctx4.params.push(AbiParam::new(types::I64));
        }
        sig_ctx4.returns.push(AbiParam::new(types::I64));
        let mut sig_1 = module.make_signature();
        sig_1.params.push(AbiParam::new(types::I64));
        sig_1.returns.push(AbiParam::new(types::I64));

        let malloc = module
            .declare_function("wie_ucrt_malloc", Linkage::Import, &sig_ctx1)
            .map_err(|e| e.to_string())?;
        let free = module
            .declare_function("wie_ucrt_free", Linkage::Import, &sig_ctx1_void)
            .map_err(|e| e.to_string())?;
        let memcpy = module
            .declare_function("wie_ucrt_memcpy", Linkage::Import, &sig_ctx3)
            .map_err(|e| e.to_string())?;
        let strlen = module
            .declare_function("wie_ucrt_strlen", Linkage::Import, &sig_ctx1)
            .map_err(|e| e.to_string())?;
        let iob = module
            .declare_function("wie_ucrt_iob", Linkage::Import, &sig_1)
            .map_err(|e| e.to_string())?;
        let fwrite = module
            .declare_function("wie_ucrt_fwrite", Linkage::Import, &sig_ctx4)
            .map_err(|e| e.to_string())?;
        let fflush = module
            .declare_function("wie_ucrt_fflush", Linkage::Import, &sig_1)
            .map_err(|e| e.to_string())?;

        Ok(Self {
            module: CapturingModule::new(module),
            ctx: cranelift_codegen::Context::new(),
            func_ctx: FunctionBuilderContext::new(),
            block_sig,
            load_id,
            store_id,
            string_id,
            host_span_id,
            f32_id,
            f64_id,
            sse_int_id,
            div64_id,
            sse_shift_id,
            sse_pshufb_lo_id,
            sse_pshufb_hi_id,
            sse_fp_unop_id,
            sse_fp_binop_id,
            sse_cvt_id,
            lookup_id,
            ucrt: UcrtImportIds {
                malloc,
                free,
                memcpy,
                strlen,
                iob,
                fwrite,
                fflush,
            },
        })
    }
}

// ---------------------------------------------------------------------------
// Emitted-code capture and restore (`WIE_JIT_CODE_CACHE`)
// ---------------------------------------------------------------------------

impl IsaEngine {
    /// Number of **import** declarations at the head of this module.
    ///
    /// Derived from the live declaration table rather than a hand-kept list, so
    /// it cannot drift from [`IsaEngine::new`]. Imports are declared first and
    /// only imports are `Linkage::Import`, so the prefix is exactly
    /// `FuncId` 0..`import_count()` — which is what makes a persisted
    /// `FuncId` index meaningful again in a later process.
    pub(super) fn import_count(&self) -> u32 {
        let decls = cranelift_module::Module::declarations(&self.module);
        let mut n = 0_u32;
        // `get_functions` is the non-panicking accessor (`get_function_decl`
        // indexes a `SecondaryMap` and traps past the end), and it walks the
        // declaration table in `FuncId` order.
        for (id, decl) in decls.get_functions() {
            if id.as_u32() != n || !matches!(decl.linkage, cranelift_module::Linkage::Import) {
                return n;
            }
            n = n.saturating_add(1);
        }
        n
    }

    /// Name of import declaration `index`, or `None` when `index` is not an
    /// import in this module.
    ///
    /// The per-relocation half of the persisted-file identity check: a code
    /// blob's relocation says "call import #7", which only means anything if
    /// import #7 in *this* process is the same host helper.
    pub(super) fn import_name_at(&self, index: u32) -> Option<&str> {
        if index >= self.import_count() {
            return None;
        }
        cranelift_module::Module::declarations(&self.module)
            .get_functions()
            .find(|(id, _)| id.as_u32() == index)
            .and_then(|(_, decl)| decl.name.as_deref())
    }

    /// FNV-1a over the ordered import names: the on-disk code file's identity
    /// check. Reordering or renaming a single helper invalidates every blob,
    /// which is the safe direction.
    pub(super) fn import_fingerprint(&self) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325_u64;
        for i in 0..self.import_count() {
            let name = self.import_name_at(i).unwrap_or("<anon>");
            h ^= name.len() as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
            for b in name.as_bytes() {
                h ^= u64::from(*b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        h
    }

    /// Drain the pre-relocation code just emitted for `func_id` in the tier
    /// this engine owns, if it was captured.
    pub(super) fn take_captured(&mut self, func_id: FuncId) -> Option<CapturedCode> {
        self.module.take_captured(func_id)
    }

    /// Define a block from persisted bytes instead of compiling it.
    ///
    /// Returns the finalized entry point. Fails — never panics, never maps
    /// unvalidated bytes — when any relocation target is not a host import this
    /// module declares under the same name.
    ///
    /// Rejects, deliberately and by design (see
    /// [`crate::jit::cache_persist`]):
    /// - `FunctionOffset` — a direct `bl` to another compiled block. Those
    ///   `FuncId`s are allocated in declaration order, so they name a different
    ///   function (or nothing) in the next process. A blob with any of these is
    ///   the block-level form of the chain table and is never persisted.
    /// - `LibCall` / `KnownSymbol` / `User` in a non-zero namespace (a data
    ///   object) — resolvable only through a linker cranelift-jit does not have.
    ///
    /// So a restore *rejects* rather than mis-restores; the caller compiles.
    pub(super) fn define_persisted(
        &mut self,
        code: &PersistedCode,
    ) -> Result<(FuncId, *const u8), CodeRestoreOutcome> {
        use cranelift_module::ModuleRelocTarget as Target;

        if code.code.is_empty() || code.align == 0 {
            return Err(CodeRestoreOutcome::Malformed);
        }
        for r in &code.relocs {
            let Target::User {
                namespace: 0,
                index,
            } = r.name
            else {
                return Err(CodeRestoreOutcome::UnrelocatableTarget);
            };
            // The name check is what makes "import #7" mean the same thing in
            // this process as it did in the one that wrote the file.
            if self.import_name_at(index).is_none() {
                return Err(CodeRestoreOutcome::UnrelocatableTarget);
            }
        }

        let sig = self.block_sig.clone();
        let func_id = self
            .module
            .declare_anonymous_function(&sig)
            .map_err(|_| CodeRestoreOutcome::Malformed)?;
        self.module
            .define_function_bytes(func_id, code.align, &code.code, &code.relocs)
            .map_err(|_| CodeRestoreOutcome::Malformed)?;
        // Only the function just defined is pending: every compile finalizes
        // before releasing `JitShared::engine`, which this call also holds.
        self.module
            .finalize_definitions()
            .map_err(|_| CodeRestoreOutcome::Malformed)?;
        let ptr = self.module.get_finalized_function(func_id);
        if ptr.is_null() {
            return Err(CodeRestoreOutcome::Malformed);
        }
        Ok((func_id, ptr))
    }
}
