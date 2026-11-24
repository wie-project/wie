//! Fake API registration and dense VA decode for the runtime hook range.

use ahash::HashMap;
use anyhow::Result;
use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;
use wie_winapi::{
    ComIface, ComMethod, FakeVa, WinApiId, WinApiTraits, decode_fake_va, encode_export,
    encode_unresolved, resolve_winapi_id, winapi_id_export,
};

/// Runtime fake API dispatch entry (IAT soft slots + trace metadata).
#[derive(Debug, Clone)]
pub struct RuntimeFakeApiEntry {
    /// Dense encoded stop VA in the fake range that lands control here.
    pub fake_target_va: u64,

    /// Library the import came from (ASCII, case preserved).
    pub library: Arc<str>,

    /// Imported export name.
    pub name: Arc<str>,

    /// Guest VA of the backing IAT slot (0 when not IAT-resolved).
    pub iat_slot_va: u64,

    /// Pre-resolved dense handler id; `None` for soft/string dispatch.
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
pub(crate) struct SoftApiTable {
    entries: Vec<RuntimeFakeApiEntry>,
    /// Lowercase `(library, name)` → index into `entries`.
    ///
    /// Only used by [`Self::intern`]; the enum-of-callers path reads through
    /// [`Self::get`] by dense index, so lookups on the hot handler path stay
    /// O(1) without touching this map.
    lookup: HashMap<SoftApiKey, u16>,
}

impl SoftApiTable {
    /// Look up a soft entry by dense index (O(1) on the handler path).
    #[must_use]
    pub(crate) fn get(&self, index: u16) -> Option<&RuntimeFakeApiEntry> {
        self.entries.get(index as usize)
    }

    #[must_use]
    pub(crate) fn as_slice(&self) -> &[RuntimeFakeApiEntry] {
        &self.entries
    }

    /// Intern `(library, name)` → encoded VA (stable across duplicates).
    pub(crate) fn intern(
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
pub(crate) struct ResolvedFakeApi {
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
pub(crate) fn resolve_import_fake_va(
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
/// Hot-path classification (guest stub, noisy, exit process, …) is
/// pre-computed by `make_entry` and embedded in `WinApiId::traits()` for Export
/// entries — no need to re-classify guest stubs on the hot path.
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

/// Resolve a `kind=Com` fake VA to the `(library, method-name)` pair runtime
/// dispatch routes it under.
///
/// The library comes from the interface sum, not from the method name: a
/// `ComMethod` alone is ambiguous, because `QueryInterface` / `AddRef` /
/// `Release` exist on every COM surface WIE implements.
fn resolve_com(iface: ComIface, method: ComMethod) -> Option<ResolvedFakeApi> {
    let name = method.name(iface);
    let library = iface.library();
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

/// DirectInput8 wiring tests.
///
/// These live here rather than next to the code they cover because
/// `session/pump.rs` and `memory.rs` are off-limits to this change, and both
/// facts worth pinning are properties of *this* file's name resolution plus
/// `memory.rs`'s env pair. Two things must stay true:
///
/// 1. Every DirectInput COM vtable stop resolves to the `dinput8.dll` library
///    with a readable trace name, so `dispatch_winapi` reaches
///    `crate::dinput::dispatch_object_method` instead of bailing with
///    "unsupported WinAPI call".
/// 2. `SDL_DIRECTINPUT_ENABLED=0` must stay in the guest environment. It is the
///    reason SDL2 never touches this new DirectInput path; flipping it would
///    push SDL2 onto an unproven implementation, and it is invisible in a diff
///    of the DirectInput code itself — hence the guard.
#[cfg(test)]
mod tests {
    use super::*;
    use wie_winapi::fake_va::{DInput8Iface, DirectInput8Method, DirectInputDevice8Method};

    use crate::memory::build_default_environment_strings_w;

    /// Every modelled `IDirectInput8` / `IDirectInputDevice8` slot must resolve
    /// to `dinput8.dll` with a name the dispatcher recognises. A COM stop that
    /// resolved to the wrong library would silently become an
    /// "unsupported WinAPI call" at run time.
    #[test]
    fn directinput_com_stops_resolve_to_the_dinput8_library() {
        for slot in 0..DirectInput8Method::VTABLE_SLOTS {
            let slot = u8::try_from(slot).expect("slot fits u8");
            let method = ComMethod::decode(ComIface::DInput8(DInput8Iface::DirectInput8), slot);
            let resolved = resolve_com(ComIface::DInput8(DInput8Iface::DirectInput8), method)
                .expect("an IDirectInput8 stop must resolve");
            assert_eq!(
                resolved.library.as_ref(),
                "dinput8.dll",
                "IDirectInput8 slot {slot} must resolve under dinput8.dll"
            );
            assert!(
                resolved.name.starts_with("IDirectInput8::"),
                "IDirectInput8 slot {slot} trace name was {:?}",
                resolved.name
            );
        }
        for slot in 0..DirectInputDevice8Method::VTABLE_SLOTS {
            let slot = u8::try_from(slot).expect("slot fits u8");
            let method =
                ComMethod::decode(ComIface::DInput8(DInput8Iface::DirectInputDevice8), slot);
            let resolved = resolve_com(ComIface::DInput8(DInput8Iface::DirectInputDevice8), method)
                .expect("an IDirectInputDevice8 stop must resolve");
            assert_eq!(
                resolved.library.as_ref(),
                "dinput8.dll",
                "IDirectInputDevice8 slot {slot} must resolve under dinput8.dll"
            );
            assert!(
                resolved.name.starts_with("IDirectInputDevice8::"),
                "IDirectInputDevice8 slot {slot} trace name was {:?}",
                resolved.name
            );
        }
    }

    /// The export-name contract: `dinput8.dll` is a WinAPI library with
    /// exactly one real export, and **no** COM vtable slot may be offered as a
    /// dll export name.
    ///
    /// The slot names are derived from the real `ComMethod::name()` enums
    /// rather than hand-listed, so this covers every slot WIE models instead of
    /// whatever a list happened to contain. Getting it wrong in either
    /// direction is a real bug: a slot reported as an export gets a soft
    /// placeholder from the loader, and a real export reported as missing makes
    /// a guest's `GetProcAddress` return NULL.
    #[test]
    fn the_served_directinput_methods_are_dispatchable_by_name() {
        use wie_winapi::fake_va::{ComIface, ComMethod};

        assert!(
            wie_winapi::is_winapi_library("dinput8.dll"),
            "dinput8.dll must be a WinAPI library or a guest importing it fails to load"
        );
        // The one real export does resolve, so both a static import and
        // `GetProcAddress` land on a handler.
        assert!(
            wie_winapi::is_winapi_implemented("dinput8.dll", "DirectInput8Create"),
            "DirectInput8Create must resolve to a real handler"
        );
        assert!(
            !wie_winapi::is_winapi_implemented("dinput8.dll", "DllCanUnloadNow"),
            "dinput8.dll exports exactly one name; anything else must not be claimed"
        );

        // No COM slot may be reachable as a dll export name.
        for (iface, slots) in [
            (
                ComIface::DInput8(DInput8Iface::DirectInput8),
                DirectInput8Method::VTABLE_SLOTS,
            ),
            (
                ComIface::DInput8(DInput8Iface::DirectInputDevice8),
                DirectInputDevice8Method::VTABLE_SLOTS,
            ),
        ] {
            for slot in 0..slots {
                let slot = u8::try_from(slot).expect("slot fits u8");
                let name = ComMethod::decode(iface, slot).name(iface);
                assert!(
                    !wie_winapi::is_winapi_implemented("dinput8.dll", name.as_ref()),
                    "{name} is a COM vtable slot, not a dll export; it must not be \
                     reported as a resolvable export name"
                );
            }
        }
    }

    /// `SDL_DIRECTINPUT_ENABLED=0` must stay in the guest environment block.
    ///
    /// SDL2 is the guest that motivated the DirectInput lane, and this hint is
    /// the only thing keeping `SDL_InitSubSystem(SDL_INIT_VIDEO)` off the
    /// DirectInput joystick driver. It is invisible in a diff of the DirectInput
    /// code, so without this guard a well-meaning "let's try the new path"
    /// commit could flip it silently.
    #[test]
    fn sdl_directinput_stays_disabled_in_the_guest_environment() {
        let bytes = build_default_environment_strings_w()
            .expect("the default environment block must build");
        // The block is a run of NUL-terminated UTF-16 strings (dinput.h has
        // nothing to do with it — this is the plain Win32 env block), so split
        // on the NULs rather than stopping at the first one.
        let mut entries: Vec<String> = Vec::new();
        let mut current = String::new();
        for pair in bytes.as_chunks::<2>().0 {
            match char::from_u32(u32::from(u16::from_le_bytes(*pair))) {
                Some('\0') if current.is_empty() => {}
                Some('\0') => {
                    entries.push(std::mem::take(&mut current));
                }
                Some(unit) => current.push(unit),
                None => {}
            }
        }
        assert!(
            entries
                .iter()
                .any(|entry| entry == "SDL_DIRECTINPUT_ENABLED=0"),
            "the default env block must keep SDL_DIRECTINPUT_ENABLED=0; got: {entries:?}"
        );
    }
}
