//! Host-side registry hive: per-key value storage with per-bottle persistence.
//!
//! Values are keyed by the full hive path (e.g. `HKCU\Software\Microsoft\Notepad`)
//! rather than by the per-session fake key handle: handles restart at
//! `0x7000_0000` on every launch, so only a path key survives a relaunch. The
//! whole value map is written through to `{bottle_root}/registry/hive.dat` on
//! every mutation and loaded once per session, which is what makes RNotepad
//! settings persist across relaunches.
//!
//! The store is deliberately infallible at the API boundary: a missing or
//! corrupt hive file starts the session empty (the guest falls back to its
//! built-in defaults, exactly as on a fresh Windows profile), and a failed
//! write is logged rather than failing the guest's `RegSetValueEx`.

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One named value under a registry key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryValue {
    /// Value name; the empty string is the key's default value.
    pub name: String,
    /// `REG_*` type code stored with the value (`RegQueryValueEx` returns it).
    pub value_type: u32,
    /// Raw bytes exactly as the guest wrote them; `REG_SZ` data keeps its NUL.
    pub data: Vec<u8>,
}

impl RegistryValue {
    /// Create a value record.
    #[must_use]
    pub fn new(name: String, value_type: u32, data: Vec<u8>) -> Self {
        Self {
            name,
            value_type,
            data,
        }
    }
}

/// The host-side registry store, shared across the whole process.
///
/// Lives in a [`crate::state::DllStateMap`] slot so it costs nothing until the
/// guest first touches ADVAPI32 and needs no field on the big state structs.
#[derive(Debug, Clone, Default)]
pub struct RegistryState {
    /// Values keyed by full hive path (`HKCU\Software\...`).
    values: HashMap<String, Vec<RegistryValue>>,
    /// Whether the hive file has been loaded for this session.
    loaded: bool,
}

impl RegistryState {
    /// Load the hive file once per session. A missing file (fresh bottle) or a
    /// corrupt one starts the session with an empty store; the guest then uses
    /// its built-in defaults, matching a fresh Windows profile.
    pub fn ensure_loaded(&mut self, root: Option<&Path>) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        let Some(root) = root else { return };
        let path = hive_file_path(root);
        let Ok(bytes) = std::fs::read(&path) else {
            return;
        };
        match deserialize_hive(&bytes) {
            Ok(values) => self.values = values,
            Err(err) => tracing::warn!(
                path = %path.display(),
                %err,
                "registry hive load failed; starting with an empty store"
            ),
        }
    }

    /// Create or overwrite one value under `path`.
    pub fn set_value(&mut self, path: &str, value: RegistryValue) {
        let entry = self.values.entry(path.to_owned()).or_default();
        if let Some(existing) = entry.iter_mut().find(|v| v.name == value.name) {
            *existing = value;
        } else {
            entry.push(value);
        }
    }

    /// The stored value, when the key and value both exist.
    #[must_use]
    pub fn get_value(&self, path: &str, name: &str) -> Option<&RegistryValue> {
        let entry = self.values.get(path)?;
        entry.iter().find(|v| v.name == name)
    }

    /// Remove one value; returns `true` when it existed.
    pub fn delete_value(&mut self, path: &str, name: &str) -> bool {
        let Some(entry) = self.values.get_mut(path) else {
            return false;
        };
        let before = entry.len();
        entry.retain(|v| v.name != name);
        let deleted = entry.len() != before;
        // `entry`'s last use is above; the borrow is dead before the remove.
        let emptied_key = deleted && entry.is_empty();
        if emptied_key {
            self.values.remove(path);
        }
        deleted
    }

    /// Write the whole value map through to the bottle's hive file.
    ///
    /// Write-through on every mutation, not on shutdown: a kill -9 after a
    /// `RegSetValueEx` still leaves the previous value durable, and there is no
    /// session-teardown hook to miss.
    pub fn persist(&self, root: Option<&Path>) {
        let Some(root) = root else { return };
        let path = hive_file_path(root);
        if let Err(err) = write_hive(root, &self.values) {
            tracing::warn!(
                path = %path.display(),
                %err,
                "registry hive write failed; change stays in-memory for this session"
            );
        }
    }
}

/// Canonical path prefix for the well-known root handles (WinNT.h `HKEY_*`).
///
/// `None` for anything else: such a handle is either unknown (bad guest) or a
/// fake key handle, which the path walk resolves through the key records.
#[must_use]
pub(crate) fn root_prefix(handle: u64) -> Option<&'static str> {
    match handle {
        HKEY_CLASSES_ROOT => Some("HKCR"),
        HKEY_CURRENT_USER => Some("HKCU"),
        HKEY_LOCAL_MACHINE => Some("HKLM"),
        HKEY_USERS => Some("HKU"),
        HKEY_CURRENT_CONFIG => Some("HKCC"),
        _ => None,
    }
}

// WinNT.h root-handle constants: the handle values guests pass to
// `RegOpenKey*` / `RegCreateKeyEx*` as the parent key.
pub(crate) const HKEY_CLASSES_ROOT: u64 = 0x8000_0000;
pub(crate) const HKEY_CURRENT_USER: u64 = 0x8000_0001;
pub(crate) const HKEY_LOCAL_MACHINE: u64 = 0x8000_0002;
pub(crate) const HKEY_USERS: u64 = 0x8000_0003;
pub(crate) const HKEY_CURRENT_CONFIG: u64 = 0x8000_0005;

/// The hive file lives at `{bottle_root}/registry/hive.dat`, beside `drive_c/`.
fn hive_file_path(root: &Path) -> PathBuf {
    root.join("registry").join("hive.dat")
}

/// 8-byte magic: `WIE` + `HKCV` + version `1`.
const HIVE_MAGIC: &[u8; 8] = b"WIEHKCV1";

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    push_u32(out, u32::try_from(bytes.len()).unwrap_or(u32::MAX));
    out.extend_from_slice(bytes);
}

fn push_string(out: &mut Vec<u8>, value: &str) {
    push_bytes(out, value.as_bytes());
}

fn serialize_hive(values: &HashMap<String, Vec<RegistryValue>>) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(HIVE_MAGIC);
    push_u32(&mut out, u32::try_from(values.len()).unwrap_or(u32::MAX));
    for (path, key_values) in values {
        push_string(&mut out, path);
        push_u32(
            &mut out,
            u32::try_from(key_values.len()).unwrap_or(u32::MAX),
        );
        for value in key_values {
            push_string(&mut out, &value.name);
            push_u32(&mut out, value.value_type);
            push_bytes(&mut out, &value.data);
        }
    }
    out
}

/// Read a 4-byte little-endian length-prefixed slice, advancing the cursor.
fn take_u32(bytes: &[u8], cursor: &mut usize) -> Option<u32> {
    let slice = bytes.get(*cursor..cursor.saturating_add(4))?;
    *cursor = cursor.saturating_add(4);
    Some(u32::from_le_bytes(slice.try_into().ok()?))
}

/// Read `len` bytes, advancing the cursor.
fn take_bytes<'a>(bytes: &'a [u8], cursor: &mut usize, len: usize) -> Option<&'a [u8]> {
    let end = cursor.checked_add(len)?;
    let slice = bytes.get(*cursor..end)?;
    *cursor = end;
    Some(slice)
}

fn take_string(bytes: &[u8], cursor: &mut usize) -> Result<String> {
    let len = usize::try_from(take_u32(bytes, cursor).context("hive: truncated string length")?)
        .unwrap_or(0);
    let raw = take_bytes(bytes, cursor, len).context("hive: truncated string body")?;
    Ok(String::from_utf8_lossy(raw).into_owned())
}

fn deserialize_hive(bytes: &[u8]) -> Result<HashMap<String, Vec<RegistryValue>>> {
    let mut cursor = 0usize;
    let magic = take_bytes(bytes, &mut cursor, 8).context("hive: truncated magic")?;
    if magic != HIVE_MAGIC.as_slice() {
        bail!("hive: bad magic");
    }
    let key_count = take_u32(bytes, &mut cursor).context("hive: truncated key count")?;
    let mut values = HashMap::new();
    for _ in 0..key_count {
        let path = take_string(bytes, &mut cursor).context("hive: truncated key path")?;
        let value_count = take_u32(bytes, &mut cursor).context("hive: truncated value count")?;
        let mut key_values = Vec::new();
        for _ in 0..value_count {
            let name = take_string(bytes, &mut cursor).context("hive: truncated value name")?;
            let value_type = take_u32(bytes, &mut cursor).context("hive: truncated value type")?;
            let data_len =
                usize::try_from(take_u32(bytes, &mut cursor).context("hive: truncated data len")?)
                    .unwrap_or(0);
            let data = take_bytes(bytes, &mut cursor, data_len)
                .context("hive: truncated value data")?
                .to_vec();
            key_values.push(RegistryValue {
                name,
                value_type,
                data,
            });
        }
        values.insert(path, key_values);
    }
    Ok(values)
}

fn write_hive(root: &Path, values: &HashMap<String, Vec<RegistryValue>>) -> Result<()> {
    let path = hive_file_path(root);
    let dir = path.parent().context("registry hive path has no parent")?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create registry dir {}", dir.display()))?;
    std::fs::write(&path, serialize_hive(values))
        .with_context(|| format!("write registry hive {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // WinNT.h type codes used by the fixtures below. Kept here (not in the
    // store itself) because the store is type-agnostic: it stores whatever the
    // guest passes through, exactly as real Windows does.
    const REG_SZ: u32 = 1;
    const REG_DWORD: u32 = 4;

    #[test]
    fn store_set_get_delete() {
        let mut store = RegistryState::default();
        let path = r"HKCU\Software\Microsoft\Notepad";
        assert!(store.get_value(path, "fWrap").is_none());
        store.set_value(
            path,
            RegistryValue::new("fWrap".into(), REG_DWORD, 1_u32.to_le_bytes().to_vec()),
        );
        let value = store.get_value(path, "fWrap").expect("value stored");
        assert_eq!(value.value_type, REG_DWORD);
        assert_eq!(value.data, 1_u32.to_le_bytes().to_vec());
        // Overwrite in place, keeping a single record per name.
        store.set_value(
            path,
            RegistryValue::new("fWrap".into(), REG_DWORD, 0_u32.to_le_bytes().to_vec()),
        );
        let key_values = store.values.get(path).expect("key present");
        assert_eq!(key_values.len(), 1);
        // Deleting the only value removes the key record too.
        assert!(store.delete_value(path, "fWrap"));
        assert!(!store.delete_value(path, "fWrap"));
        assert!(store.values.is_empty());
    }

    #[test]
    fn hive_serialize_round_trip() {
        let mut values = HashMap::new();
        let key_values = vec![
            RegistryValue::new(
                "iWindowPosX".into(),
                REG_DWORD,
                120_u32.to_le_bytes().to_vec(),
            ),
            RegistryValue::new("searchString".into(), REG_SZ, b"hello\0".to_vec()),
        ];
        values.insert(r"HKCU\Software\Microsoft\Notepad".to_owned(), key_values);
        let bytes = serialize_hive(&values);
        let decoded = deserialize_hive(&bytes).expect("round trip decodes");
        assert_eq!(decoded, values);
    }

    #[test]
    fn hive_rejects_corrupt_input() {
        assert!(deserialize_hive(b"nope").is_err());
        let mut bytes = serialize_hive(&HashMap::new());
        // Flip one magic byte.
        if let Some(b) = bytes.get_mut(3) {
            *b = b'X';
        }
        assert!(deserialize_hive(&bytes).is_err());
    }

    #[test]
    fn hive_persists_to_file_and_back() {
        let root = std::env::temp_dir().join("wie_registry_test_hive");
        std::fs::remove_dir_all(&root).ok();
        let path = r"HKCU\Software\Microsoft\Notepad";
        let mut first = RegistryState::default();
        first.ensure_loaded(Some(&root));
        first.set_value(
            path,
            RegistryValue::new("fStatusBar".into(), REG_DWORD, 0_u32.to_le_bytes().to_vec()),
        );
        first.persist(Some(&root));
        assert!(hive_file_path(&root).exists());

        // A brand-new session (fresh DllId slot) sees the persisted value.
        let mut second = RegistryState::default();
        second.ensure_loaded(Some(&root));
        let value = second
            .get_value(path, "fStatusBar")
            .expect("persisted value");
        assert_eq!(value.value_type, REG_DWORD);
        assert_eq!(value.data, 0_u32.to_le_bytes().to_vec());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_file_loads_empty_without_error() {
        let root = std::env::temp_dir().join("wie_registry_test_missing");
        std::fs::remove_dir_all(&root).ok();
        let mut store = RegistryState::default();
        store.ensure_loaded(Some(&root));
        assert!(store.values.is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn root_prefix_maps_standard_hives() {
        assert_eq!(root_prefix(HKEY_CURRENT_USER), Some("HKCU"));
        assert_eq!(root_prefix(HKEY_LOCAL_MACHINE), Some("HKLM"));
        assert_eq!(root_prefix(0x7000_0000), None);
    }
}
