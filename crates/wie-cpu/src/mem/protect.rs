//! Windows `PAGE_*` protection constants and software access checks (Phase 3).
//!
//! Guest correctness uses these constants at 4 KiB granularity. Host `mprotect`
//! is optional defense-in-depth and must never be the sole permission oracle
//! under the guest-4K / host-16K clinch on Apple Silicon.
//!
//! Values match Microsoft Learn memory-protection constants.

/// No access (committed or reserved placeholder).
pub const PAGE_NOACCESS: u32 = 0x01;
/// Read-only.
pub const PAGE_READONLY: u32 = 0x02;
/// Read + write.
pub const PAGE_READWRITE: u32 = 0x04;
/// Execute only (no data read/write).
pub const PAGE_EXECUTE: u32 = 0x10;
/// Execute + read.
pub const PAGE_EXECUTE_READ: u32 = 0x20;
/// Execute + read + write.
pub const PAGE_EXECUTE_READWRITE: u32 = 0x40;

/// Kind of guest memory access for software permission checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccessKind {
    /// Data load / `mem_read`.
    Read,
    /// Data store / `mem_write`.
    Write,
    /// Instruction fetch (interpreter + JIT decode source).
    Execute,
}

/// A Windows page protection, as a closed set rather than a raw `u32`.
///
/// # Why this is a distinct type
///
/// WIE carries **two** permission encodings, and as raw `u32` they collide on
/// every value while meaning opposite things:
///
/// | value | [`crate::perm`] rwx bits | Windows `PAGE_*` |
/// |-------|--------------------------|------------------|
/// | `1`   | `READ`                   | `PAGE_NOACCESS`  |
/// | `2`   | `WRITE`                  | `PAGE_READONLY`  |
/// | `4`   | `EXEC`                   | `PAGE_READWRITE` |
/// | `7`   | `ALL` (rwx)              | *invalid*        |
///
/// Passing rwx bits where a `PAGE_*` was expected used to compile silently and
/// either deny everything (`READ` reads as `PAGE_NOACCESS`) or *widen*
/// permissions (`EXEC` reads as `PAGE_READWRITE`, making an execute-only page
/// writable). Keeping the Windows side in this enum and the rwx side in
/// [`crate::RwxPerms`] makes that mix-up a type error.
///
/// The `u32` form is the guest ABI (`VirtualAlloc`/`VirtualProtect` arguments,
/// `VirtualQuery` results, PE section characteristics) and appears only in
/// [`Self::to_win32`] / [`Self::from_win32`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PageProtect {
    NoAccess,
    ReadOnly,
    ReadWrite,
    Execute,
    ExecuteRead,
    ExecuteReadWrite,
}

impl PageProtect {
    /// Windows `PAGE_*` value for the guest ABI.
    #[must_use]
    pub fn to_win32(self) -> u32 {
        match self {
            Self::NoAccess => PAGE_NOACCESS,
            Self::ReadOnly => PAGE_READONLY,
            Self::ReadWrite => PAGE_READWRITE,
            Self::Execute => PAGE_EXECUTE,
            Self::ExecuteRead => PAGE_EXECUTE_READ,
            Self::ExecuteReadWrite => PAGE_EXECUTE_READWRITE,
        }
    }

    /// Parse a guest-supplied `PAGE_*` value. `None` for unsupported values,
    /// which callers surface as `ERROR_INVALID_PARAMETER`.
    #[must_use]
    pub fn from_win32(value: u32) -> Option<Self> {
        match value {
            PAGE_NOACCESS => Some(Self::NoAccess),
            PAGE_READONLY => Some(Self::ReadOnly),
            PAGE_READWRITE => Some(Self::ReadWrite),
            PAGE_EXECUTE => Some(Self::Execute),
            PAGE_EXECUTE_READ => Some(Self::ExecuteRead),
            PAGE_EXECUTE_READWRITE => Some(Self::ExecuteReadWrite),
            _ => None,
        }
    }

    /// Whether a data read is permitted.
    #[must_use]
    pub fn allows_read(self) -> bool {
        matches!(
            self,
            Self::ReadOnly | Self::ReadWrite | Self::ExecuteRead | Self::ExecuteReadWrite
        )
    }

    /// Whether a data write is permitted.
    #[must_use]
    pub fn allows_write(self) -> bool {
        matches!(self, Self::ReadWrite | Self::ExecuteReadWrite)
    }

    /// Whether instruction fetch is permitted.
    #[must_use]
    pub fn allows_execute(self) -> bool {
        matches!(
            self,
            Self::Execute | Self::ExecuteRead | Self::ExecuteReadWrite
        )
    }

    /// Whether `kind` is permitted.
    #[must_use]
    pub fn allows(self, kind: AccessKind) -> bool {
        match kind {
            AccessKind::Read => self.allows_read(),
            AccessKind::Write => self.allows_write(),
            AccessKind::Execute => self.allows_execute(),
        }
    }

    /// Nearest Windows protection for a set of rwx bits.
    ///
    /// Windows has no write-only or write+execute-without-read protection, so
    /// those widen to the read-inclusive equivalent.
    #[must_use]
    pub fn from_rwx(rwx: crate::RwxPerms) -> Self {
        match (rwx.read(), rwx.write(), rwx.exec()) {
            (_, true, true) => Self::ExecuteReadWrite,
            (true, false, true) => Self::ExecuteRead,
            (_, true, false) => Self::ReadWrite,
            (true, false, false) => Self::ReadOnly,
            (false, false, true) => Self::Execute,
            (false, false, false) => Self::NoAccess,
        }
    }

    /// The rwx bits this protection grants.
    #[must_use]
    pub fn to_rwx(self) -> crate::RwxPerms {
        crate::RwxPerms::new(
            self.allows_read(),
            self.allows_write(),
            self.allows_execute(),
        )
    }
}

/// Convert legacy Unicorn-style rwx bits to a Windows `PAGE_*` value.
///
/// Raw-`u32` shim for host mapping call sites that still speak in rwx bits.
/// Prefer [`PageProtect::from_rwx`], which cannot be handed the wrong encoding.
#[must_use]
pub fn page_protect_from_rwx(rwx: u32) -> u32 {
    PageProtect::from_rwx(crate::RwxPerms::from_bits(rwx)).to_win32()
}

/// Convert a Windows `PAGE_*` value to Unicorn-style rwx bits.
///
/// Unsupported values carry no permissions, matching the previous behaviour
/// where every `allows_*` predicate returned false.
#[must_use]
pub fn rwx_from_page_protect(protect: u32) -> u32 {
    PageProtect::from_win32(protect).map_or(0, |p| p.to_rwx().bits())
}

/// True if `value` is one of the Phase 3 primary protect constants.
#[must_use]
pub fn is_supported_protect(value: u32) -> bool {
    PageProtect::from_win32(value).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RwxPerms;
    use crate::perm;

    #[test]
    fn rwx_all_roundtrips_to_erw() {
        let p = PageProtect::from_rwx(RwxPerms::ALL);
        assert_eq!(p, PageProtect::ExecuteReadWrite);
        assert!(p.allows_read() && p.allows_write() && p.allows_execute());
        assert_eq!(p.to_rwx(), RwxPerms::ALL);
    }

    #[test]
    fn readonly_denies_write_and_exec() {
        let p = PageProtect::from_rwx(RwxPerms::READ);
        assert_eq!(p, PageProtect::ReadOnly);
        assert!(p.allows_read());
        assert!(!p.allows_write());
        assert!(!p.allows_execute());
    }

    #[test]
    fn execute_read_allows_fetch_not_write() {
        let p = PageProtect::from_rwx(RwxPerms::new(true, false, true));
        assert_eq!(p, PageProtect::ExecuteRead);
        assert!(p.allows(AccessKind::Read));
        assert!(p.allows(AccessKind::Execute));
        assert!(!p.allows(AccessKind::Write));
    }

    #[test]
    fn execute_only_allows_fetch_not_data_read() {
        let p = PageProtect::Execute;
        assert!(p.allows_execute());
        assert!(!p.allows_read());
        assert!(!p.allows_write());
    }

    /// Every variant survives the guest-ABI round trip.
    #[test]
    fn win32_roundtrips() {
        for p in [
            PageProtect::NoAccess,
            PageProtect::ReadOnly,
            PageProtect::ReadWrite,
            PageProtect::Execute,
            PageProtect::ExecuteRead,
            PageProtect::ExecuteReadWrite,
        ] {
            assert_eq!(PageProtect::from_win32(p.to_win32()), Some(p), "{p:?}");
        }
        assert_eq!(PageProtect::from_win32(0), None);
        assert_eq!(PageProtect::from_win32(0x1234), None);
    }

    /// The two encodings collide numerically, which is the whole reason they
    /// are separate types. Pin the collision so the hazard stays documented:
    /// reading rwx bits as a `PAGE_*` yields a *different, valid* protection.
    #[test]
    fn rwx_bits_and_page_constants_collide_numerically() {
        assert_eq!(perm::READ, PAGE_NOACCESS);
        assert_eq!(perm::WRITE, PAGE_READONLY);
        // The dangerous one: execute bits read as read-write.
        assert_eq!(perm::EXEC, PAGE_READWRITE);
        // …and the combined rwx set is not a valid protection at all.
        assert_eq!(PageProtect::from_win32(RwxPerms::ALL.bits()), None);
    }
}
