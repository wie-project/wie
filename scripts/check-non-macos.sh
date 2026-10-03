#!/usr/bin/env bash
# Compile-check the ENTIRE workspace on a non-Apple target.
#
# WHY THIS EXISTS — crates/wie-cli/Cargo.toml gates 9 dependencies behind
# [target.'cfg(target_os = "macos")'.dependencies] (winit, muda, rfd,
# objc2-app-kit, objc2, objc2-foundation, wgpu, pollster, bytemuck) with ~60
# lines of commentary asserting that non-Apple builds stay clean. Until this
# script existed, nothing verified that claim: there was no `cargo check
# --target <non-mac>` anywhere in the repo or in CI, so the comments were
# unbacked — and the claim was wrong. winit sat in the plain [dependencies]
# table, where `default-features = false` + `features = ["rwh_06"]` selects no
# platform at all off-Apple and winit 0.30 fails its own build:
#     error: The platform you're compiling for is not supported by winit
# It compiled on macOS only because winit's macOS backend is unconditional, so
# the feature set happened to suffice there. Nine gated deps are correctly
# gated; winit was the ninth omission.
#
# WHAT IT CATCHES: a non-macOS-hostile dependency (objc2, metal-only wgpu
# features, a backendless winit, a `std::os::macos` path, an endian
# assumption) added to a non-gated `[dependencies]` entry, or a `mod` in the
# wie-cli GUI tree that reaches a gated crate without its own `#[cfg]`. Those
# only surface when something actually compiles for a non-Apple target.
#
# SCOPE — `--workspace`, with no crate exclusions. That is the point: an
# exclusion list silently stops covering a crate the moment it goes stale,
# which is exactly the failure this script exists to prevent. wie-cli's
# macOS-only subtree (the winit/wgpu windowed presenter: gui/app,
# gui/present_wgpu, gui/menu_bar, gui/input, gui/input_script — see the
# platform-gating comment in crates/wie-cli/src/gui/mod.rs) is cfg-gated at
# the `mod` declarations, so `--gui` compiles out cleanly and the portable
# entries (`inspect`, `trace`, `run` micro/console/persistent, `run
# --screenshot`) still build and behave normally off-macOS.
#
# `--all-targets` so tests, benches and examples are covered too: the test
# cfg is a common place for a macOS-only `use` to hide.
#
# `cargo check` links nothing, so no C cross-linker is needed on the runner —
# only the target's rust-std, which `dtolnay/rust-toolchain` installs. The
# toolchain version is NOT pinned here: rust-toolchain.toml is the single source
# of truth (`channel = "stable"`), and CI installs `stable` to match it. Do not
# introduce a second toolchain version in this script.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

TARGET="${WIE_NON_MACOS_TARGET:-x86_64-unknown-linux-gnu}"

echo "=== non-macOS compile check: $TARGET (whole workspace) ==="
if ! rustup target list --installed | grep -qx "$TARGET"; then
  echo "target $TARGET is not installed; run: rustup target add $TARGET" >&2
  exit 1
fi

cd "$ROOT"
cargo check --workspace --all-targets --target "$TARGET"

echo "non-macOS compile check ok ($TARGET, whole workspace)"