//! `CpuEngine` trait implementation for `JitCpu` (host-call interface).
//!
//! Extracted verbatim from `jit/mod.rs`. As a
//! submodule it may `impl crate::CpuEngine for super::JitCpu`; the impl
//! accesses `JitCpu`'s pipeline methods via their `pub(super)` bumps.

#![allow(
    unsafe_code, // Cranelift finalized fn pointers + host mem helpers
    private_interfaces // JitShared/PerThreadJitState expose crate-private types
)]

use super::fast_api::JitFastPathConfig;
use super::pipeline::CompiledRunMeta;
use super::{CacheEntry, JitCpu};
use crate::exec::{HookWindow, StepResult};
use crate::{CodeHookOutcome, CpuEngine, CpuError, InvalidMemoryAccess, RunUntilHook, RwxPerms};
use std::sync::atomic::Ordering;

impl CpuEngine for JitCpu {
    fn mem_map(&mut self, address: u64, size: usize, perms: RwxPerms) -> Result<(), CpuError> {
        let mut mem = self.shared.mem.write().unwrap();
        let r = mem.map(address, size, perms);
        self.shared
            .mem_gen
            .store(mem.generation(), Ordering::Release);
        r
    }

    fn mem_write(&mut self, address: u64, bytes: &[u8]) -> Result<(), CpuError> {
        self.shared.mem.write().unwrap().write(address, bytes)?;
        self.note_code_write(address, bytes.len());
        self.drain_pending_code_writes();
        Ok(())
    }

    fn mem_read(&mut self, address: u64, bytes: &mut [u8]) -> Result<(), CpuError> {
        self.shared.mem.read().unwrap().read(address, bytes)
    }

    fn host_span(&mut self, address: u64, len: usize, write: bool) -> Option<*mut u8> {
        self.shared
            .mem
            .read()
            .unwrap()
            .host_span(address, len, write)
    }

    fn host_slice(&self, address: u64, len: usize) -> Option<&[u8]> {
        if len == 0 {
            return Some(&[]);
        }
        let ptr = self
            .shared
            .mem
            .read()
            .unwrap()
            .host_span(address, len, false)?;
        // SAFETY: as `IcedCpu::host_slice` — the mmap arena outlives the read
        // guard, and the `&self` borrow excludes concurrent unmapping.
        #[expect(unsafe_code)]
        Some(unsafe { std::slice::from_raw_parts(ptr, len) })
    }

    fn mem_copy(&mut self, dst: u64, src: u64, len: usize) -> bool {
        self.shared.mem.read().unwrap().mem_copy(dst, src, len)
    }

    fn mem_fill(&mut self, address: u64, byte: u8, len: usize) -> bool {
        self.shared.mem.read().unwrap().mem_fill(address, byte, len)
    }

    fn mem_generation(&self) -> u64 {
        self.shared.mem.read().unwrap().generation()
    }

    fn virtual_alloc(
        &mut self,
        addr: u64,
        size: usize,
        alloc_type: u32,
        protect: u32,
    ) -> Result<u64, CpuError> {
        let r = self
            .shared
            .mem
            .write()
            .unwrap()
            .virtual_alloc(addr, size, alloc_type, protect);
        self.shared.mem_gen.store(
            self.shared.mem.read().unwrap().generation(),
            Ordering::Release,
        );
        self.invalidate_tlb();
        r
    }

    fn virtual_free(&mut self, addr: u64, size: usize, free_type: u32) -> Result<(), CpuError> {
        let inv_span = self.code_inv_span_for_free(addr, size, free_type);
        self.invalidate_tlb();
        let r = self
            .shared
            .mem
            .write()
            .unwrap()
            .virtual_free(addr, size, free_type);
        self.shared.mem_gen.store(
            self.shared.mem.read().unwrap().generation(),
            Ordering::Release,
        );
        if r.is_ok()
            && let Some((a, n)) = inv_span
        {
            self.invalidate_code_range(a, n);
        }
        r
    }

    fn virtual_protect(
        &mut self,
        addr: u64,
        size: usize,
        new_protect: u32,
    ) -> Result<u32, CpuError> {
        let r = self
            .shared
            .mem
            .write()
            .unwrap()
            .virtual_protect(addr, size, new_protect);
        self.shared.mem_gen.store(
            self.shared.mem.read().unwrap().generation(),
            Ordering::Release,
        );
        // X-loss: dropping execute permission invalidates any compiled blocks
        // over the range. An unparseable protect is treated as non-executable,
        // matching the previous `allows_execute(u32)`, which returned false for
        // values outside the supported set.
        let loses_exec = crate::mem::protect::PageProtect::from_win32(new_protect)
            .is_none_or(|p| !p.allows_execute());
        if r.is_ok() && loses_exec {
            self.invalidate_code_range(addr, size);
        }
        self.invalidate_tlb();
        r
    }

    fn virtual_query(&self, addr: u64) -> crate::MemoryBasicInformation {
        self.shared.mem.read().unwrap().virtual_query(addr)
    }

    fn flush_instruction_cache(&mut self, addr: u64, size: usize) -> Result<(), CpuError> {
        if size == 0 {
            if !self.shared.cache.read().unwrap().is_empty() {
                self.clear_compiled();
                self.invalidate_chain_and_shadow();
                self.stats.code_invs = self.stats.code_invs.saturating_add(1);
            }
        } else {
            self.invalidate_code_range(addr, size);
        }
        Ok(())
    }

    fn mem_map_image(
        &mut self,
        address: u64,
        size: usize,
        perms: RwxPerms,
    ) -> Result<(), CpuError> {
        let r = self
            .shared
            .mem
            .write()
            .unwrap()
            .map_image(address, size, perms);
        self.shared.mem_gen.store(
            self.shared.mem.read().unwrap().generation(),
            Ordering::Release,
        );
        self.invalidate_tlb();
        r
    }

    fn cpu_stats(&self) -> Option<crate::JitStats> {
        Some(self.stats())
    }

    fn mem_backend_name(&self) -> &'static str {
        self.shared.mem.read().unwrap().backend_name()
    }

    fn register_region(&mut self, region: crate::mem::GuestRegion) {
        self.shared.mem.write().unwrap().register_region(region);
    }

    fn find_region(&self, va: u64) -> Option<crate::mem::GuestRegion> {
        self.shared.mem.read().unwrap().find_region(va).cloned()
    }

    fn install_runtime_hooks(
        &mut self,
        hook_begin: u64,
        hook_end: u64,
        stop_bitmap: std::sync::Arc<[u8]>,
    ) -> Result<(), CpuError> {
        self.clear_compiled();
        self.invalidate_tlb();
        self.invalidate_chain_and_shadow();
        let range_len = hook_end.saturating_sub(hook_begin).saturating_add(1);
        let expected_bytes = usize::try_from(range_len).unwrap_or(usize::MAX).div_ceil(8);
        if expected_bytes != usize::MAX && stop_bitmap.len() < expected_bytes {
            return Err(CpuError::Message(format!(
                "stop_bitmap too small: {} < {expected_bytes}",
                stop_bitmap.len()
            )));
        }
        self.thread.hooks = Some(HookWindow {
            begin: hook_begin,
            end: hook_end,
            stop_bitmap,
        });
        Ok(())
    }

    fn configure_jit_fast_path(&mut self, cfg: JitFastPathConfig) {
        self.configure_fast_path(cfg);
        self.invalidate_chain_and_shadow();
    }

    fn precompile_at(&mut self, address: u64) {
        if !self.shared.engine_ready.load(Ordering::Relaxed) {
            return;
        }
        if let Some(hook) = self.thread.hooks.as_ref()
            && hook.should_host_stop(address)
        {
            return;
        }
        if let Some(compiled) = self.try_compile(address) {
            self.insert_ready(address, compiled);
        } else {
            self.shared
                .cache
                .write()
                .unwrap()
                .entry(address)
                .or_insert(CacheEntry::Never);
        }
    }

    fn run_until_stop(
        &mut self,
        begin: u64,
        until: u64,
        _timeout: u64,
        count: usize,
        _hook_begin: u64,
        _hook_end: u64,
    ) -> Result<RunUntilHook, CpuError> {
        self.thread.regs.rip = begin;
        let budget = if count == 0 { 100_000_000_usize } else { count };
        let mut executed = 0_usize;
        while executed < budget {
            let rip = self.thread.regs.rip;
            if until != 0 && rip == until {
                break;
            }
            if let Some(hook) = self.thread.hooks.as_ref()
                && hook.should_host_stop(rip)
            {
                return Ok(RunUntilHook {
                    code: CodeHookOutcome {
                        hit: true,
                        address: rip,
                        size: 1,
                    },
                    invalid_memory: InvalidMemoryAccess {
                        hit: false,
                        exception_code: 0,
                        access_type: 0,
                        address: 0,
                        size: 0,
                        value: 0,
                    },
                });
            }
            // Pick up background-installed Ready blocks into this thread's
            // chain table (cheap relaxed load; resync only after an install).
            let epoch = self.shared.cache_epoch.load(Ordering::Relaxed);
            if epoch != self.chain_sync_epoch {
                self.chain_sync_epoch = epoch;
                self.resync_chain_table();
            }
            // Hot chain: run consecutive Ready blocks without re-entering step_one.
            let mut chain_result = None;
            if self.shared.engine_ready.load(Ordering::Relaxed) {
                let meta = {
                    let cache = self.shared.cache.read().unwrap();
                    cache.get(&rip).and_then(|e| match e {
                        CacheEntry::Ready(c) => Some(CompiledRunMeta::from(c)),
                        _ => None,
                    })
                };
                if let Some(meta) = meta {
                    self.stats.cache_hits = self.stats.cache_hits.saturating_add(1);
                    let (result, retired) = self.finish_compiled(rip, meta);
                    match result {
                        StepResult::Continue => {
                            executed = executed.saturating_add(retired.max(1));
                            continue;
                        }
                        other => {
                            chain_result = Some(other);
                        }
                    }
                }
            }
            if let Some(result) = chain_result {
                return match result {
                    StepResult::HostStop { address, size } => Ok(RunUntilHook {
                        code: CodeHookOutcome {
                            hit: true,
                            address,
                            size,
                        },
                        invalid_memory: InvalidMemoryAccess {
                            hit: false,
                            exception_code: 0,
                            access_type: 0,
                            address: 0,
                            size: 0,
                            value: 0,
                        },
                    }),
                    StepResult::InvalidMemory(inv) => Ok(RunUntilHook {
                        code: CodeHookOutcome {
                            hit: false,
                            address: 0,
                            size: 0,
                        },
                        invalid_memory: InvalidMemoryAccess {
                            hit: true,
                            exception_code: crate::exception_code::ACCESS_VIOLATION,
                            access_type: inv.access_type.as_i32(),
                            address: inv.address,
                            size: inv.size,
                            value: inv.value,
                        },
                    }),
                    StepResult::Continue => Err(CpuError::Message(
                        "unexpected Continue from chained block RET".into(),
                    )),
                };
            }
            let (result, retired) = self.step_one()?;
            match result {
                StepResult::Continue => {
                    executed = executed.saturating_add(retired.max(1));
                }
                StepResult::HostStop { address, size } => {
                    return Ok(RunUntilHook {
                        code: CodeHookOutcome {
                            hit: true,
                            address,
                            size,
                        },
                        invalid_memory: InvalidMemoryAccess {
                            hit: false,
                            exception_code: 0,
                            access_type: 0,
                            address: 0,
                            size: 0,
                            value: 0,
                        },
                    });
                }
                StepResult::InvalidMemory(inv) => {
                    return Ok(RunUntilHook {
                        code: CodeHookOutcome {
                            hit: false,
                            address: 0,
                            size: 0,
                        },
                        invalid_memory: InvalidMemoryAccess {
                            hit: true,
                            exception_code: crate::exception_code::ACCESS_VIOLATION,
                            access_type: inv.access_type.as_i32(),
                            address: inv.address,
                            size: inv.size,
                            value: inv.value,
                        },
                    });
                }
            }
        }
        Ok(RunUntilHook {
            code: CodeHookOutcome {
                hit: false,
                address: 0,
                size: 0,
            },
            invalid_memory: InvalidMemoryAccess {
                hit: false,
                exception_code: 0,
                access_type: 0,
                address: 0,
                size: 0,
                value: 0,
            },
        })
    }

    fn return_from_win64_api(&mut self, rax: u64) -> Result<u64, CpuError> {
        self.thread.shadow_sp = 0;
        let rsp = self.thread.regs.rsp();
        let mut ret_bytes = [0_u8; 8];
        self.shared
            .mem
            .read()
            .unwrap()
            .read(rsp, &mut ret_bytes)
            .map_err(|e| CpuError::Message(format!("return_from_win64_api stack read: {e}")))?;
        let return_address = u64::from_le_bytes(ret_bytes);
        self.thread.regs.set_rsp(rsp.wrapping_add(8));
        self.thread.regs.set_rax(rax);
        self.thread.regs.rip = return_address;
        Ok(return_address)
    }

    fn read_rip(&mut self) -> Result<u64, CpuError> {
        Ok(self.thread.regs.rip)
    }
    fn write_rip(&mut self, value: u64) -> Result<(), CpuError> {
        self.thread.regs.rip = value;
        Ok(())
    }
    fn read_rsp(&mut self) -> Result<u64, CpuError> {
        Ok(self.thread.regs.rsp())
    }
    fn write_rsp(&mut self, value: u64) -> Result<(), CpuError> {
        self.thread.regs.set_rsp(value);
        Ok(())
    }
    fn read_rax(&mut self) -> Result<u64, CpuError> {
        Ok(self.thread.regs.rax())
    }
    fn write_rax(&mut self, value: u64) -> Result<(), CpuError> {
        self.thread.regs.set_rax(value);
        Ok(())
    }
    fn read_rcx(&mut self) -> Result<u64, CpuError> {
        Ok(self.thread.regs.rcx())
    }
    fn write_rcx(&mut self, value: u64) -> Result<(), CpuError> {
        self.thread.regs.set_rcx(value);
        Ok(())
    }
    fn read_rdx(&mut self) -> Result<u64, CpuError> {
        Ok(self.thread.regs.rdx())
    }
    fn write_rdx(&mut self, value: u64) -> Result<(), CpuError> {
        self.thread.regs.set_rdx(value);
        Ok(())
    }
    fn read_r8(&mut self) -> Result<u64, CpuError> {
        Ok(self.thread.regs.r8())
    }
    fn write_r8(&mut self, value: u64) -> Result<(), CpuError> {
        self.thread.regs.set_r8(value);
        Ok(())
    }
    fn read_r9(&mut self) -> Result<u64, CpuError> {
        Ok(self.thread.regs.r9())
    }
    fn write_r9(&mut self, value: u64) -> Result<(), CpuError> {
        self.thread.regs.set_r9(value);
        Ok(())
    }
    fn read_rbx(&mut self) -> Result<u64, CpuError> {
        Ok(self.thread.regs.gpr(3))
    }
    fn read_r12(&mut self) -> Result<u64, CpuError> {
        Ok(self.thread.regs.gpr(12))
    }

    fn snapshot_thread_context(&mut self) -> crate::ThreadContext {
        self.thread.regs.snapshot()
    }

    fn restore_thread_context(&mut self, ctx: &crate::ThreadContext) {
        self.thread.regs.restore(ctx);
    }

    fn on_thread_switch(&mut self) {
        self.invalidate_tlb();
        self.invalidate_chain_and_shadow();
    }
}
