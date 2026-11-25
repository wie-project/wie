//! Persistent JIT **machine-code** cache (`WIE_JIT_CODE_CACHE`) — the second
//! half of the on-disk cache, independent of the metadata ledger in
//! [`super::cache_persist`].
//!
//! # Why this exists
//!
//! The metadata ledger records *that* a guest block compiled, which skips hotness
//! warm-up but still pays Cranelift's ~3 ms/block `compile_block` on every
//! launch. For a guest with thousands of hot blocks that is seconds of launch
//! latency, every launch. This module persists the emitted code so a warm boot
//! can *replay* a block instead of compiling it.
//!
//! # Why it is safe, which is the whole problem
//!
//! Replaying host code is only safe if every address the code bakes in can be
//! re-resolved in the new process. With `is_pic = false` on aarch64 the emitted
//! code contains exactly these address-bearing constructs, and **no others**:
//!
//! | Class | Relocation | Replayable? |
//! | --- | --- | --- |
//! | Our declared host helpers (`load`, `store`, `f32_binop`, …) | `Arm64Call` / `Aarch64AdrPrelPgHi21` + `Aarch64AddAbsLo12Nc` / `Abs8` / `Abs4` to `User{0, idx}` with `idx < import_count` | **Yes** — a fixed, ordered import list in [`IsaEngine::new`], re-checked by name |
//! | Intra-function branches and blocks | *none* | Yes — Cranelift resolves them at emit time and emits no relocation |
//! | Direct chain-table calls to another compiled block | `Arm64Call` to `User{0, idx}` with `idx >= import_count` | **Yes, since v3** — recorded as the successor's *guest VA* and re-resolved against the live chain table (next section) |
//! | Any *other* reference to a block `FuncId` (adrp/add, absolute data) | `Aarch64*` / `Abs*` to `User{0, idx}` with `idx >= import_count` | **No** — only a direct call has a guest-VA-shaped meaning this format can re-derive |
//! | Cranelift internal libcalls (`Memcpy`), data objects, linker-known symbols | `LibCall` / `User{1, ..}` / `KnownSymbol` | **No** — resolved through a linker `cranelift-jit` does not have |
//! | `is_pic` GOT references | `Aarch64AdrGotPage21` / `Aarch64Ld64GotLo12Nc` | **N/A** — `cranelift-jit` asserts `is_pic == false` and panics on these |
//!
//! The first and third rows are told apart by the `FuncId` **index**, not by the
//! relocation kind: [`IsaEngine::new`] declares the imports first and only
//! imports are `Linkage::Import`, so `idx < import_count` names a host helper and
//! every other index names a function this process allocated for a compiled
//! block. (Testing only `namespace == 0` — which every block reference also
//! satisfies — is how chained blocks used to be *persisted* and then *refused at
//! restore*, showing up in the `target` counter rather than never being written.)
//!
//! So the reference set is *enumerable*, and the policy is blunt and total: a
//! block is persisted **iff every one of its relocations is in one of the two
//! replayable classes above**. On restore every import is re-checked against the
//! live module's import list by name and every chain edge is re-resolved from
//! its guest VA through the live chain table. Anything else is refused and
//! recompiled. Refusing costs a compile; guessing costs a crash.
//!
//! # Chain edges: a guest VA crosses the boundary, never an address
//!
//! A direct chain edge is `Arm64Call` to another compiled block's `FuncId`, and
//! a `FuncId` is an index into *this* process's declaration order — meaningless
//! in the next one. The format therefore stores, per edge, the **guest VA it
//! transfers to** (the block's `exit_rip`) and nothing else, and restore turns
//! that VA back into a live `FuncId` through
//! [`JitShared::chain_func_for`](crate::jit::shared::JitShared::chain_func_for)
//! — the same tier-tagged lookup a fresh compile resolves its own chain edges
//! with. Why that cannot honour a stale target:
//!
//! 1. **The VA is invariant.** The probe has already proven the blob's bytes are
//!    byte-identical to live guest memory at `[va, guest_end)` (FNV-1a below),
//!    and decoding is a pure function of those bytes — so the successor this
//!    blob names is exactly the successor a fresh decode of the *current* bytes
//!    names. Nothing about the recording process's layout survives.
//! 2. **The code behind it is this run's.** Only the VA crosses; the address
//!    comes from this process's table, so a live hit is a block that *this* run
//!    compiled, tagged with the tier whose module declared it (`FuncId`s are
//!    module-local, so the tag is what keeps the edge same-tier).
//! 3. **A live hit implies current bytes.** `JitCpu::invalidate_code_range`
//!    drops the overlapping `Ready` entry *and* its `chain_ids` entry before the
//!    `Release` bump; `insert_ready` does the same when it replaces one. So a successor whose guest bytes
//!    moved is *absent*, and absence is a refusal, not a guess. This is the same
//!    invariant a compile-time chain already relies on — re-resolution adds no
//!    trust of its own.
//! 4. **The record side refuses ambiguity.** `FuncId -> VA` is read from the
//!    authoritative forward table, never from a parallel index, and a scan that
//!    finds two entries claiming one `(tier, FuncId)` yields *no* VA — so the
//!    answer does not depend on `FuncId`s never being reused.
//! 5. **The edge is still guarded.** Replayed or compiled, a chain edge runs the
//!    frame-generation guard before it touches any chain machinery, so an
//!    invalidation between restore and execution costs a dispatcher exit instead
//!    of honouring a dead target (see the invariant section below).
//!
//! A chain edge whose successor is not chainable at restore time — never
//! compiled in this run, or invalidated since — refuses the *whole* blob
//! (`UnrelocatableTarget`) and the block is compiled as if there were no cache.
//! A second edge recorded at one offset, or an edge recorded with a non-
//! `Arm64Call` kind, is refused by [`PersistedCode::structurally_sound`].
//!
//! # The non-obvious invariant: no per-process value in the emitted code
//!
//! Replaying host code is only meaningful if a blob's behaviour after restore
//! depends on *this* process's state and nothing carried over from the one
//! that emitted it. The design guarantees that by construction: **the emitted
//! code contains no per-process value at all.** Every runtime input arrives
//! either through [`JitCtx`](crate::jit::lower::JitCtx) — `pins`, `mem_ptr`,
//! `chain_slots`, `inv_gen_ptr` — through a relocation re-checked against the
//! live module's import list by name, or, for a direct chain edge, through a
//! guest VA re-resolved against the live chain table (previous section).
//! Cranelift emits no absolute address of its own (see the relocation table
//! above), so a blob is a pure function of guest bytes plus the frame the host
//! builds around it.
//!
//! This was not free. Every edge out of a block guards against cross-thread
//! code invalidation by comparing the live `JitShared::invalidate_gen` against
//! a bake (see `lower::emit::emit_inv_gen_check`). That bake used to be an
//! **immediate constant**, per block, per compile — so a blob written by an
//! earlier process carried *that* process's generation forever and bailed to
//! the dispatcher at every chain edge. The obvious fix, restoring such blobs
//! anyway, does not help: the guard would then fail at runtime on every edge.
//! Restores were therefore gated on `record.inv_gen == live invalidate_gen`,
//! which silently turned into a chainability filter — it admitted exactly those
//! blobs whose baked constant happened to match, which is often but never
//! reliably true.
//!
//! The bake now lives in [`JitCtx::inv_gen_baked`](crate::jit::lower::JitCtx),
//! stamped per dispatcher entry, matching what `trampolines::chain_tail` has
//! always done. So no gate is needed and none is applied: a restored blob's
//! guard compares against *this* frame's generation.
//!
//! Why "adopt the live generation and chain" is correct rests on two
//! enumerable facts, neither of which any hash covers:
//!
//! 1. `JitEngine::invalidate_code_range` drops every overlapping `Ready` entry
//!    **before** the `Release` bump, so no surviving `Ready` entry's code can
//!    disagree with its current guest bytes; the background worker likewise
//!    refuses to install a block whose bytes moved mid-compile. The only two
//!    writers of `invalidate_gen` are that bump and `clear_compiled`.
//! 2. The dispatcher purges its chain table / edge IC / shadow stack whenever it
//!    observes a generation change, and that check precedes every compiled-block
//!    execution on the production path.
//!
//! Hence `guard passes` ⟺ `live == entry.gen` ⟺ no invalidation completed since
//! the entry block compiled ⟹ every `Ready` block still matches its bytes.
//!
//! Note what deliberately does **not** cover this: the guest-byte FNV-1a hashes
//! `[va, guest_end)` — the block's *own* bytes — while chaining reaches *other*
//! blocks, which is the entire exposure; and the import fingerprint covers host
//! addresses, which is orthogonal. Neither can see the reasoning above. A chain
//! edge into another block is covered instead by that *successor's* own `Ready`
//! invariant (fact 1) plus refusal-when-absent, which is why the chain section
//! above spells out how the target is re-derived rather than hashed.
//!
//! # Everything else that can invalidate a blob
//!
//! - **Format / WIE version / PE identity / opt level** — file header, see
//!   [`CODE_FORMAT_VERSION`].
//! - **Emitter configuration** — [`JitConfig::emit_fingerprint`] covers every
//!   knob that changes lowering plus build and host shape. A knob flip moves
//!   the key, so the two configurations never share a file.
//! - **Import identity** — [`IsaEngine::import_fingerprint`] over the ordered
//!   import names, plus a per-relocation name check.
//! - **Guest bytes** — FNV-1a over `[va, guest_end)`, recomputed at probe time
//!   against current guest memory.
//!
//! # Disk format
//!
//! One file per `(PE, opt level, emitter fingerprint)` under `<cache>/jit/code`,
//! named `<key:016x>.code`. Entries are whole-file rewritten on flush through a
//! temp-file rename, so a crash mid-write leaves the previous file intact.
//!
//! The cache file is untrusted input. Every length, index and offset is
//! bounds-checked before it reaches `Module::define_function_bytes`, because
//! cranelift-jit's relocation applier writes at `blob_ptr + reloc.offset` with
//! only a `debug_assert` on that offset. A recorded chain target is bounds- and
//! integrity-checked like every other field but is *not* re-derived from the
//! block's own bytes: a file edited by someone who can write the cache directory
//! could name a different live block. That is the same trust boundary the code
//! bytes themselves sit on — such a writer could replace the machine code
//! outright — and `disk_integrity` is what makes the accidental case a refusal.

use super::super::config::JitConfig;
use super::super::tier::OptTier;
use super::IsaEngine;
use crate::mem::GuestMemory;
use cranelift_codegen::binemit::Reloc;
use cranelift_module::{ModuleReloc, ModuleRelocTarget};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// File magic: `"WIECODE"` + format byte.
const MAGIC: [u8; 8] = *b"WIECODE\x01";

/// On-disk format version.
///
/// **v3** — direct chain edges are persisted. A block whose emitted code calls
/// another compiled block (a `bl` to that block's `FuncId`) is no longer
/// refused: the edge is stored as the successor's **guest VA**
/// ([`ChainReloc`]) and re-resolved against the live chain table on restore
/// (module docs). `DiskBlob` gains the `chain_relocs` list, so the layout moved
/// as well — and the bump is mandatory regardless:
/// `JitConfig::emit_fingerprint` hashes config knobs and build shape, **not**
/// emitter source, so no lowering change can move it on its own.
///
/// **v2** — the emitted invalidation guard reads its bake from `JitCtx` instead
/// of carrying a per-block compile-time immediate (module docs on the
/// no-per-process-value invariant), so a restored blob's chain edges behave
/// identically to a freshly compiled one's. `DiskBlob`'s *layout* is unchanged
/// by this, but the bump is mandatory: `JitConfig::emit_fingerprint` hashes
/// config knobs and build shape, **not** emitter source, so a lowering change
/// cannot move it. Without the bump, v1 blobs (per-block guards) and v2 blobs
/// (frame guards) would coexist in one file with different exit semantics.
///
/// **v1** was the first version that carries machine code at all. There was
/// nothing to migrate from, so that was a fresh namespace with fresh filenames
/// (`.code` next to the ledger's `.bin`) rather than a bump of an existing
/// format: a version conflict cannot be confused with a ledger reset, and a
/// stale blob from any future format is deleted on sight.
///
/// Bumping this invalidates every `.code` file, which is the correct behaviour:
/// the blobs are only meaningful under the exact layout that produced them.
const CODE_FORMAT_VERSION: u32 = 3;

/// Relocation kinds that may appear in a persisted blob, and their on-disk
/// codes.
///
/// **This list is the format's safety contract.** It is deliberately an
/// explicit mapping rather than `kind as u8`: it keeps the on-disk codes stable
/// if upstream ever reorders the enum, and it is the list a reviewer checks when
/// Cranelift gains a new address-bearing relocation. Anything not named here is
/// refused, which is the safe default.
fn reloc_code(kind: Reloc) -> Option<u8> {
    match kind {
        // `bl` to a host import (or, refused, to another compiled block).
        Reloc::Arm64Call => Some(1),
        // `adrp` + `add` pair addressing a `func_addr` of a host import.
        Reloc::Aarch64AdrPrelPgHi21 => Some(2),
        Reloc::Aarch64AddAbsLo12Nc => Some(3),
        // Absolute pointer constant for a `func_addr` of a host import.
        Reloc::Abs8 => Some(4),
        Reloc::Abs4 => Some(5),
        _ => None,
    }
}

fn reloc_from_code(code: u8) -> Option<Reloc> {
    match code {
        1 => Some(Reloc::Arm64Call),
        2 => Some(Reloc::Aarch64AdrPrelPgHi21),
        3 => Some(Reloc::Aarch64AddAbsLo12Nc),
        4 => Some(Reloc::Abs8),
        5 => Some(Reloc::Abs4),
        _ => None,
    }
}

/// Bytes `cranelift-jit`'s relocation applier writes at the relocation offset,
/// or `None` when the kind is not on disk.
fn reloc_width(code: u8) -> Option<u64> {
    match code {
        // Abs8 is the only 8-byte write (`write_unaligned::<u64>`); the rest
        // patch a single 32-bit instruction.
        4 => Some(8),
        _ => Some(4),
    }
}

/// Bounds on persisted relocation addends.
///
/// Cranelift-jit resolves a target as `get_address(name).offset(addend)`. A
/// tampered addend cannot corrupt the host (it is plain address arithmetic, no
/// dereference), but it can aim a `bl` anywhere. Genuine addends here are `0`
/// (a bare import address) or small forward offsets; 1 MiB is far above anything
/// this lowering produces and far below anything useful to an attacker.
const MAX_ABS_ADDEND: i64 = 1 << 20;

/// Hard cap on one blob's code size (corrupt / hostile-file guard). Cranelift
/// blocks are capped well below this by the pipeline's block-insn limit.
const MAX_CODE_BYTES: usize = 1 << 20;

/// Hard cap on relocations per blob. A real block has tens.
const MAX_RELOCS: usize = 4096;

/// Hard cap on deserialized blobs per file.
const MAX_DISK_BLOBS: usize = 4_000_000;

/// Code alignment ceiling and power-of-two requirement, checked before the
/// value reaches the JIT memory allocator.
const MAX_ALIGN: u64 = 4096;

/// Minimum buffered blobs before a rewrite, and how fast the bar is allowed to
/// grow with the table.
///
/// A flush is a **whole-file** rewrite (temp file + rename — the only shape that
/// survives a crash mid-write), so flushing on a fixed record count is quadratic
/// in the number of blobs: 10k blobs means ~80 rewrites of a growing file. That
/// is not a theoretical cost — it turned a 6 s 7-Zip boot into hours. Letting
/// the bar grow by a quarter of the table per flush makes the number of
/// rewrites logarithmic, i.e. total rewrite work O(n).
const APPEND_FLUSH_CAP: usize = 128;
const APPEND_FLUSH_GROWTH_NUM: usize = 4;
const APPEND_FLUSH_GROWTH_DEN: usize = 5;

/// Minimum interval between flushes (code blobs are much bigger than metadata,
/// so this is lazier and the cap above does the real work).
const FLUSH_MIN_INTERVAL: Duration = Duration::from_secs(5);

/// Why a persisted blob was not used. Counted, not just logged: a cache whose
/// hit rate is not observable cannot be tuned or trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodeRestoreOutcome {
    /// Bytes were replayed; the entry is executable.
    Restored,
    /// No blob for this VA in this file.
    Absent,
    /// Guest bytes no longer hash to the recorded value (SMC, or a stale file).
    BytesChanged,
    /// File-level identity mismatch: format, WIE version, PE, opt level, import
    /// list, or emitter fingerprint.
    FileMismatch,
    /// Passed the cheap guards but failed a structural or target check: unknown
    /// relocation kind, out-of-range offset, addend out of bounds, a target that
    /// is not a host import this module declares under the same name, or a chain
    /// edge whose successor is not chainable in this run.
    UnrelocatableTarget,
    /// Zero-length code, or an unusable alignment.
    Malformed,
}

impl CodeRestoreOutcome {
    /// Whether the block is executable as a result.
    pub(crate) fn is_restored(self) -> bool {
        matches!(self, Self::Restored)
    }

    /// Stable short name for the profile dump.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Restored => "restored",
            Self::Absent => "absent",
            Self::BytesChanged => "bytes-changed",
            Self::FileMismatch => "file-mismatch",
            Self::UnrelocatableTarget => "unrelocatable-target",
            Self::Malformed => "malformed",
        }
    }
}

/// One outgoing **direct chain edge**, recorded as the successor's guest VA.
///
/// The emitted edge is an `Arm64Call` to another compiled block's `FuncId`, and
/// a `FuncId` is an index into the declaring process's allocation order — it
/// names a different function, or nothing, in the next one. What *is* invariant
/// is the guest VA the edge transfers control to (the block's `exit_rip`), so
/// that is what is stored; restore re-resolves it against the live chain table
/// and refuses the blob when the successor is not chainable (module docs, "Chain
/// edges: a guest VA crosses the boundary, never an address").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChainReloc {
    /// Byte offset of the `bl` inside the blob's code.
    pub offset: u32,
    /// Guest VA the edge hands control to.
    pub target_va: u64,
}

/// One persisted blob: a block's pre-relocation code plus its relocations.
#[derive(Clone)]
pub(crate) struct PersistedCode {
    pub va: u64,
    /// Exclusive end of the hashed guest range; `va + guest_end - va` is also
    /// the guest byte extent the hash covers.
    pub guest_end: u64,
    pub bytes_hash: u64,
    pub insn_count: u32,
    /// `JitShared::invalidate_gen` at capture time.
    ///
    /// Recorded, hashed into the blob's integrity check and **not** a restore
    /// gate: the emitted edge guard compares the live generation against the
    /// frame's `JitCtx::inv_gen_baked`, so a blob recorded under a different
    /// generation is as replayable as one recorded under this one (module docs).
    /// Kept because it is cheap, it is what a stale-file report needs, and
    /// because dropping a field from `DiskBlob` would move the layout for no gain.
    pub inv_gen: u64,
    pub compiled_at_opt: OptTier,
    pub align: u64,
    pub code: Vec<u8>,
    /// Relocations naming a **host import** (`User{0, index}` with
    /// `index < import_count`), re-checked by name on restore.
    ///
    /// [`classify_capture_relocs`] is the only place that decides an index
    /// belongs here rather than in [`Self::chain_relocs`].
    pub relocs: Vec<ModuleReloc>,
    /// Relocations naming another compiled block, recorded as guest VAs.
    pub chain_relocs: Vec<ChainReloc>,
}

impl PersistedCode {
    /// Reject anything whose shape could make cranelift-jit's relocation
    /// applier write outside the blob it allocated.
    ///
    /// The upstream check is a `debug_assert`, i.e. absent in release — so this
    /// is the only bounds check between an untrusted file and a `write_unaligned`.
    pub(crate) fn structurally_sound(&self) -> bool {
        if self.code.is_empty() || self.code.len() > MAX_CODE_BYTES {
            return false;
        }
        if self.align == 0 || !self.align.is_power_of_two() || self.align > MAX_ALIGN {
            return false;
        }
        if self.guest_end < self.va
            || self.relocs.len().saturating_add(self.chain_relocs.len()) > MAX_RELOCS
        {
            return false;
        }
        let len = u64::try_from(self.code.len()).unwrap_or(u64::MAX);
        for r in &self.relocs {
            let Some(kind) = reloc_code(r.kind) else {
                return false;
            };
            let Some(width) = reloc_width(kind) else {
                return false;
            };
            let offset = u64::from(r.offset);
            // `offset + width <= len`, computed without overflow.
            if offset > len || width > len - offset {
                return false;
            }
            if r.addend.unsigned_abs() > MAX_ABS_ADDEND as u64 {
                return false;
            }
        }
        for (i, c) in self.chain_relocs.iter().enumerate() {
            let offset = u64::from(c.offset);
            // A chain edge is always a `bl`: one 4-byte instruction.
            if offset > len || 4 > len - offset {
                return false;
            }
            // Two relocations patching the same bytes would make one of them
            // silently win, so any collision is ambiguous rather than mergeable.
            if self.relocs.iter().any(|r| r.offset == c.offset)
                || self.chain_relocs[..i].iter().any(|o| o.offset == c.offset)
            {
                return false;
            }
            // Guest VA 0 is not code: no block is compiled there.
            if c.target_va == 0 {
                return false;
            }
        }
        true
    }

    /// Whether every relocation targets a host **import**, the only class this
    /// cache persists *without* a guest-VA re-resolution.
    ///
    /// Note what this does **not** check: that the index is below the module's
    /// import count, which is the actual import/block discriminator (module
    /// docs). That bound is [`Self::import_indices_in_range`]'s job, because it
    /// is the one form that needs the live module; [`classify_capture_relocs`]
    /// applies it before a blob is ever built.
    pub(crate) fn all_targets_are_imports(&self) -> bool {
        self.relocs
            .iter()
            .all(|r| matches!(r.name, ModuleRelocTarget::User { namespace: 0, .. }))
    }

    /// Whether every import relocation names one of `import_count` imports of the
    /// module that will define it.
    ///
    /// Load-time check: a file written for a module whose import count differs
    /// has already failed `imports_fp`, so this is belt-and-braces against a file
    /// whose *index* (not identity) is wrong — the case that would otherwise put
    /// a block `FuncId` in the import class.
    pub(crate) fn import_indices_in_range(&self, import_count: u32) -> bool {
        self.relocs.iter().all(|r| {
            matches!(
                r.name,
                ModuleRelocTarget::User {
                    namespace: 0,
                    index
                } if index < import_count
            )
        })
    }

    /// The relocation list this blob must be defined with **in this process**.
    ///
    /// `live_chain` re-resolves a recorded chain edge's guest VA to the `FuncId`
    /// this run compiled for that VA (its only implementation is
    /// `JitShared::chain_func_for`). `None` — the caller refuses the whole blob
    /// and compiles instead — when any successor is not chainable now, which is
    /// the one direction that must never be guessed at.
    pub(crate) fn resolved_relocs(
        &self,
        live_chain: impl Fn(u64) -> Option<cranelift_module::FuncId>,
    ) -> Option<Vec<ModuleReloc>> {
        let mut out = Vec::with_capacity(self.relocs.len().saturating_add(self.chain_relocs.len()));
        out.extend_from_slice(&self.relocs);
        for c in &self.chain_relocs {
            let func_id = live_chain(c.target_va)?;
            out.push(ModuleReloc {
                offset: c.offset,
                kind: Reloc::Arm64Call,
                name: ModuleRelocTarget::User {
                    namespace: 0,
                    index: func_id.as_u32(),
                },
                addend: 0,
            });
        }
        Some(out)
    }
}

/// Split a captured block's relocations into the format's two replayable
/// classes — host imports and direct chain edges — or `None` when the block
/// carries anything else.
///
/// **This is the whole policy in one place**, and it is total: every relocation
/// lands in exactly one class or takes the whole block out of the cache.
/// `import_count` is the live module's (`FuncId`s below it are imports because
/// they are declared first), and `chain_va` maps a block `FuncId` of the same
/// tier back to the guest VA this run installed it at — `None` there means the
/// capture names a block this run can no longer name, which is a refusal rather
/// than a guess.
pub(crate) fn classify_capture_relocs(
    raw: Vec<ModuleReloc>,
    import_count: u32,
    chain_va: impl Fn(cranelift_module::FuncId) -> Option<u64>,
) -> Option<(Vec<ModuleReloc>, Vec<ChainReloc>)> {
    let mut imports = Vec::with_capacity(raw.len());
    let mut chain = Vec::new();
    for r in raw {
        let ModuleRelocTarget::User {
            namespace: 0,
            index,
        } = r.name
        else {
            return None;
        };
        if index < import_count {
            imports.push(r);
            continue;
        }
        // A block `FuncId`. Only a direct call carries a guest-VA-shaped
        // meaning; a data reference to a block has none this format can
        // re-derive, and a non-zero addend is not something a `bl` produces.
        if !matches!(r.kind, Reloc::Arm64Call) || r.addend != 0 {
            return None;
        }
        chain.push(ChainReloc {
            offset: r.offset,
            target_va: chain_va(cranelift_module::FuncId::from_u32(index))?,
        });
    }
    Some((imports, chain))
}

#[derive(bincode::Encode, bincode::Decode)]
struct FileCodeBody {
    wie_version: String,
    format_version: u32,
    /// `super::super::cache_persist::jit_cache_key(pe_hash, opt_level)`, further
    /// mixed with [`JitConfig::emit_fingerprint`].
    key: u64,
    opt_level: String,
    /// [`IsaEngine::import_fingerprint`] when this file was written.
    imports_fp: u64,
    /// [`JitConfig::emit_fingerprint`] when this file was written.
    emit_fp: u64,
    blobs: Vec<DiskBlob>,
}

/// FNV-1a accumulator, matching the rest of the crate's on-disk hashing.
fn fnv(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// Digest over every field that decides what the blob *is*.
///
/// Without this, a truncated or bit-flipped file still decodes cleanly: bincode
/// is not self-checking, and the relocation offsets are only range-checked. The
/// result would be host code that decodes, maps and runs, having been silently
/// altered in the middle. This is the cache file's checksum, and it is the last
/// line of defence after every structural check.
fn disk_integrity(b: &DiskBlob) -> u64 {
    let mut h = FNV_OFFSET;
    h = fnv(h, &b.va.to_le_bytes());
    h = fnv(h, &b.guest_end.to_le_bytes());
    h = fnv(h, &b.bytes_hash.to_le_bytes());
    h = fnv(h, &b.insn_count.to_le_bytes());
    h = fnv(h, &b.inv_gen.to_le_bytes());
    h = fnv(h, &[b.compiled_at_opt]);
    h = fnv(h, &b.align.to_le_bytes());
    h = fnv(h, &b.code);
    for r in &b.relocs {
        h = fnv(h, &r.offset.to_le_bytes());
        h = fnv(h, &[r.kind]);
        h = fnv(h, &r.target.to_le_bytes());
        h = fnv(h, &r.addend.to_le_bytes());
    }
    for c in &b.chain_relocs {
        h = fnv(h, &c.offset.to_le_bytes());
        h = fnv(h, &c.target_va.to_le_bytes());
    }
    h
}

#[derive(bincode::Encode, bincode::Decode)]
struct DiskBlob {
    /// [`disk_integrity`] over every field below. See its doc comment: this is
    /// what makes a corrupted file a refusal rather than altered host code.
    integrity: u64,
    va: u64,
    guest_end: u64,
    bytes_hash: u64,
    insn_count: u32,
    inv_gen: u64,
    /// [`OptTier::code`]; `None` on decode is a rejected record, never a guess.
    compiled_at_opt: u8,
    align: u64,
    code: Vec<u8>,
    relocs: Vec<DiskReloc>,
    /// Direct chain edges, kept apart from `relocs` because their target is a
    /// guest VA rather than an import index (module docs).
    chain_relocs: Vec<DiskChainReloc>,
}

#[derive(bincode::Encode, bincode::Decode)]
struct DiskReloc {
    offset: u32,
    kind: u8,
    /// `User{namespace: 0, index}` — an index into [`IsaEngine`]'s import list.
    target: u32,
    addend: i64,
}

#[derive(bincode::Encode, bincode::Decode)]
struct DiskChainReloc {
    offset: u32,
    /// Successor guest VA, re-resolved against the live chain table on restore.
    target_va: u64,
}

fn disk_blob(p: &PersistedCode) -> Option<DiskBlob> {
    let mut relocs = Vec::with_capacity(p.relocs.len());
    for r in &p.relocs {
        let ModuleRelocTarget::User { index, .. } = r.name else {
            return None;
        };
        relocs.push(DiskReloc {
            offset: r.offset,
            kind: reloc_code(r.kind)?,
            target: index,
            addend: r.addend,
        });
    }
    let chain_relocs = p
        .chain_relocs
        .iter()
        .map(|c| DiskChainReloc {
            offset: c.offset,
            target_va: c.target_va,
        })
        .collect();
    let mut b = DiskBlob {
        integrity: 0,
        va: p.va,
        guest_end: p.guest_end,
        bytes_hash: p.bytes_hash,
        insn_count: p.insn_count,
        inv_gen: p.inv_gen,
        compiled_at_opt: p.compiled_at_opt.code(),
        align: p.align,
        code: p.code.clone(),
        relocs,
        chain_relocs,
    };
    b.integrity = disk_integrity(&b);
    Some(b)
}

fn from_disk(b: DiskBlob) -> Option<PersistedCode> {
    if disk_integrity(&b) != b.integrity {
        return None;
    }
    let compiled_at_opt = OptTier::from_code(b.compiled_at_opt)?;
    let mut relocs = Vec::with_capacity(b.relocs.len());
    for r in b.relocs {
        relocs.push(ModuleReloc {
            offset: r.offset,
            kind: reloc_from_code(r.kind)?,
            name: ModuleRelocTarget::User {
                namespace: 0,
                index: r.target,
            },
            addend: r.addend,
        });
    }
    let chain_relocs = b
        .chain_relocs
        .into_iter()
        .map(|c| ChainReloc {
            offset: c.offset,
            target_va: c.target_va,
        })
        .collect();
    Some(PersistedCode {
        va: b.va,
        guest_end: b.guest_end,
        bytes_hash: b.bytes_hash,
        insn_count: b.insn_count,
        inv_gen: b.inv_gen,
        compiled_at_opt,
        align: b.align,
        code: b.code,
        relocs,
        chain_relocs,
    })
}

/// Counters for the profile dump. The whole point of the feature is that it is
/// measurable: without a hit rate, "persist the code" and "persist nothing" are
/// indistinguishable from the outside.
#[derive(Debug, Default)]
pub(crate) struct CodeCacheCounters {
    /// Times the cache was **consulted** with a guest VA — one per
    /// [`CodeCache::probe`], whatever came back: a hit, a refusal, or no blob at
    /// all.
    ///
    /// This exists because `restored=0 refused=0` was unreadable: without a
    /// consultation count, "nobody asked the cache" and "the cache was asked and
    /// had nothing to offer" print the same two zeros, and the difference
    /// decides whether a startup cost is a miss or a missing call site. A
    /// counter that only fired on a hit would reproduce exactly that blindness,
    /// so this one is incremented *before* the table lookup and cannot be
    /// short-circuited by a miss, an unattached cache, or a `?`.
    pub probes: AtomicU64,
    pub restored: AtomicU64,
    pub compiled: AtomicU64,
    /// Blobs successfully replayed (one per `restored`; kept separate so the
    /// summary can be diffed against a suspected over-count).
    pub blocks_replayed: AtomicU64,
    /// Total machine-code bytes handed to `define_function_bytes`.
    ///
    /// The load-independent cost signal for lazy restore: it is exactly the
    /// memcpy + relocation-application work the restore path performed, with no
    /// timing in it. Compare against `compiled` to see how much emitted code a
    /// warm boot avoided producing; divide by `restored` for mean block size.
    pub bytes_replayed: AtomicU64,
    pub probe_absent: AtomicU64,
    pub refused_bytes_changed: AtomicU64,
    pub refused_file_mismatch: AtomicU64,
    pub refused_unrelocatable: AtomicU64,
    pub refused_malformed: AtomicU64,
    /// Blocks whose emitted code had a relocation this format cannot replay — a
    /// data reference to another block, a libcall, or a chain edge whose
    /// successor this run cannot name — so they were recorded as metadata only.
    /// Non-zero is expected and healthy.
    pub skipped_unpersistable: AtomicU64,
    /// Successful whole-file rewrites performed. Equals 1 per session with the
    /// session-teardown flush, and proves the flush is no longer dependent on
    /// the last `Arc` dropping.
    pub flushes_written: AtomicU64,
    /// Blobs found and validated but NOT replayed, because another thread held
    /// the JIT engine lock at that instant. Non-zero is normal and cheap: the
    /// block compiles normally this visit and restores on a later one. A large
    /// number means the restore path is losing the lock race constantly, which
    /// is the signature of a guest thread being starved behind compiles.
    pub restore_deferred: AtomicU64,
    /// Times the exit summary was actually emitted. Exactly 1 for the life of
    /// the process even when both the explicit session flush and `Drop` run —
    /// a counter rather than a log assertion, so a test can pin it.
    pub summaries_reported: AtomicU64,
}

impl CodeCacheCounters {
    /// Plain-data copy, so callers can hold a snapshot without borrowing the
    /// cache (which [`super::JitShared::code_cache`] cannot hand out).
    pub(crate) fn snapshot(&self) -> CodeCacheCounts {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        CodeCacheCounts {
            probes: g(&self.probes),
            restored: g(&self.restored),
            compiled: g(&self.compiled),
            blocks_replayed: g(&self.blocks_replayed),
            bytes_replayed: g(&self.bytes_replayed),
            missing: g(&self.probe_absent),
            refused_bytes_changed: g(&self.refused_bytes_changed),
            refused_file_mismatch: g(&self.refused_file_mismatch),
            refused_unrelocatable: g(&self.refused_unrelocatable),
            refused_malformed: g(&self.refused_malformed),
            skipped_unpersistable: g(&self.skipped_unpersistable),
            flushes_written: g(&self.flushes_written),
            restore_deferred: g(&self.restore_deferred),
            summaries_reported: g(&self.summaries_reported),
        }
    }

    pub(crate) fn record(&self, outcome: CodeRestoreOutcome) {
        let slot = match outcome {
            CodeRestoreOutcome::Restored => {
                self.blocks_replayed.fetch_add(1, Ordering::Relaxed);
                &self.restored
            }
            CodeRestoreOutcome::Absent => &self.probe_absent,
            CodeRestoreOutcome::BytesChanged => &self.refused_bytes_changed,
            CodeRestoreOutcome::FileMismatch => &self.refused_file_mismatch,
            CodeRestoreOutcome::UnrelocatableTarget => &self.refused_unrelocatable,
            CodeRestoreOutcome::Malformed => &self.refused_malformed,
        };
        slot.fetch_add(1, Ordering::Relaxed);
    }
}

/// Plain snapshot of [`CodeCacheCounters`]; every field documented there.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CodeCacheCounts {
    /// Consultations; see [`CodeCacheCounters::probes`]. Always `>= restored +
    /// refused() + missing + restore_deferred`, and equal to that sum once the
    /// deferred case is included: every probe resolves into exactly one of them.
    pub probes: u64,
    pub restored: u64,
    pub compiled: u64,
    pub blocks_replayed: u64,
    pub bytes_replayed: u64,
    pub missing: u64,
    pub refused_bytes_changed: u64,
    pub refused_file_mismatch: u64,
    pub refused_unrelocatable: u64,
    pub refused_malformed: u64,
    pub skipped_unpersistable: u64,
    pub flushes_written: u64,
    pub restore_deferred: u64,
    pub summaries_reported: u64,
}

impl CodeCacheCounts {
    /// Every refusal, summed.
    pub(crate) fn refused(&self) -> u64 {
        self.refused_bytes_changed
            .saturating_add(self.refused_file_mismatch)
            .saturating_add(self.refused_unrelocatable)
            .saturating_add(self.refused_malformed)
    }

    /// Fraction of "we had something to offer" cases that were replayed, in
    /// percent. The denominator is deliberately `restored + every refusal`, not
    /// `compiled + restored`: a block with no blob at all is an *absent* cache,
    /// not a failed restore, and folding it in would make the ratio a measure of
    /// guest coverage rather than of restore correctness.
    pub(crate) fn hit_rate_pct(&self) -> u64 {
        let attempted = self.restored.saturating_add(self.refused());
        if attempted == 0 {
            return 0;
        }
        (self.restored.saturating_mul(100)) / attempted
    }
}

struct FlushState {
    /// Records appended since the last successful write.
    ///
    /// A safety net for one narrow window, not a second source of truth: the
    /// table scan in [`CodeCache::write_file_now`] is a lock-free iteration, so
    /// an insert that lands after the iteration has passed its slot would
    /// otherwise be missing from that rewrite. `record` inserts into the table
    /// *before* it queues here, so every record this list can contribute is
    /// already in the table as well — and the table is authoritative:
    /// [`CodeCache::write_file_now`] skips any queued record whose VA is no
    /// longer in it.
    pending: Vec<PersistedCode>,
    last_flush: Instant,
    warned_err: bool,
}

/// On-disk machine-code cache. Constructed always; inert unless enabled.
///
/// Deliberately independent of the metadata ledger: it carries its own guest
/// byte hash, so it can be enabled (or diagnosed) without touching the ledger's
/// files or its warm-up behaviour.
pub(crate) struct CodeCache {
    enabled: bool,
    base_dir: PathBuf,
    /// Per-(PE, opt level) tables of blobs, keyed like the ledger.
    ///
    /// `papaya` for **O(1) insert**, which is not an optimisation here but a
    /// correctness-of-scale requirement: a guest like 7-Zip records >10k blobs
    /// in one run, so a copy-on-write map (clone the table per record) is
    /// quadratic *in bytes copied* and turns a 6 s boot into hours. Same choice
    /// as the metadata ledger for the same reason.
    tables: RwLock<ahash::HashMap<u64, Arc<papaya::HashMap<u64, PersistedCode>>>>,
    /// The attached file's identity, checked on every probe.
    identity: RwLock<Option<FileIdentity>>,
    flush: Mutex<FlushState>,
    counters: CodeCacheCounters,
    /// Set by any mutation that changes what a flush would write, and cleared
    /// only by a **successful** write. This is what makes [`CodeCache::flush`]
    /// idempotent in the cheap direction: a second flush with nothing recorded
    /// since is a no-op rather than a second identical rewrite.
    dirty: AtomicU64,
    /// One-shot guard for the exit summary. `flush` may legitimately run twice
    /// (explicit teardown, then `Drop` as a backstop) and the counters must be
    /// reported exactly once.
    summary_reported: std::sync::atomic::AtomicBool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    key: u64,
    imports_fp: u64,
    emit_fp: u64,
    opt_level: &'static str,
    matched: bool,
}

impl CodeCache {
    /// Resolve `WIE_JIT_CODE_CACHE`. **Off unless explicitly enabled** — see
    /// [`JitConfig::code_cache_enabled`].
    pub(crate) fn new() -> Self {
        let enabled = JitConfig::get().code_cache_enabled();
        let base_dir = if enabled {
            crate::jit::cache_persist::code_cache_dir()
        } else {
            PathBuf::new()
        };
        Self {
            enabled,
            base_dir,
            tables: RwLock::new(ahash::HashMap::default()),
            identity: RwLock::new(None),
            flush: Mutex::new(FlushState {
                pending: Vec::new(),
                last_flush: Instant::now(),
                warned_err: false,
            }),
            counters: CodeCacheCounters::default(),
            dirty: AtomicU64::new(0),
            summary_reported: AtomicBool::new(false),
        }
    }

    /// Test-only constructor: an explicitly enabled cache rooted at `dir`,
    /// bypassing the (correctly) off-by-default env gate.
    #[cfg(test)]
    pub(crate) fn enabled_in(dir: PathBuf, opt_level: &'static str) -> Self {
        let mut me = Self {
            enabled: true,
            base_dir: dir,
            tables: RwLock::new(ahash::HashMap::default()),
            identity: RwLock::new(None),
            flush: Mutex::new(FlushState {
                pending: Vec::new(),
                last_flush: Instant::now(),
                warned_err: false,
            }),
            counters: CodeCacheCounters::default(),
            dirty: AtomicU64::new(0),
            summary_reported: AtomicBool::new(false),
        };
        me.identity = RwLock::new(Some(FileIdentity {
            key: 0,
            imports_fp: 0,
            emit_fp: 0,
            opt_level,
            matched: true,
        }));
        me
    }

    /// Whether restoration is armed at all.
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Whether a PE has been attached and the file's identity was accepted.
    pub(crate) fn attached(&self) -> bool {
        self.enabled
            && self
                .identity
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .is_some_and(|i| i.matched)
    }

    pub(crate) fn counters(&self) -> &CodeCacheCounters {
        &self.counters
    }

    /// Bind this cache to a PE and re-read the file identity from the live
    /// engine. Idempotent per `(pe_hash, opt_level)`.
    pub(crate) fn attach(&self, pe_hash: u64, opt_level: &'static str, eng: &IsaEngine) {
        if !self.enabled || pe_hash == 0 {
            return;
        }
        let key = super::super::cache_persist::jit_cache_key(pe_hash, opt_level);
        let identity = FileIdentity {
            key,
            imports_fp: eng.import_fingerprint(),
            emit_fp: JitConfig::get().emit_fingerprint(),
            opt_level,
            matched: true,
        };
        {
            let tables = self.tables.read().unwrap_or_else(|e| e.into_inner());
            if tables.contains_key(&key) {
                *self.identity.write().unwrap_or_else(|e| e.into_inner()) = Some(identity);
                return;
            }
        }
        let map = self.load_file(&identity, eng.import_count());
        {
            let mut tables = self.tables.write().unwrap_or_else(|e| e.into_inner());
            tables.insert(key, Arc::new(map));
        }
        *self.identity.write().unwrap_or_else(|e| e.into_inner()) = Some(identity);
    }

    fn file_path(&self, key: u64) -> PathBuf {
        self.base_dir.join(format!("{key:016x}.code"))
    }

    fn load_file(
        &self,
        identity: &FileIdentity,
        import_count: u32,
    ) -> papaya::HashMap<u64, PersistedCode> {
        let path = self.file_path(identity.key);
        let Ok(raw) = fs::read(&path) else {
            return papaya::HashMap::new(); // cold boot, stay quiet
        };
        let decoded = (|| {
            if raw.len() < MAGIC.len() || raw[..MAGIC.len()] != MAGIC {
                return None;
            }
            let (body, _): (FileCodeBody, usize) =
                bincode::decode_from_slice(&raw[MAGIC.len()..], bincode::config::standard())
                    .ok()?;
            if body.format_version != CODE_FORMAT_VERSION
                || body.wie_version != env!("CARGO_PKG_VERSION")
                || body.key != identity.key
                || body.opt_level != identity.opt_level
                || body.imports_fp != identity.imports_fp
                || body.emit_fp != identity.emit_fp
                || body.blobs.len() > MAX_DISK_BLOBS
            {
                return None;
            }
            Some(body)
        })();

        let Some(body) = decoded else {
            // Version / identity mismatch resets the file. Deleting rather than
            // leaving it readable is deliberate: a blob we cannot prove belongs
            // to this build is worth nothing, and `rename` in will not fix it.
            if fs::remove_file(&path).is_ok() {
                tracing::info!(
                    key = format_args!("{:#x}", identity.key),
                    "jit code cache identity mismatch — reset"
                );
            }
            return papaya::HashMap::new();
        };

        let map = papaya::HashMap::with_capacity(body.blobs.len());
        let mut rejected = 0_u64;
        {
            let pin = map.pin();
            for b in body.blobs {
                let Some(p) = from_disk(b) else {
                    rejected = rejected.saturating_add(1);
                    continue;
                };
                if !p.structurally_sound()
                    || !p.all_targets_are_imports()
                    || !p.import_indices_in_range(import_count)
                {
                    rejected = rejected.saturating_add(1);
                    continue;
                }
                pin.insert(p.va, p);
            }
        }
        if rejected > 0 {
            tracing::info!(
                key = format_args!("{:#x}", identity.key),
                rejected,
                "jit code cache blobs rejected at load"
            );
        }
        map
    }

    fn table(&self) -> Option<Arc<papaya::HashMap<u64, PersistedCode>>> {
        let identity = (*self.identity.read().unwrap_or_else(|e| e.into_inner()))?;
        let tables = self.tables.read().unwrap_or_else(|e| e.into_inner());
        tables.get(&identity.key).cloned()
    }

    /// Persist one block's emitted code.
    ///
    /// Skipped (counted, not silent) when the block carries a relocation this
    /// format refuses, or when it would not pass
    /// [`PersistedCode::structurally_sound`]. Blocks whose *capture* has to be
    /// classified before a [`PersistedCode`] can exist at all — every one of
    /// them, since [`classify_capture_relocs`] is what splits imports from chain
    /// edges — are counted by [`Self::note_unpersistable`] instead.
    pub(crate) fn record(&self, code: PersistedCode) {
        if !self.enabled || !code.structurally_sound() || !code.all_targets_are_imports() {
            self.counters
                .skipped_unpersistable
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        let va = code.va;
        self.counters.compiled.fetch_add(1, Ordering::Relaxed);
        let Some(identity) = *self.identity.read().unwrap_or_else(|e| e.into_inner()) else {
            return;
        };
        if !self
            .tables
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&identity.key)
        {
            return;
        }
        // In-place insert: `papaya` gives lock-free reads for the probe path and
        // an O(1) write here, with no whole-table copy.
        let Some(table) = self.table() else {
            return;
        };
        table.pin().insert(va, code.clone());
        {
            let mut st = self.flush.lock().unwrap_or_else(|e| e.into_inner());
            st.pending.push(code);
        }
        self.dirty.store(1, Ordering::Release);
        self.flush_tick();
    }

    /// Count one block whose captured relocations this format cannot replay.
    ///
    /// The `skipped_unpersistable` bucket for the case that never reaches
    /// [`Self::record`]: [`classify_capture_relocs`] rejected the capture (a data
    /// reference to another block, a libcall, or a chain edge whose successor
    /// this run can no longer name). Same counter, so the report stays one number
    /// meaning "emitted code we could not prove replayable".
    pub(crate) fn note_unpersistable(&self) {
        if self.enabled {
            self.counters
                .skipped_unpersistable
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Byte-validated lookup.
    ///
    /// There is deliberately no invalidation-generation check here. The
    /// emitted guard reads its bake from the frame's `JitCtx`, not from a
    /// compile-time literal, so a blob recorded under a different
    /// `invalidate_gen` behaves exactly like a freshly compiled one (module docs
    /// on the no-per-process-value invariant). A generation gate here would only
    /// have re-imposed the chainability filter that invariant removed.
    pub(crate) fn probe(&self, mem: &GuestMemory, va: u64) -> Option<PersistedCode> {
        // First statement on purpose: a consultation that finds no table, no
        // blob or unreadable bytes is still a consultation, and that is the
        // distinction `restored=0 refused=0` could not express.
        self.counters.probes.fetch_add(1, Ordering::Relaxed);
        let p = self.table()?.pin().get(&va).cloned()?;
        let hash = super::super::cache_persist::hash_guest_range(mem, va, p.guest_end)?;
        (hash == p.bytes_hash).then_some(p)
    }

    /// Classify a probe miss so the counters say *why* the cache is cold.
    pub(crate) fn classify_miss(&self, mem: &GuestMemory, va: u64) -> CodeRestoreOutcome {
        let Some(identity) = *self.identity.read().unwrap_or_else(|e| e.into_inner()) else {
            return CodeRestoreOutcome::Absent;
        };
        if !identity.matched {
            return CodeRestoreOutcome::FileMismatch;
        }
        let Some(table) = self.table() else {
            return CodeRestoreOutcome::Absent;
        };
        let p = table.pin().get(&va).cloned();
        let Some(p) = p else {
            return CodeRestoreOutcome::Absent;
        };
        match super::super::cache_persist::hash_guest_range(mem, va, p.guest_end) {
            Some(h) if h == p.bytes_hash => CodeRestoreOutcome::Restored,
            _ => CodeRestoreOutcome::BytesChanged,
        }
    }

    /// Count one restore attempt's outcome.
    /// Record a blob that was found but not replayed because the JIT engine
    /// lock was held by another thread.
    pub(crate) fn note_restore_deferred(&self) {
        self.counters
            .restore_deferred
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record a successful replay of `code_bytes` of machine code.
    pub(crate) fn note_restored(&self, code_bytes: usize) {
        self.counters
            .bytes_replayed
            .fetch_add(code_bytes as u64, Ordering::Relaxed);
        self.note(CodeRestoreOutcome::Restored);
    }

    pub(crate) fn note(&self, outcome: CodeRestoreOutcome) {
        self.counters.record(outcome);
        if outcome.is_restored() {
            tracing::debug!(outcome = outcome.label(), "jit code cache outcome");
        } else if !matches!(outcome, CodeRestoreOutcome::Absent) {
            tracing::debug!(outcome = outcome.label(), "jit code cache refused a blob");
        }
    }

    /// Records buffered before the next whole-file rewrite is due: the fixed
    /// floor, plus a quarter of the current table so the rewrite count stays
    /// logarithmic in the number of blobs.
    fn flush_threshold(&self, table_len: usize) -> usize {
        APPEND_FLUSH_CAP
            .saturating_add(table_len / APPEND_FLUSH_GROWTH_DEN * APPEND_FLUSH_GROWTH_NUM)
    }

    fn flush_tick(&self) {
        let table_len = self.table().map_or(0, |t| t.pin().len());
        let due = {
            let st = self.flush.lock().unwrap_or_else(|e| e.into_inner());
            st.pending.len() >= self.flush_threshold(table_len)
                || st.last_flush.elapsed() >= FLUSH_MIN_INTERVAL
        };
        if due {
            self.flush();
        }
    }

    /// Rewrite the attached file through a temp-file rename.
    /// Rewrite the attached file if anything changed since the last successful
    /// write. Idempotent: a call with no intervening mutation does nothing.
    ///
    /// Public because teardown order is easy to get wrong twice; safe to call
    /// any number of times from any number of places.
    pub(crate) fn flush(&self) {
        if !self.enabled {
            return;
        }
        // Claim the write: `swap` returns the previous value, so 0 means
        // nothing has been recorded since the last successful write and the
        // whole rewrite can be skipped. Claiming it up front also serialises
        // two concurrent flushes — the loser's `pending` is still in the table
        // snapshot the winner takes.
        if self.dirty.swap(0, Ordering::AcqRel) == 0 {
            return;
        }
        self.write_file_now();
    }

    /// Flush, then report the counters **exactly once**.
    ///
    /// This is the deterministic end-of-session entry point: it does not depend
    /// on the last `Arc<CodeCache>` happening to drop, which is *not* something
    /// a guest can be relied on to do (see [`super::shared`]'s
    /// `RuntimeSession` teardown note). [`Drop`] calls it as a backstop, and the
    /// `summary_reported` swap makes the double call harmless.
    pub(crate) fn finish(&self) {
        if !self.enabled {
            return;
        }
        self.flush();
        if self.summary_reported.swap(true, Ordering::AcqRel) {
            return;
        }
        self.counters
            .summaries_reported
            .fetch_add(1, Ordering::Relaxed);
        tracing::info!("jit code cache: {}", self.summary().unwrap_or_default());
    }

    /// The actual temp-file + rename write. Only reached when [`Self::flush`]
    /// saw pending mutations, so [`Self::dirty`] stays authoritative.
    fn write_file_now(&self) {
        let Some(table) = self.table() else {
            return;
        };
        let Some(identity) = *self.identity.read().unwrap_or_else(|e| e.into_inner()) else {
            return;
        };
        let pending = {
            let mut st = self.flush.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut st.pending)
        };
        let mut blobs = Vec::with_capacity(table.pin().len() + pending.len());
        let pin = table.pin();
        for p in pin.values() {
            if let Some(b) = disk_blob(p) {
                blobs.push(b);
            }
        }
        for p in &pending {
            // The table is authoritative, and this is where that is enforced:
            // a queued record whose blob has left the table since — an
            // invalidated range, a replaced entry — must not be written back.
            // `record` inserts into the table *before* it queues, so a flush that
            // raced the push can only ever miss a live blob here, never resurrect
            // a dead one; doing this at the invalidation site instead could not
            // cover that race, and would have to be repeated at every future way
            // a blob can leave the table.
            if pin.get(&p.va).is_none() {
                continue;
            }
            if !blobs.iter().any(|b| b.va == p.va)
                && let Some(b) = disk_blob(p)
            {
                blobs.push(b);
            }
        }
        if blobs.is_empty() {
            return;
        }
        let body = FileCodeBody {
            wie_version: env!("CARGO_PKG_VERSION").to_string(),
            format_version: CODE_FORMAT_VERSION,
            key: identity.key,
            opt_level: identity.opt_level.to_string(),
            imports_fp: identity.imports_fp,
            emit_fp: identity.emit_fp,
            blobs,
        };
        let path = self.file_path(identity.key);
        let written = write_file(&path, &body);
        match written {
            Ok(()) => {
                self.counters
                    .flushes_written
                    .fetch_add(1, Ordering::Relaxed);
                let mut st = self.flush.lock().unwrap_or_else(|e| e.into_inner());
                st.last_flush = Instant::now();
            }
            Err(e) => {
                // Re-arm `dirty` so a later `flush()` (the session hook, or the
                // `Drop` backstop) retries instead of silently dropping the
                // only on-disk copy of these blobs. We warn once either way:
                // a cache that cannot be written must not look like a cache
                // that has nothing to save.
                self.dirty.store(1, Ordering::Release);
                let mut st = self.flush.lock().unwrap_or_else(|e| e.into_inner());
                if !st.warned_err {
                    st.warned_err = true;
                    tracing::warn!(
                        error = %e,
                        path = %path.display(),
                        "jit code cache write failed — code persistence degraded for this run"
                    );
                }
            }
        }
    }

    /// Drop every blob overlapping `[addr, end)` and rewrite (SMC / X-loss).
    ///
    /// Queued `pending` copies are **not** filtered here: [`Self::write_file_now`]
    /// drops any queued record whose VA is no longer in the table, which covers
    /// this case in one place and also copes with `record`'s insert-then-queue
    /// ordering (filtering here would race that push).
    ///
    /// **Known limit.** The rewrite is a whole-table snapshot and is skipped when
    /// that snapshot is empty, so invalidating the *last* blob leaves the previous
    /// file — and its now-dead record — on disk until something else dirties the
    /// cache. A later process then pays one failed byte-hash per probe of that VA,
    /// which is the cost the caller's doc comment says this method avoids. It is a
    /// staleness cost, not a correctness one: the blob is still a pure function of
    /// those guest bytes, so restoring it would be sound.
    pub(crate) fn invalidate_range(&self, addr: u64, end: u64) {
        if !self.enabled || end <= addr {
            return;
        }
        let Some(table) = self.table() else {
            return;
        };
        let doomed: Vec<u64> = {
            let pin = table.pin();
            pin.iter()
                .filter(|(_, p)| p.va < end && addr < p.guest_end.max(p.va))
                .map(|(&va, _)| va)
                .collect()
        };
        if doomed.is_empty() {
            return;
        }
        let pin = table.pin();
        for va in doomed {
            pin.remove(&va);
        }
        drop(pin);
        self.dirty.store(1, Ordering::Release);
        self.flush();
    }

    /// One-line hit-rate summary, or `None` when the cache was never enabled.
    ///
    /// The primary success metric, formatted so it can be pasted straight into
    /// a report: `restored` blocks replayed against `compiled` blocks Cranelift
    /// still had to emit, plus every refusal broken out by reason.
    pub(crate) fn summary(&self) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let c = &self.counters;
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        Some(format!(
            "probes={} restored={} compiled={} unpersistable={} \
             refused(bytes={} file={} target={} malformed={}) missing={} \
             replayed={}",
            g(&c.probes),
            g(&c.restored),
            g(&c.compiled),
            g(&c.skipped_unpersistable),
            g(&c.refused_bytes_changed),
            g(&c.refused_file_mismatch),
            g(&c.refused_unrelocatable),
            g(&c.refused_malformed),
            g(&c.probe_absent),
            g(&c.blocks_replayed),
        ))
        .map(|line| {
            let c = &self.counters;
            format!(
                "{line} replayed_bytes={} flushes={} deferred={}",
                c.bytes_replayed.load(Ordering::Relaxed),
                c.flushes_written.load(Ordering::Relaxed),
                c.restore_deferred.load(Ordering::Relaxed)
            )
        })
        .map(|line| {
            let counts = self.counters.snapshot();
            format!("{line} hit_rate={}%", counts.hit_rate_pct())
        })
    }

    /// Captured blob for `va`, for tests that assert on what was emitted.
    #[cfg(test)]
    pub(crate) fn test_blob(&self, va: u64) -> Option<PersistedCode> {
        self.table()?.pin().get(&va).cloned()
    }

    /// Guest VAs currently held in the table (test failure output).
    #[cfg(test)]
    pub(crate) fn test_vas(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self
            .table()
            .map(|t| t.pin().keys().copied().collect())
            .unwrap_or_default();
        v.sort_unstable();
        v
    }

    /// Whether the file identity resolved to an attached table (test output).
    #[cfg(test)]
    pub(crate) fn test_attached_key(&self) -> Option<u64> {
        (*self.identity.read().unwrap_or_else(|e| e.into_inner())).map(|i| i.key)
    }

    #[cfg(test)]
    pub(crate) fn test_len(&self) -> usize {
        self.table().map_or(0, |t| t.pin().len())
    }
}

/// Backstop only.
///
/// The deterministic flush is [`CodeCache::finish`], called from guest-session
/// teardown; this exists for the paths that skip teardown entirely (an aborted
/// CLI run, a panic unwinding past the session). It is a no-op when the session
/// already finished, and vice versa.
impl Drop for CodeCache {
    fn drop(&mut self) {
        CodeCache::finish(self);
    }
}

fn write_file(path: &Path, body: &FileCodeBody) -> Result<(), String> {
    let mut payload = Vec::with_capacity(MAGIC.len() + 4096);
    payload.extend_from_slice(&MAGIC);
    payload.extend_from_slice(
        &bincode::encode_to_vec(body, bincode::config::standard()).map_err(|e| e.to_string())?,
    );
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        let mut f = fs::File::create(&tmp).map_err(|e| e.to_string())?;
        f.write_all(&payload).map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, path).map_err(|e| e.to_string())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod probe_counter_tests {
    use super::*;

    fn mem_with(va: u64, bytes: &[u8]) -> GuestMemory {
        let mut mem = GuestMemory::new();
        let base = va & !0xFFF;
        let size = usize::try_from((bytes.len() as u64) + 0x2000).expect("size") & !0xFFF;
        mem.map(base, size, crate::RwxPerms::ALL).expect("map");
        mem.write(va, bytes).expect("write");
        mem
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("wie-cc-probe-counter-{tag}-{}", std::process::id()));
        match std::fs::remove_dir_all(&dir) {
            Ok(()) | Err(_) => {}
        }
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    /// The counter must fire on **misses**, not only on hits.
    ///
    /// The reading that started this slice was `restored=0 refused=0`, which is
    /// what *both* "nobody consulted the cache" and "it was consulted and had
    /// nothing to offer" print. A counter incremented on a hit — or on a
    /// `record` — would pass a hit-shaped test and stay blind to the case that
    /// mattered, so this asserts the miss, and the assertion is that `probes`
    /// exceeds every resolved outcome put together.
    #[test]
    fn a_miss_is_a_consultation() {
        let cache = CodeCache::enabled_in(scratch("miss"), JitConfig::get().opt_level());
        let mem = mem_with(0x4000_0000, &[0x90, 0x90, 0x90, 0x90]);

        // No PE attached, no blob, nothing to find: a miss, twice.
        assert!(cache.probe(&mem, 0x4000_0000).is_none());
        assert_eq!(cache.counters().snapshot().probes, 1, "a miss is a probe");
        assert!(cache.probe(&mem, 0x4000_0000).is_none());
        assert_eq!(cache.counters().snapshot().probes, 2, "every miss counts");

        let after = cache.counters().snapshot();
        assert_eq!(after.restored, 0, "nothing was restored, and that is fine");
        assert_eq!(after.refused(), 0, "and nothing was refused");
        assert!(
            after.probes > after.restored.saturating_add(after.refused()),
            "consulted with nothing to offer must be visible: {after:?}"
        );
    }

    /// Writing a blob is not a consultation; asking for one is.
    #[test]
    fn a_written_blob_is_not_a_consultation_but_the_lookup_is() {
        let cache = CodeCache::enabled_in(scratch("record"), JitConfig::get().opt_level());
        let mem = mem_with(0x4000_0000, &[0x90, 0x90, 0x90, 0x90]);
        let va = 0x4000_0000;
        let bytes_hash =
            super::super::super::cache_persist::hash_guest_range(&mem, va, va + 4).expect("hash");
        cache.record(PersistedCode {
            va,
            guest_end: va + 4,
            bytes_hash,
            insn_count: 2,
            inv_gen: 0,
            compiled_at_opt: OptTier::Base,
            align: 16,
            code: vec![0x1f, 0x20, 0x03, 0xd5],
            relocs: Vec::new(),
            chain_relocs: Vec::new(),
        });
        assert_eq!(
            cache.counters().snapshot().probes,
            0,
            "recording is a write, not a consultation"
        );
        drop(cache.probe(&mem, va));
        assert_eq!(
            cache.counters().snapshot().probes,
            1,
            "and the lookup counts even when no table is attached"
        );
    }
}
