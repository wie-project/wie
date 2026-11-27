//! A last-page cache per memory access site, for loops.
//!
//! The prologue homes in [`super::mem`] are built from the stack pin, so every
//! global, DLL-data or heap access in a loop paid the whole refill-table probe
//! on every iteration. Here each of the first [`SITES`] access sites of a loop
//! remembers the page the refill probe last resolved for it, and checks that
//! with one subtract and one compare.
//!
//! # Why a remembered page is no staler than the probe it short-cuts
//!
//! A refill slot is matched against the frame's `mem_gen`, which only the
//! dispatcher writes, and the per-thread table only changes when this thread
//! calls a load or store helper. So a site is filled from a successful refill
//! probe, lives no longer than the frame, and every site is cleared after any
//! helper access. Permissions are per direction: a load site is only filled by a
//! read probe and a store site by a write probe.

use cranelift::codegen::ir::{Block, BlockArg, condcodes::IntCC, types};
use cranelift::prelude::{FunctionBuilder, InstBuilder, Value, Variable};

use crate::mem::PAGE_SIZE;

/// Sites per compiled loop. Each costs two loop-carried values.
const SITES: usize = 8;

/// A page base no guest address is within 4 KiB of.
const EMPTY: i64 = -0x1_0000;

/// One site: the guest page base and `host - guest` for that page.
#[derive(Clone, Copy)]
pub(super) struct Site {
    page: Variable,
    bias: Variable,
}

/// The sites of one loop, handed out to accesses in emission order.
pub(super) struct SiteHomes {
    sites: Vec<Site>,
    next: std::cell::Cell<usize>,
}

impl SiteHomes {
    /// Declare empty sites. Must run in the **entry** block, before the loop.
    pub(super) fn declare(bcx: &mut FunctionBuilder<'_>) -> Self {
        let sites = (0..SITES)
            .map(|_| Site {
                page: bcx.declare_var(types::I64),
                bias: bcx.declare_var(types::I64),
            })
            .collect();
        let homes = Self {
            sites,
            next: std::cell::Cell::new(0),
        };
        homes.clear(bcx);
        homes
    }

    /// The next unused site, if any is left.
    pub(super) fn take(&self) -> Option<Site> {
        let index = self.next.get();
        let site = self.sites.get(index).copied()?;
        self.next.set(index.saturating_add(1));
        Some(site)
    }

    /// Forget every remembered page (after a helper access).
    pub(super) fn clear(&self, bcx: &mut FunctionBuilder<'_>) {
        let empty = bcx.ins().iconst(types::I64, EMPTY);
        let zero = bcx.ins().iconst(types::I64, 0);
        for site in &self.sites {
            bcx.def_var(site.page, empty);
            bcx.def_var(site.bias, zero);
        }
    }
}

impl Site {
    /// Branch to `hit(host)` when `size` bytes at `addr` lie in the remembered
    /// page, and continue in a fresh block otherwise.
    pub(super) fn probe(
        self,
        bcx: &mut FunctionBuilder<'_>,
        addr: Value,
        size: u32,
        merge: Block,
        hit: impl FnOnce(&mut FunctionBuilder<'_>, Value) -> Option<Value>,
    ) {
        let page = bcx.use_var(self.page);
        let offset = bcx.ins().isub(addr, page);
        let limit = PAGE_SIZE.saturating_sub(u64::from(size)).saturating_add(1);
        let ok = bcx
            .ins()
            .icmp_imm(IntCC::UnsignedLessThan, offset, limit.cast_signed());
        let hit_block = bcx.create_block();
        let miss = bcx.create_block();
        bcx.ins().brif(ok, hit_block, &[], miss, &[]);
        bcx.switch_to_block(hit_block);
        bcx.seal_block(hit_block);
        let bias = bcx.use_var(self.bias);
        let host = bcx.ins().iadd(addr, bias);
        match hit(bcx, host) {
            Some(v) => bcx.ins().jump(merge, &[BlockArg::Value(v)]),
            None => bcx.ins().jump(merge, &[]),
        };
        bcx.switch_to_block(miss);
        bcx.seal_block(miss);
    }

    /// Remember the page holding `addr`, which translates to `host`.
    pub(super) fn fill(self, bcx: &mut FunctionBuilder<'_>, addr: Value, host: Value) {
        let page = bcx.ins().band_imm(addr, -PAGE_SIZE.cast_signed());
        let bias = bcx.ins().isub(host, addr);
        bcx.def_var(self.page, page);
        bcx.def_var(self.bias, bias);
    }
}
