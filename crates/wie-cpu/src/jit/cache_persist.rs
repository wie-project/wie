//! Persistent JIT code-cache ledger (`WIE_JIT_CACHE`).
//!
//! **Chosen approach: known-good metadata ledger, NOT raw machine-code
//! restore.** Re-materializing Cranelift-emitted aarch64 code is not feasible
//! safely with the current [`JitEngine`] setup:
//!
//! - `is_pic = false` + `use_colocated_libcalls = false`: block bodies bake
//!   absolute / short-branch references to libcall trampolines and import
//!   stubs inside the same `JITModule` text region. Region base addresses and
//!   inter-section distances differ between runs (mmap ASLR + differing
//!   allocation sequences), so replayed bytes would branch into garbage.
//! - Cranelift does not expose per-function relocation records we could re-
//!   apply on restore.
//! - Restored blocks cannot join the `FuncId`-keyed chain-table world without
//!   going through the module, which defeats the purpose.
//!
//! The ledger therefore persists per-PE *metadata*: key =
//! `(pe_hash, guest_va, fnv1a(guest bytes))`, value = `{guest_start,
//! guest_end, insn_count, inv_gen, never}` (spec metadata minus machine-code
//! bytes). On warm boot, [`JitShared::attach_pe_cache`] bulk-loads the file;
//! every later consumption re-validates the CURRENT guest bytes against the
//! recorded hash before acting on it, so any stale/SMC-diverged entry simply
//! probes as absent and falls back to the normal cold path.
//!
//! Warm-boot savings (honest accounting): known-good blocks skip the Hot
//! visit-threshold warmup entirely (immediate background compile), and
//! known-bad (`Never`) blocks skip repeated decode attempts. The ~3 ms/block
//! Cranelift cost itself is NOT eliminated by this module; eliminating it
//! requires either position-independent emit with serialized relocations or a
//! real tier-0 emitter (see docs/RUNBOOK.md knob table note).

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
const FORMAT_VERSION: u32 = 1;

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
#[must_use]
pub fn jit_cache_pe_hash(pe_file_bytes: &[u8]) -> u64 {
    fnv1a(pe_file_bytes)
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
    pe_hash: u64,
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
/// the same map.
type PeTables = RwLock<ahash::HashMap<u64, Arc<papaya::HashMap<u64, LedgerRec>>>>;

/// Process-wide persistent JIT cache handle. Constructed always (cheap even
/// disabled); all methods are no-ops when [`Self::enabled`] is false and all
/// I/O errors are swallowed into a warn-once + permanent disable, so any
/// filesystem trouble falls back to cold compilation transparently.
pub(super) struct PersistentJitCache {
    enabled: bool,
    base_dir: PathBuf,
    /// Active PE hash (0 == none attached yet).
    active_pe: AtomicU64,
    /// Load-once latch per attached PE.
    tables: PeTables,
    flush: Mutex<FlushState>,
}

impl PersistentJitCache {
    /// Resolve config from the environment: `WIE_JIT_CACHE`.
    ///
    /// - unset → default dir under `$WIE_CACHE_DIR` / XDG cache home
    ///   (`wie/jit`), DISABLED under `cfg(test)` (suite determinism);
    /// - `0` / `false` / `off` → disabled;
    /// - anything else → that value used as the cache directory.
    pub(super) fn new() -> Self {
        let resolved: Option<Option<PathBuf>> = match std::env::var("WIE_JIT_CACHE") {
            Ok(v)
                if v.is_empty()
                    || v == "0"
                    || v.eq_ignore_ascii_case("false")
                    || v.eq_ignore_ascii_case("off") =>
            {
                Some(None) // explicitly disabled
            }
            Ok(v) => Some(Some(PathBuf::from(v))), // explicit dir override
            Err(_) => (!cfg!(test)).then_some(Some(default_cache_dir())), // default on (off in tests)
        };
        match resolved.flatten() {
            Some(dir) => Self {
                enabled: true,
                base_dir: dir,
                active_pe: AtomicU64::new(0),
                tables: RwLock::new(ahash::HashMap::default()),
                flush: Mutex::new(FlushState::new()),
            },
            None => Self::disabled(),
        }
    }

    fn disabled() -> Self {
        Self {
            enabled: false,
            base_dir: PathBuf::new(),
            active_pe: AtomicU64::new(0),
            tables: RwLock::new(ahash::HashMap::default()),
            flush: Mutex::new(FlushState::new()),
        }
    }

    #[cfg(test)]
    fn with_base_dir(dir: PathBuf) -> Self {
        Self {
            enabled: true,
            base_dir: dir,
            active_pe: AtomicU64::new(0),
            tables: RwLock::new(ahash::HashMap::default()),
            flush: Mutex::new(FlushState::new()),
        }
    }

    fn file_path_for(&self, pe: u64) -> PathBuf {
        self.base_dir.join(format!("{pe:016x}.bin"))
    }

    /// Current attached PE hash (0 == none).
    pub(super) fn active_pe(&self) -> u64 {
        self.active_pe.load(Ordering::Acquire)
    }

    /// Attach + bulk-load the ledger for `pe_hash`. Idempotent per PE; errors
    /// degrade to "no ledger" (warn-once), never propagate.
    pub(super) fn attach(&self, pe_hash: u64) {
        if !self.enabled || pe_hash == 0 {
            return;
        }
        {
            let tables = self.tables.read().unwrap_or_else(|e| e.into_inner());
            if tables.contains_key(&pe_hash) {
                self.active_pe.store(pe_hash, Ordering::Release);
                return; // already loaded
            }
        }
        let map = self.load_file(pe_hash);
        {
            let mut tables = self.tables.write().unwrap_or_else(|e| e.into_inner());
            tables.entry(pe_hash).or_insert(Arc::new(map));
        }
        self.active_pe.store(pe_hash, Ordering::Release);
        tracing::debug!(pe = format_args!("{pe_hash:#x}"), "jit disk cache attached");
    }

    /// Read + version-validate `<pe>.bin`. A version/magic/WIE-version
    /// mismatch DELETES the stale file and returns an empty map ("reset").
    fn load_file(&self, pe: u64) -> papaya::HashMap<u64, LedgerRec> {
        let empty = papaya::HashMap::new();
        let path = self.file_path_for(pe);
        let body = match load_body(&path) {
            Ok(b) => b,
            Err(_) => return empty, // missing or unreadable: cold boot, keep quiet
        };
        if body.format_version != FORMAT_VERSION
            || body.wie_version != env!("CARGO_PKG_VERSION")
            || body.entries.len() > MAX_DISK_ENTRIES
        {
            // Version mismatch resets the file entirely (requirement). A
            // missing file is equivalent to reset.
            if fs::remove_file(&path).is_ok() {
                tracing::info!(
                    pe = format_args!("{pe:#x}"),
                    "jit disk cache version mismatch — reset"
                );
            }
            return empty;
        }
        let map = papaya::HashMap::with_capacity(body.entries.len());
        {
            let pin = map.pin();
            for e in body.entries {
                if e.guest_end < e.va {
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
                    },
                );
            }
        }
        map
    }

    fn active_table(&self) -> Option<Arc<papaya::HashMap<u64, LedgerRec>>> {
        let pe = self.active_pe.load(Ordering::Acquire);
        if pe == 0 {
            return None;
        }
        let tables = self.tables.read().unwrap_or_else(|e| e.into_inner());
        tables.get(&pe).cloned()
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

    /// Record one successfully installed block (inline or worker install).
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
        self.insert_rec(LedgerRec {
            va,
            guest_end,
            bytes_hash: hash,
            insn_count,
            inv_gen,
            never: false,
        });
    }

    /// Record a `Never` verdict for `va` (fixed-window hash validation).
    pub(super) fn record_never(&self, mem: &GuestMemory, va: u64) {
        if !self.enabled {
            return;
        }
        let Some(table) = self.active_table() else {
            return;
        };
        if table.pin().get(&va).is_some() {
            return; // already recorded (Ready beats Never)
        }
        let win = u64::try_from(NEVER_WINDOW).unwrap_or(u64::MAX);
        let Some(hash) = hash_guest_range(mem, va, va.saturating_add(win)) else {
            return;
        };
        self.insert_rec(LedgerRec {
            va,
            guest_end: va.saturating_add(win),
            bytes_hash: hash,
            insn_count: 0,
            inv_gen: 0,
            never: true,
        });
    }

    fn insert_rec(&self, rec: LedgerRec) {
        let Some(table) = self.active_table() else {
            return;
        };
        // Ready upserts overwrite older Never records for the same VA.
        table.pin().insert(rec.va, rec);
        {
            let mut st = self.flush.lock().unwrap_or_else(|e| e.into_inner());
            st.pending.push(rec);
        }
        self.flush_tick();
    }

    /// Drop ledger records overlapping `[addr, addr+len)` (in-memory now, on
    /// disk at next flush via full rewrite) — SMC / X-loss invalidations.
    pub(super) fn invalidate_range(&self, addr: u64, len: usize) {
        if !self.enabled || len == 0 {
            return;
        }
        let end = addr.saturating_add(u64::try_from(len).unwrap_or(u64::MAX));
        let Some(table) = self.active_table() else {
            return;
        };
        let pin = table.pin();
        let doomed: Vec<u64> = pin
            .iter()
            .filter(|(_, r)| r.overlaps(addr, end))
            .map(|(&va, _)| va)
            .collect();
        if doomed.is_empty() {
            return;
        }
        for va in &doomed {
            pin.remove(va);
        }
        drop(pin);
        {
            let mut st = self.flush.lock().unwrap_or_else(|e| e.into_inner());
            st.dirty_rewrite = true;
            st.last_flush = Instant::now(); // invalidation flushes soon (below)
        }
        // Rewrite promptly (metadata files are small).
        self.flush_now(false);
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
        let Some(table) = self.active_table() else {
            return;
        };
        table.pin().clear();
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

    /// Serialize the active PE's table (+ buffered appends) to
    /// `<dir>/<pe-hash>.bin` via temp-file rename. `force_fsync` also calls
    /// `sync_all` (the lazy-fsync policy keeps ordinary flushes unsynced).
    fn flush_now(&self, force_fsync: bool) {
        if !self.enabled {
            return;
        }
        let pe = self.active_pe.load(Ordering::Acquire);
        if pe == 0 {
            return;
        }
        let Some(table) = self.active_table() else {
            return;
        };
        let mut snapshot: Vec<DiskEntry> = {
            let pin = table.pin();
            pin.iter()
                .map(|(_, r)| DiskEntry {
                    va: r.va,
                    guest_end: r.guest_end,
                    bytes_hash: r.bytes_hash,
                    insn_count: r.insn_count,
                    inv_gen: r.inv_gen,
                    never: r.never,
                })
                .collect()
        };
        {
            let mut st = self.flush.lock().unwrap_or_else(|e| e.into_inner());
            for r in st.pending.drain(..) {
                // Pending additions may have raced the snapshot above; fold
                // them back in so a very short-lived process still persists.
                if !snapshot.iter().any(|e| e.va == r.va) {
                    snapshot.push(DiskEntry {
                        va: r.va,
                        guest_end: r.guest_end,
                        bytes_hash: r.bytes_hash,
                        insn_count: r.insn_count,
                        inv_gen: r.inv_gen,
                        never: r.never,
                    });
                }
            }
            st.dirty_rewrite = false;
        }
        let body = FileBody {
            wie_version: env!("CARGO_PKG_VERSION").to_string(),
            format_version: FORMAT_VERSION,
            pe_hash: pe,
            entries: snapshot,
        };
        let path = self.file_path_for(pe);
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
        let path = dir.join(format!("{PE_A:016x}.bin"));
        // Hand-write a body claiming a different WIE version.
        let stale_body = FileBody {
            wie_version: "0.0.0-old".to_string(),
            format_version: FORMAT_VERSION,
            pe_hash: PE_A,
            entries: vec![DiskEntry {
                va: VA1,
                guest_end: VA1 + 16,
                bytes_hash: 42,
                insn_count: 3,
                inv_gen: 9,
                never: false,
            }],
        };
        write_body(&path, &stale_body).expect("seed stale file");
        assert!(path.exists());

        let cache = PersistentJitCache::with_base_dir(dir.clone());
        cache.attach(PE_A);
        assert_eq!(cache.test_entries_len(), 0, "stale entries rejected");
        assert!(!path.exists(), "version-mismatch file deleted (reset)");
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
        assert!(cache.probe(&mem, VA1).is_none());
        assert!(cache.never_vas().is_empty());
    }

    #[test]
    fn fnv1a_is_stable() {
        // FNV-1a reference vector for "foobar".
        assert_eq!(fnv1a(b"foobar"), 0x85944171f73967e8);
    }
}
