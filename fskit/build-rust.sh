#!/bin/bash
# Xcode "Build Rust core" phase of the extension target: builds the
# buildfs-ffi static library (and regenerates Extension/buildfs_ffi.h).
set -euo pipefail
cd "${SRCROOT:-$(dirname "$0")}/.."
PATH="/run/current-system/sw/bin:$HOME/.cargo/bin:$HOME/.nix-profile/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
# Run cargo in a clean environment: Xcode exports hundreds of build
# settings, some of which confuse cc-based build scripts (zstd-sys).
exec env -i HOME="$HOME" USER="${USER:-}" PATH="$PATH" TMPDIR="${TMPDIR:-/tmp}" \
  MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-27.0}" \
  ${DEVELOPER_DIR:+DEVELOPER_DIR="$DEVELOPER_DIR"} \
  ${CARGO_HOME:+CARGO_HOME="$CARGO_HOME"} ${RUSTUP_HOME:+RUSTUP_HOME="$RUSTUP_HOME"} \
  cargo build --release -p buildfs-ffi --target aarch64-apple-darwin
