//! Host / virtual FS backend ops for the guest path namespace.

use super::path::{guest_basename, paths_equal_ci, wildcard_match};
use super::volume::{VolumeConfig, guest_path_to_host};
use ahash::HashMapExt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Win32-ish file attributes we surface.
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

// ── Host stat cache ────────────────────────────────────────────────────
//
// A guest file probe typically costs 3-4 real `stat()` syscalls: 7-Zip does
// `GetFileAttributes` → `CreateFile` → `GetFileInformationByHandle` →
// `CloseHandle` per archive member, and the size-threshold check in
// `allocate_open_file_ex` adds another. On a wide directory tree that
// dominates scan time.
//
// Two independent guards keep the cache honest:
//
// 1. **Mutation epoch** — every mutating VFS entry point bumps
//    [`FS_EPOCH`]. A cache entry recorded at an older epoch is discarded,
//    so writes *we* perform are never masked by a stale hit. This is exact,
//    not heuristic.
// 2. **Wall-clock TTL** — bounds staleness w.r.t. mutations made by other
//    host processes, which we cannot observe. Deliberately short: the
//    3-4 stats inside one guest API sequence land microseconds apart, so a
//    small TTL captures the whole win while keeping externally-visible
//    staleness under [`STAT_TTL`].
//
// Prior behaviour also had no cross-process guarantee — each `stat()` was a
// point-in-time snapshot — so this narrows, rather than introduces, a race.

/// Global filesystem mutation counter; bumped by every mutating entry point.
static FS_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Max age of a cached stat before it is re-issued.
const STAT_TTL: std::time::Duration = std::time::Duration::from_millis(50);

/// Cap on cached paths per thread; cleared wholesale on overflow (a scan that
/// touches this many distinct paths gets no reuse from the older entries anyway).
const STAT_CACHE_CAP: usize = 1024;

#[derive(Clone)]
struct StatCacheEntry {
    epoch: u64,
    at: std::time::Instant,
    stat: PathStat,
}

thread_local! {
    static STAT_CACHE: std::cell::RefCell<ahash::HashMap<PathBuf, StatCacheEntry>> =
        std::cell::RefCell::new(ahash::HashMap::new());
}

/// Invalidate every cached stat. Called by mutating entry points.
///
/// Uses a global epoch bump rather than a targeted eviction: mutations are
/// rare relative to stats, and a bump is one relaxed atomic add versus
/// having to reason about which derived paths a rename/copy touched.
pub fn bump_fs_epoch() {
    FS_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

fn stat_cache_get(host: &Path) -> Option<PathStat> {
    let epoch_now = FS_EPOCH.load(std::sync::atomic::Ordering::Relaxed);
    STAT_CACHE.with(|c| {
        let cache = c.borrow();
        let entry = cache.get(host)?;
        if entry.epoch != epoch_now || entry.at.elapsed() > STAT_TTL {
            return None;
        }
        Some(entry.stat.clone())
    })
}

fn stat_cache_put(host: &Path, stat: &PathStat) {
    let epoch_now = FS_EPOCH.load(std::sync::atomic::Ordering::Relaxed);
    STAT_CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        if cache.len() >= STAT_CACHE_CAP {
            cache.clear();
        }
        cache.insert(
            host.to_path_buf(),
            StatCacheEntry {
                epoch: epoch_now,
                at: std::time::Instant::now(),
                stat: stat.clone(),
            },
        );
    });
}

/// Open fully into memory when size ≤ this (also gates guest I/O mirror).
pub const BUFFER_SIZE_THRESHOLD: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    NotFound,
    File,
    Directory,
}

#[derive(Debug, Clone)]
pub struct PathStat {
    pub kind: PathKind,
    pub size: u64,
    pub attributes: u32,
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub attributes: u32,
    pub size: u64,
}

/// Context for resolving existence across bottle/D/mounts/virtual/main PE.
pub struct ResolveCtx<'a> {
    pub volumes: &'a VolumeConfig,
    pub main_module_path: &'a str,
    pub main_module_file_name: &'a str,
    pub host_file_mounts: &'a [(String, PathBuf)],
    pub virtual_files: &'a [(String, usize)],
    /// Synthetic directories that always exist as dirs (skeleton / known probes).
    pub synthetic_dirs: &'a [&'a str],
}

impl ResolveCtx<'_> {
    pub fn path_is_main_module(&self, path: &str) -> bool {
        paths_equal_ci(path, self.main_module_path)
            || guest_basename(path).eq_ignore_ascii_case(self.main_module_file_name)
    }
}

/// Stat a guest path.
pub fn stat_path(ctx: &ResolveCtx<'_>, full_path: &str) -> PathStat {
    if full_path.is_empty() {
        return not_found();
    }

    // Drive root and synthetic dirs.
    let norm = full_path.trim_end_matches('\\');
    if is_drive_root(full_path) || is_synthetic_dir(ctx, full_path) {
        return PathStat {
            kind: PathKind::Directory,
            size: 0,
            attributes: FILE_ATTRIBUTE_DIRECTORY,
        };
    }

    if ctx.path_is_main_module(full_path) {
        return PathStat {
            kind: PathKind::File,
            size: 0, // caller may fill from executable bytes
            attributes: FILE_ATTRIBUTE_ARCHIVE,
        };
    }

    for (guest, host) in ctx.host_file_mounts {
        if paths_equal_ci(full_path, guest) {
            return stat_host_path(host);
        }
    }

    for (guest, size) in ctx.virtual_files {
        if paths_equal_ci(full_path, guest) {
            return PathStat {
                kind: PathKind::File,
                size: u64::try_from(*size).unwrap_or(0),
                attributes: FILE_ATTRIBUTE_ARCHIVE,
            };
        }
    }

    if let Some(map) = guest_path_to_host(ctx.volumes, full_path) {
        return stat_host_path(&map.host);
    }

    // Without bottle, treat synthetic dirs only; files unknown.
    // Also: trailing slash → directory probe of parent name.
    if full_path.ends_with('\\') || full_path.ends_with('/') {
        let parent = norm;
        if is_synthetic_dir(ctx, parent) {
            return PathStat {
                kind: PathKind::Directory,
                size: 0,
                attributes: FILE_ATTRIBUTE_DIRECTORY,
            };
        }
    }

    not_found()
}

fn not_found() -> PathStat {
    PathStat {
        kind: PathKind::NotFound,
        size: 0,
        attributes: 0,
    }
}

fn is_drive_root(path: &str) -> bool {
    let p = path.trim_end_matches(['\\', '/']);
    let b = p.as_bytes();
    b.len() == 2 && b.get(1) == Some(&b':') && b.first().is_some_and(u8::is_ascii_alphabetic)
}

fn is_synthetic_dir(ctx: &ResolveCtx<'_>, path: &str) -> bool {
    let trimmed = path.trim_end_matches(['\\', '/']);
    for d in ctx.synthetic_dirs {
        if paths_equal_ci(trimmed, d) {
            return true;
        }
    }
    // Prefix of any synthetic path that is a directory component chain.
    // e.g. C:\Users, C:\Users\WIE, C:\Windows
    let lower = trimmed.to_ascii_lowercase();
    for d in ctx.synthetic_dirs {
        let dl = d.trim_end_matches('\\').to_ascii_lowercase();
        if dl.starts_with(&lower)
            && (dl.len() == lower.len() || dl.as_bytes().get(lower.len()) == Some(&b'\\'))
        {
            return true;
        }
    }
    false
}

fn stat_host_path(host: &Path) -> PathStat {
    if let Some(hit) = stat_cache_get(host) {
        return hit;
    }
    let stat = stat_host_path_uncached(host);
    stat_cache_put(host, &stat);
    stat
}

fn stat_host_path_uncached(host: &Path) -> PathStat {
    match fs::metadata(host) {
        Ok(meta) if meta.is_dir() => PathStat {
            kind: PathKind::Directory,
            size: 0,
            attributes: FILE_ATTRIBUTE_DIRECTORY,
        },
        Ok(meta) if meta.is_file() => PathStat {
            kind: PathKind::File,
            size: meta.len(),
            attributes: FILE_ATTRIBUTE_ARCHIVE | FILE_ATTRIBUTE_NORMAL,
        },
        _ => not_found(),
    }
}

/// List directory entries for a guest path (not pattern-filtered).
pub fn list_dir(ctx: &ResolveCtx<'_>, dir_path: &str) -> Vec<DirEntry> {
    let mut entries = Vec::new();

    // Always include . and .. for real/synthetic dirs that exist.
    let st = stat_path(ctx, dir_path);
    if st.kind != PathKind::Directory {
        return entries;
    }

    entries.push(DirEntry {
        name: ".".to_owned(),
        attributes: FILE_ATTRIBUTE_DIRECTORY,
        size: 0,
    });
    entries.push(DirEntry {
        name: "..".to_owned(),
        attributes: FILE_ATTRIBUTE_DIRECTORY,
        size: 0,
    });

    if let Some(map) = guest_path_to_host(ctx.volumes, dir_path)
        && map.host.is_dir()
        && let Ok(rd) = fs::read_dir(&map.host)
    {
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            let meta = ent.metadata().ok();
            let is_dir = meta.as_ref().is_some_and(std::fs::Metadata::is_dir);
            let size = meta.as_ref().map_or(0, std::fs::Metadata::len);
            entries.push(DirEntry {
                name,
                attributes: if is_dir {
                    FILE_ATTRIBUTE_DIRECTORY
                } else {
                    FILE_ATTRIBUTE_ARCHIVE
                },
                size,
            });
        }
    }

    // Virtual files whose parent is this dir.
    let dir_norm = dir_path.trim_end_matches('\\');
    for (guest, size) in ctx.virtual_files {
        let parent = super::path::guest_parent(guest);
        if paths_equal_ci(&parent, dir_norm) || paths_equal_ci(&parent, dir_path) {
            entries.push(DirEntry {
                name: guest_basename(guest).to_owned(),
                attributes: FILE_ATTRIBUTE_ARCHIVE,
                size: u64::try_from(*size).unwrap_or(0),
            });
        }
    }

    // Mounts in this dir.
    for (guest, host) in ctx.host_file_mounts {
        let parent = super::path::guest_parent(guest);
        if paths_equal_ci(&parent, dir_norm) || paths_equal_ci(&parent, dir_path) {
            let size = fs::metadata(host).map_or(0, |m| m.len());
            entries.push(DirEntry {
                name: guest_basename(guest).to_owned(),
                attributes: FILE_ATTRIBUTE_ARCHIVE,
                size,
            });
        }
    }

    // Main module if under dir.
    if !ctx.main_module_path.is_empty() {
        let parent = super::path::guest_parent(ctx.main_module_path);
        if paths_equal_ci(&parent, dir_norm) || paths_equal_ci(&parent, dir_path) {
            let name = ctx.main_module_file_name.to_owned();
            if !entries.iter().any(|e| e.name.eq_ignore_ascii_case(&name)) {
                entries.push(DirEntry {
                    name,
                    attributes: FILE_ATTRIBUTE_ARCHIVE,
                    size: 0,
                });
            }
        }
    }

    // Child synthetic dirs one level below.
    let dir_lower = dir_norm.to_ascii_lowercase();
    for d in ctx.synthetic_dirs {
        let dl = d.trim_end_matches('\\').to_ascii_lowercase();
        if let Some(rest) = dl.strip_prefix(&dir_lower) {
            let rest = rest.trim_start_matches('\\');
            if rest.is_empty() {
                continue;
            }
            if !rest.contains('\\') {
                let name = guest_basename(d).to_owned();
                if !entries.iter().any(|e| e.name.eq_ignore_ascii_case(&name)) {
                    entries.push(DirEntry {
                        name,
                        attributes: FILE_ATTRIBUTE_DIRECTORY,
                        size: 0,
                    });
                }
            }
        }
    }

    entries
}

/// Filter list_dir by wildcard mask.
pub fn list_dir_filtered(ctx: &ResolveCtx<'_>, dir_path: &str, mask: &str) -> Vec<DirEntry> {
    let mask = if mask.is_empty() || mask == "*.*" {
        "*"
    } else {
        mask
    };
    list_dir(ctx, dir_path)
        .into_iter()
        .filter(|e| wildcard_match(mask, &e.name))
        .collect()
}

/// Read entire host/virtual file bytes (for small buffered opens).
pub fn read_all_host(path: &Path) -> std::io::Result<Vec<u8>> {
    fs::read(path)
}

/// Create parent dirs and empty file on host.
pub fn create_host_file(path: &Path) -> std::io::Result<()> {
    bump_fs_epoch();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    File::create(path)?;
    Ok(())
}

pub fn mkdir_host(path: &Path) -> std::io::Result<()> {
    bump_fs_epoch();
    fs::create_dir_all(path)
}

pub fn remove_file_host(path: &Path) -> std::io::Result<()> {
    bump_fs_epoch();
    fs::remove_file(path)
}

pub fn remove_dir_host(path: &Path) -> std::io::Result<()> {
    bump_fs_epoch();
    fs::remove_dir(path)
}

pub fn rename_host(from: &Path, to: &Path) -> std::io::Result<()> {
    bump_fs_epoch();
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(from, to)
}

pub fn copy_host(from: &Path, to: &Path) -> std::io::Result<u64> {
    bump_fs_epoch();
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(from, to)
}

/// Streamed read at offset from host path (one-shot open+seek+read+close).
///
/// Prefer [`cached_read_at`] on `Arc<Mutex<File>>` for hot loops — the one-shot
/// form pays an `open` + `close` per call.
pub fn host_read_at(path: &Path, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    f.read(buf)
}

/// Streamed write at offset (extends file as needed).
///
/// Prefer [`cached_write_at`] on `Arc<Mutex<File>>` for hot loops.
pub fn host_write_at(path: &Path, offset: u64, data: &[u8]) -> std::io::Result<()> {
    // Writes can extend the file, so the cached size/kind is now stale.
    bump_fs_epoch();
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    f.write_all(data)?;
    Ok(())
}

/// Open (or reuse a cached) `File` for read+write streaming on `path`.
///
/// Reuse is opportunistic: callers pool the returned `Arc<Mutex<File>>` in
/// `FileIoState::cached_streams`, keyed by the guest-visible file handle, and
/// drop it on `CloseHandle`. First call pays `File::open`; subsequent calls
/// are free besides a mutex acquisition and a `seek`.
pub fn open_stream_cached(path: &Path) -> std::io::Result<std::sync::Arc<std::sync::Mutex<File>>> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    Ok(std::sync::Arc::new(std::sync::Mutex::new(f)))
}

/// Read at `offset` from a cached streaming file. Returns bytes read.
pub fn cached_read_at(
    file: &std::sync::Arc<std::sync::Mutex<File>>,
    offset: u64,
    buf: &mut [u8],
) -> std::io::Result<usize> {
    let mut guard = file
        .lock()
        .map_err(|_| std::io::Error::other("cached stream mutex poisoned"))?;
    guard.seek(SeekFrom::Start(offset))?;
    guard.read(buf)
}

/// Write `data` at `offset` to a cached streaming file.
pub fn cached_write_at(
    file: &std::sync::Arc<std::sync::Mutex<File>>,
    offset: u64,
    data: &[u8],
) -> std::io::Result<()> {
    // May extend the file — invalidate cached stats.
    bump_fs_epoch();
    let mut guard = file
        .lock()
        .map_err(|_| std::io::Error::other("cached stream mutex poisoned"))?;
    guard.seek(SeekFrom::Start(offset))?;
    guard.write_all(data)
}

/// File length via the shared stat cache (avoids a bare `metadata` syscall).
pub fn host_file_len(path: &Path) -> std::io::Result<u64> {
    let stat = stat_host_path(path);
    if stat.kind == PathKind::NotFound {
        return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
    }
    Ok(stat.size)
}

pub fn host_set_len(path: &Path, len: u64) -> std::io::Result<()> {
    bump_fs_epoch();
    let f = OpenOptions::new().write(true).open(path)?;
    f.set_len(len)
}

/// Default synthetic directory list for Win10 skeleton probes.
pub const DEFAULT_SYNTHETIC_DIRS: &[&str] = &[
    r"C:\",
    r"C:\App",
    r"C:\Windows",
    r"C:\Windows\System32",
    r"C:\Windows\SysWOW64",
    r"C:\Users",
    r"C:\Users\WIE",
    r"C:\Users\WIE\AppData",
    r"C:\Users\WIE\AppData\Local",
    r"C:\Users\WIE\AppData\Local\Temp",
    r"C:\Temp",
    r"C:\ProgramData",
];
