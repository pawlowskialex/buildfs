#!/usr/bin/env bash
# Builds and signs the FSKit module, registers it, mounts a
# buildfs volume and runs the same benchmarks used for NFS and APFS.
#
# Prerequisites (one-time, manual):
#   - Apple Developer Program License Agreement accepted (developer.apple.com/account)
#   - after the first run: enable "buildfs" in System Settings > General >
#     Login Items & Extensions > File System Extensions
set -euo pipefail
cd "$(dirname "$0")"
MNT="${MNT:-/tmp/fskit-mnt}"

./install.sh
sleep 4 # registration race: the first mount right after installing can fail

mkdir -p "$MNT"
mount -F -t buildfs -o owners,nobrowse,fresh buildfs://bench "$MNT"
trap 'umount "$MNT" 2>/dev/null || true' EXIT
mount | grep " on $MNT "

echo "== microbenchmarks on FSKit"
python3 -I ../scripts/bench/micro.py "$MNT"
python3 -I ../scripts/bench/links.py "$MNT"

if [[ "${CARGO_BENCH:-1}" == 1 ]]; then
  echo "== cargo build of this workspace with the target dir on FSKit"
  T="$MNT/target"
  s=$(date +%s)
  CARGO_TARGET_DIR="$T" cargo build -q -p buildfs --manifest-path ../Cargo.toml
  echo "clean: $(( $(date +%s) - s ))s"
  for i in 1 2 3; do
    touch ../crates/buildfs-core/src/vfs/mod.rs
    s=$(python3 -c 'import time; print(time.time())')
    CARGO_TARGET_DIR="$T" cargo build -q -p buildfs --manifest-path ../Cargo.toml
    python3 -c "import time; print('incremental #$i: %.2fs' % (time.time() - $s))"
  done
fi
