//! Fake API registration and dense VA decode for the runtime hook range.

use anyhow::Result;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use wie_winapi::{
    ComMethod, D3d9Iface, FakeVa, WinApiId, WinApiTraits, decode_fake_va, encode_export,
    encode_unresolved, resolve_winapi_id, winapi_id_export,
};

/// Runtime fake API dispatch entry (IAT soft slots + trace metadata).
#[derive(Debug, Clone)]
pub struct RuntimeFakeApiEntry {
    /// Fake API target virtual address.
    pub fake_target_va: u64,

    /// Imported library name.
    pub library: Arc<str>,

    /// Imported function name.
    pub name: Arc<str>,

    /// Runtime `IAT` slot virtual address (0 if not from IAT).
    pub iat_slot_va: u64,

    /// Pre-resolved dense handler id (None = soft / string dispatch).
    pub winapi_id: Option<WinApiId>,

    /// Hot-path classification resolved once at table build.
    pub traits: WinApiTraits,

    /// Pre-computed guest stub kind, if this entry can run entirely in-guest.
    /// Avoids re-classifying during stub planting.
    pub(crate) stub_kind: Option<crate::guest_stubs::GuestStubKind>,
}

/// Soft (unresolved) table: indexed by dense soft payload, plus a lowercase
/// `(library, name)` → index side-map so `intern` is O(1) instead of O(n).
///
/// Init cost was O(n²) in import count (linear scan for every intern call);
/// on large mingw / MSVC CRT bundles this was a measurable startup drag.
#[derive(Debug, Default, Clone)]
pub struct SoftApiTable {
    entries: Vec<RuntimeFakeApiEntry>,
    /// Lowercase `(library, name)` → index into `entries`.
    ///
    /// Only used by [`Self::intern`]; the enum-of-callers path reads through
    /// [`Self::get`] by dense index, so lookups on the hot handler path stay
    /// O(1) without touching this map.
    lookup: HashMap<SoftApiKey, u16>,
}

impl SoftApiTable {
    #[must_use]
    pub fn get(&self, index: u16) -> Option<&RuntimeFakeApiEntry> {
        self.entries.get(index as usize)
    }

    #[must_use]
    pub fn as_slice(&self) -> &[RuntimeFakeApiEntry] {
        &self.entries
    }

    /// Intern `(library, name)` → encoded VA (stable across duplicates).
    pub fn intern(
        &mut self,
        library: &str,
        name: &str,
        iat_slot_va: u64,
    ) -> Result<(u64, RuntimeFakeApiEntry)> {
        let key = SoftApiKey::new(library, name);
        if let Some(&idx) = self.lookup.get(&key)
            && let Some(existing) = self.entries.get(usize::from(idx))
        {
            return Ok((existing.fake_target_va, existing.clone()));
        }

        let idx = self.entries.len();
        let idx_u16 =
            u16::try_from(idx).map_err(|_| anyhow::anyhow!("soft API table exceeds 32767"))?;
        if idx_u16 >= 0x8000 {
            anyhow::bail!("soft API table exceeds encoding capacity");
        }
        let va = encode_unresolved(idx_u16);
        let entry = make_entry(va, library.to_owned(), name.to_owned(), iat_slot_va);
        self.entries.push(entry.clone());
        self.lookup.insert(key, idx_u16);
        Ok((va, entry))
    }
}

/// Case-insensitive `(library, name)` lookup key for [`SoftApiTable::intern`].
///
/// Both parts are ASCII-lowercased exactly like the old NUL-joined
/// `"library\0name"` string key: `("a", "bc")` stays distinct from
/// `("ab", "c")`, and mixed-case imports still hit the same slot.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SoftApiKey {
    library: String,
    name: String,
}

impl SoftApiKey {
    fn new(library: &str, name: &str) -> Self {
        Self {
            library: lowercase_ascii(library),
            name: lowercase_ascii(name),
        }
    }
}

/// ASCII-lowercase `s` (byte-preserving for non-ASCII, as before).
fn lowercase_ascii(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        out.push(c.to_ascii_lowercase());
    }
    out
}

/// Resolved stop target after bit-decode (no HashMap).
///
/// `library` and `name` use [`Cow<'static, str>`] so the Export/Alias path can
/// borrow the static string slices returned by [`winapi_id_export`] — zero
/// allocation on the hot path, no ref-counting.  The Unresolved/COM paths that
/// need owned strings allocate only once, at resolution time.
#[derive(Debug, Clone)]
pub struct ResolvedFakeApi {
    pub library: Cow<'static, str>,
    pub name: Cow<'static, str>,
    pub winapi_id: Option<WinApiId>,
    pub traits: WinApiTraits,
}

pub(crate) fn make_entry(
    fake_target_va: u64,
    library: String,
    name: String,
    iat_slot_va: u64,
) -> RuntimeFakeApiEntry {
    let winapi_id = resolve_winapi_id(&library, &name);
    let mut traits = winapi_id.map(WinApiId::traits).unwrap_or_default();
    // ExitProcess / CRT exit: handled specially in the session loop.
    if (library.eq_ignore_ascii_case("KERNEL32.dll") && name.eq_ignore_ascii_case("ExitProcess"))
        || (wie_winapi::ucrt::is_ucrt_library(&library)
            && (name.eq_ignore_ascii_case("exit")
                || name.eq_ignore_ascii_case("_exit")
                || name.eq_ignore_ascii_case("abort")))
    {
        traits = WinApiTraits::EMPTY.with_exit_process();
    }
    // Align traits with planted guest stubs even if WinApiId map is incomplete.
    // Cache the stub kind so plant_guest_stubs doesn't re-classify.
    let stub_kind = crate::guest_stubs::classify_guest_stub(
        &library,
        &name,
        &crate::guest_stubs::GuestStubConfig::CLASSIFY_ONLY,
    );
    if stub_kind.is_some() {
        traits.set_guest_stub(true);
        traits.set_noisy(true);
    }

    RuntimeFakeApiEntry {
        fake_target_va,
        library: library.into(),
        name: name.into(),
        iat_slot_va,
        winapi_id,
        traits,
        stub_kind,
    }
}

/// Resolve import to dense fake VA; grows `soft` for non-WinApiId exports.
pub fn resolve_import_fake_va(
    library: &str,
    name: &str,
    iat_slot_va: u64,
    soft: &mut SoftApiTable,
) -> Result<(u64, RuntimeFakeApiEntry)> {
    if let Some(id) = resolve_winapi_id(library, name) {
        let va = encode_export(id);
        return Ok((
            va,
            make_entry(va, library.to_owned(), name.to_owned(), iat_slot_va),
        ));
    }
    soft.intern(library, name, iat_slot_va)
}

/// O(1) decode of a host-stop address into dispatch metadata.
///
/// Traits (guest_stub, noisy, exit_process, …) are pre-computed by `make_entry`
/// and embedded in `WinApiId::traits()` for Export entries — no need to
/// re-classify guest stubs on the hot path.
///
/// `library` and `name` borrow from [`winapi_id_export`]'s static strings for
/// the Export/Alias path — zero allocation on every stop.  The Unresolved path
/// converts the soft-table `Arc<str>` to an owned `String` (far less common).
pub(crate) fn resolve_fake_api_at(address: u64, soft: &SoftApiTable) -> Option<ResolvedFakeApi> {
    let decoded = decode_fake_va(address)?;
    let _ = address; // available for future trace correlation
    match decoded {
        FakeVa::Export(id) | FakeVa::Alias(id) => {
            let (lib, name) = winapi_id_export(id).unwrap_or(("unknown.dll", "unknown"));
            Some(ResolvedFakeApi {
                library: Cow::Borrowed(lib),
                name: Cow::Borrowed(name),
                winapi_id: Some(id),
                traits: id.traits(),
            })
        }
        FakeVa::Unresolved(index) => {
            let e = soft.get(index)?;
            Some(ResolvedFakeApi {
                library: Cow::Owned(e.library.to_string()),
                name: Cow::Owned(e.name.to_string()),
                winapi_id: e.winapi_id,
                traits: e.traits,
            })
        }
        FakeVa::Com { iface, method } => resolve_com(iface, method),
        FakeVa::Special(_) => None, // handled by session before resolve
    }
}

fn resolve_com(iface: D3d9Iface, method: ComMethod) -> Option<ResolvedFakeApi> {
    let name = method.name(iface);
    let library = "D3D9.dll";
    let winapi_id = resolve_winapi_id(library, name.as_ref());
    let traits = winapi_id.map(WinApiId::traits).unwrap_or_default();
    Some(ResolvedFakeApi {
        library: Cow::Borrowed(library),
        name,
        winapi_id,
        traits,
    })
}

/// Collect every known plantable entry for guest stubs: IAT + soft table uniques.
///
/// Uses a [`HashSet`] of known VAs for O(n+m) dedup instead of O(n×m) linear scan.
pub(crate) fn collect_stub_entries(
    iat_entries: &[RuntimeFakeApiEntry],
    soft: &SoftApiTable,
) -> Vec<RuntimeFakeApiEntry> {
    let soft_len = soft.as_slice().len();
    let mut seen = HashSet::with_capacity(iat_entries.len().max(soft_len));
    let mut out = Vec::with_capacity(iat_entries.len().max(soft_len));

    for e in iat_entries {
        seen.insert(e.fake_target_va);
        out.push(e.clone());
    }
    for e in soft.as_slice() {
        if seen.insert(e.fake_target_va) {
            out.push(e.clone());
        }
    }
    out
}
