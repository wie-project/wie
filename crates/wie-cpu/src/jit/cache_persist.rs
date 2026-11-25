//! Persistent JIT cache **ledger** (`WIE_JIT_CACHE`): per-PE *metadata* about
//! blocks that compiled, or did not.
//!
//! # What this stores, and what it deliberately does not
//!
//! The ledger persists per-PE *metadata*: file key = `(pe_hash, opt_level)` (see
//! [`jit_cache_key`]), in-file key = `(guest_va, fnv1a(guest bytes))`, value =
//! `{guest_start, guest_end, insn_count, inv_gen, never, compiled_at_opt}`.
//! On warm boot, [`JitShared::attach_pe_cache`] bulk-loads the file; every later
//! consumption re-validates the CURRENT guest bytes against the recorded hash
//! before acting on it, so any stale/SMC-diverged entry simply probes as absent
//! and falls back to the normal cold path.
//!
//! **No machine code is stored here.** Re-materializing Cranelift-emitted aarch64
//! code needs the emitted bytes *and* their relocations, and cranelift-jit
//! applies relocations inside `define_function_with_control_plane` and keeps the
//! patched result private — so capturing the pre-relocation form is an
//! interception on the define, not a read of anything that exists afterwards.
//! That is a different subsystem with a different risk profile, and it lives in
//! [`crate::jit::engine::code_cache`] behind its own **off-by-default** knob
//! (`WIE_JIT_CODE_CACHE`). Keeping it separate is deliberate: this ledger's
//! worst case is a wasted compile, whereas replayed code's worst case is a wrong
//! answer, and the two should not share a switch, a file, or a blast radius.
//!
//! Warm-boot savings from *this module alone* (honest accounting): known-good
//! blocks skip the Hot visit-threshold warmup entirely (immediate background
//! compile), and known-bad (`Never`) blocks skip repeated decode attempts. The
//! ~3 ms/block Cranelift cost itself is NOT eliminated here — that is what
//! `WIE_JIT_CODE_CACHE` is for.
//!
//! **The file key alone is not enough once opt level becomes per-block.** With
//! hot-block tiering one process compiles the same guest VA at *both* levels,
//! so a record must name the level that produced it (see [`OptTier`]). Two
//! consequences, both enforced here:
//!
//! - a tier-level record is written to a **second ledger file**, keyed by the
//!   tier's own `jit_cache_key`, so the base file only ever holds base-level
//!   records and vice versa;
//! - [`Self::load_file`] rejects any record whose `compiled_at_opt` does not
//!   match the level the file declares, so a foreign record can never be served
//!   to a compile expecting the other level (defence in depth behind the key).
//!
//! # Write-wins contract (one key, one flush window)
//!
//! A key is a guest VA; two verdicts can be written for it — `Ready` (the block
//! compiled and installed) and `Never` (it structurally cannot compile) — and
//! they are written **by different threads**: a worker's failure verdict for a
//! queued block against a guest thread's successful inline install, or an
//! install against a refusal after a resync. Which one survives is decided at
//! the write, in [`PersistentJitCache::write_wins`], inside the same critical
//! section as the table upsert and the append:
//!
//! - **`Ready` beats `Never`, in either arrival order.** `Never` claims "these
//!   bytes cannot be compiled"; a successful install at the same VA is evidence
//!   against it and the reverse is never true. This is not a preference: a
//!   `Never` row makes [`PersistentJitCache::probe`] report *absent* and
//!   [`PersistentJitCache::never_vas`] seed `CacheEntry::Never` on the next run,
//!   so letting it win retires a block that compiles fine.
//! - **Among `Ready` records for one key, the last install wins.** A re-install
//!   is what replaces the block after a resync or an invalidation, so its extent
//!   and byte hash describe the bytes that were live when the surviving block
//!   was installed.
//! - **The append buffer never outvotes the table.** A flush writes the table
//!   (which already holds the winning record per key) and folds a queued record
//!   back in only for a key the table no longer has — which is why an
//!   invalidation has to purge the buffer as well as the table, or the flush it
//!   triggers would write the tombstoned row straight back out.
//!
//! Enforcing the first rule with a *read* before the write is what used to fail:
//! the reader could see an empty key, a concurrent install could fill it, and
//! the `Never` the reader had already decided to write would then overwrite the
//! `Ready` row. Measured on a synchronised first contact, the old shape lost the
//! `Ready` row in ~20–44 % of rounds — and **never** produced a missing row (the
//! row was always present, saying the opposite), which is why this reads like a
//! flaky test and not like data loss.
//!
//! Note that `inv_gen` is a **per-process** counter starting at zero, so a
//! persisted value is only meaningful as "the generation at capture time" —
//! which is exactly why the code cache (which bakes it into the emitted edge
//! guard) has to compare it against the live value. Here it is carried for
//! diagnosis and for the cross-process story in the module docs above; no
//! in-process consumption compares it, because every consumption is already
//! hash-validated against live guest bytes.

use super::config::JitConfig;
use super::tier::{OptTier, TIER_OPT_LEVEL};
use crate::mem::GuestMemory;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// File magic: `"WIEJITC"` + format byte + flag byte.
const MAGIC: [u8; 8] = *b"WIEJITC\x01";
/// Current on-disk format version.
///
/// v3 adds a per-record `compiled_at_opt` (an [`OptTier`] code). The per-FILE
/// `opt_level` of v2 is not sufficient: hot-block tiering compiles one guest VA
/// at both levels inside a single process, so a file keyed on the base level
/// can hold a record that only ever succeeded at the tier level, and a base
/// run would happily serve it. v2 files are deleted rather than left readable
/// under a record layout that no longer matches.
///
/// v2 added `opt_level` to [`FileBody`] and folded it into the file key. v1
/// files were keyed by PE hash alone, so a `WIE_JIT_OPT=none` run and a
/// `WIE_JIT_OPT=speed` run shared one ledger file and each consumed the other's
/// entries.
const FORMAT_VERSION: u32 = 3;

/// Window hashed for `Never` (negative) entries: they lack an exact byte
/// extent at record time, so both record and validate sides compare a fixed
/// 32-byte window starting at the VA.
const NEVER_WINDOW: usize = 32;

/// Buffered installs flushed to disk when exceeded (lazy-append policy).
const APPEND_FLUSH_CAP: usize = 256;
/// Minimum interval between background-ish flushes (lazy fsync policy).
const FLUSH_MIN_INTERVAL: Duration = Duration::from_secs(5);
/// Hard cap on deserialized entries per file (corrupt/truncated guard).
const MAX_DISK_ENTRIES: usize = 4_000_000;

// FNV-1a 64-bit parameters (deterministic, stable across runs — unlike
// `RandomState`/`DefaultHasher`, whose algorithms are not stability-guaranteed).
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

#[must_use]
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = FNV_OFFSET;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// PE identity hash for the persistent JIT cache: plain FNV-1a over the image
/// file bytes. Any byte-level change to the EXE produces a different file.
///
/// This is the *image* half of the cache key only; pair it with the Cranelift
/// `opt_level` via [`jit_cache_key`] before trusting a persisted ledger.
#[must_use]
pub fn jit_cache_pe_hash(pe_file_bytes: &[u8]) -> u64 {
    fnv1a(pe_file_bytes)
}

/// Ledger identity for one `(PE image, Cranelift opt level)` pair — the whole
/// on-disk key, and also the in-memory table key.
///
/// Opt level is part of the identity, not an incidental field, because both
/// facts the ledger records are properties of the *compiler settings* that
/// produced them:
///
/// - a `Ready` record means "these exact guest bytes compiled successfully",
///   and Cranelift at `speed` / `speed_and_size` accepts and optimizes blocks
///   that `none` rejects (and vice versa) — a `none`-run verdict must not be
///   replayed as known-good for a `speed` run;
/// - a `Never` record means "do not retry compiling these bytes", which is a
///   much stronger claim at one opt level than at another.
///
/// Mixing (not concatenating) keeps the derivation a pure, total function of
/// two integers-ish and makes the on-disk name a single hex word.
#[must_use]
pub(super) fn jit_cache_key(pe_hash: u64, opt_level: &str) -> u64 {
    let mut h = pe_hash ^ FNV_OFFSET;
    // Length-prefix so "speed" + trailing junk cannot alias "speedy".
    h ^= u64::try_from(opt_level.len()).unwrap_or(0);
    h = h.wrapping_mul(FNV_PRIME);
    for &b in opt_level.as_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// Hash the exact guest bytes in `[start, end)` from guest memory, page-chunked.
///
/// Returns `None` when any part of the range is unreadable (or the memory
/// generation moves mid-hash): callers treat this as "probe absent".
pub(super) fn hash_guest_range(mem: &GuestMemory, start: u64, end: u64) -> Option<u64> {
    if end <= start {
        return None;
    }
    let mut h = FNV_OFFSET;
    let mut pos = start;
    while pos < end {
        let page_end = ((pos >> 12) + 1) << 12;
        let n = page_end.min(end).saturating_sub(pos);
        let n_us = usize::try_from(n).ok()?;
        let mut buf = vec![0_u8; n_us];
        mem.read(pos, &mut buf).ok()?;
        for &b in &buf {
            h ^= u64::from(b);
            h = h.wrapping_mul(FNV_PRIME);
        }
        pos = pos.saturating_add(n);
    }
    Some(h)
}

/// One persistent-ledger record.
#[derive(Debug, Clone, Copy)]
pub(super) struct LedgerRec {
    pub va: u64,
    /// Exclusive end of the hashed guest-byte range (== `va + len`; equal to
    /// `va + NEVER_WINDOW` for `Never` records).
    pub guest_end: u64,
    pub bytes_hash: u64,
    pub insn_count: u32,
    pub inv_gen: u64,
    /// `true`: this block failed to compile last run (negative entry).
    pub never: bool,
    /// Which opt level the recorded verdict belongs to.
    ///
    /// Load-bearing with per-block tiering: `Ready` means "these exact bytes
    /// compiled successfully", which is a claim about the compiler settings
    /// that produced it, and `Never` ("do not retry compiling these bytes") is
    /// a *stronger* claim still. Neither transfers across opt levels.
    pub compiled_at_opt: OptTier,
}

impl LedgerRec {
    fn overlaps(&self, addr: u64, end: u64) -> bool {
        self.va < end && addr < self.guest_end.max(self.va)
    }
}

/// Byte-validated probe result for one guest VA against the active PE's
/// ledger: `Some(insn_count)` when the previous run compiled these exact
/// guest bytes successfully.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LedgerProbe {
    pub insn_count: u32,
}

// ---------------------------------------------------------------------------
// Disk layout
// ---------------------------------------------------------------------------

#[derive(bincode::Encode, bincode::Decode)]
struct FileBody {
    wie_version: String,
    format_version: u32,
    /// [`jit_cache_key`] of this file: the ledger identity, not the bare PE hash.
    key: u64,
    /// The Cranelift `opt_level` the entries were compiled at. Redundant with
    /// `key` by construction; stored so a key collision (or a hand-edited
    /// file) is *detected* rather than silently served.
    opt_level: String,
    entries: Vec<DiskEntry>,
}

#[derive(bincode::Encode, bincode::Decode)]
struct DiskEntry {
    va: u64,
    guest_end: u64,
    bytes_hash: u64,
    insn_count: u32,
    inv_gen: u64,
    never: bool,
    /// [`OptTier::code`]; `None` on decode is a rejected record, never a guess.
    compiled_at_opt: u8,
}

/// Build one on-disk entry from a live record.
fn disk_entry(r: &LedgerRec) -> DiskEntry {
    DiskEntry {
        va: r.va,
        guest_end: r.guest_end,
        bytes_hash: r.bytes_hash,
        insn_count: r.insn_count,
        inv_gen: r.inv_gen,
        never: r.never,
        compiled_at_opt: r.compiled_at_opt.code(),
    }
}

fn load_body(path: &Path) -> Result<FileBody, String> {
    let raw = fs::read(path).map_err(|e| e.to_string())?;
    if raw.len() < MAGIC.len() || raw[..MAGIC.len()] != MAGIC {
        return Err("bad magic".into());
    }
    let (body, consumed): (FileBody, usize) =
        bincode::decode_from_slice(&raw[MAGIC.len()..], bincode::config::standard())
            .map_err(|e| e.to_string())?;
    let _ = consumed;
    Ok(body)
}

fn write_body(path: &Path, body: &FileBody) -> Result<(), String> {
    let mut payload = Vec::with_capacity(MAGIC.len() + 1024);
    payload.extend_from_slice(&MAGIC);
    let encoded =
        bincode::encode_to_vec(body, bincode::config::standard()).map_err(|e| e.to_string())?;
    payload.extend_from_slice(&encoded);
    // Atomic replace so a crash mid-write leaves the previous file intact.
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        let mut f = fs::File::create(&tmp).map_err(|e| e.to_string())?;
        f.write_all(&payload).map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Persistent cache state
// ---------------------------------------------------------------------------

struct FlushState {
    pending: Vec<LedgerRec>,
    dirty_rewrite: bool,
    last_flush: Instant,
    warned_err: bool,
}

impl FlushState {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
            dirty_rewrite: false,
            last_flush: Instant::now(),
            warned_err: false,
        }
    }
}

/// Per-PE in-memory ledger tables (lock-free reads via papaya). The inner
/// handle is shared through `Arc`: `papaya::HashMap::clone()` does NOT alias
/// the same map. Keyed by [`jit_cache_key`], i.e. per `(PE, opt level)`.
type PeTables = RwLock<ahash::HashMap<u64, Arc<papaya::HashMap<u64, LedgerRec>>>>;

/// Process-wide persistent JIT cache handle. Constructed always (cheap even
/// disabled); all methods are no-ops when [`Self::enabled`] is false and all
/// I/O errors are swallowed into a warn-once + permanent disable, so any
/// filesystem trouble falls back to cold compilation transparently.
pub(super) struct PersistentJitCache {
    enabled: bool,
    base_dir: PathBuf,
    /// Cranelift `opt_level` this process compiles at, for the BASE tier. Part
    /// of every ledger key — see [`jit_cache_key`].
    opt_level: &'static str,
    /// Active BASE-tier ledger key ([`jit_cache_key`], so `0` still means
    /// "none"). This is the only key whose file is ever loaded.
    active_key: AtomicU64,
    /// Ledger key for the TIER opt level ([`TIER_OPT_LEVEL`]) for the attached
    /// PE, or `0` when there is no separate tier file (persistence off, not
    /// attached, or the base level already IS the tier level).
    ///
    /// Tier-level records are written here and **never read by the process that
    /// wrote them**: a `none` run must not consume `speed` verdicts. A later
    /// `WIE_JIT_OPT=speed` run loads this very file as its own base file, which
    /// is the whole point of persisting it.
    tier_key: AtomicU64,
    /// VAs compiled at the tier level this run, so [`Self::record_ready`] files
    /// their record under the tier's key instead of the base key.
    tiered: RwLock<ahash::HashSet<u64>>,
    /// Load-once latch per attached key.
    tables: PeTables,
    flush: Mutex<FlushState>,
}

impl PersistentJitCache {
    /// Resolve config from the environment: `WIE_JIT_CACHE`.
    ///
    /// - unset → default dir under `$WIE_CACHE_DIR` / XDG cache home
    ///   (`wie/jit`), DISABLED whenever this process is a test process (see
    ///   [`under_test_process`]) so that suites stay deterministic and no two
    ///   test processes share one ledger file;
    /// - `0` / `false` / `off` → disabled;
    /// - anything else → that value used as the cache directory.
    pub(super) fn new() -> Self {
        #[cfg(test)]
        if let Some(dir) = test_dir() {
            return Self::enabled_with(dir, JitConfig::get().opt_level());
        }
        match resolve_config(std::env::var("WIE_JIT_CACHE").ok(), under_test_process()) {
            Some(dir) => Self::enabled_with(dir, JitConfig::get().opt_level()),
            None => Self::disabled(),
        }
    }

    fn enabled_with(dir: PathBuf, opt_level: &'static str) -> Self {
        Self {
            enabled: true,
            base_dir: dir,
            opt_level,
            active_key: AtomicU64::new(0),
            tier_key: AtomicU64::new(0),
            tiered: RwLock::new(ahash::HashSet::default()),
            tables: RwLock::new(ahash::HashMap::default()),
            flush: Mutex::new(FlushState::new()),
        }
    }

    fn disabled() -> Self {
        Self {
            enabled: false,
            base_dir: PathBuf::new(),
            opt_level: JitConfig::get().opt_level(),
            active_key: AtomicU64::new(0),
            tier_key: AtomicU64::new(0),
            tiered: RwLock::new(ahash::HashSet::default()),
            tables: RwLock::new(ahash::HashMap::default()),
            flush: Mutex::new(FlushState::new()),
        }
    }

    #[cfg(test)]
    fn with_base_dir(dir: PathBuf) -> Self {
        Self::enabled_with(dir, JitConfig::get().opt_level())
    }

    /// Test-only handle pinned to a specific opt level, so the keying can be
    /// exercised for two levels inside one process.
    #[cfg(test)]
    fn with_base_dir_opt(dir: PathBuf, opt_level: &'static str) -> Self {
        Self::enabled_with(dir, opt_level)
    }

    fn file_path_for(&self, key: u64) -> PathBuf {
        self.base_dir.join(format!("{key:016x}.bin"))
    }

    /// Current attached ledger key ([`jit_cache_key`]; `0` == none).
    ///
    /// Named for its only caller, which only ever asks "is anything attached?"
    /// — the value is the mixed key, not the bare PE hash.
    pub(super) fn active_pe(&self) -> u64 {
        self.active_key.load(Ordering::Acquire)
    }

    /// Attach + bulk-load the ledger for `pe_hash` at this process's opt
    /// level. Idempotent per `(pe_hash, opt_level)`; errors degrade to "no
    /// ledger" (warn-once), never propagate.
    ///
    /// Only the BASE-tier file is loaded. The TIER file (if the tiers differ) is
    /// opened empty for writing: a base run must not consume tier-level
    /// verdicts, and a tier run will load the very same file as its own base
    /// file because its base level *is* the tier level.
    pub(super) fn attach(&self, pe_hash: u64) {
        if !self.enabled || pe_hash == 0 {
            return;
        }
        let key = jit_cache_key(pe_hash, self.opt_level);
        let tier_key = if TIER_OPT_LEVEL == self.opt_level {
            0 // tiers coincide: one file, no routing
        } else {
            jit_cache_key(pe_hash, TIER_OPT_LEVEL)
        };
        {
            let tables = self.tables.read().unwrap_or_else(|e| e.into_inner());
            if tables.contains_key(&key) {
                self.active_key.store(key, Ordering::Release);
                self.tier_key.store(tier_key, Ordering::Release);
                return; // already loaded
            }
        }
        let map = self.load_file(key);
        {
            let mut tables = self.tables.write().unwrap_or_else(|e| e.into_inner());
            tables.entry(key).or_insert(Arc::new(map));
            if tier_key != 0 {
                // Deliberately NOT loaded — see the doc comment.
                tables
                    .entry(tier_key)
                    .or_insert_with(|| Arc::new(papaya::HashMap::new()));
            }
        }
        self.active_key.store(key, Ordering::Release);
        self.tier_key.store(tier_key, Ordering::Release);
        tracing::debug!(
            pe = format_args!("{pe_hash:#x}"),
            opt = self.opt_level,
            key = format_args!("{key:#x}"),
            tier_key = format_args!("{tier_key:#x}"),
            "jit disk cache attached"
        );
    }

    /// Read + version-validate `<key>.bin`. A version/magic/WIE-version/opt-level
    /// mismatch DELETES the stale file and returns an empty map ("reset").
    fn load_file(&self, key: u64) -> papaya::HashMap<u64, LedgerRec> {
        let empty = papaya::HashMap::new();
        let path = self.file_path_for(key);
        let body = match load_body(&path) {
            Ok(b) => b,
            Err(_) => return empty, // missing or unreadable: cold boot, keep quiet
        };
        if body.format_version != FORMAT_VERSION
            || body.wie_version != env!("CARGO_PKG_VERSION")
            || body.key != key
            || body.opt_level != self.opt_level
            || body.entries.len() > MAX_DISK_ENTRIES
        {
            // Version mismatch resets the file entirely (requirement). A
            // missing file is equivalent to reset.
            if fs::remove_file(&path).is_ok() {
                tracing::info!(
                    key = format_args!("{key:#x}"),
                    "jit disk cache version mismatch — reset"
                );
            }
            return empty;
        }
        let map = papaya::HashMap::with_capacity(body.entries.len());
        let mut rejected = 0_u64;
        {
            let pin = map.pin();
            for e in body.entries {
                if e.guest_end < e.va {
                    continue;
                }
                // Per-RECORD opt-level gate. The file-level checks above already
                // make this unreachable for a file this build wrote; it exists
                // because `Ready`/`Never` are claims about compiler settings, so
                // a record produced at the other level must not be served to a
                // compile expecting this one — even if a key collision or a
                // hand-edited file got it onto the right key.
                let Some(compiled_at_opt) = OptTier::from_code(e.compiled_at_opt) else {
                    rejected = rejected.saturating_add(1);
                    continue;
                };
                if compiled_at_opt.opt_level() != body.opt_level {
                    rejected = rejected.saturating_add(1);
                    continue;
                }
                pin.insert(
                    e.va,
                    LedgerRec {
                        va: e.va,
                        guest_end: e.guest_end,
                        bytes_hash: e.bytes_hash,
                        insn_count: e.insn_count,
                        inv_gen: e.inv_gen,
                        never: e.never,
                        compiled_at_opt,
                    },
                );
            }
        }
        if rejected > 0 {
            // Not a file-level mismatch, so the file is NOT deleted: the bad
            // records are dropped here and the next flush rewrites the file
            // without them.
            tracing::info!(
                key = format_args!("{key:#x}"),
                rejected,
                "jit disk cache records rejected — opt level mismatch or unknown tier code"
            );
        }
        map
    }

    fn active_table(&self) -> Option<Arc<papaya::HashMap<u64, LedgerRec>>> {
        let key = self.active_key.load(Ordering::Acquire);
        if key == 0 {
            return None;
        }
        let tables = self.tables.read().unwrap_or_else(|e| e.into_inner());
        tables.get(&key).cloned()
    }

    /// Table holding TIER-level records for the attached PE (`None` when the
    /// tiers coincide or nothing is attached). Write-only for this process.
    fn tier_table(&self) -> Option<Arc<papaya::HashMap<u64, LedgerRec>>> {
        let key = self.tier_key.load(Ordering::Acquire);
        if key == 0 || key == self.active_key.load(Ordering::Acquire) {
            return None;
        }
        let tables = self.tables.read().unwrap_or_else(|e| e.into_inner());
        tables.get(&key).cloned()
    }

    /// Declare `va` as compiled at the tier opt level, so its next
    /// [`Self::record_ready`] files the record under the tier key.
    ///
    /// Called only after a tier compile has SUCCEEDED: a tier compile that the
    /// verifier rejects falls back to a base compile, and that base verdict
    /// must not be filed as a tier one.
    pub(super) fn mark_tiered(&self, va: u64) {
        if !self.enabled {
            return;
        }
        self.tiered
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(va);
    }

    /// Byte-validated probe of the CURRENT guest bytes at `va`.
    pub(super) fn probe(&self, mem: &GuestMemory, va: u64) -> Option<LedgerProbe> {
        let rec = self.active_table()?.pin().get(&va).copied()?;
        if rec.never {
            return None; // Never records are consumed via seed_never_entries()
        }
        let h = hash_guest_range(mem, va, rec.guest_end)?;
        (h == rec.bytes_hash).then_some(LedgerProbe {
            insn_count: rec.insn_count,
        })
    }

    /// VAs of all `Never` (negative) records for the ACTIVE PE, for seeding
    /// the live in-memory cache at attach time. Unvalidated by design: any
    /// bytes diverged since recording were already dropped by
    /// [`Self::invalidate_range`] tombstones (SMC/protect paths), and a stale
    /// seed degrades to interpretation only — iced remains correct.
    pub(super) fn never_vas(&self) -> Vec<u64> {
        match self.active_table() {
            Some(table) => table
                .pin()
                .iter()
                .filter_map(|(&va, r)| r.never.then_some(va))
                .collect(),
            None => Vec::new(),
        }
    }

    #[cfg(test)]
    pub(super) fn test_entries_len(&self) -> usize {
        self.active_table().map_or(0, |t| t.pin().len())
    }

    #[cfg(test)]
    pub(super) fn test_tier_entries_len(&self) -> usize {
        self.tier_table().map_or(0, |t| t.pin().len())
    }

    /// Sorted VAs currently in the active in-memory table.
    ///
    /// Exists so a test can tell the three failure shapes apart: a row that was
    /// never *recorded* is absent here and absent from `pending`; a row recorded
    /// and then *dropped* has left this table; a row present here but missing
    /// from the file was lost by the *flush*.
    #[cfg(test)]
    pub(super) fn test_vas(&self) -> Vec<u64> {
        let mut vas: Vec<u64> = self
            .active_table()
            .map(|t| t.pin().iter().map(|(&va, _)| va).collect())
            .unwrap_or_default();
        vas.sort_unstable();
        vas
    }

    /// `(never, guest_end, insn_count)` for `va` in the active table.
    ///
    /// `Some((false, ..))` is the positive verdict a warm boot consumes;
    /// `Some((true, ..))` probes as *absent* everywhere, which is why a test has
    /// to look at the flag rather than at the row's presence.
    #[cfg(test)]
    pub(super) fn test_row(&self, va: u64) -> Option<(bool, u64, u32)> {
        self.active_table()
            .and_then(|t| t.pin().get(&va).copied())
            .map(|r| (r.never, r.guest_end, r.insn_count))
    }

    /// Number of records queued for the next flush (the append buffer).
    #[cfg(test)]
    pub(super) fn test_pending_len(&self) -> usize {
        self.flush
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .len()
    }

    /// Force a synchronous flush, exactly as `Drop` does.
    ///
    /// A test cannot reach the flush any other way: the append buffer drains at
    /// [`APPEND_FLUSH_CAP`] or after [`FLUSH_MIN_INTERVAL`], and a live
    /// `JitShared` never drops its handle inside a test process.
    #[cfg(test)]
    pub(super) fn test_flush(&self) {
        self.flush_now(true);
    }

    /// What a *fresh* handle would conclude about `va` when reloaded from disk,
    /// byte-validated against `mem` — the ledger's only real question.
    #[cfg(test)]
    pub(super) fn test_disk_probe(
        dir: PathBuf,
        pe_hash: u64,
        mem: &GuestMemory,
        va: u64,
    ) -> Option<u32> {
        let fresh = Self::with_base_dir(dir);
        fresh.attach(pe_hash);
        fresh.probe(mem, va).map(|p| p.insn_count)
    }

    /// Whether the row a *fresh* handle loads from disk for `va` is a `Never`
    /// verdict (`Some(true)`), a positive one (`Some(false)`), or absent
    /// (`None`) — the three shapes a reload can hand the next run.
    #[cfg(test)]
    pub(super) fn test_disk_row_never(dir: PathBuf, pe_hash: u64, va: u64) -> Option<bool> {
        let fresh = Self::with_base_dir(dir);
        fresh.attach(pe_hash);
        fresh.active_table()?.pin().get(&va).map(|r| r.never)
    }

    /// Sorted VAs a *fresh* handle loads from disk for `pe_hash`, without
    /// attaching to this process's handle.
    ///
    /// The engine's own ledger flushes on drop, which a live `JitShared` does
    /// not reach inside a test; this is how a test asks "what would the next
    /// process see", which is the only question a persistent ledger has to
    /// answer.
    #[cfg(test)]
    pub(super) fn test_disk_vas(dir: PathBuf, pe_hash: u64) -> Vec<u64> {
        let fresh = Self::with_base_dir(dir);
        fresh.attach(pe_hash);
        fresh.test_vas()
    }

    /// Record one successfully installed block (inline or worker install).
    ///
    /// The record is filed under the key for the tier that actually compiled
    /// it: a VA marked by [`Self::mark_tiered`] goes to the tier ledger, every
    /// other VA to the base ledger. That is what keeps a base run from ever
    /// reading (or writing) a tier-level verdict under its own key.
    pub(super) fn record_ready(
        &self,
        mem: &GuestMemory,
        va: u64,
        guest_end: u64,
        insn_count: u32,
        inv_gen: u64,
    ) {
        if !self.enabled || guest_end <= va {
            return;
        }
        let Some(hash) = hash_guest_range(mem, va, guest_end) else {
            return;
        };
        let tier = if self.is_tiered(va) {
            OptTier::Speed
        } else {
            OptTier::Base
        };
        self.insert_rec(
            LedgerRec {
                va,
                guest_end,
                bytes_hash: hash,
                insn_count,
                inv_gen,
                never: false,
                compiled_at_opt: tier,
            },
            tier,
        );
    }

    fn is_tiered(&self, va: u64) -> bool {
        self.tiered
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&va)
    }

    /// Record a `Never` verdict for `va` (fixed-window hash validation).
    ///
    /// Always the BASE ledger: a `Never` is only ever reached after a base
    /// compile failed, so a tier-level record would overstate the claim.
    pub(super) fn record_never(&self, mem: &GuestMemory, va: u64) {
        if !self.enabled {
            return;
        }
        let Some(table) = self.active_table() else {
            return;
        };
        // Fast path only — it skips the hash for the common "already recorded"
        // case. It is NOT what enforces "Ready beats Never": a concurrent
        // install can insert between this read and the write below, so the
        // authority is [`Self::write_wins`], applied inside the write's own
        // critical section.
        if table.pin().get(&va).is_some() {
            return;
        }
        let win = u64::try_from(NEVER_WINDOW).unwrap_or(u64::MAX);
        let Some(hash) = hash_guest_range(mem, va, va.saturating_add(win)) else {
            return;
        };
        self.insert_rec(
            LedgerRec {
                va,
                guest_end: va.saturating_add(win),
                bytes_hash: hash,
                insn_count: 0,
                inv_gen: 0,
                never: true,
                compiled_at_opt: OptTier::Base,
            },
            OptTier::Base,
        );
    }

    fn insert_rec(&self, rec: LedgerRec, tier: OptTier) {
        let table = match tier {
            OptTier::Base => self.active_table(),
            OptTier::Speed => self.tier_table(),
        };
        let Some(table) = table else {
            return;
        };
        // ONE critical section for the whole write — the merge rule, the table
        // upsert and the append to the buffer. Splitting them is what used to
        // let two recorders of the same key disagree: the check that enforces
        // "Ready beats Never" lived in `record_never`, *before* the hash, so a
        // concurrent install could insert a Ready row in the gap and be
        // overwritten by the Never that had already decided to write. Measured
        // on a synchronised first contact, the old shape lost the Ready row in
        // 44 % of rounds (0 rows ever *missing*: the row was always there,
        // saying the opposite). See the module docs' write-wins contract.
        {
            let mut st = self.flush.lock().unwrap_or_else(|e| e.into_inner());
            let pin = table.pin();
            if !Self::write_wins(pin.get(&rec.va).copied(), &rec) {
                return;
            }
            // Ready upserts overwrite older Never records for the same VA.
            pin.insert(rec.va, rec);
            st.pending.push(rec);
        }
        // Outside the lock: `flush_tick` re-takes it (and `flush_now` holds it
        // across the table snapshot); a std `Mutex` is not reentrant.
        self.flush_tick();
    }

    /// Whether `incoming` may replace `existing` for the same ledger key.
    ///
    /// This is the whole write-wins contract for one key, in one place, so
    /// [`Self::insert_rec`] is its only caller and every writer obeys it:
    ///
    /// - **`Ready` beats `Never`.** `Never` claims "these bytes cannot be
    ///   compiled"; a successful install at the same VA is direct evidence
    ///   against it, and the reverse is never true — so a `Never` may not
    ///   displace a `Ready`, whichever thread wrote first.
    /// - **Among `Ready` records, the last write wins.** A re-install is what
    ///   replaces a block after a resync or an invalidation, so its extent and
    ///   byte hash describe the bytes that were live when the surviving block
    ///   was installed. Keeping the older row would persist a hash of bytes
    ///   that no longer exist, which `probe` would then refuse anyway.
    /// - **A `Never` may replace a `Never`.** The window re-hashes under the
    ///   same rule, so the newest verdict about the current bytes wins.
    fn write_wins(existing: Option<LedgerRec>, incoming: &LedgerRec) -> bool {
        match existing {
            None => true,
            Some(old) => !(incoming.never && !old.never),
        }
    }

    /// Drop ledger records overlapping `[addr, addr+len)` (in-memory now, on
    /// disk at next flush via full rewrite) — SMC / X-loss invalidations.
    ///
    /// Both ledgers are purged: the tier ledger holds records for the same guest
    /// bytes, and a stale entry there would be served to a later `speed` run.
    /// The **append buffer is purged too**: a record queued in this window is no
    /// longer in the table, and `flush_table` folds queued records back in for
    /// any key the table does not hold — so without this the flush triggered
    /// just below would write the invalidated row straight back out (and a
    /// crash before the next flush would persist it for a whole warm boot).
    /// `clear_all` drops the buffer for the same reason.
    pub(super) fn invalidate_range(&self, addr: u64, len: usize) {
        if !self.enabled || len == 0 {
            return;
        }
        let end = addr.saturating_add(u64::try_from(len).unwrap_or(u64::MAX));
        if !self.drop_overlapping(addr, end) {
            return;
        }
        {
            let mut st = self.flush.lock().unwrap_or_else(|e| e.into_inner());
            st.pending.retain(|r| !r.overlaps(addr, end));
            st.dirty_rewrite = true;
            st.last_flush = Instant::now(); // invalidation flushes soon (below)
        }
        // Rewrite promptly (metadata files are small).
        self.flush_now(false);
    }

    /// Remove every record overlapping `[addr, end)` from the base and (when
    /// present) tier ledgers. Returns whether anything was dropped.
    fn drop_overlapping(&self, addr: u64, end: u64) -> bool {
        let Some(base) = self.active_table() else {
            return false;
        };
        let tier = self.tier_table();
        let mut dropped = false;
        for table in [Some(base), tier].into_iter().flatten() {
            let pin = table.pin();
            let doomed: Vec<u64> = pin
                .iter()
                .filter(|(_, r)| r.overlaps(addr, end))
                .map(|(&va, _)| va)
                .collect();
            dropped |= !doomed.is_empty();
            for va in &doomed {
                pin.remove(va);
            }
        }
        dropped
    }

    /// Full-clear handling.
    ///
    /// `full == true` (guest-triggered FlushInstructionCache(0) / pending-code
    /// overflow): purge the whole table AND mark the file for rewrite.
    ///
    /// `full == false` (init-time clears from `configure_fast_path` /
    /// `install_runtime_hooks`, which run BEFORE any guest execution): the
    /// ledger facts about static image bytes stay valid — every consumption
    /// is hash-validated anyway — so only records learned THIS run (pending)
    /// are dropped. This keeps a warm boot's knowledge alive through session
    /// initialization order.
    pub(super) fn clear_all(&self, full: bool) {
        if !self.enabled {
            return;
        }
        {
            let mut st = self.flush.lock().unwrap_or_else(|e| e.into_inner());
            st.pending.clear();
            if !full {
                return;
            }
            st.dirty_rewrite = true;
        }
        let Some(base) = self.active_table() else {
            return;
        };
        base.pin().clear();
        if let Some(tier) = self.tier_table() {
            tier.pin().clear();
        }
        self.flush_now(true);
    }

    /// Lazy flush gate: append-buffered records are written when the buffer
    /// exceeds [`APPEND_FLUSH_CAP`] or after [`FLUSH_MIN_INTERVAL`].
    fn flush_tick(&self) {
        let due = {
            let st = self.flush.lock().unwrap_or_else(|e| e.into_inner());
            st.pending.len() >= APPEND_FLUSH_CAP
                || st.dirty_rewrite
                || st.last_flush.elapsed() >= FLUSH_MIN_INTERVAL
        };
        if due {
            self.flush_now(false);
        }
    }

    /// Serialize the attached PE's ledgers to `<dir>/<ledger-key>.bin` via
    /// temp-file rename — the base ledger always, the tier ledger when the two
    /// tiers differ and something was recorded there. `force_fsync` also calls
    /// `sync_all` (the lazy-fsync policy keeps ordinary flushes unsynced).
    fn flush_now(&self, force_fsync: bool) {
        if !self.enabled {
            return;
        }
        let key = self.active_key.load(Ordering::Acquire);
        if key == 0 {
            return;
        }
        // Drain the append buffer ONCE and split it: the routing is a pure
        // function of `compiled_at_opt`, so one drain feeds both files. Draining
        // per file would hand the first call everything and starve the second.
        let (base_pending, tier_pending) = {
            let mut st = self.flush.lock().unwrap_or_else(|e| e.into_inner());
            let pending = std::mem::take(&mut st.pending);
            let mut base_pending = Vec::new();
            let mut tier_pending = Vec::new();
            for r in pending {
                if r.compiled_at_opt == OptTier::Base {
                    base_pending.push(r);
                } else {
                    tier_pending.push(r);
                }
            }
            st.dirty_rewrite = false;
            (base_pending, tier_pending)
        };
        if let Some(table) = self.active_table() {
            self.flush_table(key, table, OptTier::Base, &base_pending, force_fsync);
        }
        let tier_key = self.tier_key.load(Ordering::Acquire);
        // An all-empty tier ledger is not written at all: creating an empty file
        // would materialise a second ledger for a level this process compiled
        // nothing at, and a base run must leave the other level's ledger
        // namespace alone. A non-empty one is rewritten, so an invalidation
        // flush reaches it too.
        if tier_key != 0
            && tier_key != key
            && let Some(table) = self.tier_table()
            && !(table.pin().is_empty() && tier_pending.is_empty())
        {
            self.flush_table(tier_key, table, OptTier::Speed, &tier_pending, force_fsync);
        }
    }

    /// Snapshot one ledger table (plus the matching buffered appends) and
    /// write it under `key`.
    fn flush_table(
        &self,
        key: u64,
        table: Arc<papaya::HashMap<u64, LedgerRec>>,
        tier: OptTier,
        pending: &[LedgerRec],
        force_fsync: bool,
    ) {
        let mut snapshot: Vec<DiskEntry> = {
            let pin = table.pin();
            pin.iter()
                .filter(|(_, r)| r.compiled_at_opt == tier)
                .map(|(_, r)| disk_entry(r))
                .collect()
        };
        // Pending additions may have raced the snapshot above; fold them back in
        // so a very short-lived process still persists.
        for r in pending {
            if !snapshot.iter().any(|e| e.va == r.va) {
                snapshot.push(disk_entry(r));
            }
        }
        let body = FileBody {
            wie_version: env!("CARGO_PKG_VERSION").to_string(),
            format_version: FORMAT_VERSION,
            key,
            opt_level: tier.opt_level().to_string(),
            entries: snapshot,
        };
        let path = self.file_path_for(key);
        let res = write_body(&path, &body).and_then(|()| {
            if force_fsync {
                fs::File::open(&path)
                    .and_then(|f| f.sync_all())
                    .map_err(|e| e.to_string())
            } else {
                Ok(())
            }
        });
        match res {
            Ok(()) => {
                let mut st = self.flush.lock().unwrap_or_else(|e| e.into_inner());
                st.last_flush = Instant::now();
            }
            Err(e) => {
                // Warn once, then disable persistence entirely for this
                // process (fall back to pure cold-compilation behavior).
                let mut st = self.flush.lock().unwrap_or_else(|e| e.into_inner());
                st.dirty_rewrite = false;
                if !st.warned_err {
                    st.warned_err = true;
                    tracing::warn!(
                        error = %e,
                        path = %path.display(),
                        "jit disk cache write failed — persistence disabled for this run"
                    );
                }
            }
        }
    }
}

/// Drop-flush best effort. Guard-poison `into_inner` mirrors sibling modules.
impl Drop for PersistentJitCache {
    fn drop(&mut self) {
        self.flush_now(true);
    }
}

/// True when this process is a test process, and the on-disk ledger must stay
/// off for suite determinism.
///
/// `cfg!(test)` alone is NOT sufficient. It is true only for a crate's own
/// `#[cfg(test)] mod tests`, which means the `wie-cpu` rlib gets linked into
/// its unit-test binary with the flag set. Integration tests under
/// `crates/*/tests/` link `wie-cpu` as an ordinary external dependency, so
/// `cfg!(test)` is FALSE there — and `cargo nextest` runs every test in its
/// own process (`NEXTEST_EXECUTION_MODE=process-per-test`). Without the
/// runtime check below, all of those processes would open, read, rewrite and
/// `rename` the SAME `$cache/wie/jit/<pe-hash>.bin` concurrently: a
/// last-writer-wins ledger plus torn-read exposure, i.e. a latent flake and
/// data-corruption source that has nothing to do with the emulator being
/// wrong.
///
/// `cargo-nextest` exports `NEXTEST=1` (plus `NEXTEST_EXECUTION_MODE`,
/// `NEXTEST_RUN_ID`, …) into every test process; that is the marker we key
/// on. Plain `cargo test` exports no distinguishing variable at all (verified:
/// it exports only the same `CARGO_*` set a plain `cargo run` does), so an
/// integration test run under `cargo test` still needs `WIE_JIT_CACHE=0` by
/// hand. `cargo test` is not part of this project's gate — see
/// scripts/check.sh and docs/TESTING.md, which use nextest throughout.
fn under_test_process() -> bool {
    // `cfg!(test)`: our own unit tests, which link us with `cfg(test)` set.
    // `NEXTEST`: cargo-nextest, which covers integration tests linked as an
    // external crate (no `cfg(test)` in wie-cpu at all).
    cfg!(test) || std::env::var_os("NEXTEST").is_some()
}

// Test-only directory override for the *next* handle built on this thread.
//
// The ambient ledger is deliberately inert in a test process (see
// `under_test_process`) and `JitShared` builds its handle exactly once, deep
// inside `JitCpu::open_x86_64`, so a test that needs the *engine's own* ledger
// to be live has no other way in: `env::set_var` is `unsafe` under this crate's
// lint set, and a `JitShared` field cannot be swapped after construction. A
// thread-local keeps the override per test (nextest runs one test per process,
// and a unit test's engine is built on the test thread), so two tests cannot
// see each other's directory.
#[cfg(test)]
thread_local! {
    static TEST_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Point subsequently constructed ledgers on this thread at `dir`.
#[cfg(test)]
pub(super) fn test_set_dir(dir: PathBuf) {
    TEST_DIR.with(|d| *d.borrow_mut() = Some(dir));
}

/// The override installed by [`test_set_dir`], if any.
#[cfg(test)]
fn test_dir() -> Option<PathBuf> {
    TEST_DIR.with(|d| d.borrow().clone())
}

/// Pure `WIE_JIT_CACHE` precedence resolver, split out of [`PersistentJitCache::new`]
/// so the whole decision table is unit-testable without mutating the process
/// environment (which would be `unsafe` under this crate's lint set and racy
/// under `cargo test`'s shared-process model).
///
/// An explicit `WIE_JIT_CACHE` always wins — including in tests — so a
/// developer debugging the ledger can point a test run at a scratch dir.
fn resolve_config(explicit: Option<String>, under_test: bool) -> Option<PathBuf> {
    match explicit {
        None => (!under_test).then(default_cache_dir),
        Some(v)
            if v.is_empty()
                || v == "0"
                || v.eq_ignore_ascii_case("false")
                || v.eq_ignore_ascii_case("off") =>
        {
            None
        }
        // `WIE_JIT_CACHE` doubles as a boolean switch *and* as a directory
        // override, so it must accept the same "on" spellings as its sibling
        // `WIE_JIT_CODE_CACHE`. Without this arm `WIE_JIT_CACHE=1` — the value
        // every other knob in this table takes for "on", and the value a reader
        // will reach for out of symmetry with `WIE_JIT_CODE_CACHE=1` — falls
        // through to `Some(PathBuf::from(v))` and silently creates a directory
        // literally named `1` in the cwd, then writes the ledger into it.
        Some(v)
            if v == "1"
                || v.eq_ignore_ascii_case("true")
                || v.eq_ignore_ascii_case("on")
                || v.eq_ignore_ascii_case("yes") =>
        {
            (!under_test).then(default_cache_dir)
        }
        Some(v) => Some(PathBuf::from(v)),
    }
}

/// Directory holding persisted machine code (`WIE_JIT_CODE_CACHE`).
///
/// A sibling `code/` subdirectory of the ledger's own directory, so the two
/// caches share one root (one `WIE_CACHE_DIR` to point at, one place to clear)
/// but never the same file: the ledger's `.bin` and the code cache's `.code`
/// have unrelated formats, and resetting one must not touch the other.
pub(crate) fn code_cache_dir() -> PathBuf {
    default_cache_dir().join("code")
}

/// Default cache directory: `$WIE_CACHE_DIR`, else XDG cache home, else the
/// macOS / Linux user-cache convention. `None` disables persistence.
fn default_cache_dir() -> PathBuf {
    if let Ok(v) = std::env::var("WIE_CACHE_DIR")
        && !v.is_empty()
    {
        return PathBuf::from(v).join("jit");
    }
    if let Ok(v) = std::env::var("XDG_CACHE_HOME")
        && !v.is_empty()
    {
        return PathBuf::from(v).join("wie").join("jit");
    }
    #[cfg(target_os = "macos")]
    if let Ok(h) = std::env::var("HOME")
        && !h.is_empty()
    {
        return PathBuf::from(h)
            .join("Library")
            .join("Caches")
            .join("wie")
            .join("jit");
    }
    #[allow(unreachable_code)] // Linux branch follows the macOS early return
    if let Ok(h) = std::env::var("HOME")
        && !h.is_empty()
    {
        return PathBuf::from(h).join(".cache").join("wie").join("jit");
    }
    PathBuf::from("wie-jit-cache") // last resort relative dir
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    /// Fresh scratch dir per test (no tempfile dep).
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wie-jit-cache-test-{}-{}-{:?}",
            tag,
            std::process::id(),
            std::thread::current().id()
        ));
        let _cleaned: std::io::Result<()> = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn test_memory(bytes_at: &[(&u64, &[u8])], region: u64, size: u64) -> GuestMemory {
        let mut mem = GuestMemory::new();
        let size_us = usize::try_from(size).expect("size");
        mem.map(region, size_us, crate::RwxPerms::ALL)
            .expect("map guest region");
        for (addr, bytes) in bytes_at {
            mem.write(**addr, bytes).expect("write guest bytes");
        }
        mem
    }

    const PE_A: u64 = 0xA11C_E000_0000_0001;
    const REGION: u64 = 0x0040_0000;
    const VA1: u64 = REGION + 0x100;

    /// Hand-written on-disk record, for the paths this build never writes.
    fn disk_rec(tier: OptTier, va: u64, end: u64) -> DiskEntry {
        DiskEntry {
            va,
            guest_end: end,
            bytes_hash: 42,
            insn_count: 3,
            inv_gen: 9,
            never: false,
            compiled_at_opt: tier.code(),
        }
    }

    #[test]
    fn round_trip_ready_and_never() {
        let dir = scratch_dir("round-trip");
        let code = [0x90_u8; 64];
        let bad = [0xCC_u8; 32];
        {
            // Writer process-equivalent: attach to PE A, record one Ready and
            // one Never block against identical bytes.
            let mem = test_memory(&[(&VA1, &code), (&(VA1 + 0x200), &bad)], REGION, 0x1000);
            let cache = PersistentJitCache::with_base_dir(dir.clone());
            cache.attach(PE_A);
            let hash = hash_guest_range(&mem, VA1, VA1 + 64).expect("hash");
            assert_eq!(fnv1a(&code), hash);
            cache.record_ready(&mem, VA1, VA1 + 64, 9, 5);
            cache.record_never(&mem, VA1 + 0x200);
            assert!(cache.test_entries_len() >= 2);
            // Flush happens on drop; keep `cache` scoped.
        }
        // Warm boot: reload from disk into a fresh handle.
        let warm = PersistentJitCache::with_base_dir(dir.clone());
        warm.attach(PE_A);
        // Byte-validated probe with freshly built memory holding same bytes.
        let mem2 = test_memory(&[(&VA1, &code), (&(VA1 + 0x200), &bad)], REGION, 0x1000);
        let probe = warm.probe(&mem2, VA1).expect("known-good hit");
        assert_eq!(probe.insn_count, 9);
        assert!(
            warm.never_vas().contains(&(VA1 + 0x200)),
            "negative entry survived round trip"
        );
        // Probe misses when current bytes differ from recorded ones.
        let other = [0xEB_u8; 64];
        let mem3 = test_memory(&[(&VA1, &other)], REGION, 0x1000);
        assert_eq!(warm.probe(&mem3, VA1), None);
    }

    #[test]
    fn invalidation_drops_covered_entries_and_rewrites_file() {
        let dir = scratch_dir("invalidate");
        let code = [0x90_u8; 64];
        let v_outside = REGION + 0x400;
        {
            let mem = test_memory(&[(&VA1, &code), (&v_outside, &code)], REGION, 0x1000);
            let cache = PersistentJitCache::with_base_dir(dir.clone());
            cache.attach(PE_A);
            cache.record_ready(&mem, VA1, VA1 + 64, 4, 7);
            cache.record_ready(&mem, v_outside, v_outside + 64, 8, 7);
            assert_eq!(cache.test_entries_len(), 2);
            // Range covering VA1's whole extent drops exactly that record.
            cache.invalidate_range(VA1, 64);
            assert_eq!(cache.test_entries_len(), 1, "only uncovered rec remains");
        }
        // Reload proves the on-disk file was rewritten without the tombstone.
        let warm = PersistentJitCache::with_base_dir(dir.clone());
        warm.attach(PE_A);
        assert_eq!(
            warm.test_entries_len(),
            1,
            "tombstoned entry gone from disk"
        );
    }

    #[test]
    fn version_mismatch_resets_file() {
        let dir = scratch_dir("version-reset");
        let opt = JitConfig::get().opt_level();
        let path = dir.join(format!("{:016x}.bin", jit_cache_key(PE_A, opt)));
        // Hand-write a body claiming a different WIE version.
        let stale_body = FileBody {
            wie_version: "0.0.0-old".to_string(),
            format_version: FORMAT_VERSION,
            key: jit_cache_key(PE_A, opt),
            opt_level: opt.to_string(),
            entries: vec![disk_rec(OptTier::Base, VA1, VA1 + 16)],
        };
        write_body(&path, &stale_body).expect("seed stale file");
        assert!(path.exists());

        let cache = PersistentJitCache::with_base_dir(dir.clone());
        cache.attach(PE_A);
        assert_eq!(cache.test_entries_len(), 0, "stale entries rejected");
        assert!(!path.exists(), "version-mismatch file deleted (reset)");
    }

    // --- opt-level-aware keying (correctness prerequisite for variable
    // --- per-compilation opt levels) ----------------------------------------

    #[test]
    fn cache_key_varies_with_opt_level() {
        // Pure-function half: the derived key must separate every opt level
        // Cranelift accepts, and must be stable for a given one.
        let keys: Vec<u64> = ["none", "speed", "speed_and_size"]
            .iter()
            .map(|o| jit_cache_key(PE_A, o))
            .collect();
        assert_ne!(keys[0], keys[1], "none vs speed must not collide");
        assert_ne!(keys[1], keys[2], "speed vs speed_and_size must not collide");
        assert_ne!(keys[0], keys[2], "none vs speed_and_size must not collide");
        // Stable (no per-call randomness), and independent of the PE hash.
        assert_eq!(jit_cache_key(PE_A, "none"), keys[0]);
        assert_ne!(jit_cache_key(PE_A + 1, "none"), keys[0]);
        // Length prefix: "speed" must not alias a longer string sharing a prefix.
        assert_ne!(jit_cache_key(PE_A, "speedy"), keys[1]);
    }

    #[test]
    fn artifact_from_one_opt_level_is_not_served_to_another() {
        let dir = scratch_dir("opt-split");
        let code = [0x90_u8; 64];
        let bad = [0xCC_u8; 32];
        let mem = test_memory(&[(&VA1, &code), (&(VA1 + 0x200), &bad)], REGION, 0x1000);

        // Writer: a `WIE_JIT_OPT=none` process records one Ready and one Never.
        {
            let none = PersistentJitCache::with_base_dir_opt(dir.clone(), "none");
            none.attach(PE_A);
            none.record_ready(&mem, VA1, VA1 + 64, 9, 5);
            none.record_never(&mem, VA1 + 0x200);
            assert_eq!(none.test_entries_len(), 2);
        }
        // The `none` ledger must have landed on its OWN key-derived path, not
        // on the bare PE hash, and no other file may exist.
        let none_path = dir.join(format!("{:016x}.bin", jit_cache_key(PE_A, "none")));
        assert!(none_path.exists(), "none ledger file present");
        let speed_path = dir.join(format!("{:016x}.bin", jit_cache_key(PE_A, "speed")));
        assert!(!speed_path.exists(), "no speed ledger file yet");
        assert!(!dir.join(format!("{PE_A:016x}.bin")).exists());

        // Reader: a `WIE_JIT_OPT=speed` process must see NOTHING.
        {
            let speed = PersistentJitCache::with_base_dir_opt(dir.clone(), "speed");
            speed.attach(PE_A);
            assert_eq!(
                speed.test_entries_len(),
                0,
                "a none-compiled artifact must not be loaded by a speed run"
            );
            assert_eq!(speed.probe(&mem, VA1), None, "no cross-level probe hit");
            assert!(speed.never_vas().is_empty(), "no cross-level Never seed");
            // ... and it must not have clobbered the `none` ledger on drop.
        }
        assert!(none_path.exists(), "speed run left the none ledger alone");

        // Reader at the SAME opt level still gets the hit (keying did not
        // become so narrow that the cache stopped working).
        {
            let warm = PersistentJitCache::with_base_dir_opt(dir.clone(), "none");
            warm.attach(PE_A);
            assert_eq!(warm.test_entries_len(), 2);
            let probe = warm.probe(&mem, VA1).expect("same-level known-good hit");
            assert_eq!(probe.insn_count, 9);
            assert!(warm.never_vas().contains(&(VA1 + 0x200)));
        }
        // Two opt levels, two files.
        {
            let speed = PersistentJitCache::with_base_dir_opt(dir.clone(), "speed");
            speed.attach(PE_A);
            speed.record_ready(&mem, VA1, VA1 + 64, 11, 5);
        }
        assert!(speed_path.exists(), "speed ledger written to its own key");
        assert!(none_path.exists(), "none ledger still there");
    }

    #[test]
    fn body_opt_level_mismatch_resets_even_when_key_matches() {
        // Defense in depth: the key already encodes the opt level, so a
        // mismatch here means a key collision or a hand-edited file. It must
        // still be refused, not served.
        let dir = scratch_dir("opt-body-mismatch");
        let opt = JitConfig::get().opt_level();
        let key = jit_cache_key(PE_A, opt);
        let path = dir.join(format!("{key:016x}.bin"));
        let lying = FileBody {
            wie_version: env!("CARGO_PKG_VERSION").to_string(),
            format_version: FORMAT_VERSION,
            key,
            opt_level: "speed_and_size".to_string(),
            entries: vec![disk_rec(OptTier::Base, VA1, VA1 + 16)],
        };
        write_body(&path, &lying).expect("seed lying file");

        let cache = PersistentJitCache::with_base_dir(dir.clone());
        cache.attach(PE_A);
        assert_eq!(
            cache.test_entries_len(),
            0,
            "opt-level mismatch in the body must be refused"
        );
        assert!(!path.exists(), "mismatched file deleted (reset)");
    }

    #[test]
    fn disabled_cache_is_inert() {
        let cache = PersistentJitCache::disabled();
        cache.attach(PE_A);
        let mem = test_memory(&[(&VA1, &[0x90; 8])], REGION, 0x1000);
        cache.record_ready(&mem, VA1, VA1 + 8, 1, 0);
        cache.invalidate_range(VA1, 8);
        cache.clear_all(true);
        assert_eq!(cache.test_entries_len(), 0);
        assert_eq!(cache.test_tier_entries_len(), 0);
        assert!(cache.probe(&mem, VA1).is_none());
        assert!(cache.never_vas().is_empty());
    }

    // --- per-record `compiled_at_opt` (the tier-up prerequisite) ----------
    //
    // Task 1 keyed the cache on the RUN-level opt level, which is sound while
    // one process compiles everything at one level. Per-block tiering breaks
    // that: one process compiles the same guest VA at both levels, so the
    // record has to name the level that produced it. These tests are the
    // RECORD-level half; `artifact_from_one_opt_level_is_not_served_to_another`
    // above is the file-level half.

    #[test]
    fn speed_compiled_record_is_not_served_to_a_none_compile() {
        // A record that only ever succeeded at `speed`, sitting on the `none`
        // ledger's own key (key collision or hand edit): it must be refused at
        // the RECORD, not served.
        let dir = scratch_dir("rec-speed-into-none");
        let code = [0x90_u8; 64];
        let opt = JitConfig::get().opt_level();
        let key = jit_cache_key(PE_A, opt);
        let path = dir.join(format!("{key:016x}.bin"));
        write_body(
            &path,
            &FileBody {
                wie_version: env!("CARGO_PKG_VERSION").to_string(),
                format_version: FORMAT_VERSION,
                key,
                opt_level: opt.to_string(),
                entries: vec![disk_rec(OptTier::Speed, VA1, VA1 + 64)],
            },
        )
        .expect("seed file");

        let cache = PersistentJitCache::with_base_dir_opt(dir.clone(), "none");
        cache.attach(PE_A);
        assert_eq!(
            cache.test_entries_len(),
            0,
            "a speed-compiled record must not be served to a none compile"
        );
        let mem = test_memory(&[(&VA1, &code)], REGION, 0x1000);
        assert_eq!(
            cache.probe(&mem, VA1),
            None,
            "no probe hit from a speed record"
        );
        assert!(cache.never_vas().is_empty());
    }

    #[test]
    fn none_compiled_record_is_not_served_to_a_speed_compile() {
        let dir = scratch_dir("rec-none-into-speed");
        let code = [0x90_u8; 64];
        let key = jit_cache_key(PE_A, "speed");
        let path = dir.join(format!("{key:016x}.bin"));
        write_body(
            &path,
            &FileBody {
                wie_version: env!("CARGO_PKG_VERSION").to_string(),
                format_version: FORMAT_VERSION,
                key,
                opt_level: "speed".to_string(),
                entries: vec![disk_rec(OptTier::Base, VA1, VA1 + 64)],
            },
        )
        .expect("seed file");

        let cache = PersistentJitCache::with_base_dir_opt(dir.clone(), "speed");
        cache.attach(PE_A);
        assert_eq!(
            cache.test_entries_len(),
            0,
            "a none-compiled record must not be served to a speed compile"
        );
        let mem = test_memory(&[(&VA1, &code)], REGION, 0x1000);
        assert_eq!(
            cache.probe(&mem, VA1),
            None,
            "no probe hit from a none record"
        );
    }

    #[test]
    fn unknown_record_tier_code_is_rejected() {
        let dir = scratch_dir("rec-unknown-code");
        let opt = JitConfig::get().opt_level();
        let key = jit_cache_key(PE_A, opt);
        let path = dir.join(format!("{key:016x}.bin"));
        let mut bad = disk_rec(OptTier::Base, VA1, VA1 + 64);
        bad.compiled_at_opt = 200; // no such tier
        write_body(
            &path,
            &FileBody {
                wie_version: env!("CARGO_PKG_VERSION").to_string(),
                format_version: FORMAT_VERSION,
                key,
                opt_level: opt.to_string(),
                entries: vec![bad],
            },
        )
        .expect("seed file");

        let cache = PersistentJitCache::with_base_dir(dir.clone());
        cache.attach(PE_A);
        assert_eq!(
            cache.test_entries_len(),
            0,
            "an unknown tier code must be rejected, never guessed"
        );
    }

    #[test]
    fn tiered_record_lands_in_the_tier_ledger_only() {
        // End-to-end routing: a VA compiled at the tier level is filed under
        // the TIER key, so the base run cannot read its own record back, while
        // a later `WIE_JIT_OPT=speed` run loads that same file as its base.
        let dir = scratch_dir("rec-tier-routing");
        let code = [0x90_u8; 64];
        let mem = test_memory(&[(&VA1, &code)], REGION, 0x1000);
        {
            let none = PersistentJitCache::with_base_dir_opt(dir.clone(), "none");
            none.attach(PE_A);
            none.mark_tiered(VA1);
            none.record_ready(&mem, VA1, VA1 + 64, 7, 2);
            assert_eq!(none.test_entries_len(), 0, "not in the base ledger");
            assert_eq!(none.test_tier_entries_len(), 1, "in the tier ledger");
            assert_eq!(none.probe(&mem, VA1), None, "base run cannot read it back");
        }
        // A second, unrelated VA in the same process stays on the base ledger.
        {
            let none = PersistentJitCache::with_base_dir_opt(dir.clone(), "none");
            none.attach(PE_A);
            let other = VA1 + 0x200;
            none.record_ready(&mem, other, other + 64, 8, 2);
            assert_eq!(none.test_entries_len(), 1);
            assert_eq!(
                none.test_tier_entries_len(),
                0,
                "a fresh handle never LOADS the tier ledger — it may write tier \
                 verdicts but must not consume another level's"
            );
        }
        // The `speed` run reads the tier file as its OWN base file.
        {
            let speed = PersistentJitCache::with_base_dir_opt(dir.clone(), "speed");
            speed.attach(PE_A);
            let probe = speed
                .probe(&mem, VA1)
                .expect("tier knowledge reaches speed");
            assert_eq!(probe.insn_count, 7);
            assert_eq!(speed.probe(&mem, VA1 + 0x200), None);
        }
    }

    #[test]
    fn format_version_is_three_and_v2_files_are_deleted() {
        // v2 recorded no per-record level, so those files are not merely
        // unreadable — they would be read as if every record were base-level.
        assert_eq!(FORMAT_VERSION, 3);
        let dir = scratch_dir("v2-reset");
        let opt = JitConfig::get().opt_level();
        let key = jit_cache_key(PE_A, opt);
        let path = dir.join(format!("{key:016x}.bin"));
        write_body(
            &path,
            &FileBody {
                wie_version: env!("CARGO_PKG_VERSION").to_string(),
                format_version: 2,
                key,
                opt_level: opt.to_string(),
                entries: vec![disk_rec(OptTier::Base, VA1, VA1 + 16)],
            },
        )
        .expect("seed v2 file");
        assert!(path.exists());

        let cache = PersistentJitCache::with_base_dir(dir.clone());
        cache.attach(PE_A);
        assert_eq!(cache.test_entries_len(), 0, "v2 records rejected");
        assert!(!path.exists(), "v2 file deleted, not orphaned");
    }

    #[test]
    fn fnv1a_is_stable() {
        // FNV-1a reference vector for "foobar".
        assert_eq!(fnv1a(b"foobar"), 0x85944171f73967e8);
    }

    #[test]
    fn test_processes_never_persist_by_default() {
        // We ARE a `cfg(test)` process, so the default must be disabled. This
        // is the property the nextest integration suites depend on: they link
        // wie-cpu WITHOUT `cfg(test)`, so for them the load-bearing half of
        // the condition is the `NEXTEST` probe, and `under_test` below stands
        // in for it.
        assert!(under_test_process());
        assert!(resolve_config(None, true).is_none());
    }

    #[test]
    fn non_test_process_keeps_the_default_dir() {
        // The non-test default must be unchanged by the isolation work.
        assert!(resolve_config(None, false).is_some());
    }

    #[test]
    fn explicit_env_always_wins_over_test_detection() {
        // `under_test` says "off", but an explicit dir is honoured so a
        // developer can debug the ledger from a test.
        let explicit = Some("/tmp/wie-jit-cache-explicit".to_string());
        assert_eq!(
            resolve_config(explicit.clone(), true),
            Some(PathBuf::from("/tmp/wie-jit-cache-explicit"))
        );
        // ... and the disabling spellings still disable, in tests and out.
        for off in ["", "0", "false", "FALSE", "off", "Off"] {
            let v = Some(off.to_string());
            assert!(
                resolve_config(v.clone(), false).is_none(),
                "{off:?} must disable"
            );
            assert!(resolve_config(v, true).is_none(), "{off:?} must disable");
        }
    }

    #[test]
    fn boolean_on_spellings_resolve_to_the_default_dir_not_a_relative_path() {
        // Regression: `WIE_JIT_CACHE=1` used to fall through to
        // `Some(PathBuf::from("1"))`, creating a directory named `1` in the cwd
        // and writing the ledger into it. Every knob in this table spells "on"
        // as `1`/`true`/`on`/`yes`, and `WIE_JIT_CODE_CACHE` already honours
        // exactly those, so this knob must too.
        for on in ["1", "true", "TRUE", "on", "On", "yes", "YES"] {
            let v = Some(on.to_string());
            assert_eq!(
                resolve_config(v.clone(), false),
                Some(default_cache_dir()),
                "{on:?} must mean \"on\", not a directory called {on:?}"
            );
            // ... and in a test process it must not re-enable persistence, the
            // same isolation guarantee `None` gets.
            assert!(
                resolve_config(v, true).is_none(),
                "{on:?} must not defeat test isolation"
            );
        }
    }

    #[test]
    fn a_real_directory_override_is_still_honoured() {
        // The boolean arm must not swallow genuine paths.
        let dir = Some("/tmp/wie-jit-cache-some-dir".to_string());
        assert_eq!(
            resolve_config(dir.clone(), false),
            Some(PathBuf::from("/tmp/wie-jit-cache-some-dir"))
        );
        assert_eq!(
            resolve_config(dir, true),
            Some(PathBuf::from("/tmp/wie-jit-cache-some-dir"))
        );
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod write_order {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wie-ledger-write-order-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _c: std::io::Result<()> = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    const PE: u64 = 0xA11C_E000_0000_0005;
    const BASE: u64 = 0x0080_0000;

    fn mem_with(len: usize) -> GuestMemory {
        let mut mem = GuestMemory::new();
        let size = (len + 0x2000) & !0xFFF;
        mem.map(BASE & !0xFFF, size, crate::RwxPerms::ALL)
            .expect("map");
        mem.write(BASE, &vec![0x90_u8; len]).expect("write");
        mem
    }

    fn rec(va: u64, end: u64, never: bool) -> LedgerRec {
        LedgerRec {
            va,
            guest_end: end,
            bytes_hash: 1,
            insn_count: if never { 0 } else { 3 },
            inv_gen: 0,
            never,
            compiled_at_opt: OptTier::Base,
        }
    }

    /// The write-wins contract, exhaustively and without threads.
    #[test]
    fn ready_beats_never_and_the_last_ready_wins() {
        let ready = rec(BASE, BASE + 8, false);
        let never = rec(BASE, BASE + 32, true);
        // Into an empty key, either write is admissible.
        assert!(PersistentJitCache::write_wins(None, &ready));
        assert!(PersistentJitCache::write_wins(None, &never));
        // Never may not displace a Ready, in either arrival order.
        assert!(!PersistentJitCache::write_wins(Some(ready), &never));
        // Ready displaces Never, and a later Ready displaces an earlier one.
        assert!(PersistentJitCache::write_wins(Some(never), &ready));
        assert!(PersistentJitCache::write_wins(
            Some(rec(BASE, BASE + 4, false)),
            &ready
        ));
        // Never re-hashes over Never: the newest verdict about the bytes wins.
        assert!(PersistentJitCache::write_wins(Some(never), &never));
    }

    /// Ten overlapping blocks queued off one 10-byte walk — what the JIT does
    /// when the guest interprets and the entry block's install loses the race —
    /// must all reach the file, the entry block included, in any order.
    #[test]
    fn overlapping_records_in_one_window_all_reach_the_file() {
        let mem = mem_with(16);
        let order = [
            BASE + 9,
            BASE + 1,
            BASE + 5,
            BASE + 2,
            BASE + 8,
            BASE,
            BASE + 3,
            BASE + 7,
            BASE + 4,
            BASE + 6,
        ];
        let dir = scratch("overlap");
        let cache = PersistentJitCache::with_base_dir(dir.clone());
        cache.attach(PE);
        for &va in &order {
            cache.record_ready(&mem, va, BASE + 10, 1, 0);
        }
        assert_eq!(cache.test_vas().len(), 10, "all ten rows in the table");
        cache.test_flush();
        let disk = PersistentJitCache::test_disk_vas(dir.clone(), PE);
        assert_eq!(
            disk.len(),
            10,
            "flush wrote {} of 10: {disk:x?}",
            disk.len()
        );
        assert!(disk.contains(&BASE), "entry block row on disk: {disk:x?}");
        assert_eq!(
            PersistentJitCache::test_disk_probe(dir, PE, &mem, BASE),
            Some(1),
            "entry block probes clean after reload"
        );
    }

    /// Two instals of the *same* key in one flush window: the newest wins, and
    /// the file holds exactly one row for that key.
    #[test]
    fn newest_ready_wins_for_a_key_written_twice_in_one_window() {
        let mem = mem_with(16);
        let dir = scratch("twice");
        {
            let cache = PersistentJitCache::with_base_dir(dir.clone());
            cache.attach(PE);
            cache.record_ready(&mem, BASE, BASE + 4, 1, 0);
            cache.record_ready(&mem, BASE, BASE + 12, 3, 0);
            cache.test_flush();
            assert_eq!(cache.test_vas(), vec![BASE], "one key, one row");
        }
        assert_eq!(
            PersistentJitCache::test_disk_probe(dir.clone(), PE, &mem, BASE),
            Some(3),
            "on-disk row carries the newest install's insn_count"
        );
        assert_eq!(PersistentJitCache::test_disk_vas(dir, PE), vec![BASE]);
    }

    /// The bug this module was opened for: `Ready` and `Never` for the same VA
    /// reach the ledger from different threads (a worker's failure verdict for a
    /// queued block against a guest thread's successful install). Whichever
    /// lands last, the row must be the `Ready` one — a `Never` makes `probe`
    /// report *absent*, so a lost race used to erase a block that compiles fine
    /// from the next run's ledger entirely.
    ///
    /// Synchronised two-thread first contact, so each round has exactly one
    /// window. Against the pre-fix code this failed in ~44 % of rounds (0 rows
    /// ever missing — the row was always there, saying the opposite), so
    /// [`ROUNDS`] is two orders of magnitude more than enough.
    #[test]
    fn ready_beats_never_under_a_concurrent_first_contact() {
        const ROUNDS: usize = 4_000;
        let mem = Arc::new(mem_with(ROUNDS + 0x1000));
        let cache = Arc::new(PersistentJitCache::with_base_dir(scratch("race")));
        cache.attach(PE);
        let round = Arc::new(AtomicUsize::new(0));
        let wrong = Arc::new(AtomicUsize::new(0));
        let go = Arc::new(Barrier::new(3));
        let done = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for records_ready in [true, false] {
            let (cache, mem, round, go, done) = (
                Arc::clone(&cache),
                Arc::clone(&mem),
                Arc::clone(&round),
                Arc::clone(&go),
                Arc::clone(&done),
            );
            handles.push(std::thread::spawn(move || {
                for _ in 0..ROUNDS {
                    go.wait();
                    let va = BASE + round.load(Ordering::Relaxed) as u64;
                    if records_ready {
                        cache.record_ready(&mem, va, va + 8, 3, 0);
                    } else {
                        cache.record_never(&mem, va);
                    }
                    done.wait();
                }
            }));
        }
        for i in 0..ROUNDS {
            round.store(i, Ordering::Relaxed);
            go.wait(); // both writers start together
            done.wait(); // both have finished
            let va = BASE + i as u64;
            let row = cache.active_table().and_then(|t| t.pin().get(&va).copied());
            // A `Ready` was recorded for this VA in this round, so the surviving
            // row must be that `Ready` — and it must exist.
            match row {
                Some(r) if !r.never => {}
                _ => {
                    wrong.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        for h in handles {
            h.join().expect("join");
        }
        assert_eq!(
            wrong.load(Ordering::Relaxed),
            0,
            "a concurrent Never verdict erased a Ready row in {} of {ROUNDS} rounds",
            wrong.load(Ordering::Relaxed)
        );
    }

    /// An invalidation must reach the append buffer, not only the table: the
    /// flush it triggers folds queued rows back in for keys the table no longer
    /// holds, so a row invalidated in the same window it was recorded in would
    /// be written straight back to disk.
    #[test]
    fn invalidated_pending_record_is_not_written_back() {
        let mem = mem_with(16);
        let dir = scratch("tombstone");
        let cache = PersistentJitCache::with_base_dir(dir.clone());
        cache.attach(PE);
        cache.record_ready(&mem, BASE, BASE + 16, 4, 0);
        assert_eq!(cache.test_pending_len(), 1, "queued");
        // The invalidation's own flush must leave nothing on disk for this VA —
        // no second flush to clean up after it.
        cache.invalidate_range(BASE, 16);
        assert!(cache.test_vas().is_empty(), "dropped in memory");
        assert_eq!(cache.test_pending_len(), 0, "dropped from the buffer");
        let disk = PersistentJitCache::test_disk_vas(dir, PE);
        assert!(
            disk.is_empty(),
            "invalidated row written back from the append buffer: {disk:x?}"
        );
    }
}
