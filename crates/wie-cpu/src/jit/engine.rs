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

/// The per-tier Cranelift machinery: one `JITModule`, one compile `Context`,
/// and that module's own import `FuncId`s.
pub(crate) struct IsaEngine {
    pub(super) module: cranelift_jit::JITModule,
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
        // Verifier stays ON unconditionally (see comment above). WIE_JIT_VERIFY
        // is no longer a gate; the per-compile verifier cost on small blocks
        // is negligible next to the crash-safety it provides.
        flag_builder
            .set("enable_verifier", "true")
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
            module,
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
