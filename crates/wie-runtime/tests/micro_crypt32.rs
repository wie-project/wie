//! CRYPT32 micro-test: SHA-1 + SHA-256 hash round-trip (freestanding PE64).
//!
//! `crypt_hash.exe` drives the host crypt32 handlers end to end: acquire a
//! provider, draw 16 bytes of entropy, hash "abc" with both algorithms and
//! compare against the published test vectors. Binary from
//! `make -C micro-exes crypt_hash`. Skips if missing (no mingw).

use std::path::PathBuf;

fn micro_exe(name: &str) -> Option<PathBuf> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("micro-exes/out");
    path.push(name);
    path.is_file().then_some(path)
}

#[test]
fn crypt32_sha1_sha256_hashes_match_known_vectors() {
    let Some(path) = micro_exe("crypt_hash.exe") else {
        eprintln!(
            "skip: micro-exes/out/crypt_hash.exe not built (run make -C micro-exes crypt_hash)"
        );
        return;
    };
    let summary = wie_runtime::run_micro_exe(&path, 256).expect("run_micro_exe");
    assert_eq!(
        summary.exit_code,
        Some(0),
        "crypt_hash.exe: termination={:?} backend={}",
        summary.run.termination,
        summary.cpu_backend
    );
}
