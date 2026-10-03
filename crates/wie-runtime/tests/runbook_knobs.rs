//! `docs/RUNBOOK.md`'s knob table must equal the set of knobs the code reads.
//!
//! # Why
//!
//! The RUNBOOK knob table is the emulator's bisecting interface: when something
//! regresses, the documented set of `WIE_*` kill-switches is what you reach for.
//! It drifted in both directions before this test existed — rows for knobs that
//! no longer exist (a reader sets `WIE_HOST_SLEEP=1` and nothing happens, which
//! reads as "the switch is broken" rather than "the switch is gone"), and no
//! rows for knobs that do (a reader greps the table for the knob they just
//! added and finds nothing).
//!
//! # Which direction is enforced, and why
//!
//! **Both**, modulo `RUST_LOG`:
//!
//! - `live ⊆ documented` — a knob the code reads but the table omits is drift.
//! - `documented ⊆ live ∪ {RUST_LOG}` — a row naming a knob nothing reads is
//!   drift. This direction is the one that matters most, because a dead row is
//!   actively misleading: setting it does nothing, and there is no way for a
//!   reader to tell that from a broken switch.
//!
//! Bidirectional equality is achievable because the "live" side is extracted
//! from *read/write call sites only* — see [`live_knobs`] — so it is immune to
//! the false positives that make an exhaustive text scan flaky. The known
//! comment-only mentions (`WIE_HOST_SLEEP` in a doc-comment that says it is no
//! longer parsed) are therefore invisible to the live set, which is exactly
//! what should make their RUNBOOK rows fail.
//!
//! `RUST_LOG` is the one documented name that is not a `WIE_*` knob; it is a
//! `tracing-subscriber` filter and no code in this repo reads it, so it is
//! allowed through explicitly rather than by loosening the extraction.
//!
//! Adding a knob is therefore a two-step change: read it through one of the
//! getters in `crates/wie-runtime/src/knobs.rs` (or the equivalent in the crate
//! that owns it) *and* add a row. This test is what makes the second half
//! impossible to forget.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Names documented in the RUNBOOK that are not `WIE_*` knobs. Each one is
/// allowed through `documented ⊆ live` explicitly rather than by widening the
/// extraction, so adding a second entry here is a deliberate decision.
const NON_WIE_DOCUMENTED: &[&str] = &["RUST_LOG"];

/// Knobs that used to exist, whose rows were removed. Asserted absent so that a
/// future edit cannot quietly reintroduce a row implying they still work; each
/// is noted in the RUNBOOK's prose note instead. If one of these ever comes
/// back as a real read, the live set will pick it up and this list must lose it.
const KNOWN_DEAD: &[&str] = &[
    "WIE_COMPACT_STRING",
    "WIE_D3D9_INPLACE",
    "WIE_HOST_SLEEP",
    "WIE_JIT_DIRECT_REGS",
    "WIE_JIT_TAILCHAIN",
    "WIE_JIT_VERIFY",
];

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is `<repo>/crates/wie-runtime`.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/wie-runtime is two levels below the repo root")
        .to_path_buf()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Every `WIE_*` name read or written from a real environment call site.
///
/// Anchoring on the *call* rather than scanning for `WIE_[A-Z0-9_]+` anywhere is
/// the whole point: a bare text scan picks up doc-comments, `tracing` messages
/// and guest-side names injected with `set_guest_env` (`WIE_SELFTEST`,
/// `WIE_DIALOG_HOSTDRIVEN`), plus clipboard format names in tests
/// (`WIE_TEST_FORMAT`) — all knobs-looking text that no host env read ever
/// touches. Matching only the callees below makes the extraction immune to
/// those, which in turn is what lets the test assert bidirectional equality
/// instead of a fuzzier subset relation.
///
/// The callee list must cover every wrapper the workspace uses. Missing one
/// shows up as an "undocumented" failure naming that knob, which is a safe
/// failure direction: it cannot silently make the test pass.
fn live_knobs(root: &Path) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for crate_dir in ["wie-pe", "wie-cpu", "wie-winapi", "wie-runtime", "wie-cli"] {
        collect_rs_files(&root.join("crates").join(crate_dir), &mut |path| {
            for line in read(path).lines() {
                // Only the call site matters, so scan for a quoted WIE_ name
                // that directly follows one of the known read/write callees.
                for callee in [
                    "env::var(",
                    "env::var_os(",
                    "env::set_var(",
                    "env::remove_var(",
                    "env_override(",
                    "parse_u64_env(",
                    "env_u32(",
                    "env_u64(",
                    "env_bool(",
                    "env_usize(",
                ] {
                    let mut rest = line;
                    while let Some(at) = rest.find(callee) {
                        rest = &rest[at + callee.len()..];
                        let trimmed = rest.trim_start();
                        if let Some(name) = trimmed
                            .strip_prefix('"')
                            .and_then(|s| s.split('"').next())
                            .filter(|n| n.starts_with("WIE_"))
                        {
                            found.insert(name.to_owned());
                        }
                    }
                }
            }
        });
    }
    assert!(
        !found.is_empty(),
        "extraction found no knobs — is the regex stale?"
    );
    found
}

fn collect_rs_files(dir: &Path, sink: &mut dyn FnMut(&Path)) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_rs_files(&path, sink);
        } else if path.extension().is_some_and(|e| e == "rs") {
            sink(&path);
        }
    }
}

/// `WIE_*` names appearing in the **first column** of the RUNBOOK's knob tables.
///
/// Restricted to the two tables under "Environment knobs (full table)": the
/// earlier "Identity" and "Symptoms → actions" tables also mention `WIE_*`, but
/// those are prose pointers at the knob table, not the authoritative list, and
/// counting them would double-count names the knob table does not list.
fn documented_knobs(runbook: &str) -> BTreeSet<String> {
    let start = runbook
        .find("## Environment knobs (full table)")
        .expect("RUNBOOK lost its '## Environment knobs (full table)' heading");
    let section = &runbook[start..];

    let mut names = BTreeSet::new();
    let mut in_table = false;
    for line in section.lines() {
        let line = line.trim();
        if line.starts_with("## ") {
            // The knob tables are the first two in this section; anything after
            // them ("Bottles", "Launching apps", …) is not knob documentation.
            if !names.is_empty() {
                break;
            }
            in_table = false;
            continue;
        }
        if !line.starts_with('|') {
            continue;
        }
        let first_cell = line.split('|').nth(1).unwrap_or_default().trim();
        if first_cell.is_empty() {
            continue;
        }
        // Skip the header and the `---` separator rows.
        if first_cell == "Variable" || first_cell.chars().all(|c| c == '-') {
            in_table = true;
            continue;
        }
        if !in_table {
            continue;
        }
        for name in extract_wie_names(first_cell) {
            names.insert(name);
        }
    }
    assert!(
        !names.is_empty(),
        "parsed no knobs from the RUNBOOK knob table — is the table still markdown pipes?"
    );
    names
}

/// `WIE_[A-Z0-9_]+` tokens in `text`, trimmed of trailing `_` runs so a
/// truncated diagnostic like `"WIE_JIT_"` cannot masquerade as a knob.
fn extract_wie_names(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if text[i..].starts_with("WIE_") && (i == 0 || !is_name_byte(bytes[i - 1])) {
            let mut j = i + 4;
            while j < bytes.len()
                && (bytes[j].is_ascii_uppercase() || bytes[j].is_ascii_digit() || bytes[j] == b'_')
            {
                j += 1;
            }
            let name = &text[i..j];
            if name.len() > "WIE_".len() && !name.ends_with('_') {
                out.push(name.to_owned());
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn report(label: &str, left: &BTreeSet<String>, right: &BTreeSet<String>) -> String {
    let only_left = left.difference(right).cloned().collect::<Vec<_>>();
    let only_right = right.difference(left).cloned().collect::<Vec<_>>();
    format!(
        "{label}:\n  only in {lhs}: {only_left:?}\n  only in {rhs}: {only_right:?}",
        lhs = "left",
        rhs = "right",
        only_left = only_left,
        only_right = only_right,
    )
}

#[test]
fn runbook_knob_table_matches_the_knobs_the_code_reads() {
    let root = repo_root();
    let live = live_knobs(&root);
    let documented = documented_knobs(&read(&root.join("docs/RUNBOOK.md")));

    // Every knob the code reads is documented. This is the direction a reader
    // depends on: greping the table for the knob you just added must find it.
    let undocumented = live.difference(&documented).cloned().collect::<Vec<_>>();
    assert!(
        undocumented.is_empty(),
        "these knobs are read by the code but have no RUNBOOK row: {undocumented:?}"
    );

    // Every documented knob is read by the code (plus the explicitly allowed
    // non-`WIE_*` names). This is the direction that bites: a dead row is a
    // switch that silently does nothing.
    let mut allowed = live.clone();
    for name in NON_WIE_DOCUMENTED {
        allowed.insert((*name).to_owned());
    }
    let phantom = documented.difference(&allowed).cloned().collect::<Vec<_>>();
    assert!(
        phantom.is_empty(),
        "the RUNBOOK documents knobs no code reads: {phantom:?}.\n{}",
        report("live vs documented", &live, &documented)
    );
}

#[test]
fn removed_knobs_are_not_listed_as_live() {
    let runbook = read(&repo_root().join("docs/RUNBOOK.md"));
    let documented = documented_knobs(&runbook);
    let live = live_knobs(&repo_root());

    for name in KNOWN_DEAD {
        assert!(
            !documented.contains(*name),
            "{name} has no read site in any crate but still has a RUNBOOK row; \
             delete the row and, if the removal is not already covered, fold it \
             into the prose note above the knob table"
        );
        assert!(
            !live.contains(*name),
            "{name} is in KNOWN_DEAD but the code reads it again — remove it from \
             KNOWN_DEAD and give it a real row"
        );
    }

    // A dead knob may still be *mentioned*, because a reader who remembers it
    // has no other way to learn it is gone. What must not happen is a mention
    // that reads like a working switch. So: if a KNOWN_DEAD name appears in the
    // prose at all, the line it appears on has to say it is gone.
    for name in KNOWN_DEAD {
        for (i, line) in runbook.lines().enumerate() {
            if !line.contains(name) {
                continue;
            }
            let lower = line.to_ascii_lowercase();
            assert!(
                ["removed", "deprecated", "never", "no longer", "not listed"]
                    .iter()
                    .any(|w| lower.contains(w)),
                "RUNBOOK.md:{} mentions {name} without saying it is gone: {line:?}",
                i + 1
            );
        }
    }
}

/// The three removed JIT knobs are not listed as live, and the RUNBOOK says so
/// in prose where a reader will actually look.
#[test]
fn removed_jit_knobs_are_documented_in_prose() {
    let runbook = read(&repo_root().join("docs/RUNBOOK.md"));
    for name in ["WIE_JIT_DIRECT_REGS", "WIE_JIT_TAILCHAIN", "WIE_JIT_VERIFY"] {
        assert!(
            runbook.contains(name),
            "{name} was removed but the RUNBOOK never says so — a reader who \
             remembers it has no way to learn it is gone"
        );
    }
    // The replacement must be named, so "use the new one" is actionable.
    assert!(
        runbook.contains("WIE_JIT_VERIFIER"),
        "the prose note must name WIE_JIT_VERIFIER as WIE_JIT_VERIFY's replacement"
    );
}
