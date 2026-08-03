//! Generation-validated, set-associative translation cache.
//!
//! An entry is valid iff its stamp equals the current generation. Invalidation
//! is one counter bump: stale entries fail the compare on the next lookup and
//! fall through to the slow path — no locks, no per-entry removal, no
//! reclamation. This is the contract the JIT's soft-translate path relies on.

/// One set of `WAYS` ways: parallel key / value / generation arrays.
///
/// `tags` is the first field and the set is 16-byte aligned so the aarch64
/// Neon tag compare (`lower::tlb::tlb_neon_tag_mask`) can load all ways with
/// two `vld1q_u64` ops (the TLB instantiates `K = u64`).
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub(super) struct GenSet<K, V, const WAYS: usize> {
    pub(super) tags: [K; WAYS],
    pub(super) values: [V; WAYS],
    pub(super) gens: [u64; WAYS],
    /// Round-robin victim for the next miss install (`& (WAYS - 1)` window).
    pub(super) rr: u8,
}

/// Generation-validated, set-associative cache.
///
/// Keyed by `K` (guest page keys) storing `V` (host page bases / bundled
/// metadata); a way is valid iff `gens[way] == current generation` and the
/// caller's [`Self::lookup`] allow-predicate passes (e.g. software R/W bits).
/// Set selection XOR-folds high key bits so arena bases that share low page
/// bits (64 KiB-aligned stack / heap / VirtualAlloc reserves) do not all
/// collide on set 0.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct GenTlb<K, V, const SETS: usize, const WAYS: usize> {
    sets: [GenSet<K, V, WAYS>; SETS],
}

impl<K, V, const SETS: usize, const WAYS: usize> GenTlb<K, V, SETS, WAYS>
where
    K: Copy + Eq + Into<u64>,
    V: Copy,
{
    /// Fresh cache: every way invalid (default tag, default value, stamp 0).
    ///
    /// `SETS` and `WAYS` must be powers of two (the victim index and the set
    /// index both mask instead of divide).
    pub(super) fn new() -> Self
    where
        K: Default,
        V: Default,
    {
        const {
            assert!(SETS.is_power_of_two() && SETS > 0);
        }
        const {
            assert!(WAYS.is_power_of_two() && WAYS > 0);
        }
        Self {
            sets: std::array::from_fn(|_| GenSet {
                tags: [K::default(); WAYS],
                values: [V::default(); WAYS],
                gens: [0; WAYS],
                rr: 0,
            }),
        }
    }

    /// Set index for `key`: XOR-fold high bits into the low `log2(SETS)`.
    ///
    /// Same arithmetic as the old free `tlb_set_index` — kept inside so any
    /// `GenTlb` instantiation (TLB or a future pin cache) shares the folding.
    #[inline]
    pub(super) fn set_index(&self, key: K) -> usize {
        let k: u64 = key.into();
        let mixed = k ^ (k >> 4) ^ (k >> 8) ^ (k >> 12);
        usize::try_from(mixed).unwrap_or(0) & (SETS - 1)
    }

    /// Way search + generation compare (same arithmetic as the old scalar
    /// `tlb_bucket_lookup_scalar`).
    ///
    /// `allow` runs last per way so the caller keeps its own validity rules on
    /// the value (non-null host base, R/W prot bits) without making `GenTlb`
    /// value-type-aware. Predicates are order-independent and the way tags are
    /// unique, so this is behavior-identical to the old inline loop.
    #[inline]
    pub(super) fn lookup<F>(&self, key: K, stamp: u64, mut allow: F) -> Option<V>
    where
        F: FnMut(V) -> bool,
    {
        let s = self.sets.get(self.set_index(key))?;
        for way in 0..WAYS {
            if s.tags.get(way).copied() != Some(key) {
                continue;
            }
            if s.gens.get(way).copied() != Some(stamp) {
                continue;
            }
            let Some(value) = s.values.get(way).copied() else {
                continue;
            };
            if allow(value) {
                return Some(value);
            }
        }
        None
    }

    /// Refresh-if-present, else round-robin victim install (`rr & (WAYS-1)`).
    ///
    /// Mirrors the old `tlb_install` semantics exactly: `rr` starts at 0 and
    /// only advances on victim installs, so the first `WAYS` fills occupy ways
    /// 0..`WAYS`-1 and later eviction cycles the same order the old
    /// empty-tag-preference + rr scheme produced. Stale (gen-mismatched) ways
    /// are refreshed in place, matching the old install.
    #[inline]
    pub(super) fn insert(&mut self, key: K, value: V, stamp: u64) {
        let set = self.set_index(key);
        let Some(s) = self.sets.get_mut(set) else {
            return;
        };
        for way in 0..WAYS {
            if s.tags.get(way).copied() == Some(key) {
                if let Some(v) = s.values.get_mut(way) {
                    *v = value;
                }
                if let Some(g) = s.gens.get_mut(way) {
                    *g = stamp;
                }
                return;
            }
        }
        let way = usize::from(s.rr) & (WAYS - 1);
        s.rr = s.rr.wrapping_add(1);
        if let Some(t) = s.tags.get_mut(way) {
            *t = key;
        }
        if let Some(v) = s.values.get_mut(way) {
            *v = value;
        }
        if let Some(g) = s.gens.get_mut(way) {
            *g = stamp;
        }
    }

    /// Borrow one set for the aarch64 Neon tag-compare fast path.
    #[cfg(target_arch = "aarch64")]
    #[inline]
    pub(super) fn set(&self, set: usize) -> Option<&GenSet<K, V, WAYS>> {
        self.sets.get(set)
    }
}
