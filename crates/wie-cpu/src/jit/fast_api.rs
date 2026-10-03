//! Direct host implementations of hot UCRT imports for the Cranelift JIT.
//!
//! When a guest `call` resolves to one of these fake-API VAs, the lowerer emits a
//! Cranelift `call` to a host helper instead of exiting to the runtime host-stop
//! loop (saves most of the CRT startup / `printf` path stops).

use super::lower::JitCtx;
use super::trampolines::ctx_mut;
use crate::guest_layout::{
    CRT_FILE_STDERR as FILE_STDERR, CRT_FILE_STDIN as FILE_STDIN, CRT_FILE_STDOUT as FILE_STDOUT,
    HEAP_BLOCK_HEADER_SIZE, HEAP_CTRL_BUMP_OFFSET, HEAP_CTRL_HEAD_BASE, HEAP_CTRL_HEAD_STRIDE,
    HEAP_PAYLOAD_ALIGN, LARGE_THRESHOLD, SIZE_CLASSES,
};
use crate::mem::GuestMemory;

/// Which UCRT/CRT import to accelerate from JIT code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FastApiKind {
    Malloc,
    Free,
    Memcpy,
    Strlen,
    AcrtIobFunc,
    Fwrite,
    Fflush,
}

impl FastApiKind {
    /// Map an export name (case-insensitive) to a fast-path kind.
    #[must_use]
    pub fn from_export_name(name: &str) -> Option<Self> {
        let n = name.as_bytes();
        // Fast path without allocation: common CRT names are ASCII.
        let eq = |a: &str| name.eq_ignore_ascii_case(a);
        if eq("malloc") {
            Some(Self::Malloc)
        } else if eq("free") {
            Some(Self::Free)
        } else if eq("memcpy") {
            Some(Self::Memcpy)
        } else if eq("strlen") {
            Some(Self::Strlen)
        } else if eq("__acrt_iob_func") {
            Some(Self::AcrtIobFunc)
        } else if eq("fwrite") {
            Some(Self::Fwrite)
        } else if eq("fflush") {
            Some(Self::Fflush)
        } else {
            let _ = n;
            None
        }
    }

    /// Cranelift import symbol name.
    #[must_use]
    pub const fn symbol(self) -> &'static str {
        match self {
            Self::Malloc => "wie_ucrt_malloc",
            Self::Free => "wie_ucrt_free",
            Self::Memcpy => "wie_ucrt_memcpy",
            Self::Strlen => "wie_ucrt_strlen",
            Self::AcrtIobFunc => "wie_ucrt_iob",
            Self::Fwrite => "wie_ucrt_fwrite",
            Self::Fflush => "wie_ucrt_fflush",
        }
    }
}

/// Guest heap layout for JIT `malloc`/`free` (matches guest HeapAlloc control block).
#[derive(Debug, Clone, Copy, Default)]
pub struct JitHeapLayout {
    pub ctrl_va: u64,
    pub base: u64,
    pub end: u64,
}

/// Configuration installed by the runtime after fake-API table build.
#[derive(Debug, Clone, Default)]
pub struct JitFastPathConfig {
    pub heap: JitHeapLayout,
    /// Dense (VA, kind) pairs for UCRT fast paths — typically ≤ 8 entries.
    /// Lookup is linear; cheaper than HashMap for this size and avoids stop tax.
    pub pairs: Vec<(u64, FastApiKind)>,
}

// FILE* cookies (`FILE_STDIN/OUT/ERR`) and size-class ladder
// (`SIZE_CLASSES`/`LARGE_THRESHOLD`) are re-exported from
// [`crate::guest_layout`], the shared home kept in lockstep with
// `wie_winapi::ucrt` and `wie_winapi::guest_heap`.
//
// The guest heap *layout* and the large free list deliberately do NOT live in
// process globals, unlike those constants: they are mutable per-session state,
// and the consumers are per-guest-thread Cranelift import symbols. A `static`
// made every engine in the process — including a second `RuntimeSession`'s —
// read the last-installed layout and share one free list. They now live on
// [`JitCtx`](super::lower::JitCtx) and
// [`PerThreadJitState`](super::PerThreadJitState) respectively; see
// [`JitCpu::configure_fast_path`](super::JitCpu::configure_fast_path) for the
// publish.

/// Host-side large free list for the UCRT `malloc`/`free` fast path when size
/// > [`LARGE_THRESHOLD`], as `(payload_va, size)` pairs.
///
/// Guest control block only stores size-class heads; large blocks need a host
/// list (same role as `GuestHeap::large_free` on the WinAPI path). One list per
/// ENGINE, owned by [`PerThreadJitState`](super::PerThreadJitState) and reached
/// from a running frame through [`JitCtx::large_free`] — so it outlives any one
/// frame, needs no `Mutex`, and cannot be wiped by another session's init.
pub(super) type LargeFreeList = Vec<(u64, u64)>;

/// Heap layout for this engine's session, read from the running `JitCtx`.
/// Strictly cheaper than the process statics it replaces: three plain loads
/// from a struct already on the native stack, versus three `Relaxed` atomic
/// loads of cache lines shared by every guest thread.
#[inline]
fn heap_layout(ctx: &JitCtx) -> JitHeapLayout {
    JitHeapLayout {
        ctrl_va: ctx.heap_ctrl_va,
        base: ctx.heap_base,
        end: ctx.heap_end,
    }
}

/// Borrow this engine's large free list for the duration of one native frame.
///
/// Takes the POINTER, not the context, and returns an unconstrained lifetime.
/// That is deliberate: the caller already holds a `&mut GuestMemory` reborrowed
/// from `ctx` via [`mem_mut`], and the two pointees are disjoint fields
/// (`GuestMemory` vs `LargeFreeList`). Handing the pointer across keeps the
/// borrow checker from having to see through the `*mut` indirection, so both
/// can be live at once without a false alias — and it keeps the `unsafe` claim
/// in one place, exactly as [`mem_mut`] does for `ctx.mem`.
///
/// The invariant this relies on: the pointer is always
/// [`JitCtx::large_free`], set by `run_compiled` from `&mut self.thread`,
/// which is not touched again until the frame returns. Both `wie_ucrt_malloc`
/// and `wie_ucrt_free` read it out of their own context and pass it straight
/// here, so no other value can reach this function.
#[inline]
fn large_free_mut<'a>(ptr: *mut LargeFreeList) -> &'a mut LargeFreeList {
    // SAFETY: `ptr` is the running frame's own context field, installed by
    // `run_compiled` from `&mut self.thread` and live and unaliased for the
    // whole native frame — the same one claim `ctx_mut` makes for the context
    // itself. The pointee is disjoint from the `ctx.mem` the caller holds.
    unsafe { &mut *ptr }
}

fn mem_mut(ctx: &mut JitCtx) -> &mut GuestMemory {
    // SAFETY: `mem` is set by `run_compiled` for the duration of the block.
    unsafe { &mut *ctx.mem }
}

fn read_u64(mem: &GuestMemory, va: u64) -> Option<u64> {
    let mut b = [0_u8; 8];
    mem.read(va, &mut b).ok()?;
    Some(u64::from_le_bytes(b))
}

fn write_u64(mem: &mut GuestMemory, va: u64, value: u64) -> bool {
    mem.write(va, &value.to_le_bytes()).is_ok()
}

fn round_up_size(size: u64) -> u64 {
    let size = size.max(1);
    if size <= LARGE_THRESHOLD {
        for &c in &SIZE_CLASSES {
            if size <= c {
                return c;
            }
        }
        // size is in (last class, LARGE_THRESHOLD] — should not happen with classes
        // ending at LARGE_THRESHOLD; keep exact payload align as a safe fallback.
        align_up_payload(size)
    } else {
        align_up_payload(size)
    }
}

/// Align `size` up to [`HEAP_PAYLOAD_ALIGN`] (the JIT bump/header layout).
fn align_up_payload(size: u64) -> u64 {
    size.wrapping_add(HEAP_PAYLOAD_ALIGN - 1) & !(HEAP_PAYLOAD_ALIGN - 1)
}

fn size_class_index(size: u64) -> usize {
    for (i, &c) in SIZE_CLASSES.iter().enumerate() {
        if size <= c {
            return i;
        }
    }
    SIZE_CLASSES.len() - 1
}

fn head_va(ctrl: u64, class: usize) -> u64 {
    ctrl.wrapping_add(HEAP_CTRL_HEAD_BASE).wrapping_add(
        u64::try_from(class)
            .unwrap_or(0)
            .wrapping_mul(HEAP_CTRL_HEAD_STRIDE),
    )
}

fn find_large_fit(list: &[(u64, u64)], need: u64) -> Option<usize> {
    let mut best: Option<(usize, u64)> = None;
    for (i, &(_, size)) in list.iter().enumerate() {
        if size >= need {
            match best {
                None => best = Some((i, size)),
                Some((_, best_size)) if size < best_size => best = Some((i, size)),
                _ => {}
            }
        }
    }
    best.map(|(i, _)| i)
}

/// Large allocation: best-fit on this engine's free list, else bump (mirrors
/// `GuestHeap`).
fn large_alloc(
    mem: &mut GuestMemory,
    list: &mut LargeFreeList,
    heap: &JitHeapLayout,
    rounded: u64,
) -> u64 {
    if let Some(best_i) = find_large_fit(list, rounded) {
        let (addr, size) = list.swap_remove(best_i);
        if size >= rounded.saturating_add(LARGE_THRESHOLD) {
            let residual_addr = addr.saturating_add(rounded);
            let residual_size = size.saturating_sub(rounded);
            if residual_addr > addr && residual_size >= 16 {
                list.push((residual_addr, residual_size));
            }
        }
        let _ = write_u64(mem, addr.wrapping_sub(HEAP_BLOCK_HEADER_SIZE), rounded);
        return addr;
    }
    bump_alloc(mem, heap, rounded)
}

// SAFETY: the `wie_ucrt_*` helpers below are Cranelift import symbols, entered
// only from `JitCpu::run_compiled`, which owns the `JitCtx` for the whole native
// frame. The `unsafe { ctx_mut(ctx) }` in each body is that one claim, discharged
// by `super::trampolines::ctx_mut` — the single place in the JIT that converts a
// `*mut JitCtx` into a reference. `mem_mut` below derefs a *different* pointer
// (`ctx.mem`) and keeps its own SAFETY comment.

/// `malloc(size)` — freelist / bump via guest heap control block.
pub(super) unsafe extern "C" fn wie_ucrt_malloc(ctx: *mut JitCtx, size: u64) -> u64 {
    let ctx = unsafe { ctx_mut(ctx) };
    if ctx.fault != 0 {
        return 0;
    }
    if size == 0 {
        return 0;
    }
    // Read the layout BEFORE `mem_mut` reborrows the context mutably.
    let heap = heap_layout(ctx);
    if heap.ctrl_va == 0 || heap.base == 0 || heap.end <= heap.base {
        return 0;
    }
    let large_free = ctx.large_free;
    let mem = mem_mut(ctx);
    let rounded = round_up_size(size);
    if rounded == 0 {
        return 0;
    }
    if rounded > LARGE_THRESHOLD {
        return large_alloc(mem, large_free_mut(large_free), &heap, rounded);
    }
    let class = size_class_index(rounded);
    let hva = head_va(heap.ctrl_va, class);
    if let Some(head) = read_u64(mem, hva)
        && head != 0
        && head >= heap.base
        && head < heap.end
    {
        let next = read_u64(mem, head).unwrap_or(0);
        if !write_u64(mem, hva, next) {
            return 0;
        }
        let _ = write_u64(mem, head.wrapping_sub(HEAP_BLOCK_HEADER_SIZE), rounded);
        return head;
    }
    bump_alloc(mem, &heap, rounded)
}

fn bump_alloc(mem: &mut GuestMemory, heap: &JitHeapLayout, rounded: u64) -> u64 {
    let Some(mut bump) = read_u64(mem, heap.ctrl_va.wrapping_add(HEAP_CTRL_BUMP_OFFSET)) else {
        return 0;
    };
    if bump < heap.base {
        bump = heap.base;
    }
    let pre = bump.wrapping_add(HEAP_BLOCK_HEADER_SIZE);
    let payload = pre.wrapping_add(HEAP_PAYLOAD_ALIGN - 1) & !(HEAP_PAYLOAD_ALIGN - 1);
    let end = payload.wrapping_add(rounded);
    if payload < heap.base || end > heap.end || end < payload {
        return 0;
    }
    if !write_u64(mem, heap.ctrl_va.wrapping_add(HEAP_CTRL_BUMP_OFFSET), end) {
        return 0;
    }
    let _ = write_u64(mem, payload.wrapping_sub(HEAP_BLOCK_HEADER_SIZE), rounded);
    payload
}

/// `free(ptr)`.
pub(super) unsafe extern "C" fn wie_ucrt_free(ctx: *mut JitCtx, ptr: u64) {
    let ctx = unsafe { ctx_mut(ctx) };
    if ctx.fault != 0 || ptr == 0 {
        return;
    }
    // Read the layout BEFORE `mem_mut` reborrows the context mutably.
    let heap = heap_layout(ctx);
    if heap.ctrl_va == 0 {
        return;
    }
    if ptr < heap.base || ptr >= heap.end {
        return;
    }
    let large_free = ctx.large_free;
    let mem = mem_mut(ctx);
    let Some(size) = read_u64(mem, ptr.wrapping_sub(HEAP_BLOCK_HEADER_SIZE)) else {
        return;
    };
    if size == 0 {
        return;
    }
    // Poison header so double-free is a no-op (matches host free_coherent).
    let _ = write_u64(mem, ptr.wrapping_sub(HEAP_BLOCK_HEADER_SIZE), 0);
    if size > LARGE_THRESHOLD {
        large_free_mut(large_free).push((ptr, size));
        return;
    }
    let class = size_class_index(size);
    let hva = head_va(heap.ctrl_va, class);
    let old = read_u64(mem, hva).unwrap_or(0);
    let _ = write_u64(mem, ptr, old);
    let _ = write_u64(mem, hva, ptr);
}

/// `memcpy(dest, src, n)` → dest.
pub(super) unsafe extern "C" fn wie_ucrt_memcpy(
    ctx: *mut JitCtx,
    dest: u64,
    src: u64,
    n: u64,
) -> u64 {
    let ctx = unsafe { ctx_mut(ctx) };
    if ctx.fault != 0 {
        return dest;
    }
    let n_usize = usize::try_from(n).unwrap_or(0);
    if n_usize == 0 || dest == 0 || src == 0 {
        return dest;
    }
    let mem = mem_mut(ctx);
    // Chunk to bound stack; prefer page-sized buffers.
    let mut remaining = n_usize;
    let mut d = dest;
    let mut s = src;
    let mut buf = [0_u8; 4096];
    while remaining > 0 {
        let chunk = remaining.min(buf.len());
        if mem.read(s, &mut buf[..chunk]).is_err() {
            ctx.fault = 1;
            ctx.fault_addr = s;
            ctx.fault_size = u64::try_from(chunk).unwrap_or(0);
            ctx.fault_access = 0;
            return dest;
        }
        if mem.write(d, &buf[..chunk]).is_err() {
            ctx.fault = 1;
            ctx.fault_addr = d;
            ctx.fault_size = u64::try_from(chunk).unwrap_or(0);
            ctx.fault_access = 1;
            return dest;
        }
        d = d.wrapping_add(u64::try_from(chunk).unwrap_or(0));
        s = s.wrapping_add(u64::try_from(chunk).unwrap_or(0));
        remaining -= chunk;
    }
    dest
}

/// `strlen(s)`.
pub(super) unsafe extern "C" fn wie_ucrt_strlen(ctx: *mut JitCtx, s: u64) -> u64 {
    let ctx = unsafe { ctx_mut(ctx) };
    if ctx.fault != 0 || s == 0 {
        return 0;
    }
    let mem = mem_mut(ctx);
    let mut len = 0_u64;
    loop {
        let mut b = [0_u8; 1];
        if mem.read(s.wrapping_add(len), &mut b).is_err() {
            ctx.fault = 1;
            ctx.fault_addr = s.wrapping_add(len);
            ctx.fault_size = 1;
            ctx.fault_access = 0;
            return len;
        }
        if b[0] == 0 {
            return len;
        }
        len = len.saturating_add(1);
        if len > 1_000_000 {
            return len;
        }
    }
}

/// `__acrt_iob_func(ix)` → FILE*.
pub(super) extern "C" fn wie_ucrt_iob(ix: u64) -> u64 {
    match ix & 0xffff_ffff {
        0 => FILE_STDIN,
        1 => FILE_STDOUT,
        2 => FILE_STDERR,
        _ => 0,
    }
}

/// `fwrite(buf, size, count, stream)` → count written (or 0).
///
/// Security: validates `[buf, buf+size*count)` is within readable guest memory
/// before writing any output to the host console.  Caps output at 64 KiB per
/// call to limit information disclosure.
pub(super) unsafe extern "C" fn wie_ucrt_fwrite(
    ctx: *mut JitCtx,
    buf: u64,
    size: u64,
    count: u64,
    stream: u64,
) -> u64 {
    const MAX_FWRITE_OUTPUT: usize = 64 * 1024;
    let ctx = unsafe { ctx_mut(ctx) };
    if ctx.fault != 0 {
        return 0;
    }
    if size == 0 || count == 0 {
        return 0;
    }
    let total = size.saturating_mul(count);
    let total_usize = usize::try_from(total).unwrap_or(0);
    if total_usize == 0 {
        return 0;
    }
    // Only console streams write to the host; other FILE* cookies are no-ops.
    if stream != FILE_STDOUT && stream != FILE_STDERR {
        return count;
    }
    if buf == 0 {
        return count;
    }

    // Security: validate the entire range [buf, buf+total) is readable via
    // a probe read on the first byte before entering the output loop.
    // mem.read() enforces SPC (Software Permission Check), so unmapped or
    // non-readable addresses produce an error and no bytes reach the host.
    let mem = mem_mut(ctx);
    let mut probe = [0_u8; 1];
    if mem.read(buf, &mut probe).is_err() {
        return count; // Silently skip: same as /dev/null.
    }

    let capped = total_usize.min(MAX_FWRITE_OUTPUT);

    // Chunked host write — avoid a single heap allocation the size of the whole
    // transfer (large `fwrite` was a host-RSS spike on the JIT path).
    let mut remaining = capped;
    let mut src = buf;
    let mut chunk_buf = [0_u8; 4096];
    while remaining > 0 {
        let chunk = remaining.min(chunk_buf.len());
        if mem.read(src, &mut chunk_buf[..chunk]).is_err() {
            ctx.fault = 1;
            ctx.fault_addr = src;
            ctx.fault_size = u64::try_from(chunk).unwrap_or(0);
            ctx.fault_access = 0;
            return 0;
        }
        write_host_console(stream, &chunk_buf[..chunk]);
        src = src.wrapping_add(u64::try_from(chunk).unwrap_or(0));
        remaining -= chunk;
    }
    count
}

/// `fflush(stream)` → 0.
///
/// Console I/O uses unbuffered `libc::write`, so there is no userspace buffer to
/// flush and no Rust `stdout` mutex to take.
pub(super) extern "C" fn wie_ucrt_fflush(_stream: u64) -> u64 {
    0
}

/// Host console write without `std::io::{stdout,stderr}` lock (hot JIT path).
#[cfg(unix)]
fn write_host_console(stream: u64, bytes: &[u8]) {
    let fd = if stream == FILE_STDOUT {
        libc::STDOUT_FILENO
    } else {
        // FILE_STDERR (caller already filtered).
        libc::STDERR_FILENO
    };
    write_all_fd(fd, bytes);
}

/// Write the full buffer to `fd`, retrying EINTR; give up on other errors.
#[cfg(unix)]
fn write_all_fd(fd: libc::c_int, bytes: &[u8]) {
    let mut offset = 0_usize;
    while offset < bytes.len() {
        let Some(chunk) = bytes.get(offset..) else {
            break;
        };
        // SAFETY: `fd` is host stdout/stderr; `chunk` is a live contiguous buffer;
        // write does not retain the pointer. Skips `std::io` mutex on every fwrite.
        let n = unsafe { libc::write(fd, chunk.as_ptr().cast::<libc::c_void>(), chunk.len()) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if n == 0 {
            break;
        }
        offset = offset.saturating_add(usize::try_from(n).unwrap_or(0));
    }
}

#[cfg(not(unix))]
fn write_host_console(stream: u64, bytes: &[u8]) {
    use std::io::Write;
    if stream == FILE_STDOUT {
        drop(std::io::stdout().write_all(bytes));
    } else if stream == FILE_STDERR {
        drop(std::io::stderr().write_all(bytes));
    }
}

/// FILE* constants for IR inlining of `__acrt_iob_func`.
#[inline]
pub(super) const fn file_cookie(ix: u64) -> u64 {
    match ix {
        0 => FILE_STDIN,
        1 => FILE_STDOUT,
        2 => FILE_STDERR,
        _ => 0,
    }
}
