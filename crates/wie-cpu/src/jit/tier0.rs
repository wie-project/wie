//! Executable memory for hand-emitted host code (the tier-0 emitter's substrate).
//!
//! Everything else in this engine gets its code from `cranelift_jit::JITModule`,
//! which owns an `mmap`ed `MAP_JIT` arena internally and exposes no way to hand
//! it bytes. A single-pass emitter produces bytes, so it needs its own arena and
//! this is it — deliberately the smallest thing that can be *executed and
//! asserted on*, so every later encoding is validated by running it rather than
//! by comparing it to what I wrote.
//!
//! # Why two phases (`RW` then `RX`), and not `MAP_JIT`
//!
//! Apple Silicon enforces W^X, so bytes are never mapped writable *and*
//! executable: the buffer is created `PROT_READ | PROT_WRITE`, filled, and
//! `mprotect`ed to `PROT_READ | PROT_EXEC` in [`CodeBuffer::seal`]. That is what
//! `cranelift-jit` itself does (`memmap2::MmapMut::map_anon` then a `mprotect`,
//! no `MAP_JIT`), and matching it is deliberate rather than incidental.
//!
//! **The tempting variant does not work here and fails badly.** Asking `mmap`
//! for `PROT_READ | PROT_WRITE | PROT_EXEC` with `MAP_JIT` does *not* return
//! `MAP_FAILED` in this process — the allocation succeeds and the first call
//! faults with **SIGBUS**, i.e. a crash at the call site rather than an error at
//! the allocation. Adding the `pthread_jit_write_protect_np` toggle around the
//! stores and an `mprotect` in `seal` did not rescue it either. Both were tried
//! before this shape; the smoke tests below are what distinguishes them, which is
//! exactly why they execute instead of comparing bytes.
//!
//! Instruction-cache coherency is separate again: Darwin gives no architectural
//! guarantee that freshly written bytes are visible to the fetch unit without
//! `sys_icache_invalidate`, which is why `seal` exists and why the smoke test
//! would fail intermittently without it rather than never.

#![allow(
    unsafe_code, // mmap/mprotect/munmap are FFI by definition
    dead_code,   // exercised by the smoke tests; consumed by the emitter increment
    reason = "raw executable memory, and an API whose only caller is not written yet"
)]

use std::ffi::c_void;
use std::ptr;

unsafe extern "C" {
    /// `void sys_icache_invalidate(void *start, size_t len)` — `libkern/OSCacheControl.h`.
    fn sys_icache_invalidate(start: *mut c_void, len: libc::size_t);
}

/// A page of executable memory the emitter fills and then calls.
///
/// One allocation per emitter arena, not per block: blocks are appended at a
/// bump offset, and [`CodeBuffer::seal`] is called once before any of them run.
/// The `Drop` impl unmaps, so a test or a failed compile leaks nothing.
pub(super) struct CodeBuffer {
    base: *mut u8,
    len: usize,
    used: usize,
    /// Set once `seal` has invalidated the icache; a later `write` is a bug
    /// (the fetch unit may already have cached those bytes), so it asserts.
    sealed: bool,
}

impl CodeBuffer {
    /// Allocate `len` bytes of `MAP_JIT` memory (rounded up to a page by the
    /// kernel). `None` if `mmap` fails, which the caller must treat as "fall
    /// back to Cranelift", never as "carry on without code".
    pub(super) fn new(len: usize) -> Option<Self> {
        // SAFETY: standard MAP_ANON mapping; the pointer is checked for MAP_FAILED
        // below and owned exclusively by the returned struct.
        let base = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return None;
        }
        Some(Self {
            base: base.cast::<u8>(),
            len,
            used: 0,
            sealed: false,
        })
    }

    /// Bytes still unused.
    pub(super) fn remaining(&self) -> usize {
        self.len.saturating_sub(self.used)
    }

    /// Append `bytes` at the current offset and return the offset, or `None` if
    /// they do not fit (the caller picks a bigger buffer; it must not truncate).
    pub(super) fn write(&mut self, bytes: &[u8]) -> Option<usize> {
        assert!(!self.sealed, "CodeBuffer::write after seal");
        if bytes.len() > self.remaining() {
            return None;
        }
        let at = self.used;
        // SAFETY: writes stay inside the mapping (checked above), which is
        // writable by construction until `seal` tightens it to RX.
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), self.base.add(at), bytes.len());
        }
        self.used += bytes.len();
        Some(at)
    }

    /// Make everything written so far executable: flush the icache and refuse
    /// further writes.
    pub(super) fn seal(&mut self) {
        if self.sealed || self.used == 0 {
            self.sealed = true;
            return;
        }
        // Two-phase, the way `JITMemoryProvider` does it: fill while writable,
        // then *make* the range executable. Asking `mmap` for `PROT_EXEC` up
        // front is not equivalent — under `MAP_JIT` the kernel may hand back a
        // mapping whose fetch permission is not yet in effect, which faults as
        // SIGBUS on the first call rather than failing the allocation.
        // SAFETY: `base` is a live mapping of at least `used` bytes; the length
        // is rounded up to the page the kernel owns for that address.
        unsafe {
            sys_icache_invalidate(self.base.cast(), self.used);
            let page = libc::sysconf(libc::_SC_PAGESIZE).max(1) as usize;
            let len = self.used.div_ceil(page) * page;
            libc::mprotect(self.base.cast(), len, libc::PROT_READ | libc::PROT_EXEC);
        }
        self.sealed = true;
    }

    /// The entry point at `offset`, as the emitter's calling convention.
    ///
    /// # Safety
    ///
    /// The caller asserts that the bytes at `offset` implement `extern "C" fn()`
    /// for the chosen signature complete with a `ret`, that `seal` has run, and
    /// that `offset` was returned by [`Self::write`] for this buffer. Everything
    /// about a hand-written emitter's correctness rests on those three being
    /// true, and none of them can be checked here.
    pub(super) unsafe fn entry<F>(&self, offset: usize) -> F
    where
        F: Copy,
    {
        assert!(self.sealed, "CodeBuffer::entry before seal");
        assert!(
            offset < self.used,
            "CodeBuffer::entry outside written range"
        );
        debug_assert_eq!(
            std::mem::size_of::<F>(),
            std::mem::size_of::<*const u8>(),
            "entry point must be a plain function pointer"
        );
        // SAFETY: the caller's contract, above.
        unsafe { std::mem::transmute_copy(&self.base.add(offset)) }
    }
}

impl Drop for CodeBuffer {
    fn drop(&mut self) {
        // SAFETY: `base`/`len` are exactly what `mmap` returned and was asked for.
        unsafe {
            libc::munmap(self.base.cast(), self.len);
        }
    }
}

#[cfg(test)]
mod tests {
    //! The platform brick, proved by **executing** emitted bytes.
    //!
    //! This is the whole point of the module: until something hand-emitted runs,
    //! an encoder can only be tested against itself, and a self-referential test
    //! is exactly the failure this campaign has already been bitten by once (a
    //! classifier that agreed with its own diagnostic while both were wrong).
    //! Everything here therefore ends in a call and an assertion on the returned
    //! value.

    use super::*;

    /// `movz w0, #imm16` — `mov w0, #N` for N < 65536.
    fn movz_w0(imm: u16) -> [u8; 4] {
        let insn = 0x5280_0000_u32 | (u32::from(imm) << 5);
        insn.to_le_bytes()
    }

    /// `ret` (`br x30`).
    const RET: [u8; 4] = 0xD65F_03C0_u32.to_le_bytes();

    #[test]
    fn emitted_bytes_run_and_return_their_value() {
        // The signature is `u32`, so the value survives in `w0`; a stale or
        // unwritten w0 cannot accidentally equal 42 on a fresh thread, which is
        // why the constant is not 0.
        let mut buf = CodeBuffer::new(4096).expect("mmap failed");
        let at = buf.write(&movz_w0(42)).expect("fits");
        assert_eq!(at, 0);
        buf.write(&RET).expect("fits");
        buf.seal();
        // SAFETY: two instructions written above, sealed, ending in `ret`, and
        // the signature matches `movz w0` + `ret` exactly.
        let f: extern "C" fn() -> u32 = unsafe { buf.entry(0) };
        assert_eq!(f(), 42, "hand-emitted code must run");
    }

    #[test]
    fn a_second_block_at_a_nonzero_offset_runs_too() {
        // Bump-allocation, which is how the emitter will lay blocks out: the
        // entry point is an offset, not the base. A buffer that only worked at
        // offset 0 would pass the test above and fail in the emitter.
        let mut buf = CodeBuffer::new(4096).expect("mmap failed");
        let first = buf.write(&movz_w0(7)).expect("fits");
        buf.write(&RET).expect("fits");
        let second = buf.write(&movz_w0(11)).expect("fits");
        buf.write(&RET).expect("fits");
        buf.seal();
        assert_eq!(first, 0);
        assert!(second > first);
        // SAFETY: as above, for each entry point.
        let a: extern "C" fn() -> u32 = unsafe { buf.entry(first) };
        let b: extern "C" fn() -> u32 = unsafe { buf.entry(second) };
        assert_eq!((a(), b()), (7, 11), "each offset is its own entry point");
    }

    #[test]
    fn a_full_buffer_refuses_rather_than_truncating() {
        // The emitter decides "this block does not fit" and falls back; a silent
        // partial write would be a truncated block, i.e. wrong code.
        let mut buf = CodeBuffer::new(4096).expect("mmap failed");
        let big = vec![0_u8; buf.remaining() + 1];
        assert!(buf.write(&big).is_none());
        assert_eq!(buf.remaining(), 4096, "a refused write must not advance");
    }

    #[test]
    #[should_panic(expected = "CodeBuffer::write after seal")]
    fn writing_after_seal_is_a_bug_not_a_silent_overwrite() {
        // `should_panic` rather than a caught unwind: a caught panic leaves the
        // buffer alive across it, and nextest reports the test as leaking.
        let mut buf = CodeBuffer::new(4096).expect("mmap failed");
        buf.write(&movz_w0(1)).expect("fits");
        buf.seal();
        buf.write(&RET).expect("fits");
    }
}
