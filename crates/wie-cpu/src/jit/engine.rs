//! Cranelift `JITModule` wrapper: import declarations + engine construction.
//!
//! Extracted verbatim from `jit/mod.rs` (Phase 4 split, lane 2). `lower.rs`
//! mutates `JitEngine` fields directly, so every field is `pub(super)`
//! (visible across `crate::jit`, matching the old private-in-`mod.rs` scope).

#![allow(
    unsafe_code, // Cranelift finalized fn pointers + host mem helpers
    private_interfaces, // JitShared/PerThreadJitState expose crate-private types
    clippy::indexing_slicing, // fixed gpr[0..16]
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::arithmetic_side_effects,
    clippy::unwrap_used // Mutex/RwLock poison recovery is hard-coded (never occurs in practice)
)]

use super::UcrtImportIds;
use super::config::JitConfig;
use super::fast_api::{
    wie_ucrt_fflush, wie_ucrt_free, wie_ucrt_fwrite, wie_ucrt_iob, wie_ucrt_malloc,
    wie_ucrt_memcpy, wie_ucrt_strlen,
};
use super::lower::{
    wie_f32_binop, wie_f64_binop, wie_jit_chain_lookup, wie_jit_host_span, wie_jit_load,
    wie_jit_store, wie_jit_string, wie_sse_cvt, wie_sse_fp_binop, wie_sse_fp_unop,
    wie_sse_int_binop, wie_sse_pshufb_hi, wie_sse_pshufb_lo, wie_sse_shift,
};

pub(crate) struct JitEngine {
    pub(super) module: cranelift_jit::JITModule,
    pub(super) ctx: cranelift_codegen::Context,
    pub(super) func_ctx: cranelift::prelude::FunctionBuilderContext,
    pub(super) next_name: u32,
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
impl JitEngine {
    pub(super) fn new() -> Result<Self, String> {
        use cranelift::prelude::*;
        use cranelift_codegen::settings::Configurable;
        use cranelift_jit::{JITBuilder, JITModule};
        use cranelift_module::{Linkage, Module, default_libcall_names};

        let mut flag_builder = settings::builder();
        // Phase 5.5 Track D: prefer speed of host code for hot translated blocks.
        flag_builder
            .set("opt_level", JitConfig::get().opt_level())
            .map_err(|e| e.to_string())?;
        let verify = if JitConfig::get().verifier_enabled() {
            "true"
        } else {
            "false"
        };
        flag_builder
            .set("enable_verifier", verify)
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
            next_name: 0,
            block_sig,
            load_id,
            store_id,
            string_id,
            host_span_id,
            f32_id,
            f64_id,
            sse_int_id,
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
