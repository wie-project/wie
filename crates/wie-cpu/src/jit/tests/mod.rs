// JIT parity / invalidation / background-worker integration tests.
// Moved out of jit/mod.rs (file-size policy, ADR-002); wired via
// `#[cfg(test)] #[allow(clippy::expect_used)] mod tests;`.
mod atomic_tests;
mod bg_compile_tests;
mod bg_pool_tests;
mod btx_tests;
mod chain_hop_tests;
mod chain_repair_tests;
mod chain_tests;
mod cmpxchg_tests;
mod code_cache_tests;
mod code_inv_tests;
mod frame_entry_tests;
mod gs_teb_tests;
mod harness;
mod heap_isolation_tests;
mod implicit_lock_tests;
mod implicit_operands;
mod interp_edge_tests;
mod inv_gen_tests;
mod locked_alu_tests;
mod pause_tests;
mod refill_tlb_tests;
mod simd_parity_tests;
mod strlen_repro;
mod testb_high_byte;
mod tier_tests;
mod trip_tests;
use super::*;
use crate::exec::StepResult;
use crate::mem::protect;
use crate::mem::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE};
use crate::regs::RegFile;
use crate::{CpuEngine, RwxPerms};
use block::BlockKind;
use config::JitConfig;
use harness::{alloc_code_page, map_code_page, map_code_region};
use lower::{JitCtx, chain_table_insert};
use pipeline::ranges_overlap;
use shared::{BgEnqueueOutcome, BgPool, BgWaitCell};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tier::OptTier;

/// Code base shared by the SIMD/`x87` fixtures: the two pages are mapped RWX
/// together (`SIMD_DATA` is the second one).
const SIMD_BASE: u64 = 0x2000_0000;
const SIMD_DATA: u64 = SIMD_BASE + 0x1000;

/// Number of instructions `bytes` decodes to, stopping at the trailing
/// `ud2` terminator (test-encoding sanity). Shared by every dual-path fixture
/// that steps the interpreter a known number of times.
fn decode_count(bytes: &[u8]) -> usize {
    let mut dec = iced_x86::Decoder::with_ip(64, bytes, 0, iced_x86::DecoderOptions::NONE);
    let mut n = 0;
    while dec.position() < bytes.len() {
        let insn = dec.decode();
        assert!(
            !insn.is_invalid(),
            "invalid test encoding at {n}: {bytes:02x?}"
        );
        if insn.mnemonic() == iced_x86::Mnemonic::Ud2 {
            break;
        }
        n += 1;
    }
    n
}

unsafe extern "C" fn dummy_block(_ctx: *mut JitCtx) {}

impl JitCpu {
    /// Test helper: plant a Ready entry without Cranelift.
    fn test_plant_ready(&mut self, rip: u64, guest_end: u64) {
        self.insert_ready(
            rip,
            CompiledBlock {
                func: dummy_block,
                func_id: None,
                // Planted, never compiled: no module owns it.
                tier: OptTier::Base,
                insn_count: 1,
                guest_start: rip,
                guest_end,
                inv_gen: 0,
            },
        );
        if JitConfig::get().chain_enabled() {
            let fn_ptr = dummy_block as *const () as usize as u64;
            chain_table_insert(self.thread.chain_slots.as_mut(), rip, fn_ptr);
        }
        // Simulate edge IC hit for S6.
        self.thread.ctx.edge_ic_va[0] = rip;
        self.thread.ctx.edge_ic_fn[0] = dummy_block as *const () as usize as u64;
    }
}
