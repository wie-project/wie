//! A last-page cache for memory accesses in loops.
//!
//! The prologue homes in [`super::mem`] are built from the stack pin, so every
//! global, DLL-data or heap access in a loop paid the whole refill-table probe
//! on every iteration. Here each distinct (base, index, direction) of a loop's
//! operands, up to [`SITES`], remembers the page the refill probe last resolved
//! for it, and checks that with one subtract and one compare. Operands through
//! the same registers share a site, since they usually walk the same buffer.
//! `RSP`/`RBP` operands and implicit stack accesses take none: the stack home
//! already serves them. The choice only affects speed — every site check bounds
//! the address itself.
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

use iced_x86::Register;

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

/// Which operand registers an access goes through, the guest page for a
/// fixed address (0 otherwise), and its direction.
type Key = (Register, Register, u64, bool);

/// The sites of one loop, handed out by operand key in emission order.
pub(super) struct SiteHomes {
    sites: Vec<Site>,
    keys: std::cell::RefCell<Vec<Key>>,
    /// `(base, index, fixed page)` of the operand being lowered.
    operand: std::cell::Cell<Option<(Register, Register, u64)>>,
    /// The slot already chosen for the current operand, per direction. A 16-byte
    /// operand asks again from its split path, and must get the same one back.
    chosen: std::cell::Cell<[Option<usize>; 2]>,
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
            keys: std::cell::RefCell::new(Vec::with_capacity(SITES)),
            operand: std::cell::Cell::new(None),
            chosen: std::cell::Cell::new([None; 2]),
        };
        homes.clear(bcx);
        homes
    }

    /// Record the registers of the operand whose address is being computed.
    pub(super) fn note_operand(&self, base: Register, index: Register, disp: u64) {
        // With a fixed address, operands on the same page can share a site.
        let fixed = index == Register::None
            && matches!(base, Register::None | Register::RIP | Register::EIP);
        let page = if fixed { disp / PAGE_SIZE } else { 0 };
        self.operand.set(Some((base, index, page)));
        self.chosen.set([None; 2]);
    }

    /// Forget the operand, so a following implicit stack access takes no site.
    pub(super) fn end_operand(&self) {
        self.operand.set(None);
        self.chosen.set([None; 2]);
    }

    /// The site serving the current operand in this direction, if it gets one.
    pub(super) fn site_for(&self, write: bool) -> Option<Site> {
        let (base, index, page) = self.operand.get()?;
        if matches!(base, Register::RSP | Register::RBP) {
            return None;
        }
        let mut chosen = self.chosen.get();
        if let Some(slot) = chosen.get(usize::from(write)).copied().flatten() {
            return self.sites.get(slot).copied();
        }
        let key = (base, index, page, write);
        let mut keys = self.keys.borrow_mut();
        let found = keys.iter().position(|k| *k == key);
        let slot = found.unwrap_or_else(|| {
            keys.push(key);
            keys.len().saturating_sub(1)
        });
        if let Some(entry) = chosen.get_mut(usize::from(write)) {
            *entry = Some(slot);
        }
        self.chosen.set(chosen);
        self.sites.get(slot).copied()
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
